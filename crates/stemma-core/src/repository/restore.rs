//! Restoring the state before an entry of the operation log (ADR-013).
//!
//! A restore puts back the old values of the pointers that one entry moved: HEAD, the
//! working-copy commit, and the branches. It writes an entry of its own, so the log is never
//! rewritten and a restore can itself be restored. Because the working copy is a commit that
//! stands for the directory (ADR-010), a restore that moves it also brings the files of the
//! directory to the old tree, through the checkout layer.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use super::checkout::{CheckoutError, CheckoutPlan};
use super::snapshot::{read_commit, SnapshotError};
use super::{now, Repository, UpdateError};
use crate::object::{CommitId, TreeId};
use crate::oplog::{
    outcomes, Change, Current, Entry, EntryError, Moved, OpLogError, OperationName, Outcome,
    Pointer,
};
use crate::refs::{BranchName, Head, Logged, RefError, RestoreMoves, RestoreTargets};
use crate::{Actor, Authorizer};

/// The operation that the log records for a restore.
const OPERATION: &str = "restore";

/// What a restore does, or would do: the pointers it moves and the files it writes and removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    head: Option<Moved<Head>>,
    working_copy: Option<Moved<CommitId>>,
    branches: Vec<(BranchName, Moved<CommitId>)>,
    checkout: Option<CheckoutPlan>,
}

impl RestorePlan {
    /// The move of HEAD, if it moves.
    pub fn head(&self) -> Option<&Moved<Head>> {
        self.head.as_ref()
    }

    /// The move of the working-copy commit, if it moves.
    pub fn working_copy(&self) -> Option<&Moved<CommitId>> {
        self.working_copy.as_ref()
    }

    /// The branches that move, sorted by name. A branch that the restore deletes has no value
    /// after.
    pub fn branches(&self) -> &[(BranchName, Moved<CommitId>)] {
        &self.branches
    }

    /// The files to write and to remove. `None` if the working-copy commit does not move, so that
    /// the directory stays as it is.
    pub fn checkout(&self) -> Option<&CheckoutPlan> {
        self.checkout.as_ref()
    }
}

/// How a restore ended.
#[derive(Debug)]
#[non_exhaustive]
pub enum Restored {
    /// The pointers have the old values of the entry already, or the entry did not take effect:
    /// its state before is the state now. Nothing was changed, and no entry was written (a
    /// snapshot may have been).
    NothingToRestore,
    /// A dry run: what the restore would do. Nothing was written, not even the snapshot, so a
    /// change that is not saved yet is not in the plan.
    Planned(Box<RestorePlan>),
    /// The pointers were moved back.
    Done {
        /// The entry of the snapshot that came first, if the directory had unsaved changes.
        snapshot: Option<Box<Entry>>,
        /// The entry of the restore. Restoring it undoes the restore.
        entry: Box<Entry>,
    },
}

impl Repository {
    /// Puts back the values that the pointers had before the entry `number`, and writes an entry
    /// of its own for it (ADR-013).
    ///
    /// The steps are: the authorizer is asked for `restore`; the entry and the outcomes of the log
    /// are read; a snapshot of the directory is taken, so that no unsaved change is lost; then,
    /// under the lock of the references, the plan is made (every commit must be in the store, and
    /// every file must be placeable), the entry is written and flushed, the directory is brought
    /// to the old tree, and the pointers move, the branches first and HEAD last (ADR-011).
    ///
    /// An entry whose move did not take effect is no work: the state before it is the state now,
    /// and the call returns [`Restored::NothingToRestore`]. If the log and the references do not
    /// decide whether it took effect, the call is refused with [`RestoreError::OutcomeUnknown`]
    /// unless `force` is set. The restore puts the old values back even if other entries moved the
    /// pointers afterwards. A pointer that has its old value already is left alone, and a branch
    /// that the entry created is deleted.
    ///
    /// With `dry_run` nothing is written, not even the snapshot: the call returns the plan.
    ///
    /// # Errors
    ///
    /// Returns [`RestoreError::Update`] for a refusal of the authorizer and for what goes wrong
    /// when the pointers move, [`RestoreError::NoSuchEntry`] and [`RestoreError::Log`] for the
    /// entry, [`RestoreError::OutcomeUnknown`], [`RestoreError::NoOldValue`] for HEAD or the
    /// working-copy commit that the entry created (they cannot be absent), [`RestoreError::Refs`]
    /// for a commit the store lacks or a lock that is held, [`RestoreError::Snapshot`] for the
    /// snapshot and for reading a commit, and [`RestoreError::Checkout`] for a file in the way
    /// (nothing was written then) or a failure while the files were written (the entry is in the
    /// log, no pointer moved, and the next snapshot saves what was done as a change of the actor).
    pub fn restore(
        &self,
        number: u64,
        force: bool,
        dry_run: bool,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Restored, RestoreError> {
        let operation = OperationName::new(OPERATION).expect("`restore` is a valid operation name");
        self.authorize(actor, authorizer, &operation, &format!("entry {number}"))
            .map_err(RestoreError::Update)?;
        let entries = self.oplog().entries().map_err(RestoreError::Log)?;
        let Some(position) = entries.iter().position(|entry| entry.number() == number) else {
            return Err(RestoreError::NoSuchEntry { number });
        };
        match self.outcome_of(&entries, position)? {
            Outcome::NotTaken => return Ok(Restored::NothingToRestore),
            Outcome::Unknown if !force => return Err(RestoreError::OutcomeUnknown { number }),
            Outcome::Took | Outcome::Unknown => {}
        }
        let change = entries[position].change();
        let head = match change.head() {
            Some(moved) => Some(moved.before().cloned().ok_or(RestoreError::NoOldValue {
                number,
                pointer: Pointer::Head,
            })?),
            None => None,
        };
        let working_copy = match change.working_copy() {
            Some(moved) => Some(*moved.before().ok_or(RestoreError::NoOldValue {
                number,
                pointer: Pointer::WorkingCopy,
            })?),
            None => None,
        };
        let branches: Vec<(BranchName, Option<CommitId>)> = change
            .branches()
            .iter()
            .map(|(name, moved)| (name.clone(), moved.before().copied()))
            .collect();
        let targets = RestoreTargets {
            head: head.as_ref(),
            working_copy,
            branches: &branches,
        };
        if dry_run {
            let (moves, _) = self
                .refs_store()
                .plan_restore(&targets, self.objects())
                .map_err(RestoreError::Refs)?;
            if moves.is_empty() {
                return Ok(Restored::NothingToRestore);
            }
            let plan = self
                .make_plan(&moves)
                .map_err(|source| RestoreError::Checkout {
                    entry: None,
                    source,
                })?;
            return Ok(Restored::Planned(Box::new(plan)));
        }
        let snapshot = self.snapshot(actor, authorizer)?;
        let logged = self
            .refs_store()
            .restore_after(&targets, self.objects(), |moves| {
                let plan = self
                    .make_plan(moves)
                    .map_err(|source| RestoreError::Checkout {
                        entry: None,
                        source,
                    })?;
                let change = Change::new(
                    now(),
                    actor.clone(),
                    operation.clone(),
                    plan.head.clone(),
                    plan.working_copy.clone(),
                    plan.branches.clone(),
                )
                .map_err(OpLogError::Entry)
                .map_err(|error| RestoreError::Update(UpdateError::Log(error)))?;
                let entry = self
                    .oplog()
                    .append(&change)
                    .map_err(|error| RestoreError::Update(UpdateError::Log(error)))?;
                if let Some(checkout) = &plan.checkout {
                    self.apply_checkout(checkout)
                        .map_err(|source| RestoreError::Checkout {
                            entry: Some(Box::new(entry.clone())),
                            source,
                        })?;
                }
                Ok(entry)
            })
            .map_err(RestoreError::Refs)?;
        match logged {
            Logged::Unchanged => Ok(Restored::NothingToRestore),
            Logged::Moved(entry) => Ok(Restored::Done {
                snapshot: snapshot.map(Box::new),
                entry: Box::new(entry),
            }),
            Logged::StepFailed(error) => Err(error),
            Logged::MoveFailed(entry, source) => {
                Err(RestoreError::Update(UpdateError::MoveFailed {
                    entry: Box::new(entry),
                    source,
                }))
            }
            Logged::PartlyMoved(entry, source) => {
                Err(RestoreError::Update(UpdateError::PartlyMoved {
                    entry: Box::new(entry),
                    source,
                }))
            }
            Logged::NotFlushed(entry, source) => {
                Err(RestoreError::Update(UpdateError::NotDurable {
                    entry: Box::new(entry),
                    source,
                }))
            }
        }
    }

    /// The plan of the moves: the values, and the files if the working-copy commit moves.
    fn make_plan(&self, moves: &RestoreMoves) -> Result<RestorePlan, CheckoutError> {
        let checkout = match &moves.working_copy {
            Some((current, target)) => {
                let from = self.tree_of(*current)?;
                let to = self.tree_of(*target)?;
                Some(self.plan_checkout(Some(from), to)?)
            }
            None => None,
        };
        Ok(RestorePlan {
            head: moves
                .head
                .as_ref()
                .map(|(old, new)| Moved::from_to(Some(old.clone()), Some(new.clone()))),
            working_copy: moves
                .working_copy
                .as_ref()
                .map(|(old, new)| Moved::from_to(Some(*old), Some(*new))),
            branches: moves
                .branches
                .iter()
                .map(|(name, old, new)| (name.clone(), Moved::from_to(*old, *new)))
                .collect(),
            checkout,
        })
    }

    fn tree_of(&self, commit: CommitId) -> Result<TreeId, SnapshotError> {
        Ok(read_commit(self.objects(), commit)?.tree())
    }

    /// Whether the entry took effect, judged from the log and the references as `op log` does.
    fn outcome_of(&self, entries: &[Entry], position: usize) -> Result<Outcome, RestoreError> {
        let refs = self.refs();
        let locked_before = refs.is_locked().map_err(RestoreError::Refs)?;
        let head = refs.head().map_err(RestoreError::Refs)?;
        let working_copy = refs.working_copy().map_err(RestoreError::Refs)?;
        let names: BTreeSet<&BranchName> = entries
            .iter()
            .flat_map(|entry| entry.change().branches().iter().map(|(name, _)| name))
            .collect();
        let mut branches = Vec::new();
        for name in names {
            if let Some(commit) = refs.branch(name).map_err(RestoreError::Refs)? {
                branches.push((name.clone(), commit));
            }
        }
        let locked_after = refs.is_locked().map_err(RestoreError::Refs)?;
        let current = Current::new(head, branches, !(locked_before || locked_after))
            .with_working_copy(Some(working_copy));
        Ok(outcomes(entries, Some(&current))[position])
    }
}

/// Why [`Repository::restore`] fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum RestoreError {
    /// The authorizer refused, or the log or the pointers failed when they were moved. See
    /// [`UpdateError`] for what is in the log and whether a pointer moved.
    Update(UpdateError),
    /// The log cannot be read.
    Log(OpLogError),
    /// The log has no entry with the number.
    NoSuchEntry {
        /// The number.
        number: u64,
    },
    /// The log and the references do not show whether the entry took effect, so the state before
    /// it may not be what it says. `force` restores anyway.
    OutcomeUnknown {
        /// The number.
        number: u64,
    },
    /// HEAD or the working-copy commit did not exist before the entry, and cannot be absent.
    NoOldValue {
        /// The number.
        number: u64,
        /// The pointer.
        pointer: Pointer,
    },
    /// A commit that a pointer takes is not in the object store, a name is in the way, a lock is
    /// held, or the references cannot be read. Nothing was written.
    Refs(RefError),
    /// The snapshot that comes first failed, or a commit cannot be read.
    Snapshot(SnapshotError),
    /// The files could not be brought to the old tree. Without an entry, nothing was written. With
    /// one, the entry is in the log and names a move that did not happen; no pointer moved, and
    /// the files done so far stay (see [`CheckoutError::Interrupted`]).
    Checkout {
        /// The entry that was written, if it was.
        entry: Option<Box<Entry>>,
        /// Why.
        source: CheckoutError,
    },
}

impl RestoreError {
    /// Returns the error of the file system, if there is one inside.
    pub fn io_error(&self) -> Option<&std::io::Error> {
        match self {
            Self::Update(error) => error.io_error(),
            Self::Log(error) => error.io_error(),
            Self::Refs(error) => error.io_error(),
            Self::Snapshot(error) => error.io_error(),
            Self::Checkout { source, .. } => source.io_error(),
            Self::NoSuchEntry { .. } | Self::OutcomeUnknown { .. } | Self::NoOldValue { .. } => {
                None
            }
        }
    }

    /// The entry that was written, if the error leaves one in the log.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Update(error) => error.entry(),
            Self::Checkout { entry, .. } => entry.as_deref(),
            _ => None,
        }
    }

    /// Whether a pointer moved although the call failed.
    pub fn pointer_moved(&self) -> bool {
        matches!(self, Self::Update(error) if error.pointer_moved())
    }
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Update(error) => write!(f, "could not restore: {error}"),
            Self::Log(error) => write!(f, "could not read the operation log: {error}"),
            Self::NoSuchEntry { number } => {
                write!(f, "the operation log has no entry {number}")
            }
            Self::OutcomeUnknown { number } => write!(
                f,
                "the log does not show whether entry {number} took effect, so the state before \
                 it may not be what it says; use --force to restore anyway"
            ),
            Self::NoOldValue { number, pointer } => write!(
                f,
                "entry {number} created {}, which cannot be absent, so it cannot be restored",
                match pointer {
                    Pointer::Head => "HEAD".to_owned(),
                    Pointer::WorkingCopy => "the working-copy commit".to_owned(),
                    Pointer::Branch(name) => format!("the branch `{name}`"),
                }
            ),
            Self::Refs(error) => write!(f, "could not restore, nothing was written: {error}"),
            Self::Snapshot(error) => {
                write!(f, "could not save the working directory first: {error}")
            }
            Self::Checkout {
                entry: None,
                source,
            } => write!(f, "could not restore, nothing was written: {source}"),
            Self::Checkout {
                entry: Some(entry),
                source,
            } => write!(
                f,
                "could not write the files; entry {} names a restore that did not happen and no \
                 pointer moved: {source}",
                entry.number()
            ),
        }
    }
}

impl Error for RestoreError {}

impl From<SnapshotError> for RestoreError {
    fn from(error: SnapshotError) -> Self {
        Self::Snapshot(error)
    }
}

impl From<EntryError> for RestoreError {
    fn from(error: EntryError) -> Self {
        Self::Update(UpdateError::Log(OpLogError::Entry(error)))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
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

    fn repository(dir: &TempDir) -> Repository {
        Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap()
    }

    fn write(dir: &TempDir, path: &str, content: &str) {
        fs::write(dir.path().join(path), content).unwrap();
    }

    fn read(dir: &TempDir, path: &str) -> String {
        fs::read_to_string(dir.path().join(path)).unwrap()
    }

    fn main() -> BranchName {
        BranchName::new("main").unwrap()
    }

    fn numbers(repository: &Repository) -> Vec<u64> {
        repository
            .oplog()
            .entries()
            .unwrap()
            .iter()
            .map(Entry::number)
            .collect()
    }

    fn restore(repository: &Repository, number: u64) -> Result<Restored, RestoreError> {
        repository.restore(number, false, false, &actor(), &AllowAll)
    }

    fn done(result: Result<Restored, RestoreError>) -> (Option<Entry>, Entry) {
        match result {
            Ok(Restored::Done { snapshot, entry }) => (snapshot.map(|entry| *entry), *entry),
            other => panic!("expected a restore, got {other:?}"),
        }
    }

    /// A repository with a.txt "one" committed as "first", then "two" committed as "second".
    /// Returns the numbers of the entries of the second round: the snapshot, then the commit.
    fn two_commits(dir: &TempDir, repository: &Repository) -> (u64, u64) {
        write(dir, "a.txt", "one");
        repository
            .track(&["a.txt".to_owned()], &actor(), &AllowAll)
            .unwrap();
        repository
            .commit("first", &actor(), &AllowAll)
            .map(drop)
            .unwrap();
        write(dir, "a.txt", "two");
        let before = numbers(repository).len();
        repository
            .commit("second", &actor(), &AllowAll)
            .map(drop)
            .unwrap();
        let all = numbers(repository);
        assert_eq!(all.len(), before + 2, "a snapshot and a commit");
        (all[before], all[before + 1])
    }

    #[test]
    fn the_old_files_and_pointers_come_back_and_the_restore_has_its_own_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _commit) = two_commits(&dir, &repository);
        let entries = repository.oplog().entries().unwrap();
        let old = *entries
            .iter()
            .find(|entry| entry.number() == snapshot)
            .unwrap()
            .change()
            .working_copy()
            .unwrap()
            .before()
            .unwrap();
        let count = entries.len();
        let (saved, entry) = done(restore(&repository, snapshot));
        assert!(saved.is_none(), "nothing was unsaved");
        assert_eq!(read(&dir, "a.txt"), "one");
        assert_eq!(repository.refs().working_copy().unwrap(), old);
        assert_eq!(entry.change().operation().as_str(), "restore");
        assert_eq!(numbers(&repository).len(), count + 1);
        let moved = entry.change().working_copy().unwrap();
        assert_eq!(moved.after(), Some(&old));
    }

    #[test]
    fn a_restore_can_be_restored() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        let (_, first) = done(restore(&repository, snapshot));
        assert_eq!(read(&dir, "a.txt"), "one");
        done(restore(&repository, first.number()));
        assert_eq!(read(&dir, "a.txt"), "two");
    }

    #[test]
    fn an_unsaved_change_is_saved_first_and_is_not_lost() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        write(&dir, "a.txt", "scratch");
        let (saved, restored) = done(restore(&repository, snapshot));
        let saved = saved.expect("the unsaved change is saved first");
        assert_eq!(saved.change().operation().as_str(), "snapshot");
        assert!(saved.number() < restored.number());
        assert_eq!(read(&dir, "a.txt"), "one");
        done(restore(&repository, restored.number()));
        assert_eq!(read(&dir, "a.txt"), "scratch");
    }

    #[test]
    fn a_branch_that_the_entry_created_is_deleted() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        repository
            .track(&["a.txt".to_owned()], &actor(), &AllowAll)
            .unwrap();
        repository
            .commit("first", &actor(), &AllowAll)
            .map(drop)
            .unwrap();
        assert!(repository.refs().branch(&main()).unwrap().is_some());
        let created = repository
            .oplog()
            .entries()
            .unwrap()
            .into_iter()
            .find(|entry| {
                entry
                    .change()
                    .branches()
                    .iter()
                    .any(|(_, moved)| moved.before().is_none())
            })
            .unwrap()
            .number();
        done(restore(&repository, created));
        assert_eq!(repository.refs().branch(&main()).unwrap(), None);
        assert!(
            repository.refs().head().is_ok(),
            "HEAD still names the branch that is not there"
        );
    }

    #[test]
    fn a_dry_run_writes_nothing_and_reports_the_plan() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        write(&dir, "a.txt", "unsaved");
        let before = numbers(&repository);
        let result = repository
            .restore(snapshot, false, true, &actor(), &AllowAll)
            .unwrap();
        let Restored::Planned(plan) = result else {
            panic!("expected a plan, got {result:?}");
        };
        assert!(plan.working_copy().is_some());
        assert!(plan.checkout().is_some_and(|checkout| !checkout.is_empty()));
        assert_eq!(numbers(&repository), before, "not even a snapshot");
        assert_eq!(read(&dir, "a.txt"), "unsaved");
    }

    #[test]
    fn pointers_that_have_the_old_values_already_are_left_alone() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        done(restore(&repository, snapshot));
        let before = numbers(&repository);
        let result = restore(&repository, snapshot).unwrap();
        assert!(matches!(result, Restored::NothingToRestore), "{result:?}");
        assert_eq!(numbers(&repository), before);
    }

    #[test]
    fn an_entry_that_is_not_in_the_log_is_reported() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let error = restore(&repository, 99).unwrap_err();
        assert!(
            matches!(error, RestoreError::NoSuchEntry { number: 99 }),
            "{error}"
        );
    }

    #[test]
    fn the_first_entry_cannot_be_restored_because_it_created_head() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let first = numbers(&repository)[0];
        let error = restore(&repository, first).unwrap_err();
        assert!(matches!(error, RestoreError::NoOldValue { .. }), "{error}");
    }

    #[test]
    fn a_refusal_of_the_authorizer_changes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        write(&dir, "a.txt", "unsaved");
        let before = numbers(&repository);
        let error = repository
            .restore(snapshot, false, false, &actor(), &DenyAll)
            .unwrap_err();
        assert!(
            matches!(error, RestoreError::Update(UpdateError::Denied { .. })),
            "{error}"
        );
        assert_eq!(numbers(&repository), before);
        assert_eq!(read(&dir, "a.txt"), "unsaved");
    }

    #[test]
    fn a_file_in_the_way_stops_the_restore_before_the_entry_and_the_files() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        repository
            .track(&["a.txt".to_owned()], &actor(), &AllowAll)
            .unwrap();
        repository
            .commit("first", &actor(), &AllowAll)
            .map(drop)
            .unwrap();
        // The snapshot drops a.txt from the tree because the file is gone.
        fs::remove_file(dir.path().join("a.txt")).unwrap();
        let before_snapshot = numbers(&repository).len();
        repository.snapshot(&actor(), &AllowAll).unwrap();
        let dropped = numbers(&repository)[before_snapshot];
        fs::create_dir(dir.path().join("a.txt")).unwrap();
        write_inside(&dir, "a.txt/x", "mine");
        let before = numbers(&repository);
        let error = restore(&repository, dropped).unwrap_err();
        assert!(
            matches!(&error, RestoreError::Checkout { entry: None, .. }),
            "{error}"
        );
        assert_eq!(numbers(&repository), before, "no entry was written");
        assert_eq!(read(&dir, "a.txt/x"), "mine");
    }

    fn write_inside(dir: &TempDir, path: &str, content: &str) {
        fs::write(dir.path().join(path), content).unwrap();
    }

    #[test]
    fn a_working_copy_that_moved_outside_the_log_makes_the_outcome_unknown() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        // Point the working-copy commit at a commit that no entry names.
        let tip = repository.refs().branch(&main()).unwrap().unwrap();
        let first_parent = read_commit(repository.objects(), tip).unwrap().parents()[0];
        fs::write(
            repository.metadata_dir().join("working-copy"),
            format!("{first_parent}\n"),
        )
        .unwrap();
        let error = restore(&repository, snapshot + 1).unwrap_err();
        assert!(
            matches!(error, RestoreError::OutcomeUnknown { .. }),
            "{error}"
        );
        let forced = repository
            .restore(snapshot + 1, true, false, &actor(), &AllowAll)
            .unwrap();
        assert!(matches!(forced, Restored::Done { .. }), "{forced:?}");
    }

    #[test]
    fn a_branch_moves_back_and_a_nested_branch_is_deleted_with_its_directory() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (_, _) = two_commits(&dir, &repository);
        let tip = repository.refs().branch(&main()).unwrap().unwrap();
        let first = read_commit(repository.objects(), tip).unwrap().parents()[0];
        let nested = BranchName::new("feature/deep/x").unwrap();
        let operation = OperationName::new("branch").unwrap();
        let entry = repository
            .update_branch(&operation, &nested, None, tip, &actor(), &AllowAll)
            .unwrap()
            .unwrap();
        let moved = repository
            .update_branch(&operation, &main(), Some(tip), first, &actor(), &AllowAll)
            .unwrap()
            .unwrap();
        done(restore(&repository, entry.number()));
        assert_eq!(repository.refs().branch(&nested).unwrap(), None);
        assert!(
            !repository
                .metadata_dir()
                .join("refs/heads/feature")
                .exists(),
            "the empty directories go with the branch"
        );
        done(restore(&repository, moved.number()));
        assert_eq!(repository.refs().branch(&main()).unwrap(), Some(tip));
    }

    #[test]
    fn a_commit_that_the_store_lacks_stops_the_restore_before_anything_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        let old = *repository
            .oplog()
            .read(snapshot)
            .unwrap()
            .change()
            .working_copy()
            .unwrap()
            .before()
            .unwrap();
        let path = repository.objects().object_path(&old.object_id());
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(&path, permissions).unwrap();
        fs::remove_file(&path).unwrap();
        let before = numbers(&repository);
        let error = restore(&repository, snapshot).unwrap_err();
        assert!(
            matches!(error, RestoreError::Refs(RefError::MissingCommit(_))),
            "{error}"
        );
        assert_eq!(numbers(&repository), before);
        assert_eq!(read(&dir, "a.txt"), "two");
    }
}
