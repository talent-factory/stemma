//! The fold of the signed statements (ADR-025): what a set of statements adds up to.
//!
//! The evidence about a pull request is signed statements
//! ([`Statement`]): its opening, the approvals of its head, the results of the checks, and its
//! closing. A clone and a hub hold a set of them, and both read it with [`fold`], the one
//! function of this module. It returns the pull requests with their state, the approvals, and
//! the checks, before any rule says which of them count; those rules are [`crate::pull::recount`]
//! and the functions of [`crate::check`].
//!
//! # A statement is placed by whoever holds it
//!
//! A statement names no number, no time, and no actor. The holder of the set adds them, and a
//! statement with them is [`Placed`]:
//!
//! - In a clone the entry of the operation log that names the hash of the statement places it
//!   (ADR-011): the number and the time are those of the entry, and the actor is its actor. In
//!   an enforcing repository the signature of the statement must be one of a key of that actor,
//!   which the rules that count the evidence ask against the register; in the open mode the
//!   actor is a claim, as the whole entry is (ADR-003).
//! - On a hub the lines that the hub adds to a statement it keeps place it: its running number,
//!   its time, and the actor that the register names for the key.
//!
//! "Merged" is no statement (ADR-025). It is a fact that the holder knows from elsewhere, a
//! [`Merge`]: the entry `pr-merge` of the log in a clone, and on a hub the entry of the record
//! that moved `main` with the pull request as its evidence.
//!
//! # The fold is tolerant
//!
//! A set can hold more than the writers of this crate would write: a log can be edited by hand,
//! and a purge leaves a closing whose opening is gone (ADR-014). The fold skips what does not
//! fit and reports nothing: a closing, an approval, or a merge of a pull request that it does
//! not know, whatever follows the end of a pull request, an opening that is placed a second
//! time (the same opening is not accepted twice), and an opening whose opener is one of its
//! reviewers. An approval and a check are listed as they are; whether they count is not a
//! question of the fold.

use crate::check::Check;
use crate::object::CommitId;
use crate::protocol::statement::{Body, Statement, StatementHash};
use crate::pull::{Approval, PullRequest, PullState};
use crate::register::Identity;
use crate::Actor;

/// A statement in the order of whoever holds it: with a number, a time, and the actor that it
/// is attributed to (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    number: u64,
    time: i64,
    actor: Actor,
    hash: StatementHash,
    statement: Statement,
}

impl Placed {
    /// Places `statement` at `number`, at `time`, as a statement of `actor`.
    pub fn new(number: u64, time: i64, actor: Actor, statement: Statement) -> Self {
        Self {
            number,
            time,
            actor,
            hash: statement.hash(),
            statement,
        }
    }

    /// The number in the order of the holder: the number of the entry of the log in a clone.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// The time that the holder names, in seconds since the Unix epoch.
    pub fn time(&self) -> i64 {
        self.time
    }

    /// The actor that the statement is attributed to.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// The hash of the statement.
    pub fn hash(&self) -> StatementHash {
        self.hash
    }

    /// The statement.
    pub fn statement(&self) -> &Statement {
        &self.statement
    }
}

/// The fact that a pull request was merged, which no statement says (ADR-025).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    /// The pull request: the hash of its opening.
    pub pull: StatementHash,
    /// The number in the order of the holder, the same order as [`Placed::number`].
    pub number: u64,
    /// When, in seconds since the Unix epoch.
    pub time: i64,
    /// Who merged it.
    pub by: Actor,
    /// The head that was merged.
    pub commit: CommitId,
}

/// What a set of statements adds up to (see [`fold`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Evidence {
    /// The pull requests, in the order of their openings.
    pub pulls: Vec<PullRequest>,
    /// The approvals, in their order, before the recount says which of them count.
    pub approvals: Vec<Approval>,
    /// The results of the checks, in their order, before a rule says which of them count.
    pub checks: Vec<Check>,
}

/// One step of the fold: a placed statement or a merge, in the order of their numbers.
enum Step<'a> {
    Placed(&'a Placed),
    Merge(&'a Merge),
}

impl Step<'_> {
    fn number(&self) -> u64 {
        match self {
            Self::Placed(placed) => placed.number,
            Self::Merge(merge) => merge.number,
        }
    }
}

/// Folds the statements of a set, and the merges that the holder knows, into the pull requests,
/// the approvals, and the checks (ADR-025). `placed` and `merges` are each in the ascending
/// order of their numbers, and the two share that order. A clone and a hub call this one
/// function, so they read the same set alike; see the module documentation for what it skips.
pub fn fold(placed: &[Placed], merges: &[Merge]) -> Evidence {
    let mut steps: Vec<Step<'_>> = placed
        .iter()
        .map(Step::Placed)
        .chain(merges.iter().map(Step::Merge))
        .collect();
    // The sort is stable: of a statement and a merge with one number the statement comes first.
    steps.sort_by_key(Step::number);
    let mut evidence = Evidence::default();
    for step in steps {
        match step {
            Step::Merge(merge) => {
                if let Some(pull) = open_pull(&mut evidence.pulls, merge.pull) {
                    pull.end(PullState::Merged {
                        entry: merge.number,
                        by: merge.by.clone(),
                        time: merge.time,
                        commit: merge.commit,
                    });
                }
            }
            Step::Placed(placed) => match placed.statement.body() {
                Body::Opening(opening) => {
                    let again = evidence.pulls.iter().any(|pull| pull.id() == placed.hash);
                    let reviews_itself = opening.reviewers().contains(&Identity::of(&placed.actor));
                    if !again && !reviews_itself {
                        evidence
                            .pulls
                            .push(PullRequest::from_opening(placed, opening));
                    }
                }
                Body::Closing(closing) => {
                    if let Some(pull) = open_pull(&mut evidence.pulls, closing.pull()) {
                        pull.end(PullState::Closed {
                            entry: placed.number,
                            by: placed.actor.clone(),
                            time: placed.time,
                        });
                    }
                }
                Body::Approval(approval) => {
                    evidence.approvals.push(Approval::placed(placed, approval));
                }
                Body::Check(checked) => evidence.checks.push(Check::placed(placed, checked)),
                // An override is evidence for a move of a branch on a hub, not for a pull
                // request.
                Body::Override(_) => {}
            },
        }
    }
    evidence
}

/// The pull request with this name, if the fold knows it and it is open.
fn open_pull(pulls: &mut [PullRequest], id: StatementHash) -> Option<&mut PullRequest> {
    pulls
        .iter_mut()
        .find(|pull| pull.id() == id)
        .filter(|pull| pull.is_open())
}
