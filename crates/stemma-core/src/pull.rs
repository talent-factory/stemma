//! The pull request as the state that its entries add up to (ADR-004, ADR-011, STEMMA-238).
//!
//! A pull request has no object and no reference. It is the sequence of the entries `pr-open`,
//! `pr-close`, and `pr-merge` in the operation log ([`PullEntry`](crate::oplog::PullEntry)), named by the number of its
//! `pr-open` entry. The state is derived: [`pull_requests`] folds the log and returns one
//! [`PullRequest`] for every opening that is still in the log.
//!
//! ```text
//!            pr-open ──► open ──┬─ pr-close ──► closed
//!                               └─ pr-merge ──► merged
//! ```
//!
//! `closed` and `merged` are final. Whether an open pull request is *approved* is no state of the
//! log: it follows from the approvals on the current head (ADR-004 rule 4). An approval is an entry
//! `pr-approve` ([`approvals`] lists them), but it changes no state of the fold; the recount that
//! says which approvals count is [`Repository::counted_approvals`](crate::repository::Repository).
//!
//! The fold is tolerant, as the log may hold stubs after a purge: an entry about a pull request
//! whose opening is a stub is skipped, and so is an entry that follows a closing or a merge. The
//! rules that keep such entries from being written are those of
//! [`Repository`](crate::repository::Repository).

use crate::allowed::{self_approval, SelfApproval};
use crate::object::CommitId;
use crate::oplog::{Entry, PullKind};
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
        /// The number of the entry of the closing.
        entry: u64,
        /// Who closed it.
        by: Actor,
        /// When, in seconds since the Unix epoch.
        time: i64,
    },
    /// Merged.
    Merged {
        /// The number of the entry of the merge.
        entry: u64,
        /// Who merged it.
        by: Actor,
        /// When, in seconds since the Unix epoch.
        time: i64,
        /// The head that was merged.
        commit: CommitId,
    },
}

/// A pull request, as far as the log tells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    number: u64,
    branch: BranchName,
    opener: Actor,
    opened: i64,
    head: CommitId,
    reviewers: Vec<Actor>,
    repairs: Option<CommitId>,
    state: PullState,
}

impl PullRequest {
    /// The number: the number of the entry `pr-open`.
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

    /// When it was opened, in seconds since the Unix epoch.
    pub fn opened(&self) -> i64 {
        self.opened
    }

    /// The commit at the tip of the branch when the pull request was opened.
    pub fn head(&self) -> &CommitId {
        &self.head
    }

    /// The reviewers that the opener named, in ascending order.
    pub fn reviewers(&self) -> &[Actor] {
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

/// The pull requests that `entries` tell of, in the order of their openings. `entries` are the
/// entries of the log in ascending order of their numbers.
pub fn pull_requests(entries: &[Entry]) -> Vec<PullRequest> {
    let mut pulls: Vec<PullRequest> = Vec::new();
    for entry in entries {
        let Some(pull) = entry.pull() else { continue };
        match pull.kind() {
            PullKind::Open => {
                let (Some(branch), Some(head)) = (pull.branch(), pull.commit()) else {
                    unreachable!("an opening has a branch and a commit: PullEntry::build");
                };
                pulls.push(PullRequest {
                    number: entry.number(),
                    branch: branch.clone(),
                    opener: entry.actor().clone(),
                    opened: entry.time(),
                    head: *head,
                    reviewers: pull.reviewers().to_vec(),
                    repairs: pull.repairs().copied(),
                    state: PullState::Open,
                });
            }
            // An approval changes no state: the recount reads it (`approvals`).
            PullKind::Approve => {}
            PullKind::Close | PullKind::Merge => {
                let Some(target) = pull
                    .pull()
                    .and_then(|number| pulls.iter_mut().find(|p| p.number == number))
                else {
                    continue;
                };
                if !target.is_open() {
                    continue;
                }
                target.state = match (pull.kind(), pull.commit()) {
                    (PullKind::Merge, Some(commit)) => PullState::Merged {
                        entry: entry.number(),
                        by: entry.actor().clone(),
                        time: entry.time(),
                        commit: *commit,
                    },
                    (PullKind::Close, None) => PullState::Closed {
                        entry: entry.number(),
                        by: entry.actor().clone(),
                        time: entry.time(),
                    },
                    _ => {
                        unreachable!("a merge has a commit and a close has none: PullEntry::build")
                    }
                };
            }
        }
    }
    pulls
}

/// An approval as the log holds it: the entry `pr-approve`, before the recount decides whether it
/// counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    entry: u64,
    time: i64,
    reviewer: Actor,
    pull: u64,
    head: CommitId,
}

impl Approval {
    /// The number of the entry. The proof of the approval is the file with this number.
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// When the writer claims to have approved, in seconds since the Unix epoch.
    pub fn time(&self) -> i64 {
        self.time
    }

    /// Who approved.
    pub fn reviewer(&self) -> &Actor {
        &self.reviewer
    }

    /// The number of the pull request.
    pub fn pull(&self) -> u64 {
        self.pull
    }

    /// The head that the reviewer names: the only commit for which the approval can count.
    pub fn head(&self) -> &CommitId {
        &self.head
    }
}

/// The approvals in `entries`, in the order of their entries. An entry that is a stub is no
/// approval any more, and an approval of a pull request that the log does not know is listed all
/// the same: the recount of a pull request picks its own by number.
pub fn approvals(entries: &[Entry]) -> Vec<Approval> {
    entries
        .iter()
        .filter_map(|entry| {
            let pull = entry
                .pull()
                .filter(|pull| pull.kind() == PullKind::Approve)?;
            Some(Approval {
                entry: entry.number(),
                time: entry.time(),
                reviewer: entry.actor().clone(),
                pull: pull.pull()?,
                head: *pull.commit()?,
            })
        })
        .collect()
}

/// The bytes that the reviewer signs for an approval, as
/// [`PayloadKind::Approval`](crate::signature::PayloadKind::Approval): the number of the pull
/// request and the head that was reviewed. The head binds the approval: a commit on the branch
/// makes another head, and the signature does not verify for it (ADR-004 rule 4). The number keeps
/// it from standing for another pull request of the same head.
pub fn approval_payload(pull: u64, head: &CommitId) -> Vec<u8> {
    format!("pull {pull}\ncommit {head}\n").into_bytes()
}

/// Why an approval of the log does not count.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NotCounted {
    /// The entry stands before the opening of the pull request, which no writer produces.
    BeforeOpening,
    /// The reviewer named another head than the current one: a commit came after the review.
    OtherHead,
    /// The reviewer opened the pull request or wrote one of its commits.
    SelfApproval(SelfApproval),
    /// The proof is missing, or the register does not accept it. Carries the reason.
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
    /// The approvals that count, one for each identity, in the order of their entries.
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
/// function over what the log says. `authors` wrote the commits that the pull request brings.
/// `proof` checks the signature of one approval against the register; it is the only part that
/// reads anything else. An approval counts if it names `head`, its reviewer is neither the opener
/// nor an author, its proof holds, and its identity has no approval that counts already.
///
/// The writer of an approval asks the same questions before it writes the entry, but an entry can
/// come from another hand, or the head can move after the entry; the recount does not trust the
/// writer.
pub fn recount(
    pull: &PullRequest,
    head: &CommitId,
    authors: &[Actor],
    approvals: &[Approval],
    proof: &dyn Fn(&Approval) -> Result<(), String>,
) -> Recount {
    let mut counted: Vec<Approval> = Vec::new();
    let mut void = Vec::new();
    for approval in approvals.iter().filter(|a| a.pull() == pull.number()) {
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
    use crate::oplog::{Chain, PullEntry, Record};

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

    fn entry(number: u64, pull: PullEntry) -> Entry {
        Entry::linked(number, Chain::Start, Record::Pull(pull)).unwrap()
    }

    fn open(number: u64, name: &str) -> Entry {
        entry(
            number,
            PullEntry::open(10, mia(), branch(name), commit(A), vec![ana()], None).unwrap(),
        )
    }

    #[test]
    fn an_opening_makes_an_open_pull_request_named_by_its_number() {
        let pulls = pull_requests(&[open(4, "feature/x")]);
        let [pull] = pulls.as_slice() else {
            panic!("{pulls:?}")
        };
        assert_eq!(pull.number(), 4);
        assert_eq!(pull.branch(), &branch("feature/x"));
        assert_eq!(pull.opener(), &mia());
        assert_eq!(pull.opened(), 10);
        assert_eq!(pull.head(), &commit(A));
        assert_eq!(pull.reviewers(), [ana()]);
        assert!(pull.is_open());
        assert_eq!(open_for(&pulls, &branch("feature/x")), Some(pull));
        assert_eq!(open_for(&pulls, &branch("other")), None);
    }

    #[test]
    fn a_closing_and_a_merge_end_the_pull_request_and_the_end_is_final() {
        let log = [
            open(1, "a"),
            open(2, "b"),
            entry(3, PullEntry::close(20, ana(), 1).unwrap()),
            entry(4, PullEntry::merge(30, mia(), 2, commit(B)).unwrap()),
            // Whatever follows a closing or a merge is skipped.
            entry(5, PullEntry::merge(40, mia(), 1, commit(B)).unwrap()),
            entry(6, PullEntry::close(50, mia(), 2).unwrap()),
        ];
        let pulls = pull_requests(&log);
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
    fn a_branch_can_have_a_new_pull_request_after_the_end_of_the_first() {
        let log = [
            open(1, "a"),
            entry(2, PullEntry::close(20, mia(), 1).unwrap()),
            open(3, "a"),
        ];
        let pulls = pull_requests(&log);
        assert_eq!(pulls.len(), 2);
        assert_eq!(open_for(&pulls, &branch("a")).unwrap().number(), 3);
    }

    #[test]
    fn an_entry_about_a_pull_request_that_the_log_does_not_know_is_skipped() {
        let log = [
            entry(5, PullEntry::close(20, mia(), 2).unwrap()),
            open(6, "a"),
            entry(7, PullEntry::merge(30, mia(), 99, commit(B)).unwrap()),
        ];
        let pulls = pull_requests(&log);
        assert_eq!(pulls.len(), 1);
        assert!(pulls[0].is_open());
    }

    #[test]
    fn a_stub_in_place_of_an_opening_removes_the_pull_request() {
        let stub = open(1, "a").to_stub().unwrap();
        let log = [stub, entry(2, PullEntry::close(20, mia(), 1).unwrap())];
        assert!(pull_requests(&log).is_empty());
    }
}
