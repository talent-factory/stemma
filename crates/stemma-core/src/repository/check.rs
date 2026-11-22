//! The operations on the evidence of a check (ADR-019, STEMMA-240).
//!
//! [`record_check`](Repository::record_check) writes the entry `check` under the lock of the
//! references, as every entry that moves no pointer. The entry says who ran the check, which commit
//! it tested, the tip of `main` it was merged onto, and the result. It asks the authorizer for the
//! operation `check` and nothing more about the writer: whether a result counts for a change is
//! decided when it is read, by [`counted_checks`](Repository::counted_checks), because it depends
//! on the pull request, its authors, and its risk level, none of which the entry holds. The last
//! green tip is read by [`last_green_tip`](Repository::last_green_tip).

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;

use super::{now, Repository};
use crate::check::{self, Check};
use crate::conflict::AnyCommit;
use crate::history::{self, HistoryError, Parents};
use crate::object::{CommitId, Kind};
use crate::oplog::{CheckEntry, CheckResult, Entry, OpLogError, OperationName, CHECK};
use crate::policy::RiskLevel;
use crate::refs::RefError;
use crate::register::{proof_path, ProofFile, Register};
use crate::store::StoreError;
use crate::{Actor, Authorizer};

impl Repository {
    /// Writes the entry of a check run by `actor`: `commit` was tested, merged onto `base` (the
    /// tip of `main` of that test), or run as it is for `None`, and the run has `result`.
    ///
    /// One entry holds one base, so a run against another tip is another call. In an enforcing
    /// repository the caller signs the hash of the returned entry with the key of the actor and
    /// writes the proof (`register::write_proof`), as for every entry whose rule depends on who
    /// acts; a result without a proof does not count ([`counted_checks`](Self::counted_checks)).
    ///
    /// # Errors
    ///
    /// Returns [`CheckError::Denied`] if the authorizer refuses, [`CheckError::BaseIsCommit`] if
    /// `base` is `commit` (a run merged onto itself), [`CheckError::NoSuchCommit`] if the store
    /// has no commit of that ID (nothing is written), and [`CheckError::Refs`],
    /// [`CheckError::Store`], and [`CheckError::Log`] for a failure of the lock, the store, or
    /// the log.
    pub fn record_check(
        &self,
        commit: CommitId,
        base: Option<CommitId>,
        result: CheckResult,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Entry, CheckError> {
        let name = OperationName::try_new(CHECK).expect("`check` is an operation name");
        let target = commit.to_string();
        let allowed = self
            .authorize(actor, authorizer, &name, &target)
            .map_err(|_| CheckError::Denied {
                operation: CHECK.to_owned(),
                target,
            })?;
        if base == Some(commit) {
            return Err(CheckError::BaseIsCommit { commit });
        }
        let lock = self.refs_store().lock_writer(&allowed)?;
        for named in std::iter::once(commit).chain(base) {
            match self.objects().kind_of(&named.object_id())? {
                Some(Kind::Commit | Kind::Commit2) => {}
                Some(Kind::Blob | Kind::Tree) | None => {
                    return Err(CheckError::NoSuchCommit { commit: named });
                }
            }
        }
        let entry = CheckEntry::new(now(), actor.clone(), commit, base, result);
        Ok(self.append_check(&lock, &entry)?)
    }

    /// The checks that the log tells of, in the order of their entries (see [`check::checks`]).
    ///
    /// # Errors
    ///
    /// Returns [`CheckError::Log`] if the log cannot be read.
    pub fn checks(&self) -> Result<Vec<Check>, CheckError> {
        let entries = self.oplog().entries().map_err(CheckError::Log)?;
        Ok(check::checks(&entries))
    }

    /// Every check of the log that an enforcing repository accepts: all of them in the open mode
    /// (`register` is `None`, the writer is a claim), and in an enforcing one those with a proof
    /// of the writer, as in [`counted_checks`](Self::counted_checks). A run that fails counts as
    /// well as one that passes: an unproven failure must not pause `main`.
    ///
    /// # Errors
    ///
    /// Returns [`CheckError::Log`] if the log cannot be read and [`CheckError::Io`] if a proof
    /// file cannot be read.
    pub(super) fn proven_checks(
        &self,
        register: Option<&Register>,
    ) -> Result<Vec<Check>, CheckError> {
        let mut found = Vec::new();
        for check in self.checks()? {
            if let Some(register) = register {
                if !self.has_proof(register, &check)? {
                    continue;
                }
            }
            found.push(check);
        }
        Ok(found)
    }

    /// The results that count for the pull request `pull` with the head `commit` at the risk
    /// `level`, when `main` stands at `tip` (ADR-019).
    ///
    /// A result counts if it passed and
    ///
    /// - names `commit` and has a base the rule of the level accepts ([`check::applies_at`]: the
    ///   tip itself at the levels 2 and 3),
    /// - has a writer that counts at the level ([`check::counts`]): registered where a register
    ///   exists, at every level, and at the levels 2 and 3 independent of the opener of the pull
    ///   request, of the authors of the commits `tip..commit`, and of their sponsors, and
    /// - has, in an enforcing repository (`register` is `Some`), a proof file that the register
    ///   accepts for the number of the entry: a signature of the hash of the entry by a key of the
    ///   writer. A missing, malformed, or invalid proof does not count and is not reported here;
    ///   `stemma op verify` checks the proof files that exist, and a missing proof only means that
    ///   the result does not count.
    ///
    /// `register` is the register of the base commit of the pull request, the one that counts for
    /// the signatures (ADR-003); at the levels 0 and 1 the results may have other bases, and the
    /// caller decides which register is the right one for them. `None` is the open mode, where
    /// the actor is a claim and no proof exists.
    ///
    /// # Errors
    ///
    /// Returns [`CheckError::NoSuchPull`] for a number that no opening has,
    /// [`CheckError::NotTheHead`] if `commit` is not the head that the opening names,
    /// [`CheckError::TipReachesHead`] at the levels 2 and 3 if `tip` is `commit` or descends from
    /// it (the authors of the change would be none), [`CheckError::History`] at these levels if
    /// the commits cannot be walked,
    /// [`CheckError::Io`] if a proof file cannot be read, and [`CheckError::Log`] if the log
    /// cannot be read.
    pub fn counted_checks(
        &self,
        pull: u64,
        commit: &CommitId,
        tip: &CommitId,
        level: RiskLevel,
        register: Option<&Register>,
    ) -> Result<Vec<Check>, CheckError> {
        let entries = self.oplog().entries().map_err(CheckError::Log)?;
        let pulls = crate::pull::pull_requests(&entries);
        let pull = pulls
            .iter()
            .find(|candidate| candidate.number() == pull)
            .ok_or(CheckError::NoSuchPull { number: pull })?;
        if pull.head() != commit {
            return Err(CheckError::NotTheHead {
                number: pull.number(),
            });
        }
        // Only the levels 2 and 3 ask for independence, so only they need the authors (and a
        // history that can be walked).
        let authors = match level {
            RiskLevel::Zero | RiskLevel::One => Vec::new(),
            RiskLevel::Two | RiskLevel::Three => self.change_authors(commit, tip)?,
        };
        let mut counted = Vec::new();
        for found in check::checks(&entries) {
            if found.commit() != commit
                || !found.passed()
                || !check::applies_at(level, &found, tip)
                || !check::counts(level, &found, pull.opener(), &authors, register)
            {
                continue;
            }
            if let Some(register) = register {
                if !self.has_proof(register, &found)? {
                    continue;
                }
            }
            counted.push(found);
        }
        Ok(counted)
    }

    /// The last green tip (ADR-019): the newest commit of `main` that a passing run without a
    /// base covered (see [`check::last_green_tip`]). `main_tip` is the current tip of `main`; its
    /// first-parent history is `main`, and a run on a commit outside it does not count.
    ///
    /// In an enforcing repository (`register` is `Some`, the register of the current tip of
    /// `main`) only a run counts whose writer has a proof that the register accepts, as in
    /// [`counted_checks`](Self::counted_checks); otherwise any actor could write the tip. `None`
    /// is the open mode, where the actor is a claim and no proof exists; it must not be passed for
    /// an enforcing repository, as the proof would then not be asked for.
    ///
    /// # Errors
    ///
    /// Returns [`CheckError::Log`] if the log cannot be read, [`CheckError::History`] if the
    /// history of `main` cannot be walked, and [`CheckError::Io`] if a proof file cannot be read.
    pub fn last_green_tip(
        &self,
        main_tip: &CommitId,
        register: Option<&Register>,
    ) -> Result<Option<Check>, CheckError> {
        let main: Vec<CommitId> =
            history::ancestors(self.objects(), *main_tip, Parents::First, None)?
                .into_iter()
                .map(|(id, _)| id)
                .collect();
        let mut candidates = Vec::new();
        for found in self.checks()? {
            if found.base().is_some() || !found.passed() || !main.contains(found.commit()) {
                continue;
            }
            if let Some(register) = register {
                if !self.has_proof(register, &found)? {
                    continue;
                }
            }
            candidates.push(found);
        }
        Ok(check::last_green_tip(&candidates, &main).cloned())
    }

    /// The authors of the commits `tip..commit`: those that `commit` reaches and `tip` does not.
    ///
    /// # Errors
    ///
    /// Returns [`CheckError::TipReachesHead`] if `tip` reaches `commit`, and
    /// [`CheckError::History`] if a commit cannot be read.
    pub(super) fn change_authors(
        &self,
        commit: &CommitId,
        tip: &CommitId,
    ) -> Result<Vec<Actor>, CheckError> {
        let store = self.objects();
        let known: HashSet<CommitId> = history::ancestors(store, *tip, Parents::All, None)?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        if known.contains(commit) {
            return Err(CheckError::TipReachesHead { commit: *commit });
        }
        let mut authors: Vec<Actor> = Vec::new();
        for (id, found) in history::ancestors(store, *commit, Parents::All, None)? {
            let author: &Actor = match &found {
                AnyCommit::Commit(found) => found.author(),
                AnyCommit::Commit2(found) => found.author(),
            };
            if !known.contains(&id) && !authors.contains(author) {
                authors.push(author.clone());
            }
        }
        Ok(authors)
    }

    /// Whether the proof file of the entry exists and the register accepts it.
    fn has_proof(&self, register: &Register, check: &Check) -> Result<bool, CheckError> {
        let text = match fs::read_to_string(proof_path(&self.metadata_dir(), check.number())) {
            Ok(text) => text,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::InvalidData
                ) =>
            {
                return Ok(false)
            }
            Err(error) => return Err(CheckError::Io(error)),
        };
        let Ok(proof) = ProofFile::parse(&text) else {
            return Ok(false);
        };
        Ok(proof.actor == *check.writer()
            && register
                .verify_log_entry(
                    &proof.actor,
                    check.number(),
                    check.hash().to_string().as_bytes(),
                    &proof.signature,
                )
                .is_ok())
    }
}

/// Why an operation on the evidence of a check fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum CheckError {
    /// The authorizer refused the operation. Nothing was written.
    Denied {
        /// The operation.
        operation: String,
        /// The tested commit.
        target: String,
    },
    /// The base of the run is the tested commit: a run merged onto itself says nothing.
    BaseIsCommit {
        /// The commit.
        commit: CommitId,
    },
    /// The store has no commit of that ID.
    NoSuchCommit {
        /// The commit.
        commit: CommitId,
    },
    /// The log has no opening with this number.
    NoSuchPull {
        /// The number.
        number: u64,
    },
    /// The commit is not the head that the opening of the pull request names. A branch that moved
    /// after the opening is a new head (ADR-004: any new commit voids the evidence).
    NotTheHead {
        /// The number of the pull request.
        number: u64,
    },
    /// The tip of `main` is the head or descends from it, so no commit is left for the change.
    TipReachesHead {
        /// The head.
        commit: CommitId,
    },
    /// The lock of the references or the references failed. Nothing was written.
    Refs(RefError),
    /// The log could not be read or written. See [`OpLogError`].
    Log(OpLogError),
    /// The object store could not be read. Nothing was written.
    Store(StoreError),
    /// The history could not be walked.
    History(HistoryError),
    /// A proof file could not be read.
    Io(io::Error),
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied { operation, target } => {
                write!(f, "`{operation}` on `{target}` is not allowed")
            }
            Self::BaseIsCommit { commit } => {
                write!(
                    f,
                    "the base of a check cannot be the commit {commit} itself"
                )
            }
            Self::NotTheHead { number } => {
                write!(f, "the commit is not the head of pull request {number}")
            }
            Self::TipReachesHead { commit } => {
                write!(f, "the tip of main reaches the head {commit}")
            }
            Self::NoSuchCommit { commit } => write!(f, "the store has no commit {commit}"),
            Self::NoSuchPull { number } => write!(f, "the log has no pull request {number}"),
            Self::Refs(error) => error.fmt(f),
            Self::Log(error) => error.fmt(f),
            Self::Store(error) => error.fmt(f),
            Self::History(error) => error.fmt(f),
            Self::Io(error) => write!(f, "cannot read a proof: {error}"),
        }
    }
}

impl Error for CheckError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Refs(error) => Some(error),
            Self::Log(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::History(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<RefError> for CheckError {
    fn from(error: RefError) -> Self {
        Self::Refs(error)
    }
}

impl From<OpLogError> for CheckError {
    fn from(error: OpLogError) -> Self {
        Self::Log(error)
    }
}

impl From<StoreError> for CheckError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<HistoryError> for CheckError {
    fn from(error: HistoryError) -> Self {
        Self::History(error)
    }
}

#[cfg(test)]
mod tests {
    use ssh_key::private::Ed25519Keypair;
    use ssh_key::{HashAlg, LineEnding, PrivateKey};

    use super::*;
    use crate::change::ChangeId;
    use crate::object::Commit;
    use crate::refs::BranchName;
    use crate::register::write_proof;
    use crate::repository::InitOptions;
    use crate::signature::{self, PayloadKind, PublicKey, SignError, Signature, Signer, NAMESPACE};
    use crate::store::ObjectStore;
    use crate::testutil::TempDir;
    use crate::{AllowAll, Decision};

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &OperationName, _target: &str) -> Decision {
            Decision::Deny
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

    fn human(id: &str) -> Actor {
        Actor::human(id).unwrap()
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

    /// A repository whose `main` is at `tip`, and a branch `feature` with one commit of Mia on it.
    /// The pull request 2 is opened by Mia.
    struct Fixture {
        repository: Repository,
        tip: CommitId,
        head: CommitId,
        /// A commit of Zoe on `tip` that is neither: the base of a run against an older tip.
        old: CommitId,
        number: u64,
    }

    fn fixture(dir: &TempDir) -> Fixture {
        let repository = Repository::init(
            dir.path(),
            &InitOptions::default(),
            &human("daniel"),
            &AllowAll,
        )
        .unwrap();
        let tip = repository.refs().working_copy().unwrap();
        let tree = repository
            .objects()
            .get_commit(&tip)
            .unwrap()
            .unwrap()
            .tree();
        let head = repository
            .objects()
            .put_commit(
                &Commit::try_new(
                    tree,
                    vec![tip],
                    ChangeId::generate().unwrap(),
                    human("mia"),
                    1,
                    "work of mia",
                )
                .unwrap(),
            )
            .unwrap();
        let old = repository
            .objects()
            .put_commit(
                &Commit::try_new(
                    tree,
                    vec![tip],
                    ChangeId::generate().unwrap(),
                    human("zoe"),
                    1,
                    "work of zoe",
                )
                .unwrap(),
            )
            .unwrap();
        let feature = BranchName::try_new("feature").unwrap();
        repository
            .create_branch(
                &OperationName::try_new("branch").unwrap(),
                &feature,
                head.object_id(),
                &human("daniel"),
                &AllowAll,
            )
            .unwrap();
        let number = repository
            .open_pull(&feature, &human("mia"), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        Fixture {
            repository,
            tip,
            head,
            old,
            number,
        }
    }

    fn record(f: &Fixture, writer: &str, base: Option<CommitId>, result: CheckResult) -> Entry {
        f.repository
            .record_check(f.head, base, result, &human(writer), &AllowAll)
            .unwrap()
    }

    fn counted(f: &Fixture, level: RiskLevel, register: Option<&Register>) -> Vec<u64> {
        f.repository
            .counted_checks(f.number, &f.head, &f.tip, level, register)
            .unwrap()
            .iter()
            .map(Check::number)
            .collect()
    }

    #[test]
    fn a_check_is_written_to_the_log_and_raises_the_format() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let entry = record(&f, "ana", Some(f.tip), CheckResult::Pass);
        let check = entry.check().unwrap();
        assert_eq!(check.commit(), &f.head);
        assert_eq!(check.base(), Some(&f.tip));
        assert_eq!(check.result(), CheckResult::Pass);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".stemma/format")).unwrap(),
            "stemma-repository 12\n"
        );
        let found = f.repository.checks().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].number(), entry.number());
        f.repository.oplog().verify().expect("the chain holds");
    }

    #[test]
    fn a_check_refuses_before_anything_is_written() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let before = f.repository.oplog().entries().unwrap().len();
        let not_stored = CommitId::new(crate::object::Blob::new("not stored").id().object_id());
        let run = |commit, base, authorizer: &dyn Authorizer| {
            f.repository
                .record_check(commit, base, CheckResult::Pass, &human("ana"), authorizer)
        };
        assert!(matches!(
            run(not_stored, None, &AllowAll),
            Err(CheckError::NoSuchCommit { .. })
        ));
        assert!(matches!(
            run(f.head, Some(not_stored), &AllowAll),
            Err(CheckError::NoSuchCommit { .. })
        ));
        assert!(matches!(
            run(f.head, None, &DenyAll),
            Err(CheckError::Denied { ref operation, ref target })
                if operation == "check" && *target == f.head.to_string()
        ));
        // The authorizer is asked first: a refused actor learns nothing about the base.
        assert!(matches!(
            run(f.head, Some(f.head), &DenyAll),
            Err(CheckError::Denied { .. })
        ));
        // A run merged onto itself says nothing; a blob is no commit.
        assert!(matches!(
            run(f.head, Some(f.head), &AllowAll),
            Err(CheckError::BaseIsCommit { .. })
        ));
        let blob = f
            .repository
            .objects()
            .put_blob(&crate::object::Blob::new("a blob"))
            .unwrap();
        assert!(matches!(
            run(CommitId::new(blob.object_id()), None, &AllowAll),
            Err(CheckError::NoSuchCommit { .. })
        ));
        assert_eq!(f.repository.oplog().entries().unwrap().len(), before);
    }

    #[test]
    fn a_result_is_evidence_for_its_pair_and_a_later_run_for_another_tip_is_another_entry() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let first = record(&f, "ana", Some(f.tip), CheckResult::Pass);
        let second = record(&f, "ana", Some(f.old), CheckResult::Fail);
        let found = f.repository.checks().unwrap();
        let pair = |base: &CommitId| -> Vec<u64> {
            check::for_pair(&found, &f.head, Some(base))
                .map(Check::number)
                .collect()
        };
        assert_eq!(pair(&f.tip), [first.number()]);
        assert_eq!(pair(&f.old), [second.number()]);
    }

    #[test]
    fn at_the_levels_two_and_three_only_the_tip_of_the_run_counts_and_an_independent_writer() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        // Mia opened and wrote the change; Ana and Daniel are independent of it.
        let own = record(&f, "mia", Some(f.tip), CheckResult::Pass);
        let ana = record(&f, "ana", Some(f.tip), CheckResult::Pass);
        let stale = record(&f, "daniel", Some(f.old), CheckResult::Pass);
        let tip_run = record(&f, "ana", None, CheckResult::Pass);
        for level in [RiskLevel::Two, RiskLevel::Three] {
            assert_eq!(counted(&f, level, None), [ana.number()], "{level:?}");
        }
        // Levels 0 and 1: every writer counts, the base need not be the tip, and a run on a tip
        // is no result of the change.
        for level in [RiskLevel::Zero, RiskLevel::One] {
            assert_eq!(
                counted(&f, level, None),
                [own.number(), ana.number(), stale.number()],
                "{level:?}"
            );
        }
        assert!(!counted(&f, RiskLevel::Zero, None).contains(&tip_run.number()));
    }

    #[test]
    fn an_agent_of_the_opener_does_not_count_at_level_two() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let agent = Actor::agent("bot", "m", "s", "mia").unwrap();
        let entry = f
            .repository
            .record_check(f.head, Some(f.tip), CheckResult::Pass, &agent, &AllowAll)
            .unwrap();
        assert!(counted(&f, RiskLevel::Two, None).is_empty());
        assert_eq!(counted(&f, RiskLevel::One, None), [entry.number()]);
    }

    #[test]
    fn in_an_enforcing_repository_a_result_counts_only_with_a_proof_of_its_writer() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let register = register();
        let metadata = f.repository.metadata_dir();
        let signed = record(&f, "ana", Some(f.tip), CheckResult::Pass);
        let unsigned = record(&f, "daniel", Some(f.tip), CheckResult::Pass);
        let forged = record(&f, "mia", Some(f.tip), CheckResult::Pass);
        let sign = |entry: &Entry, seed: u8, actor: &str| {
            let signature = signature::sign(
                &Seeded::new(seed),
                PayloadKind::LogEntry,
                entry.hash().unwrap().to_string().as_bytes(),
            )
            .unwrap();
            write_proof(
                &metadata,
                entry.number(),
                &ProofFile {
                    actor: human(actor),
                    signature,
                },
            )
            .unwrap();
        };
        sign(&signed, 2, "ana");
        // A proof with the key of another actor is none.
        sign(&forged, 2, "mia");
        assert_eq!(
            counted(&f, RiskLevel::Zero, Some(&register)),
            [signed.number()]
        );
        assert_eq!(
            counted(&f, RiskLevel::Two, Some(&register)),
            [signed.number()]
        );
        // The open mode asks for no proof.
        assert_eq!(counted(&f, RiskLevel::Two, None).len(), 2);
        assert!(!counted(&f, RiskLevel::Zero, Some(&register)).contains(&unsigned.number()));
        // A proof file that is no proof, and a proof of another actor, do not count and do not
        // fail.
        let garbage = record(&f, "ana", Some(f.tip), CheckResult::Pass);
        std::fs::write(proof_path(&metadata, garbage.number()), "not a proof").unwrap();
        let other = record(&f, "ana", Some(f.tip), CheckResult::Pass);
        sign(&other, 3, "mia");
        assert_eq!(
            counted(&f, RiskLevel::Zero, Some(&register)),
            [signed.number()]
        );
    }

    #[test]
    fn a_failing_result_is_no_evidence_and_the_author_of_the_tip_stays_independent() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        record(&f, "ana", Some(f.tip), CheckResult::Fail);
        // Daniel wrote the commit that the tip names; only `tip..commit` counts for the authors.
        let daniel = record(&f, "daniel", Some(f.tip), CheckResult::Pass);
        for level in [RiskLevel::Zero, RiskLevel::Two] {
            assert_eq!(counted(&f, level, None), [daniel.number()], "{level:?}");
        }
    }

    #[test]
    fn every_author_between_the_tip_and_the_head_is_excluded_also_over_a_merge() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let store = f.repository.objects();
        let tree = store.get_commit(&f.tip).unwrap().unwrap().tree();
        let put = |parents: Vec<CommitId>, author: &str| {
            let commit = Commit::try_new(
                tree,
                parents,
                ChangeId::generate().unwrap(),
                human(author),
                2,
                "work",
            )
            .unwrap();
            store.put_commit(&commit).unwrap()
        };
        // The head is a merge of Mia's commit and a side commit of Zoe, on top of one by Ana.
        let side = put(vec![f.tip], "zoe");
        let ana = put(vec![f.head], "ana");
        let merge = put(vec![ana, side], "mia");
        let authors = f.repository.change_authors(&merge, &f.tip).unwrap();
        for name in ["mia", "ana", "zoe"] {
            assert!(authors.contains(&human(name)), "{name}");
        }
        assert!(
            !authors.contains(&human("daniel")),
            "the author of the tip is not one"
        );
        assert_eq!(authors.len(), 3, "each author once");
    }

    #[test]
    fn counting_for_a_pull_request_that_the_log_does_not_have_is_an_error() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let error = f
            .repository
            .counted_checks(99, &f.head, &f.tip, RiskLevel::Zero, None)
            .unwrap_err();
        assert!(
            matches!(error, CheckError::NoSuchPull { number: 99 }),
            "{error:?}"
        );
    }

    #[test]
    fn a_restore_refuses_the_entry_of_a_check() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let entry = record(&f, "ana", None, CheckResult::Pass);
        let error = f
            .repository
            .restore(entry.number(), false, &human("daniel"), &AllowAll)
            .unwrap_err();
        assert!(
            matches!(error, crate::repository::RestoreError::IsCheck { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_result_for_another_commit_does_not_count_for_the_head() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        // A pass of Ana for another commit, merged onto the very tip, is no result for the head.
        f.repository
            .record_check(
                f.old,
                Some(f.tip),
                CheckResult::Pass,
                &human("ana"),
                &AllowAll,
            )
            .unwrap();
        let own = record(&f, "ana", Some(f.tip), CheckResult::Pass);
        for level in [RiskLevel::Zero, RiskLevel::Two] {
            assert_eq!(counted(&f, level, None), [own.number()], "{level:?}");
        }
    }

    #[test]
    fn a_later_failing_run_does_not_take_back_a_passing_one() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let pass = record(&f, "ana", Some(f.tip), CheckResult::Pass);
        record(&f, "ana", Some(f.tip), CheckResult::Fail);
        // A rerun is a new entry; whether the failure matters is the gate's decision.
        assert_eq!(counted(&f, RiskLevel::Two, None), [pass.number()]);
    }

    #[test]
    fn counting_refuses_a_commit_that_is_not_the_head_and_a_tip_that_reaches_it() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let count = |commit: &CommitId, tip: &CommitId| {
            f.repository
                .counted_checks(f.number, commit, tip, RiskLevel::Two, None)
                .unwrap_err()
        };
        assert!(matches!(
            count(&f.tip, &f.tip),
            CheckError::NotTheHead { .. }
        ));
        assert!(matches!(
            count(&f.head, &f.head),
            CheckError::TipReachesHead { .. }
        ));
        // A tip that descends from the head reaches it too.
        let tree = f
            .repository
            .objects()
            .get_commit(&f.head)
            .unwrap()
            .unwrap()
            .tree();
        let child = f
            .repository
            .objects()
            .put_commit(
                &Commit::try_new(
                    tree,
                    vec![f.head],
                    ChangeId::generate().unwrap(),
                    human("zoe"),
                    3,
                    "child",
                )
                .unwrap(),
            )
            .unwrap();
        assert!(matches!(
            count(&f.head, &child),
            CheckError::TipReachesHead { .. }
        ));
    }

    /// A commit of `author` on `parents` with the tree of `tip`.
    fn commit_on(f: &Fixture, parents: Vec<CommitId>, author: &str) -> CommitId {
        let store = f.repository.objects();
        let tree = store.get_commit(&f.tip).unwrap().unwrap().tree();
        let commit = Commit::try_new(
            tree,
            parents,
            ChangeId::generate().unwrap(),
            human(author),
            2,
            "work",
        )
        .unwrap();
        store.put_commit(&commit).unwrap()
    }

    #[test]
    fn the_last_green_tip_follows_the_first_parents_of_main_only() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        // `main` is a merge of `tip` (first parent) and `head` (second parent): `head` is in its
        // ancestry but not on its line, so a pass on `head` is not a green tip.
        let main = commit_on(&f, vec![f.tip, f.head], "daniel");
        let on_second_parent = record(&f, "ana", None, CheckResult::Pass);
        let on_line = f
            .repository
            .record_check(f.tip, None, CheckResult::Pass, &human("ana"), &AllowAll)
            .unwrap();
        let found = f.repository.last_green_tip(&main, None).unwrap().unwrap();
        assert_eq!(found.number(), on_line.number());
        assert_ne!(found.number(), on_second_parent.number());
    }

    #[test]
    fn a_proof_of_one_entry_does_not_make_another_entry_of_the_same_writer_count() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let register = register();
        let metadata = f.repository.metadata_dir();
        let signed = record(&f, "ana", Some(f.tip), CheckResult::Pass);
        let signature = signature::sign(
            &Seeded::new(2),
            PayloadKind::LogEntry,
            signed.hash().unwrap().to_string().as_bytes(),
        )
        .unwrap();
        write_proof(
            &metadata,
            signed.number(),
            &ProofFile {
                actor: human("ana"),
                signature,
            },
        )
        .unwrap();
        // The same writer, the same key, the same words: only the entry differs, and a copy of
        // the proof file under its number is a replay.
        let replay = record(&f, "ana", Some(f.tip), CheckResult::Pass);
        std::fs::copy(
            proof_path(&metadata, signed.number()),
            proof_path(&metadata, replay.number()),
        )
        .unwrap();
        assert_eq!(
            counted(&f, RiskLevel::Zero, Some(&register)),
            [signed.number()]
        );
        assert_eq!(
            f.repository
                .last_green_tip(&f.tip, Some(&register))
                .unwrap()
                .map(|check| check.number()),
            None,
            "no run without a base has a proof"
        );
    }

    #[test]
    fn a_conflicted_commit_has_an_author_and_can_be_checked() {
        use crate::object::{Blob, FileMode, Kind, Tree, TreeEntry};
        let dir = TempDir::new();
        let f = fixture(&dir);
        let store = f.repository.objects();
        let tree_of = |name: &str| {
            let blob = store.put_blob(&Blob::new(name)).unwrap();
            let entry = TreeEntry::file(name, FileMode::File, blob).unwrap();
            store
                .put_tree(&Tree::try_new(vec![entry]).unwrap())
                .unwrap()
        };
        let base = store.get_commit(&f.tip).unwrap().unwrap().tree();
        let conflict = |parent: CommitId, author: &str| {
            let commit = crate::object::Commit2::try_new(
                vec![tree_of("a.txt"), base, tree_of("b.txt")],
                vec![parent],
                ChangeId::generate().unwrap(),
                human(author),
                2,
                "conflict",
            )
            .unwrap();
            CommitId::new(store.put_raw(Kind::Commit2, &commit.encode()).unwrap())
        };
        // A conflict of Zoe on the tip, a normal commit of Ana on it, and a conflict of Mia.
        let first = conflict(f.tip, "zoe");
        let second = commit_on(&f, vec![first], "ana");
        let head = conflict(second, "mia");
        let authors = f.repository.change_authors(&head, &f.tip).unwrap();
        for name in ["zoe", "ana", "mia"] {
            assert!(authors.contains(&human(name)), "{name}");
        }
        assert_eq!(authors.len(), 3);
        // A run on a conflicted commit is accepted like one on any commit.
        f.repository
            .record_check(head, None, CheckResult::Pass, &human("ana"), &AllowAll)
            .unwrap();
    }

    #[test]
    fn the_authors_of_the_change_matter_only_at_the_levels_two_and_three() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let own = record(&f, "ana", Some(f.tip), CheckResult::Pass);
        // `tip` is the head itself: no change to speak of, an error at the levels 2 and 3 only.
        let count = |level| {
            f.repository
                .counted_checks(f.number, &f.head, &f.head, level, None)
        };
        assert!(matches!(
            count(RiskLevel::Two),
            Err(CheckError::TipReachesHead { .. })
        ));
        assert_eq!(
            count(RiskLevel::Zero)
                .unwrap()
                .iter()
                .map(Check::number)
                .collect::<Vec<_>>(),
            [own.number()]
        );
    }

    /// The register with the agent `bot`, registered under the sponsor Mia (key of seed 3); the
    /// key of the agent is the seed 4.
    fn register_with_bot() -> Register {
        use crate::register::{sponsorship_payload, Identity, Role};
        let bot = Identity::of(&Actor::agent("bot", "m", "s", "mia").unwrap());
        let mia = Identity::human("mia").unwrap();
        let cosign = signature::sign(
            &Seeded::new(3),
            PayloadKind::Sponsorship,
            &sponsorship_payload(&bot, Role::Member, &mia),
        )
        .unwrap();
        let mut register = register();
        register
            .add_agent(bot, Role::Member, mia, Seeded::new(4).public(), cosign)
            .unwrap();
        register
    }

    #[test]
    fn a_registered_agent_counts_through_the_register_and_its_own_proof() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let register = register_with_bot();
        let metadata = f.repository.metadata_dir();
        // The register says that Mia, the opener, sponsors the agent.
        let bot = Actor::agent("bot", "m", "s", "mia").unwrap();
        let sign = |entry: &Entry, seed: u8, actor: &Actor| {
            let signature = signature::sign(
                &Seeded::new(seed),
                PayloadKind::LogEntry,
                entry.hash().unwrap().to_string().as_bytes(),
            )
            .unwrap();
            write_proof(
                &metadata,
                entry.number(),
                &ProofFile {
                    actor: actor.clone(),
                    signature,
                },
            )
            .unwrap();
        };
        let write = |actor: &Actor| {
            f.repository
                .record_check(f.head, Some(f.tip), CheckResult::Pass, actor, &AllowAll)
                .unwrap()
        };
        let good = write(&bot);
        sign(&good, 4, &bot);
        // The key of the sponsor is not the key of the agent.
        let sponsor_key = write(&bot);
        sign(&sponsor_key, 3, &bot);
        // A proof of the same agent in another session is a proof of another actor.
        let other_session = Actor::agent("bot", "m", "s2", "mia").unwrap();
        let session = write(&other_session);
        sign(&session, 4, &bot);
        // A line that claims another sponsor than the register has no valid proof.
        let liar = Actor::agent("bot", "m", "s", "daniel").unwrap();
        let claim = write(&liar);
        sign(&claim, 4, &liar);
        assert_eq!(
            counted(&f, RiskLevel::Zero, Some(&register)),
            [good.number()]
        );
        // The agent is sponsored by the opener, so it is not independent at the levels 2 and 3.
        assert!(counted(&f, RiskLevel::Two, Some(&register)).is_empty());
    }

    #[test]
    fn the_last_green_tip_is_a_commit_of_main_and_counts_only_with_a_proof_of_a_writer() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let register = register();
        let metadata = f.repository.metadata_dir();
        // `main` stands at `old`; its history is `old`, then `tip`. `head` is not on it.
        let sign = |entry: &Entry, seed: u8, actor: &str| {
            let signature = signature::sign(
                &Seeded::new(seed),
                PayloadKind::LogEntry,
                entry.hash().unwrap().to_string().as_bytes(),
            )
            .unwrap();
            write_proof(
                &metadata,
                entry.number(),
                &ProofFile {
                    actor: human(actor),
                    signature,
                },
            )
            .unwrap();
        };
        let run = |commit: CommitId, base: Option<CommitId>, writer: &str, result| {
            f.repository
                .record_check(commit, base, result, &human(writer), &AllowAll)
                .unwrap()
        };
        let pass = CheckResult::Pass;
        let green = run(f.old, None, "ana", pass);
        sign(&green, 2, "ana");
        // Newer entries that must all be passed over in the enforcing mode:
        let unsigned = run(f.old, None, "daniel", pass);
        let wrong_key = run(f.old, None, "ana", pass);
        sign(&wrong_key, 3, "ana");
        let other_actor = run(f.old, None, "ana", pass);
        sign(&other_actor, 2, "daniel");
        let red = run(f.old, None, "ana", CheckResult::Fail);
        sign(&red, 2, "ana");
        let with_base = run(f.old, Some(f.tip), "ana", pass);
        sign(&with_base, 2, "ana");
        let off_main = run(f.head, None, "ana", pass);
        sign(&off_main, 2, "ana");
        let rerun_older = run(f.tip, None, "ana", pass);
        sign(&rerun_older, 2, "ana");
        let number = |tip: Option<Check>| tip.map(|check| check.number());
        assert_eq!(
            number(
                f.repository
                    .last_green_tip(&f.old, Some(&register))
                    .unwrap()
            ),
            Some(green.number())
        );
        // The open mode asks for no proof: the newest pass on the newest commit of `main` wins,
        // and neither the run on `head` nor the late rerun on the older tip displaces it.
        assert_eq!(
            number(f.repository.last_green_tip(&f.old, None).unwrap()),
            Some(other_actor.number())
        );
        assert!(unsigned.number() < wrong_key.number());
        // While `main` stands at `tip`, the run on `old` is not on it any more.
        assert_eq!(
            number(
                f.repository
                    .last_green_tip(&f.tip, Some(&register))
                    .unwrap()
            ),
            Some(rerun_older.number())
        );
    }
}
