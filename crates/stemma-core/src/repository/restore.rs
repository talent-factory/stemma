//! Restoring the state before an entry of the operation log (ADR-013).
//!
//! A restore puts back the old values of the pointers that one entry moved: HEAD, the
//! working-copy commit, and the branches. It writes an entry of its own, so a restore never
//! rewrites the log (only a purge replaces entries by stubs, ADR-014) and a restore can itself be
//! restored. Because the working copy is a commit that
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
    outcomes, pending_stubs, Change, Current, Entry, Moved, OpLogError, OperationName, Outcome,
    Pointer, Record,
};
use crate::refs::{BranchName, Head, Logged, RefError, RestoreMoves, RestoreTargets};
use crate::{Actor, Authorizer};

/// The operation that the log records for a restore.
const OPERATION: &str = "restore";

/// What a restore does, or would do: the pointers it moves and the files it writes and removes.
///
/// The plan is made from the pointers as they are now. A real restore saves the unsaved changes
/// of the directory first, which the plan does not know: a file with such a change is replaced
/// all the same, and what was in it is in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    entry: u64,
    head: Option<Moved<Head>>,
    working_copy: Option<Moved<CommitId>>,
    branches: Vec<(BranchName, Moved<CommitId>)>,
    checkout: Option<CheckoutPlan>,
}

impl RestorePlan {
    /// The number of the entry that is restored.
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// The move of HEAD, if it moves.
    pub fn head(&self) -> Option<&Moved<Head>> {
        self.head.as_ref()
    }

    /// The move of the working-copy commit, if it moves.
    pub fn working_copy(&self) -> Option<&Moved<CommitId>> {
        self.working_copy.as_ref()
    }

    /// The branches that move, sorted by name. A branch that the restore creates has no value
    /// before, and one that it deletes has no value after.
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
    /// The pointers have the old values of the entry already. Nothing was moved and no entry of
    /// the restore was written. A snapshot of unsaved changes was taken only if the pointers
    /// changed between the plan and the move, so `snapshot` is `None` in practice.
    NothingToRestore {
        /// The entry of the snapshot, if one was taken.
        snapshot: Option<Box<Entry>>,
    },
    /// The pointers were moved back.
    Done {
        /// The entry of the snapshot that came first, if the directory had unsaved changes.
        snapshot: Option<Box<Entry>>,
        /// The entry of the restore. Restoring it undoes the restore.
        entry: Box<Entry>,
    },
}

impl Restored {
    /// The entry of the restore, or `None` if there was nothing to restore.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Done { entry, .. } => Some(entry),
            Self::NothingToRestore { .. } => None,
        }
    }

    /// The entry of the snapshot that came first, if the directory had unsaved changes.
    pub fn snapshot(&self) -> Option<&Entry> {
        match self {
            Self::Done { snapshot, .. } | Self::NothingToRestore { snapshot } => {
                snapshot.as_deref()
            }
        }
    }
}

/// What is read from the log before anything is done.
struct Prepared {
    head: Option<Head>,
    working_copy: Option<CommitId>,
    branches: Vec<(BranchName, Option<CommitId>)>,
    /// The plan, or `None` if every pointer has its old value already.
    plan: Option<RestorePlan>,
}

impl Prepared {
    fn targets(&self) -> RestoreTargets<'_> {
        RestoreTargets {
            head: self.head.as_ref(),
            working_copy: self.working_copy,
            branches: &self.branches,
        }
    }
}

impl Repository {
    /// Plans the restore of the entry `number` and writes nothing, not even a snapshot. Returns
    /// `None` if every pointer that the entry moved has its old value already.
    ///
    /// The authorizer is asked for `restore`, as for [`restore`](Self::restore). See there for the
    /// rules about the outcome of the entry and `force`.
    ///
    /// # Errors
    ///
    /// Returns what [`restore`](Self::restore) returns before it takes the snapshot.
    pub fn plan_restore(
        &self,
        number: u64,
        force: bool,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Option<RestorePlan>, RestoreError> {
        Ok(self.prepare(number, force, actor, authorizer)?.plan)
    }

    /// Puts back the values that the pointers had before the entry `number`, and writes an entry
    /// of its own for it (ADR-013).
    ///
    /// The steps are: the authorizer is asked for `restore` (the target is the entry); the log is
    /// read and the outcome of the entry is judged; the plan is made once, so that a missing
    /// commit or a file in the way is found before anything is written; the lock of the
    /// references is taken and held to the last move, so that no purge comes in between
    /// (ADR-014); a snapshot of the directory is taken (a second question to the authorizer,
    /// `snapshot`), so that no unsaved change is lost; then the plan is made again from the values
    /// the lock sees, the entry is written and flushed, the directory is brought to the old tree,
    /// and the pointers move: the branches first, then the working-copy commit, and HEAD last
    /// (ADR-013).
    ///
    /// Each pointer is judged on its own: the restore puts the old value of every pointer that the
    /// entry names back, whether or not the entry took effect and even if other entries moved the
    /// pointer afterwards. A pointer that has its old value already is left alone, so an entry
    /// that stopped half way can be restored, and so can a restore that stopped half way. A branch
    /// that the entry created is deleted. If the log and the references do not decide whether the
    /// entry took effect, the call is refused with [`RestoreError::OutcomeUnknown`] unless
    /// `force` is set; this is so for every entry before the newest stub (ADR-014). A stub, the
    /// entry of a purge, and an entry that an unfinished purge lists are refused even with `force`.
    ///
    /// # Errors
    ///
    /// Before the snapshot: [`RestoreError::Update`] for a refusal of the question `restore`,
    /// [`RestoreError::Log`] and [`RestoreError::NoSuchEntry`] for the entry,
    /// [`RestoreError::Purged`] for a stub, [`RestoreError::IsPurge`] for the entry of a purge,
    /// [`RestoreError::PurgePending`] for an entry that an unfinished purge lists,
    /// [`RestoreError::OutcomeUnknown`], [`RestoreError::NoOldValue`] for HEAD or the working-copy
    /// commit that the entry created (they cannot be absent), [`RestoreError::Refs`] for a commit
    /// the store lacks, a name in the way, or references that cannot be read,
    /// [`RestoreError::Snapshot`] for a commit or a tree that cannot be read, and
    /// [`RestoreError::Checkout`] for a file in the way. Nothing is written then.
    ///
    /// After the snapshot, the same errors can come again if the pointers changed meanwhile, and
    /// [`RestoreError::Snapshot`] covers the snapshot itself (a refusal of the question
    /// `snapshot` is one). When the snapshot wrote an entry and a later step fails, the error is
    /// wrapped in [`RestoreError::AfterSnapshot`], which names that entry. The restore wrote
    /// nothing in these cases. Then [`RestoreError::Update`] covers the entry that could not be
    /// written, [`RestoreError::CheckoutFailed`] a failure while the files were applied (the entry
    /// is in the log and no pointer moved), and [`RestoreError::MoveFailed`],
    /// [`RestoreError::PartlyMoved`], and [`RestoreError::NotDurable`] the moves of the pointers.
    ///
    /// If the snapshot moved the working-copy commit and a directory could not be flushed, the
    /// restore goes on, and the flush is reported at the end: as
    /// [`RestoreError::SnapshotNotDurable`] when the restore was made (its `restored` is the
    /// entry of the restore, or `None` if there was nothing to restore, and the restore is *not*
    /// to be repeated), and in the `late` field of [`RestoreError::AfterSnapshot`] when a later
    /// step failed. [`RestoreError::pointer_moved`] is true in both.
    pub fn restore(
        &self,
        number: u64,
        force: bool,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Restored, RestoreError> {
        // A purge that has not finished is named first, before the entry is judged; the lock
        // below refuses it again.
        self.refs_store()
            .refuse_pending_purge()
            .map_err(RestoreError::Refs)?;
        let prepared = self.prepare(number, force, actor, authorizer)?;
        if prepared.plan.is_none() {
            return Ok(Restored::NothingToRestore { snapshot: None });
        }
        // One lock from the first object of the snapshot to the last move, so that no purge
        // comes in between and deletes an object that the restore names (ADR-014). It is taken
        // after the plan above: the plan judges the newest entry by the lock (`is_locked`), and
        // would take the lock of this call for one of an unfinished command.
        let lock = self
            .refs_store()
            .lock_writer()
            .map_err(RestoreError::Refs)?;
        let (snapshot, mut snapshot_late) = match self.snapshot_locked(&lock, actor, authorizer) {
            Ok(entry) => (entry, None),
            // The snapshot was made and the pointer moved; only the flush is not known. The
            // restore goes on, as the unsaved work is in the log, and reports the flush at the end.
            Err(SnapshotError::Update(UpdateError::NotDurable { entry, source })) => {
                (Some(*entry), Some(source))
            }
            Err(error) => return Err(RestoreError::Snapshot(error)),
        };
        let snapshot = snapshot.map(Box::new);
        #[cfg(test)]
        super::snapshot::pause::reached(super::snapshot::pause::Stage::Moves);
        let operation = OperationName::new(OPERATION).expect("`restore` is a valid operation name");
        let logged =
            self.refs_store()
                .restore_after(&lock, &prepared.targets(), self.objects(), |moves| {
                    let plan = self.make_plan(number, moves)?;
                    let change = Change::new(
                        now(),
                        actor.clone(),
                        operation.clone(),
                        plan.head.clone(),
                        plan.working_copy.clone(),
                        plan.branches.clone(),
                    )
                    .map_err(|error| {
                        RestoreError::Update(UpdateError::Log(OpLogError::Entry(error)))
                    })?;
                    // Through the repository, which moves it to the format of the chain first.
                    let entry = self
                        .append(&change)
                        .map_err(|error| RestoreError::Update(UpdateError::Log(error)))?;
                    if let Some(checkout) = &plan.checkout {
                        self.apply_checkout(checkout).map_err(|source| {
                            RestoreError::CheckoutFailed {
                                entry: Box::new(entry.clone()),
                                source,
                            }
                        })?;
                    }
                    Ok(entry)
                });
        // A failure after the snapshot names it, and the flush that did not succeed with it.
        let mut wrap = |error: RestoreError| match &snapshot {
            Some(saved) => RestoreError::AfterSnapshot {
                snapshot: saved.clone(),
                late: snapshot_late.take(),
                source: Box::new(error),
            },
            None => error,
        };
        match logged {
            Err(error) => Err(wrap(RestoreError::Refs(error))),
            Ok(Logged::Unchanged) => match (snapshot, snapshot_late) {
                (Some(saved), Some(source)) => Err(RestoreError::SnapshotNotDurable {
                    snapshot: saved,
                    restored: None,
                    source,
                }),
                (snapshot, _) => Ok(Restored::NothingToRestore { snapshot }),
            },
            Ok(Logged::Moved(entry)) => match (snapshot, snapshot_late) {
                (Some(saved), Some(source)) => Err(RestoreError::SnapshotNotDurable {
                    snapshot: saved,
                    restored: Some(Box::new(entry)),
                    source,
                }),
                (snapshot, _) => Ok(Restored::Done {
                    snapshot,
                    entry: Box::new(entry),
                }),
            },
            Ok(Logged::StepFailed(error)) => Err(wrap(error)),
            Ok(Logged::MoveFailed(entry, source)) => Err(wrap(RestoreError::MoveFailed {
                entry: Box::new(entry),
                source,
            })),
            Ok(Logged::PartlyMoved(entry, source)) => Err(wrap(RestoreError::PartlyMoved {
                entry: Box::new(entry),
                source,
            })),
            Ok(Logged::NotFlushed(entry, source)) => Err(wrap(RestoreError::NotDurable {
                entry: Box::new(entry),
                source,
            })),
        }
    }

    /// Everything a restore needs before it writes: the authorization, the entry, its outcome,
    /// the target values, and the plan.
    fn prepare(
        &self,
        number: u64,
        force: bool,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Prepared, RestoreError> {
        let operation = OperationName::new(OPERATION).expect("`restore` is a valid operation name");
        self.authorize(actor, authorizer, &operation, &format!("entry {number}"))
            .map_err(RestoreError::Update)?;
        let entries = self.oplog().entries().map_err(RestoreError::Log)?;
        let Some(position) = entries.iter().position(|entry| entry.number() == number) else {
            return Err(RestoreError::NoSuchEntry { number });
        };
        let change = match entries[position].record() {
            Record::Change(change) => change,
            Record::Stub(_) => return Err(RestoreError::Purged { number }),
            Record::Purge(_) => return Err(RestoreError::IsPurge { number }),
        };
        if pending_stubs(&entries).contains(&number) {
            return Err(RestoreError::PurgePending { number });
        }
        if self.outcome_of(&entries, position)? == Outcome::Unknown && !force {
            return Err(RestoreError::OutcomeUnknown { number });
        }
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
        let branches = change
            .branches()
            .iter()
            .map(|(name, moved)| (name.clone(), moved.before().copied()))
            .collect();
        let mut prepared = Prepared {
            head,
            working_copy,
            branches,
            plan: None,
        };
        let (moves, _) = self
            .refs_store()
            .plan_restore(&prepared.targets(), self.objects())
            .map_err(RestoreError::Refs)?;
        if !moves.is_empty() {
            prepared.plan = Some(self.make_plan(number, &moves)?);
        }
        Ok(prepared)
    }

    /// The plan of the moves: the values, and the files if the working-copy commit moves.
    fn make_plan(&self, number: u64, moves: &RestoreMoves) -> Result<RestorePlan, RestoreError> {
        let checkout = match &moves.working_copy {
            Some((current, target)) => {
                let from = self.tree_of(*current)?;
                let to = self.tree_of(*target)?;
                Some(
                    self.plan_checkout(Some(from), to)
                        .map_err(RestoreError::Checkout)?,
                )
            }
            None => None,
        };
        Ok(RestorePlan {
            entry: number,
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

    fn tree_of(&self, commit: CommitId) -> Result<TreeId, RestoreError> {
        read_commit(self.objects(), commit)
            .map(|commit| commit.tree())
            .map_err(RestoreError::Snapshot)
    }

    /// Whether the entry took effect, judged from the log and the references as `op log` does.
    fn outcome_of(&self, entries: &[Entry], position: usize) -> Result<Outcome, RestoreError> {
        let refs = self.refs();
        let locked_before = refs.is_locked().map_err(RestoreError::Refs)?;
        let head = refs.head().map_err(RestoreError::Refs)?;
        let working_copy = refs.working_copy().map_err(RestoreError::Refs)?;
        let names: BTreeSet<&BranchName> = entries
            .iter()
            .filter_map(Entry::change)
            .flat_map(|change| change.branches().iter().map(|(name, _)| name))
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

/// Why [`Repository::restore`] fails. Every variant says whether the restore wrote something: the
/// ones before the entry (`Update`, `Log`, `NoSuchEntry`, `Purged`, `IsPurge`, `PurgePending`,
/// `OutcomeUnknown`, `NoOldValue`, `Refs`, `Snapshot`, `Checkout`) wrote nothing of the restore;
/// those with an `entry` wrote it.
#[derive(Debug)]
#[non_exhaustive]
pub enum RestoreError {
    /// The authorizer refused `restore`, or the entry of the restore could not be written (see
    /// [`UpdateError::Log`]; with [`OpLogError::NotFlushed`] the entry is in the log). No pointer
    /// moved and no file was written.
    Update(UpdateError),
    /// The log cannot be read.
    Log(OpLogError),
    /// The log has no entry with the number.
    NoSuchEntry {
        /// The number.
        number: u64,
    },
    /// The entry was replaced by a stub when a secret was purged (ADR-014): what it moved is gone.
    Purged {
        /// The number.
        number: u64,
    },
    /// The entry is that of a purge, which cannot be restored: it removed history for good
    /// (ADR-014).
    IsPurge {
        /// The number.
        number: u64,
    },
    /// A purge lists the entry but stopped before it replaced it by a stub, so the entry is about
    /// to be purged and names commits that the purge rewrote (ADR-014).
    PurgePending {
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
        /// The pointer: HEAD or the working-copy commit.
        pointer: Pointer,
    },
    /// A commit that a pointer takes is not in the object store, a name is in the way, a lock is
    /// held, or the references cannot be read. The restore wrote nothing.
    Refs(RefError),
    /// The snapshot failed (a refusal of the question `snapshot` is one), or a commit or a tree
    /// cannot be read. The restore wrote nothing.
    Snapshot(SnapshotError),
    /// A file is in the way, or the files cannot be planned. The restore wrote nothing.
    Checkout(CheckoutError),
    /// The files could not be brought to the old tree after the entry was written. The entry is in
    /// the log and names a restore that did not happen; no pointer moved, and the files done so
    /// far stay (see [`CheckoutError::Interrupted`]). The next snapshot saves them as a change.
    CheckoutFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: CheckoutError,
    },
    /// The entry was written and the files are at the old state, but the first pointer did not
    /// move: the file system failed.
    MoveFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: RefError,
    },
    /// The entry was written and the files are at the old state, but only some of the pointers
    /// moved: the file system failed.
    PartlyMoved {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why the next pointer did not move.
        source: RefError,
    },
    /// The restore was made, but a directory could not be flushed to disk, so it may not survive
    /// a crash.
    NotDurable {
        /// The entry that was written.
        entry: Box<Entry>,
        /// The error of the flush.
        source: RefError,
    },
    /// The restore was made (`restored` is its entry; `None` if there was nothing to restore), but
    /// the snapshot that came first moved the working-copy commit and a directory could not be
    /// flushed to disk, so the saved unsaved work may not survive a crash.
    SnapshotNotDurable {
        /// The entry of the snapshot.
        snapshot: Box<Entry>,
        /// The entry of the restore, if it was made.
        restored: Option<Box<Entry>>,
        /// The error of the flush.
        source: RefError,
    },
    /// The snapshot that comes first wrote an entry, and a later step failed with `source`.
    AfterSnapshot {
        /// The entry of the snapshot.
        snapshot: Box<Entry>,
        /// The error of the flush, if a directory of the snapshot could not be flushed: the saved
        /// unsaved work may not survive a crash.
        late: Option<RefError>,
        /// The failure.
        source: Box<RestoreError>,
    },
}

impl RestoreError {
    /// Returns the error of the file system, if there is one inside.
    pub fn io_error(&self) -> Option<&std::io::Error> {
        match self {
            Self::Update(error) => error.io_error(),
            Self::Log(error) => error.io_error(),
            Self::Refs(error)
            | Self::MoveFailed { source: error, .. }
            | Self::PartlyMoved { source: error, .. }
            | Self::NotDurable { source: error, .. }
            | Self::SnapshotNotDurable { source: error, .. } => error.io_error(),
            Self::Snapshot(error) => error.io_error(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.io_error(),
            Self::AfterSnapshot { source, .. } => source.io_error(),
            Self::NoSuchEntry { .. }
            | Self::Purged { .. }
            | Self::IsPurge { .. }
            | Self::PurgePending { .. }
            | Self::OutcomeUnknown { .. }
            | Self::NoOldValue { .. } => None,
        }
    }

    /// The entry of the restore, if the error leaves one in the log.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Update(error) => error.entry(),
            Self::CheckoutFailed { entry, .. }
            | Self::MoveFailed { entry, .. }
            | Self::PartlyMoved { entry, .. }
            | Self::NotDurable { entry, .. } => Some(entry),
            Self::SnapshotNotDurable { restored, .. } => restored.as_deref(),
            Self::AfterSnapshot { source, .. } => source.entry(),
            _ => None,
        }
    }

    /// Whether the restore is half made: some pointers moved and a later one did not, so that the
    /// same restore run again moves the rest.
    pub fn half_made(&self) -> bool {
        match self {
            Self::PartlyMoved { .. } => true,
            Self::AfterSnapshot { source, .. } => source.half_made(),
            _ => false,
        }
    }

    /// The entry of the snapshot that came first, if it wrote one and a later step failed.
    pub fn snapshot(&self) -> Option<&Entry> {
        match self {
            Self::AfterSnapshot { snapshot, .. } | Self::SnapshotNotDurable { snapshot, .. } => {
                Some(snapshot)
            }
            _ => None,
        }
    }

    /// Whether a pointer moved although the call failed: some pointers of the restore moved, or
    /// the restore was made and only a flush failed. A snapshot that came first and could not be
    /// flushed counts too ([`SnapshotNotDurable`](Self::SnapshotNotDurable)); a snapshot that
    /// came first and was followed by another failure does not (see
    /// [`snapshot`](Self::snapshot)).
    pub fn pointer_moved(&self) -> bool {
        match self {
            Self::PartlyMoved { .. }
            | Self::NotDurable { .. }
            | Self::SnapshotNotDurable { .. } => true,
            Self::AfterSnapshot { late, source, .. } => late.is_some() || source.pointer_moved(),
            _ => false,
        }
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
            Self::Purged { number } => write!(
                f,
                "entry {number} was replaced by a stub when a secret was purged, so what it moved \
                 is gone and it cannot be restored"
            ),
            Self::IsPurge { number } => write!(
                f,
                "entry {number} is a purge, which removed history for good and cannot be restored"
            ),
            Self::PurgePending { number } => write!(
                f,
                "entry {number} is listed by a purge that has not finished, so it is about to be \
                 purged and cannot be restored; finish the purge first"
            ),
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
            Self::Refs(error) => write!(f, "could not restore, nothing of it was written: {error}"),
            Self::Snapshot(error) => write!(
                f,
                "could not save the working directory first, so the restore wrote nothing: {error}"
            ),
            Self::Checkout(error) => {
                write!(f, "could not restore, nothing of it was written: {error}")
            }
            Self::CheckoutFailed { entry, source } => write!(
                f,
                "could not write the files; entry {} names a restore that did not happen and no \
                 pointer moved, and the files done so far stay: {source}",
                entry.number()
            ),
            Self::MoveFailed { entry, source } => write!(
                f,
                "entry {} was written and the files are at the old state, but no pointer moved: \
                 {source}; run the same restore again, as a snapshot would save the files as a \
                 change",
                entry.number()
            ),
            Self::PartlyMoved { entry, source } => write!(
                f,
                "entry {} was written and the files are at the old state, but only some of the \
                 pointers moved: {source}; run the same restore again, it moves only the pointers \
                 that have not moved",
                entry.number()
            ),
            Self::NotDurable { entry, source } => write!(
                f,
                "the restore was made as entry {}, but a directory could not be flushed, so it \
                 may not survive a crash: {source}",
                entry.number()
            ),
            Self::SnapshotNotDurable {
                snapshot,
                restored,
                source,
            } => {
                match restored {
                    Some(entry) => write!(f, "the restore was made as entry {}", entry.number())?,
                    None => write!(f, "there was nothing to restore")?,
                }
                write!(
                    f,
                    ", but the unsaved changes that were saved first as entry {} could not be \
                     flushed to disk and may not survive a crash: {source}",
                    snapshot.number()
                )
            }
            Self::AfterSnapshot {
                snapshot,
                late,
                source,
            } => {
                write!(
                    f,
                    "{source}; the unsaved changes were saved first as entry {}",
                    snapshot.number()
                )?;
                match late {
                    Some(late) => write!(
                        f,
                        ", but that could not be flushed to disk and may not survive a crash: {late}"
                    ),
                    None => Ok(()),
                }
            }
        }
    }
}

impl Error for RestoreError {
    /// The cause of the cause: `Display` includes the message of the wrapped error, so the chain
    /// leaves it out and continues behind it, as the other errors of the crate do.
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Update(error) => error.source(),
            Self::Log(error) => error.source(),
            Self::Refs(error)
            | Self::MoveFailed { source: error, .. }
            | Self::PartlyMoved { source: error, .. }
            | Self::NotDurable { source: error, .. }
            | Self::SnapshotNotDurable { source: error, .. } => error.source(),
            Self::Snapshot(error) => error.source(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.source(),
            Self::AfterSnapshot { source, .. } => source.source(),
            Self::NoSuchEntry { .. }
            | Self::Purged { .. }
            | Self::IsPurge { .. }
            | Self::PurgePending { .. }
            | Self::OutcomeUnknown { .. }
            | Self::NoOldValue { .. } => None,
        }
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
        repository.restore(number, false, &actor(), &AllowAll)
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
            .unwrap()
            .working_copy()
            .unwrap()
            .before()
            .unwrap();
        let count = entries.len();
        let (saved, entry) = done(restore(&repository, snapshot));
        assert!(saved.is_none(), "nothing was unsaved");
        assert_eq!(read(&dir, "a.txt"), "one");
        assert_eq!(repository.refs().working_copy().unwrap(), old);
        assert_eq!(entry.change().unwrap().operation().as_str(), "restore");
        assert_eq!(numbers(&repository).len(), count + 1);
        let moved = entry.change().unwrap().working_copy().unwrap();
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
        assert_eq!(saved.change().unwrap().operation().as_str(), "snapshot");
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
                    .unwrap()
                    .branches()
                    .iter()
                    .any(|(_, moved)| moved.before().is_none())
            })
            .unwrap()
            .number();
        done(restore(&repository, created));
        assert_eq!(repository.refs().branch(&main()).unwrap(), None);
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(main()),
            "HEAD still names the branch that is not there"
        );
    }

    #[test]
    fn a_plan_writes_nothing_and_reports_what_a_restore_would_do() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        write(&dir, "a.txt", "unsaved");
        let before = numbers(&repository);
        let plan = repository
            .plan_restore(snapshot, false, &actor(), &AllowAll)
            .unwrap()
            .expect("the pointers have other values");
        assert_eq!(plan.entry(), snapshot);
        assert!(plan.working_copy().is_some());
        let checkout = plan.checkout().expect("the working copy moves");
        let writes: Vec<&str> = checkout.writes().iter().map(|write| write.path()).collect();
        assert_eq!(writes, ["a.txt"]);
        assert_eq!(numbers(&repository), before, "not even a snapshot");
        assert_eq!(read(&dir, "a.txt"), "unsaved");
    }

    #[test]
    fn a_plan_for_pointers_that_are_there_already_is_none() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        done(restore(&repository, snapshot));
        let plan = repository
            .plan_restore(snapshot, false, &actor(), &AllowAll)
            .unwrap();
        assert!(plan.is_none());
    }

    #[test]
    fn pointers_that_have_the_old_values_already_are_left_alone() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        done(restore(&repository, snapshot));
        let before = numbers(&repository);
        let result = restore(&repository, snapshot).unwrap();
        assert!(
            matches!(result, Restored::NothingToRestore { snapshot: None }),
            "{result:?}"
        );
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
        assert!(
            matches!(
                error,
                RestoreError::NoOldValue {
                    pointer: Pointer::Head,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_refusal_of_the_authorizer_changes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        write(&dir, "a.txt", "unsaved");
        let before = numbers(&repository);
        let error = repository
            .restore(snapshot, false, &actor(), &DenyAll)
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
        assert!(matches!(&error, RestoreError::Checkout(_)), "{error}");
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
            .restore(snapshot + 1, true, &actor(), &AllowAll)
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
            .unwrap()
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

    fn op(name: &str) -> OperationName {
        OperationName::new(name).unwrap()
    }

    /// The tip of `main` and its parent, after `two_commits`.
    fn tip_and_first(repository: &Repository) -> (CommitId, CommitId) {
        let tip = repository.refs().branch(&main()).unwrap().unwrap();
        let first = read_commit(repository.objects(), tip).unwrap().parents()[0];
        (tip, first)
    }

    #[test]
    fn head_goes_back_to_the_branch_and_to_a_detached_commit() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        two_commits(&dir, &repository);
        let (tip, first) = tip_and_first(&repository);
        let detach = repository
            .set_head(&op("checkout"), &Head::Detached(first), &actor(), &AllowAll)
            .unwrap()
            .unwrap();
        let attach = repository
            .set_head(&op("checkout"), &Head::Branch(main()), &actor(), &AllowAll)
            .unwrap()
            .unwrap();
        // Undo the second move: HEAD is detached again, at `first`.
        done(restore(&repository, attach.number()));
        assert_eq!(repository.refs().head().unwrap(), Head::Detached(first));
        // Undo the first move: HEAD names the branch again.
        done(restore(&repository, detach.number()));
        assert_eq!(repository.refs().head().unwrap(), Head::Branch(main()));
        assert_eq!(repository.refs().branch(&main()).unwrap(), Some(tip));
    }

    #[test]
    fn a_detached_commit_that_the_store_lacks_is_found_before_anything_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        two_commits(&dir, &repository);
        let (_, first) = tip_and_first(&repository);
        repository
            .set_head(&op("checkout"), &Head::Detached(first), &actor(), &AllowAll)
            .unwrap();
        let attach = repository
            .set_head(&op("checkout"), &Head::Branch(main()), &actor(), &AllowAll)
            .unwrap()
            .unwrap();
        let path = repository.objects().object_path(&first.object_id());
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(&path, permissions).unwrap();
        fs::remove_file(&path).unwrap();
        write(&dir, "a.txt", "unsaved");
        let before = numbers(&repository);
        let error = restore(&repository, attach.number()).unwrap_err();
        assert!(
            matches!(error, RestoreError::Refs(RefError::MissingCommit(_))),
            "{error}"
        );
        assert_eq!(numbers(&repository), before, "not even a snapshot");
        assert_eq!(repository.refs().head().unwrap(), Head::Branch(main()));
    }

    #[test]
    fn a_commit_that_stopped_after_the_branch_can_be_restored() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (_, commit) = two_commits(&dir, &repository);
        let (_, first) = tip_and_first(&repository);
        let entry = repository.oplog().read(commit).unwrap();
        // The commit moved the branch and stopped before the working-copy commit moved.
        let wc_before = *entry
            .change()
            .unwrap()
            .working_copy()
            .unwrap()
            .before()
            .unwrap();
        fs::write(
            repository.metadata_dir().join("working-copy"),
            format!("{wc_before}\n"),
        )
        .unwrap();
        let result = restore(&repository, commit).unwrap();
        assert!(matches!(result, Restored::Done { .. }), "{result:?}");
        assert_eq!(repository.refs().branch(&main()).unwrap(), Some(first));
    }

    #[test]
    fn a_restore_puts_the_old_value_back_although_the_pointer_moved_on() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        two_commits(&dir, &repository);
        let (tip, first) = tip_and_first(&repository);
        let wc = repository.refs().working_copy().unwrap();
        let older = repository
            .update_branch(
                &op("branch"),
                &main(),
                Some(tip),
                first,
                &actor(),
                &AllowAll,
            )
            .unwrap()
            .unwrap();
        repository
            .update_branch(&op("branch"), &main(), Some(first), wc, &actor(), &AllowAll)
            .unwrap();
        let (_, entry) = done(restore(&repository, older.number()));
        assert_eq!(repository.refs().branch(&main()).unwrap(), Some(tip));
        let moved = &entry.change().unwrap().branches()[0].1;
        assert_eq!((moved.before(), moved.after()), (Some(&wc), Some(&tip)));
    }

    #[test]
    fn several_branches_of_one_entry_all_go_back() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        two_commits(&dir, &repository);
        let (tip, first) = tip_and_first(&repository);
        let (x, y) = (BranchName::new("x").unwrap(), BranchName::new("y").unwrap());
        for name in [&x, &y] {
            repository
                .update_branch(&op("branch"), name, None, tip, &actor(), &AllowAll)
                .unwrap();
        }
        // One entry that claims to have moved `x` from `first` and to have created `y`.
        let change = Change::new(
            now(),
            actor(),
            op("branch"),
            None,
            None,
            vec![
                (x.clone(), Moved::from_to(Some(first), Some(tip))),
                (y.clone(), Moved::from_to(None, Some(tip))),
            ],
        )
        .unwrap();
        let entry = repository.oplog().append(&change).unwrap();
        let (_, restored) = done(restore(&repository, entry.number()));
        assert_eq!(repository.refs().branch(&x).unwrap(), Some(first));
        assert_eq!(repository.refs().branch(&y).unwrap(), None);
        let names: Vec<&str> = restored
            .change()
            .unwrap()
            .branches()
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, ["x", "y"]);
    }

    #[test]
    fn a_sibling_branch_keeps_the_directory_when_a_branch_is_deleted() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        two_commits(&dir, &repository);
        let (tip, _) = tip_and_first(&repository);
        let a = BranchName::new("feature/a").unwrap();
        let b = BranchName::new("feature/b").unwrap();
        let created = repository
            .update_branch(&op("branch"), &a, None, tip, &actor(), &AllowAll)
            .unwrap()
            .unwrap();
        repository
            .update_branch(&op("branch"), &b, None, tip, &actor(), &AllowAll)
            .unwrap();
        done(restore(&repository, created.number()));
        assert_eq!(repository.refs().branch(&a).unwrap(), None);
        assert_eq!(repository.refs().branch(&b).unwrap(), Some(tip));
    }

    #[test]
    fn an_entry_without_a_working_copy_leaves_the_files_alone() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        two_commits(&dir, &repository);
        let (tip, first) = tip_and_first(&repository);
        let entry = repository
            .update_branch(
                &op("branch"),
                &main(),
                Some(tip),
                first,
                &actor(),
                &AllowAll,
            )
            .unwrap()
            .unwrap();
        write(&dir, "a.txt", "unsaved");
        let (saved, _) = done(restore(&repository, entry.number()));
        assert!(saved.is_some(), "the unsaved change is saved first");
        assert_eq!(read(&dir, "a.txt"), "unsaved");
        assert_eq!(repository.refs().branch(&main()).unwrap(), Some(tip));
    }

    #[test]
    fn a_lock_that_is_held_stops_the_restore_and_stays() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        let lock = repository.metadata_dir().join("refs.lock");
        fs::write(&lock, "someone").unwrap();
        let before = numbers(&repository);
        let error = restore(&repository, snapshot).unwrap_err();
        assert!(
            matches!(error, RestoreError::Refs(RefError::Locked { .. })),
            "{error}"
        );
        assert_eq!(numbers(&repository), before);
        assert_eq!(read(&dir, "a.txt"), "two");
        assert!(lock.exists(), "the lock of the other writer is not removed");
    }

    #[test]
    fn a_name_in_the_way_of_a_branch_that_comes_back_stops_the_restore() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        two_commits(&dir, &repository);
        let (tip, first) = tip_and_first(&repository);
        let below = BranchName::new("a/b").unwrap();
        repository
            .update_branch(&op("branch"), &below, None, tip, &actor(), &AllowAll)
            .unwrap();
        // An entry that claims to have moved `a` from `first`; `a` is a directory now.
        let a = BranchName::new("a").unwrap();
        let change = Change::new(
            now(),
            actor(),
            op("branch"),
            None,
            None,
            vec![(a, Moved::from_to(Some(first), Some(tip)))],
        )
        .unwrap();
        let entry = repository.oplog().append(&change).unwrap();
        let before = numbers(&repository);
        let error = repository
            .restore(entry.number(), true, &actor(), &AllowAll)
            .unwrap_err();
        assert!(
            matches!(error, RestoreError::Refs(RefError::Conflict { .. })),
            "{error}"
        );
        assert_eq!(numbers(&repository), before);
    }

    #[test]
    fn a_restore_that_stopped_after_the_files_can_be_run_again() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        let before = repository.refs().working_copy().unwrap();
        done(restore(&repository, snapshot));
        // The files are at the old state, but the working-copy commit is still the new one.
        fs::write(
            repository.metadata_dir().join("working-copy"),
            format!("{before}\n"),
        )
        .unwrap();
        let (_, entry) = done(restore(&repository, snapshot));
        assert_eq!(read(&dir, "a.txt"), "one");
        assert_eq!(entry.change().unwrap().operation().as_str(), "restore");
    }

    #[test]
    fn an_unsaved_change_is_named_when_a_later_step_fails() {
        let snapshot = {
            let dir = TempDir::new();
            let repository = repository(&dir);
            let (number, _) = two_commits(&dir, &repository);
            repository.oplog().read(number).unwrap()
        };
        let error = RestoreError::AfterSnapshot {
            snapshot: Box::new(snapshot.clone()),
            late: None,
            source: Box::new(RestoreError::Refs(RefError::Locked {
                path: "refs.lock".into(),
                holder: crate::refs::LockHolder::Unknown,
            })),
        };
        assert_eq!(error.snapshot().map(Entry::number), Some(snapshot.number()));
        assert!(error.entry().is_none());
        assert!(!error.pointer_moved());
        assert!(
            error.to_string().contains("saved first as entry"),
            "{error}"
        );
    }

    #[test]
    fn the_text_of_a_half_made_restore_says_what_to_do() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        let entry = Box::new(repository.oplog().read(snapshot).unwrap());
        let source = RefError::Locked {
            path: "x".into(),
            holder: crate::refs::LockHolder::Unknown,
        };
        let moved = RestoreError::PartlyMoved {
            entry: entry.clone(),
            source,
        };
        assert!(moved.pointer_moved());
        assert!(
            moved.to_string().contains("run the same restore again"),
            "{moved}"
        );
        let failed = RestoreError::MoveFailed {
            entry,
            source: RefError::Locked {
                path: "x".into(),
                holder: crate::refs::LockHolder::Unknown,
            },
        };
        assert!(!failed.pointer_moved());
        assert!(failed.entry().is_some());
    }

    #[test]
    fn a_snapshot_that_is_not_durable_is_named_when_a_later_step_fails() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        let entry = Box::new(repository.oplog().read(snapshot).unwrap());
        let late = RestoreError::AfterSnapshot {
            snapshot: entry.clone(),
            late: Some(RefError::Locked {
                path: "x".into(),
                holder: crate::refs::LockHolder::Unknown,
            }),
            source: Box::new(RestoreError::Refs(RefError::Locked {
                path: "y".into(),
                holder: crate::refs::LockHolder::Unknown,
            })),
        };
        assert!(
            late.pointer_moved(),
            "the saved work may not survive a crash"
        );
        assert!(
            late.to_string().contains("could not be flushed to disk"),
            "{late}"
        );
        let plain = RestoreError::AfterSnapshot {
            snapshot: entry,
            late: None,
            source: Box::new(RestoreError::Refs(RefError::Locked {
                path: "y".into(),
                holder: crate::refs::LockHolder::Unknown,
            })),
        };
        assert!(!plain.pointer_moved());
        assert!(!plain.to_string().contains("flushed"), "{plain}");
    }

    #[test]
    fn a_made_restore_with_a_snapshot_that_is_not_durable_is_an_error_with_both_entries() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        let entry = Box::new(repository.oplog().read(snapshot).unwrap());
        let made = RestoreError::SnapshotNotDurable {
            snapshot: entry.clone(),
            restored: Some(entry.clone()),
            source: RefError::Locked {
                path: "x".into(),
                holder: crate::refs::LockHolder::Unknown,
            },
        };
        assert!(made.pointer_moved());
        assert_eq!(made.snapshot().map(Entry::number), Some(snapshot));
        assert_eq!(made.entry().map(Entry::number), Some(snapshot));
        assert!(
            made.to_string()
                .starts_with("the restore was made as entry"),
            "{made}"
        );
        let nothing = RestoreError::SnapshotNotDurable {
            snapshot: entry,
            restored: None,
            source: RefError::Locked {
                path: "x".into(),
                holder: crate::refs::LockHolder::Unknown,
            },
        };
        assert!(nothing.entry().is_none());
        assert!(
            nothing
                .to_string()
                .starts_with("there was nothing to restore"),
            "{nothing}"
        );
    }

    #[test]
    fn a_stub_and_a_purge_are_refused_and_an_entry_before_them_needs_force() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, commit) = two_commits(&dir, &repository);
        let log = repository.oplog();
        let purge = log
            .append_purge(crate::oplog::Purge::new(1, actor(), 1, &[commit]).unwrap())
            .unwrap()[0]
            .number();
        log.replace_with_stub(commit).unwrap();

        assert!(matches!(
            restore(&repository, commit),
            Err(RestoreError::Purged { number }) if number == commit
        ));
        assert!(matches!(
            restore(&repository, purge),
            Err(RestoreError::IsPurge { number }) if number == purge
        ));
        let count = numbers(&repository).len();
        assert_eq!(count, purge as usize, "the refusals wrote nothing");
        // The stub took the evidence that the next move of the working copy gave (ADR-014).
        assert!(matches!(
            restore(&repository, snapshot),
            Err(RestoreError::OutcomeUnknown { .. })
        ));
        done(repository.restore(snapshot, true, &actor(), &AllowAll));
        assert_eq!(read(&dir, "a.txt"), "one");
    }

    #[test]
    fn an_entry_that_an_unfinished_purge_lists_is_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (snapshot, _) = two_commits(&dir, &repository);
        repository
            .oplog()
            .append_purge(crate::oplog::Purge::new(1, actor(), 1, &[snapshot]).unwrap())
            .unwrap();
        let count = numbers(&repository).len();
        assert!(matches!(
            repository.restore(snapshot, true, &actor(), &AllowAll),
            Err(RestoreError::PurgePending { number }) if number == snapshot
        ));
        assert_eq!(numbers(&repository).len(), count, "nothing was written");
    }
}
