//! The merge gate: the optimistic swap that lands a pull request on `main` (ADR-019, STEMMA-241).
//!
//! [`merge_gate`](Repository::merge_gate) takes the tip `T` of `main`, merges the change onto it,
//! and moves `main` from `T` to the result `M` by compare and swap. Nothing is held while a check
//! runs, and nothing is held between the steps:
//!
//! 1. Read `T`, merge the change onto `T`, and write the blobs and the trees (the commit `M` follows
//!    after step 2; it has one parent, `T`, so that `main` stays linear). A conflict ends the call here: it never
//!    enters `main`.
//! 2. Apply the rule table of ADR-019 to the results of the checks (levels 0 and 1: a clean merge
//!    onto the new tip is enough; levels 2 and 3: a result with `T` as its base).
//!    Then apply the bounds of ADR-019 ([`GateError::Paused`]): a red tip, the unverified window,
//!    and the reservation of another change, unless the change is a declared repair.
//! 3. Take the lock of the references, and under it check that the pull request is still open
//!    and that `M` is in the store, write the entry of the move, swap the pointer, and then write
//!    `pr-merge`.
//!
//! The objects come before the lock. This amends ADR-008 and ADR-011 for the gate only: the lock
//! covers the check of the tip, the entries, and the move, and never the work of the merge. A
//! purge may remove objects that no reference reaches (ADR-014), so `M` is looked up under the
//! lock; if it is gone, or if `main` moved (`Stale`), or if another change holds the lock
//! (`Locked`), the gate starts again at step 1 with the new tip.
//!
//! The gate moves the branch and no working copy. The worktree that has `main` checked out stays
//! on the old tip until [`follow_main`](Repository::follow_main) brings it along, which the
//! command calls after the gate (STEMMA-261).

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use super::verdict::{Case, LogProofs, Refusal, Rules, Verdict};
use super::{now, KeptNamespaceReadError, Pause, Repository};
use crate::change::{ChangeId, GenerateError};
use crate::conflict::FromSumError;
use crate::history::HistoryError;
use crate::merge::MergeError;
use crate::object::{Commit, CommitId};
use crate::oplog::{Change, Entry, Moved, OpLogError, OperationName, PR_MERGE};
use crate::policy::RiskLevel;
use crate::protocol::refusal::LevelSource;
use crate::pull::{NotCounted, PullRequest, TARGET};
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
    /// The approvals that count are fewer than the level of the change asks for (ADR-004): the
    /// approvals of agents that the policy names, and a human at the levels 2 and 3, an owner
    /// at level 3. Only an enforcing gate by the policy asks for them
    /// ([`merge_gate_by_policy`](Repository::merge_gate_by_policy) with a register).
    NotApproved {
        /// The number of the pull request.
        number: u64,
        /// The level of the change.
        level: RiskLevel,
        /// Whether the escalation for weakened tests set the level.
        source: LevelSource,
        /// The approvals of agents that count.
        agents: u32,
        /// The approvals of agents that the level asks for.
        agents_needed: u32,
        /// The approvals of humans that count for the level.
        humans: u32,
        /// The approvals of humans that the level asks for.
        humans_needed: u32,
        /// The approvals that do not count, each with the reason.
        dropped: Vec<Dropped>,
    },
    /// A bound of ADR-019 holds the change back: a red tip, the unverified window, or the
    /// reservation of another change. Nothing was written but objects; the call can be repeated
    /// when the reason has gone (see [`Pause`]).
    Paused {
        /// The number of the pull request.
        number: u64,
        /// Why it waits.
        pause: Pause,
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
    /// The policy of the base commit cannot be used: it is invalid, of an unknown version, or not
    /// a file (in either mode), or it is missing in an enforcing repository, or unreadable. The
    /// merge is refused, and nothing was written but objects.
    Policy(PolicyError),
    /// The level of the change cannot be told. The merge is refused.
    Level(LevelError),
    // The namespace policy (ADR-027, STEMMA-311).
    /// The namespace policy that a fetch kept cannot be read. The merge is refused, and nothing
    /// was written.
    Namespace(KeptNamespaceReadError),
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
            Self::NotApproved {
                number,
                level,
                source,
                agents,
                agents_needed,
                humans,
                humans_needed,
                dropped,
            } => {
                let by = match source {
                    LevelSource::Paths => "",
                    LevelSource::Weakened => " (raised for weakened tests)",
                };
                write!(
                    f,
                    "pull request {number} has level {}{by} and needs {agents_needed} approvals \
                     of agents and {humans_needed} of humans; {agents} and {humans} count",
                    *level as u8
                )?;
                for (index, dropped) in dropped.iter().enumerate() {
                    let lead = if index == 0 { "; not counted: " } else { ", " };
                    write!(f, "{lead}{dropped}")?;
                }
                Ok(())
            }
            Self::Paused { number, pause } => {
                write!(f, "pull request {number} waits: {pause}")
            }
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
            Self::Namespace(error) => write!(f, "the merge is refused: {error}"),
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
            Self::Namespace(error) => Some(error),
            _ => None,
        }
    }
}

impl GateError {
    /// The error of the local gate for a refusal of the verdict about `main`.
    fn refused(refusal: Refusal, number: u64, tip: CommitId, pull: &PullRequest) -> Self {
        match refusal {
            Refusal::BranchMoved { head: Some(_) } => Self::BranchMoved { number },
            Refusal::BranchMoved { head: None } => Self::Pull(PullError::NoSuchBranch {
                branch: pull.branch().clone(),
            }),
            Refusal::Conflict => Self::Conflict { number, tip },
            Refusal::Policy(error) => Self::Policy(error),
            Refusal::Level(error) => Self::Level(error),
            Refusal::NotChecked { level, .. } => Self::NotChecked { number, level, tip },
            Refusal::NotApproved {
                level,
                source,
                agents,
                agents_needed,
                humans,
                humans_needed,
                dropped,
            } => Self::NotApproved {
                number,
                level,
                source,
                agents,
                agents_needed,
                humans,
                humans_needed,
                dropped,
            },
            Refusal::Paused(pause) => Self::Paused { number, pause },
        }
    }
}

/// An approval that does not count for the verdict of the gate (ADR-004), with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    /// Who approved.
    pub reviewer: Actor,
    /// Why the approval does not count.
    pub reason: DroppedReason,
}

impl fmt::Display for Dropped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identity = crate::register::Identity::of(&self.reviewer);
        write!(f, "{identity} ({})", self.reason)
    }
}

/// Why an approval does not count for the verdict of the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DroppedReason {
    /// The recount of the pull request voids it ([`crate::pull::recount`]).
    Recount(NotCounted),
    /// The reviewer is a human, but no owner, and level 3 asks for the approval of an owner.
    NotOwner,
}

impl fmt::Display for DroppedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recount(why) => why.fmt(f),
            Self::NotOwner => f.write_str("level 3 asks for the approval of an owner"),
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
    /// This entry reads no policy: the level and the register are the caller's. A caller that
    /// enforces the policy of the base commit uses
    /// [`merge_gate_by_policy`](Self::merge_gate_by_policy).
    ///
    /// The rule of the table: at least one result of a check must count. At levels 0 and 1 a
    /// passing result for the head counts whatever tip it names as its base, as the merge onto
    /// the tip is clean (a conflict is refused below); a run without a base is the run on a tip
    /// of `main` and never counts for a change. At levels 2 and 3 only a result with the current
    /// tip as its base counts.
    ///
    /// The bounds of ADR-019 are applied after the rule table ([`GateError::Paused`], see
    /// [`Pause`]): the pause of the levels 2 and 3 while the latest verified tip is red (a declared
    /// repair is exempt), the window `max_unverified`, and the reservation. Here, where the caller
    /// gives the level and no policy is read, `max_unverified` has its default
    /// ([`DEFAULT_MAX_UNVERIFIED`](crate::policy::DEFAULT_MAX_UNVERIFIED)) and no
    /// reservation is applied, as the levels of the other changes are the policy's to tell; the
    /// pause and the window hold in both modes. The gate does not yet ask for the approvals of
    /// ADR-004 (none is asked here), nor for the rule that the authors of a change are registered
    /// actors (the command). The level and the policy of the base commit come from
    /// [`merge_gate_by_policy`](Self::merge_gate_by_policy) (STEMMA-246).
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
    /// No working copy is read or written, in the normal path and in the repair path alike: the
    /// worktree that has `main` checked out keeps its working-copy commit on the old tip. The
    /// caller brings it along with [`follow_main`](Self::follow_main) after this call returned.
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
    /// [`GateError::NotChecked`], [`GateError::Paused`], and [`GateError::Contended`]. For all of
    /// these nothing is
    /// written but objects. [`GateError::MoveFailed`] and [`GateError::PullNotRecorded`] leave
    /// the entry of the move, as above. The other variants wrap a failure of the store, the
    /// references, or the log.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn merge_gate(
        &self,
        number: u64,
        level: RiskLevel,
        register: Option<&Register>,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<GateMerge, GateError> {
        self.require_oplog("merge through the gate")
            .map_err(GateError::Refs)?;
        self.merge_gate_with(number, Some(level), register, actor, authorizer)
    }

    /// [`merge_gate`](Self::merge_gate) with the rules of the policy of the base commit
    /// (STEMMA-246): the level of the change is derived from the policy file of the tip of `main`
    /// that the attempt merges onto, the highest level of its paths
    /// ([`level_of_change`](Self::level_of_change)) raised by the escalation for weakened tests
    /// (ADR-004, rule 10), and `mode` says which results of checks count (the register of an
    /// enforcing repository).
    ///
    /// In an enforcing repository the approvals of ADR-004 must count as well, by the recount of
    /// [`recount_approvals`](Self::recount_approvals) (STEMMA-303): as many approvals of
    /// registered agents as the policy names for the level, and at the levels 2 and 3 one
    /// approval of a registered human, at level 3 of an owner ([`GateError::NotApproved`]). The
    /// open mode has no register and asks for none. The verdict is the one that a hub reaches
    /// for the same inputs.
    ///
    /// In the open mode a repository without a policy file has the default policy: every path has
    /// level 2, except the files that the floor of the core keeps at level 3. In the enforcing
    /// mode a policy that is missing refuses every merge that is not already on `main`, and an
    /// invalid, unknown-version, non-file, or unreadable policy refuses in both modes
    /// ([`GateError::Policy`] names the reason). The policy is read again for every attempt: a
    /// change that carries a new policy is judged by the policy of the tip it merges onto, not by
    /// its own. A pull request that is already on `main` (the repair path) reads no policy, as
    /// it moves nothing. The values `reservation_after` and `max_unverified` of the same policy
    /// are the bounds of ADR-019 that the gate applies ([`GateError::Paused`]), and the levels of
    /// the other open changes, which the reservation needs, come from the same policy.
    /// `mode` is only as strict as the caller makes it: derive it with
    /// [`Mode::of`].
    ///
    /// # Errors
    ///
    /// The errors of [`merge_gate`](Self::merge_gate), [`GateError::Policy`],
    /// [`GateError::Level`], and [`GateError::NotApproved`].
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn merge_gate_by_policy(
        &self,
        number: u64,
        mode: &Mode,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<GateMerge, GateError> {
        self.require_oplog("merge through the gate")
            .map_err(GateError::Refs)?;
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
        let evidence = self.evidence(register).map_err(PullError::from)?;
        let pull = super::pull::open_pull_of(&evidence.pulls, number)?;
        let head = self.branch_head(pull.branch())?;
        if head != *pull.head() {
            return Err(GateError::BranchMoved { number });
        }
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        let tip = self.refs().branch(&main)?.ok_or(GateError::NoMain)?;
        let heads: BTreeMap<BranchName, CommitId> = self.refs().branches()?.into_iter().collect();
        // The writers are read for the ban on self-approval, which only a register and a policy
        // ask for.
        let writers = match (fixed, register) {
            (None, Some(_)) => self.pull_authors(&head)?,
            _ => Vec::new(),
        };

        // The namespace policies that the fetches kept count beside the policy of `T`, as the
        // namespace policy does on the hub (ADR-027, STEMMA-311).
        let namespaces = match fixed {
            Some(_) => Vec::new(),
            None => self
                .kept_namespace_policies()
                .map_err(GateError::Namespace)?,
        };

        // Steps 1 and 2: the verdict about `main` (STEMMA-303), the function that a hub calls
        // as well. The merge writes its blobs and trees before the lock.
        let proofs = LogProofs(register);
        let case = Case {
            tip,
            pull,
            evidence: &evidence,
            writers: &writers,
            heads: &heads,
            register,
            proofs: &proofs,
            rules: match fixed {
                Some(level) => Rules::Level(level),
                None => Rules::Policies(&namespaces),
            },
            now: now(),
        };
        let tree = match self.judge_main(&case)? {
            Ok(Verdict::Merge(merge)) => merge.tree,
            // The change is on `main` already if merging it changes nothing: its commits are in
            // `main`, or an earlier call moved `main` to its squash commit and stopped before
            // `pr-merge`. Only the record is missing then.
            Ok(Verdict::Landed) => {
                return self.record_landed(number, head, tip, actor, register, allowed)
            }
            Err(refusal) => return Err(GateError::refused(refusal, number, tip, pull)),
        };

        let message = format!("Merge pull request {number} ({})", pull.branch());
        let commit = Commit::try_new(
            tree,
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
        let pulls = self.pull_requests(register)?;
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
            self.record_pull_merge_locked(&lock, number, actor, head, register)
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
        register: Option<&Register>,
        allowed: &crate::Allowed,
    ) -> Result<Result<GateMerge, Retry>, GateError> {
        #[cfg(test)]
        pause::before_repair_lock();
        let lock = match self.refs_store().lock_writer(allowed) {
            Ok(lock) => lock,
            Err(RefError::Locked { .. }) => return Ok(Err(Retry::Locked)),
            Err(error) => return Err(error.into()),
        };
        let pulls = self.pull_requests(register)?;
        let pull = super::pull::open_pull_of(&pulls, number)?;
        if self.branch_head(pull.branch())? != head {
            return Err(GateError::BranchMoved { number });
        }
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        if self.refs().branch(&main)? != Some(tip) {
            return Ok(Err(Retry::Stale));
        }
        let entry = self.record_pull_merge_locked(&lock, number, actor, head, register)?;
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
    use crate::repository::statements::by_hand;
    use crate::repository::InitOptions;
    use crate::signature::{PublicKey, SignError, Signature, Signer, NAMESPACE};
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

    /// Ana and Mia are members, Daniel owns the register, and Daniel sponsors the agents `bot`
    /// and `cat` (keys of the seeds 4 and 5), which approve with him ([`approve`]).
    fn register() -> Register {
        let text = format!(
            "stemma-register 1\nentry human ana member\nkey {} active\n\
             entry human daniel owner\nkey {} active\n\
             entry human mia member\nkey {} active\n",
            Seeded::new(2).public(),
            Seeded::new(1).public(),
            Seeded::new(3).public()
        );
        let mut register = Register::parse(text.as_bytes()).unwrap();
        let daniel = crate::register::Identity::of(&human("daniel"));
        for (id, seed) in [("bot", 4), ("cat", 5)] {
            let agent = crate::register::Identity::of(&agent(id));
            let role = crate::register::Role::Member;
            let cosign = crate::signature::sign(
                &Seeded::new(1),
                crate::signature::PayloadKind::Sponsorship,
                &crate::register::sponsorship_payload(&agent, role, &daniel),
            )
            .unwrap();
            register
                .add_agent(
                    agent,
                    role,
                    daniel.clone(),
                    Seeded::new(seed).public(),
                    cosign,
                )
                .unwrap();
        }
        register
    }

    /// An agent of Daniel.
    fn agent(id: &str) -> Actor {
        Actor::agent(id, "model", "session", "daniel").unwrap()
    }

    /// Signed approvals of `head` for the pull request `number` that every level asks for in an
    /// enforcing repository (ADR-004): the agents `bot` and `cat`, and Daniel, a human owner.
    fn approve(f: &Fixture, number: u64, head: CommitId) {
        for (actor, seed) in [(agent("bot"), 4), (agent("cat"), 5), (human("daniel"), 1)] {
            by_hand::approve(
                &f.repository,
                &actor,
                number,
                head,
                Some(&Seeded::new(seed)),
            );
        }
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
        // Mia signs the opening, so the pull request counts in both modes; the tests of the
        // enforcing mode pass the register to the gate.
        let number = by_hand::open(
            &repository,
            &human("mia"),
            &branch("feature"),
            head,
            None,
            Some(&Seeded::new(3)),
        )
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
                    None,
                    None,
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
                .record_check(commit, base, result, &human(writer), None, None, &AllowAll)
                .unwrap()
        }

        /// A result of Ana for `commit` whose statement her key signed, so that it counts in an
        /// enforcing repository. It is written past the writer, which would ask for the same
        /// signature.
        fn proven(&self, commit: CommitId, base: Option<CommitId>, result: CheckResult) -> Entry {
            by_hand::check(
                &self.repository,
                &human("ana"),
                commit,
                base,
                result,
                Some(&Seeded::new(2)),
            )
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
        let pulls = f.repository.pull_requests(None).unwrap();
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
        assert!(f.repository.pull_requests(None).unwrap()[0].is_open());
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
        assert!(f.repository.pull_requests(None).unwrap()[0].is_open());
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
                        .record_pull_merge(number, &human("daniel"), head, None, &AllowAll)
                        .unwrap();
                } else {
                    repository
                        .close_pull(number, &human("mia"), None, None, &AllowAll)
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
        assert!(f.repository.pull_requests(None).unwrap()[0].is_open());
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
        assert!(f.repository.pull_requests(None).unwrap()[0].is_open());
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
            ("stemma-policy 3\n", "unknown version"),
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
        f.proven(f.head, Some(f.main()), CheckResult::Pass);
        approve(&f, f.number, f.head);

        let merged = gate_by_policy(&f, &Mode::Enforcing(register)).unwrap();

        assert_eq!(f.main(), merged.commit);
    }

    #[test]
    fn a_refusal_for_want_of_approvals_names_each_approval_that_does_not_count() {
        let f = fixture("zero\n");
        let register = register();
        land_policy(&f, "stemma-policy 1\nlevel 0 a.txt\n");
        f.proven(f.head, Some(f.main()), CheckResult::Pass);
        // `bot` signs with the key of `cat`, which the register does not hold for it.
        by_hand::approve(
            &f.repository,
            &agent("bot"),
            f.number,
            f.head,
            Some(&Seeded::new(5)),
        );

        let error = gate_by_policy(&f, &Mode::Enforcing(register)).unwrap_err();

        let GateError::NotApproved {
            agents: 0,
            agents_needed: 1,
            ref dropped,
            ..
        } = error
        else {
            panic!("{error}");
        };
        let [Dropped {
            reviewer,
            reason: DroppedReason::Recount(NotCounted::Proof(_)),
        }] = dropped.as_slice()
        else {
            panic!("{dropped:?}");
        };
        assert_eq!(reviewer, &agent("bot"));
        assert!(
            error.to_string().contains("not counted: agent bot ("),
            "{error}"
        );
    }

    #[test]
    fn the_policy_is_read_again_when_main_moves_between_the_attempts() {
        // The first tip says level 0 for `a.txt`, so a result for it is enough. While the gate
        // waits for the lock, `main` moves onto a policy with level 3. The second attempt must
        // judge by that policy, and a result for the old tip no longer counts.
        let f = fixture("zero\n");
        let first = land_policy(&f, "stemma-policy 1\nlevel 0 a.txt\n");
        f.check(first);
        let path = f.path();
        let second = commit_of(
            &f.repository,
            first,
            &[
                ("b.txt", "b\n"),
                ("c.txt", "zero\n"),
                (crate::policy::FILE_NAME, "stemma-policy 1\nlevel 3 a.txt\n"),
            ],
            "zoe",
        );
        pause::queue(move |_| {
            let repository = Repository::open(&path).unwrap();
            set_branch(&repository, "main", Some(first), second);
        });

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
        assert_eq!(f.main(), second);
    }

    #[test]
    fn an_enforcing_repository_refuses_when_main_moves_onto_a_commit_without_a_policy() {
        // Attempt 1 has a valid policy and a signed result. Then `main` moves onto a commit that
        // has no policy file. Attempt 2 must refuse: nothing falls back to the old policy.
        let f = fixture("zero\n");
        let first = land_policy(&f, "stemma-policy 1\nlevel 0 a.txt\n");
        f.proven(f.head, Some(first), CheckResult::Pass);
        approve(&f, f.number, f.head);
        let path = f.path();
        let second = commit_of(
            &f.repository,
            first,
            &[("b.txt", "b\n"), ("c.txt", "zero\n")],
            "zoe",
        );
        pause::queue(move |_| {
            let repository = Repository::open(&path).unwrap();
            set_branch(&repository, "main", Some(first), second);
        });

        let error = gate_by_policy(&f, &Mode::Enforcing(register())).unwrap_err();

        assert!(
            matches!(error, GateError::Policy(PolicyError::Missing)),
            "{error}"
        );
        assert_eq!(f.main(), second);
        assert!(f.repository.pull_requests(None).unwrap()[0].is_open());
    }

    #[test]
    fn a_pull_request_that_main_already_holds_is_recorded_without_reading_the_policy() {
        // The first call lands in the open mode and is not flushed. The second call is
        // enforcing, and `main` has no policy file: the repair moves nothing, so it needs none.
        let f = fixture("zero\n");
        f.check(f.base);
        let denied = crate::store::fail_flush_of(&f.path().join(".stemma/refs/heads"));
        let error = gate_by_policy(&f, &Mode::Open).unwrap_err();
        drop(denied);
        assert!(matches!(error, GateError::NotDurable { .. }), "{error}");

        let repaired = gate_by_policy(&f, &Mode::Enforcing(register())).unwrap();

        assert!(repaired.repaired);
        assert!(!f.repository.pull_requests(None).unwrap()[0].is_open());
    }

    #[test]
    fn the_register_of_the_mode_decides_which_results_count() {
        // The same unsigned result: it counts in the open mode and not in the enforcing mode.
        let f = fixture("zero\n");
        land_policy(&f, "stemma-policy 1\nlevel 0 a.txt\n");
        f.record(f.head, Some(f.main()), CheckResult::Pass, "ana");

        let error = gate_by_policy(&f, &Mode::Enforcing(register())).unwrap_err();
        assert!(matches!(error, GateError::NotChecked { .. }), "{error}");

        let merged = gate_by_policy(&f, &Mode::Open).unwrap();
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
    fn in_an_enforcing_repository_only_a_signed_result_counts() {
        let f = fixture("zero\n");
        let register = register();
        f.record(f.head, Some(f.base), CheckResult::Pass, "ana");
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
        // The same result with a statement that the key of its writer signed counts.
        f.proven(f.head, Some(f.base), CheckResult::Pass);
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
        assert!(!f.repository.pull_requests(None).unwrap()[0].is_open());
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
            .close_pull(f.number, &human("mia"), None, None, &AllowAll)
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
        assert!(f.repository.pull_requests(None).unwrap()[0].is_open());
        drop(denied);
        let done = f.gate(RiskLevel::Zero).unwrap();
        assert_eq!(f.main(), done.commit);
        assert!(!f.repository.pull_requests(None).unwrap()[0].is_open());
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
        assert!(f.repository.pull_requests(None).unwrap()[0].is_open());
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
        assert!(f.repository.pull_requests(None).unwrap()[0].is_open());

        let repaired = f.gate(RiskLevel::Zero).unwrap();

        assert!(repaired.repaired);
        assert_eq!(f.main(), *commit);
        assert_eq!(f.landings(), 2);
        assert!(!f.repository.pull_requests(None).unwrap()[0].is_open());
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
        assert!(f.repository.pull_requests(None).unwrap()[0].is_open());
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
        assert!(f.repository.pull_requests(None).unwrap()[0].is_open());
        assert_eq!(f.landings(), 1);

        let repaired = f.gate(RiskLevel::Zero).unwrap();

        assert!(repaired.repaired);
        assert_eq!(f.main(), *commit, "the repair moves nothing");
        assert_eq!(f.landings(), 2);
        assert!(!f.repository.pull_requests(None).unwrap()[0].is_open());
        f.repository.oplog().verify().expect("the chain holds");
    }

    // The bounds of ADR-019 (STEMMA-242).

    fn merge(f: &Fixture, number: u64, level: RiskLevel) -> Result<GateMerge, GateError> {
        f.repository
            .merge_gate(number, level, None, &human("daniel"), &AllowAll)
    }

    fn paused(error: GateError) -> Pause {
        match error {
            GateError::Paused { pause, .. } => pause,
            other => panic!("expected a pause, got: {other}"),
        }
    }

    /// Writes the result of a check with the time `time`, which the clock does not give.
    fn record_at(
        f: &Fixture,
        time: i64,
        commit: CommitId,
        base: Option<CommitId>,
        result: CheckResult,
    ) -> Entry {
        by_hand::check_at(
            &f.repository,
            &human("ana"),
            time,
            (commit, base, result),
            None,
        )
    }

    /// A branch `name` with one commit on `parent` that holds `files`, and its open pull request,
    /// declared as the repair of `repairs`.
    fn open_branch(
        f: &Fixture,
        name: &str,
        parent: CommitId,
        files: &[(&str, &str)],
        repairs: Option<CommitId>,
    ) -> (CommitId, u64) {
        let head = commit_of(&f.repository, parent, files, "mia");
        set_branch(&f.repository, name, None, head);
        let number = by_hand::open(
            &f.repository,
            &human("mia"),
            &branch(name),
            head,
            repairs,
            Some(&Seeded::new(3)),
        )
        .number();
        (head, number)
    }

    /// `main` at `red`, which edits `c.txt`; a failing run covers it.
    fn red_tip(f: &Fixture) -> CommitId {
        let red = commit_of(&f.repository, f.base, &[("c.txt", "red\n")], "zoe");
        set_branch(&f.repository, "main", Some(f.base), red);
        f.record(red, None, CheckResult::Fail, "ana");
        red
    }

    #[test]
    fn a_red_tip_pauses_levels_2_and_3_and_lets_levels_0_and_1_go() {
        for (level, lands) in [
            (RiskLevel::Zero, true),
            (RiskLevel::One, true),
            (RiskLevel::Two, false),
            (RiskLevel::Three, false),
        ] {
            let f = fixture("zero\n");
            f.record(f.base, None, CheckResult::Fail, "ana");
            f.check(f.base);
            let before = f.entries();

            let result = f.gate(level);

            if lands {
                assert_eq!(f.main(), result.unwrap().commit, "{level:?}");
            } else {
                assert_eq!(paused(result.unwrap_err()), Pause::RedTip { red: f.base });
                assert_eq!(f.main(), f.base);
                assert_eq!(f.entries(), before, "{level:?}: nothing is written");
            }
        }
    }

    #[test]
    fn a_passing_run_on_a_newer_tip_ends_the_pause() {
        let f = fixture("zero\n");
        f.record(f.base, None, CheckResult::Fail, "ana");
        let tip = f.land_other();
        f.check(tip);
        assert!(matches!(
            f.gate(RiskLevel::Two),
            Err(GateError::Paused { .. })
        ));

        f.record(tip, None, CheckResult::Pass, "ana");

        let merged = f.gate(RiskLevel::Two).unwrap();
        assert_eq!(f.main(), merged.commit);
    }

    #[test]
    fn a_declared_repair_lands_during_a_red_tip_and_another_change_does_not() {
        let f = fixture("zero\n");
        let red = red_tip(&f);
        f.check(red);
        let (fix, repair) = open_branch(&f, "fix", red, &[("c.txt", "zero\n")], Some(red));
        f.repository
            .record_check(
                fix,
                Some(red),
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();

        let error = merge(&f, f.number, RiskLevel::Two).unwrap_err();
        assert_eq!(paused(error), Pause::RedTip { red });

        let merged = merge(&f, repair, RiskLevel::Two).unwrap();
        assert_eq!(f.main(), merged.commit);
    }

    #[test]
    fn a_change_that_only_declares_a_repair_is_not_exempt() {
        let f = fixture("zero\n");
        let red = red_tip(&f);
        // It names the red commit and changes another path.
        let (elsewhere, other) = open_branch(
            &f,
            "elsewhere",
            red,
            &[("z.txt", "z\n"), ("c.txt", "red\n")],
            Some(red),
        );
        f.repository
            .record_check(
                elsewhere,
                Some(red),
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();
        assert_eq!(
            paused(merge(&f, other, RiskLevel::Two).unwrap_err()),
            Pause::RedTip { red }
        );
    }

    #[test]
    fn a_repair_must_name_a_commit_between_the_last_green_and_the_first_red_tip() {
        let f = fixture("zero\n");
        // `base` is green, so it is not in the window; the red commit is.
        f.record(f.base, None, CheckResult::Pass, "ana");
        let red = red_tip(&f);
        let (fix, repair) = open_branch(&f, "fix", red, &[("c.txt", "zero\n")], Some(f.base));
        f.repository
            .record_check(
                fix,
                Some(red),
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();

        assert_eq!(
            paused(merge(&f, repair, RiskLevel::Two).unwrap_err()),
            Pause::RedTip { red }
        );
    }

    #[test]
    fn a_red_tip_that_appears_between_the_attempts_pauses_the_second_attempt() {
        // The result is valid for the first tip. While the gate waits for the lock, `main` moves
        // and the new tip turns red. The second attempt must see the red tip.
        let f = fixture("zero\n");
        f.check(f.base);
        let path = f.path();
        let (base, head) = (f.base, f.head);
        pause::queue(move |_| {
            let repository = Repository::open(&path).unwrap();
            let next = commit_of(
                &repository,
                base,
                &[("b.txt", "b\n"), ("c.txt", "zero\n")],
                "zoe",
            );
            set_branch(&repository, "main", Some(base), next);
            repository
                .record_check(
                    head,
                    Some(next),
                    CheckResult::Pass,
                    &human("ana"),
                    None,
                    None,
                    &AllowAll,
                )
                .unwrap();
            repository
                .record_check(
                    next,
                    None,
                    CheckResult::Fail,
                    &human("ana"),
                    None,
                    None,
                    &AllowAll,
                )
                .unwrap();
        });

        let error = f.gate(RiskLevel::Two).unwrap_err();

        assert!(matches!(
            paused(error),
            Pause::RedTip { red } if red == f.main()
        ));
    }

    #[test]
    fn an_unproven_failing_run_does_not_pause_an_enforcing_repository() {
        let f = fixture("zero\n");
        let register = register();
        f.proven(f.head, Some(f.base), CheckResult::Pass);
        // Anyone can write the entry of a failing run; without a signature it says nothing.
        f.record(f.base, None, CheckResult::Fail, "ana");
        f.proven(f.base, None, CheckResult::Fail);
        let gate = || {
            f.repository.merge_gate(
                f.number,
                RiskLevel::Two,
                Some(&register),
                &human("daniel"),
                &AllowAll,
            )
        };
        let before = f.entries();
        assert!(
            matches!(gate(), Err(GateError::Paused { .. })),
            "a signed failure pauses"
        );
        assert_eq!(f.entries(), before);

        let other = fixture("zero\n");
        other.proven(other.head, Some(other.base), CheckResult::Pass);
        other.record(other.base, None, CheckResult::Fail, "ana");
        let merged = other
            .repository
            .merge_gate(
                other.number,
                RiskLevel::Two,
                Some(&register),
                &human("daniel"),
                &AllowAll,
            )
            .unwrap();
        assert_eq!(other.main(), merged.commit);
    }

    const BOUNDED: &str = "stemma-policy 1\nreservation 2\nmax-unverified 3\nlevel 0 z.txt\n";

    #[test]
    fn the_unverified_window_stops_every_merge_until_a_run_covers_a_newer_commit() {
        let f = fixture("zero\n");
        // History: base, the policy, one more commit; the run covers `base`, two commits older.
        let policy = land_policy(&f, "stemma-policy 1\nmax-unverified 2\nlevel 0 a.txt\n");
        let tip = commit_of(
            &f.repository,
            policy,
            &[
                ("b.txt", "b\n"),
                ("c.txt", "zero\n"),
                (
                    crate::policy::FILE_NAME,
                    "stemma-policy 1\nmax-unverified 2\nlevel 0 a.txt\n",
                ),
            ],
            "zoe",
        );
        set_branch(&f.repository, "main", Some(policy), tip);
        f.record(f.base, None, CheckResult::Pass, "ana");
        f.check(tip);
        let before = f.entries();

        let error = gate_by_policy(&f, &Mode::Open).unwrap_err();

        assert_eq!(paused(error), Pause::Unverified { count: 2, max: 2 });
        assert_eq!(f.main(), tip);
        assert_eq!(f.entries(), before);

        f.record(tip, None, CheckResult::Pass, "ana");
        let merged = gate_by_policy(&f, &Mode::Open).unwrap();
        assert_eq!(f.main(), merged.commit);
    }

    #[test]
    fn the_unverified_window_has_the_default_bound_of_10_where_the_caller_gives_the_level() {
        let f = fixture("zero\n");
        let mut tip = f.base;
        for at in 0..10 {
            tip = commit_of(
                &f.repository,
                tip,
                &[("c.txt", "zero\n"), ("n.txt", &at.to_string())],
                "zoe",
            );
            set_branch(&f.repository, "main", Some(f.main()), tip);
        }
        f.record(f.base, None, CheckResult::Pass, "ana");
        f.check(tip);
        assert_eq!(
            paused(f.gate(RiskLevel::Zero).unwrap_err()),
            Pause::Unverified { count: 10, max: 10 }
        );
        f.record(tip, None, CheckResult::Pass, "ana");
        assert!(f.gate(RiskLevel::Zero).is_ok());
    }

    #[test]
    fn a_repair_is_exempt_from_the_unverified_window() {
        let f = fixture("zero\n");
        let policy = land_policy(&f, "stemma-policy 1\nmax-unverified 1\nlevel 0 a.txt\n");
        let red = commit_of(
            &f.repository,
            policy,
            &[
                ("c.txt", "red\n"),
                (
                    crate::policy::FILE_NAME,
                    "stemma-policy 1\nmax-unverified 1\nlevel 0 a.txt\n",
                ),
            ],
            "zoe",
        );
        set_branch(&f.repository, "main", Some(policy), red);
        let tip = commit_of(
            &f.repository,
            red,
            &[
                ("c.txt", "red\n"),
                ("n.txt", "n\n"),
                (
                    crate::policy::FILE_NAME,
                    "stemma-policy 1\nmax-unverified 1\nlevel 0 a.txt\n",
                ),
            ],
            "zoe",
        );
        set_branch(&f.repository, "main", Some(red), tip);
        f.record(f.base, None, CheckResult::Pass, "ana");
        f.record(red, None, CheckResult::Fail, "ana");
        f.check(tip);
        // `max-unverified 1`: `n.txt` is one commit after the latest verified tip `red`.
        assert!(matches!(
            paused(gate_by_policy(&f, &Mode::Open).unwrap_err()),
            Pause::Unverified { .. }
        ));

        let (fix, repair) = open_branch(
            &f,
            "fix",
            tip,
            &[
                ("c.txt", "zero\n"),
                ("n.txt", "n\n"),
                (
                    crate::policy::FILE_NAME,
                    "stemma-policy 1\nmax-unverified 1\nlevel 0 a.txt\n",
                ),
            ],
            Some(red),
        );
        f.repository
            .record_check(
                fix,
                Some(tip),
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();
        let merged = f
            .repository
            .merge_gate_by_policy(repair, &Mode::Open, &human("daniel"), &AllowAll)
            .unwrap();
        assert_eq!(f.main(), merged.commit);
    }

    /// `n` invalidated runs for the head of the fixture, the last one at `last`, `gap` seconds
    /// after the one before. The bases are old commits, so none is valid for the tip.
    fn lose_runs(f: &Fixture, last: i64, gap: i64) {
        let root = f.repository.refs().working_copy().unwrap();
        record_at(f, last - gap, f.head, Some(root), CheckResult::Pass);
        record_at(f, last, f.head, Some(f.base), CheckResult::Pass);
    }

    #[test]
    fn a_change_that_lost_n_runs_holds_a_reservation_and_levels_0_and_1_report_paused() {
        let f = fixture("zero\n");
        let tip = land_policy(&f, BOUNDED);
        lose_runs(&f, now(), 100);
        let (z, other) = open_branch(
            &f,
            "zfile",
            tip,
            &[
                ("c.txt", "zero\n"),
                ("z.txt", "z\n"),
                (crate::policy::FILE_NAME, BOUNDED),
            ],
            None,
        );
        f.repository
            .record_check(
                z,
                Some(tip),
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();
        f.record(tip, None, CheckResult::Pass, "ana");
        let before = f.entries();

        let error = f
            .repository
            .merge_gate_by_policy(other, &Mode::Open, &human("daniel"), &AllowAll)
            .unwrap_err();

        let Pause::Reserved { holder, until } = paused(error) else {
            panic!("expected the reservation");
        };
        assert_eq!(holder, f.number);
        assert!(until > now() + 100, "twice the 100 s of the last run");
        assert_eq!(f.entries(), before);

        // The holder lands with a result for the tip, and the reservation ends with it.
        f.check(tip);
        assert!(
            matches!(
                by_policy(&f, other, &Mode::Open),
                Err(GateError::Paused { .. })
            ),
            "its own result for the tip does not end the reservation"
        );
        let merged = gate_by_policy(&f, &Mode::Open).unwrap();
        assert_eq!(f.main(), merged.commit);
        let merged =
            f.repository
                .merge_gate_by_policy(other, &Mode::Open, &human("daniel"), &AllowAll);
        assert!(
            !matches!(merged, Err(GateError::Paused { .. })),
            "{merged:?}"
        );
    }

    #[test]
    fn the_reservation_ends_after_twice_the_duration_of_the_last_run() {
        let f = fixture("zero\n");
        let tip = land_policy(&f, BOUNDED);
        // The last run ended 1000 s ago after 100 s; the window ended 800 s ago.
        lose_runs(&f, now() - 1000, 100);
        let (z, other) = open_branch(
            &f,
            "zfile",
            tip,
            &[
                ("c.txt", "zero\n"),
                ("z.txt", "z\n"),
                (crate::policy::FILE_NAME, BOUNDED),
            ],
            None,
        );
        f.repository
            .record_check(
                z,
                Some(tip),
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();
        f.record(tip, None, CheckResult::Pass, "ana");

        let merged = f
            .repository
            .merge_gate_by_policy(other, &Mode::Open, &human("daniel"), &AllowAll)
            .unwrap();

        assert_eq!(f.main(), merged.commit);
    }

    /// The merge of a level 0 change while a change of level 2 lost its runs: the last at `last`,
    /// `gap` seconds after the one before. Margins of the tests are far above a second.
    fn merge_against_lost_runs(last: i64, gap: i64) -> Result<GateMerge, GateError> {
        let f = fixture("zero\n");
        let tip = land_policy(&f, BOUNDED);
        lose_runs(&f, last, gap);
        let (z, other) = open_branch(
            &f,
            "zfile",
            tip,
            &[
                ("c.txt", "zero\n"),
                ("z.txt", "z\n"),
                (crate::policy::FILE_NAME, BOUNDED),
            ],
            None,
        );
        f.repository
            .record_check(
                z,
                Some(tip),
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();
        f.record(tip, None, CheckResult::Pass, "ana");
        by_policy(&f, other, &Mode::Open)
    }

    #[test]
    fn the_reservation_holds_just_before_its_end_and_ends_just_after() {
        // The window is twice the 200 s of the last run: 400 s from its entry.
        let at = now();
        let held = merge_against_lost_runs(at - 370, 200).unwrap_err();
        let Pause::Reserved { until, .. } = paused(held) else {
            panic!("expected the reservation");
        };
        assert_eq!(until, at - 370 + 400);
        assert!(merge_against_lost_runs(at - 430, 200).is_ok());
    }

    #[test]
    fn the_reservation_lasts_one_hour_at_most_however_long_the_claimed_run() {
        // 10 days between the two entries would be 20 days of window.
        let at = now();
        let held = merge_against_lost_runs(at - 3000, 864_000).unwrap_err();
        let Pause::Reserved { until, .. } = paused(held) else {
            panic!("expected the reservation");
        };
        assert_eq!(until, at - 3000 + 3600);
        assert!(merge_against_lost_runs(at - 3700, 864_000).is_ok());
    }

    #[test]
    fn an_entry_dated_in_the_future_holds_nothing() {
        assert!(merge_against_lost_runs(now() + 1000, 100).is_ok());
    }

    #[test]
    fn fewer_than_n_invalidated_runs_hold_no_reservation() {
        let f = fixture("zero\n");
        let tip = land_policy(&f, BOUNDED);
        record_at(&f, now(), f.head, Some(f.base), CheckResult::Pass);
        let (z, other) = open_branch(
            &f,
            "zfile",
            tip,
            &[
                ("c.txt", "zero\n"),
                ("z.txt", "z\n"),
                (crate::policy::FILE_NAME, BOUNDED),
            ],
            None,
        );
        f.repository
            .record_check(
                z,
                Some(tip),
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();
        f.record(tip, None, CheckResult::Pass, "ana");

        assert!(f
            .repository
            .merge_gate_by_policy(other, &Mode::Open, &human("daniel"), &AllowAll)
            .is_ok());
    }

    #[test]
    fn a_reservation_that_appears_between_the_attempts_pauses_the_second_attempt() {
        // A level 0 change lands on the first tip. While the gate waits for the lock, `main`
        // moves and the other change loses its second run; the second attempt must pause.
        let f = fixture("zero\n");
        let tip = land_policy(&f, BOUNDED);
        let files = [
            ("c.txt", "zero\n"),
            ("z.txt", "z\n"),
            (crate::policy::FILE_NAME, BOUNDED),
        ];
        let (z, other) = open_branch(&f, "zfile", tip, &files, None);
        f.repository
            .record_check(
                z,
                Some(tip),
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();
        f.record(tip, None, CheckResult::Pass, "ana");
        let path = f.path();
        let (head, base) = (f.head, f.base);
        let root = f.repository.refs().working_copy().unwrap();
        pause::queue(move |_| {
            let repository = Repository::open(&path).unwrap();
            let next = commit_of(
                &repository,
                tip,
                &[
                    ("c.txt", "zero\n"),
                    ("y.txt", "y\n"),
                    (crate::policy::FILE_NAME, BOUNDED),
                ],
                "zoe",
            );
            set_branch(&repository, "main", Some(tip), next);
            let at = |time, base| {
                by_hand::check_at(
                    &repository,
                    &human("ana"),
                    time,
                    (head, Some(base), CheckResult::Pass),
                    None,
                );
            };
            at(now() - 100, root);
            at(now(), base);
            repository
                .record_check(
                    z,
                    Some(next),
                    CheckResult::Pass,
                    &human("ana"),
                    None,
                    None,
                    &AllowAll,
                )
                .unwrap();
        });

        let error = f
            .repository
            .merge_gate_by_policy(other, &Mode::Open, &human("daniel"), &AllowAll)
            .unwrap_err();

        assert!(matches!(paused(error), Pause::Reserved { holder, .. } if holder == f.number));
    }

    #[test]
    fn bisection_over_the_red_window_finds_the_commit_that_broke_main() {
        let f = fixture("zero\n");
        f.record(f.base, None, CheckResult::Pass, "ana");
        let mut window = Vec::new();
        let mut tip = f.base;
        for at in 0..5 {
            let next = commit_of(
                &f.repository,
                tip,
                &[("c.txt", "zero\n"), ("n.txt", &at.to_string())],
                "zoe",
            );
            set_branch(&f.repository, "main", Some(tip), next);
            window.push(next);
            tip = next;
        }
        f.record(tip, None, CheckResult::Fail, "ana");
        assert_eq!(f.repository.red_window(None).unwrap(), window);

        // The third commit broke it; the test is what a run of the checks on one commit says.
        let broken = window[2];
        let position = |commit: &CommitId| window.iter().position(|c| c == commit).unwrap();
        let found = crate::bounds::bisect(&window, |commit| {
            Ok::<_, ()>(position(commit) >= position(&broken))
        })
        .unwrap();

        assert_eq!(found.map(|at| window[at]), Some(broken));
        let green = fixture("zero\n");
        green.record(green.base, None, CheckResult::Pass, "ana");
        assert!(green.repository.red_window(None).unwrap().is_empty());
    }

    fn by_policy(f: &Fixture, number: u64, mode: &Mode) -> Result<GateMerge, GateError> {
        f.repository
            .merge_gate_by_policy(number, mode, &human("daniel"), &AllowAll)
    }

    /// The pull request `zfile` of level 0 (`z.txt`) on `tip`, with a result for `tip`.
    fn z_change(f: &Fixture, tip: CommitId, policy: &str) -> u64 {
        let (z, number) = open_branch(
            f,
            "zfile",
            tip,
            &[
                ("c.txt", "zero\n"),
                ("z.txt", "z\n"),
                (crate::policy::FILE_NAME, policy),
            ],
            None,
        );
        f.repository
            .record_check(
                z,
                Some(tip),
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();
        number
    }

    #[test]
    fn the_holder_keeps_the_reservation_when_its_result_for_the_tip_is_recorded() {
        // Its fresh result must not end the reservation before it merges, or any other change
        // lands first and invalidates the result again.
        let f = fixture("zero\n");
        let tip = land_policy(&f, BOUNDED);
        lose_runs(&f, now(), 100);
        let other = z_change(&f, tip, BOUNDED);
        f.record(tip, None, CheckResult::Pass, "ana");
        f.check(tip);

        let error = by_policy(&f, other, &Mode::Open).unwrap_err();

        assert!(matches!(paused(error), Pause::Reserved { holder, .. } if holder == f.number));
    }

    #[test]
    fn a_closed_change_and_a_holder_of_level_0_hold_no_reservation() {
        let f = fixture("zero\n");
        let tip = land_policy(&f, BOUNDED);
        lose_runs(&f, now(), 100);
        let other = z_change(&f, tip, BOUNDED);
        f.record(tip, None, CheckResult::Pass, "ana");
        f.repository
            .close_pull(f.number, &human("mia"), None, None, &AllowAll)
            .unwrap();
        assert!(by_policy(&f, other, &Mode::Open).is_ok(), "a closed change");

        // `a.txt` has level 0, so the feature is no change of level 2 or 3.
        let g = fixture("zero\n");
        let policy =
            "stemma-policy 1\nreservation 2\nmax-unverified 3\nlevel 0 a.txt\nlevel 0 z.txt\n";
        let tip = land_policy(&g, policy);
        lose_runs(&g, now(), 100);
        let other = z_change(&g, tip, policy);
        g.record(tip, None, CheckResult::Pass, "ana");
        assert!(
            by_policy(&g, other, &Mode::Open).is_ok(),
            "a holder of level 0"
        );
    }

    #[test]
    fn a_declared_repair_goes_ahead_of_a_reservation() {
        let f = fixture("zero\n");
        let tip = land_policy(&f, BOUNDED);
        let red = commit_of(
            &f.repository,
            tip,
            &[("c.txt", "red\n"), (crate::policy::FILE_NAME, BOUNDED)],
            "zoe",
        );
        set_branch(&f.repository, "main", Some(tip), red);
        f.record(red, None, CheckResult::Fail, "ana");
        lose_runs(&f, now(), 100);
        let (fix, repair) = open_branch(
            &f,
            "fix",
            red,
            &[("c.txt", "zero\n"), (crate::policy::FILE_NAME, BOUNDED)],
            Some(red),
        );
        f.repository
            .record_check(
                fix,
                Some(red),
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();

        let merged = by_policy(&f, repair, &Mode::Open).unwrap();

        assert_eq!(f.main(), merged.commit);
    }

    #[test]
    fn in_an_enforcing_repository_unsigned_runs_make_no_reservation() {
        for proven in [false, true] {
            let f = fixture("zero\n");
            let register = register();
            let tip = land_policy(&f, BOUNDED);
            let root = f.repository.refs().working_copy().unwrap();
            let key = Seeded::new(2);
            for (time, base) in [(now() - 100, root), (now(), f.base)] {
                by_hand::check_at(
                    &f.repository,
                    &human("ana"),
                    time,
                    (f.head, Some(base), CheckResult::Pass),
                    proven.then_some(&key as &dyn Signer),
                );
            }
            // The result of the other change and the run on the tip are signed in both cases.
            let other = z_change(&f, tip, BOUNDED);
            f.proven(tip, None, CheckResult::Pass);
            let pulls = f.repository.pull_requests(None).unwrap();
            let z = *pulls
                .iter()
                .find(|pull| pull.number() == other)
                .unwrap()
                .head();
            f.proven(z, Some(tip), CheckResult::Pass);
            approve(&f, other, z);

            let result = by_policy(&f, other, &Mode::Enforcing(register));

            if proven {
                assert!(matches!(
                    paused(result.unwrap_err()),
                    Pause::Reserved { .. }
                ));
            } else {
                assert!(
                    result.is_ok(),
                    "no reservation without a signature: {result:?}"
                );
            }
        }
    }

    #[test]
    fn an_enforcing_gate_does_not_merge_a_pull_request_whose_opening_is_not_signed() {
        let f = fixture("zero\n");
        let register = register();
        // A second branch at the same head, opened by hand without a signature.
        set_branch(&f.repository, "unsigned", None, f.head);
        let unsigned = by_hand::open(
            &f.repository,
            &human("mia"),
            &branch("unsigned"),
            f.head,
            None,
            None,
        )
        .number();
        f.proven(f.head, Some(f.base), CheckResult::Pass);
        let before = (f.entries(), f.main());
        let error = f
            .repository
            .merge_gate(
                unsigned,
                RiskLevel::Two,
                Some(&register),
                &human("daniel"),
                &AllowAll,
            )
            .unwrap_err();
        assert!(
            matches!(error, GateError::Pull(PullError::NoSuchPull { .. })),
            "{error}"
        );
        assert_eq!((f.entries(), f.main()), before);
        // The open mode knows the opening, and the signed pull request of the fixture lands.
        assert!(f.repository.pull_requests(None).unwrap().len() == 2);
        f.repository
            .merge_gate(
                f.number,
                RiskLevel::Two,
                Some(&register),
                &human("daniel"),
                &AllowAll,
            )
            .unwrap();
    }

    #[test]
    fn a_stand_in_failure_makes_the_tip_red_and_opens_no_repair() {
        // Anyone who writes the log can make a stand-in: an entry `check` that names a
        // statement which is not in the set, under any actor. It keeps a red tip red, and in
        // an enforcing repository it must not open the exemption of a repair.
        for signed in [true, false] {
            let f = fixture("zero\n");
            let register = register();
            let red = commit_of(&f.repository, f.base, &[("c.txt", "red\n")], "zoe");
            set_branch(&f.repository, "main", Some(f.base), red);
            if signed {
                f.proven(red, None, CheckResult::Fail);
            } else {
                by_hand::check_without_statement(
                    &f.repository,
                    &human("ana"),
                    (red, None, CheckResult::Fail),
                );
            }
            let (fix, repair) = open_branch(&f, "fix", red, &[("c.txt", "zero\n")], Some(red));
            f.proven(fix, Some(red), CheckResult::Pass);
            let gate = |number| {
                f.repository.merge_gate(
                    number,
                    RiskLevel::Two,
                    Some(&register),
                    &human("daniel"),
                    &AllowAll,
                )
            };
            // Every other change of level 2 waits at the red tip in both cases.
            f.proven(f.head, Some(red), CheckResult::Pass);
            assert_eq!(
                paused(gate(f.number).unwrap_err()),
                Pause::RedTip { red },
                "signed: {signed}"
            );
            if signed {
                gate(repair).expect("a signed failure opens the repair");
            } else {
                assert_eq!(paused(gate(repair).unwrap_err()), Pause::RedTip { red });
            }
        }
    }

    #[test]
    fn a_closing_without_its_proof_does_not_end_a_reservation() {
        for proof in [false, true] {
            let f = fixture("zero\n");
            let register = register();
            let tip = land_policy(&f, BOUNDED);
            let root = f.repository.refs().working_copy().unwrap();
            for (time, base) in [(now() - 100, root), (now(), f.base)] {
                by_hand::check_at(
                    &f.repository,
                    &human("ana"),
                    time,
                    (f.head, Some(base), CheckResult::Pass),
                    Some(&Seeded::new(2)),
                );
            }
            let other = z_change(&f, tip, BOUNDED);
            f.proven(tip, None, CheckResult::Pass);
            let pulls = f.repository.pull_requests(None).unwrap();
            let z = *pulls
                .iter()
                .find(|pull| pull.number() == other)
                .unwrap()
                .head();
            f.proven(z, Some(tip), CheckResult::Pass);
            // The holder is closed: by hand without a signature, or by its opener with hers.
            let key = Seeded::new(3);
            by_hand::close(
                &f.repository,
                &human("mia"),
                f.number,
                proof.then_some(&key as &dyn Signer),
            );
            let pulls = f.repository.pull_requests(Some(&register)).unwrap();
            let holder = pulls.iter().find(|pull| pull.number() == f.number).unwrap();
            assert!(!holder.is_open());
            assert_eq!(holder.ended_without_proof(), !proof);
            approve(&f, other, z);

            let result = by_policy(&f, other, &Mode::Enforcing(register));

            if proof {
                assert!(result.is_ok(), "the reservation ends: {result:?}");
            } else {
                assert!(
                    matches!(
                        paused(result.unwrap_err()),
                        Pause::Reserved { holder, .. } if holder == f.number
                    ),
                    "a closing without its proof does not end the reservation"
                );
            }
        }
    }

    #[test]
    fn a_red_tip_stays_when_the_statement_of_its_failing_run_is_lost() {
        for enforcing in [false, true] {
            let f = fixture("zero\n");
            let register = register();
            let mode = enforcing.then_some(&register);
            f.proven(f.head, Some(f.base), CheckResult::Pass);
            let failing = f.proven(f.base, None, CheckResult::Fail);
            let gate = || {
                f.repository
                    .merge_gate(f.number, RiskLevel::Two, mode, &human("daniel"), &AllowAll)
            };
            assert!(matches!(gate(), Err(GateError::Paused { .. })));
            // The file goes, and the tip stays red: the failure is read from its entry.
            let set = f.repository.statement_set();
            std::fs::remove_file(set.path_of(&failing.statement().unwrap())).unwrap();
            let before = f.entries();
            let error = gate().unwrap_err();
            assert!(
                matches!(paused(error), Pause::RedTip { red } if red == f.base),
                "enforcing: {enforcing}"
            );
            assert_eq!(f.entries(), before);
            // `op verify` reports the lost statement.
            let entries = f.repository.oplog().verify().unwrap();
            assert!(f.repository.verify_statements(&entries, mode).is_err());
        }
    }

    #[test]
    fn an_unproven_pass_does_not_clear_a_red_tip_in_an_enforcing_repository() {
        let f = fixture("zero\n");
        let register = register();
        f.proven(f.head, Some(f.base), CheckResult::Pass);
        f.proven(f.base, None, CheckResult::Fail);
        // The pass is only written, never signed.
        f.record(f.base, None, CheckResult::Pass, "ana");
        let before = f.entries();

        let error = f
            .repository
            .merge_gate(
                f.number,
                RiskLevel::Two,
                Some(&register),
                &human("daniel"),
                &AllowAll,
            )
            .unwrap_err();

        assert_eq!(paused(error), Pause::RedTip { red: f.base });
        assert_eq!(f.entries(), before);
    }

    #[test]
    fn a_run_of_the_opener_or_an_author_does_not_clear_a_red_tip_for_their_change() {
        let f = fixture("zero\n");
        f.record(f.base, None, CheckResult::Fail, "ana");
        f.check(f.base);
        // Mia opened the change and wrote its commit.
        f.record(f.base, None, CheckResult::Pass, "mia");
        assert_eq!(
            paused(f.gate(RiskLevel::Two).unwrap_err()),
            Pause::RedTip { red: f.base }
        );

        // A failing run of the author still pauses: it only holds a merge back.
        let other = fixture("zero\n");
        other.check(other.base);
        other.record(other.base, None, CheckResult::Fail, "mia");
        assert_eq!(
            paused(other.gate(RiskLevel::Two).unwrap_err()),
            Pause::RedTip { red: other.base }
        );

        f.record(f.base, None, CheckResult::Pass, "zoe");
        let merged = f.gate(RiskLevel::Two).unwrap();
        assert_eq!(f.main(), merged.commit);
    }

    #[test]
    fn a_failing_run_of_the_repairing_author_does_not_open_the_exemption() {
        let f = fixture("zero\n");
        let red = commit_of(&f.repository, f.base, &[("c.txt", "red\n")], "zoe");
        set_branch(&f.repository, "main", Some(f.base), red);
        // Mia writes the failing run, the fix, and opens the pull request.
        f.record(red, None, CheckResult::Fail, "mia");
        let (fix, repair) = open_branch(&f, "fix", red, &[("c.txt", "zero\n")], Some(red));
        f.repository
            .record_check(
                fix,
                Some(red),
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();

        assert_eq!(
            paused(merge(&f, repair, RiskLevel::Two).unwrap_err()),
            Pause::RedTip { red }
        );
    }

    #[test]
    fn one_commit_less_than_the_bound_lands() {
        let f = fixture("zero\n");
        let policy = land_policy(&f, "stemma-policy 1\nmax-unverified 3\nlevel 0 a.txt\n");
        let tip = commit_of(
            &f.repository,
            policy,
            &[
                ("b.txt", "b\n"),
                ("c.txt", "zero\n"),
                (
                    crate::policy::FILE_NAME,
                    "stemma-policy 1\nmax-unverified 3\nlevel 0 a.txt\n",
                ),
            ],
            "zoe",
        );
        set_branch(&f.repository, "main", Some(policy), tip);
        f.record(f.base, None, CheckResult::Pass, "ana");
        f.check(tip);
        // Two commits lie after `base`, and the bound is three.
        let merged = gate_by_policy(&f, &Mode::Open).unwrap();
        assert_eq!(f.main(), merged.commit);
    }

    #[test]
    fn the_red_window_runs_from_after_the_last_green_to_the_first_red_tip() {
        let f = fixture("zero\n");
        f.record(f.base, None, CheckResult::Pass, "ana");
        let mut chain = Vec::new();
        let mut tip = f.base;
        for at in 0..3 {
            tip = commit_of(
                &f.repository,
                tip,
                &[("c.txt", "zero\n"), ("n.txt", &at.to_string())],
                "zoe",
            );
            set_branch(&f.repository, "main", Some(f.main()), tip);
            chain.push(tip);
        }
        f.record(chain[1], None, CheckResult::Fail, "ana");

        assert_eq!(f.repository.red_window(None).unwrap(), [chain[0], chain[1]]);
        assert!(
            f.repository
                .red_window(Some(&register()))
                .unwrap()
                .is_empty(),
            "no signature"
        );
    }

    // The namespace policy that a fetch kept (ADR-027, STEMMA-311).
    mod namespace;
}
