//! Naming the working-copy commit (ADR-010).
//!
//! [`Repository::commit`] gives the working-copy commit its message, moves the branch that HEAD
//! names to it, and starts a new, empty working-copy commit on top. Both pointers move in one
//! operation, so the log has one entry for them (ADR-011).

use std::error::Error;
use std::fmt;

use super::snapshot::read_commit;
use super::{now, Repository, SnapshotError, UpdateError};
use crate::change::{ChangeId, GenerateError};
use crate::index::Index;
use crate::object::{Commit, CommitId};
use crate::oplog::{Change, Entry, Moved, OpLogError, OperationName};
use crate::refs::{BranchMove, BranchName, Head, Logged, RefError, WorkingCopyMove};
use crate::store::ObjectStore;
use crate::{Actor, Authorizer};

/// The operation that the log records for a commit.
const OPERATION: &str = "commit";

/// What [`Repository::commit`] did.
#[derive(Debug)]
#[non_exhaustive]
pub struct Committed {
    /// The commit that got the message and that the branch names now. It is the working-copy
    /// commit as it was, with the same change ID.
    pub commit: CommitId,
    /// The new working-copy commit: empty (the tree of `commit`), a child of `commit`, with a new
    /// change ID.
    pub working_copy: CommitId,
    /// The branch that moved to `commit`.
    pub branch: BranchName,
    /// The entry of the log for the two moves.
    pub entry: Entry,
    /// The moves happened, but a directory could not be flushed: [`UpdateError::NotDurable`]. The
    /// caller reports it after it has shown the result, and does not repeat the commit.
    pub late: Option<UpdateError>,
}

impl Repository {
    /// Names the working-copy commit and starts a new one (ADR-010).
    ///
    /// The tracked files are saved first, as [`snapshot`](Self::snapshot) does, so that a change
    /// since the last command is part of the commit. That is an operation of its own and has its
    /// own entry. Then the working-copy commit gets `message`, with its parents and its change ID,
    /// by the author `actor` at the time now. The branch that HEAD names moves to it, and a new
    /// working-copy commit takes its place: a child of the named commit, with the same tree, an
    /// empty message, and a change ID of its own. The same tree is what makes it empty, since the
    /// tracked set is the tree (ADR-010). The branch moves first, so that a stop between the moves
    /// keeps the commit reachable. The authorizer is asked for the operation `commit` on the
    /// branch before anything is read, and again for the snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`CommitError::EmptyMessage`] for a message that has no character but white space,
    /// [`CommitError::DetachedHead`] if HEAD names a commit and not a branch,
    /// [`CommitError::NothingToCommit`] if the tree is that of the parent (or empty, for a first
    /// commit), and [`CommitError::BranchMoved`] if the branch is not at the parent of the
    /// working-copy commit, so that the commit would drop what the branch has. In all of them
    /// nothing was changed but the snapshot. [`CommitError::Update`] covers the authorizer, a
    /// pointer that another process moved first ([`RefError::Stale`], [`RefError::StaleWorkingCopy`]:
    /// repeat the commit) and a failure of the log or the references. [`CommitError::Snapshot`]
    /// covers the snapshot and reading the commits, and [`CommitError::Refs`] reading HEAD and the
    /// branch.
    pub fn commit(
        &self,
        message: &str,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Committed, CommitError> {
        if message.trim().is_empty() {
            return Err(CommitError::EmptyMessage);
        }
        let operation = OperationName::new(OPERATION).expect("`commit` is a valid operation name");
        let name = match self.refs().head().map_err(CommitError::Refs)? {
            Head::Branch(name) => name,
            Head::Detached(commit) => return Err(CommitError::DetachedHead(commit)),
        };
        self.authorize(actor, authorizer, &operation, name.as_str())?;
        self.snapshot(actor, authorizer)?;

        let current = self.refs().working_copy().map_err(CommitError::Refs)?;
        let working = read_commit(self.objects(), current)?;
        let tip = self.refs().branch(&name).map_err(CommitError::Refs)?;
        if working.parents() != tip.as_slice() {
            return Err(CommitError::BranchMoved {
                branch: name,
                parents: working.parents().to_vec(),
                tip,
            });
        }
        let before = match tip {
            Some(parent) => read_commit(self.objects(), parent)?.tree(),
            None => Index::new()
                .write_tree(self.objects())
                .map_err(SnapshotError::WriteTree)?,
        };
        if working.tree() == before {
            return Err(CommitError::NothingToCommit);
        }

        let named = working.rewrite(working.tree(), actor.clone(), now(), message);
        let named_id = self.put(&named)?;
        let next = Commit::new(
            working.tree(),
            vec![named_id],
            ChangeId::generate().map_err(CommitError::ChangeId)?,
            actor.clone(),
            now(),
            "",
        )
        .expect("a single parent is not repeated");
        let next_id = self.put(&next)?;

        let outcome = self
            .refs
            .update_branch_and_working_copy_after(
                &BranchMove {
                    name: &name,
                    expected: tip,
                    new: named_id,
                },
                &WorkingCopyMove {
                    expected: current,
                    new: next_id,
                },
                &self.objects,
                || {
                    let change = Change::new(
                        now(),
                        actor.clone(),
                        operation.clone(),
                        None,
                        Some(Moved::from_to(Some(current), Some(next_id))),
                        vec![(name.clone(), Moved::from_to(tip, Some(named_id)))],
                    )
                    .map_err(OpLogError::Entry)?;
                    self.oplog.append(&change)
                },
            )
            .map_err(UpdateError::Refs)?;
        let (entry, late) = match outcome {
            Logged::Moved(entry) => (entry, None),
            Logged::NotFlushed(entry, source) => {
                let error = UpdateError::NotDurable {
                    entry: Box::new(entry.clone()),
                    source,
                };
                (entry, Some(error))
            }
            Logged::Unchanged => unreachable!("a commit moves both pointers"),
            other => return Err(CommitError::Update(into_error(other))),
        };
        Ok(Committed {
            commit: named_id,
            working_copy: next_id,
            branch: name,
            entry,
            late,
        })
    }

    fn put(&self, commit: &Commit) -> Result<CommitId, CommitError> {
        self.objects()
            .put_commit(commit)
            .map_err(|error| CommitError::Snapshot(SnapshotError::Store(Box::new(error))))
    }
}

/// The error of an outcome that is no success.
fn into_error(outcome: Logged<Entry, OpLogError>) -> UpdateError {
    match outcome {
        Logged::StepFailed(error) => UpdateError::Log(error),
        Logged::MoveFailed(entry, source) => UpdateError::MoveFailed {
            entry: Box::new(entry),
            source,
        },
        Logged::PartlyMoved(entry, source) => UpdateError::PartlyMoved {
            entry: Box::new(entry),
            source,
        },
        Logged::NotFlushed(entry, source) => UpdateError::NotDurable {
            entry: Box::new(entry),
            source,
        },
        Logged::Unchanged | Logged::Moved(_) => unreachable!("these are no errors"),
    }
}

/// Reasons why [`Repository::commit`] fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum CommitError {
    /// The message is empty or has only white space. There is no editor in phase 1.
    EmptyMessage,
    /// HEAD names a commit and not a branch, so there is no branch to move.
    DetachedHead(CommitId),
    /// The tree of the working-copy commit is that of its parent (or empty, if there is none).
    NothingToCommit,
    /// The branch is not at the parent of the working-copy commit, so a commit on top of that parent
    /// would leave out what the branch has. A rebase (phase 3) is the way to bring the two
    /// together.
    BranchMoved {
        /// The branch that HEAD names.
        branch: BranchName,
        /// The parents of the working-copy commit.
        parents: Vec<CommitId>,
        /// The commit of the branch, or `None` if it does not exist.
        tip: Option<CommitId>,
    },
    /// The snapshot failed, or a commit or tree could not be read or stored.
    Snapshot(SnapshotError),
    /// HEAD or the branch could not be read.
    Refs(RefError),
    /// The authorizer refused, a pointer was moved by another process, or the log or the references
    /// failed. [`UpdateError::entry`] and [`UpdateError::pointer_moved`] say what was done.
    Update(UpdateError),
    /// The operating system gave no random bytes for the change ID of the new working-copy commit.
    ChangeId(GenerateError),
}

impl From<SnapshotError> for CommitError {
    fn from(error: SnapshotError) -> Self {
        Self::Snapshot(error)
    }
}

impl From<UpdateError> for CommitError {
    fn from(error: UpdateError) -> Self {
        Self::Update(error)
    }
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyMessage => f.write_str("the message of a commit must not be empty"),
            Self::DetachedHead(commit) => write!(
                f,
                "HEAD names the commit {commit} and not a branch, so there is no branch to commit to"
            ),
            Self::NothingToCommit => f.write_str("there is nothing to commit: no tracked file changed"),
            Self::BranchMoved {
                branch,
                parents,
                tip,
            } => {
                let show = |commits: &[CommitId]| {
                    if commits.is_empty() {
                        "no commit".to_owned()
                    } else {
                        commits
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                };
                write!(
                    f,
                    "the branch `{branch}` is at {}, but the working-copy commit is based on {}; \
                     committing would leave out what the branch has",
                    show(tip.as_slice()),
                    show(parents)
                )
            }
            Self::Snapshot(error) => error.fmt(f),
            Self::Refs(error) => write!(f, "could not read the references: {error}"),
            Self::Update(error) => error.fmt(f),
            Self::ChangeId(error) => write!(f, "could not make a change ID: {error}"),
        }
    }
}

impl Error for CommitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Snapshot(error) => Some(error),
            Self::Refs(error) => error.source(),
            Self::Update(error) => Some(error),
            Self::ChangeId(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::oplog::OpLogError;
    use crate::repository::InitOptions;
    use crate::testutil::TempDir;
    use crate::{AllowAll, Decision};

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &str, _target: &str) -> Decision {
            Decision::Deny
        }
    }

    fn actor() -> Actor {
        Actor::human("daniel").unwrap()
    }

    fn other_actor() -> Actor {
        Actor::human("alice").unwrap()
    }

    fn repository(dir: &TempDir) -> Repository {
        Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap()
    }

    fn write(dir: &TempDir, path: &str, content: &str) {
        fs::write(dir.path().join(path), content).unwrap();
    }

    fn track(repository: &Repository, path: &str) {
        repository
            .track(&[path.to_owned()], &actor(), &AllowAll)
            .unwrap();
    }

    fn commit(repository: &Repository, message: &str) -> Result<Committed, CommitError> {
        repository.commit(message, &other_actor(), &AllowAll)
    }

    fn read(repository: &Repository, id: CommitId) -> Commit {
        read_commit(repository.objects(), id).unwrap()
    }

    fn main() -> BranchName {
        BranchName::new("main").unwrap()
    }

    #[test]
    fn a_first_commit_creates_the_branch_and_starts_a_new_working_copy() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let before = repository.refs().working_copy().unwrap();
        let change = read(&repository, before).change();
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");

        let done = commit(&repository, "Add a").unwrap();

        assert!(done.late.is_none());
        assert_eq!(done.branch, main());
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(done.commit)
        );
        assert_eq!(repository.refs().working_copy().unwrap(), done.working_copy);
        let named = read(&repository, done.commit);
        assert_eq!(named.message(), "Add a");
        assert_eq!(
            named.change(),
            change,
            "the named commit keeps the change ID"
        );
        assert_eq!(named.author(), &other_actor());
        assert!(named.parents().is_empty());
        let next = read(&repository, done.working_copy);
        assert_eq!(next.parents(), [done.commit]);
        assert_eq!(next.tree(), named.tree(), "the new working copy is empty");
        assert_eq!(next.message(), "");
        assert_ne!(next.change(), change);
    }

    #[test]
    fn two_commits_have_different_change_ids_and_form_a_chain() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let first = commit(&repository, "one").unwrap();
        write(&dir, "a.txt", "two");

        let second = commit(&repository, "two").unwrap();

        let (one, two) = (
            read(&repository, first.commit),
            read(&repository, second.commit),
        );
        assert_ne!(one.change(), two.change());
        assert_eq!(two.parents(), [first.commit]);
        assert_ne!(one.tree(), two.tree());
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(second.commit)
        );
    }

    #[test]
    fn a_change_since_the_last_command_is_part_of_the_commit() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        write(&dir, "a.txt", "changed afterwards");

        let done = commit(&repository, "Add a").unwrap();

        let tree = read(&repository, done.commit).tree();
        let files = super::super::snapshot::tracked_files(repository.objects(), tree).unwrap();
        assert_eq!(files.len(), 1);
        let again = repository.snapshot(&actor(), &AllowAll).unwrap();
        assert!(again.is_none(), "the working directory equals the commit");
    }

    #[test]
    fn a_commit_writes_one_entry_for_both_pointers() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let entries = repository.oplog().entries().unwrap().len();

        let done = commit(&repository, "Add a").unwrap();

        let all = repository.oplog().entries().unwrap();
        assert_eq!(all.len(), entries + 1);
        let last = all.last().unwrap();
        assert_eq!(last, &done.entry);
        let change = last.change();
        assert_eq!(change.operation().as_str(), "commit");
        assert_eq!(change.actor(), &other_actor());
        assert_eq!(
            change.working_copy().unwrap().after(),
            Some(&done.working_copy)
        );
        let (name, moved) = change.branches().first().unwrap();
        assert_eq!(name, &main());
        assert_eq!(moved.before(), None);
        assert_eq!(moved.after(), Some(&done.commit));
    }

    #[test]
    fn a_change_since_the_last_command_is_saved_by_its_own_snapshot_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        write(&dir, "a.txt", "two");
        let entries = repository.oplog().entries().unwrap().len();

        commit(&repository, "Add a").unwrap();

        let all = repository.oplog().entries().unwrap();
        let names: Vec<&str> = all[entries..]
            .iter()
            .map(|entry| entry.change().operation().as_str())
            .collect();
        assert_eq!(names, ["snapshot", "commit"]);
    }

    #[test]
    fn nothing_to_commit_in_a_new_repository_and_after_a_commit() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        assert!(matches!(
            commit(&repository, "nothing"),
            Err(CommitError::NothingToCommit)
        ));
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        commit(&repository, "Add a").unwrap();
        let entries = repository.oplog().entries().unwrap().len();

        let error = commit(&repository, "again").unwrap_err();

        assert!(matches!(error, CommitError::NothingToCommit), "{error}");
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[test]
    fn an_empty_message_is_refused_before_anything_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let entries = repository.oplog().entries().unwrap().len();

        for message in ["", "  \n\t"] {
            let error = commit(&repository, message).unwrap_err();
            assert!(matches!(error, CommitError::EmptyMessage), "{error}");
        }

        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[test]
    fn a_detached_head_is_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let first = commit(&repository, "one").unwrap();
        let operation = OperationName::new("checkout").unwrap();
        repository
            .set_head(
                &operation,
                &Head::Detached(first.commit),
                &actor(),
                &AllowAll,
            )
            .unwrap();
        write(&dir, "a.txt", "two");

        let error = commit(&repository, "two").unwrap_err();

        assert!(
            matches!(error, CommitError::DetachedHead(id) if id == first.commit),
            "{error}"
        );
    }

    #[test]
    fn a_branch_that_moved_is_not_overwritten() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let first = commit(&repository, "one").unwrap();
        // Someone moves the branch to the working-copy commit, past the parent it is based on.
        let operation = OperationName::new("branch").unwrap();
        repository
            .update_branch(
                &operation,
                &main(),
                Some(first.commit),
                first.working_copy,
                &actor(),
                &AllowAll,
            )
            .unwrap();
        write(&dir, "a.txt", "two");

        let error = commit(&repository, "two").unwrap_err();

        let CommitError::BranchMoved { tip, parents, .. } = &error else {
            panic!("{error}");
        };
        assert_eq!(tip, &Some(first.working_copy));
        assert_eq!(parents, &[first.commit]);
        assert!(error.to_string().contains("main"), "{error}");
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(first.working_copy)
        );
    }

    #[test]
    fn a_refusal_by_the_authorizer_changes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let working_copy = repository.refs().working_copy().unwrap();
        let entries = repository.oplog().entries().unwrap().len();

        let error = repository
            .commit("Add a", &other_actor(), &DenyAll)
            .unwrap_err();

        assert!(
            matches!(&error, CommitError::Update(UpdateError::Denied { operation, target })
                if operation == "commit" && target == "main"),
            "{error}"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), working_copy);
        assert_eq!(repository.refs().branch(&main()).unwrap(), None);
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[test]
    fn a_locked_repository_is_reported_and_nothing_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let entries = repository.oplog().entries().unwrap().len();
        fs::write(repository.metadata_dir().join("refs.lock"), "someone").unwrap();

        let error = commit(&repository, "Add a").unwrap_err();

        assert!(
            matches!(
                error,
                CommitError::Update(UpdateError::Refs(RefError::Locked { .. }))
            ),
            "{error}"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert_eq!(repository.refs().branch(&main()).unwrap(), None);
    }

    #[test]
    fn a_pointer_that_another_process_moved_first_writes_no_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let working_copy = repository.refs().working_copy().unwrap();
        let first = commit(&repository, "one").unwrap();
        let entries = repository.oplog().entries().unwrap().len();
        // The moves use the working-copy commit that the caller saw before the first commit.
        let stale = repository.refs.update_branch_and_working_copy_after(
            &BranchMove {
                name: &main(),
                expected: Some(first.commit),
                new: first.working_copy,
            },
            &WorkingCopyMove {
                expected: working_copy,
                new: first.working_copy,
            },
            repository.objects(),
            || Ok::<_, OpLogError>(()),
        );

        assert!(
            matches!(stale, Err(RefError::StaleWorkingCopy { .. })),
            "the working-copy commit is checked"
        );
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(first.commit)
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[test]
    fn a_full_log_stops_the_commit_before_any_pointer_moves() {
        // An entry that cannot be written is a step that failed: nothing moved.
        let error = into_error(Logged::StepFailed(OpLogError::Full));
        assert!(matches!(error, UpdateError::Log(OpLogError::Full)));
        assert!(!error.pointer_moved());
    }

    #[test]
    fn a_half_made_move_is_an_error_that_says_a_pointer_moved() {
        let entry = {
            let dir = TempDir::new();
            let repository = repository(&dir);
            repository.oplog().entries().unwrap().remove(0)
        };
        let error = into_error(Logged::PartlyMoved(
            entry,
            RefError::Locked { path: "x".into() },
        ));
        assert!(matches!(error, UpdateError::PartlyMoved { .. }));
        assert!(error.pointer_moved());
        assert!(error.entry().is_some());
        assert!(error.to_string().contains("do not repeat"), "{error}");
    }
}
