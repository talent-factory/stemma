//! The verdict about `main` (ADR-019, ADR-025, STEMMA-303): one function over the commits, the
//! register and the policy of the tip `T`, and the statements, which the local gate calls in a
//! clone and the accepting operation calls on a hub.
//!
//! [`judge_main`](Repository::judge_main) answers whether the open pull request may move `main`
//! from `T` to the merge of its head:
//!
//! 1. The branch of the pull request is at the head of its opening.
//! 2. The tree merge of the head onto `T` is clean, a sum of one tree. It writes blobs and trees
//!    and no commit, and moves no reference. A merge that changes nothing is a change that `main`
//!    holds already ([`Verdict::Landed`]).
//! 3. The level of the change: the highest level of the paths that differ between `T` and the
//!    merge, by the policy of `T` and every further policy, raised by the escalation for
//!    weakened tests (ADR-004, rule 10) over the old and the new blobs. A caller that gives its
//!    own level ([`Rules::Level`]) reads no policy and gets no escalation.
//! 4. The rule table of ADR-019 over the results of the checks.
//! 5. The approvals of ADR-004, where a register and a policy are asked: the approvals of agents
//!    that the policy names for the level, and one human at the levels 2 and 3, an owner at
//!    level 3.
//! 6. The bounds of ADR-019: the pause at a red tip with the exemption of a repair, the
//!    unverified window, and the reservation.
//!
//! The function reads the object store and what the caller passes, and nothing else: no log, no
//! record, no reference, and no clock. The holder of the statements passes their fold, the order
//! in which it holds them, the heads of its branches, and its time. It needs no working copy and
//! no lock, so it is callable on a bare repository (ADR-025, requirement 1 of STEMMA-278).

use std::collections::{BTreeMap, BTreeSet};

use super::gate::{Dropped, DroppedReason};
use super::{GateError, LevelError, PolicyError, Repository};
use crate::check::{self, Check};
use crate::evidence::Evidence;
use crate::merge::merge_commits;
use crate::object::{CommitId, Kind, ObjectId, TreeId};
use crate::policy::{Combined, Policy, RiskLevel};
use crate::protocol::refusal::LevelSource;
use crate::protocol::statement::Statement;
use crate::pull::{self, Approval, PullRequest};
use crate::refs::BranchName;
use crate::register::{IdentityKind, Register, Role};
use crate::store::ObjectStore;
use crate::weakened::FileChange;
use crate::Actor;

use super::bounds::{Pause, Subject};
use super::difference::{self, DiffError};
use super::statements::proof_of;

/// Whether the signature of a placed statement counts, by the rule of its holder. A clone asks
/// the register for the number of the entry of its log that places the statement
/// ([`LogProofs`]); a hub asks it for the place of the statement in the order of the hub.
pub(crate) trait Proofs {
    /// Whether the signature of `statement`, placed at `number` as a statement of `actor`,
    /// counts, or why not.
    fn holds(&self, actor: &Actor, number: u64, statement: &Statement) -> Result<(), String>;
}

/// The proofs of a clone: every statement counts in the open mode (`None`), and in an enforcing
/// one if the register accepts its signature for the actor and the number of the entry that
/// places it ([`Register::verify_statement`]).
pub(crate) struct LogProofs<'a>(pub(crate) Option<&'a Register>);

impl Proofs for LogProofs<'_> {
    fn holds(&self, actor: &Actor, number: u64, statement: &Statement) -> Result<(), String> {
        proof_of(self.0, actor, number, statement).map_err(|error| error.to_string())
    }
}

/// Where the level of the change comes from.
#[derive(Clone, Copy)]
pub(crate) enum Rules<'a> {
    /// The caller gives the level. No policy is read: no escalation, no approvals, the bounds
    /// with their defaults, and no reservation (see [`merge_gate`](Repository::merge_gate)).
    Level(RiskLevel),
    /// The level comes from the policy of `T` and from these further policies, combined by the
    /// maximum: the namespace policy of a hub (ADR-027) is one of them. The policy of `T` is read
    /// by the verdict.
    Policies(&'a [Policy]),
}

/// What the verdict judges: the open pull request, the tip, and what its holder knows.
pub(crate) struct Case<'a> {
    /// The tip `T` of `main`.
    pub(crate) tip: CommitId,
    /// The pull request, as the fold of [`evidence`](Self::evidence) has it.
    pub(crate) pull: &'a PullRequest,
    /// The fold of the statements of the holder.
    pub(crate) evidence: &'a Evidence,
    /// The writers of the commits that the pull request brings, for the ban on self-approval
    /// (ADR-004 rule 4). Only read where the approvals are asked.
    pub(crate) writers: &'a [Actor],
    /// The branches of the holder and their commits.
    pub(crate) heads: &'a BTreeMap<BranchName, CommitId>,
    /// The register of `T`, or `None` in the open mode.
    pub(crate) register: Option<&'a Register>,
    /// How the holder judges a signature.
    pub(crate) proofs: &'a dyn Proofs,
    /// Where the level comes from.
    pub(crate) rules: Rules<'a>,
    /// The time of the holder, in seconds since the Unix epoch, by which a reservation ends.
    pub(crate) now: i64,
}

/// A change that may land on `main`.
#[derive(Debug)]
pub(crate) struct Merge {
    /// The tree of the merge of the head onto `T`.
    pub(crate) tree: TreeId,
}

/// What the verdict allows.
#[derive(Debug)]
pub(crate) enum Verdict {
    /// The change may land as the commit with the single parent `T` and this tree.
    Merge(Merge),
    /// The merge changes nothing: `main` holds the change already (the repair path of the local
    /// gate).
    Landed,
}

/// Why the verdict refuses the change. Nothing is written but blobs and trees.
#[derive(Debug)]
pub(crate) enum Refusal {
    /// The branch is not at the head of the opening: at this commit, or gone.
    BranchMoved { head: Option<CommitId> },
    /// The merge of the head onto `T` has a conflict.
    Conflict,
    /// The policy of `T` cannot be used.
    Policy(PolicyError),
    /// The level of the change cannot be told.
    Level(LevelError),
    /// No result of a check counts.
    NotChecked {
        level: RiskLevel,
        source: LevelSource,
    },
    /// Fewer approvals count than the level asks for.
    NotApproved {
        level: RiskLevel,
        source: LevelSource,
        agents: u32,
        agents_needed: u32,
        humans: u32,
        humans_needed: u32,
        dropped: Vec<Dropped>,
    },
    /// A bound holds the change back.
    Paused(Pause),
}

impl Repository {
    /// The verdict about the move of `main` from `case.tip` to the merge of the pull request
    /// (see the module). The outer error is a failure of the store; the inner one refuses.
    pub(crate) fn judge_main(
        &self,
        case: &Case<'_>,
    ) -> Result<Result<Verdict, Refusal>, GateError> {
        let pull = case.pull;
        let head = *pull.head();
        let found = case.heads.get(pull.branch()).copied();
        if found != Some(head) {
            return Ok(Err(Refusal::BranchMoved { head: found }));
        }
        let tip = case.tip;
        let sum = merge_commits(self.objects(), tip, head)?.simplify();
        let [tree] = sum.terms() else {
            return Ok(Err(Refusal::Conflict));
        };
        let tree = *tree;
        let tip_tree = self.objects().get_commit(&tip)?.map(|commit| commit.tree());
        if tip_tree == Some(tree) {
            return Ok(Ok(Verdict::Landed));
        }

        let policies = match case.rules {
            Rules::Level(_) => None,
            Rules::Policies(more) => {
                let base = match self.policy_at(&tip, case.register.is_some()) {
                    Ok(base) => base,
                    Err(error) => return Ok(Err(Refusal::Policy(error))),
                };
                Some(
                    std::iter::once(base)
                        .chain(more.iter().cloned())
                        .collect::<Vec<_>>(),
                )
            }
        };
        let (level, source) = match (&case.rules, &policies) {
            (Rules::Level(level), _) => (*level, LevelSource::Paths),
            (_, Some(policies)) => match self.level_by(policies, &tip, tree) {
                Ok(found) => found,
                Err(error) => return Ok(Err(Refusal::Level(error))),
            },
            (Rules::Policies(_), None) => unreachable!("the policies were read above"),
        };

        if self.counted_in(case, level)?.is_empty() {
            return Ok(Err(Refusal::NotChecked { level, source }));
        }
        if let (Some(register), Some(policies)) = (case.register, &policies) {
            if let Some(refusal) = approvals_missing(case, register, policies, level, source) {
                return Ok(Err(refusal));
            }
        }
        // The bounds come after the rule table, so that a repair is exempt only with a result
        // that counts.
        let subject = Subject {
            pull,
            tip,
            tree,
            level,
        };
        if let Some(pause) = self.pause_of(&subject, case, policies.as_deref())? {
            return Ok(Err(Refusal::Paused(pause)));
        }
        Ok(Ok(Verdict::Merge(Merge { tree })))
    }

    /// The level of the change that turns the tree of `tip` into `result`: the maximum over the
    /// policies of the level of its paths and of the escalation for weakened tests, and whether
    /// the escalation raised it.
    pub(super) fn level_by(
        &self,
        policies: &[Policy],
        tip: &CommitId,
        result: TreeId,
    ) -> Result<(RiskLevel, LevelSource), LevelError> {
        let mut paths = RiskLevel::Zero;
        for policy in policies {
            paths = paths.max(self.level_of_change(policy, tip, result)?);
        }
        let files = self.changed_blobs(tip, result)?;
        let changes: Vec<FileChange<'_>> = files
            .iter()
            .map(|(path, old, new)| FileChange {
                path,
                old: old.as_deref(),
                new: new.as_deref(),
            })
            .collect();
        let weakened = Combined::new(policies).weakening(&changes).level;
        if weakened > paths {
            Ok((weakened, LevelSource::Weakened))
        } else {
            Ok((paths, LevelSource::Paths))
        }
    }

    /// The paths that differ between the tree of `tip` and `result`, each with its old and its
    /// new content; a side that is no blob is `None`.
    #[allow(clippy::type_complexity)]
    fn changed_blobs(
        &self,
        tip: &CommitId,
        result: TreeId,
    ) -> Result<Vec<(String, Option<Vec<u8>>, Option<Vec<u8>>)>, LevelError> {
        let before = self.files_with_modes_of(*tip).map_err(LevelError::Diff)?;
        let after =
            difference::files_with_modes(self.objects(), result).map_err(LevelError::Diff)?;
        let paths: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
        let store = self.objects();
        let content = |id: Option<ObjectId>| -> Result<Option<Vec<u8>>, LevelError> {
            let Some(id) = id else {
                return Ok(None);
            };
            match store.get(&id) {
                Ok(Some(raw)) if raw.kind() == Kind::Blob => Ok(Some(raw.body().to_vec())),
                Ok(Some(_)) => Ok(None),
                Ok(None) => Err(LevelError::Diff(DiffError::Missing { id })),
                Err(error) => Err(LevelError::Diff(DiffError::Store(error))),
            }
        };
        let mut changed = Vec::new();
        for path in paths {
            let (old, new) = (before.get(path), after.get(path));
            if old == new {
                continue;
            }
            changed.push((
                path.clone(),
                content(old.map(|(id, _)| *id))?,
                content(new.map(|(id, _)| *id))?,
            ));
        }
        Ok(changed)
    }

    /// The results that count for the pull request of `case` at `level` (ADR-019): passing runs
    /// of its head with a base that the rule of the level accepts, of a writer that counts at
    /// the level, whose signature the holder accepts.
    fn counted_in(&self, case: &Case<'_>, level: RiskLevel) -> Result<Vec<Check>, GateError> {
        let pull = case.pull;
        let authors = match level {
            RiskLevel::Zero | RiskLevel::One => Vec::new(),
            RiskLevel::Two | RiskLevel::Three => self.change_authors(pull.head(), &case.tip)?,
        };
        Ok(case
            .evidence
            .checks
            .iter()
            .filter(|found| {
                found.commit() == pull.head()
                    && found.passed()
                    && check::applies_at(level, found, &case.tip)
                    && check::counts(level, found, pull.opener(), &authors, case.register)
                    && has_proof(case, found)
            })
            .cloned()
            .collect())
    }
}

/// Whether a check counts as far as the proof of its writer goes (see `has_proof` of the
/// checks): a failing run that stands in for a lost statement counts without one, so that its
/// loss never clears a red tip; every other run counts with a signature that the holder accepts.
pub(super) fn has_proof(case: &Case<'_>, found: &Check) -> bool {
    (found.is_stand_in() && !found.passed())
        || case
            .proofs
            .holds(found.writer(), found.number(), found.statement())
            .is_ok()
}

/// The refusal for approvals that do not suffice at `level` (ADR-004), or `None`. The approvals
/// that count are those of the recount ([`pull::recount`]) with the signatures that the holder
/// accepts. An agent approval is one of a registered agent; the human that the levels 2 and 3
/// ask for is a registered human, an owner at level 3. The policies may only raise the number
/// of agent approvals.
fn approvals_missing(
    case: &Case<'_>,
    register: &Register,
    policies: &[Policy],
    level: RiskLevel,
    source: LevelSource,
) -> Option<Refusal> {
    let proof = |approval: &Approval| {
        case.proofs
            .holds(approval.reviewer(), approval.entry(), approval.statement())
    };
    let recount = pull::recount(
        case.pull,
        case.pull.head(),
        case.writers,
        &case.evidence.approvals,
        &proof,
    );
    let count = |of: &dyn Fn(IdentityKind, Role) -> bool| {
        let found = recount
            .counted()
            .iter()
            .filter(|approval| {
                let identity = crate::register::Identity::of(approval.reviewer());
                register
                    .entry(&identity)
                    .is_some_and(|entry| of(identity.kind(), entry.role()))
            })
            .count();
        u32::try_from(found).unwrap_or(u32::MAX)
    };
    let agents = count(&|kind, _| kind == IdentityKind::Agent);
    let humans = count(&|kind, role| {
        kind == IdentityKind::Human && (level < RiskLevel::Three || role == Role::Owner)
    });
    let agents_needed = Combined::new(policies).agent_approvals(level);
    let humans_needed = u32::from(level >= RiskLevel::Two);
    if agents >= agents_needed && humans >= humans_needed {
        return None;
    }
    let void = recount.void().iter().map(|(approval, why)| Dropped {
        reviewer: approval.reviewer().clone(),
        reason: DroppedReason::Recount(why.clone()),
    });
    // At level 3 the approval of a human counts only for an owner.
    let members = recount.counted().iter().filter_map(|approval| {
        let identity = crate::register::Identity::of(approval.reviewer());
        let member = level == RiskLevel::Three
            && identity.kind() == IdentityKind::Human
            && register
                .entry(&identity)
                .is_some_and(|entry| entry.role() != Role::Owner);
        member.then(|| Dropped {
            reviewer: approval.reviewer().clone(),
            reason: DroppedReason::NotOwner,
        })
    });
    Some(Refusal::NotApproved {
        level,
        source,
        agents,
        agents_needed,
        humans,
        humans_needed,
        dropped: void.chain(members).collect(),
    })
}
