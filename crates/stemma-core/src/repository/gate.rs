//! The merge gate: the optimistic swap that lands a pull request on `main` (ADR-019, STEMMA-241).
//!
//! [`merge_gate`](Repository::merge_gate) takes the tip `T` of `main`, merges the change onto it,
//! and moves `main` from `T` to the result `M` by compare and swap. Nothing is held while a check
//! runs, and nothing is held between the steps:
//!
//! 1. Read `T`, merge the change onto `T`, and write the blobs, the trees, and the commit `M`
//!    (one parent, `T`, so that `main` stays linear). A conflict ends the call here: it never
//!    enters `main`.
//! 2. Apply the rule table of ADR-019 to the results of the checks (levels 0 and 1: a clean merge
//!    onto the new tip is enough; levels 2 and 3: a result with `T` as its base).
//! 3. Take the lock of the references, and under it check that the pull request is still open
//!    and that `M` is in the store, write the entry of the move, swap the pointer, and then write
//!    `pr-merge`.
//!
//! The objects come before the lock. This amends ADR-008 and ADR-011 for the gate only: the lock
//! covers the check of the tip, the entries, and the move, and never the work of the merge. A
//! purge may remove objects that no reference reaches (ADR-014), so `M` is looked up under the
//! lock; if it is gone, or if `main` moved (`Stale`), or if another change holds the lock
//! (`Locked`), the gate starts again at step 1 with the new tip.

use std::error::Error;
use std::fmt;

use super::{now, Repository};
use crate::change::{ChangeId, GenerateError};
use crate::conflict::FromSumError;
use crate::history::HistoryError;
use crate::merge::{merge_commits, MergeError};
use crate::object::{Commit, CommitId};
use crate::oplog::{Change, Entry, Moved, OpLogError, OperationName, PR_MERGE};
use crate::policy::RiskLevel;
use crate::pull::TARGET;
use crate::refs::{BranchName, Logged, RefError};
use crate::register::Mode;
use crate::register::Register;
use crate::repository::{CheckError, LevelError, PolicyError, PullError, UpdateError};
use crate::store::{ObjectStore, ReadError, StoreError};
use crate::{Actor, Authorizer};

/// The operation of the entry that names the move of `main` by the gate. The entry of the merge
/// of the pull request is `pr-merge` (ADR-011); it moves no pointer, so the move has an entry of
/// its own. The name is not reserved: anyone can write an entry with it through `update_branch`,
/// so a later slice must not take such an entry as proof that the gate moved `main`.
const OPERATION: &str = "gate-merge";

/// How often one call of the gate starts again after `Stale`, `Locked`, or a purge. A technical
/// cap that keeps one call from spinning for ever; it is not the bound of ADR-019 that keeps a
/// change from starving (that bound is a policy value and has its own place).
pub const MAX_ATTEMPTS: u32 = 8;

/// How long the gate waits after it found the lock of another change, before it starts again. The
/// wait is short and bounded: a lock is held for the check and the write of one change only
/// (ADR-008), and a lock that a dead process left must not burn the attempts in milliseconds.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(20);

/// Why an attempt of the gate did not end the call and the gate starts again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Retry {
    /// Another change holds the lock of the references.
    Locked,
    /// `main` moved after the gate read the tip.
    Stale,
    /// A purge removed the commit `M` that the gate had written. Every failed attempt leaves
    /// the objects of its merge in the store; no reference reaches them, and a purge collects
    /// them (ADR-014).
    Purged,
}

impl fmt::Display for Retry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Locked => "another change held the lock of the references",
            Self::Stale => "`main` moved",
            Self::Purged => "a purge removed the merge commit",
        })
    }
}

/// A pull request that the gate merged, or whose merge it recorded (see
/// [`merge_gate`](Repository::merge_gate)).
#[derive(Debug)]
pub struct GateMerge {
    /// The commit `M` that `main` names now: one parent, the tip it was merged onto.
    pub commit: CommitId,
    /// The tip `T` of `main` before the swap, the only parent of `commit`.
    pub tip: CommitId,
    /// How many attempts the call needed: 1 if nothing moved in between.
    pub attempts: u32,
    /// The entry that names the move of `main`; `None` for a repair (see `repaired`), which
    /// moved nothing.
    pub moved: Option<Box<Entry>>,
    /// The entry `pr-merge` of the pull request.
    pub merged: Box<Entry>,
    /// Whether the call only recorded the merge of a change that `main` held already: an earlier
    /// call moved `main` and stopped before `pr-merge`. `commit` and `tip` are the tip of `main`.
    pub repaired: bool,
}

/// What the gate does with a change that it cannot merge, or cannot merge yet.
#[derive(Debug)]
#[non_exhaustive]
pub enum GateError {
    /// The authorizer refused the operation. Nothing was written.
    Denied {
        /// The name of the operation.
        operation: String,
        /// What it was asked for.
        target: String,
    },
    /// The repository has no branch `main`.
    NoMain,
    /// The branch of the pull request has another head than the one that the opening names. Any
    /// new commit voids the results (ADR-004): open a new pull request.
    BranchMoved {
        /// The number of the pull request.
        number: u64,
    },
    /// The merge of the change onto the tip has a conflict. The conflict never enters `main`:
    /// the change goes back to its author, who runs `rebase` onto the new tip (ADR-019).
    Conflict {
        /// The number of the pull request.
        number: u64,
        /// The tip of `main` that the merge was made on.
        tip: CommitId,
    },
    /// No result of a check counts under the rule of the risk level for this tip.
    NotChecked {
        /// The number of the pull request.
        number: u64,
        /// The risk level that the caller gave.
        level: RiskLevel,
        /// The tip of `main` that the rule was applied to.
        tip: CommitId,
    },
    /// The gate started again [`MAX_ATTEMPTS`] times without a landing.
    Contended {
        /// The number of the pull request.
        number: u64,
        /// The attempts made.
        attempts: u32,
        /// Why the last attempt did not land.
        last: Retry,
    },
    /// A commit could not be read.
    Read(ReadError),
    /// The authorizer failed in another way than a refusal.
    Update(UpdateError),
    /// The results of the checks could not be read or do not fit the pull request.
    Check(CheckError),
    /// The pull request is unknown or no longer open, or its entry could not be written.
    Pull(PullError),
    /// The trees could not be merged.
    Merge(MergeError),
    /// The history could not be walked.
    History(HistoryError),
    /// The merged tree is no valid commit.
    Commit(FromSumError),
    /// No change ID could be made.
    ChangeId(GenerateError),
    /// The object store failed. Nothing but objects was written.
    Store(StoreError),
    /// The references failed. Nothing was moved.
    Refs(RefError),
    /// The log could not be read or written.
    Log(OpLogError),
    /// The policy of the base commit cannot be used: in an enforcing repository it is missing,
    /// unreadable, or of an unknown version. The merge is refused, and nothing was written but
    /// objects.
    Policy(PolicyError),
    /// The level of the change cannot be told. The merge is refused.
    Level(LevelError),
    /// The entry of the move is written, and the file system failed before `main` moved. The
    /// pointer is where it was; the entry stays in the log as an intent, and `pr-merge` is not
    /// written. The call can be repeated.
    MoveFailed {
        /// The entry of the move, the only one that was written.
        entry: Box<Entry>,
        /// What failed.
        source: RefError,
    },
    /// `main` moved after the entry of the move, but the merge of the pull request could not be
    /// recorded: the pull request stays open although its change is on `main`. This is the
    /// window of a crash between the move and `pr-merge`. Nothing is lost: the next call finds
    /// the change in `main` and records the merge (see [`merge_gate`](Repository::merge_gate)).
    PullNotRecorded {
        /// The entry of the move, which stands.
        moved: Box<Entry>,
        /// The merge `M` that `main` names now.
        commit: CommitId,
        /// What failed.
        source: Box<PullError>,
    },
    /// `main` moved, but the directory could not be flushed to disk. The move took effect and
    /// its entry is written, but `pr-merge` is not: it is not known that `main` holds the
    /// change on disk. The pull request stays open, and the next call records the merge by the
    /// repair path.
    NotDurable {
        /// The entry of the move, which stands.
        moved: Box<Entry>,
        /// The merge `M` that `main` names now.
        commit: CommitId,
        /// What failed.
        source: Box<RefError>,
    },
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied { operation, target } => {
                write!(f, "`{operation}` on `{target}` is not allowed")
            }
            Self::NoMain => write!(f, "the repository has no branch `{TARGET}`"),
            Self::BranchMoved { number } => write!(
                f,
                "the branch of pull request {number} has another head than its opening names"
            ),
            Self::Conflict { number, tip } => write!(
                f,
                "pull request {number} conflicts with `{TARGET}` at {tip}; rebase the branch"
            ),
            Self::NotChecked { number, level, tip } => write!(
                f,
                "no result of a check counts for pull request {number} at level {} on the tip {tip}",
                *level as u8
            ),
            Self::Contended {
                number,
                attempts,
                last,
            } => write!(
                f,
                "pull request {number} was not merged in {attempts} attempts; the last: {last}"
            ),
            Self::Update(error) => error.fmt(f),
            Self::Read(error) => error.fmt(f),
            Self::Check(error) => error.fmt(f),
            Self::Pull(error) => error.fmt(f),
            Self::Merge(error) => error.fmt(f),
            Self::History(error) => error.fmt(f),
            Self::Commit(error) => error.fmt(f),
            Self::ChangeId(error) => error.fmt(f),
            Self::Store(error) => error.fmt(f),
            Self::Refs(error) => error.fmt(f),
            Self::Log(error) => error.fmt(f),
            Self::Policy(error) => write!(f, "the merge is refused: {error}"),
            Self::Level(error) => write!(f, "the merge is refused: {error}"),
            Self::MoveFailed { source, .. } => {
                write!(f, "the entry of the move is written, but `{TARGET}` did not move: {source}")
            }
            Self::PullNotRecorded { source, .. } => write!(
                f,
                "`{TARGET}` moved, but the merge of the pull request is not recorded; call the \
                 gate again to record it: {source}"
            ),
            Self::NotDurable { source, .. } => {
                write!(f, "`{TARGET}` moved, but it is not known to be on disk: {source}")
            }
        }
    }
}

impl Error for GateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Check(error) => Some(error),
            Self::Update(error) => Some(error),
            Self::Pull(error) => Some(error),
            Self::Merge(error) => Some(error),
            Self::History(error) => Some(error),
            Self::Commit(error) => Some(error),
            Self::ChangeId(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Refs(error) | Self::MoveFailed { source: error, .. } => Some(error),
            Self::NotDurable { source, .. } => Some(source.as_ref()),
            Self::PullNotRecorded { source, .. } => Some(source),
            Self::Read(error) => Some(error),
            Self::Log(error) => Some(error),
            Self::Policy(error) => Some(error),
            Self::Level(error) => Some(error),
            _ => None,
        }
    }
}

macro_rules! from_error {
    ($($source:ty => $variant:ident),* $(,)?) => {
        $(impl From<$source> for GateError {
            fn from(error: $source) -> Self {
                Self::$variant(error)
            }
        })*
    };
}

from_error! {
    CheckError => Check,
    PullError => Pull,
    MergeError => Merge,
    HistoryError => History,
    FromSumError => Commit,
    GenerateError => ChangeId,
    StoreError => Store,
    RefError => Refs,
    OpLogError => Log,
    ReadError => Read,
    PolicyError => Policy,
    LevelError => Level,
}

impl Repository {
    /// Lands the open pull request `number` on `main` (ADR-019): merges its branch onto the tip,
    /// applies the rule table to the results of the checks, and moves `main` by compare and swap.
    ///
    /// `level` is the risk level of the change and `register` the register that counts for the
    /// signatures (see [`counted_checks`](Self::counted_checks); `None` is the open mode). The
    /// caller gives the level because the policy that derives it from the paths is not part of
    /// the gate; the gate does not ask for approvals either (ADR-011: the entry of a merge checks
    /// no approval).
    ///
    /// The rule of the table: at least one result of a check must count. At levels 0 and 1 a
    /// passing result for the head counts whatever tip it names as its base, as the merge onto
    /// the tip is clean (a conflict is refused below); a run without a base is the run on a tip
    /// of `main` and never counts for a change. At levels 2 and 3 only a result with the current
    /// tip as its base counts.
    ///
    /// The gate does not enforce these rules yet; the caller must until the slices for them
    /// exist: the approvals of ADR-004 (none is asked here), the pause of the levels 2 and 3
    /// while the last verified tip is red (with the exemption of a declared repair), the bound
    /// `max_unverified`, the reservation against starving, and the rule that the authors of a
    /// change are registered actors (STEMMA-242 and the command). The level and the policy of the base
    /// commit come from [`merge_gate_by_policy`](Self::merge_gate_by_policy) (STEMMA-246).
    ///
    /// The commit `M` has the single parent `T`, the tree of the merge, the gate `actor` as its
    /// author, and a change ID of its own. The objects of the merge are written before the lock.
    /// Under the lock the gate checks that the pull request is still open and its branch
    /// unchanged, writes the entry of the move (`gate-merge`), moves `main`, and only then
    /// writes `pr-merge`, so a pull request is never recorded as merged before `main` holds its
    /// change. If `main` moved in between (`Stale`), the lock is held by another
    /// change (`Locked`), or a purge removed `M`, it starts again at step 1, at most
    /// [`MAX_ATTEMPTS`] times; after a `Locked` it waits a few milliseconds first. Every failed
    /// attempt leaves the objects of its merge in the store. No reference reaches them, and a
    /// purge collects them (ADR-014). The error `Contended` names the reason of the last attempt.
    ///
    /// # The window between the move and `pr-merge`
    ///
    /// A process can stop after `main` moved and before `pr-merge` is written, or the write of
    /// `pr-merge` can fail ([`GateError::PullNotRecorded`]). Then `main` holds the change, and
    /// the pull request is still open. Nothing is lost, and the pull request is never recorded
    /// as merged without the change. The repair path closes the window: the next call merges the
    /// branch onto the new tip, finds that this changes nothing (the tree equals the tree of the
    /// tip), and, under the lock, after it checked again that the pull request is open, its
    /// branch unchanged, and `main` at the tip, records `pr-merge` and moves nothing
    /// ([`GateMerge::repaired`]). The same path records a pull request whose commits `main`
    /// reaches. It writes nothing for a pull request that is not open and takes the normal path
    /// for a change that `main` does not hold. Until the repair, a reader sees an open pull
    /// request whose change is on `main`.
    ///
    /// The entries of a call that fails: [`GateError::MoveFailed`] and [`GateError::Log`] (a
    /// refused write of the entry of the move, which wrote nothing, or an entry that is not
    /// known to be flushed, which the error carries) keep `main` where it was; the call can be
    /// repeated. [`GateError::NotDurable`] carries the entry of the move: `main` moved, but the
    /// flush of the directory is not known, so `pr-merge` is not written, and the next call
    /// records it by the repair path, as after [`GateError::PullNotRecorded`].
    ///
    /// # Errors
    ///
    /// Returns [`GateError::Denied`] if the authorizer refuses `pr-merge` or `gate-merge` (each
    /// is asked once, before anything is written),
    /// [`GateError::Pull`] for a pull request that is unknown or not open,
    /// [`GateError::BranchMoved`], [`GateError::NoMain`], [`GateError::Conflict`],
    /// [`GateError::NotChecked`], and [`GateError::Contended`]. For all of these nothing is
    /// written but objects. [`GateError::MoveFailed`] and [`GateError::PullNotRecorded`] leave
    /// the entry of the move, as above. The other variants wrap a failure of the store, the
    /// references, or the log.
    pub fn merge_gate(
        &self,
        number: u64,
        level: RiskLevel,
        register: Option<&Register>,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<GateMerge, GateError> {
        self.merge_gate_with(number, Some(level), register, actor, authorizer)
    }

    /// [`merge_gate`](Self::merge_gate) with the rules of the policy of the base commit
    /// (STEMMA-246): the level of the change is derived from the policy file of the tip of `main`
    /// that the attempt merges onto ([`level_of_change`](Self::level_of_change)), and `mode` says
    /// which results of checks count (the register of an enforcing repository).
    ///
    /// In the open mode a repository without a policy file has the default policy: every path has
    /// level 2. In the enforcing mode a policy that is missing, unreadable, or of an unknown
    /// version refuses every merge with [`GateError::Policy`], which names the reason. The policy
    /// is read again for every attempt, so a change that moves `main` onto a new policy is judged
    /// by the policy of the tip it lands on. The values `reservation_after` and `max_unverified`
    /// of the same policy ([`policy_of_main`](Self::policy_of_main)) are the bounds that the
    /// slices for the reservation and the unverified window apply; the gate does not apply
    /// them yet.
    ///
    /// # Errors
    ///
    /// The errors of [`merge_gate`](Self::merge_gate), [`GateError::Policy`], and
    /// [`GateError::Level`].
    pub fn merge_gate_by_policy(
        &self,
        number: u64,
        mode: &Mode,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<GateMerge, GateError> {
        let register = match mode {
            Mode::Open => None,
            Mode::Enforcing(register) => Some(register),
        };
        self.merge_gate_with(number, None, register, actor, authorizer)
    }

    /// The loop of the gate. `fixed` is the level that the caller gave; without one the level
    /// comes from the policy of the tip of each attempt, and `register` says whether the
    /// repository is enforcing.
    fn merge_gate_with(
        &self,
        number: u64,
        fixed: Option<RiskLevel>,
        register: Option<&Register>,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<GateMerge, GateError> {
        let ask = |operation: &str, target: &str| {
            let name = OperationName::try_new(operation).expect("an operation name");
            self.authorize(actor, authorizer, &name, target)
                .map_err(|error| match error {
                    UpdateError::Denied { operation, target } => {
                        GateError::Denied { operation, target }
                    }
                    other => GateError::Update(other),
                })
        };
        // The record of the merge is asked for as well; its proof is not needed, as the entry
        // is written under the lock that the proof of the move opens.
        drop(ask(PR_MERGE, &format!("pull {number}"))?);
        let allowed = ask(OPERATION, TARGET)?;
        let mut last = Retry::Stale;
        for attempt in 1..=MAX_ATTEMPTS {
            match self.gate_attempt(number, fixed, register, actor, &allowed)? {
                Ok(mut merged) => {
                    merged.attempts = attempt;
                    return Ok(merged);
                }
                Err(retry) => {
                    last = retry;
                    if retry == Retry::Locked {
                        std::thread::sleep(LOCK_WAIT);
                    }
                }
            }
        }
        Err(GateError::Contended {
            number,
            attempts: MAX_ATTEMPTS,
            last,
        })
    }

    /// One pass over the steps of the gate. `Err(retry)` inside means that the gate starts again.
    fn gate_attempt(
        &self,
        number: u64,
        fixed: Option<RiskLevel>,
        register: Option<&Register>,
        actor: &Actor,
        allowed: &crate::Allowed,
    ) -> Result<Result<GateMerge, Retry>, GateError> {
        let objects = self.objects();
        let pulls = self.pull_requests()?;
        let pull = super::pull::open_pull_of(&pulls, number)?;
        let head = self.branch_head(pull.branch())?;
        if head != *pull.head() {
            return Err(GateError::BranchMoved { number });
        }
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        let tip = self.refs().branch(&main)?.ok_or(GateError::NoMain)?;

        // Step 1: the merge, written before the lock.
        let sum = merge_commits(objects, tip, head)?.simplify();
        let [tree] = sum.terms() else {
            return Err(GateError::Conflict { number, tip });
        };
        // The change is on `main` already if merging it changes nothing: its commits are in
        // `main`, or an earlier call moved `main` to its squash commit and stopped before
        // `pr-merge`. Only the record is missing then.
        let tip_tree = objects.get_commit(&tip)?.map(|commit| commit.tree());
        if tip_tree == Some(*tree) {
            return self.record_landed(number, head, tip, actor, allowed);
        }

        // Step 2: the rule table, at the level of the caller or of the policy of the tip.
        let level = match fixed {
            Some(level) => level,
            None => {
                let policy = self.policy_at(&tip, register.is_some())?;
                self.level_of_change(&policy, &tip, *tree)?
            }
        };
        if self
            .counted_checks(number, &head, &tip, level, register)?
            .is_empty()
        {
            return Err(GateError::NotChecked { number, level, tip });
        }

        let message = format!("Merge pull request {number} ({})", pull.branch());
        let commit = Commit::try_new(
            *tree,
            vec![tip],
            ChangeId::generate()?,
            actor.clone(),
            now(),
            message,
        )
        .expect("a commit with one parent has no duplicate parent");
        let merged = objects.put_commit(&commit)?;
        #[cfg(test)]
        pause::before_lock(merged);

        // Step 3: the lock covers the check, the entry of the move, and the swap. `pr-merge`
        // follows the swap, still under the lock.
        let lock = match self.refs_store().lock_writer(allowed) {
            Ok(lock) => lock,
            Err(RefError::Locked { .. }) => return Ok(Err(Retry::Locked)),
            Err(error) => return Err(error.into()),
        };
        let pulls = self.pull_requests()?;
        let pull = super::pull::open_pull_of(&pulls, number)?;
        if self.branch_head(pull.branch())? != head {
            return Err(GateError::BranchMoved { number });
        }
        let operation = OperationName::try_new(OPERATION).expect("an operation name");
        let logged =
            self.refs_store()
                .update_branch_after(&lock, &main, Some(tip), merged, objects, || {
                    let change = Change::try_new(
                        now(),
                        actor.clone(),
                        operation.clone(),
                        None,
                        None,
                        vec![(main.clone(), Moved::from_to(Some(tip), Some(merged)))],
                    )
                    .map_err(OpLogError::Entry)?;
                    self.append(&lock, &change)
                });
        let record = |moved: Entry| {
            #[cfg(test)]
            pause::after_move();
            self.record_pull_merge_locked(&lock, number, actor, head)
                .map(|entry| (moved.clone(), entry))
                .map_err(|source| GateError::PullNotRecorded {
                    moved: Box::new(moved),
                    commit: merged,
                    source: Box::new(source),
                })
        };
        let done = |(moved, entry): (Entry, Entry)| GateMerge {
            commit: merged,
            tip,
            attempts: 1,
            moved: Some(Box::new(moved)),
            merged: Box::new(entry),
            repaired: false,
        };
        match logged {
            // `main` moved, a purge took `M`, or another change holds the lock: start again.
            Err(RefError::Stale { .. }) => Ok(Err(Retry::Stale)),
            Err(RefError::MissingCommit(missing)) if missing == merged => Ok(Err(Retry::Purged)),
            Err(error) => Err(error.into()),
            Ok(Logged::Moved(moved)) => Ok(Ok(done(record(moved)?))),
            Ok(Logged::Unchanged) => unreachable!("the tip is not `M`: M is a new commit"),
            Ok(Logged::StepFailed(error)) => Err(GateError::Log(error)),
            Ok(Logged::MoveFailed(entry, source) | Logged::PartlyMoved(entry, source)) => {
                Err(GateError::MoveFailed {
                    entry: Box::new(entry),
                    source,
                })
            }
            // `pr-merge` is not written for a move that is not known to be on disk: after a
            // crash `main` may lack the change, and a pull request must never be recorded as
            // merged without it. The repair path records it on the next call.
            Ok(Logged::NotFlushed(moved, source)) => Err(GateError::NotDurable {
                moved: Box::new(moved),
                commit: merged,
                source: Box::new(source),
            }),
        }
    }

    /// The repair path: `main` holds the change of the pull request already, so the gate moves
    /// nothing and only records the merge, under the lock, after it checked again that the pull
    /// request is open, its branch unchanged, and `main` still at `tip`. Nothing is written for
    /// any other outcome.
    fn record_landed(
        &self,
        number: u64,
        head: CommitId,
        tip: CommitId,
        actor: &Actor,
        allowed: &crate::Allowed,
    ) -> Result<Result<GateMerge, Retry>, GateError> {
        #[cfg(test)]
        pause::before_repair_lock();
        let lock = match self.refs_store().lock_writer(allowed) {
            Ok(lock) => lock,
            Err(RefError::Locked { .. }) => return Ok(Err(Retry::Locked)),
            Err(error) => return Err(error.into()),
        };
        let pulls = self.pull_requests()?;
        let pull = super::pull::open_pull_of(&pulls, number)?;
        if self.branch_head(pull.branch())? != head {
            return Err(GateError::BranchMoved { number });
        }
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        if self.refs().branch(&main)? != Some(tip) {
            return Ok(Err(Retry::Stale));
        }
        let entry = self.record_pull_merge_locked(&lock, number, actor, head)?;
        Ok(Ok(GateMerge {
            commit: tip,
            tip,
            attempts: 1,
            moved: None,
            merged: Box::new(entry),
            repaired: true,
        }))
    }
}

/// The stop points of the gate for a test: steps that run after the objects are written and
/// before the lock is taken, to reproduce what another process does in between (ADR-019).
#[cfg(test)]
pub(super) mod pause {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use crate::object::CommitId;

    type Step = Box<dyn FnOnce(CommitId)>;
    type AfterMove = Box<dyn FnOnce() -> Box<dyn std::any::Any>>;

    thread_local! {
        static STEPS: RefCell<VecDeque<Step>> = RefCell::new(VecDeque::new());
        static AFTER_MOVE: RefCell<Option<AfterMove>> = RefCell::new(None);
        static HELD: RefCell<Vec<Box<dyn std::any::Any>>> = RefCell::new(Vec::new());
    }

    /// Sets a step that runs after `main` moved and before `pr-merge` is written. It returns a
    /// value (an armed fault, for instance) that stays alive until [`release`] is called.
    pub(super) fn queue_after_move(step: impl FnOnce() -> Box<dyn std::any::Any> + 'static) {
        AFTER_MOVE.with(|slot| *slot.borrow_mut() = Some(Box::new(step)));
    }

    /// The stop point after the move; keeps what the step returned alive.
    pub(super) fn after_move() {
        let step = AFTER_MOVE.with(|slot| slot.borrow_mut().take());
        if let Some(step) = step {
            let held = step();
            HELD.with(|held_list| held_list.borrow_mut().push(held));
        }
    }

    thread_local! {
        static REPAIR: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    /// Sets a step that runs in the repair path before the lock is taken.
    pub(super) fn queue_before_repair_lock(step: impl FnOnce() + 'static) {
        REPAIR.with(|slot| *slot.borrow_mut() = Some(Box::new(step)));
    }

    /// The stop point of the repair path before the lock.
    pub(super) fn before_repair_lock() {
        let step = REPAIR.with(|slot| slot.borrow_mut().take());
        if let Some(step) = step {
            step();
        }
    }

    /// Drops what the steps after the move returned.
    pub(super) fn release() {
        HELD.with(|held| held.borrow_mut().clear());
    }

    /// Queues `step`; the n-th attempt of the gate runs the n-th step, with the commit `M` that
    /// the attempt wrote.
    pub(super) fn queue(step: impl FnOnce(CommitId) + 'static) {
        STEPS.with(|steps| steps.borrow_mut().push_back(Box::new(step)));
    }

    /// The stop point: runs the next queued step, if any.
    pub(super) fn before_lock(merged: CommitId) {
        let step = STEPS.with(|steps| steps.borrow_mut().pop_front());
        if let Some(step) = step {
            step(merged);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::conflict::AnyCommit;
    use crate::fault;
    use crate::index::Index;
    use crate::object::{Blob, FileMode, TreeId};
    use crate::oplog::{CheckResult, PullKind};
    use crate::pull::PullState;
    use crate::register::{write_proof, ProofFile};
    use crate::repository::InitOptions;
    use crate::signature::{self, PayloadKind, PublicKey, SignError, Signature, Signer, NAMESPACE};
    use crate::testutil::TempDir;
    use crate::{AllowAll, Decision};
    use ssh_key::private::Ed25519Keypair;
    use ssh_key::{HashAlg, LineEnding, PrivateKey};

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &OperationName, _target: &str) -> Decision {
            Decision::Deny
        }
    }

    /// Refuses one operation and allows the rest.
    struct DenyOnly(&'static str);

    impl Authorizer for DenyOnly {
        fn authorize(&self, _actor: &Actor, operation: &OperationName, _target: &str) -> Decision {
            if operation.as_str() == self.0 {
                Decision::Deny
            } else {
                Decision::Allow
            }
        }
    }

    struct Seeded(PrivateKey);

    impl Seeded {
        fn new(seed: u8) -> Self {
            Self(PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32])))
        }

        fn public(&self) -> PublicKey {
            PublicKey::from_openssh(&self.0.public_key().to_openssh().unwrap()).unwrap()
        }
    }

    impl Signer for Seeded {
        fn sign(&self, message: &[u8]) -> Result<Signature, SignError> {
            let pem = self
                .0
                .sign(NAMESPACE, HashAlg::Sha512, message)
                .unwrap()
                .to_pem(LineEnding::LF)
                .unwrap();
            Ok(Signature::from_armored(&pem).unwrap())
        }
    }

    /// Ana and Mia are members, Daniel owns the register.
    fn register() -> Register {
        let text = format!(
            "stemma-register 1\nentry human ana member\nkey {} active\n\
             entry human daniel owner\nkey {} active\n\
             entry human mia member\nkey {} active\n",
            Seeded::new(2).public(),
            Seeded::new(1).public(),
            Seeded::new(3).public()
        );
        Register::parse(text.as_bytes()).unwrap()
    }

    fn human(id: &str) -> Actor {
        Actor::human(id).unwrap()
    }

    fn branch(name: &str) -> BranchName {
        BranchName::try_new(name).unwrap()
    }

    fn tree_of(repository: &Repository, files: &[(&str, &str)]) -> TreeId {
        let mut index = Index::new();
        for (path, text) in files {
            let blob = repository.objects().put_blob(&Blob::new(*text)).unwrap();
            index.insert(*path, FileMode::File, blob).unwrap();
        }
        index.write_tree(repository.objects()).unwrap()
    }

    fn commit_of(
        repository: &Repository,
        parent: CommitId,
        files: &[(&str, &str)],
        author: &str,
    ) -> CommitId {
        let tree = tree_of(repository, files);
        repository
            .objects()
            .put_commit(
                &Commit::try_new(
                    tree,
                    vec![parent],
                    ChangeId::generate().unwrap(),
                    human(author),
                    1,
                    format!("work of {author}"),
                )
                .unwrap(),
            )
            .unwrap()
    }

    fn set_branch(repository: &Repository, name: &str, expected: Option<CommitId>, to: CommitId) {
        repository
            .update_branch(
                &OperationName::try_new("branch").unwrap(),
                &branch(name),
                expected,
                to,
                &human("daniel"),
                &AllowAll,
            )
            .unwrap();
    }

    /// `main` at the root `base`, whose file `c.txt` is `zero`. The branch `feature` has a commit
    /// of Mia that adds `a.txt` and edits `c.txt` as `feature` says; its pull request is open.
    struct Fixture {
        dir: TempDir,
        repository: Repository,
        base: CommitId,
        head: CommitId,
        number: u64,
    }

    fn fixture(c_in_feature: &str) -> Fixture {
        let dir = TempDir::new();
        let repository = Repository::init(
            dir.path(),
            &InitOptions::default(),
            &human("daniel"),
            &AllowAll,
        )
        .unwrap();
        let root = repository.refs().working_copy().unwrap();
        let base = commit_of(&repository, root, &[("c.txt", "zero\n")], "daniel");
        set_branch(&repository, "main", None, base);
        let head = commit_of(
            &repository,
            base,
            &[("a.txt", "a\n"), ("c.txt", c_in_feature)],
            "mia",
        );
        set_branch(&repository, "feature", None, head);
        let number = repository
            .open_pull(
                &branch("feature"),
                &human("mia"),
                vec![],
                None,
                None,
                &AllowAll,
            )
            .unwrap()
            .number();
        Fixture {
            dir,
            repository,
            base,
            head,
            number,
        }
    }

    impl Fixture {
        fn check(&self, base: CommitId) {
            self.repository
                .record_check(
                    self.head,
                    Some(base),
                    CheckResult::Pass,
                    &human("ana"),
                    &AllowAll,
                )
                .unwrap();
        }

        fn gate(&self, level: RiskLevel) -> Result<GateMerge, GateError> {
            self.repository
                .merge_gate(self.number, level, None, &human("daniel"), &AllowAll)
        }

        fn main(&self) -> CommitId {
            self.repository
                .refs()
                .branch(&branch("main"))
                .unwrap()
                .unwrap()
        }

        /// Another change lands on `main` that edits `b.txt` only, as a second writer would.
        fn land_other(&self) -> CommitId {
            let landed = commit_of(
                &self.repository,
                self.main(),
                &[("b.txt", "b\n"), ("c.txt", "zero\n")],
                "zoe",
            );
            set_branch(&self.repository, "main", Some(self.main()), landed);
            landed
        }

        /// A result of `writer` for `commit`.
        fn record(
            &self,
            commit: CommitId,
            base: Option<CommitId>,
            result: CheckResult,
            writer: &str,
        ) -> Entry {
            self.repository
                .record_check(commit, base, result, &human(writer), &AllowAll)
                .unwrap()
        }

        /// The entries that only a landing writes: the move of `main` and the merge record.
        fn landings(&self) -> usize {
            self.repository
                .oplog()
                .entries()
                .unwrap()
                .iter()
                .filter(|entry| {
                    entry
                        .change()
                        .is_some_and(|change| change.operation().as_str() == OPERATION)
                        || entry
                            .pull()
                            .is_some_and(|pull| pull.kind() == PullKind::Merge)
                })
                .count()
        }

        fn entries(&self) -> usize {
            self.repository.oplog().entries().unwrap().len()
        }

        fn path(&self) -> PathBuf {
            self.dir.path().to_path_buf()
        }
    }

    fn files_of(repository: &Repository, commit: CommitId) -> Vec<String> {
        let tree = repository
            .objects()
            .get_commit(&commit)
            .unwrap()
            .unwrap()
            .tree();
        let tree = repository.objects().get_tree(&tree).unwrap().unwrap();
        tree.entries().iter().map(|e| e.name().to_owned()).collect()
    }

    #[test]
    fn a_clean_change_lands_as_one_commit_on_the_tip_and_the_pull_request_is_merged() {
        let f = fixture("zero\n");
        f.check(f.base);
        let before = f.entries();

        let merged = f.gate(RiskLevel::Zero).unwrap();

        assert_eq!(merged.attempts, 1);
        assert_eq!(merged.tip, f.base);
        assert_eq!(f.main(), merged.commit);
        let commit = f
            .repository
            .objects()
            .get_commit(&merged.commit)
            .unwrap()
            .unwrap();
        assert_eq!(commit.parents(), [f.base]);
        assert_eq!(files_of(&f.repository, merged.commit), ["a.txt", "c.txt"]);
        let pulls = f.repository.pull_requests().unwrap();
        assert!(matches!(pulls[0].state(), PullState::Merged { commit, .. } if *commit == f.head));
        // The entry of the move comes first, then the record of the merge.
        let entries = f.repository.oplog().entries().unwrap();
        assert_eq!(entries.len(), before + 2);
        assert_eq!(
            entries[before].change().unwrap().operation().as_str(),
            OPERATION
        );
        assert!(entries[before + 1].pull().is_some());
        f.repository.oplog().verify().expect("the chain holds");
    }

    #[test]
    fn without_a_counted_result_nothing_is_merged_or_written() {
        let f = fixture("zero\n");
        let before = f.entries();

        let error = f.gate(RiskLevel::Zero).unwrap_err();

        assert!(matches!(error, GateError::NotChecked { .. }), "{error}");
        assert_eq!(f.main(), f.base);
        assert_eq!(f.entries(), before);
    }

    #[test]
    fn levels_0_and_1_keep_a_result_for_an_older_tip_if_the_merge_is_clean() {
        for level in [RiskLevel::Zero, RiskLevel::One] {
            let f = fixture("zero\n");
            f.check(f.base);
            let landed = f.land_other();

            let merged = f.gate(level).unwrap();

            assert_eq!(merged.tip, landed);
            assert_eq!(
                files_of(&f.repository, merged.commit),
                ["a.txt", "b.txt", "c.txt"]
            );
        }
    }

    #[test]
    fn levels_2_and_3_need_a_result_whose_base_is_the_current_tip() {
        for level in [RiskLevel::Two, RiskLevel::Three] {
            let f = fixture("zero\n");
            f.check(f.base);
            let landed = f.land_other();
            let before = f.entries();

            let error = f.gate(level).unwrap_err();

            assert!(
                matches!(error, GateError::NotChecked { tip, .. } if tip == landed),
                "{error}"
            );
            assert_eq!(f.main(), landed);
            assert_eq!(f.entries(), before);

            f.check(landed);
            assert_eq!(f.gate(level).unwrap().tip, landed);
        }
    }

    #[test]
    fn a_conflict_never_enters_main() {
        let f = fixture("feature\n");
        f.check(f.base);
        let landed = commit_of(&f.repository, f.base, &[("c.txt", "main\n")], "zoe");
        set_branch(&f.repository, "main", Some(f.base), landed);
        let before = f.entries();

        let error = f.gate(RiskLevel::Zero).unwrap_err();

        assert!(
            matches!(error, GateError::Conflict { tip, .. } if tip == landed),
            "{error}"
        );
        assert_eq!(f.main(), landed);
        assert_eq!(f.entries(), before);
        assert!(f.repository.pull_requests().unwrap()[0].is_open());
    }

    #[test]
    fn the_objects_are_written_before_the_lock_is_taken() {
        let f = fixture("zero\n");
        f.check(f.base);
        let path = f.path();
        pause::queue(move |merged| {
            let repository = Repository::open(&path).unwrap();
            assert!(repository.objects().contains(&merged.object_id()).unwrap());
            assert!(!path.join(".stemma/refs.lock").exists());
        });

        let done = f.gate(RiskLevel::Zero).unwrap();

        assert_eq!(done.attempts, 1);
    }

    #[test]
    fn a_move_of_main_between_the_merge_and_the_lock_starts_the_gate_again_on_the_new_tip() {
        let f = fixture("zero\n");
        f.check(f.base);
        let path = f.path();
        let other = commit_of(
            &f.repository,
            f.base,
            &[("b.txt", "b\n"), ("c.txt", "zero\n")],
            "zoe",
        );
        let base = f.base;
        pause::queue(move |_| {
            let repository = Repository::open(&path).unwrap();
            set_branch(&repository, "main", Some(base), other);
        });

        let done = f.gate(RiskLevel::Zero).unwrap();

        assert_eq!(done.attempts, 2);
        assert_eq!(done.tip, other);
        let commit = f
            .repository
            .objects()
            .get_commit(&done.commit)
            .unwrap()
            .unwrap();
        assert_eq!(commit.parents(), [other], "main stays linear");
        assert_eq!(f.main(), done.commit);
    }

    #[test]
    fn a_lock_that_another_change_holds_starts_the_gate_again_without_waiting() {
        let f = fixture("zero\n");
        f.check(f.base);
        let lock = f.path().join(".stemma/refs.lock");
        let held = lock.clone();
        pause::queue(move |_| fs::write(held, "someone").unwrap());
        pause::queue(move |_| fs::remove_file(lock).unwrap());

        let done = f.gate(RiskLevel::Zero).unwrap();

        assert_eq!(done.attempts, 2);
        assert_eq!(f.main(), done.commit);
    }

    #[test]
    fn a_purge_between_the_write_and_the_lock_makes_the_gate_write_the_commit_again() {
        let f = fixture("zero\n");
        f.check(f.base);
        let path = f.path();
        let first = std::rc::Rc::new(std::cell::Cell::new(None));
        let seen = first.clone();
        pause::queue(move |merged| {
            // What a purge does to an object that no reference reaches.
            let repository = Repository::open(&path).unwrap();
            assert!(repository.objects().remove(&merged.object_id()).unwrap());
            seen.set(Some(merged));
        });

        let done = f.gate(RiskLevel::Zero).unwrap();

        assert_eq!(done.attempts, 2);
        assert_ne!(Some(done.commit), first.get());
        assert!(f
            .repository
            .objects()
            .contains(&done.commit.object_id())
            .unwrap());
        assert_eq!(f.main(), done.commit);
    }

    #[test]
    fn the_gate_gives_up_after_the_cap_when_main_never_stands_still() {
        let f = fixture("zero\n");
        f.check(f.base);
        for _ in 0..MAX_ATTEMPTS {
            let path = f.path();
            pause::queue(move |_| {
                let repository = Repository::open(&path).unwrap();
                let tip = repository.refs().branch(&branch("main")).unwrap().unwrap();
                let next = commit_of(&repository, tip, &[("c.txt", "zero\n")], "zoe");
                set_branch(&repository, "main", Some(tip), next);
            });
        }

        let error = f.gate(RiskLevel::Zero).unwrap_err();

        assert!(
            matches!(error, GateError::Contended { attempts, last: Retry::Stale, .. } if attempts == MAX_ATTEMPTS),
            "{error}"
        );
        assert!(f.repository.pull_requests().unwrap()[0].is_open());
    }

    #[test]
    fn refusals_write_nothing() {
        let f = fixture("zero\n");
        f.check(f.base);
        let denied = f
            .repository
            .merge_gate(f.number, RiskLevel::Zero, None, &human("daniel"), &DenyAll)
            .unwrap_err();
        assert!(matches!(denied, GateError::Denied { .. }));
        let unknown = f
            .repository
            .merge_gate(99, RiskLevel::Zero, None, &human("daniel"), &AllowAll)
            .unwrap_err();
        assert!(matches!(
            unknown,
            GateError::Pull(PullError::NoSuchPull { .. })
        ));
        // The branch moved after the opening: any new commit voids the results.
        let more = commit_of(&f.repository, f.head, &[("a.txt", "a2\n")], "mia");
        set_branch(&f.repository, "feature", Some(f.head), more);
        let before = f.entries();
        let moved = f.gate(RiskLevel::Zero).unwrap_err();
        assert!(matches!(moved, GateError::BranchMoved { .. }), "{moved}");

        assert_eq!(f.entries(), before);
        assert_eq!(f.main(), f.base);
    }

    #[test]
    fn a_merged_pull_request_is_final() {
        let f = fixture("zero\n");
        f.check(f.base);
        f.gate(RiskLevel::Zero).unwrap();
        let before = f.entries();

        let again = f.gate(RiskLevel::Zero).unwrap_err();

        assert!(
            matches!(again, GateError::Pull(PullError::NotOpen { .. })),
            "{again}"
        );
        assert_eq!(f.entries(), before);
    }

    #[test]
    fn the_commit_of_the_gate_is_a_plain_commit_with_a_change_id_of_its_own() {
        let f = fixture("zero\n");
        f.check(f.base);

        let done = f.gate(RiskLevel::Zero).unwrap();

        let stored = f
            .repository
            .objects()
            .get_commit(&done.commit)
            .unwrap()
            .unwrap();
        let head = f.repository.objects().get_commit(&f.head).unwrap().unwrap();
        assert_ne!(stored.change(), head.change());
        assert_eq!(stored.author(), &human("daniel"));
        assert!(!AnyCommit::Commit(stored).is_conflicted());
    }

    #[test]
    fn a_branch_that_moves_between_the_merge_and_the_lock_is_found_under_the_lock() {
        let f = fixture("zero\n");
        f.check(f.base);
        let path = f.path();
        let head = f.head;
        pause::queue(move |_| {
            let repository = Repository::open(&path).unwrap();
            let more = commit_of(&repository, head, &[("a.txt", "a2\n")], "mia");
            set_branch(&repository, "feature", Some(head), more);
        });

        let error = f.gate(RiskLevel::Zero).unwrap_err();

        assert!(matches!(error, GateError::BranchMoved { .. }), "{error}");
        assert_eq!(f.main(), f.base);
        assert_eq!(f.landings(), 0);
    }

    #[test]
    fn a_pull_request_that_ends_between_the_merge_and_the_lock_is_found_under_the_lock() {
        for merge in [false, true] {
            let f = fixture("zero\n");
            f.check(f.base);
            let path = f.path();
            let (number, head) = (f.number, f.head);
            pause::queue(move |_| {
                let repository = Repository::open(&path).unwrap();
                if merge {
                    repository
                        .record_pull_merge(number, &human("daniel"), head, &AllowAll)
                        .unwrap();
                } else {
                    repository
                        .close_pull(number, &human("mia"), None, &AllowAll)
                        .unwrap();
                }
            });
            let before = f.entries();

            let error = f.gate(RiskLevel::Zero).unwrap_err();

            assert!(
                matches!(error, GateError::Pull(PullError::NotOpen { .. })),
                "{error}"
            );
            assert_eq!(f.main(), f.base);
            assert_eq!(
                f.entries(),
                before + 1,
                "only the entry of the other change"
            );
            assert_eq!(f.landings(), usize::from(merge));
        }
    }

    #[test]
    fn a_lock_that_stays_held_ends_in_contended_with_the_reason_and_writes_nothing() {
        let f = fixture("zero\n");
        f.check(f.base);
        fs::write(f.path().join(".stemma/refs.lock"), "someone").unwrap();
        let before = f.entries();
        let started = std::time::Instant::now();

        let error = f.gate(RiskLevel::Zero).unwrap_err();

        assert!(
            matches!(
                error,
                GateError::Contended {
                    last: Retry::Locked,
                    ..
                }
            ),
            "{error}"
        );
        assert!(started.elapsed() >= LOCK_WAIT, "the gate waits for a lock");
        assert!(error.to_string().contains("lock"), "{error}");
        assert_eq!(f.main(), f.base);
        assert_eq!(f.entries(), before);
        assert!(f.repository.pull_requests().unwrap()[0].is_open());
    }

    #[test]
    fn contended_after_a_move_of_main_leaves_main_and_the_log_to_the_other_writer() {
        let f = fixture("zero\n");
        f.check(f.base);
        for _ in 0..MAX_ATTEMPTS {
            let path = f.path();
            pause::queue(move |_| {
                let repository = Repository::open(&path).unwrap();
                let tip = repository.refs().branch(&branch("main")).unwrap().unwrap();
                let next = commit_of(&repository, tip, &[("c.txt", "zero\n")], "zoe");
                set_branch(&repository, "main", Some(tip), next);
            });
        }

        let error = f.gate(RiskLevel::Zero).unwrap_err();

        assert!(matches!(error, GateError::Contended { .. }));
        assert_eq!(f.landings(), 0);
        assert!(f.repository.pull_requests().unwrap()[0].is_open());
    }

    #[test]
    fn a_refusal_of_the_authorizer_names_the_operation_and_writes_nothing() {
        let f = fixture("zero\n");
        f.check(f.base);
        let before = f.entries();
        for operation in [OPERATION, PR_MERGE] {
            let error = f
                .repository
                .merge_gate(
                    f.number,
                    RiskLevel::Zero,
                    None,
                    &human("daniel"),
                    &DenyOnly(operation),
                )
                .unwrap_err();

            assert!(
                matches!(&error, GateError::Denied { operation: denied, .. } if denied == operation),
                "{error}"
            );
        }
        assert_eq!(f.main(), f.base);
        assert_eq!(f.entries(), before);
    }

    #[test]
    fn level_1_with_the_base_at_the_tip_and_levels_2_and_3_succeed_on_a_plain_result() {
        for level in [RiskLevel::One, RiskLevel::Two, RiskLevel::Three] {
            let f = fixture("zero\n");
            f.check(f.base);

            let done = f.gate(level).unwrap();

            assert_eq!(done.attempts, 1);
            assert_eq!(f.main(), done.commit);
        }
    }

    #[test]
    fn a_failed_result_a_result_for_another_head_and_a_run_without_a_base_do_not_count() {
        for level in [
            RiskLevel::Zero,
            RiskLevel::One,
            RiskLevel::Two,
            RiskLevel::Three,
        ] {
            let f = fixture("zero\n");
            let other = commit_of(&f.repository, f.base, &[("z.txt", "z\n")], "zoe");
            f.record(f.head, Some(f.base), CheckResult::Fail, "ana");
            f.record(other, Some(f.base), CheckResult::Pass, "ana");
            f.record(f.head, None, CheckResult::Pass, "ana");
            let before = f.entries();

            let error = f.gate(level).unwrap_err();

            assert!(
                matches!(error, GateError::NotChecked { .. }),
                "{level:?}: {error}"
            );
            assert_eq!(f.main(), f.base);
            assert_eq!(f.entries(), before);
        }
    }

    /// Lands a commit with the policy file on `main`; the feature of the fixture merges onto it.
    fn land_policy(f: &Fixture, text: &str) -> CommitId {
        let landed = commit_of(
            &f.repository,
            f.main(),
            &[("c.txt", "zero\n"), (crate::policy::FILE_NAME, text)],
            "daniel",
        );
        set_branch(&f.repository, "main", Some(f.main()), landed);
        landed
    }

    fn gate_by_policy(f: &Fixture, mode: &Mode) -> Result<GateMerge, GateError> {
        f.repository
            .merge_gate_by_policy(f.number, mode, &human("daniel"), &AllowAll)
    }

    #[test]
    fn the_open_mode_without_a_policy_file_takes_level_2_for_every_path() {
        let f = fixture("zero\n");
        // `main` moves on, so a result for the old tip counts at levels 0 and 1 only.
        f.land_other();
        f.check(f.base);
        let before = f.entries();

        let error = gate_by_policy(&f, &Mode::Open).unwrap_err();

        assert!(
            matches!(
                error,
                GateError::NotChecked {
                    level: RiskLevel::Two,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(f.entries(), before);

        f.check(f.main());
        let merged = gate_by_policy(&f, &Mode::Open).unwrap();
        assert_eq!(f.main(), merged.commit);
    }

    #[test]
    fn the_level_of_the_change_comes_from_the_policy_of_the_base_commit() {
        let f = fixture("zero\n");
        f.land_other();
        land_policy(&f, "stemma-policy 1\nlevel 0 a.txt\n");
        // The change adds `a.txt` only, which the policy puts at level 0: a result for an older
        // tip counts.
        f.check(f.base);

        let merged = gate_by_policy(&f, &Mode::Open).unwrap();

        assert_eq!(f.main(), merged.commit);
        assert_eq!(f.landings(), 2);
    }

    #[test]
    fn a_path_that_the_policy_raises_needs_a_result_for_the_current_tip() {
        let f = fixture("zero\n");
        land_policy(&f, "stemma-policy 1\nlevel 3 a.txt\n");
        f.check(f.base);
        let tip = f.main();

        let error = gate_by_policy(&f, &Mode::Open).unwrap_err();

        assert!(
            matches!(
                error,
                GateError::NotChecked {
                    level: RiskLevel::Three,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(f.main(), tip);
    }

    #[test]
    fn a_policy_in_the_change_does_not_lower_its_own_level() {
        // The branch adds a policy that gives everything level 0. The tip has none, so the
        // default applies, and the policy file itself has level 3 by the floor.
        let f = fixture("zero\n");
        let head = commit_of(
            &f.repository,
            f.base,
            &[
                ("a.txt", "a\n"),
                ("c.txt", "zero\n"),
                (crate::policy::FILE_NAME, "stemma-policy 1\nlevel 0 a.txt\n"),
            ],
            "mia",
        );
        set_branch(&f.repository, "relax", None, head);
        let number = f
            .repository
            .open_pull(
                &branch("relax"),
                &human("mia"),
                vec![],
                None,
                None,
                &AllowAll,
            )
            .unwrap()
            .number();
        f.land_other();
        f.record(head, Some(f.base), CheckResult::Pass, "ana");

        let error = f
            .repository
            .merge_gate_by_policy(number, &Mode::Open, &human("daniel"), &AllowAll)
            .unwrap_err();

        assert!(
            matches!(
                error,
                GateError::NotChecked {
                    level: RiskLevel::Three,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn an_enforcing_repository_without_a_policy_refuses_every_merge_and_names_the_reason() {
        let f = fixture("zero\n");
        let enforcing = Mode::Enforcing(register());
        let before = f.entries();

        let error = gate_by_policy(&f, &enforcing).unwrap_err();

        assert!(
            matches!(error, GateError::Policy(PolicyError::Missing)),
            "{error}"
        );
        assert!(error.to_string().contains(".stemma-policy"), "{error}");
        assert_eq!(f.main(), f.base);
        assert_eq!(f.entries(), before);
    }

    #[test]
    fn an_enforcing_repository_refuses_an_unreadable_or_unknown_policy() {
        for (text, reason) in [
            ("stemma-policy 2\n", "unknown version"),
            ("not a policy\n", "first line has to be"),
            ("", "no header"),
            ("stemma-policy 1\nlevel 9 x\n", "not a level"),
        ] {
            let f = fixture("zero\n");
            let tip = land_policy(&f, text);
            let before = f.entries();

            let error = gate_by_policy(&f, &Mode::Enforcing(register())).unwrap_err();

            assert!(
                matches!(error, GateError::Policy(PolicyError::Invalid(_))),
                "{error}"
            );
            assert!(error.to_string().contains(reason), "{text:?}: {error}");
            assert_eq!(f.main(), tip);
            assert_eq!(f.entries(), before);
        }
    }

    #[test]
    fn a_policy_that_is_a_directory_is_refused() {
        let f = fixture("zero\n");
        let landed = commit_of(
            &f.repository,
            f.main(),
            &[("c.txt", "zero\n"), (".stemma-policy/x", "1")],
            "daniel",
        );
        set_branch(&f.repository, "main", Some(f.main()), landed);

        for mode in [Mode::Open, Mode::Enforcing(register())] {
            let error = gate_by_policy(&f, &mode).unwrap_err();
            assert!(
                matches!(error, GateError::Policy(PolicyError::NotAFile)),
                "{error}"
            );
        }
        assert_eq!(f.main(), landed);
    }

    #[test]
    fn a_policy_file_that_exists_but_is_invalid_also_refuses_in_the_open_mode() {
        let f = fixture("zero\n");
        land_policy(&f, "stemma-policy 3\n");
        f.check(f.main());

        let error = gate_by_policy(&f, &Mode::Open).unwrap_err();

        assert!(
            matches!(error, GateError::Policy(PolicyError::Invalid(_))),
            "{error}"
        );
    }

    #[test]
    fn a_valid_policy_lets_the_gate_land_in_an_enforcing_repository() {
        let f = fixture("zero\n");
        let register = register();
        land_policy(
            &f,
            "stemma-policy 1\nlevel 0 a.txt\nreservation 5\nmax-unverified 20\n",
        );
        let policy = f.repository.policy_of_main(true).unwrap();
        assert_eq!(
            (policy.reservation_after(), policy.max_unverified()),
            (5, 20)
        );
        let entry = f.record(f.head, Some(f.main()), CheckResult::Pass, "ana");
        let signature = signature::sign(
            &Seeded::new(2),
            PayloadKind::LogEntry,
            entry.hash().unwrap().to_string().as_bytes(),
        )
        .unwrap();
        write_proof(
            &f.repository.metadata_dir(),
            entry.number(),
            &ProofFile {
                actor: human("ana"),
                signature,
            },
        )
        .unwrap();

        let merged = gate_by_policy(&f, &Mode::Enforcing(register)).unwrap();

        assert_eq!(f.main(), merged.commit);
    }

    #[test]
    fn a_conflict_is_refused_at_every_level() {
        for level in [RiskLevel::One, RiskLevel::Three] {
            let f = fixture("feature\n");
            let landed = commit_of(&f.repository, f.base, &[("c.txt", "main\n")], "zoe");
            set_branch(&f.repository, "main", Some(f.base), landed);
            f.check(landed);
            let before = f.entries();

            let error = f.gate(level).unwrap_err();

            assert!(matches!(error, GateError::Conflict { .. }), "{error}");
            assert_eq!(f.main(), landed);
            assert_eq!(f.entries(), before);
        }
    }

    #[test]
    fn in_an_enforcing_repository_only_a_result_with_a_proof_counts() {
        let f = fixture("zero\n");
        let register = register();
        let entry = f.record(f.head, Some(f.base), CheckResult::Pass, "ana");
        let before = f.entries();
        let gate = || {
            f.repository.merge_gate(
                f.number,
                RiskLevel::Zero,
                Some(&register),
                &human("daniel"),
                &AllowAll,
            )
        };

        let refused = gate().unwrap_err();

        assert!(matches!(refused, GateError::NotChecked { .. }), "{refused}");
        assert_eq!(f.entries(), before);
        let signature = signature::sign(
            &Seeded::new(2),
            PayloadKind::LogEntry,
            entry.hash().unwrap().to_string().as_bytes(),
        )
        .unwrap();
        write_proof(
            &f.repository.metadata_dir(),
            entry.number(),
            &ProofFile {
                actor: human("ana"),
                signature,
            },
        )
        .unwrap();
        let done = gate().unwrap();
        assert_eq!(f.main(), done.commit);
    }

    #[test]
    fn a_head_that_main_holds_already_is_recorded_as_merged_without_a_move() {
        let f = fixture("zero\n");
        let ahead = commit_of(
            &f.repository,
            f.head,
            &[("a.txt", "a\n"), ("c.txt", "zero\n")],
            "zoe",
        );
        set_branch(&f.repository, "main", Some(f.base), ahead);
        let before = f.entries();

        let done = f.gate(RiskLevel::Three).unwrap();

        assert!(done.repaired);
        assert!(done.moved.is_none());
        assert_eq!(f.main(), ahead, "main did not move");
        assert_eq!(f.entries(), before + 1, "only `pr-merge`");
        assert_eq!(f.landings(), 1);
        assert!(!f.repository.pull_requests().unwrap()[0].is_open());
    }

    #[test]
    fn the_repair_writes_nothing_for_a_pull_request_that_is_not_open() {
        let f = fixture("zero\n");
        let ahead = commit_of(
            &f.repository,
            f.head,
            &[("a.txt", "a\n"), ("c.txt", "zero\n")],
            "zoe",
        );
        set_branch(&f.repository, "main", Some(f.base), ahead);
        f.repository
            .close_pull(f.number, &human("mia"), None, &AllowAll)
            .unwrap();
        let before = f.entries();

        let error = f.gate(RiskLevel::Zero).unwrap_err();

        assert!(
            matches!(error, GateError::Pull(PullError::NotOpen { .. })),
            "{error}"
        );
        assert_eq!(f.entries(), before);
    }

    #[test]
    fn a_head_that_main_does_not_hold_is_no_repair() {
        // `main` moved on, but its tree lacks the change: the gate merges, it does not record.
        let f = fixture("zero\n");
        f.check(f.base);
        f.land_other();

        let done = f.gate(RiskLevel::Zero).unwrap();

        assert!(!done.repaired);
        assert!(done.moved.is_some());
    }

    #[test]
    fn a_failed_move_leaves_the_entry_of_the_move_and_no_pr_merge_and_can_be_repeated() {
        let f = fixture("zero\n");
        f.check(f.base);
        let before = f.entries();
        let heads = f.path().join(".stemma/refs/heads");
        let denied = fault::deny_writes_in(&heads);

        let error = f.gate(RiskLevel::Zero).unwrap_err();

        let GateError::MoveFailed { entry, .. } = &error else {
            panic!("{error}");
        };
        assert_eq!(entry.change().unwrap().operation().as_str(), OPERATION);
        assert_eq!(f.main(), f.base);
        assert_eq!(f.entries(), before + 1);
        assert_eq!(f.landings(), 1, "the entry of the move, and no `pr-merge`");
        assert!(f.repository.pull_requests().unwrap()[0].is_open());
        drop(denied);
        let done = f.gate(RiskLevel::Zero).unwrap();
        assert_eq!(f.main(), done.commit);
        assert!(!f.repository.pull_requests().unwrap()[0].is_open());
    }

    #[test]
    fn a_log_that_cannot_be_written_stops_the_gate_before_the_move() {
        let f = fixture("zero\n");
        f.check(f.base);
        let before = f.entries();
        let denied = fault::deny_writes_in(&f.path().join(".stemma/oplog"));

        let error = f.gate(RiskLevel::Zero).unwrap_err();

        assert!(matches!(error, GateError::Log(_)), "{error}");
        drop(denied);
        assert_eq!(f.main(), f.base);
        assert_eq!(f.entries(), before);
        assert!(f.repository.pull_requests().unwrap()[0].is_open());
    }

    #[test]
    fn a_move_that_is_not_flushed_stands_without_pr_merge_and_the_next_call_records_it() {
        let f = fixture("zero\n");
        f.check(f.base);
        let denied = crate::store::fail_flush_of(&f.path().join(".stemma/refs/heads"));

        let error = f.gate(RiskLevel::Zero).unwrap_err();

        drop(denied);
        let GateError::NotDurable { moved, commit, .. } = &error else {
            panic!("{error}");
        };
        assert_eq!(moved.change().unwrap().operation().as_str(), OPERATION);
        assert_eq!(f.main(), *commit);
        assert_eq!(f.landings(), 1, "the entry of the move, and no `pr-merge`");
        assert!(f.repository.pull_requests().unwrap()[0].is_open());

        let repaired = f.gate(RiskLevel::Zero).unwrap();

        assert!(repaired.repaired);
        assert_eq!(f.main(), *commit);
        assert_eq!(f.landings(), 2);
        assert!(!f.repository.pull_requests().unwrap()[0].is_open());
    }

    /// `main` holds the change of the pull request already, as after a stop before `pr-merge`.
    fn landed(f: &Fixture) -> CommitId {
        let ahead = commit_of(
            &f.repository,
            f.head,
            &[("a.txt", "a\n"), ("c.txt", "zero\n")],
            "zoe",
        );
        set_branch(&f.repository, "main", Some(f.base), ahead);
        ahead
    }

    #[test]
    fn the_repair_finds_a_branch_that_moved_under_the_lock_and_writes_nothing() {
        let f = fixture("zero\n");
        landed(&f);
        let path = f.path();
        let head = f.head;
        pause::queue_before_repair_lock(move || {
            let repository = Repository::open(&path).unwrap();
            let more = commit_of(&repository, head, &[("a.txt", "a2\n")], "mia");
            set_branch(&repository, "feature", Some(head), more);
        });
        let before = f.entries();

        let error = f.gate(RiskLevel::Zero).unwrap_err();

        assert!(matches!(error, GateError::BranchMoved { .. }), "{error}");
        assert_eq!(
            f.entries(),
            before + 1,
            "only the entry of the other change"
        );
        assert_eq!(f.landings(), 0);
        assert!(f.repository.pull_requests().unwrap()[0].is_open());
    }

    #[test]
    fn the_repair_retries_when_main_moved_under_the_lock() {
        let f = fixture("zero\n");
        let ahead = landed(&f);
        let path = f.path();
        pause::queue_before_repair_lock(move || {
            let repository = Repository::open(&path).unwrap();
            let next = commit_of(&repository, ahead, &[("b.txt", "b\n")], "zoe");
            set_branch(&repository, "main", Some(ahead), next);
        });

        let done = f.gate(RiskLevel::Zero).unwrap();

        assert!(done.repaired);
        assert_eq!(done.attempts, 2, "the first attempt saw a stale tip");
        assert_eq!(f.main(), done.tip);
        assert_eq!(f.landings(), 1, "one `pr-merge`, no move by the gate");
    }

    #[test]
    fn a_stop_between_the_move_and_pr_merge_is_repaired_by_the_next_call() {
        let f = fixture("zero\n");
        f.check(f.base);
        let oplog = f.path().join(".stemma/oplog");
        pause::queue_after_move(move || Box::new(fault::deny_writes_in(&oplog)));

        let error = f.gate(RiskLevel::Zero).unwrap_err();

        pause::release();
        let GateError::PullNotRecorded { moved, commit, .. } = &error else {
            panic!("{error}");
        };
        assert_eq!(moved.change().unwrap().operation().as_str(), OPERATION);
        assert_eq!(f.main(), *commit, "main moved");
        assert!(f.repository.pull_requests().unwrap()[0].is_open());
        assert_eq!(f.landings(), 1);

        let repaired = f.gate(RiskLevel::Zero).unwrap();

        assert!(repaired.repaired);
        assert_eq!(f.main(), *commit, "the repair moves nothing");
        assert_eq!(f.landings(), 2);
        assert!(!f.repository.pull_requests().unwrap()[0].is_open());
        f.repository.oplog().verify().expect("the chain holds");
    }
}
