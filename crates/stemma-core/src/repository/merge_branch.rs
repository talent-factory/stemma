//! Merging a branch into the current one (ADR-010).
//!
//! [`Repository::merge`] joins the tip of another branch into the branch that HEAD names. If the
//! other tip is an ancestor of the current one, nothing is to do. If the current tip is an
//! ancestor of the other one, the branch fast-forwards: it takes the other tip, and no commit is
//! made. Otherwise the call makes a merge commit with the current tip and the other tip as its
//! parents, whose tree is the merge of the two over their merge bases ([`merge_commits`]). A
//! conflict does not stop the merge: the merge commit records it as data (ADR-005), and the
//! result lists its paths.
//!
//! The working-copy commit follows the new tip by the shared mechanism of
//! [`Repository::rewrite_working_copy`] (the carry of ADR-010). The move of the branch and the
//! move of the working-copy commit are one operation with one entry in the log, in the order of
//! ADR-013: snapshot, plan of the files, entry, files, pointers. The branch moves by
//! compare-and-swap: it has to be at the tip that the merge was made from.

use std::error::Error;
use std::fmt;
use std::io;

use super::rewrite::{Carried, RewriteError};
use super::snapshot::{read_any_commit, SnapshotError};
use super::{now, Repository, UpdateError, WriteLock};
use crate::change::{ChangeId, GenerateError};
use crate::conflict::{AnyCommit, FromSumError};
use crate::merge::{merge_commits, MergeError};
use crate::merge_base::merge_bases;
use crate::object::{CommitId, Kind};
use crate::oplog::{Entry, Moved, OpLogError, OperationName, TAKE_OVER};
use crate::refs::{BranchMove, BranchName, Head, Logged, RefError, WorkingCopyMove};
use crate::store::{ObjectStore, StoreError};
use crate::{Actor, Authorizer};

/// The operation that the log records for a merge.
const OPERATION: &str = "merge";

/// How the branch got the other tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeKind {
    /// The branch took the other tip, and no commit was made.
    FastForward,
    /// A merge commit with two parents was made.
    MergeCommit,
}

/// How a merge ended.
#[derive(Debug)]
#[must_use]
pub enum Merged {
    /// The other branch is an ancestor of the current one. Nothing but the snapshot was written,
    /// and the log has no entry of a merge.
    UpToDate {
        /// The entry of the snapshot, if the directory held unsaved changes.
        snapshot: Option<Box<Entry>>,
    },
    /// The branch and the working-copy commit moved.
    Done {
        /// The entry of the snapshot that came first, if the directory had unsaved changes.
        snapshot: Option<Box<Entry>>,
        /// The entry of the merge.
        entry: Box<Entry>,
        /// The branch that moved: the one that HEAD names.
        branch: BranchName,
        /// The branch that was merged in.
        merged: BranchName,
        /// Whether the merge fast-forwarded or made a commit.
        kind: MergeKind,
        /// The commit that the branch names now.
        commit: CommitId,
        /// The new working-copy commit, a child of `commit`.
        working_copy: CommitId,
        /// The paths that are an open conflict in the new working-copy commit, sorted. Empty if
        /// the merge was clean.
        conflicts: Vec<String>,
    },
}

/// What a merge did, as far as a report needs it.
#[derive(Debug)]
pub struct MergeResult {
    /// The branch that moved.
    pub branch: BranchName,
    /// Whether the merge fast-forwarded or made a commit.
    pub kind: MergeKind,
    /// The commit that the branch names now.
    pub commit: CommitId,
    /// The paths that are an open conflict in the new working-copy commit, sorted.
    pub conflicts: Vec<String>,
}

/// What is decided before anything is written under the lock.
struct Plan {
    /// The tip of the branch before the merge.
    ours: CommitId,
    /// The tip of the branch after the merge.
    tip: CommitId,
    kind: MergeKind,
    /// The working-copy commit before the merge.
    current: CommitId,
    carried: Carried,
    /// Whether the branch is to move: false for the repeat of a merge that moved it already.
    branch_pending: bool,
}

/// A merge that stopped after its files were written, read from the newest entry of the log.
struct Stopped {
    old_tip: CommitId,
    new_tip: CommitId,
    next: CommitId,
    kind: MergeKind,
    branch_pending: bool,
}

impl Repository {
    /// Merges the branch `other` into the branch that HEAD names (ADR-010).
    ///
    /// The steps are: the authorizer is asked for `merge` twice, for the branch that moves and for `other`; the lock of the
    /// references is taken and held to the last move (ADR-014); the tracked files are saved in a
    /// snapshot (a second question, `snapshot`); the new tip is decided and, for a merge commit,
    /// made; the working-copy commit is carried onto it; then the plan of the files, the entry,
    /// the files, and the pointers follow in the order of ADR-013, the branch first. One entry
    /// holds both moves.
    ///
    /// `message` is the message of the merge commit; by default it is `Merge <other> into
    /// <branch>`. A fast-forward and "up to date" make no commit and do not use it.
    ///
    /// If the newest entry of the log is a merge into the same branch that did not take effect
    /// (the files were written and a pointer did not move), the call is its repeat: it takes no
    /// snapshot, as one would save the new files as changes of the old working-copy commit, and it
    /// uses the commits of that entry again, so a different `message` is not used. It checks the
    /// owner of the working copy instead (ADR-010). If the stopped entry is another merge, the
    /// call is refused with [`MergeBranchError::Stopped`].
    ///
    /// # Errors
    ///
    /// Before anything of the merge is written (the message is checked first, even for a
    /// fast-forward or "up to date"): [`MergeBranchError::EmptyMessage`],
    /// [`MergeBranchError::DetachedHead`], [`MergeBranchError::NoSuchBranch`],
    /// [`MergeBranchError::NoCommit`], [`MergeBranchError::Update`] for a refusal of the question
    /// `merge`, [`MergeBranchError::Refs`] for a lock that another command holds or a purge that
    /// is pending, [`MergeBranchError::Log`], and [`MergeBranchError::Stopped`].
    ///
    /// [`MergeBranchError::Snapshot`] is a refusal of the snapshot or, for a repeat, of the owner
    /// check. After the snapshot, the errors of the preparation (a commit or a tree that cannot be
    /// read, the merge of the trees, the carry of the working-copy commit) wrote nothing but
    /// objects and, if the snapshot wrote an entry, that entry; they are wrapped in
    /// [`MergeBranchError::AfterSnapshot`] then. [`MergeBranchError::MoveFailed`],
    /// [`MergeBranchError::PartlyMoved`], [`MergeBranchError::NotDurable`], and a
    /// [`MergeBranchError::Rewrite`] that holds a failure of the files say how far the merge
    /// came.
    pub fn merge(
        &self,
        other: &BranchName,
        message: Option<&str>,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Merged, MergeBranchError> {
        if message.is_some_and(|text| text.trim().is_empty()) {
            return Err(MergeBranchError::EmptyMessage);
        }
        let operation =
            OperationName::try_new(OPERATION).expect("`merge` is a valid operation name");
        let name = match self.refs().head().map_err(MergeBranchError::Refs)? {
            Head::Branch(name) => name,
            Head::Detached(commit) => return Err(MergeBranchError::DetachedHead(commit)),
        };
        self.refs_store()
            .refuse_pending_purge()
            .map_err(MergeBranchError::Refs)?;
        // The branch that is written comes first and gives the proof for the lock; the branch
        // that is read is asked for as well, as `rename` asks for both names.
        let allowed = self
            .authorize(actor, authorizer, &operation, name.as_str())
            .map_err(MergeBranchError::Update)?;
        let _source = self
            .authorize(actor, authorizer, &operation, other.as_str())
            .map_err(MergeBranchError::Update)?;
        if self
            .refs()
            .branch(other)
            .map_err(MergeBranchError::Refs)?
            .is_none()
        {
            return Err(MergeBranchError::NoSuchBranch(other.clone()));
        }
        let lock = self
            .refs_store()
            .lock_writer(&allowed)
            .map_err(MergeBranchError::Refs)?;
        // A merge that stopped after its files is repeated without a snapshot (see above).
        let stopped = self.stopped_merge(&name, other)?;
        let (snapshot, repeated) = match stopped {
            Some(stopped) => {
                self.own_for_merge_repeat(&lock, actor, authorizer)?;
                (None, Some(stopped))
            }
            None => match self.snapshot_locked(&lock, actor, authorizer) {
                Ok(entry) => (entry.map(Box::new), None),
                // The snapshot was made and the pointer moved; only the flush is not known.
                Err(SnapshotError::Update(UpdateError::NotDurable { entry, source })) => {
                    return Err(MergeBranchError::SnapshotNotDurable {
                        snapshot: entry,
                        source,
                    });
                }
                // The entry of a snapshot that failed to move the pointer stays reachable.
                Err(SnapshotError::Update(error)) => return Err(MergeBranchError::Update(error)),
                Err(error) => return Err(MergeBranchError::Snapshot(error)),
            },
        };
        #[cfg(test)]
        super::snapshot::pause::reached(super::snapshot::pause::Stage::Moves);
        let wrap = |error: MergeBranchError| match &snapshot {
            Some(saved) => MergeBranchError::AfterSnapshot {
                snapshot: saved.clone(),
                source: Box::new(error),
            },
            None => error,
        };
        let plan = match self
            .plan_merge(&name, other, message, repeated, actor)
            .map_err(wrap)?
        {
            Some(plan) => plan,
            None => return Ok(Merged::UpToDate { snapshot }),
        };
        let branch_moved = vec![(
            name.clone(),
            Moved::from_to(Some(plan.ours), Some(plan.tip)),
        )];
        let logged = if plan.branch_pending {
            self.refs_store().update_branch_and_working_copy_after(
                &lock,
                &BranchMove {
                    name: &name,
                    expected: Some(plan.ours),
                    new: plan.tip,
                },
                &WorkingCopyMove {
                    expected: plan.current,
                    new: plan.carried.id,
                },
                self.objects(),
                || {
                    self.write_carried(
                        &lock,
                        &operation,
                        plan.current,
                        &plan.carried,
                        branch_moved,
                        actor,
                    )
                },
            )
        } else {
            self.refs_store().update_working_copy_after(
                &lock,
                plan.current,
                plan.carried.id,
                self.objects(),
                || {
                    self.write_carried(
                        &lock,
                        &operation,
                        plan.current,
                        &plan.carried,
                        // The entry of a repeat names the branch move too, so that a repeat that
                        // stops again is found again, and `op restore` undoes both moves.
                        branch_moved,
                        actor,
                    )
                },
            )
        };
        match logged {
            Err(error) => Err(wrap(MergeBranchError::Refs(error))),
            Ok(Logged::Moved(entry)) => Ok(Merged::Done {
                snapshot,
                entry: Box::new(entry),
                branch: name,
                merged: other.clone(),
                kind: plan.kind,
                commit: plan.tip,
                working_copy: plan.carried.id,
                conflicts: plan.carried.conflicts,
            }),
            Ok(Logged::Unchanged) => Ok(Merged::UpToDate { snapshot }),
            Ok(Logged::StepFailed(error)) => Err(wrap(MergeBranchError::Rewrite(error))),
            Ok(Logged::MoveFailed(entry, source)) => Err(wrap(MergeBranchError::MoveFailed {
                entry: Box::new(entry),
                source,
            })),
            Ok(Logged::PartlyMoved(entry, source)) => Err(wrap(MergeBranchError::PartlyMoved {
                entry: Box::new(entry),
                source,
            })),
            Ok(Logged::NotFlushed(entry, source)) => Err(wrap(MergeBranchError::NotDurable {
                entry: Box::new(entry),
                source,
                result: Box::new(MergeResult {
                    branch: name,
                    kind: plan.kind,
                    commit: plan.tip,
                    conflicts: plan.carried.conflicts,
                }),
            })),
        }
    }

    /// Decides the new tip and makes the commits, under the lock. Writes objects and nothing
    /// else. `None` means that the other branch is an ancestor of the current one already.
    fn plan_merge(
        &self,
        name: &BranchName,
        other: &BranchName,
        message: Option<&str>,
        repeated: Option<Stopped>,
        actor: &Actor,
    ) -> Result<Option<Plan>, MergeBranchError> {
        let objects = self.objects();
        let current = self.refs().working_copy().map_err(MergeBranchError::Refs)?;
        if let Some(stopped) = repeated {
            let carried = self
                .carried(stopped.next)
                .map_err(MergeBranchError::Rewrite)?;
            return Ok(Some(Plan {
                ours: stopped.old_tip,
                tip: stopped.new_tip,
                kind: stopped.kind,
                current,
                carried,
                branch_pending: stopped.branch_pending,
            }));
        }
        let ours = self
            .refs()
            .branch(name)
            .map_err(MergeBranchError::Refs)?
            .ok_or_else(|| MergeBranchError::NoCommit(name.clone()))?;
        let theirs = self
            .refs()
            .branch(other)
            .map_err(MergeBranchError::Refs)?
            .ok_or_else(|| MergeBranchError::NoSuchBranch(other.clone()))?;
        let bases = merge_bases(objects, ours, theirs)
            .map_err(|error| MergeBranchError::Merge(MergeError::from(error)))?;
        if bases == [theirs] {
            return Ok(None);
        }
        let (tip, kind) = if bases == [ours] {
            (theirs, MergeKind::FastForward)
        } else {
            let sum = merge_commits(objects, ours, theirs).map_err(MergeBranchError::Merge)?;
            let text = message.map_or_else(|| format!("Merge {other} into {name}"), str::to_owned);
            let commit = AnyCommit::from_sum(
                &sum,
                vec![ours, theirs],
                ChangeId::generate().map_err(MergeBranchError::ChangeId)?,
                actor.clone(),
                now(),
                text,
            )
            .map_err(MergeBranchError::Invalid)?;
            let id = match &commit {
                AnyCommit::Commit(commit) => objects.put_commit(commit),
                AnyCommit::Commit2(commit) => objects
                    .put_raw(Kind::Commit2, &commit.encode())
                    .map(CommitId::new),
            }
            .map_err(|error| MergeBranchError::Store(Box::new(error)))?;
            (id, MergeKind::MergeCommit)
        };
        let working = read_any_commit(objects, current).map_err(MergeBranchError::Read)?;
        let carried = self
            .carry(&working, tip, actor)
            .map_err(MergeBranchError::Rewrite)?;
        Ok(Some(Plan {
            ours,
            tip,
            kind,
            current,
            carried,
            branch_pending: true,
        }))
    }

    /// The merge of the newest entry, if that entry is a merge that did not finish: the
    /// working-copy commit is at its old value, the files were written, and the branch is at its
    /// old or its new value. The caller holds the lock of the references, so no writer came
    /// between the entry and its moves.
    ///
    /// If the entry merged `other` into `name`, this is the repeat. If it is another merge, the
    /// call is refused with [`MergeBranchError::Stopped`].
    fn stopped_merge(
        &self,
        name: &BranchName,
        other: &BranchName,
    ) -> Result<Option<Stopped>, MergeBranchError> {
        let entries = self.oplog().entries().map_err(MergeBranchError::Log)?;
        // A `take-over` names no pointer: the repeat of another actor writes one before its own
        // entry, and must not hide a stopped merge from a second repeat.
        let Some(change) = entries
            .iter()
            .rev()
            .filter_map(Entry::change)
            .find(|change| change.operation().as_str() != TAKE_OVER)
        else {
            return Ok(None);
        };
        if change.operation().as_str() != OPERATION {
            return Ok(None);
        }
        let Some(moved) = change.working_copy() else {
            return Ok(None);
        };
        let (Some(&old), Some(&next)) = (moved.before(), moved.after()) else {
            return Ok(None);
        };
        let current = self.refs().working_copy().map_err(MergeBranchError::Refs)?;
        if current != old {
            return Ok(None);
        }
        let refused = Err(MergeBranchError::Stopped { stopped: next });
        let [(branch, moved_branch)] = change.branches() else {
            // A repeat of a stopped merge that moved the working-copy commit only has no branch.
            return Ok(None);
        };
        let (Some(&old_tip), Some(&new_tip)) = (moved_branch.before(), moved_branch.after()) else {
            return refused;
        };
        let found = self.refs().branch(branch).map_err(MergeBranchError::Refs)?;
        let branch_pending = found == Some(old_tip);
        if branch != name || !(branch_pending || found == Some(new_tip)) {
            return refused;
        }
        // The commits are in the store, as the entry was written after them: a failure to read
        // one is damage and not "no repeat", which would take the snapshot that the repeat avoids.
        let theirs = self.refs().branch(other).map_err(MergeBranchError::Refs)?;
        let tip = read_any_commit(self.objects(), new_tip).map_err(MergeBranchError::Read)?;
        let kind = if Some(new_tip) == theirs {
            MergeKind::FastForward
        } else if theirs.is_some_and(|theirs| tip.parents() == [old_tip, theirs]) {
            MergeKind::MergeCommit
        } else {
            return refused;
        };
        Ok(Some(Stopped {
            old_tip,
            new_tip,
            next,
            kind,
            branch_pending,
        }))
    }

    /// The owner check of the snapshot that a repeat does not take: another actor does not finish
    /// the merge of the owner without `--take-over`, and an agent never takes over (ADR-010).
    fn own_for_merge_repeat(
        &self,
        lock: &WriteLock,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<(), MergeBranchError> {
        let (_, owner) = self
            .refs_store()
            .working_copy_and_owner()
            .map_err(MergeBranchError::Refs)?;
        if self
            .check_owner(actor, owner.as_ref())
            .map_err(MergeBranchError::Snapshot)?
        {
            let operation = self
                .ask_take_over(actor, authorizer)
                .map_err(MergeBranchError::Snapshot)?;
            self.take_over_locked(lock, &operation, actor)
                .map_err(MergeBranchError::Snapshot)?;
        }
        Ok(())
    }
}

/// Why [`Repository::merge`] fails. [`MoveFailed`](Self::MoveFailed),
/// [`PartlyMoved`](Self::PartlyMoved), [`NotDurable`](Self::NotDurable), and a
/// [`Rewrite`](Self::Rewrite) of a failure of the files wrote the entry of the merge; the other
/// variants did not. [`SnapshotNotDurable`](Self::SnapshotNotDurable) and
/// [`AfterSnapshot`](Self::AfterSnapshot) name the entry of a snapshot that came first.
#[derive(Debug)]
#[non_exhaustive]
pub enum MergeBranchError {
    /// The message is empty or holds only white space.
    EmptyMessage,
    /// HEAD names a commit and not a branch, so there is no branch to move.
    DetachedHead(CommitId),
    /// There is no branch with the name.
    NoSuchBranch(BranchName),
    /// The branch that HEAD names has no commit yet.
    NoCommit(BranchName),
    /// The authorizer refused `merge` or the question of the snapshot, or an entry could not be
    /// written (see [`UpdateError::Log`]). No pointer of the merge moved and no file was written;
    /// the entry of a snapshot may exist.
    Update(UpdateError),
    /// The references could not be read, the lock is held, or a purge is pending.
    Refs(RefError),
    /// The log could not be read.
    Log(OpLogError),
    /// A commit could not be stored.
    Store(Box<StoreError>),
    /// The snapshot failed, or the owner of the working copy is not the actor.
    Snapshot(SnapshotError),
    /// A commit or a tree could not be read.
    Read(SnapshotError),
    /// The history could not be walked, or the trees could not be merged.
    Merge(MergeError),
    /// The sum of trees of the merge commit is not valid for a commit.
    Invalid(FromSumError),
    /// The operating system gave no random bytes for a change ID.
    ChangeId(GenerateError),
    /// The working-copy commit could not be carried onto the new tip, or its files could not be
    /// brought to the new tree. For [`RewriteError::CheckoutFailed`] the entry was written.
    Rewrite(RewriteError),
    /// The newest entry is a merge that did not finish and is not this one. Taking a snapshot
    /// would save its files as changes of the working-copy commit, so nothing was done: the same
    /// merge run again finishes it. Nothing was written.
    Stopped {
        /// The working-copy commit that the stopped merge made.
        stopped: CommitId,
    },
    /// The entry was written and the files are at the new tree, but the first pointer did not
    /// move.
    MoveFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: RefError,
    },
    /// The entry was written and the files are at the new tree, and the branch moved, but the
    /// working-copy commit did not.
    PartlyMoved {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: RefError,
    },
    /// The merge was made, but a directory could not be flushed to disk, so it may not survive a
    /// crash.
    NotDurable {
        /// The entry that was written.
        entry: Box<Entry>,
        /// The error of the flush.
        source: RefError,
        /// What the merge did, for the report of a merge that is not to be repeated.
        result: Box<MergeResult>,
    },
    /// The snapshot moved the working-copy commit and its directory could not be flushed. The
    /// merge did not start.
    SnapshotNotDurable {
        /// The entry of the snapshot.
        snapshot: Box<Entry>,
        /// The error of the flush.
        source: RefError,
    },
    /// The snapshot wrote an entry, and a later step failed.
    AfterSnapshot {
        /// The entry of the snapshot.
        snapshot: Box<Entry>,
        /// The failure.
        source: Box<MergeBranchError>,
    },
}

impl MergeBranchError {
    /// Returns the error of the file system, if one is inside.
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
            Self::Merge(error) => error.store_error().and_then(StoreError::io_error),
            Self::Rewrite(error) => error.io_error(),
            Self::AfterSnapshot { source, .. } => source.io_error(),
            Self::EmptyMessage
            | Self::DetachedHead(_)
            | Self::NoSuchBranch(_)
            | Self::NoCommit(_)
            | Self::Invalid(_)
            | Self::ChangeId(_)
            | Self::Stopped { .. } => None,
        }
    }

    /// The entry of the merge, if the error leaves one in the log.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Update(error) => error.entry(),
            Self::MoveFailed { entry, .. }
            | Self::PartlyMoved { entry, .. }
            | Self::NotDurable { entry, .. } => Some(entry),
            Self::Rewrite(error) => error.entry(),
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

    /// Whether the merge took effect and only the flush failed: the result is to be reported and
    /// the merge is not to be repeated.
    pub fn made(&self) -> bool {
        match self {
            Self::NotDurable { .. } => true,
            Self::AfterSnapshot { source, .. } => source.made(),
            _ => false,
        }
    }

    /// The result of a merge that was made and whose flush failed: the branch, how it moved, its
    /// commit, and the conflicted paths. `None` for any other error.
    pub fn result(&self) -> Option<&MergeResult> {
        match self {
            Self::NotDurable { result, .. } => Some(result),
            Self::AfterSnapshot { source, .. } => source.result(),
            _ => None,
        }
    }

    /// Whether a pointer moved although the call failed and the move is not known to be durable
    /// or complete: the branch moved and the working-copy commit did not, or a flush failed. A
    /// snapshot that moved a pointer durably before a later step failed is not counted;
    /// [`snapshot`](Self::snapshot) names it.
    pub fn pointer_moved(&self) -> bool {
        match self {
            Self::PartlyMoved { .. }
            | Self::NotDurable { .. }
            | Self::SnapshotNotDurable { .. } => true,
            Self::AfterSnapshot { source, .. } => source.pointer_moved(),
            _ => false,
        }
    }
}

impl fmt::Display for MergeBranchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyMessage => f.write_str("the message of the merge commit is empty"),
            Self::DetachedHead(commit) => write!(
                f,
                "HEAD names the commit {commit} and not a branch, so there is no branch to merge \
                 into; `stemma switch` changes to a branch"
            ),
            Self::NoSuchBranch(name) => write!(f, "there is no branch `{name}`"),
            Self::NoCommit(name) => write!(
                f,
                "the branch `{name}` has no commit yet, so there is nothing to merge into"
            ),
            Self::Update(error) => write!(f, "could not merge: {error}"),
            Self::Refs(error) => write!(f, "could not merge, nothing of it was written: {error}"),
            Self::Log(error) => write!(f, "could not read the operation log: {error}"),
            Self::Store(error) => write!(f, "could not store a commit: {error}"),
            Self::Snapshot(error) if error.stands_alone() => error.fmt(f),
            Self::Snapshot(error) => write!(
                f,
                "could not save the working directory or check its owner first, so the merge \
                 did not start: {error}"
            ),
            Self::Read(error) => write!(f, "could not read a commit or a tree: {error}"),
            Self::Merge(error) => write!(f, "could not merge the branches: {error}"),
            Self::Invalid(error) => write!(f, "the merge commit is not valid: {error}"),
            Self::ChangeId(error) => write!(f, "could not make a change ID: {error}"),
            Self::Rewrite(RewriteError::CheckoutFailed { entry, source }) => write!(
                f,
                "could not write the files; entry {} names a merge that did not happen and no \
                 pointer moved, and the files done so far stay: {source}; run the same merge \
                 again before any other command, as a snapshot saves only the files that the \
                 working-copy commit tracks and would miss the files that only the new tree has",
                entry.number()
            ),
            Self::Rewrite(RewriteError::Checkout(error)) => {
                write!(f, "could not merge, nothing of it was written: {error}")
            }
            Self::Rewrite(error) => error.fmt(f),
            Self::Stopped { stopped } => write!(
                f,
                "the newest merge (working-copy commit {stopped}) did not finish and is not this \
                 one; run that merge again before any other command, as a snapshot would save \
                 its files as changes"
            ),
            Self::MoveFailed { entry, source } => write!(
                f,
                "entry {} was written and the files are at the new tree, but no pointer moved: \
                 {source}; run the same merge again before any other command, as a snapshot \
                 saves only the files that the working-copy commit tracks and would miss the \
                 files that only the new tree has",
                entry.number()
            ),
            Self::PartlyMoved { entry, source } => write!(
                f,
                "entry {} was written and the branch moved, but the working-copy commit did \
                 not: {source}; run the same merge again before any other command",
                entry.number()
            ),
            Self::NotDurable { entry, source, .. } => write!(
                f,
                "the merge was made as entry {}, but the directory could not be flushed, so it \
                 may not survive a crash: {source}",
                entry.number()
            ),
            Self::SnapshotNotDurable { snapshot, source } => write!(
                f,
                "unsaved changes were saved first as entry {}, but the directory could not be \
                 flushed to disk and the saved state may not survive a crash, so the merge did \
                 not start: {source}",
                snapshot.number()
            ),
            Self::AfterSnapshot { snapshot, source } => write!(
                f,
                "{source}; unsaved changes were saved first as entry {}",
                snapshot.number()
            ),
        }
    }
}

impl Error for MergeBranchError {
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
            Self::Merge(error) => error.source(),
            Self::Invalid(error) => error.source(),
            Self::ChangeId(error) => error.source(),
            Self::Rewrite(error) => error.source(),
            Self::AfterSnapshot { source, .. } => source.source(),
            Self::EmptyMessage
            | Self::DetachedHead(_)
            | Self::NoSuchBranch(_)
            | Self::NoCommit(_)
            | Self::Stopped { .. } => None,
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
    use crate::repository::{InitOptions, SwitchTarget};
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

    fn track(repository: &Repository, path: &str) {
        repository
            .track(&[path.to_owned()], &actor(), &AllowAll)
            .unwrap();
    }

    fn commit(repository: &Repository, message: &str) {
        repository
            .commit(message, &actor(), &AllowAll)
            .map(drop)
            .unwrap();
    }

    fn switch(repository: &Repository, target: &SwitchTarget) {
        repository
            .switch(target, &actor(), &AllowAll)
            .map(drop)
            .unwrap();
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

    fn tip(repository: &Repository, branch: &str) -> CommitId {
        repository.refs().branch(&name(branch)).unwrap().unwrap()
    }

    fn working(repository: &Repository) -> AnyCommit {
        let id = repository.refs().working_copy().unwrap();
        read_any_commit(repository.objects(), id).unwrap()
    }

    fn entries(repository: &Repository) -> usize {
        repository.oplog().entries().unwrap().len()
    }

    fn merge(repository: &Repository, other: &str) -> Result<Merged, MergeBranchError> {
        repository.merge(&name(other), None, &actor(), &AllowAll)
    }

    struct Done {
        snapshot: Option<Entry>,
        entry: Entry,
        kind: MergeKind,
        commit: CommitId,
        conflicts: Vec<String>,
    }

    fn done(result: Result<Merged, MergeBranchError>) -> Done {
        match result {
            Ok(Merged::Done {
                snapshot,
                entry,
                kind,
                commit,
                conflicts,
                ..
            }) => Done {
                snapshot: snapshot.map(|entry| *entry),
                entry: *entry,
                kind,
                commit,
                conflicts,
            },
            other => panic!("expected a merge, got {other:?}"),
        }
    }

    const BASE: &str = "one\ntwo\nthree\n";

    /// `main` holds `a.txt` with `BASE`. `feature` changes its first line to `ONE`; `other` changes
    /// its last line to `THREE`; `clash` changes its first line to `uno`. The repository is on
    /// `main` with an empty working-copy commit.
    fn history(dir: &TempDir, repository: &Repository) {
        write(dir, "a.txt", BASE);
        track(repository, "a.txt");
        commit(repository, "first");
        for (branch, text) in [
            ("feature", "ONE\ntwo\nthree\n"),
            ("other", "one\ntwo\nTHREE\n"),
            ("clash", "uno\ntwo\nthree\n"),
        ] {
            switch(repository, &create(branch));
            write(dir, "a.txt", text);
            commit(repository, branch);
            switch(repository, &existing("main"));
        }
    }

    #[test]
    fn a_branch_that_is_ahead_is_fast_forwarded_without_a_commit() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        let count = entries(&repository);
        let before = working(&repository);

        let merged = done(merge(&repository, "feature"));

        assert_eq!(merged.kind, MergeKind::FastForward);
        assert_eq!(merged.commit, feature);
        assert!(merged.conflicts.is_empty());
        assert_eq!(tip(&repository, "main"), feature, "the branch took the tip");
        assert_eq!(entries(&repository), count + 1, "one entry for both moves");
        let change = merged.entry.change().unwrap();
        assert_eq!(change.operation().as_str(), "merge");
        assert!(change.working_copy().is_some());
        assert_eq!(change.branches().len(), 1);
        assert_eq!(working(&repository).parents(), [feature]);
        assert_ne!(
            working(&repository).change(),
            before.change(),
            "an empty copy"
        );
        assert_eq!(read(&dir, "a.txt"), "ONE\ntwo\nthree\n");
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    #[test]
    fn a_branch_that_is_an_ancestor_is_up_to_date_and_writes_no_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let _ = done(merge(&repository, "feature"));
        let count = entries(&repository);

        // `main` is at `feature` now, and `feature` is itself.
        let result = merge(&repository, "feature");
        assert!(matches!(result, Ok(Merged::UpToDate { snapshot: None })));
        // The first commit is an ancestor too.
        switch(&repository, &create("old"));
        let result = merge(&repository, "main");
        assert!(matches!(result, Ok(Merged::UpToDate { .. })), "{result:?}");
        assert_eq!(entries(&repository), count + 1, "only the switch wrote one");
    }

    #[test]
    fn two_diverged_branches_get_a_merge_commit_with_two_parents() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        let other = tip(&repository, "other");
        switch(&repository, &existing("feature"));

        let merged = done(merge(&repository, "other"));

        assert_eq!(merged.kind, MergeKind::MergeCommit);
        assert!(merged.conflicts.is_empty());
        assert_eq!(tip(&repository, "feature"), merged.commit);
        let commit = read_any_commit(repository.objects(), merged.commit).unwrap();
        assert_eq!(commit.parents(), [feature, other]);
        assert_eq!(commit.message(), "Merge other into feature");
        assert!(!commit.is_conflicted());
        assert_eq!(read(&dir, "a.txt"), "ONE\ntwo\nTHREE\n");
        assert_eq!(working(&repository).parents(), [merged.commit]);
        assert_eq!(tip(&repository, "other"), other, "the other branch stays");
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    #[test]
    fn the_message_of_the_merge_commit_can_be_given() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        switch(&repository, &existing("feature"));

        let merged =
            done(repository.merge(&name("other"), Some("both edits"), &actor(), &AllowAll));

        let commit = read_any_commit(repository.objects(), merged.commit).unwrap();
        assert_eq!(commit.message(), "both edits");
        let error = repository
            .merge(&name("clash"), Some("  \n"), &actor(), &AllowAll)
            .unwrap_err();
        assert!(matches!(error, MergeBranchError::EmptyMessage), "{error}");
    }

    #[test]
    fn a_conflict_does_not_stop_the_merge_and_is_data_of_the_commit() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        switch(&repository, &existing("feature"));

        let merged = done(merge(&repository, "clash"));

        assert_eq!(merged.kind, MergeKind::MergeCommit);
        assert_eq!(merged.conflicts, ["a.txt"]);
        let commit = read_any_commit(repository.objects(), merged.commit).unwrap();
        assert!(
            commit.is_conflicted(),
            "the merge commit holds the conflict"
        );
        assert_eq!(commit.tree_sum().sides(), 2);
        assert!(read(&dir, "a.txt").starts_with("<<<<<<< conflict\n"));
        assert!(working(&repository).is_conflicted());
        // The snapshot keeps the conflict while the text is unchanged.
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    #[test]
    fn a_criss_cross_history_merges_over_both_bases() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        // `a` and `b` start at `feature` and `other`; each merges the other, so that the two merge
        // commits have two best common ancestors.
        switch(&repository, &existing("feature"));
        switch(&repository, &create("a"));
        switch(&repository, &existing("other"));
        switch(&repository, &create("b"));
        let first_a = tip(&repository, "a");
        let first_b = tip(&repository, "b");
        switch(&repository, &existing("a"));
        let _ = done(merge(&repository, "b"));
        switch(&repository, &existing("b"));
        // `b` merges the old `a`, which is a commit of `feature` and not the merge commit of `a`.
        repository
            .switch(
                &SwitchTarget::Create {
                    name: name("a-old"),
                    start: Some(crate::repository::Revision::Commit(first_a.object_id())),
                },
                &actor(),
                &AllowAll,
            )
            .map(drop)
            .unwrap();
        switch(&repository, &existing("b"));
        let _ = done(merge(&repository, "a-old"));
        let (a, b) = (tip(&repository, "a"), tip(&repository, "b"));
        assert_ne!(a, b);
        let bases = merge_bases(repository.objects(), a, b).unwrap();
        assert_eq!(bases.len(), 2, "a criss-cross history: {bases:?}");
        assert!(bases.contains(&first_a) && bases.contains(&first_b));
        switch(&repository, &existing("a"));

        let merged = done(merge(&repository, "b"));

        assert_eq!(merged.kind, MergeKind::MergeCommit);
        assert!(merged.conflicts.is_empty());
        assert_eq!(read(&dir, "a.txt"), "ONE\ntwo\nTHREE\n");
    }

    #[test]
    fn changes_of_the_working_copy_are_saved_and_carried_onto_the_new_tip() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        switch(&repository, &existing("feature"));
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let before = working(&repository);

        let merged = done(merge(&repository, "other"));

        assert!(merged.conflicts.is_empty());
        let after = working(&repository);
        assert_eq!(after.parents(), [merged.commit]);
        assert_eq!(after.change(), before.change(), "the change is carried");
        assert_eq!(read(&dir, "b.txt"), "b\n");
        assert_eq!(read(&dir, "a.txt"), "ONE\ntwo\nTHREE\n");
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    #[test]
    fn unsaved_edits_are_saved_first_and_survive_the_merge() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        write(&dir, "a.txt", "one\ntwo\nthree\nfour\n");

        let merged = done(merge(&repository, "feature"));

        assert!(merged.snapshot.is_some(), "the edit was saved first");
        assert!(merged.conflicts.is_empty());
        assert_eq!(read(&dir, "a.txt"), "ONE\ntwo\nthree\nfour\n");
    }

    #[test]
    fn a_merge_is_restored_like_any_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        switch(&repository, &existing("feature"));
        let (feature, before) = (
            tip(&repository, "feature"),
            repository.refs().working_copy().unwrap(),
        );
        let merged = done(merge(&repository, "clash"));

        repository
            .restore(merged.entry.number(), false, &actor(), &AllowAll)
            .unwrap();

        assert_eq!(tip(&repository, "feature"), feature);
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(read(&dir, "a.txt"), "ONE\ntwo\nthree\n");
    }

    #[test]
    fn the_authorizer_is_asked_for_the_branch_that_moves_and_for_the_other_one() {
        struct DenyTarget(&'static str);
        impl Authorizer for DenyTarget {
            fn authorize(&self, _: &Actor, operation: &OperationName, target: &str) -> Decision {
                if operation.as_str() == "merge" && target == self.0 {
                    Decision::Deny
                } else {
                    Decision::Allow
                }
            }
        }
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let count = entries(&repository);

        for denied in ["main", "feature"] {
            let error = repository
                .merge(&name("feature"), None, &actor(), &DenyTarget(denied))
                .unwrap_err();
            assert!(
                matches!(error, MergeBranchError::Update(_)),
                "{denied}: {error}"
            );
        }
        assert_eq!(entries(&repository), count);
        assert_ne!(tip(&repository, "main"), tip(&repository, "feature"));
    }

    #[test]
    fn a_detached_head_a_missing_branch_and_a_refusal_write_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let count = entries(&repository);

        let error = merge(&repository, "absent").unwrap_err();
        assert!(
            matches!(error, MergeBranchError::NoSuchBranch(_)),
            "{error}"
        );
        let error = repository
            .merge(&name("feature"), None, &actor(), &DenyAll)
            .unwrap_err();
        assert!(matches!(error, MergeBranchError::Update(_)), "{error}");
        switch(
            &repository,
            &SwitchTarget::Detach(crate::repository::Revision::Branch(name("main"))),
        );
        let count = count + 1;
        let error = merge(&repository, "feature").unwrap_err();
        assert!(
            matches!(error, MergeBranchError::DetachedHead(_)),
            "{error}"
        );
        assert_eq!(entries(&repository), count);
    }

    #[test]
    fn a_stop_after_the_branch_moved_is_finished_by_the_same_call() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        switch(&repository, &existing("feature"));
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let (feature, before) = (
            tip(&repository, "feature"),
            repository.refs().working_copy().unwrap(),
        );
        // The pointer files cannot be replaced once the entry and the files are written.
        let metadata = repository.metadata_dir();
        let denied = Rc::new(RefCell::new(None));
        let arm = Rc::clone(&denied);
        super::super::snapshot::pause::at_next(
            super::super::snapshot::pause::Stage::Moves,
            move || {
                *arm.borrow_mut() = Some(fault::deny_writes_in(&metadata));
            },
        );

        let error = merge(&repository, "other").unwrap_err();
        drop(denied);
        let _ = fs::remove_file(repository.metadata_dir().join("refs.lock"));

        // The branch moves first; the working-copy commit is the pointer that stays.
        assert!(
            matches!(error, MergeBranchError::PartlyMoved { .. }),
            "{error}"
        );
        assert!(error.pointer_moved());
        let message = error.to_string();
        assert!(message.contains("run the same merge again"), "{message}");
        let stopped = error.entry().unwrap().number();
        let named = tip(&repository, "feature");
        assert_ne!(named, feature, "the branch took the merge commit");
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(
            read(&dir, "a.txt"),
            "ONE\ntwo\nTHREE\n",
            "the files are done"
        );

        // Another merge is refused: a snapshot would save the new files as changes.
        let count = entries(&repository);
        let error = merge(&repository, "clash").unwrap_err();
        assert!(matches!(error, MergeBranchError::Stopped { .. }), "{error}");
        assert_eq!(entries(&repository), count);

        // The same merge finishes it, without a snapshot.
        let merged = done(merge(&repository, "other"));
        assert!(merged.snapshot.is_none(), "the repeat takes no snapshot");
        assert!(merged.entry.number() > stopped);
        assert_eq!(
            merged.commit, named,
            "the repeat uses the commit of the entry"
        );
        assert_eq!(tip(&repository, "feature"), named);
        assert_eq!(read(&dir, "b.txt"), "b\n");
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    /// Stops the next merge after its entry and files: the pointer of the working-copy commit
    /// cannot be replaced. Returns the error of the call.
    fn stopped(repository: &Repository, other: &str) -> MergeBranchError {
        let metadata = repository.metadata_dir();
        let denied = Rc::new(RefCell::new(None));
        let arm = Rc::clone(&denied);
        super::super::snapshot::pause::at_next(
            super::super::snapshot::pause::Stage::Moves,
            move || {
                *arm.borrow_mut() = Some(fault::deny_writes_in(&metadata));
            },
        );
        let error = merge(repository, other).unwrap_err();
        drop(denied);
        let _ = fs::remove_file(repository.metadata_dir().join("refs.lock"));
        error
    }

    #[test]
    fn a_repeat_that_stops_again_is_found_again_and_names_both_moves() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        switch(&repository, &existing("feature"));
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let first = stopped(&repository, "other");
        assert!(
            matches!(first, MergeBranchError::PartlyMoved { .. }),
            "{first}"
        );
        let named = tip(&repository, "feature");

        let second = stopped(&repository, "other");

        let entry = second.entry().expect("the repeat wrote an entry");
        let change = entry.change().unwrap();
        assert_eq!(
            change.branches().len(),
            1,
            "the entry of a repeat names the branch"
        );
        let count = entries(&repository);
        let merged = done(merge(&repository, "other"));
        assert!(
            merged.snapshot.is_none(),
            "no snapshot of the files of the stop"
        );
        assert_eq!(merged.commit, named);
        assert_eq!(entries(&repository), count + 1);
        assert_eq!(read(&dir, "b.txt"), "b\n");
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());

        // The entry of the repeat undoes both moves.
        let before = tip(&repository, "other");
        repository
            .restore(merged.entry.number(), false, &actor(), &AllowAll)
            .unwrap();
        assert_ne!(tip(&repository, "feature"), named, "the branch is back");
        assert_eq!(tip(&repository, "other"), before);
    }

    #[test]
    fn a_stopped_fast_forward_is_finished_as_a_fast_forward() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        let error = stopped(&repository, "feature");
        assert!(
            matches!(error, MergeBranchError::PartlyMoved { .. }),
            "{error}"
        );

        let merged = done(merge(&repository, "feature"));

        assert_eq!(merged.kind, MergeKind::FastForward);
        assert_eq!(merged.commit, feature);
        assert!(merged.snapshot.is_none());
        assert_eq!(tip(&repository, "main"), feature);
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    #[test]
    fn another_actor_does_not_finish_a_stopped_merge() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        switch(&repository, &existing("feature"));
        let _ = stopped(&repository, "other");
        let count = entries(&repository);
        let mara = Actor::human("mara").unwrap();

        let error = repository
            .merge(&name("other"), None, &mara, &AllowAll)
            .unwrap_err();

        assert!(
            matches!(
                error,
                MergeBranchError::Snapshot(SnapshotError::NotOwner { .. })
            ),
            "{error}"
        );
        assert_eq!(entries(&repository), count);
    }
}
