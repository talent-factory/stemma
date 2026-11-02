//! Changing the branch: `switch` (ADR-010).
//!
//! A switch takes a snapshot, builds a new, empty working-copy commit on the tip of the target, and
//! moves HEAD, the working-copy commit, and (for a new branch) the branch in one entry of the log.
//! The directory is brought to the tree of the target by the checkout layer (ADR-013), in the order
//! of a restore: plan, entry, files, pointers. Only an empty working-copy commit can be left; a
//! working-copy commit that holds changes is refused, and the user commits first.

use std::error::Error;
use std::fmt;
use std::io;

use super::checkout::{CheckoutError, CheckoutPlan};
use super::snapshot::{read_commit, SnapshotError};
use super::{now, Repository, UpdateError, WriteLock};
use crate::change::{ChangeId, GenerateError};
use crate::index::Index;
use crate::object::{Commit, CommitId, ObjectId, TreeId};
use crate::oplog::{Change, Entry, Moved, OpLogError, OperationName};
use crate::refs::{BranchName, Head, Logged, RefError, RestoreMoves, RestoreTargets};
use crate::store::{ObjectStore, StoreError};
use crate::{Actor, Authorizer};

/// The operation that the log records for a switch.
const OPERATION: &str = "switch";

/// A name for a commit in the arguments of a switch: the tip of a branch, or a commit by its ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revision {
    /// The commit at the tip of the branch.
    Branch(BranchName),
    /// A commit. The ID is a claim until the store confirms it.
    Commit(ObjectId),
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Branch(name) => name.fmt(f),
            Self::Commit(id) => id.fmt(f),
        }
    }
}

/// What a switch is asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchTarget {
    /// `switch <name>`: HEAD names the existing branch.
    Existing(BranchName),
    /// `switch -c <name> [<start>]`: the branch is made at `start` (by default the commit that HEAD
    /// leads to) and HEAD names it.
    Create {
        /// The name of the new branch.
        name: BranchName,
        /// Where the branch starts.
        start: Option<Revision>,
    },
    /// `switch --detach <revision>`: HEAD names the commit and not a branch.
    Detach(Revision),
}

/// How a switch ended.
#[derive(Debug)]
#[must_use]
pub enum Switched {
    /// HEAD names the branch already. Nothing was read or written, and the log has no entry.
    AlreadyOn(BranchName),
    /// HEAD, the working-copy commit, and (for a new branch) the branch moved.
    Done {
        /// The entry of the snapshot that came first, if the directory had unsaved changes.
        snapshot: Option<Box<Entry>>,
        /// The entry of the switch.
        entry: Box<Entry>,
        /// The value of HEAD afterwards.
        head: Head,
        /// The commit at the target: the tip of the branch, or the detached commit.
        commit: CommitId,
        /// The new working-copy commit, a child of `commit`.
        working_copy: CommitId,
    },
}

/// What is decided before anything is written.
struct Resolved {
    /// The value of HEAD afterwards.
    head: Head,
    /// The commit at the target, the parent of the new working-copy commit.
    tip: CommitId,
    /// The branch that the switch makes, with its commit.
    created: Option<(BranchName, CommitId)>,
}

impl Resolved {
    fn branches(&self) -> Vec<(BranchName, Option<CommitId>)> {
        self.created
            .iter()
            .map(|(name, commit)| (name.clone(), Some(*commit)))
            .collect()
    }
}

impl Repository {
    /// Changes the branch that HEAD names, or detaches HEAD (ADR-010).
    ///
    /// The steps are: the authorizer is asked for `switch`; the target is resolved and the checks
    /// that need no write are made, among them the plan of the files, so that a file of the
    /// directory that is in the way is found before the first write; the lock of the references is
    /// taken and held to the last move (ADR-014); the tracked files are saved in a snapshot (a
    /// second question to the authorizer, `snapshot`); the working-copy commit must be empty (its
    /// tree is that of its first parent, or empty if it has none); a new empty working-copy commit
    /// with a new change ID is made on the tip of the target; the plan is made again from the
    /// values the lock sees, the entry is written and flushed, the directory is brought to the
    /// tree of the target, and the pointers move: the new branch first, then the working-copy
    /// commit, and HEAD last (ADR-013). One entry holds all moves.
    ///
    /// A switch to the branch that HEAD names is [`Switched::AlreadyOn`] and writes nothing. If the
    /// trees are equal, only the pointers move.
    ///
    /// If the newest entry of the log is a switch with the same result that did not take effect
    /// (the files were written and a pointer did not move), the call is its repeat: it takes no
    /// snapshot, as one would save the files of the target as changes of the old working-copy
    /// commit, and it uses the working-copy commit of that entry again.
    ///
    /// # Errors
    ///
    /// Before the snapshot: [`SwitchError::Update`] for a refusal of the question `switch`,
    /// [`SwitchError::NoSuchBranch`], [`SwitchError::NoSuchCommit`], [`SwitchError::BranchExists`],
    /// [`SwitchError::NoStart`], [`SwitchError::Refs`], [`SwitchError::Store`],
    /// [`SwitchError::Read`] for a commit that cannot be read, and [`SwitchError::Checkout`]
    /// for a file in the way. Nothing is written then. After the snapshot, the same errors can
    /// come again, and [`SwitchError::NotEmpty`] if the working-copy commit holds changes; they are
    /// wrapped in [`SwitchError::AfterSnapshot`] when the snapshot wrote an entry. Then
    /// [`SwitchError::CheckoutFailed`], [`SwitchError::MoveFailed`],
    /// [`SwitchError::PartlyMoved`], and [`SwitchError::NotDurable`] say how far the switch came.
    pub fn switch(
        &self,
        target: &SwitchTarget,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Switched, SwitchError> {
        let operation =
            OperationName::try_new(OPERATION).expect("`switch` is a valid operation name");
        self.refs_store()
            .refuse_pending_purge()
            .map_err(SwitchError::Refs)?;
        let allowed = self
            .authorize(actor, authorizer, &operation, &target_name(target))
            .map_err(SwitchError::Update)?;
        let resolved = match self.resolve(target)? {
            Ok(resolved) => resolved,
            Err(already) => return Ok(Switched::AlreadyOn(already)),
        };
        self.preflight(&resolved)?;
        let lock = self
            .refs_store()
            .lock_writer(&allowed)
            .map_err(SwitchError::Refs)?;
        // The target is read again under the lock: another command may have moved a branch since
        // the first read, and the new working-copy commit is built on this tip.
        let resolved = match self.resolve(target)? {
            Ok(resolved) => resolved,
            Err(already) => return Ok(Switched::AlreadyOn(already)),
        };
        // A switch that stopped after its files is repeated without a snapshot (see above).
        let stopped = self.stopped_switch(&resolved)?;
        if stopped.is_none() {
            self.require_free(target)?;
        }
        let (snapshot, mut snapshot_late, repeated) = match stopped {
            // The repeat takes no snapshot, so the rule of the owner is applied here (ADR-010).
            Some(next) => {
                self.own_for_repeat(&lock, actor, authorizer)?;
                (None, None, Some(next))
            }
            None => match self.snapshot_locked(&lock, actor, authorizer) {
                Ok(entry) => (entry, None, None),
                // The snapshot was made and the pointer moved; only the flush is not known. The
                // switch goes on, as the unsaved work is in the log, and reports it at the end.
                Err(SnapshotError::Update(UpdateError::NotDurable { entry, source })) => {
                    (Some(*entry), Some(source), None)
                }
                Err(error) => return Err(SwitchError::Snapshot(error)),
            },
        };
        let snapshot = snapshot.map(Box::new);
        #[cfg(test)]
        super::snapshot::pause::reached(super::snapshot::pause::Stage::Moves);
        let wrap = |error: SwitchError, late: Option<RefError>| match &snapshot {
            Some(saved) => SwitchError::AfterSnapshot {
                snapshot: saved.clone(),
                late,
                source: Box::new(error),
            },
            None => error,
        };
        let next = match repeated {
            Some(next) => next,
            None => match self.start_working_copy(&resolved, actor) {
                Ok(next) => next,
                Err(error) => return Err(wrap(error, snapshot_late.take())),
            },
        };
        let branches = resolved.branches();
        let targets = RestoreTargets {
            head: Some(&resolved.head),
            working_copy: Some(next),
            branches: &branches,
        };
        let logged = self
            .refs_store()
            .restore_after(&lock, &targets, self.objects(), |moves| {
                let plan = self.plan_files(moves)?;
                let change = Change::try_new(
                    now(),
                    actor.clone(),
                    operation.clone(),
                    moves
                        .head
                        .as_ref()
                        .map(|(old, new)| Moved::from_to(Some(old.clone()), Some(new.clone()))),
                    moves
                        .working_copy
                        .as_ref()
                        .map(|(old, new)| Moved::from_to(Some(*old), Some(*new))),
                    moves
                        .branches
                        .iter()
                        .map(|(name, old, new)| (name.clone(), Moved::from_to(*old, *new)))
                        .collect(),
                )
                .map_err(|error| SwitchError::Update(UpdateError::Log(OpLogError::Entry(error))))?;
                let entry = self
                    .append(&lock, &change)
                    .map_err(|error| SwitchError::Update(UpdateError::Log(error)))?;
                if let Some(plan) = &plan {
                    self.apply_checkout(plan)
                        .map_err(|source| SwitchError::CheckoutFailed {
                            entry: Box::new(entry.clone()),
                            source,
                        })?;
                }
                Ok(entry)
            });
        let made = |entry: Entry| Switched::Done {
            snapshot: snapshot.clone(),
            entry: Box::new(entry),
            head: resolved.head.clone(),
            commit: resolved.tip,
            working_copy: next,
        };
        match logged {
            Err(error) => Err(wrap(SwitchError::Refs(error), snapshot_late.take())),
            Ok(Logged::Unchanged) => {
                unreachable!("the new working-copy commit differs from the old one")
            }
            Ok(Logged::Moved(entry)) => match (&snapshot, snapshot_late) {
                (Some(saved), Some(source)) => Err(SwitchError::SnapshotNotDurable {
                    snapshot: saved.clone(),
                    switched: Some(Box::new(entry)),
                    source,
                }),
                _ => Ok(made(entry)),
            },
            Ok(Logged::StepFailed(error)) => Err(wrap(error, snapshot_late.take())),
            Ok(Logged::MoveFailed(entry, source)) => Err(wrap(
                SwitchError::MoveFailed {
                    entry: Box::new(entry),
                    source,
                },
                snapshot_late.take(),
            )),
            Ok(Logged::PartlyMoved(entry, source)) => Err(wrap(
                SwitchError::PartlyMoved {
                    entry: Box::new(entry),
                    source,
                },
                snapshot_late.take(),
            )),
            Ok(Logged::NotFlushed(entry, source)) => Err(wrap(
                SwitchError::NotDurable {
                    entry: Box::new(entry),
                    source,
                },
                snapshot_late.take(),
            )),
        }
    }

    /// Reads the target. The inner `Err` is the branch that HEAD names already.
    fn resolve(&self, target: &SwitchTarget) -> Result<Result<Resolved, BranchName>, SwitchError> {
        let refs = self.refs();
        let head = refs.head().map_err(SwitchError::Refs)?;
        match target {
            SwitchTarget::Existing(name) => {
                if head == Head::Branch(name.clone()) {
                    return Ok(Err(name.clone()));
                }
                let tip = refs
                    .branch(name)
                    .map_err(SwitchError::Refs)?
                    .ok_or_else(|| SwitchError::NoSuchBranch(name.clone()))?;
                Ok(Ok(Resolved {
                    head: Head::Branch(name.clone()),
                    tip,
                    created: None,
                }))
            }
            SwitchTarget::Create { name, start } => {
                let tip = match start {
                    Some(revision) => self.commit_of(revision)?,
                    None => match &head {
                        Head::Detached(commit) => *commit,
                        Head::Branch(current) => {
                            refs.branch(current)
                                .map_err(SwitchError::Refs)?
                                .ok_or_else(|| SwitchError::NoStart(current.clone()))?
                        }
                    },
                };
                Ok(Ok(Resolved {
                    head: Head::Branch(name.clone()),
                    tip,
                    created: Some((name.clone(), tip)),
                }))
            }
            SwitchTarget::Detach(revision) => {
                let tip = self.commit_of(revision)?;
                Ok(Ok(Resolved {
                    head: Head::Detached(tip),
                    tip,
                    created: None,
                }))
            }
        }
    }

    /// Refuses `switch -c` for a branch that exists. A switch that stopped after it made the
    /// branch is not refused: [`stopped_switch`](Self::stopped_switch) is asked first.
    fn require_free(&self, target: &SwitchTarget) -> Result<(), SwitchError> {
        if let SwitchTarget::Create { name, .. } = target {
            if self
                .refs()
                .branch(name)
                .map_err(SwitchError::Refs)?
                .is_some()
            {
                return Err(SwitchError::BranchExists(name.clone()));
            }
        }
        Ok(())
    }

    /// The commit that a revision names, which the store has.
    fn commit_of(&self, revision: &Revision) -> Result<CommitId, SwitchError> {
        match revision {
            Revision::Branch(name) => self
                .refs()
                .branch(name)
                .map_err(SwitchError::Refs)?
                .ok_or_else(|| SwitchError::NoSuchBranch(name.clone())),
            Revision::Commit(id) => self
                .objects()
                .resolve_commit(id)
                .map_err(|error| SwitchError::Store(Box::new(error)))?
                .ok_or(SwitchError::NoSuchCommit(*id)),
        }
    }

    /// The checks that write nothing: a name in the way of the new branch, and, if the
    /// working-copy commit is empty, the plan of the files, so that a file that is in the way is
    /// found before the snapshot. A working-copy commit that holds changes is refused later, after
    /// its snapshot, so that the message comes after the changes are saved.
    fn preflight(&self, resolved: &Resolved) -> Result<(), SwitchError> {
        let branches = resolved.branches();
        let targets = RestoreTargets {
            head: Some(&resolved.head),
            working_copy: None,
            branches: &branches,
        };
        self.refs_store()
            .plan_restore(&targets, self.objects())
            .map_err(SwitchError::Refs)?;
        let current = self.refs().working_copy().map_err(SwitchError::Refs)?;
        let working = read_commit(self.objects(), current).map_err(SwitchError::Read)?;
        if self.is_empty(&working)? {
            let from = working.tree();
            let to = self.tip_tree(resolved.tip)?;
            self.plan_checkout(Some(from), to)
                .map_err(SwitchError::Checkout)?;
        }
        Ok(())
    }

    /// Whether the commit holds no change: its tree is that of its first parent, or the empty tree
    /// if it has none (the rule of `status`).
    fn is_empty(&self, working: &Commit) -> Result<bool, SwitchError> {
        let before = match working.parents().first() {
            Some(parent) => read_commit(self.objects(), *parent)
                .map_err(SwitchError::Read)?
                .tree(),
            None => Index::new()
                .write_tree(self.objects())
                .map_err(|error| SwitchError::Read(SnapshotError::WriteTree(error)))?,
        };
        Ok(working.tree() == before)
    }

    fn tip_tree(&self, commit: CommitId) -> Result<TreeId, SwitchError> {
        read_commit(self.objects(), commit)
            .map(|commit| commit.tree())
            .map_err(SwitchError::Read)
    }

    /// The new working-copy commit: the tree of the tip, the tip as only parent, an empty
    /// message, and a change ID of its own. The old working-copy commit must be empty.
    fn start_working_copy(
        &self,
        resolved: &Resolved,
        actor: &Actor,
    ) -> Result<CommitId, SwitchError> {
        let current = self.refs().working_copy().map_err(SwitchError::Refs)?;
        let working = read_commit(self.objects(), current).map_err(SwitchError::Read)?;
        if !self.is_empty(&working)? {
            return Err(SwitchError::NotEmpty { working: current });
        }
        let next = Commit::try_new(
            self.tip_tree(resolved.tip)?,
            vec![resolved.tip],
            ChangeId::generate().map_err(SwitchError::ChangeId)?,
            actor.clone(),
            now(),
            "",
        )
        .expect("a single parent is not repeated");
        self.objects()
            .put_commit(&next)
            .map_err(|error| SwitchError::Store(Box::new(error)))
    }

    /// The working-copy commit of the newest entry if that entry is a switch with the result that
    /// `resolved` asks for and that did not finish: every pointer it names is at its old or at its
    /// new value, and at least one is at its old value. Such an entry was written, and the files
    /// were brought to the target, but a pointer did not move (a stop after the files, or after
    /// some of the pointers). The caller holds the lock of the references, so no writer is between
    /// its entry and its moves.
    fn stopped_switch(&self, resolved: &Resolved) -> Result<Option<CommitId>, SwitchError> {
        let entries = self.oplog().entries().map_err(SwitchError::Log)?;
        let Some(change) = entries.last().and_then(Entry::change) else {
            return Ok(None);
        };
        if change.operation().as_str() != OPERATION {
            return Ok(None);
        }
        let refs = self.refs();
        let current = refs.working_copy().map_err(SwitchError::Refs)?;
        let head = refs.head().map_err(SwitchError::Refs)?;
        let Some(moved) = change.working_copy() else {
            return Ok(None);
        };
        let (Some(&old), Some(&next)) = (moved.before(), moved.after()) else {
            return Ok(None);
        };
        // The result of the entry must be the one that is asked for.
        if change.head().map_or(Some(&head), Moved::after) != Some(&resolved.head) {
            return Ok(None);
        }
        let created: Vec<_> = change
            .branches()
            .iter()
            .map(|(name, moved)| (name.clone(), moved.after().copied()))
            .collect();
        if created != resolved.branches() {
            return Ok(None);
        }
        // Each pointer is at its old or its new value, and one is not at the new one yet.
        let mut unfinished = false;
        if current == old {
            unfinished = true;
        } else if current != next {
            return Ok(None);
        }
        if let Some(moved) = change.head() {
            if Some(&head) == moved.before() {
                unfinished = true;
            } else if Some(&head) != moved.after() {
                return Ok(None);
            }
        }
        for (name, moved) in change.branches() {
            let found = refs.branch(name).map_err(SwitchError::Refs)?;
            if found == moved.before().copied() {
                unfinished = true;
            } else if found != moved.after().copied() {
                return Ok(None);
            }
        }
        if !unfinished {
            return Ok(None);
        }
        // The commit is in the store, as the entry was written after it, so a failure to read it
        // is damage and not "no repeat": going on would take the snapshot that a repeat avoids.
        let commit = read_commit(self.objects(), next).map_err(SwitchError::Read)?;
        Ok((commit.parents() == [resolved.tip]).then_some(next))
    }

    /// The check of the owner that the snapshot makes, for a repeat that takes none: another
    /// actor does not finish the switch of an owner without `--take-over`, and an agent never
    /// takes over.
    fn own_for_repeat(
        &self,
        lock: &WriteLock,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<(), SwitchError> {
        let (_, owner) = self
            .refs_store()
            .working_copy_and_owner()
            .map_err(SwitchError::Refs)?;
        if self
            .check_owner(actor, owner.as_ref())
            .map_err(SwitchError::Snapshot)?
        {
            let operation = self
                .ask_take_over(actor, authorizer)
                .map_err(SwitchError::Snapshot)?;
            self.take_over_locked(lock, &operation, actor)
                .map_err(SwitchError::Snapshot)?;
        }
        Ok(())
    }

    /// The plan of the files for the moves under the lock, or `None` if the working-copy commit
    /// does not move.
    fn plan_files(&self, moves: &RestoreMoves) -> Result<Option<CheckoutPlan>, SwitchError> {
        match &moves.working_copy {
            Some((current, target)) => {
                let from = self.tip_tree(*current)?;
                let to = self.tip_tree(*target)?;
                self.plan_checkout(Some(from), to)
                    .map(Some)
                    .map_err(SwitchError::Checkout)
            }
            None => Ok(None),
        }
    }
}

/// The target of the question to the authorizer: the branch, or `HEAD` for a detached HEAD.
fn target_name(target: &SwitchTarget) -> String {
    match target {
        SwitchTarget::Existing(name) | SwitchTarget::Create { name, .. } => name.to_string(),
        SwitchTarget::Detach(_) => "HEAD".to_owned(),
    }
}

/// Why [`Repository::switch`] fails. The variants before `CheckoutFailed` wrote nothing of the
/// switch; the others wrote its entry.
#[derive(Debug)]
#[non_exhaustive]
pub enum SwitchError {
    /// The authorizer refused `switch`, or the entry could not be written (see
    /// [`UpdateError::Log`]). No pointer moved and no file was written.
    Update(UpdateError),
    /// The references could not be read, a lock is held, a name is in the way of the new branch, or
    /// a commit is not in the store.
    Refs(RefError),
    /// The log could not be read.
    Log(OpLogError),
    /// A commit could not be looked up or stored.
    Store(Box<StoreError>),
    /// The snapshot failed, or the owner of the working copy is not the actor.
    Snapshot(SnapshotError),
    /// A commit or a tree could not be read or made. Nothing of the switch was written.
    Read(SnapshotError),
    /// A file is in the way, or the files cannot be planned. Nothing was written.
    Checkout(CheckoutError),
    /// The operating system gave no random bytes for the change ID.
    ChangeId(GenerateError),
    /// There is no branch with the name (`switch -c` makes one).
    NoSuchBranch(BranchName),
    /// The store has no commit with the ID.
    NoSuchCommit(ObjectId),
    /// `switch -c` names a branch that exists.
    BranchExists(BranchName),
    /// `switch -c` has no start: no start is given and the branch of HEAD has no commit yet.
    NoStart(BranchName),
    /// The working-copy commit holds changes, which a switch would leave behind.
    NotEmpty {
        /// The working-copy commit.
        working: CommitId,
    },
    /// The files could not be brought to the tree of the target after the entry was written. No
    /// pointer moved, and the files done so far stay; the same switch run again finishes it.
    CheckoutFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: CheckoutError,
    },
    /// The entry was written and the files are at the target, but the first pointer did not move.
    MoveFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: RefError,
    },
    /// The entry was written and the files are at the target, but only some of the pointers moved.
    PartlyMoved {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why the next pointer did not move.
        source: RefError,
    },
    /// The switch was made, but a directory could not be flushed to disk, so it may not survive a
    /// crash.
    NotDurable {
        /// The entry that was written.
        entry: Box<Entry>,
        /// The error of the flush.
        source: RefError,
    },
    /// The switch was made (`switched` is its entry), but the snapshot that came first moved the
    /// working-copy commit and a directory could not be flushed to disk.
    SnapshotNotDurable {
        /// The entry of the snapshot.
        snapshot: Box<Entry>,
        /// The entry of the switch.
        switched: Option<Box<Entry>>,
        /// The error of the flush.
        source: RefError,
    },
    /// The snapshot that comes first wrote an entry, and a later step failed with `source`.
    AfterSnapshot {
        /// The entry of the snapshot.
        snapshot: Box<Entry>,
        /// The error of the flush, if a directory of the snapshot could not be flushed.
        late: Option<RefError>,
        /// The failure.
        source: Box<SwitchError>,
    },
}

impl SwitchError {
    /// Returns the error of the file system, if there is one inside.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Update(error) => error.io_error(),
            Self::Log(error) => error.io_error(),
            Self::Refs(error)
            | Self::MoveFailed { source: error, .. }
            | Self::PartlyMoved { source: error, .. }
            | Self::NotDurable { source: error, .. }
            | Self::SnapshotNotDurable { source: error, .. } => error.io_error(),
            Self::Store(error) => error.io_error(),
            Self::Snapshot(error) | Self::Read(error) => error.io_error(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.io_error(),
            Self::AfterSnapshot { source, .. } => source.io_error(),
            Self::ChangeId(_)
            | Self::NoSuchBranch(_)
            | Self::NoSuchCommit(_)
            | Self::BranchExists(_)
            | Self::NoStart(_)
            | Self::NotEmpty { .. } => None,
        }
    }

    /// The entry of the switch, if the error leaves one in the log.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Update(error) => error.entry(),
            Self::CheckoutFailed { entry, .. }
            | Self::MoveFailed { entry, .. }
            | Self::PartlyMoved { entry, .. }
            | Self::NotDurable { entry, .. } => Some(entry),
            Self::SnapshotNotDurable { switched, .. } => switched.as_deref(),
            Self::AfterSnapshot { source, .. } => source.entry(),
            _ => None,
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

    /// Whether the switch took effect and only the flush failed: the result is to be reported and
    /// the switch is not to be repeated.
    pub fn made(&self) -> bool {
        match self {
            Self::NotDurable { .. } => true,
            Self::SnapshotNotDurable { switched, .. } => switched.is_some(),
            Self::AfterSnapshot { source, .. } => source.made(),
            _ => false,
        }
    }

    /// Whether a pointer moved although the call failed: some pointers of the switch moved, or the
    /// switch was made and only a flush failed.
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

impl fmt::Display for SwitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Update(error) => write!(f, "could not switch: {error}"),
            Self::Refs(error) => write!(f, "could not switch, nothing of it was written: {error}"),
            Self::Log(error) => write!(f, "could not read the operation log: {error}"),
            Self::Store(error) => write!(f, "could not look up a commit: {error}"),
            Self::Snapshot(error) if error.stands_alone() => error.fmt(f),
            Self::Snapshot(error) => write!(
                f,
                "could not save the working directory first, so the switch wrote nothing: {error}"
            ),
            Self::Read(error) => write!(f, "could not read a commit or a tree: {error}"),
            Self::Checkout(error) => {
                write!(f, "could not switch, nothing of it was written: {error}")
            }
            Self::ChangeId(error) => write!(f, "could not make a change ID: {error}"),
            Self::NoSuchBranch(name) => write!(
                f,
                "there is no branch `{name}`; `stemma switch -c {name}` makes one"
            ),
            Self::NoSuchCommit(id) => write!(f, "the store has no commit {id}"),
            Self::BranchExists(name) => write!(
                f,
                "the branch `{name}` exists already; `stemma switch {name}` changes to it"
            ),
            Self::NoStart(name) => write!(
                f,
                "the branch `{name}` has no commit yet, so a new branch has no start; commit \
                 first or name a start"
            ),
            Self::NotEmpty { working } => write!(
                f,
                "the working-copy commit {working} holds changes, which a switch would leave \
                 behind; commit them first with `stemma commit`"
            ),
            Self::CheckoutFailed { entry, source } => write!(
                f,
                "could not write the files; entry {} names a switch that did not happen and no \
                 pointer moved, and the files done so far stay: {source}; run the same switch \
                 again before any other command, as a snapshot saves only the files that the \
                 working-copy commit tracks and would miss the files that only the target has",
                entry.number()
            ),
            Self::MoveFailed { entry, source } => write!(
                f,
                "entry {} was written and the files are at the target, but no pointer moved: \
                 {source}; run the same switch again before any other command, as a snapshot \
                 saves only the files that the working-copy commit tracks and would miss the \
                 files that only the target has",
                entry.number()
            ),
            Self::PartlyMoved { entry, source } => write!(
                f,
                "entry {} was written and the files are at the target, but only some of the \
                 pointers moved: {source}; run the same switch again before any other command, \
                 it moves only the pointers that have not moved",
                entry.number()
            ),
            Self::NotDurable { entry, source } => write!(
                f,
                "the switch was made as entry {}, but a directory could not be flushed, so it \
                 may not survive a crash: {source}",
                entry.number()
            ),
            Self::SnapshotNotDurable {
                snapshot,
                switched,
                source,
            } => {
                match switched {
                    Some(entry) => write!(f, "the switch was made as entry {}", entry.number())?,
                    None => write!(f, "there was nothing to switch")?,
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
                        ", but that could not be flushed to disk and may not survive a crash: \
                         {late}"
                    ),
                    None => Ok(()),
                }
            }
        }
    }
}

impl Error for SwitchError {
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
            Self::Store(error) => error.source(),
            Self::Snapshot(error) | Self::Read(error) => error.source(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.source(),
            Self::ChangeId(error) => error.source(),
            Self::AfterSnapshot { source, .. } => source.source(),
            Self::NoSuchBranch(_)
            | Self::NoSuchCommit(_)
            | Self::BranchExists(_)
            | Self::NoStart(_)
            | Self::NotEmpty { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::rc::Rc;

    use super::*;
    use crate::fault;
    use crate::repository::{CommitError, InitOptions};
    use crate::testutil::TempDir;
    use crate::{AllowAll, Decision};

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &OperationName, _target: &str) -> Decision {
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

    fn name(text: &str) -> BranchName {
        BranchName::try_new(text).unwrap()
    }

    fn track_and_commit(repository: &Repository, paths: &[&str], message: &str) {
        let paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();
        repository.track(&paths, &actor(), &AllowAll).unwrap();
        repository
            .commit(message, &actor(), &AllowAll)
            .map(drop)
            .unwrap();
    }

    fn switch(repository: &Repository, target: &SwitchTarget) -> Result<Switched, SwitchError> {
        repository.switch(target, &actor(), &AllowAll)
    }

    fn done(result: Result<Switched, SwitchError>) -> (Option<Entry>, Entry, CommitId) {
        match result {
            Ok(Switched::Done {
                snapshot,
                entry,
                working_copy,
                ..
            }) => (snapshot.map(|entry| *entry), *entry, working_copy),
            other => panic!("expected a switch, got {other:?}"),
        }
    }

    fn create(branch: &str) -> SwitchTarget {
        SwitchTarget::Create {
            name: name(branch),
            start: None,
        }
    }

    fn existing(branch: &str) -> SwitchTarget {
        SwitchTarget::Existing(name(branch))
    }

    fn entries(repository: &Repository) -> usize {
        repository.oplog().entries().unwrap().len()
    }

    /// `a.txt` is "one" and committed as "first" on `main`. The working-copy commit is empty.
    fn first_commit(dir: &TempDir, repository: &Repository) {
        write(dir, "a.txt", "one");
        track_and_commit(repository, &["a.txt"], "first");
    }

    #[test]
    fn a_switch_to_an_existing_branch_brings_its_files_and_starts_an_empty_working_copy() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        first_commit(&dir, &repository);
        let first = repository.refs().branch(&name("main")).unwrap().unwrap();
        done(switch(&repository, &create("feature")));
        write(&dir, "a.txt", "two");
        repository
            .commit("second", &actor(), &AllowAll)
            .map(drop)
            .unwrap();
        let old = repository.refs().working_copy().unwrap();
        let count = entries(&repository);

        let (saved, entry, next) = done(switch(&repository, &existing("main")));

        assert!(saved.is_none(), "nothing was unsaved");
        assert_eq!(read(&dir, "a.txt"), "one");
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(name("main"))
        );
        assert_eq!(repository.refs().working_copy().unwrap(), next);
        assert_eq!(entries(&repository), count + 1, "one entry per command");
        assert_eq!(entry.change().unwrap().operation().as_str(), "switch");
        let working = read_commit(repository.objects(), next).unwrap();
        let before = read_commit(repository.objects(), old).unwrap();
        assert_eq!(working.parents(), [first]);
        assert_eq!(
            working.tree(),
            read_commit(repository.objects(), first).unwrap().tree()
        );
        assert_eq!(working.message(), "");
        assert_ne!(working.change(), before.change(), "a new change ID");
        // The next snapshot has nothing to save.
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    #[test]
    fn a_new_branch_starts_at_the_commit_of_head_and_moves_only_the_pointers() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        first_commit(&dir, &repository);
        let first = repository.refs().branch(&name("main")).unwrap().unwrap();

        let (_, entry, next) = done(switch(&repository, &create("feature")));

        assert_eq!(
            repository.refs().branch(&name("feature")).unwrap(),
            Some(first)
        );
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(name("feature"))
        );
        assert_eq!(read(&dir, "a.txt"), "one");
        let change = entry.change().unwrap();
        assert_eq!(change.branches().len(), 1);
        assert_eq!(change.branches()[0].1.before(), None);
        assert_eq!(change.branches()[0].1.after(), Some(&first));
        assert_eq!(change.working_copy().unwrap().after(), Some(&next));
        assert!(change.head().is_some());
    }

    #[test]
    fn a_new_branch_can_start_at_another_branch_or_at_a_commit() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        first_commit(&dir, &repository);
        let first = repository.refs().branch(&name("main")).unwrap().unwrap();
        write(&dir, "a.txt", "two");
        repository
            .commit("second", &actor(), &AllowAll)
            .map(drop)
            .unwrap();

        let at_commit = SwitchTarget::Create {
            name: name("old"),
            start: Some(Revision::Commit(first.object_id())),
        };
        done(switch(&repository, &at_commit));
        assert_eq!(read(&dir, "a.txt"), "one");
        assert_eq!(repository.refs().branch(&name("old")).unwrap(), Some(first));

        let at_branch = SwitchTarget::Create {
            name: name("again"),
            start: Some(Revision::Branch(name("main"))),
        };
        done(switch(&repository, &at_branch));
        assert_eq!(read(&dir, "a.txt"), "two");
    }

    #[test]
    fn a_detached_switch_names_the_commit_and_a_commit_is_refused_there() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        first_commit(&dir, &repository);
        let first = repository.refs().branch(&name("main")).unwrap().unwrap();
        write(&dir, "a.txt", "two");
        repository
            .commit("second", &actor(), &AllowAll)
            .map(drop)
            .unwrap();

        done(switch(
            &repository,
            &SwitchTarget::Detach(Revision::Commit(first.object_id())),
        ));

        assert_eq!(repository.refs().head().unwrap(), Head::Detached(first));
        assert_eq!(read(&dir, "a.txt"), "one");
        write(&dir, "a.txt", "three");
        let error = repository
            .commit("third", &actor(), &AllowAll)
            .map(drop)
            .unwrap_err();
        assert!(
            matches!(error.failure(), CommitError::DetachedHead(id) if *id == first),
            "{error}"
        );
        assert!(error.to_string().contains("switch -c"), "{error}");
    }

    #[test]
    fn a_working_copy_with_changes_is_refused_after_its_changes_are_saved() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        first_commit(&dir, &repository);
        done(switch(&repository, &create("feature")));
        write(&dir, "a.txt", "scratch");
        let count = entries(&repository);
        let head = repository.refs().head().unwrap();

        let error = switch(&repository, &existing("main")).unwrap_err();

        let SwitchError::AfterSnapshot { source, .. } = &error else {
            panic!("expected a refusal after the snapshot, got {error}");
        };
        assert!(matches!(**source, SwitchError::NotEmpty { .. }), "{error}");
        assert!(error.to_string().contains("commit them first"), "{error}");
        assert!(error.snapshot().is_some());
        assert_eq!(entries(&repository), count + 1, "only the snapshot");
        assert_eq!(repository.refs().head().unwrap(), head);
        assert_eq!(read(&dir, "a.txt"), "scratch");
        // The edit is in the log, not lost.
        let saved = repository.refs().working_copy().unwrap();
        assert_eq!(
            read_commit(repository.objects(), saved)
                .unwrap()
                .parents()
                .len(),
            1
        );
    }

    #[test]
    fn an_untracked_file_in_the_way_is_found_before_anything_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        first_commit(&dir, &repository);
        done(switch(&repository, &create("feature")));
        write(&dir, "b.txt", "tracked on feature");
        track_and_commit(&repository, &["b.txt"], "second");
        done(switch(&repository, &existing("main")));
        assert!(!dir.path().join("b.txt").exists());
        write(&dir, "b.txt", "mine");
        let count = entries(&repository);
        let pointers = (
            repository.refs().head().unwrap(),
            repository.refs().working_copy().unwrap(),
        );

        let error = switch(&repository, &existing("feature")).unwrap_err();

        assert!(matches!(error, SwitchError::Checkout(_)), "{error}");
        assert_eq!(entries(&repository), count, "not even a snapshot");
        assert_eq!(read(&dir, "b.txt"), "mine");
        assert_eq!(
            pointers,
            (
                repository.refs().head().unwrap(),
                repository.refs().working_copy().unwrap()
            )
        );
    }

    #[test]
    fn a_switch_to_the_branch_of_head_writes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        first_commit(&dir, &repository);
        let count = entries(&repository);

        let result = switch(&repository, &existing("main")).unwrap();

        assert!(matches!(result, Switched::AlreadyOn(ref branch) if branch.as_str() == "main"));
        assert_eq!(entries(&repository), count);
    }

    #[test]
    fn an_unknown_branch_and_an_existing_name_are_refused_without_an_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        first_commit(&dir, &repository);
        done(switch(&repository, &create("feature")));
        let count = entries(&repository);

        let unknown = switch(&repository, &existing("nope")).unwrap_err();
        assert!(matches!(unknown, SwitchError::NoSuchBranch(_)), "{unknown}");
        assert!(unknown.to_string().contains("switch -c nope"), "{unknown}");
        let exists = switch(&repository, &create("main")).unwrap_err();
        assert!(matches!(exists, SwitchError::BranchExists(_)), "{exists}");
        let no_commit = switch(
            &repository,
            &SwitchTarget::Detach(Revision::Commit(
                format!("b3:{}", "7".repeat(64))
                    .parse::<ObjectId>()
                    .unwrap(),
            )),
        )
        .unwrap_err();
        assert!(
            matches!(no_commit, SwitchError::NoSuchCommit(_)),
            "{no_commit}"
        );
        assert_eq!(entries(&repository), count);
    }

    #[test]
    fn a_new_branch_without_a_commit_to_start_at_is_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let error = switch(&repository, &create("feature")).unwrap_err();
        assert!(matches!(error, SwitchError::NoStart(_)), "{error}");
    }

    #[test]
    fn a_refusal_by_the_authorizer_changes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        first_commit(&dir, &repository);
        let count = entries(&repository);
        let error = repository
            .switch(&create("feature"), &actor(), &DenyAll)
            .unwrap_err();
        assert!(
            matches!(error, SwitchError::Update(UpdateError::Denied { .. })),
            "{error}"
        );
        assert_eq!(entries(&repository), count);
        assert_eq!(repository.refs().branch(&name("feature")).unwrap(), None);
    }

    /// A repository on `main` with `new.rs` in "first" and without it in "second", and the
    /// working-copy commit empty on "second".
    fn two_commits(dir: &TempDir, repository: &Repository) -> CommitId {
        write(dir, "a.txt", "one");
        write(dir, "new.rs", "fn main() {}");
        track_and_commit(repository, &["a.txt", "new.rs"], "first");
        let first = repository.refs().branch(&name("main")).unwrap().unwrap();
        fs::remove_file(dir.path().join("new.rs")).unwrap();
        write(dir, "a.txt", "two");
        repository
            .commit("second", &actor(), &AllowAll)
            .map(drop)
            .unwrap();
        first
    }

    #[test]
    fn a_stop_after_the_files_says_to_repeat_the_same_switch_and_the_repeat_finishes_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let first = two_commits(&dir, &repository);
        let target = SwitchTarget::Create {
            name: name("old"),
            start: Some(Revision::Commit(first.object_id())),
        };
        let before = repository.refs().working_copy().unwrap();
        // The branch is the first pointer that moves, and `refs/heads` refuses the new name.
        let heads = repository.metadata_dir().join("refs").join("heads");
        let denied = Rc::new(RefCell::new(None));
        let arm = Rc::clone(&denied);
        super::super::snapshot::pause::at_next(
            super::super::snapshot::pause::Stage::Moves,
            move || {
                *arm.borrow_mut() = Some(fault::deny_writes_in(&heads));
            },
        );

        let error = switch(&repository, &target).unwrap_err();
        drop(denied);

        assert!(matches!(error, SwitchError::MoveFailed { .. }), "{error}");
        assert!(!error.pointer_moved());
        let message = error.to_string();
        assert!(message.contains("run the same switch again"), "{message}");
        assert!(message.contains("before any other command"), "{message}");
        let stopped = error.entry().unwrap().number();
        let planned = error
            .entry()
            .unwrap()
            .change()
            .unwrap()
            .working_copy()
            .unwrap()
            .after()
            .copied()
            .unwrap();
        assert_eq!(
            read(&dir, "new.rs"),
            "fn main() {}",
            "the files are at the target"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(repository.refs().branch(&name("old")).unwrap(), None);

        // The repeat takes no snapshot, which would save the target as changes, and it uses the
        // working-copy commit of the stopped entry again.
        let (saved, entry, next) = done(switch(&repository, &target));

        assert!(saved.is_none());
        assert!(entry.number() > stopped);
        assert_eq!(next, planned);
        assert_eq!(repository.refs().working_copy().unwrap(), next);
        assert_eq!(repository.refs().branch(&name("old")).unwrap(), Some(first));
        assert_eq!(repository.refs().head().unwrap(), Head::Branch(name("old")));
        assert_eq!(read(&dir, "a.txt"), "one");
        assert_eq!(read(&dir, "new.rs"), "fn main() {}");
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    #[test]
    fn a_switch_is_restored_like_any_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let first = two_commits(&dir, &repository);
        let target = SwitchTarget::Detach(Revision::Commit(first.object_id()));
        let head = repository.refs().head().unwrap();
        let (_, entry, _) = done(switch(&repository, &target));
        assert_eq!(read(&dir, "new.rs"), "fn main() {}");

        repository
            .restore(entry.number(), false, &actor(), &AllowAll)
            .unwrap();

        assert_eq!(repository.refs().head().unwrap(), head);
        assert_eq!(read(&dir, "a.txt"), "two");
        assert!(!dir.path().join("new.rs").exists());
    }
    /// Runs a switch to a new branch `old` at the first commit that stops after the files
    /// (`refs/heads` refuses the new name). Returns the first commit.
    fn stopped_switch_to_old(dir: &TempDir, repository: &Repository) -> (CommitId, SwitchTarget) {
        let first = two_commits(dir, repository);
        let target = SwitchTarget::Create {
            name: name("old"),
            start: Some(Revision::Commit(first.object_id())),
        };
        let heads = repository.metadata_dir().join("refs").join("heads");
        let denied = Rc::new(RefCell::new(None));
        let arm = Rc::clone(&denied);
        super::super::snapshot::pause::at_next(
            super::super::snapshot::pause::Stage::Moves,
            move || {
                *arm.borrow_mut() = Some(fault::deny_writes_in(&heads));
            },
        );
        let error = switch(repository, &target).unwrap_err();
        drop(denied);
        assert!(matches!(error, SwitchError::MoveFailed { .. }), "{error}");
        (first, target)
    }

    #[test]
    fn a_different_switch_after_a_stop_is_no_repeat_and_takes_a_snapshot() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (first, _) = stopped_switch_to_old(&dir, &repository);
        let count = entries(&repository);

        // Another name for the same start: not the result of the stopped entry.
        let other = SwitchTarget::Create {
            name: name("other"),
            start: Some(Revision::Commit(first.object_id())),
        };
        let error = switch(&repository, &other).unwrap_err();

        assert!(error.snapshot().is_some(), "a snapshot was taken: {error}");
        assert!(
            matches!(&error, SwitchError::AfterSnapshot { source, .. }
                if matches!(**source, SwitchError::NotEmpty { .. })),
            "{error}"
        );
        assert_eq!(entries(&repository), count + 1);
    }

    #[test]
    fn a_stopped_switch_is_no_repeat_once_another_entry_follows_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (_, target) = stopped_switch_to_old(&dir, &repository);
        // Any later entry, here a snapshot of the files of the target, ends the repeat.
        repository.snapshot(&actor(), &AllowAll).unwrap();

        let error = switch(&repository, &target).unwrap_err();

        assert!(matches!(error, SwitchError::NotEmpty { .. }), "{error}");
    }

    #[test]
    fn a_stopped_switch_whose_working_copy_commit_cannot_be_read_is_damage_and_no_repeat() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (_, target) = stopped_switch_to_old(&dir, &repository);
        let planned = repository
            .oplog()
            .entries()
            .unwrap()
            .last()
            .and_then(|entry| entry.change().and_then(Change::working_copy).cloned())
            .and_then(|moved| moved.after().copied())
            .unwrap();
        let path = dir.path().join(".stemma").join("objects");
        let hex = planned.to_string().replace(':', "");
        let object = walk(&path, &hex[hex.len() - 12..]);
        fs::remove_file(object).unwrap();
        let count = entries(&repository);

        let error = switch(&repository, &target).unwrap_err();

        assert!(matches!(error, SwitchError::Read(_)), "{error}");
        assert_eq!(entries(&repository), count, "no snapshot was taken");
    }

    #[test]
    fn a_switch_that_stopped_after_the_branch_moved_is_finished_by_the_same_switch() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (first, target) = stopped_switch_to_old(&dir, &repository);
        let planned = repository
            .oplog()
            .entries()
            .unwrap()
            .last()
            .and_then(|entry| entry.change().and_then(Change::working_copy).cloned())
            .and_then(|moved| moved.after().copied())
            .unwrap();
        // The state after a stop that came after the first move: the branch is made.
        let branch = repository
            .metadata_dir()
            .join("refs")
            .join("heads")
            .join("old");
        fs::write(branch, format!("{first}\n")).unwrap();

        let (saved, _, next) = done(switch(&repository, &target));

        assert!(saved.is_none());
        assert_eq!(next, planned);
        assert_eq!(repository.refs().working_copy().unwrap(), next);
        assert_eq!(repository.refs().head().unwrap(), Head::Branch(name("old")));
    }

    #[test]
    fn a_repeat_is_refused_for_an_actor_that_does_not_own_the_working_copy() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (_, target) = stopped_switch_to_old(&dir, &repository);
        let count = entries(&repository);
        let other = Actor::human("mara").unwrap();

        let error = repository.switch(&target, &other, &AllowAll).unwrap_err();

        assert!(
            matches!(error, SwitchError::Snapshot(SnapshotError::NotOwner { .. })),
            "{error}"
        );
        assert_eq!(entries(&repository), count, "nothing was written");
        // The owner finishes it.
        done(switch(&repository, &target));
    }

    #[test]
    fn a_stop_while_the_files_are_written_leaves_the_pointers_and_the_repeat_finishes_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let first = two_commits(&dir, &repository);
        let target = existing_at(&first);
        let before = repository.refs().working_copy().unwrap();
        // Writing `new.rs` into the working directory fails.
        let root = dir.path().to_path_buf();
        let denied = Rc::new(RefCell::new(None));
        let arm = Rc::clone(&denied);
        super::super::snapshot::pause::at_next(
            super::super::snapshot::pause::Stage::Moves,
            move || {
                *arm.borrow_mut() = Some(fault::deny_writes_in(&root));
            },
        );

        let error = switch(&repository, &target).unwrap_err();
        drop(denied);

        assert!(
            matches!(error, SwitchError::CheckoutFailed { .. }),
            "{error}"
        );
        assert!(!error.pointer_moved());
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        let (saved, _, next) = done(switch(&repository, &target));
        assert!(saved.is_none(), "no snapshot of half-written files");
        assert_eq!(repository.refs().working_copy().unwrap(), next);
        assert_eq!(read(&dir, "new.rs"), "fn main() {}");
    }

    /// A new branch `old` at `first`, as a detached switch would not name a branch.
    fn existing_at(first: &CommitId) -> SwitchTarget {
        SwitchTarget::Create {
            name: name("old"),
            start: Some(Revision::Commit(first.object_id())),
        }
    }

    #[test]
    fn a_switch_from_a_detached_head_to_a_branch_makes_commit_work_again() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let first = two_commits(&dir, &repository);
        done(switch(
            &repository,
            &SwitchTarget::Detach(Revision::Commit(first.object_id())),
        ));
        assert_eq!(repository.refs().head().unwrap(), Head::Detached(first));

        done(switch(&repository, &existing("main")));

        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(name("main"))
        );
        assert!(!dir.path().join("new.rs").exists());
        write(&dir, "a.txt", "three");
        repository
            .commit("third", &actor(), &AllowAll)
            .map(drop)
            .unwrap();
    }

    #[test]
    fn a_new_branch_from_a_detached_head_starts_at_the_detached_commit() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let first = two_commits(&dir, &repository);
        done(switch(
            &repository,
            &SwitchTarget::Detach(Revision::Commit(first.object_id())),
        ));

        done(switch(&repository, &create("topic")));

        assert_eq!(
            repository.refs().branch(&name("topic")).unwrap(),
            Some(first)
        );
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(name("topic"))
        );
    }

    #[test]
    fn a_switch_that_finished_is_no_repeat_and_a_new_branch_of_that_name_is_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (_, target) = stopped_switch_to_old(&dir, &repository);
        done(switch(&repository, &target));
        done(switch(&repository, &existing("main")));

        let error = switch(&repository, &target).unwrap_err();

        assert!(matches!(error, SwitchError::BranchExists(_)), "{error}");
    }

    /// The file below `dir` whose name ends with `suffix`.
    fn walk(dir: &std::path::Path, suffix: &str) -> std::path::PathBuf {
        fn find(dir: &std::path::Path, suffix: &str) -> Option<std::path::PathBuf> {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if let Some(found) = find(&path, suffix) {
                        return Some(found);
                    }
                } else if path.to_string_lossy().ends_with(suffix) {
                    return Some(path);
                }
            }
            None
        }
        find(dir, suffix).unwrap_or_else(|| panic!("no file ending with {suffix}"))
    }
}
