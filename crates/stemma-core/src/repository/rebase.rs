//! Replaying the current branch onto a target: `rebase` (ADR-010).
//!
//! A rebase takes the commits of the branch that HEAD names which the target does not have, replays
//! them one after the other on the target, and moves the branch to the last of them. The working-
//! copy commit follows the branch through the rewrite of `rewrite_working_copy`: it keeps its
//! change ID, unless it holds no change and is replaced by an empty commit with a new one,
//! so the branch and the working-copy commit move in one entry of the log, in the order of
//! ADR-013: snapshot, plan of the files, entry, files, pointers.
//!
//! A commit is replayed as the sum of trees `new_parent + (commit - old_parent)`, merged path by
//! path. A conflict never stops the rebase: it is data of the replayed commit (ADR-005), a commit
//! that is replayed on top of it carries it on, and a source commit with an open conflict goes
//! through the same sum. There is no `--continue` and no `--abort`: `op restore` undoes the whole
//! rebase, because the entry holds all moves.
//!
//! Only the branch of HEAD moves. Other references keep the old commits, and the log keeps them
//! as roots (ADR-011). A merge commit in the range is refused, as its change has no single base.

use std::error::Error;
use std::fmt;
use std::io;

use super::checkout::CheckoutError;
use super::conflicted::{self, ConflictedFile};
use super::rewrite::{Carried, RewriteError};
use super::snapshot::{read_any_commit, SnapshotError};
use super::switch::Revision;
use super::{now, Repository, UpdateError};
use crate::conflict::AnyCommit;
use crate::history::HistoryError;
use crate::merge_base::merge_bases;
use crate::object::{CommitId, ObjectId};
use crate::oplog::{Change, Entry, Moved, OpLogError, OperationName, TAKE_OVER};
use crate::refs::{BranchName, Head, Logged, RefError, RestoreTargets};
use crate::store::{ObjectStore, StoreError};
use crate::{Actor, Authorizer};

/// The operation that the log records for a rebase.
const OPERATION: &str = "rebase";

/// One commit of the new branch, from the target upwards.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Replayed {
    /// The replayed commit.
    pub commit: CommitId,
    /// The paths that are an open conflict in it, with the number of their sides. Empty for a
    /// commit without a conflict.
    pub conflicts: Vec<ConflictedFile>,
}

/// How a rebase ended.
#[derive(Debug)]
#[must_use]
pub enum Rebased {
    /// The branch has the target already (it is the tip or an ancestor of it). Nothing was
    /// written: no snapshot and no entry.
    UpToDate,
    /// The branch and the working-copy commit moved.
    Done {
        /// The entry of the snapshot, if the directory held unsaved changes.
        snapshot: Option<Box<Entry>>,
        /// The entry of the rebase.
        entry: Box<Entry>,
        /// The branch that moved.
        branch: BranchName,
        /// The tip of the branch before.
        old_tip: CommitId,
        /// The tip of the branch now: the last replayed commit, or the target if the branch had no
        /// commit of its own.
        new_tip: CommitId,
        /// The commits of the new branch above the target, oldest first.
        replayed: Vec<Replayed>,
        /// The new working-copy commit, a child of `new_tip`.
        working_copy: CommitId,
        /// The paths that are an open conflict in the new working-copy commit, sorted.
        conflicts: Vec<String>,
    },
}

/// What is decided before anything but objects is written.
struct Plan {
    branch: BranchName,
    old_tip: CommitId,
    /// The target.
    onto: CommitId,
    /// The commits to replay, newest first.
    range: Vec<(CommitId, AnyCommit)>,
    /// The branch has the target already.
    up_to_date: bool,
}

/// A rebase that stopped after its entry: the commits it made.
struct Stopped {
    working_copy: CommitId,
    new_tip: CommitId,
}

impl Repository {
    /// Replays the commits of the branch that HEAD names on `target`, and moves the branch and the
    /// working-copy commit (see the module documentation).
    ///
    /// The steps are: the authorizer is asked for `rebase` (the target is the revision); the lock
    /// of the references is taken and held to the last move (ADR-014); HEAD, the tip, the target,
    /// the range, and the merge commits in it are checked, which write nothing; the tracked files
    /// are saved in a snapshot (a second question, `snapshot`); the commits are replayed and
    /// stored, the working-copy commit is carried onto the new tip as `rewrite_working_copy` does
    /// (it keeps its change ID unless it holds no change); the files are planned, the entry is
    /// written and flushed, the directory is brought to the new tree, and the branch and the working-copy commit move (ADR-013). HEAD
    /// stays. The pointers are read under the lock, so the move is a compare-and-swap against the
    /// values this call saw.
    ///
    /// The range is the commits that the tip has and the target has not, found from the best
    /// common ancestor. A tip that is an ancestor of the target has none, so the branch moves
    /// forward to the target. A target that is the tip or an ancestor of it is
    /// [`Rebased::UpToDate`].
    ///
    /// If the newest entry of the log (a `take-over` does not count) is a rebase that did not take
    /// effect (the entry was written, the files are written or partly written, and the working-copy
    /// commit did not move), the call is its repeat: it takes no snapshot, as one would save the
    /// new files as changes of the old working-copy commit, and it uses the commits of that entry.
    /// It checks the owner of the working copy instead (ADR-010). A stopped rebase onto another
    /// target is refused with [`RebaseError::StoppedOnOtherTarget`] for the same reason.
    ///
    /// # Errors
    ///
    /// Before anything is written: [`RebaseError::Update`] for a refusal of the question
    /// `rebase`, [`RebaseError::Refs`] for a lock that another command holds or a purge that is
    /// pending, [`RebaseError::NotOnBranch`], [`RebaseError::NoCommits`],
    /// [`RebaseError::NoSuchBranch`], [`RebaseError::NoSuchCommit`], [`RebaseError::Store`],
    /// [`RebaseError::History`], [`RebaseError::NoCommonAncestor`],
    /// [`RebaseError::AmbiguousBase`], [`RebaseError::MergeInRange`], and, when the call looks for
    /// a stopped rebase, [`RebaseError::Log`], [`RebaseError::Read`], and
    /// [`RebaseError::StoppedOnOtherTarget`].
    ///
    /// [`RebaseError::Snapshot`] is a refusal of the snapshot or, for a repeat, of the owner
    /// check. A failure of the snapshot to move the pointer is [`RebaseError::Update`], and
    /// [`RebaseError::SnapshotNotDurable`] says that the snapshot was made and its directory not
    /// flushed; the rebase did not start then.
    ///
    /// After the snapshot, [`RebaseError::Replay`] and [`RebaseError::Checkout`] stand for the
    /// preparation of the commits and the plan of the files. They wrote nothing but objects and,
    /// if the snapshot wrote an entry, that entry; the error is then wrapped in
    /// [`RebaseError::AfterSnapshot`]. Then [`RebaseError::CheckoutFailed`],
    /// [`RebaseError::MoveFailed`], [`RebaseError::PartlyMoved`], and
    /// [`RebaseError::NotDurable`] say how far the rebase came.
    pub fn rebase(
        &self,
        target: &Revision,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Rebased, RebaseError> {
        let operation =
            OperationName::try_new(OPERATION).expect("`rebase` is a valid operation name");
        self.refs_store()
            .refuse_pending_purge()
            .map_err(RebaseError::Refs)?;
        let allowed = self
            .authorize(actor, authorizer, &operation, &target.to_string())
            .map_err(RebaseError::Update)?;
        let lock = self
            .refs_store()
            .lock_writer(&allowed)
            .map_err(RebaseError::Refs)?;
        let plan = self.plan_rebase(target)?;
        // A rebase that stopped after its files is repeated without a snapshot (see above). It is
        // looked for before the answer "up to date": a stop after the move of the branch leaves
        // the branch on the target already.
        let stopped = self.stopped_rebase(&plan)?;
        if stopped.is_none() && plan.up_to_date {
            return Ok(Rebased::UpToDate);
        }
        let snapshot = match &stopped {
            Some(_) => {
                self.own_for_repeat_locked(&lock, actor, authorizer)
                    .map_err(RebaseError::Snapshot)?;
                None
            }
            None => match self.snapshot_locked(&lock, actor, authorizer) {
                Ok(entry) => entry.map(Box::new),
                Err(SnapshotError::Update(UpdateError::NotDurable { entry, source })) => {
                    return Err(RebaseError::SnapshotNotDurable {
                        snapshot: entry,
                        source,
                    });
                }
                // The entry of a snapshot that failed to move the pointer stays reachable.
                Err(SnapshotError::Update(error)) => return Err(RebaseError::Update(error)),
                Err(error) => return Err(RebaseError::Snapshot(error)),
            },
        };
        #[cfg(test)]
        super::snapshot::pause::reached(super::snapshot::pause::Stage::Moves);
        let wrap = |error: RebaseError| match &snapshot {
            Some(saved) => RebaseError::AfterSnapshot {
                snapshot: saved.clone(),
                source: Box::new(error),
            },
            None => error,
        };
        let current = self
            .refs()
            .working_copy()
            .map_err(|error| wrap(RebaseError::Refs(error)))?;
        let (new_tip, carried) = match &stopped {
            Some(stopped) => self
                .carried(stopped.working_copy)
                .map(|carried| (stopped.new_tip, carried))
                .map_err(|error| {
                    wrap(match error {
                        RewriteError::Read(error) => RebaseError::Read(error),
                        other => RebaseError::Replay(Box::new(other)),
                    })
                }),
            None => self.replay_range(&plan, current, actor).map_err(wrap),
        }?;
        let replayed = self.replayed(new_tip, plan.onto).map_err(wrap)?;
        let branches = [(plan.branch.clone(), Some(new_tip))];
        let targets = RestoreTargets {
            head: None,
            working_copy: Some(carried.id),
            branches: &branches,
        };
        let logged = self
            .refs_store()
            .restore_after(&lock, &targets, self.objects(), |moves| {
                self.write_rebase(&lock, &operation, &plan, current, &carried, moves, actor)
            });
        let made = |entry: Entry| Rebased::Done {
            snapshot: snapshot.clone(),
            entry: Box::new(entry),
            branch: plan.branch.clone(),
            old_tip: plan.old_tip,
            new_tip,
            replayed,
            working_copy: carried.id,
            conflicts: carried.conflicts.clone(),
        };
        match logged {
            Err(error) => Err(wrap(RebaseError::Refs(error))),
            Ok(Logged::Unchanged) => {
                unreachable!("the new working-copy commit differs from the old one")
            }
            Ok(Logged::Moved(entry)) => Ok(made(entry)),
            Ok(Logged::StepFailed(error)) => Err(wrap(error)),
            Ok(Logged::MoveFailed(entry, source)) => Err(wrap(RebaseError::MoveFailed {
                entry: Box::new(entry),
                source,
            })),
            Ok(Logged::PartlyMoved(entry, source)) => Err(wrap(RebaseError::PartlyMoved {
                entry: Box::new(entry),
                source,
            })),
            Ok(Logged::NotFlushed(entry, source)) => Err(wrap(RebaseError::NotDurable {
                entry: Box::new(entry),
                source,
            })),
        }
    }

    /// Reads HEAD, the tip, and the target, and finds the commits to replay. Writes nothing.
    fn plan_rebase(&self, target: &Revision) -> Result<Plan, RebaseError> {
        let refs = self.refs();
        let branch = match refs.head().map_err(RebaseError::Refs)? {
            Head::Branch(name) => name,
            Head::Detached(_) => return Err(RebaseError::NotOnBranch),
        };
        let old_tip = refs
            .branch(&branch)
            .map_err(RebaseError::Refs)?
            .ok_or_else(|| RebaseError::NoCommits(branch.clone()))?;
        let onto = match target {
            Revision::Branch(name) => refs
                .branch(name)
                .map_err(RebaseError::Refs)?
                .ok_or_else(|| RebaseError::NoSuchBranch(name.clone()))?,
            Revision::Commit(id) => self
                .objects()
                .resolve_commit(id)
                .map_err(|error| RebaseError::Store(Box::new(error)))?
                .ok_or(RebaseError::NoSuchCommit(*id))?,
        };
        let bases = merge_bases(self.objects(), old_tip, onto).map_err(RebaseError::History)?;
        let base = match bases.as_slice() {
            [] => return Err(RebaseError::NoCommonAncestor { onto }),
            [base] => *base,
            many => {
                return Err(RebaseError::AmbiguousBase {
                    onto,
                    bases: many.to_vec(),
                })
            }
        };
        let plan_range = |base: CommitId| -> Result<Vec<(CommitId, AnyCommit)>, RebaseError> {
            let mut range = Vec::new();
            let mut at = old_tip;
            while at != base {
                let commit = read_any_commit(self.objects(), at).map_err(RebaseError::Read)?;
                match commit.parents() {
                    [parent] => {
                        let parent = *parent;
                        range.push((at, commit));
                        at = parent;
                    }
                    [] => return Err(RebaseError::NoCommonAncestor { onto }),
                    _ => return Err(RebaseError::MergeInRange { commit: at }),
                }
            }
            Ok(range)
        };
        let range = plan_range(base)?;
        Ok(Plan {
            branch,
            old_tip,
            onto,
            range,
            // The target is the tip or below it: the branch has it already.
            up_to_date: base == onto,
        })
    }

    /// Replays the range on the target and carries the working-copy commit `current` onto the
    /// new tip. Writes objects and nothing else. Returns the new tip.
    fn replay_range(
        &self,
        plan: &Plan,
        current: CommitId,
        actor: &Actor,
    ) -> Result<(CommitId, Carried), RebaseError> {
        let replay = |error: RewriteError| RebaseError::Replay(Box::new(error));
        let mut tip = read_any_commit(self.objects(), plan.onto).map_err(RebaseError::Read)?;
        for (_, commit) in plan.range.iter().rev() {
            let base = self.base_sum(commit).map_err(replay)?;
            let next = self.replay(commit, &base, &tip).map_err(replay)?;
            self.store_commit(&next).map_err(replay)?;
            tip = next;
        }
        let working = read_any_commit(self.objects(), current).map_err(RebaseError::Read)?;
        let carried = self.carry(&working, tip.id(), actor).map_err(replay)?;
        Ok((tip.id(), carried))
    }

    /// The commits from the target up to `tip`, oldest first, with their conflicts.
    fn replayed(&self, tip: CommitId, onto: CommitId) -> Result<Vec<Replayed>, RebaseError> {
        let mut chain = Vec::new();
        let mut at = tip;
        while at != onto {
            let commit = read_any_commit(self.objects(), at).map_err(RebaseError::Read)?;
            let conflicts = if commit.is_conflicted() {
                conflicted::view(self.objects(), &commit.tree_sum())
                    .map_err(RebaseError::Read)?
                    .files()
            } else {
                Vec::new()
            };
            chain.push(Replayed {
                commit: at,
                conflicts,
            });
            match commit.parents() {
                [parent] => at = *parent,
                // A chain that does not lead to the target by single parents is damage.
                _ => return Err(RebaseError::BrokenChain { commit: at }),
            }
        }
        chain.reverse();
        Ok(chain)
    }

    /// The step of the moves, under the lock: check the values, plan the files, write the entry,
    /// bring the directory to the new tree (ADR-013).
    #[allow(clippy::too_many_arguments)]
    fn write_rebase(
        &self,
        lock: &super::WriteLock,
        operation: &OperationName,
        plan: &Plan,
        current: CommitId,
        carried: &Carried,
        moves: &crate::refs::RestoreMoves,
        actor: &Actor,
    ) -> Result<Entry, RebaseError> {
        // The pointers are read under the lock; they have to be the ones this call planned with.
        if let Some((found, _)) = &moves.working_copy {
            if *found != current {
                return Err(RebaseError::Refs(RefError::StaleWorkingCopy {
                    expected: current,
                    found: *found,
                }));
            }
        }
        let working = read_any_commit(self.objects(), current).map_err(RebaseError::Read)?;
        let from = conflicted::view(self.objects(), &working.tree_sum())
            .map_err(RebaseError::Read)?
            .tree;
        let files = self
            .plan_checkout(Some(from), carried.display)
            .map_err(RebaseError::Checkout)?;
        let change = Change::try_new(
            now(),
            actor.clone(),
            operation.clone(),
            None,
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
        .map_err(|error| RebaseError::Update(UpdateError::Log(OpLogError::Entry(error))))?;
        debug_assert!(
            moves
                .branches
                .iter()
                .all(|(name, _, _)| *name == plan.branch),
            "only the branch of HEAD moves"
        );
        let entry = self
            .append(lock, &change)
            .map_err(|error| RebaseError::Update(UpdateError::Log(error)))?;
        self.apply_checkout(&files)
            .map_err(|source| RebaseError::CheckoutFailed {
                entry: Box::new(entry.clone()),
                source,
            })?;
        Ok(entry)
    }

    /// The commits of the newest entry, if that entry is a rebase that did not finish: the
    /// working-copy commit is still at its old value, and the branch is at its old or its new
    /// one. The files were written then (a stop after the files, or in their middle). The caller
    /// holds the lock of the references, so no writer came between the entry and its moves.
    ///
    /// If the commits of the entry lead to the target the call asks for, this is the repeat. If
    /// not, the call is refused with [`RebaseError::StoppedOnOtherTarget`].
    fn stopped_rebase(&self, plan: &Plan) -> Result<Option<Stopped>, RebaseError> {
        let entries = self.oplog().entries().map_err(RebaseError::Log)?;
        // A `take-over` names no pointer: the repeat of another actor writes one before its own
        // entry, and must not hide a stopped rebase from a second repeat.
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
        if self.refs().working_copy().map_err(RebaseError::Refs)? != old {
            return Ok(None);
        }
        // The commit is in the store, as the entry was written after it: a failure to read it is
        // damage and not "no repeat", which would take the snapshot that the repeat avoids.
        let commit = read_any_commit(self.objects(), next).map_err(RebaseError::Read)?;
        let &[new_tip] = commit.parents() else {
            return Err(RebaseError::StoppedOnOtherTarget { stopped: next });
        };
        // The branch is where the stopped rebase left it: at the old tip, or at the new one.
        let tip = plan.old_tip;
        // The entry of a repeat names only the pointers that still had to move, so the old value
        // of the branch is taken from the newest entry of this rebase that names the branch: the
        // stopped entries, newest first, that move the working-copy commit away from `old`.
        let before = entries
            .iter()
            .rev()
            .filter_map(Entry::change)
            .filter(|change| change.operation().as_str() != TAKE_OVER)
            .take_while(|change| {
                change.operation().as_str() == OPERATION
                    && change.working_copy().and_then(Moved::before) == Some(&old)
            })
            .find_map(|change| {
                change
                    .branches()
                    .iter()
                    .find(|(name, _)| *name == plan.branch)
                    .and_then(|(_, moved)| moved.before().copied())
            });
        let branch_fits = tip == new_tip || before == Some(tip);
        // The stopped rebase must have been onto this target, exactly: the commits it replayed
        // (as many as the old tip had above the target) lie on `new_tip`, and the target is the
        // commit below them. A target that is only an ancestor of the new tip is another one.
        let same_target = match before {
            Some(before) => self.stopped_onto(before, new_tip, plan.onto)?,
            None => false,
        };
        if branch_fits && same_target {
            Ok(Some(Stopped {
                working_copy: next,
                new_tip,
            }))
        } else {
            // A snapshot would save the files of the stopped rebase as changes of the old
            // working-copy commit.
            Err(RebaseError::StoppedOnOtherTarget { stopped: next })
        }
    }
}

impl Repository {
    /// Whether the new tip `new_tip` of a stopped rebase of the branch that was at `before` is
    /// the replay of the range of `before` on `onto`: stepping down as many commits as `before`
    /// has above its merge base with `onto` leads to `onto` exactly.
    fn stopped_onto(
        &self,
        before: CommitId,
        new_tip: CommitId,
        onto: CommitId,
    ) -> Result<bool, RebaseError> {
        let bases = merge_bases(self.objects(), before, onto).map_err(RebaseError::History)?;
        let [base] = bases.as_slice() else {
            return Ok(false);
        };
        let mut steps = 0usize;
        let mut at = before;
        while at != *base {
            let commit = read_any_commit(self.objects(), at).map_err(RebaseError::Read)?;
            let &[parent] = commit.parents() else {
                return Ok(false);
            };
            at = parent;
            steps += 1;
        }
        let mut at = new_tip;
        for _ in 0..steps {
            let commit = read_any_commit(self.objects(), at).map_err(RebaseError::Read)?;
            let &[parent] = commit.parents() else {
                return Ok(false);
            };
            at = parent;
        }
        Ok(at == onto)
    }
}

/// Why [`Repository::rebase`] fails. [`CheckoutFailed`](Self::CheckoutFailed),
/// [`MoveFailed`](Self::MoveFailed), [`PartlyMoved`](Self::PartlyMoved), and
/// [`NotDurable`](Self::NotDurable) wrote the entry of the rebase; the other variants did not.
/// [`SnapshotNotDurable`](Self::SnapshotNotDurable) and [`AfterSnapshot`](Self::AfterSnapshot)
/// name the entry of a snapshot that came first.
#[derive(Debug)]
#[non_exhaustive]
pub enum RebaseError {
    /// The authorizer refused `rebase`, or the entry could not be written (see
    /// [`UpdateError::Log`]). No pointer moved and no file was written.
    Update(UpdateError),
    /// The references could not be read, the lock is held, or a purge is pending.
    Refs(RefError),
    /// The log could not be read.
    Log(OpLogError),
    /// A commit could not be looked up.
    Store(Box<StoreError>),
    /// The history could not be walked.
    History(HistoryError),
    /// The snapshot failed, or the owner of the working copy is not the actor.
    Snapshot(SnapshotError),
    /// A commit or a tree could not be read.
    Read(SnapshotError),
    /// A commit could not be replayed: an object is missing or damaged, or the result is not a
    /// valid commit.
    Replay(Box<RewriteError>),
    /// A file is in the way, or the files cannot be planned. Nothing was written.
    Checkout(CheckoutError),
    /// HEAD names a commit and not a branch, so there is no branch to move.
    NotOnBranch,
    /// The branch of HEAD has no commit yet.
    NoCommits(BranchName),
    /// There is no branch with the name.
    NoSuchBranch(BranchName),
    /// The store has no commit with the ID.
    NoSuchCommit(ObjectId),
    /// The branch and the target share no commit.
    NoCommonAncestor {
        /// The target.
        onto: CommitId,
    },
    /// The branch and the target have more than one best common ancestor (a criss-cross merge),
    /// so the range has no single start.
    AmbiguousBase {
        /// The target.
        onto: CommitId,
        /// The best common ancestors, sorted.
        bases: Vec<CommitId>,
    },
    /// The commits above the target do not form a chain of single parents down to it: the
    /// history was damaged after the rebase made it.
    BrokenChain {
        /// The commit where the chain breaks.
        commit: CommitId,
    },
    /// A commit of the range has more than one parent. A merge commit has no single base to
    /// replay it against.
    MergeInRange {
        /// The merge commit.
        commit: CommitId,
    },
    /// The newest entry is a rebase that did not finish and leads to another target (or the
    /// branch is not where it left it). A snapshot would save its files as changes, so nothing was
    /// done: the same rebase run again finishes it. Nothing was written.
    StoppedOnOtherTarget {
        /// The working-copy commit that the stopped rebase made.
        stopped: CommitId,
    },
    /// The files could not be brought to the new tree after the entry was written. No pointer
    /// moved, and the files done so far stay; the same rebase run again finishes it.
    CheckoutFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: CheckoutError,
    },
    /// The entry was written and the files are at the new tree, but the first pointer did not
    /// move.
    MoveFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: RefError,
    },
    /// The entry was written and the files are at the new tree, but only the branch moved.
    PartlyMoved {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why the working-copy commit did not move.
        source: RefError,
    },
    /// The rebase was made, but a directory could not be flushed to disk, so it may not survive a
    /// crash.
    NotDurable {
        /// The entry that was written.
        entry: Box<Entry>,
        /// The error of the flush.
        source: RefError,
    },
    /// The snapshot moved the working-copy commit and its directory could not be flushed. The
    /// rebase did not start.
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
        source: Box<RebaseError>,
    },
}

impl RebaseError {
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
            Self::History(error) => error.store_error().and_then(StoreError::io_error),
            Self::Snapshot(error) | Self::Read(error) => error.io_error(),
            Self::Replay(error) => error.io_error(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.io_error(),
            Self::AfterSnapshot { source, .. } => source.io_error(),
            Self::NotOnBranch
            | Self::NoCommits(_)
            | Self::NoSuchBranch(_)
            | Self::NoSuchCommit(_)
            | Self::NoCommonAncestor { .. }
            | Self::AmbiguousBase { .. }
            | Self::MergeInRange { .. }
            | Self::BrokenChain { .. }
            | Self::StoppedOnOtherTarget { .. } => None,
        }
    }

    /// The entry of the rebase, if the error leaves one in the log.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Update(error) => error.entry(),
            Self::CheckoutFailed { entry, .. }
            | Self::MoveFailed { entry, .. }
            | Self::PartlyMoved { entry, .. }
            | Self::NotDurable { entry, .. } => Some(entry),
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

    /// Whether the rebase took effect and only the flush failed: the result is to be reported and
    /// the rebase is not to be repeated.
    pub fn made(&self) -> bool {
        match self {
            Self::NotDurable { .. } => true,
            Self::AfterSnapshot { source, .. } => source.made(),
            _ => false,
        }
    }

    /// Whether a pointer moved although the call failed and the move is not known to be complete
    /// or durable: some pointers of the rebase moved, or the rebase or the snapshot moved a
    /// pointer and the flush failed.
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

impl fmt::Display for RebaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Update(error) => write!(f, "could not rebase: {error}"),
            Self::Refs(error) => write!(f, "could not rebase, nothing of it was written: {error}"),
            Self::Log(error) => write!(f, "could not read the operation log: {error}"),
            Self::Store(error) => write!(f, "could not look up a commit: {error}"),
            Self::History(error) => write!(f, "could not walk the history: {error}"),
            Self::Snapshot(error) if error.stands_alone() => error.fmt(f),
            Self::Snapshot(error) => write!(
                f,
                "could not save the working directory or check its owner first, so the rebase \
                 wrote nothing: {error}"
            ),
            Self::Read(error) => write!(f, "could not read a commit or a tree: {error}"),
            Self::Replay(error) => write!(f, "could not replay a commit: {error}"),
            Self::Checkout(error) => {
                write!(f, "could not rebase, nothing of it was written: {error}")
            }
            Self::NotOnBranch => write!(
                f,
                "HEAD names a commit and not a branch, so there is no branch to rebase; \
                 `stemma switch` changes to a branch"
            ),
            Self::NoCommits(name) => write!(
                f,
                "the branch `{name}` has no commit yet, so there is nothing to rebase"
            ),
            Self::NoSuchBranch(name) => write!(f, "there is no branch `{name}`"),
            Self::NoSuchCommit(id) => write!(f, "the store has no commit {id}"),
            Self::NoCommonAncestor { onto } => write!(
                f,
                "the branch and commit {onto} share no commit, so there is no base to rebase from"
            ),
            Self::AmbiguousBase { onto, bases } => write!(
                f,
                "the branch and commit {onto} have {} best common ancestors, so the range to \
                 replay has no single start",
                bases.len()
            ),
            Self::BrokenChain { commit } => write!(
                f,
                "commit {commit} does not have exactly one parent, so the commits above the \
                 target do not form a chain down to it; the history is damaged"
            ),
            Self::MergeInRange { commit } => write!(
                f,
                "commit {commit} is a merge commit in the range to replay; a rebase does not \
                 replay merge commits"
            ),
            Self::StoppedOnOtherTarget { stopped } => write!(
                f,
                "the newest rebase (working-copy commit {stopped}) did not finish and leads to \
                 another target; run that rebase again before any other command, as a snapshot \
                 would save its files as changes"
            ),
            Self::CheckoutFailed { entry, source } => write!(
                f,
                "could not write files; entry {} names a rebase that did not happen and no \
                 working-copy commit moved, and the files done so far stay: {source}; run the \
                 same rebase again before any other command, as a snapshot saves only the files \
                 that the working-copy commit tracks and would miss the files that only the new \
                 tree has",
                entry.number()
            ),
            Self::MoveFailed { entry, source } => write!(
                f,
                "entry {} was written and the files are at the new tree, but no pointer moved: \
                 {source}; run the same rebase again before any other command, as a snapshot \
                 saves only the files that the working-copy commit tracks and would miss the \
                 files that only the new tree has",
                entry.number()
            ),
            Self::PartlyMoved { entry, source } => write!(
                f,
                "entry {} was written and the files are at the new tree, but only the branch \
                 moved: {source}; run the same rebase again before any other command, it moves \
                 the working-copy commit",
                entry.number()
            ),
            Self::NotDurable { entry, source } => write!(
                f,
                "the rebase was made as entry {}, but a directory could not be flushed, so it \
                 may not survive a crash: {source}",
                entry.number()
            ),
            Self::SnapshotNotDurable { snapshot, source } => write!(
                f,
                "unsaved changes were saved first as entry {}, but the directory could not be \
                 flushed to disk and the saved state may not survive a crash, so the rebase did \
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

impl Error for RebaseError {
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
            Self::History(error) => error.source(),
            Self::Snapshot(error) | Self::Read(error) => error.source(),
            Self::Replay(error) => error.source(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.source(),
            Self::AfterSnapshot { source, .. } => source.source(),
            Self::NotOnBranch
            | Self::NoCommits(_)
            | Self::NoSuchBranch(_)
            | Self::NoSuchCommit(_)
            | Self::NoCommonAncestor { .. }
            | Self::AmbiguousBase { .. }
            | Self::MergeInRange { .. }
            | Self::BrokenChain { .. }
            | Self::StoppedOnOtherTarget { .. } => None,
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
    use crate::object::Commit;
    use crate::refs::RefStore;
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

    fn onto(branch: &str) -> Revision {
        Revision::Branch(name(branch))
    }

    fn entries(repository: &Repository) -> usize {
        repository.oplog().entries().unwrap().len()
    }

    fn commit_of(repository: &Repository, id: CommitId) -> AnyCommit {
        read_any_commit(repository.objects(), id).unwrap()
    }

    fn rebase(repository: &Repository, target: &Revision) -> Result<Rebased, RebaseError> {
        repository.rebase(target, &actor(), &AllowAll)
    }

    struct Made {
        snapshot: Option<Entry>,
        entry: Entry,
        old_tip: CommitId,
        new_tip: CommitId,
        replayed: Vec<Replayed>,
        working_copy: CommitId,
        conflicts: Vec<String>,
    }

    fn done(result: Result<Rebased, RebaseError>) -> Made {
        match result {
            Ok(Rebased::Done {
                snapshot,
                entry,
                old_tip,
                new_tip,
                replayed,
                working_copy,
                conflicts,
                ..
            }) => Made {
                snapshot: snapshot.map(|entry| *entry),
                entry: *entry,
                old_tip,
                new_tip,
                replayed,
                working_copy,
                conflicts,
            },
            other => panic!("expected a rebase, got {other:?}"),
        }
    }

    const BASE: &str = "one\ntwo\nthree\n";

    /// `main` has `a.txt` (`BASE`) and then `c.txt`; `feature` branches off the first commit of
    /// `main` and changes the first line of `a.txt`, then adds `d.txt`. The repository is on
    /// `feature` with an empty working-copy commit.
    fn history(dir: &TempDir, repository: &Repository) {
        write(dir, "a.txt", BASE);
        track(repository, "a.txt");
        commit(repository, "first");
        switch(repository, &create("feature"));
        write(dir, "a.txt", "ONE\ntwo\nthree\n");
        commit(repository, "feature edit");
        write(dir, "d.txt", "d\n");
        track(repository, "d.txt");
        commit(repository, "feature adds d");
        switch(repository, &existing("main"));
        write(dir, "c.txt", "c\n");
        track(repository, "c.txt");
        commit(repository, "main adds c");
        switch(repository, &existing("feature"));
    }

    #[test]
    fn the_commits_of_the_branch_are_replayed_on_the_target_and_keep_their_changes() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let main = tip(&repository, "main");
        let old = tip(&repository, "feature");
        let old_commits = [
            commit_of(&repository, commit_of(&repository, old).parents()[0]),
            commit_of(&repository, old),
        ];
        let count = entries(&repository);

        let made = done(rebase(&repository, &onto("main")));

        assert!(made.snapshot.is_none());
        assert!(made.conflicts.is_empty());
        assert_eq!(made.old_tip, old);
        assert_eq!(entries(&repository), count + 1, "one entry for all moves");
        assert_eq!(made.entry.change().unwrap().operation().as_str(), "rebase");
        assert_eq!(tip(&repository, "feature"), made.new_tip);
        assert_eq!(made.replayed.len(), 2);
        assert_eq!(made.replayed[1].commit, made.new_tip);
        let new_commits = [
            commit_of(&repository, made.replayed[0].commit),
            commit_of(&repository, made.replayed[1].commit),
        ];
        assert_eq!(new_commits[0].parents(), [main]);
        assert_eq!(new_commits[1].parents(), [made.replayed[0].commit]);
        for (new, old) in new_commits.iter().zip(&old_commits) {
            assert_eq!(new.change(), old.change(), "the change ID survives");
            assert_eq!(new.message(), old.message());
            assert_eq!(new.author(), old.author());
            assert_eq!(new.timestamp(), old.timestamp());
            assert_ne!(new.id(), old.id());
        }
        // The working-copy commit follows, and the files are those of the new tip.
        assert_eq!(repository.refs().working_copy().unwrap(), made.working_copy);
        assert_eq!(
            commit_of(&repository, made.working_copy).parents(),
            [made.new_tip]
        );
        assert_eq!(read(&dir, "a.txt"), "ONE\ntwo\nthree\n");
        assert_eq!(read(&dir, "c.txt"), "c\n");
        assert_eq!(read(&dir, "d.txt"), "d\n");
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(name("feature")),
            "HEAD stays"
        );
        assert_eq!(tip(&repository, "main"), main, "the target does not move");
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    #[test]
    fn a_target_that_the_branch_has_already_is_up_to_date_and_writes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let _ = done(rebase(&repository, &onto("main")));
        let count = entries(&repository);

        assert!(matches!(
            rebase(&repository, &onto("main")),
            Ok(Rebased::UpToDate)
        ));
        let own = Revision::Commit(tip(&repository, "feature").object_id());
        assert!(matches!(rebase(&repository, &own), Ok(Rebased::UpToDate)));
        assert_eq!(entries(&repository), count);
    }

    #[test]
    fn a_branch_below_the_target_moves_forward_without_replaying() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        // `fork` starts at the first commit of `main`, which is below the tip of `main`.
        switch(&repository, &existing("main"));
        let main = tip(&repository, "main");
        let fork = commit_of(&repository, main).parents()[0];
        switch(
            &repository,
            &SwitchTarget::Create {
                name: name("fork"),
                start: Some(Revision::Commit(fork.object_id())),
            },
        );

        let made = done(rebase(&repository, &onto("main")));

        assert!(made.replayed.is_empty());
        assert_eq!(made.new_tip, main);
        assert_eq!(tip(&repository, "fork"), main);
        assert_eq!(read(&dir, "c.txt"), "c\n");
    }

    #[test]
    fn a_conflict_is_data_of_the_replayed_commit_and_travels_up() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", BASE);
        track(&repository, "a.txt");
        commit(&repository, "first");
        switch(&repository, &create("feature"));
        write(&dir, "a.txt", "ONE\ntwo\nthree\n");
        commit(&repository, "feature edit");
        write(&dir, "d.txt", "d\n");
        track(&repository, "d.txt");
        commit(&repository, "feature adds d");
        switch(&repository, &existing("main"));
        write(&dir, "a.txt", "uno\ntwo\nthree\n");
        commit(&repository, "main edit");
        switch(&repository, &existing("feature"));

        let made = done(rebase(&repository, &onto("main")));

        assert_eq!(made.replayed.len(), 2);
        assert_eq!(
            made.replayed[0].conflicts,
            [ConflictedFile::new("a.txt", 2)]
        );
        // The second commit does not touch the conflicted path, so the conflict stays below it
        // and the commit carries it on.
        assert!(commit_of(&repository, made.replayed[1].commit).is_conflicted());
        assert_eq!(
            made.conflicts,
            ["a.txt"],
            "the working copy carries it as well"
        );
        assert!(read(&dir, "a.txt").starts_with("<<<<<<< conflict\n"));
        assert_eq!(read(&dir, "d.txt"), "d\n");
    }

    #[test]
    fn unsaved_changes_are_saved_first_and_carried_with_the_change_id() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        write(&dir, "d.txt", "d changed\n");
        let before = repository.refs().working_copy().unwrap();
        let change = commit_of(&repository, before).change();

        let made = done(rebase(&repository, &onto("main")));

        assert!(made.snapshot.is_some(), "the edit was saved first");
        assert_eq!(read(&dir, "d.txt"), "d changed\n");
        assert_eq!(read(&dir, "c.txt"), "c\n");
        assert_eq!(commit_of(&repository, made.working_copy).change(), change);
    }

    #[test]
    fn a_rebase_is_undone_by_one_restore() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let old = tip(&repository, "feature");
        let working = repository.refs().working_copy().unwrap();
        let made = done(rebase(&repository, &onto("main")));

        repository
            .restore(made.entry.number(), false, &actor(), &AllowAll)
            .unwrap();

        assert_eq!(tip(&repository, "feature"), old);
        assert_eq!(repository.refs().working_copy().unwrap(), working);
        assert_eq!(read(&dir, "a.txt"), "ONE\ntwo\nthree\n");
        assert!(!dir.path().join("c.txt").exists());
    }

    #[test]
    fn a_detached_head_and_an_unknown_target_are_refused_without_an_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let count = entries(&repository);

        let error = rebase(&repository, &onto("nowhere")).unwrap_err();
        assert!(matches!(error, RebaseError::NoSuchBranch(_)), "{error}");
        let absent = crate::object::Blob::new("absent").id().object_id();
        let error = rebase(&repository, &Revision::Commit(absent)).unwrap_err();
        assert!(matches!(error, RebaseError::NoSuchCommit(_)), "{error}");
        let error = repository
            .rebase(&onto("main"), &actor(), &DenyAll)
            .unwrap_err();
        assert!(matches!(error, RebaseError::Update(_)), "{error}");

        let feature = tip(&repository, "feature");
        switch(
            &repository,
            &SwitchTarget::Detach(Revision::Commit(feature.object_id())),
        );
        let count_detached = entries(&repository);
        let error = rebase(&repository, &onto("main")).unwrap_err();
        assert!(matches!(error, RebaseError::NotOnBranch), "{error}");
        assert_eq!(entries(&repository), count_detached);
        assert!(count < count_detached);
    }

    #[test]
    fn a_merge_commit_in_the_range_is_refused_before_anything_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        let first = commit_of(&repository, tip(&repository, "main")).parents()[0];
        let tip_commit = commit_of(&repository, feature);
        let AnyCommit::Commit(plain) = &tip_commit else {
            panic!("a commit without a conflict expected");
        };
        let merge = Commit::try_new(
            plain.tree(),
            vec![plain.parents()[0], first],
            plain.change(),
            actor(),
            plain.timestamp(),
            "merge",
        )
        .unwrap();
        let merge = repository.objects().put_commit(&merge).unwrap();
        repository
            .refs_store()
            .update_branch(
                &name("feature"),
                Some(feature),
                merge,
                repository.objects(),
                &crate::Allowed::granted(),
            )
            .unwrap();
        let count = entries(&repository);

        // The range from the best common ancestor reaches the merge commit.
        let error = rebase(&repository, &onto("main")).unwrap_err();

        assert!(
            matches!(error, RebaseError::MergeInRange { commit } if commit == merge),
            "{error}"
        );
        assert_eq!(entries(&repository), count);
        assert_eq!(tip(&repository, "feature"), merge);
    }

    #[test]
    fn a_stop_after_the_files_is_finished_by_the_same_rebase() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let before = repository.refs().working_copy().unwrap();
        let old = tip(&repository, "feature");
        let metadata = repository.metadata_dir();
        let denied = Rc::new(RefCell::new(None));
        let arm = Rc::clone(&denied);
        super::super::snapshot::pause::at_next(
            super::super::snapshot::pause::Stage::Moves,
            move || {
                *arm.borrow_mut() = Some(fault::deny_writes_in(&metadata));
            },
        );

        let error = rebase(&repository, &onto("main")).unwrap_err();
        drop(denied);
        let _ = fs::remove_file(repository.metadata_dir().join("refs.lock"));

        // The branch moves first and lies below the directory that cannot be written; the
        // working-copy commit is the pointer that did not move.
        assert!(matches!(error, RebaseError::PartlyMoved { .. }), "{error}");
        assert!(error.pointer_moved());
        let message = error.to_string();
        assert!(message.contains("run the same rebase again"), "{message}");
        let stopped = error.entry().unwrap().number();
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_ne!(tip(&repository, "feature"), old);
        assert_eq!(read(&dir, "c.txt"), "c\n", "the files are done");

        // A rebase onto something else is refused, as a snapshot would save the new files.
        let count = entries(&repository);
        let error = rebase(&repository, &Revision::Commit(old.object_id())).unwrap_err();
        assert!(
            matches!(error, RebaseError::StoppedOnOtherTarget { .. }),
            "{error}"
        );
        assert_eq!(entries(&repository), count);

        let made = done(rebase(&repository, &onto("main")));

        assert!(made.snapshot.is_none(), "the repeat takes no snapshot");
        assert!(made.entry.number() > stopped);
        assert_eq!(repository.refs().working_copy().unwrap(), made.working_copy);
        assert_eq!(tip(&repository, "feature"), made.new_tip);
        assert_eq!(read(&dir, "b.txt"), "b\n");
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    #[test]
    fn another_actor_does_not_finish_a_stopped_rebase() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let metadata = repository.metadata_dir();
        let denied = Rc::new(RefCell::new(None));
        let arm = Rc::clone(&denied);
        super::super::snapshot::pause::at_next(
            super::super::snapshot::pause::Stage::Moves,
            move || {
                *arm.borrow_mut() = Some(fault::deny_writes_in(&metadata));
            },
        );
        let _ = rebase(&repository, &onto("main")).unwrap_err();
        drop(denied);
        let _ = fs::remove_file(repository.metadata_dir().join("refs.lock"));
        let count = entries(&repository);
        let mara = Actor::human("mara").unwrap();

        let error = repository
            .rebase(&onto("main"), &mara, &AllowAll)
            .unwrap_err();

        assert!(
            matches!(error, RebaseError::Snapshot(SnapshotError::NotOwner { .. })),
            "{error}"
        );
        assert_eq!(entries(&repository), count);
    }

    #[test]
    fn a_file_in_the_way_is_found_before_anything_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        // `main` adds `c.txt`; here the file exists and no commit tracks it.
        write(&dir, "c.txt", "mine\n");
        let old = tip(&repository, "feature");
        let working = repository.refs().working_copy().unwrap();
        let count = entries(&repository);

        let error = rebase(&repository, &onto("main")).unwrap_err();

        assert!(matches!(error, RebaseError::Checkout(_)), "{error}");
        assert_eq!(read(&dir, "c.txt"), "mine\n");
        assert_eq!(tip(&repository, "feature"), old);
        assert_eq!(repository.refs().working_copy().unwrap(), working);
        assert_eq!(entries(&repository), count);
    }

    #[test]
    fn a_branch_without_a_commit_is_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let count = entries(&repository);

        let error = rebase(&repository, &onto("main")).unwrap_err();

        assert!(matches!(error, RebaseError::NoCommits(_)), "{error}");
        assert_eq!(entries(&repository), count);
    }

    #[test]
    fn a_repeat_is_for_the_same_target_exactly_and_not_for_one_below_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let main = tip(&repository, "main");
        let below = commit_of(&repository, main).parents()[0];
        let metadata = repository.metadata_dir();
        let denied = Rc::new(RefCell::new(None));
        let arm = Rc::clone(&denied);
        super::super::snapshot::pause::at_next(
            super::super::snapshot::pause::Stage::Moves,
            move || {
                *arm.borrow_mut() = Some(fault::deny_writes_in(&metadata));
            },
        );
        let _ = rebase(&repository, &onto("main")).unwrap_err();
        drop(denied);
        let _ = fs::remove_file(repository.metadata_dir().join("refs.lock"));
        let count = entries(&repository);

        // `below` is an ancestor of the stopped tip, but the stopped rebase was onto `main`.
        let error = rebase(&repository, &Revision::Commit(below.object_id())).unwrap_err();

        assert!(
            matches!(error, RebaseError::StoppedOnOtherTarget { .. }),
            "{error}"
        );
        assert_eq!(entries(&repository), count);
        let made = done(rebase(&repository, &onto("main")));
        assert_eq!(made.replayed.len(), 2);
    }

    #[test]
    fn a_repeat_that_stops_again_can_still_be_finished() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let stop = |repository: &Repository| {
            let metadata = repository.metadata_dir();
            let denied = Rc::new(RefCell::new(None));
            let arm = Rc::clone(&denied);
            super::super::snapshot::pause::at_next(
                super::super::snapshot::pause::Stage::Moves,
                move || {
                    *arm.borrow_mut() = Some(fault::deny_writes_in(&metadata));
                },
            );
            let error = rebase(repository, &onto("main")).unwrap_err();
            drop(denied);
            let _ = fs::remove_file(repository.metadata_dir().join("refs.lock"));
            error
        };
        // The first stop moved the branch; the repeat stops before any pointer, and its entry
        // does not name the branch any more.
        assert!(matches!(stop(&repository), RebaseError::PartlyMoved { .. }));
        assert!(matches!(stop(&repository), RebaseError::MoveFailed { .. }));

        let made = done(rebase(&repository, &onto("main")));

        assert!(made.snapshot.is_none(), "the repeat takes no snapshot");
        assert_eq!(repository.refs().working_copy().unwrap(), made.working_copy);
        assert_eq!(read(&dir, "b.txt"), "b\n");
    }
}
