//! Bringing the worktree of `main` along after the merge gate (ADR-019, STEMMA-261).
//!
//! The gate moves the branch `main` and nothing else: it holds no lock while it works, and it
//! must not wait for the working copy of anyone. The worktree that has `main` checked out is
//! therefore left on the old tip, with a working-copy commit whose parent is no longer the tip.
//! [`Repository::follow_main`] is the step that the command runs after the gate: it finds that
//! worktree and brings it along by the mechanism of `switch` (ADR-010), which takes the snapshot,
//! builds the next working-copy commit on the tip, writes one entry, and brings the directory to
//! the new tree.
//!
//! The step is an operation of its own, with a lock and an entry of its own. A stop between the
//! gate and this step, and every case in which the worktree cannot follow, leave the state that
//! `switch main` in that worktree repairs: the merge stands either way.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use super::switch::{Changes, Handled, SwitchError, SwitchTarget, Switched};
use super::worktree::WorktreeError;
use super::{OpenError, Repository};
use crate::oplog::Entry;
use crate::pull::TARGET;
use crate::refs::{BranchName, Head, MAIN_WORKTREE};
use crate::{Actor, Authorizer};

/// What became of the worktree that has `main` checked out (see
/// [`follow_main`](Repository::follow_main)).
#[derive(Debug)]
#[must_use]
pub enum Followed {
    /// No worktree has `main` checked out. Nothing was written.
    NoWorktree,
    /// No worktree that can be read has `main` checked out, but the state of some linked
    /// worktrees cannot be read: one of them may have `main` checked out and stay on the old
    /// tip. Nothing was written.
    Unknown {
        /// The names of the worktrees whose state cannot be read, sorted.
        damaged: Vec<String>,
    },
    /// The working-copy commit of the worktree stands on the tip of `main` already. Nothing was
    /// written.
    UpToDate {
        /// The name of the worktree: `main` for the main worktree.
        worktree: String,
    },
    /// The working copy of the worktree was brought to the tip of `main`.
    Done {
        /// The name of the worktree: `main` for the main worktree.
        worktree: String,
        /// The entry of the snapshot that came first, if the directory had unsaved changes.
        snapshot: Option<Box<Entry>>,
        /// The entry of the move of the working-copy commit, an entry of `switch`.
        entry: Box<Entry>,
        /// What became of the changes of the old working-copy commit.
        handled: Handled,
    },
    /// The worktree stays on the old tip. The merge is not touched by this; `switch main` in the
    /// worktree brings it along later.
    Behind {
        /// The name of the worktree: `main` for the main worktree.
        worktree: String,
        /// The directory of the worktree.
        path: PathBuf,
        /// Why it stays behind.
        reason: Behind,
    },
}

/// Why a worktree stays behind `main`.
#[derive(Debug)]
#[non_exhaustive]
pub enum Behind {
    /// Another actor owns the working copy. Only its owner saves it (ADR-010), an agent never
    /// takes over, and the gate asks nobody for a take-over. Nothing was written.
    Owner(Box<Actor>),
    /// The directory of the worktree cannot be opened as a repository: it is gone, was moved, or
    /// is damaged. Nothing was written.
    Unreachable(Box<OpenError>),
    /// The directory holds another repository or another worktree now. Nothing was written.
    Replaced,
    /// The switch that brings the working copy along failed: a file that is not tracked is in the
    /// way, the lock is held, a command of the worktree stopped after its files
    /// ([`SwitchError::Stopped`]), or the file system failed. The error says how far it came:
    /// [`SwitchError::entry`] and [`SwitchError::snapshot`] name what it wrote, and
    /// [`SwitchError::made`] says that the working copy did follow and only a flush is not known.
    Failed(Box<SwitchError>),
}

impl fmt::Display for Behind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Owner(owner) => write!(f, "its working copy belongs to {owner}"),
            Self::Unreachable(error) => write!(f, "its directory cannot be opened: {error}"),
            Self::Replaced => f.write_str("its directory holds another working copy now"),
            Self::Failed(error) => error.fmt(f),
        }
    }
}

impl Repository {
    /// Brings the working copy of the worktree that has `main` checked out to the tip of `main`:
    /// the step after [`merge_gate`](Self::merge_gate), which moves the branch only (ADR-019).
    ///
    /// At most one worktree has `main` checked out (ADR-009). It can be this one or another one;
    /// another one is opened through the directory that its state names. The step is a
    /// [`switch`](Self::switch) to `main` in that worktree with [`Changes::Carry`], so everything
    /// that holds for a switch holds here: unsaved changes are saved in a snapshot first, a
    /// working-copy commit that holds changes is carried onto the tip as `T + (W - P)`, a conflict
    /// of the carry is data of the new commit and no failure, and one entry of `switch` holds
    /// the move, which `op restore` undoes. The tip is read under the lock of that switch, so the
    /// worktree lands on the tip of that moment, also if `main` moved again since the gate.
    ///
    /// The owner decides whether the step runs at all. `actor` brings along a working copy that
    /// it owns, and one without an owner (from before format 5). A working copy of another actor
    /// stays behind ([`Behind::Owner`]): the snapshot would refuse it, and this step never takes a
    /// working copy over, whatever value `self` is (see [`taking_over`](Self::taking_over)). The
    /// owner is read before anything is written. It is read without the lock, so a take-over in
    /// between is found by the snapshot of the switch and ends in [`Behind::Failed`].
    ///
    /// A worktree whose state cannot be read is left out of the search. If no readable worktree
    /// has `main` checked out and some are unreadable, the result is [`Followed::Unknown`] with
    /// their names, as one of them may be the one; without unreadable ones it is
    /// [`Followed::NoWorktree`].
    ///
    /// A worktree in which a command stopped after its files (a merge, a rebase, or a switch onto
    /// an older tip, whose entry is written while the working-copy pointer is old) is not brought
    /// along: a snapshot would save those files as changes. The switch refuses with
    /// [`SwitchError::Stopped`], which arrives as [`Behind::Failed`], and nothing is written.
    ///
    /// # Errors
    ///
    /// Returns [`WorktreeError`] if the worktrees cannot be listed or the state of the main
    /// worktree cannot be read. Everything that happens to the worktree itself is in
    /// [`Followed`].
    pub fn follow_main(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Followed, WorktreeError> {
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        let (rows, damaged) = self.scan_worktrees()?;
        let Some(row) = rows
            .into_iter()
            .find(|row| row.head == Head::Branch(main.clone()))
        else {
            if damaged.is_empty() {
                return Ok(Followed::NoWorktree);
            }
            let mut damaged: Vec<String> = damaged.into_iter().map(|(name, _)| name).collect();
            damaged.sort();
            return Ok(Followed::Unknown { damaged });
        };
        let behind = |reason| Followed::Behind {
            worktree: row.name.clone(),
            path: row.path.clone(),
            reason,
        };
        if let Some(owner) = row.owner.as_ref().filter(|owner| *owner != actor) {
            return Ok(behind(Behind::Owner(Box::new(owner.clone()))));
        }
        let here = self.worktree_name().unwrap_or(MAIN_WORKTREE);
        let other;
        let worktree = if row.name == here {
            // A value that takes over would do so in the snapshot; this step never does.
            other = Self {
                take_over: None,
                ..self.clone()
            };
            &other
        } else {
            other = match Self::open(&row.path) {
                Ok(opened) => opened,
                Err(error) => return Ok(behind(Behind::Unreachable(Box::new(error)))),
            };
            // The directory may hold another repository, or another worktree of this one.
            let same = same_directory(&other.metadata_dir(), &self.metadata_dir())
                && other.worktree_name().unwrap_or(MAIN_WORKTREE) == row.name;
            if !same {
                return Ok(behind(Behind::Replaced));
            }
            &other
        };
        Ok(
            match worktree.switch(
                &SwitchTarget::Existing(main),
                Changes::Carry,
                actor,
                authorizer,
            ) {
                Ok(Switched::AlreadyOn(_)) => Followed::UpToDate { worktree: row.name },
                Ok(Switched::Done {
                    snapshot,
                    entry,
                    handled,
                    ..
                }) => Followed::Done {
                    worktree: row.name,
                    snapshot,
                    entry,
                    handled,
                },
                Err(error) => behind(Behind::Failed(Box::new(error))),
            },
        )
    }
}

/// Whether two paths name one directory, links resolved. Paths that cannot be resolved are
/// compared as they are.
fn same_directory(one: &Path, other: &Path) -> bool {
    match (fs::canonicalize(one), fs::canonicalize(other)) {
        (Ok(one), Ok(other)) => one == other,
        _ => one == other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::CommitId;
    use crate::oplog::OperationName;
    use crate::repository::snapshot::read_any_commit;
    use crate::repository::{InitOptions, SnapshotError, WorktreeBranch};
    use crate::testutil::TempDir;
    use crate::AllowAll;

    fn daniel() -> Actor {
        Actor::human("daniel").expect("a valid actor")
    }

    fn ana() -> Actor {
        Actor::human("ana").expect("a valid actor")
    }

    fn name(text: &str) -> BranchName {
        BranchName::try_new(text).expect("a branch name")
    }

    /// A repository of `daniel` with one commit on `main` (`a.txt`) and a branch `feature` that
    /// adds `b.txt`, with the main worktree back on `main`.
    fn repository(dir: &TempDir) -> Repository {
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &daniel(), &AllowAll)
                .expect("init");
        commit(dir, &repository, "a.txt", "one\n", "First");
        let create = SwitchTarget::Create {
            name: name("feature"),
            start: None,
        };
        let _ = repository
            .switch(&create, Changes::Carry, &daniel(), &AllowAll)
            .expect("switch -c");
        commit(dir, &repository, "b.txt", "two\n", "Second");
        switch_to(&repository, "main");
        repository
    }

    fn commit(dir: &TempDir, repository: &Repository, path: &str, content: &str, message: &str) {
        fs::write(dir.path().join(path), content).expect("write");
        let _ = repository
            .track(&[path.to_owned()], &daniel(), &AllowAll)
            .expect("track");
        let _ = repository
            .commit(message, &daniel(), &AllowAll)
            .expect("commit");
    }

    fn switch_to(repository: &Repository, branch: &str) {
        let _ = repository
            .switch(
                &SwitchTarget::Existing(name(branch)),
                Changes::Carry,
                &daniel(),
                &AllowAll,
            )
            .expect("switch");
    }

    /// Moves `main` to the tip of `feature` without a look at any working copy, as the gate does.
    fn move_main(repository: &Repository) -> CommitId {
        let tip = repository
            .refs()
            .branch(&name("feature"))
            .expect("read")
            .expect("feature");
        let old = repository.refs().branch(&name("main")).expect("read");
        let operation = OperationName::try_new("gate-merge").expect("an operation name");
        let _ = repository
            .update_branch(&operation, &name("main"), old, tip, &daniel(), &AllowAll)
            .expect("move main");
        tip
    }

    fn parents_of_working_copy(repository: &Repository) -> Vec<CommitId> {
        let working = repository.refs().working_copy().expect("working copy");
        read_any_commit(repository.objects(), working)
            .expect("read")
            .parents()
            .to_vec()
    }

    fn entries(repository: &Repository) -> usize {
        repository.oplog().entries().expect("entries").len()
    }

    #[test]
    fn the_own_worktree_follows_main_with_one_entry_that_moves_the_working_copy_only() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let tip = move_main(&repository);
        assert!(!dir.path().join("b.txt").exists());
        let before = entries(&repository);

        let followed = repository
            .follow_main(&daniel(), &AllowAll)
            .expect("follow");

        let Followed::Done {
            worktree,
            snapshot,
            entry,
            handled,
        } = followed
        else {
            panic!("expected a move, got {followed:?}");
        };
        assert_eq!(worktree, "main");
        assert!(snapshot.is_none());
        assert_eq!(handled, Handled::Nothing);
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "two\n"
        );
        assert_eq!(parents_of_working_copy(&repository), [tip]);
        assert_eq!(entries(&repository), before + 1);
        let change = entry.change().expect("a change");
        assert_eq!(change.operation().as_str(), "switch");
        assert!(change.head().is_none());
        assert!(change.branches().is_empty());
        assert!(change.working_copy().is_some());
        assert_eq!(
            repository.refs().head().expect("head"),
            Head::Branch(name("main"))
        );
    }

    #[test]
    fn a_second_call_finds_the_worktree_on_the_tip_and_writes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let _ = move_main(&repository);
        let _ = repository
            .follow_main(&daniel(), &AllowAll)
            .expect("follow");
        let before = entries(&repository);

        let again = repository
            .follow_main(&daniel(), &AllowAll)
            .expect("follow");

        assert!(matches!(again, Followed::UpToDate { worktree } if worktree == "main"));
        assert_eq!(entries(&repository), before);
    }

    #[test]
    fn unsaved_changes_are_saved_first_and_carried_onto_the_new_tip() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        fs::write(dir.path().join("a.txt"), "one\nmore\n").expect("write");
        let tip = move_main(&repository);

        let followed = repository
            .follow_main(&daniel(), &AllowAll)
            .expect("follow");

        let Followed::Done {
            snapshot, handled, ..
        } = followed
        else {
            panic!("expected a move, got {followed:?}");
        };
        assert!(snapshot.is_some());
        assert_eq!(handled, Handled::Carried { conflicts: vec![] });
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\nmore\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "two\n"
        );
        assert_eq!(parents_of_working_copy(&repository), [tip]);
    }

    #[test]
    fn a_conflict_of_the_carry_is_data_of_the_new_working_copy_and_no_failure() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        // The change of the working copy adds the file that the new tip adds with other content.
        fs::write(dir.path().join("b.txt"), "mine\n").expect("write");
        let _ = repository
            .track(&["b.txt".to_owned()], &daniel(), &AllowAll)
            .expect("track");
        let tip = move_main(&repository);

        let followed = repository
            .follow_main(&daniel(), &AllowAll)
            .expect("follow");

        let Followed::Done { handled, .. } = followed else {
            panic!("expected a move, got {followed:?}");
        };
        assert_eq!(
            handled,
            Handled::Carried {
                conflicts: vec!["b.txt".to_owned()]
            }
        );
        assert_eq!(parents_of_working_copy(&repository), [tip]);
        let text = fs::read_to_string(dir.path().join("b.txt")).unwrap();
        assert!(text.contains("mine") && text.contains("two"), "{text}");
    }

    #[test]
    fn a_working_copy_of_another_actor_stays_behind_and_nothing_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let _ = move_main(&repository);
        let before = entries(&repository);
        let working = repository.refs().working_copy().expect("working copy");

        // Also a value that takes over does not: the step never takes a working copy over.
        for value in [repository.clone(), repository.taking_over()] {
            let followed = value.follow_main(&ana(), &AllowAll).expect("follow");

            let Followed::Behind {
                worktree, reason, ..
            } = followed
            else {
                panic!("expected a worktree that stays, got {followed:?}");
            };
            assert_eq!(worktree, "main");
            assert!(matches!(reason, Behind::Owner(owner) if *owner == daniel()));
            assert!(value.taken_over().is_none());
        }
        assert_eq!(entries(&repository), before);
        assert_eq!(repository.refs().working_copy().expect("read"), working);
        assert!(!dir.path().join("b.txt").exists());
    }

    #[test]
    fn a_file_in_the_way_leaves_the_worktree_behind_before_anything_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let _ = move_main(&repository);
        // Not tracked, and where the new tip has a file of its own.
        fs::write(dir.path().join("b.txt"), "untracked\n").expect("write");
        let before = entries(&repository);

        let followed = repository
            .follow_main(&daniel(), &AllowAll)
            .expect("follow");

        let Followed::Behind { reason, .. } = followed else {
            panic!("expected a worktree that stays, got {followed:?}");
        };
        assert!(
            matches!(&reason, Behind::Failed(error) if matches!(**error, SwitchError::Checkout(_))),
            "{reason:?}"
        );
        assert_eq!(entries(&repository), before);
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "untracked\n"
        );
    }

    /// The main worktree on `feature`, and a linked worktree `side` of `owner` on `main`.
    fn with_linked(dir: &TempDir, place: &TempDir, owner: &Actor) -> (Repository, Repository) {
        let repository = repository(dir);
        switch_to(&repository, "feature");
        let added = repository
            .add_worktree(
                &place.path().join("side"),
                &WorktreeBranch::Existing(name("main")),
                Some(owner),
                &daniel(),
                &AllowAll,
            )
            .expect("worktree add");
        let linked = Repository::open(&added.path).expect("open the worktree");
        (repository, linked)
    }

    #[test]
    fn a_linked_worktree_of_the_actor_follows_from_a_command_in_another_worktree() {
        let (dir, place) = (TempDir::new(), TempDir::new());
        let (repository, linked) = with_linked(&dir, &place, &daniel());
        let tip = move_main(&repository);
        assert!(!linked.path().join("b.txt").exists());

        let followed = repository
            .follow_main(&daniel(), &AllowAll)
            .expect("follow");

        let Followed::Done {
            worktree, entry, ..
        } = followed
        else {
            panic!("expected a move, got {followed:?}");
        };
        assert_eq!(worktree, "side");
        // The entry belongs to the worktree that moved, not to the one of the command.
        assert_eq!(entry.change().expect("a change").worktree(), Some("side"));
        assert_eq!(
            fs::read_to_string(linked.path().join("b.txt")).unwrap(),
            "two\n"
        );
        assert_eq!(parents_of_working_copy(&linked), [tip]);
        // The worktree of the command is on `feature` and is not touched.
        assert_eq!(
            repository.refs().head().expect("head"),
            Head::Branch(name("feature"))
        );
    }

    #[test]
    fn a_linked_worktree_of_another_actor_stays_behind_and_its_owner_brings_it_along() {
        let (dir, place) = (TempDir::new(), TempDir::new());
        let (repository, linked) = with_linked(&dir, &place, &ana());
        let tip = move_main(&repository);
        let before = entries(&repository);

        let followed = repository
            .follow_main(&daniel(), &AllowAll)
            .expect("follow");

        assert!(
            matches!(
                &followed,
                Followed::Behind { worktree, reason: Behind::Owner(owner), .. }
                    if worktree == "side" && **owner == ana()
            ),
            "{followed:?}"
        );
        assert_eq!(entries(&repository), before);
        assert!(!linked.path().join("b.txt").exists());

        // `switch main` of another actor is refused as any snapshot is, and the owner's works.
        let target = SwitchTarget::Existing(name("main"));
        let refused = linked
            .switch(&target, Changes::Carry, &daniel(), &AllowAll)
            .expect_err("not the owner");
        assert!(
            matches!(
                refused,
                SwitchError::Snapshot(SnapshotError::NotOwner { .. })
            ),
            "{refused:?}"
        );
        let switched = linked
            .switch(&target, Changes::Carry, &ana(), &AllowAll)
            .expect("the owner follows");
        assert!(matches!(switched, Switched::Done { .. }));
        assert_eq!(parents_of_working_copy(&linked), [tip]);
        assert_eq!(
            fs::read_to_string(linked.path().join("b.txt")).unwrap(),
            "two\n"
        );
    }

    #[test]
    fn a_linked_worktree_whose_directory_is_gone_stays_behind() {
        let (dir, place) = (TempDir::new(), TempDir::new());
        let (repository, linked) = with_linked(&dir, &place, &daniel());
        let _ = move_main(&repository);
        fs::remove_dir_all(linked.path()).expect("remove the directory");
        let before = entries(&repository);

        let followed = repository
            .follow_main(&daniel(), &AllowAll)
            .expect("follow");

        assert!(
            matches!(
                &followed,
                Followed::Behind { worktree, reason: Behind::Unreachable(_), .. }
                    if worktree == "side"
            ),
            "{followed:?}"
        );
        assert_eq!(entries(&repository), before);
    }

    #[test]
    fn a_directory_that_holds_another_repository_now_is_not_written() {
        let (dir, place) = (TempDir::new(), TempDir::new());
        let (repository, linked) = with_linked(&dir, &place, &daniel());
        let _ = move_main(&repository);
        fs::remove_dir_all(linked.path()).expect("remove the directory");
        let _ = Repository::init(linked.path(), &InitOptions::default(), &daniel(), &AllowAll)
            .expect("init another repository");

        let followed = repository
            .follow_main(&daniel(), &AllowAll)
            .expect("follow");

        assert!(
            matches!(
                &followed,
                Followed::Behind {
                    reason: Behind::Replaced,
                    ..
                }
            ),
            "{followed:?}"
        );
        assert!(!linked.path().join("b.txt").exists());
    }

    #[test]
    fn a_worktree_that_cannot_be_read_is_named_when_no_other_has_main() {
        let (dir, place) = (TempDir::new(), TempDir::new());
        let (repository, _linked) = with_linked(&dir, &place, &daniel());
        let _ = move_main(&repository);
        let state = repository.metadata_dir().join("worktrees").join("side");
        fs::write(state.join("HEAD"), "not a head\n").expect("damage HEAD");

        let followed = repository
            .follow_main(&daniel(), &AllowAll)
            .expect("follow");

        assert!(
            matches!(&followed, Followed::Unknown { damaged } if damaged == &["side".to_owned()]),
            "{followed:?}"
        );
    }

    #[test]
    fn without_a_worktree_on_main_nothing_follows() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        switch_to(&repository, "feature");
        let _ = move_main(&repository);
        let before = entries(&repository);

        let followed = repository
            .follow_main(&daniel(), &AllowAll)
            .expect("follow");

        assert!(matches!(followed, Followed::NoWorktree));
        assert_eq!(entries(&repository), before);
    }
}
