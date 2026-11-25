//! The pull request as the state that its statements add up to (ADR-004, ADR-025).
//!
//! A pull request has no object and no reference. It is a signed statement, its opening
//! ([`Opening`]), and what follows it: a closing, which is
//! a statement too, or a merge, which is a fact that the holder of the statements knows
//! ([`Merge`](crate::evidence::Merge)). The hash of the opening names the pull request
//! ([`PullRequest::id`]), in every clone and on a hub. A clone has a second, local name for it:
//! the number of the entry `pr-open` of its operation log that made the opening
//! ([`PullRequest::number`]), which the commands show.
//! The state is derived: [`fold`](crate::evidence::fold) reads the statements and returns one
//! [`PullRequest`] for every opening.
//!
//! ```text
//!            opening ──► open ──┬─ closing ──► closed
//!                               └─ merge ────► merged
//! ```
//!
//! `closed` and `merged` are final. Whether an open pull request is *approved* is no state: it
//! follows from the approvals on the current head (ADR-004 rule 4). An approval is a statement
//! as well ([`Approval`]), but it changes no state of the fold; the recount that says which
//! approvals count is [`recount`], which
//! [`Repository::recount_approvals`](crate::repository::Repository::recount_approvals) calls.
//!
//! The fold is tolerant (see [`crate::evidence`]). The rules that keep what it skips from being
//! written are those of [`Repository`](crate::repository::Repository).

use crate::allowed::{self_approval, SelfApproval};
use crate::evidence::Placed;
use crate::object::CommitId;
use crate::protocol::statement::{self, Opening, Statement, StatementHash};
use crate::refs::BranchName;
use crate::register::Identity;
use crate::Actor;

/// The branch that every pull request targets: the protected trunk (ADR-004 rule 3, ADR-006).
pub const TARGET: &str = "main";

/// Where a pull request stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullState {
    /// Opened, and neither closed nor merged.
    Open,
    /// Ended without a merge.
    Closed {
        /// The number of the closing in the order of the holder: in a clone the number of the
        /// entry `pr-close`.
        entry: u64,
        /// Who closed it.
        by: Actor,
        /// When, in seconds since the Unix epoch.
        time: i64,
    },
    /// Merged.
    Merged {
        /// The number of the merge in the order of the holder: in a clone the number of the
        /// entry `pr-merge`.
        entry: u64,
        /// Who merged it.
        by: Actor,
        /// When, in seconds since the Unix epoch.
        time: i64,
        /// The head that was merged.
        commit: CommitId,
    },
}

/// A pull request, as far as the statements tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    id: StatementHash,
    number: u64,
    branch: BranchName,
    opener: Actor,
    opened: i64,
    head: CommitId,
    reviewers: Vec<Identity>,
    repairs: Option<CommitId>,
    state: PullState,
}

impl PullRequest {
    /// The open pull request that a placed opening makes.
    pub(crate) fn from_opening(placed: &Placed, opening: &Opening) -> Self {
        Self {
            id: placed.hash(),
            number: placed.number(),
            branch: opening.branch().clone(),
            opener: placed.actor().clone(),
            opened: placed.time(),
            head: opening.head(),
            reviewers: opening.reviewers().to_vec(),
            repairs: opening.repairs(),
            state: PullState::Open,
        }
    }

    /// Ends the pull request.
    pub(crate) fn end(&mut self, state: PullState) {
        self.state = state;
    }

    /// The name of the pull request: the hash of its signed opening (ADR-025). It holds in
    /// every clone and on a hub.
    pub fn id(&self) -> StatementHash {
        self.id
    }

    /// The number of the opening in the order of whoever holds the statements. In a clone it is
    /// the number of the entry `pr-open`, the name of the pull request in the commands; another
    /// clone has another number for the same pull request.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// The branch that the pull request would merge into [`TARGET`].
    pub fn branch(&self) -> &BranchName {
        &self.branch
    }

    /// The write owner who opened it.
    pub fn opener(&self) -> &Actor {
        &self.opener
    }

    /// When it was opened, as the holder of the statements says, in seconds since the Unix epoch.
    pub fn opened(&self) -> i64 {
        self.opened
    }

    /// The commit at the tip of the branch when the pull request was opened.
    pub fn head(&self) -> &CommitId {
        &self.head
    }

    /// The reviewers that the opener named, in the ascending order of their text.
    pub fn reviewers(&self) -> &[Identity] {
        &self.reviewers
    }

    /// The red commit that the opener declared to repair (ADR-019).
    pub fn repairs(&self) -> Option<&CommitId> {
        self.repairs.as_ref()
    }

    /// The state.
    pub fn state(&self) -> &PullState {
        &self.state
    }

    /// Whether the pull request is open.
    pub fn is_open(&self) -> bool {
        self.state == PullState::Open
    }
}

/// An approval as a statement tells it, before the recount decides whether it counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    entry: u64,
    time: i64,
    reviewer: Actor,
    pull: StatementHash,
    head: CommitId,
    statement: Statement,
}

impl Approval {
    /// The approval that a placed statement makes.
    pub(crate) fn placed(placed: &Placed, approval: &statement::Approval) -> Self {
        Self {
            entry: placed.number(),
            time: placed.time(),
            reviewer: placed.actor().clone(),
            pull: approval.pull(),
            head: approval.head(),
            statement: placed.statement().clone(),
        }
    }

    /// The number of the approval in the order of the holder: in a clone the number of the
    /// entry `pr-approve`. The register counts a retired key by it (ADR-003).
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// When the holder says the approval was made, in seconds since the Unix epoch.
    pub fn time(&self) -> i64 {
        self.time
    }

    /// Who approved.
    pub fn reviewer(&self) -> &Actor {
        &self.reviewer
    }

    /// The pull request: the hash of its opening.
    pub fn pull(&self) -> StatementHash {
        self.pull
    }

    /// The head that the reviewer names: the only commit for which the approval can count.
    pub fn head(&self) -> &CommitId {
        &self.head
    }

    /// The statement of the approval, whose signature is its proof.
    pub fn statement(&self) -> &Statement {
        &self.statement
    }
}

/// The bytes that a reviewer signed for an approval before the signed statements (the
/// repository formats 10 to 15), as
/// [`PayloadKind::Approval`](crate::signature::PayloadKind::Approval): the number of the pull
/// request in one log and the head that was reviewed. Nothing signs them any more: an approval
/// is the statement [`Approval`](crate::protocol::statement::Approval), which names the pull
/// request by the hash of its opening. They are kept for the proof files of old entries, which
/// `op verify` still checks (ADR-011).
pub fn approval_payload(pull: u64, head: &CommitId) -> Vec<u8> {
    format!("pull {pull}\ncommit {head}\n").into_bytes()
}

/// Why an approval does not count.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NotCounted {
    /// The approval stands before the opening of the pull request in the order of the holder,
    /// which no writer produces.
    BeforeOpening,
    /// The reviewer named another head than the current one: a commit came after the review.
    OtherHead,
    /// The reviewer opened the pull request or wrote one of its commits.
    SelfApproval(SelfApproval),
    /// The statement is not signed, or the register does not accept its signature for the
    /// reviewer. Carries the reason.
    Proof(String),
    /// The same identity approved this head before.
    Repeat,
}

/// The approvals of one pull request as they count for its current head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recount {
    counted: Vec<Approval>,
    void: Vec<(Approval, NotCounted)>,
}

impl Recount {
    /// The approvals that count, one for each identity, in their order.
    pub fn counted(&self) -> &[Approval] {
        &self.counted
    }

    /// The approvals of the pull request that do not count, with the reason.
    pub fn void(&self) -> &[(Approval, NotCounted)] {
        &self.void
    }

    /// How many approvals count.
    pub fn count(&self) -> usize {
        self.counted.len()
    }
}

/// Which approvals of `pull` count for `head`, the head of the branch now (ADR-004 rule 4): a pure
/// function over what the statements say. `approvals` are those of the fold; the ones that name
/// another pull request are passed over. `authors` wrote the commits that the pull request
/// brings. `proof` checks the signature of one approval against the register; it is the only
/// part that reads anything else. An approval counts if it stands after the opening in the order of the
/// holder, names `head`, its reviewer is neither the opener nor an author, its proof holds, and
/// its identity has no approval that counts already.
///
/// The writer of an approval asks the same questions before it writes the statement, but a
/// statement can come from another hand, or the head can move after it; the recount does not
/// trust the writer.
pub fn recount(
    pull: &PullRequest,
    head: &CommitId,
    authors: &[Actor],
    approvals: &[Approval],
    proof: &dyn Fn(&Approval) -> Result<(), String>,
) -> Recount {
    let mut counted: Vec<Approval> = Vec::new();
    let mut void = Vec::new();
    for approval in approvals.iter().filter(|a| a.pull() == pull.id()) {
        let verdict = if approval.entry() <= pull.number() {
            Some(NotCounted::BeforeOpening)
        } else if approval.head() != head {
            Some(NotCounted::OtherHead)
        } else if let Some(reason) = self_approval(approval.reviewer(), pull.opener(), authors) {
            Some(NotCounted::SelfApproval(reason))
        } else if let Err(reason) = proof(approval) {
            Some(NotCounted::Proof(reason))
        } else if counted
            .iter()
            .any(|earlier| Identity::of(earlier.reviewer()) == Identity::of(approval.reviewer()))
        {
            Some(NotCounted::Repeat)
        } else {
            None
        };
        match verdict {
            Some(reason) => void.push((approval.clone(), reason)),
            None => counted.push(approval.clone()),
        }
    }
    Recount { counted, void }
}

/// The open pull request of `branch`, if it has one. `Repository::open_pull` lets at most one
/// exist (ADR-004); of two in a log that was edited by hand, the first is returned.
pub fn open_for<'a>(pulls: &'a [PullRequest], branch: &BranchName) -> Option<&'a PullRequest> {
    pulls
        .iter()
        .find(|pull| pull.is_open() && &pull.branch == branch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{fold, Merge};
    use crate::protocol::statement::{Body, Closing};

    const A: &str = "b3:1d719c9e1d926eb0f33467406bf50911eca1d59f976105abd3eb5ccfaabdeed9";
    const B: &str = "b3:4feb2d73a2c706e33c2a71f6f03cbfb48f0a20b48567936fca293159e7e3b32c";

    fn commit(text: &str) -> CommitId {
        CommitId::new(text.parse().unwrap())
    }

    fn mia() -> Actor {
        Actor::human("mia").unwrap()
    }

    fn ana() -> Actor {
        Actor::human("ana").unwrap()
    }

    fn branch(name: &str) -> BranchName {
        BranchName::try_new(name).unwrap()
    }

    /// The opening of the open mode for a branch, with Ana as its reviewer.
    fn opening(name: &str) -> Statement {
        Statement::unsigned(Body::Opening(
            Opening::try_new(
                None,
                branch(name),
                commit(A),
                vec![Identity::of(&ana())],
                None,
            )
            .unwrap(),
        ))
    }

    /// The opening of `name`, placed at `number` as a statement of Mia.
    fn open(number: u64, name: &str) -> Placed {
        Placed::new(number, 10, mia(), opening(name))
    }

    fn close(number: u64, time: i64, by: Actor, name: &str) -> Placed {
        let closing = Closing::new(None, opening(name).hash());
        Placed::new(
            number,
            time,
            by,
            Statement::unsigned(Body::Closing(closing)),
        )
    }

    fn merge(number: u64, time: i64, name: &str) -> Merge {
        Merge {
            pull: opening(name).hash(),
            number,
            time,
            by: mia(),
            commit: commit(B),
        }
    }

    #[test]
    fn an_opening_makes_an_open_pull_request_named_by_its_hash_and_numbered_by_its_place() {
        let pulls = fold(&[open(4, "feature/x")], &[]).pulls;
        let [pull] = pulls.as_slice() else {
            panic!("{pulls:?}")
        };
        assert_eq!(pull.id(), opening("feature/x").hash());
        assert_eq!(pull.number(), 4);
        assert_eq!(pull.branch(), &branch("feature/x"));
        assert_eq!(pull.opener(), &mia());
        assert_eq!(pull.opened(), 10);
        assert_eq!(pull.head(), &commit(A));
        assert_eq!(pull.reviewers(), [Identity::of(&ana())]);
        assert_eq!(pull.repairs(), None);
        assert!(pull.is_open());
        assert_eq!(open_for(&pulls, &branch("feature/x")), Some(pull));
        assert_eq!(open_for(&pulls, &branch("other")), None);
    }

    #[test]
    fn a_closing_and_a_merge_end_the_pull_request_and_the_end_is_final() {
        let placed = [
            open(1, "a"),
            open(2, "b"),
            close(3, 20, ana(), "a"),
            // Whatever follows a closing or a merge is skipped.
            close(6, 50, mia(), "b"),
        ];
        let merges = [merge(4, 30, "b"), merge(5, 40, "a")];
        let pulls = fold(&placed, &merges).pulls;
        assert_eq!(
            pulls[0].state(),
            &PullState::Closed {
                entry: 3,
                by: ana(),
                time: 20
            }
        );
        assert_eq!(
            pulls[1].state(),
            &PullState::Merged {
                entry: 4,
                by: mia(),
                time: 30,
                commit: commit(B)
            }
        );
        assert!(open_for(&pulls, &branch("a")).is_none());
    }

    #[test]
    fn the_same_opening_is_not_accepted_twice_and_a_new_one_is_a_new_pull_request() {
        // The same opening placed again, after its end and by another actor, makes nothing.
        let again = Placed::new(3, 30, ana(), opening("a"));
        let placed = [open(1, "a"), close(2, 20, mia(), "a"), again];
        let pulls = fold(&placed, &[]).pulls;
        assert_eq!(pulls.len(), 1);
        assert!(!pulls[0].is_open());
        // Another head is another opening, and so the next pull request of the branch.
        let next = Statement::unsigned(Body::Opening(
            Opening::try_new(None, branch("a"), commit(B), Vec::new(), None).unwrap(),
        ));
        let placed = [
            open(1, "a"),
            close(2, 20, mia(), "a"),
            Placed::new(3, 30, mia(), next.clone()),
        ];
        let pulls = fold(&placed, &[]).pulls;
        assert_eq!(pulls.len(), 2);
        let open = open_for(&pulls, &branch("a")).unwrap();
        assert_eq!((open.number(), open.id()), (3, next.hash()));
    }

    #[test]
    fn what_is_about_a_pull_request_that_the_fold_does_not_know_is_skipped() {
        let placed = [close(5, 20, mia(), "gone"), open(6, "a")];
        let pulls = fold(&placed, &[merge(7, 30, "gone")]).pulls;
        assert_eq!(pulls.len(), 1);
        assert!(pulls[0].is_open());
        // A closing that stands before its opening ends nothing either.
        let pulls = fold(&[close(1, 5, mia(), "a"), open(2, "a")], &[]).pulls;
        assert!(pulls[0].is_open());
    }

    #[test]
    fn an_opening_whose_opener_is_one_of_its_reviewers_makes_no_pull_request() {
        // The statement names no opener, so its reader cannot refuse it; the fold does.
        let pulls = fold(&[Placed::new(1, 10, ana(), opening("a"))], &[]).pulls;
        assert!(pulls.is_empty());
    }

    #[test]
    fn an_owner_override_is_no_evidence_about_a_pull_request() {
        let taken = statement::OwnerOverride::try_new(
            None,
            branch("a"),
            Identity::of(&ana()),
            Some(commit(A)),
            "the holder is away",
        )
        .unwrap();
        let placed = Placed::new(1, 10, mia(), Statement::unsigned(Body::Override(taken)));
        assert_eq!(fold(&[placed], &[]), crate::evidence::Evidence::default());
    }

    #[test]
    fn a_statement_and_a_merge_are_folded_in_the_order_of_their_numbers() {
        // The closing has the smaller number, so the merge finds an ended pull request.
        let pulls = fold(
            &[open(1, "a"), close(2, 20, mia(), "a")],
            &[merge(3, 30, "a")],
        )
        .pulls;
        assert!(matches!(
            pulls[0].state(),
            PullState::Closed { entry: 2, .. }
        ));
        let pulls = fold(
            &[open(1, "a"), close(3, 30, mia(), "a")],
            &[merge(2, 20, "a")],
        )
        .pulls;
        assert!(matches!(
            pulls[0].state(),
            PullState::Merged { entry: 2, .. }
        ));
    }

    fn approve(entry: u64, reviewer: Actor, pull: &str, head: &str) -> Placed {
        let approval = statement::Approval::new(None, opening(pull).hash(), commit(head));
        Placed::new(
            entry,
            1,
            reviewer,
            Statement::unsigned(Body::Approval(approval)),
        )
    }

    #[test]
    fn the_recount_filters_by_pull_head_author_proof_and_identity_in_this_order() {
        let zoe = Actor::human("zoe").unwrap();
        let bad_proof = |approval: &Approval| {
            if approval.reviewer() == &zoe {
                Err("no proof".to_owned())
            } else {
                Ok(())
            }
        };
        let placed = [
            // Before the opening: no writer produces it.
            approve(3, ana(), "a", A),
            open(4, "a"),
            // Another pull request: listed by the fold, passed over by the recount.
            approve(5, ana(), "other", A),
            // Another head, then the opener as reviewer, then a missing proof.
            approve(6, ana(), "a", B),
            approve(7, mia(), "a", A),
            approve(8, zoe.clone(), "a", A),
            // The first of Ana's that is valid counts, the second is a repeat.
            approve(9, ana(), "a", A),
            approve(10, ana(), "a", A),
        ];
        let evidence = fold(&placed, &[]);
        assert_eq!(evidence.approvals.len(), 7);
        assert_eq!(evidence.approvals[0].pull(), opening("a").hash());
        assert_eq!(evidence.approvals[0].time(), 1);
        let recount = recount(
            &evidence.pulls[0],
            &commit(A),
            &[],
            &evidence.approvals,
            &bad_proof,
        );
        let counted: Vec<u64> = recount.counted().iter().map(Approval::entry).collect();
        assert_eq!(counted, [9]);
        let void: Vec<(u64, NotCounted)> = recount
            .void()
            .iter()
            .map(|(approval, why)| (approval.entry(), why.clone()))
            .collect();
        assert_eq!(
            void,
            [
                (3, NotCounted::BeforeOpening),
                (6, NotCounted::OtherHead),
                (7, NotCounted::SelfApproval(SelfApproval::Opener)),
                (8, NotCounted::Proof("no proof".to_owned())),
                (10, NotCounted::Repeat),
            ]
        );
    }

    #[test]
    fn a_void_approval_does_not_block_a_later_valid_one_of_the_same_identity() {
        let placed = [
            open(4, "a"),
            approve(5, ana(), "a", B),
            approve(6, ana(), "a", A),
        ];
        let evidence = fold(&placed, &[]);
        let recount = recount(
            &evidence.pulls[0],
            &commit(A),
            &[],
            &evidence.approvals,
            &|_| Ok(()),
        );
        assert_eq!(recount.count(), 1);
        assert_eq!(recount.counted()[0].entry(), 6);
    }

    #[test]
    fn the_payload_of_an_approval_before_the_statements_has_this_text() {
        assert_eq!(
            approval_payload(7, &commit(A)),
            format!("pull 7\ncommit {A}\n").into_bytes()
        );
    }
}
