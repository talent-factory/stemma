//! The check as evidence about a pair of commits (ADR-019, STEMMA-240).
//!
//! A check is a statement about one commit: its tree passed or failed a set of tests. The entry
//! `check` of the operation log ([`CheckEntry`](crate::oplog::CheckEntry)) names the commit that
//! was tested and the tip of `main` it was merged onto for the test, its *base*, one entry per
//! pair (ADR-011). A run on the tip of `main` itself, after a merge, has no base.
//!
//! This module reads the evidence and decides nothing about a merge. It answers three questions
//! that the gate asks:
//!
//! - Which results exist for a pair of commits ([`for_pair`])?
//! - What is the *last green tip* (ADR-019): the newest commit of `main` that a passing run on
//!   a tip covered ([`last_green_tip`])?
//! - Does a result count for a change of a risk level ([`counts`])? For the levels 0 and 1 every
//!   registered actor counts. For the levels 2 and 3 the writer must be independent of the change
//!   ([`is_independent`]), and the result is valid only for the tip it was run against
//!   ([`applies_at`]). Whether a result for the level 0 and 1 is still enough after `main` moved
//!   is a question of a clean merge, which the gate computes at the moment of the merge.
//!
//! The functions are pure over the entries, the register, and the authors of the change.
//! [`Repository::counted_checks`](crate::repository::Repository::counted_checks) finds the
//! authors of the commits `tip..commit`, checks the proof of the signature, and applies them.

use crate::object::CommitId;
use crate::oplog::{CheckResult, Entry, EntryHash};
use crate::policy::RiskLevel;
use crate::register::{Identity, Register};
use crate::{Actor, ActorRef};

/// A check as the log tells it: the entry `check` with its number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    number: u64,
    hash: EntryHash,
    time: i64,
    writer: Actor,
    commit: CommitId,
    base: Option<CommitId>,
    result: CheckResult,
}

impl Check {
    /// The number of the entry.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// The hash of the entry, which the proof file of the writer signs.
    pub fn hash(&self) -> EntryHash {
        self.hash
    }

    /// The time that the writer claims, in seconds since the Unix epoch (UTC).
    pub fn time(&self) -> i64 {
        self.time
    }

    /// The actor who ran the check and wrote the entry.
    pub fn writer(&self) -> &Actor {
        &self.writer
    }

    /// The commit that was tested.
    pub fn commit(&self) -> &CommitId {
        &self.commit
    }

    /// The tip of `main` that the commit was merged onto for the test, or `None` for a run on the
    /// tip itself.
    pub fn base(&self) -> Option<&CommitId> {
        self.base.as_ref()
    }

    /// The result.
    pub fn result(&self) -> CheckResult {
        self.result
    }

    /// Whether the run passed.
    pub fn passed(&self) -> bool {
        self.result == CheckResult::Pass
    }
}

/// The checks that the log tells of, in the order of their entries. An entry that a purge replaced
/// by a stub is no check any more and is left out.
pub fn checks(entries: &[Entry]) -> Vec<Check> {
    entries
        .iter()
        .filter_map(|entry| {
            let check = entry.check()?;
            // An entry of the chain has a hash; a check is never outside the chain.
            let hash = entry.hash()?;
            Some(Check {
                number: entry.number(),
                hash,
                time: check.time(),
                writer: check.actor().clone(),
                commit: *check.commit(),
                base: check.base().copied(),
                result: check.result(),
            })
        })
        .collect()
}

/// The results for the pair `commit` and `base`, oldest first. `base` is `None` for the runs on
/// a tip of `main` itself.
pub fn for_pair<'a>(
    checks: &'a [Check],
    commit: &CommitId,
    base: Option<&CommitId>,
) -> impl Iterator<Item = &'a Check> {
    let commit = *commit;
    let base = base.copied();
    checks
        .iter()
        .filter(move |check| check.commit == commit && check.base == base)
}

/// The last green tip (ADR-019): the newest commit of `main` that a passing run without a base
/// covered. `main` is the first-parent history of its tip, newest commit first; a run on a commit
/// that is not in it says nothing about `main` and is left out. Of several runs on the same
/// commit the newest in the log counts. The result of a failing run does not take the place of
/// an older passing one; the gate finds the first red tip from the failing runs after the last
/// green tip in the history of `main`. This function reads the log and the history only: it asks
/// neither for the register nor for a proof;
/// [`Repository::last_green_tip`](crate::repository::Repository::last_green_tip) does.
pub fn last_green_tip<'a>(checks: &'a [Check], main: &[CommitId]) -> Option<&'a Check> {
    checks
        .iter()
        .filter(|check| check.base.is_none() && check.passed())
        .filter_map(|check| {
            let position = main.iter().position(|commit| *commit == check.commit)?;
            Some((position, check))
        })
        .min_by_key(|(position, check)| (*position, std::cmp::Reverse(check.number)))
        .map(|(_, check)| check)
}

/// Whether a result is valid for the tip `tip` of `main` at the risk `level` (ADR-019).
///
/// At the levels 2 and 3 a result is valid only for the tip it was run against, so its base is
/// `tip`. At the levels 0 and 1 it stays valid when `main` moved if the merge onto the new tip is
/// clean; that question is the gate's, and this function does not close it: it says `true` for
/// any base there. A run without a base is the run on a tip, never the result for a change.
pub fn applies_at(level: RiskLevel, check: &Check, tip: &CommitId) -> bool {
    match (level, check.base()) {
        (_, None) => false,
        (RiskLevel::Zero | RiskLevel::One, Some(_)) => true,
        (RiskLevel::Two | RiskLevel::Three, Some(base)) => base == tip,
    }
}

/// The sponsor that the register or, if the actor is not registered, the actor line names for
/// `actor`, or `None` for a human. The register wins: the actor line is a claim.
fn sponsor_of(actor: &Actor, register: Option<&Register>) -> Option<Identity> {
    let registered = register
        .and_then(|register| register.entry(&Identity::of(actor)))
        .and_then(|entry| entry.sponsor().cloned());
    registered.or_else(|| match actor.view() {
        // The sponsor of an agent is checked like the id of a human when the agent is created.
        ActorRef::Agent { sponsor, .. } => {
            Some(Identity::human(sponsor).expect("the sponsor of an agent is a valid id"))
        }
        _ => None,
    })
}

/// Whether `writer` is independent of the change that `opener` opened and whose commits `authors`
/// wrote (the commits `base..head` of ADR-019, which are the commits `tip..commit` for the tip of
/// `main` that the pull request is merged onto).
///
/// The writer is not independent if it is
///
/// - the opener or one of the authors (the identity counts, not the model or the session),
/// - an agent whose sponsor is the opener or one of the authors, or
/// - the sponsor of an agent that is the opener or one of the authors (ADR-004: the stricter
///   rule "also excludes the sponsor of the opener and of the authors").
///
/// Two agents of one sponsor are *not* excluded by this function when neither is the sponsor of
/// the other: they count as two approvals (ADR-004), and the fields `model` and `session` are
/// descriptive only (STEMMA-222). The sponsor is the one of the register where the actor is
/// registered, otherwise the one that the actor line claims.
pub fn is_independent(
    writer: &Actor,
    opener: &Actor,
    authors: &[Actor],
    register: Option<&Register>,
) -> bool {
    let writer_id = Identity::of(writer);
    let involved: Vec<&Actor> = std::iter::once(opener).chain(authors).collect();
    if involved
        .iter()
        .any(|actor| Identity::of(actor) == writer_id)
    {
        return false;
    }
    if let Some(sponsor) = sponsor_of(writer, register) {
        if involved.iter().any(|actor| Identity::of(actor) == sponsor) {
            return false;
        }
    }
    !involved
        .iter()
        .any(|actor| sponsor_of(actor, register).is_some_and(|sponsor| sponsor == writer_id))
}

/// Whether the check counts for a change at the risk `level` as far as its writer goes.
///
/// - At the levels 0 and 1 every registered actor counts. Without a register (the open mode) the
///   actor is a claim and every actor counts.
/// - At the levels 2 and 3 the writer must be registered as well, and independent of the change
///   ([`is_independent`]).
///
/// `register` is the register that counts for the signatures: the one of the base commit of the
/// pull request (ADR-003, ADR-004), not the `base` of the check, which is a tip of `main`. The
/// signature itself is checked by
/// [`Repository::counted_checks`](crate::repository::Repository::counted_checks).
pub fn counts(
    level: RiskLevel,
    check: &Check,
    opener: &Actor,
    authors: &[Actor],
    register: Option<&Register>,
) -> bool {
    let registered =
        register.is_none_or(|register| register.entry(&Identity::of(check.writer())).is_some());
    if !registered {
        return false;
    }
    match level {
        RiskLevel::Zero | RiskLevel::One => true,
        RiskLevel::Two | RiskLevel::Three => {
            is_independent(check.writer(), opener, authors, register)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::{Chain, CheckEntry, Record};

    const A: &str = "b3:1d719c9e1d926eb0f33467406bf50911eca1d59f976105abd3eb5ccfaabdeed9";
    const B: &str = "b3:4feb2d73a2c706e33c2a71f6f03cbfb48f0a20b48567936fca293159e7e3b32c";
    const C: &str = "b3:8cb4a1c0e7d2f5b9163c0a4e8d7f2b6a5c9e1d3f4a8b7c6d5e2f1a0b9c8d7e6f";

    fn commit(text: &str) -> CommitId {
        CommitId::new(text.parse().unwrap())
    }

    fn human(id: &str) -> Actor {
        Actor::human(id).unwrap()
    }

    fn agent(id: &str, sponsor: &str) -> Actor {
        Actor::agent(id, "m", "s", sponsor).unwrap()
    }

    fn entry(
        number: u64,
        writer: Actor,
        tested: &str,
        base: Option<&str>,
        result: CheckResult,
    ) -> Entry {
        let check = CheckEntry::new(
            number as i64,
            writer,
            commit(tested),
            base.map(commit),
            result,
        );
        Entry::linked(number, Chain::Start, Record::Check(check)).unwrap()
    }

    fn register() -> Register {
        // Daniel owns the register; Ana and Mia are members.
        let key = |seed: u8| {
            use ssh_key::private::Ed25519Keypair;
            use ssh_key::PrivateKey;
            let private = PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
            crate::signature::PublicKey::from_openssh(&private.public_key().to_openssh().unwrap())
                .unwrap()
        };
        let text = format!(
            "stemma-register 1\nentry human ana member\nkey {} active\n\
             entry human daniel owner\nkey {} active\n\
             entry human mia member\nkey {} active\n",
            key(2),
            key(1),
            key(3)
        );
        Register::parse(text.as_bytes()).unwrap()
    }

    #[test]
    fn the_checks_of_a_log_keep_their_numbers_and_skip_what_is_no_check() {
        let log = [
            entry(1, human("mia"), A, Some(B), CheckResult::Pass),
            entry(2, human("ana"), A, None, CheckResult::Fail),
        ];
        let found = checks(&log);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].number(), 1);
        assert_eq!(found[0].writer(), &human("mia"));
        assert_eq!(found[0].base(), Some(&commit(B)));
        assert!(found[0].passed());
        assert_eq!(found[1].base(), None);
        assert!(!found[1].passed());
        let stub = log[0].to_stub().unwrap();
        assert_eq!(
            checks(&[stub, log[1].clone()]).len(),
            1,
            "a stub is no check"
        );
    }

    #[test]
    fn a_result_belongs_to_its_pair_of_commits() {
        let log = [
            entry(1, human("mia"), A, Some(B), CheckResult::Pass),
            entry(2, human("mia"), A, Some(C), CheckResult::Fail),
            entry(3, human("mia"), A, Some(B), CheckResult::Fail),
        ];
        let found = checks(&log);
        let numbers = |commit_id: &CommitId, base: Option<&CommitId>| -> Vec<u64> {
            for_pair(&found, commit_id, base)
                .map(Check::number)
                .collect()
        };
        assert_eq!(numbers(&commit(A), Some(&commit(B))), [1, 3]);
        assert_eq!(numbers(&commit(A), Some(&commit(C))), [2]);
        assert!(numbers(&commit(B), Some(&commit(A))).is_empty());
        assert!(numbers(&commit(A), None).is_empty());
    }

    #[test]
    fn the_last_green_tip_is_the_newest_commit_of_main_with_a_pass_without_a_base() {
        let log = [
            entry(1, human("mia"), A, None, CheckResult::Pass),
            entry(2, human("mia"), B, Some(A), CheckResult::Pass),
            entry(3, human("mia"), B, None, CheckResult::Fail),
            entry(4, human("mia"), C, None, CheckResult::Pass),
            entry(5, human("mia"), A, None, CheckResult::Fail),
            // A late rerun on an older tip does not displace a newer one.
            entry(6, human("mia"), A, None, CheckResult::Pass),
        ];
        let found = checks(&log);
        let main = [commit(C), commit(B), commit(A)];
        let number =
            |found: &[Check], main: &[CommitId]| last_green_tip(found, main).map(Check::number);
        assert_eq!(number(&found, &main), Some(4));
        // A passes twice; the newest run on the commit counts.
        assert_eq!(number(&found, &main[1..]), Some(6));
        // B has no pass without a base, and C is not in this history: nothing on `main` is green.
        assert_eq!(number(&found[1..4], &[commit(B), commit(A)]), None);
        assert_eq!(number(&found, &[]), None);
        assert_eq!(number(&[], &main), None);
    }

    #[test]
    fn at_the_levels_two_and_three_a_result_is_valid_only_for_its_tip() {
        let log = [
            entry(1, human("mia"), A, Some(B), CheckResult::Pass),
            entry(2, human("mia"), B, None, CheckResult::Pass),
        ];
        let found = checks(&log);
        for level in [RiskLevel::Two, RiskLevel::Three] {
            assert!(applies_at(level, &found[0], &commit(B)), "{level:?}");
            assert!(!applies_at(level, &found[0], &commit(C)), "{level:?}");
        }
        for level in [RiskLevel::Zero, RiskLevel::One] {
            assert!(applies_at(level, &found[0], &commit(C)), "{level:?}");
        }
        // The run on a tip is no result for a change.
        assert!(!applies_at(RiskLevel::Zero, &found[1], &commit(B)));
    }

    #[test]
    fn the_writer_is_not_the_opener_nor_an_author() {
        let opener = human("mia");
        let authors = [human("ana"), agent("bot", "ana")];
        assert!(!is_independent(&opener, &opener, &authors, None));
        assert!(!is_independent(&human("ana"), &opener, &authors, None));
        // The same identity under another session is the same writer.
        let other_session = Actor::agent("bot", "m2", "s2", "ana").unwrap();
        assert!(!is_independent(&other_session, &opener, &authors, None));
        assert!(is_independent(&human("daniel"), &opener, &authors, None));
        assert!(is_independent(
            &agent("other", "daniel"),
            &opener,
            &authors,
            None
        ));
    }

    #[test]
    fn an_agent_of_the_sponsor_of_the_change_is_not_independent() {
        let opener = human("mia");
        let authors = [human("ana")];
        // An agent whose sponsor is the opener, or an author.
        assert!(!is_independent(
            &agent("a1", "mia"),
            &opener,
            &authors,
            None
        ));
        assert!(!is_independent(
            &agent("a2", "ana"),
            &opener,
            &authors,
            None
        ));
        assert!(is_independent(
            &agent("a3", "daniel"),
            &opener,
            &authors,
            None
        ));
    }

    #[test]
    fn the_sponsor_of_an_agent_that_opened_or_wrote_is_not_independent() {
        let opener = agent("bot", "daniel");
        assert!(!is_independent(&human("daniel"), &opener, &[], None));
        assert!(is_independent(&human("ana"), &opener, &[], None));
        let author = agent("bot", "ana");
        assert!(!is_independent(
            &human("ana"),
            &human("mia"),
            &[author],
            None
        ));
    }

    #[test]
    fn two_agents_of_one_sponsor_stay_two_reviews() {
        // Neither is the sponsor of the other, so the rule does not exclude the second.
        let opener = agent("one", "daniel");
        assert!(is_independent(&agent("two", "daniel"), &opener, &[], None));
    }

    #[test]
    fn an_agent_that_the_register_does_not_know_has_the_sponsor_of_its_actor_line() {
        let register = register();
        // `bot` is not in the register, so the sponsor that its actor line claims is all there
        // is. For a registered agent the register wins, see
        // `the_register_wins_over_the_actor_line_of_a_registered_agent`.
        let claimed = agent("bot", "mia");
        assert!(!is_independent(
            &claimed,
            &human("mia"),
            &[],
            Some(&register)
        ));
        assert!(is_independent(
            &claimed,
            &human("ana"),
            &[],
            Some(&register)
        ));
    }

    struct Seeded(ssh_key::PrivateKey);

    impl crate::signature::Signer for Seeded {
        fn sign(
            &self,
            message: &[u8],
        ) -> Result<crate::signature::Signature, crate::signature::SignError> {
            let pem = self
                .0
                .sign(
                    crate::signature::NAMESPACE,
                    ssh_key::HashAlg::Sha512,
                    message,
                )
                .unwrap()
                .to_pem(ssh_key::LineEnding::LF)
                .unwrap();
            Ok(crate::signature::Signature::from_armored(&pem).unwrap())
        }
    }

    /// The register with the agent `bot`, registered under the sponsor Mia (key of seed 3).
    fn register_with_bot() -> Register {
        use crate::register::{sponsorship_payload, Role};
        use crate::signature::{self, PayloadKind};
        use ssh_key::private::Ed25519Keypair;
        let bot = Identity::of(&agent("bot", "mia"));
        let mia = Identity::human("mia").unwrap();
        let mia_key = Seeded(ssh_key::PrivateKey::from(Ed25519Keypair::from_seed(
            &[3; 32],
        )));
        let cosign = signature::sign(
            &mia_key,
            PayloadKind::Sponsorship,
            &sponsorship_payload(&bot, Role::Member, &mia),
        )
        .unwrap();
        let bot_key = ssh_key::PrivateKey::from(Ed25519Keypair::from_seed(&[4; 32]));
        let bot_key =
            crate::signature::PublicKey::from_openssh(&bot_key.public_key().to_openssh().unwrap())
                .unwrap();
        let mut register = register();
        register
            .add_agent(bot, Role::Member, mia, bot_key, cosign)
            .unwrap();
        register
    }

    #[test]
    fn the_register_wins_over_the_actor_line_of_a_registered_agent() {
        let register = register_with_bot();
        // The register says that `bot` is sponsored by Mia; the line of this actor claims Daniel.
        let liar = agent("bot", "daniel");
        assert!(!is_independent(&liar, &human("mia"), &[], Some(&register)));
        assert!(is_independent(
            &liar,
            &human("daniel"),
            &[],
            Some(&register)
        ));
        // Without the register the claim is all there is.
        assert!(is_independent(&liar, &human("mia"), &[], None));
        assert!(!is_independent(&liar, &human("daniel"), &[], None));
    }

    #[test]
    fn every_sponsor_that_an_agent_can_have_is_an_identity() {
        // `sponsor_of` relies on it: the id of a sponsor is checked like the id of a human.
        for sponsor in ["mia", "a", "\u{21}", "~", "ana-1"] {
            let actor = agent("bot", sponsor);
            assert_eq!(
                sponsor_of(&actor, None),
                Some(Identity::human(sponsor).unwrap())
            );
        }
        assert!(Actor::agent("bot", "m", "s", "").is_err());
        assert!(Actor::agent("bot", "m", "s", "a b").is_err());
        assert!(Identity::human("a b").is_err() && Identity::human("").is_err());
    }

    #[test]
    fn the_registered_sponsor_of_an_agent_that_wrote_is_not_independent() {
        let register = register_with_bot();
        let liar = agent("bot", "daniel");
        assert!(!is_independent(
            &human("mia"),
            &human("ana"),
            std::slice::from_ref(&liar),
            Some(&register)
        ));
        assert!(is_independent(
            &human("daniel"),
            &human("ana"),
            std::slice::from_ref(&liar),
            Some(&register)
        ));
    }

    #[test]
    fn a_registered_agent_counts_at_level_two_when_its_sponsor_is_not_involved() {
        let register = register_with_bot();
        let log = [entry(1, agent("bot", "mia"), A, Some(B), CheckResult::Pass)];
        let found = checks(&log);
        assert!(counts(
            RiskLevel::Two,
            &found[0],
            &human("ana"),
            &[],
            Some(&register)
        ));
        assert!(!counts(
            RiskLevel::Two,
            &found[0],
            &human("mia"),
            &[],
            Some(&register)
        ));
    }

    #[test]
    fn at_the_levels_zero_and_one_every_registered_actor_counts() {
        let register = register();
        let log = [
            entry(1, human("mia"), A, Some(B), CheckResult::Pass),
            entry(2, human("stranger"), A, Some(B), CheckResult::Pass),
        ];
        let found = checks(&log);
        let opener = human("mia");
        for level in [RiskLevel::Zero, RiskLevel::One] {
            // Mia is the opener and still counts; a stranger is not in the register.
            assert!(counts(level, &found[0], &opener, &[], Some(&register)));
            assert!(!counts(level, &found[1], &opener, &[], Some(&register)));
            // In the open mode there is no register, and the actor is a claim.
            assert!(counts(level, &found[1], &opener, &[], None));
        }
    }

    #[test]
    fn at_the_levels_two_and_three_the_writer_is_registered_and_independent() {
        let register = register();
        let log = [
            entry(1, human("mia"), A, Some(B), CheckResult::Pass),
            entry(2, human("daniel"), A, Some(B), CheckResult::Pass),
            entry(3, human("stranger"), A, Some(B), CheckResult::Pass),
        ];
        let found = checks(&log);
        let opener = human("mia");
        for level in [RiskLevel::Two, RiskLevel::Three] {
            assert!(!counts(level, &found[0], &opener, &[], Some(&register)));
            assert!(counts(level, &found[1], &opener, &[], Some(&register)));
            assert!(!counts(level, &found[2], &opener, &[], Some(&register)));
            assert!(!counts(
                level,
                &found[1],
                &opener,
                &[human("daniel")],
                Some(&register)
            ));
        }
    }
}
