//! The operations on the evidence of a check (ADR-019, ADR-025, STEMMA-240, STEMMA-290).
//!
//! The result of a check is a signed statement ([`Checked`]), and the log records the act:
//! [`record_check`](Repository::record_check) makes the statement, puts it into the set under
//! the lock of the references, and writes the entry `check` that names its hash, as every entry
//! that moves no pointer. The statement says which commit was tested, the tip of `main` it was
//! merged onto, and the result; the entry says who ran the check and when. The writer asks the
//! authorizer for the operation `check`, and in an enforcing repository for a signature of an
//! active key of the actor, and nothing more about the writer: whether a result counts for a
//! change is decided when it is read, by [`counted_checks`](Repository::counted_checks), because
//! it depends on the pull request, its authors, and its risk level, none of which the statement
//! holds. The last green tip is read by [`last_green_tip`](Repository::last_green_tip).

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use super::pull::{make_statement, NotSigned};
use super::statements::{proof_of, EvidenceError};
use super::{now, Repository};
use crate::check::{self, Check};
use crate::conflict::AnyCommit;
use crate::history::{self, HistoryError, Parents};
use crate::object::{CommitId, Kind};
use crate::oplog::{CheckEntry, CheckResult, Entry, OpLogError, OperationName, CHECK};
use crate::policy::RiskLevel;
use crate::protocol::record::SealError;
use crate::protocol::statement::{Body, Checked};
use crate::refs::RefError;
use crate::register::{ProofError, Register};
use crate::signature::Signer;
use crate::statements::SetError;
use crate::store::StoreError;
use crate::{Actor, Authorizer};

impl Repository {
    /// Records the result of a check run by `actor`: `commit` was tested, merged onto `base` (the
    /// tip of `main` of that test), or run as it is for `None`, and the run has `result`.
    ///
    /// The result is a signed statement ([`Checked`]), and the entry `check` names its hash. One
    /// statement holds one base, so a run against another tip is another call. Where the
    /// repository has a `register`, `signer` signs the statement with a key that the register
    /// holds for `actor` as an active key, and the signature is checked before anything is
    /// written; a result counts by that signature ([`counted_checks`](Self::counted_checks)). In
    /// open mode there is no register, and the statement is unsigned. The signature is made
    /// before the lock is taken, because a key with a passphrase may ask a person.
    ///
    /// # Errors
    ///
    /// Returns [`CheckError::Denied`] if the authorizer refuses, [`CheckError::BaseIsCommit`] if
    /// `base` is `commit` (a run merged onto itself), [`CheckError::SignatureRequired`] for a
    /// register and no signer, [`CheckError::Sign`], [`CheckError::Proof`], and
    /// [`CheckError::KeyNotActive`] for a signature that cannot be made or that the register
    /// does not accept, and [`CheckError::NoSuchCommit`] if the store has no commit of that ID.
    /// Nothing is written for any of them. [`CheckError::Refs`], [`CheckError::Store`],
    /// [`CheckError::Statements`], and [`CheckError::Log`] are a failure of the lock, the
    /// store, the set of statements, or the log; a failure of the log can leave the statement
    /// without its entry, which counts for nothing.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    #[allow(clippy::too_many_arguments)]
    pub fn record_check(
        &self,
        commit: CommitId,
        base: Option<CommitId>,
        result: CheckResult,
        actor: &Actor,
        register: Option<&Register>,
        signer: Option<&dyn Signer>,
        authorizer: &dyn Authorizer,
    ) -> Result<Entry, CheckError> {
        self.require_oplog("record a check")?;
        let name = OperationName::try_new(CHECK).expect("`check` is an operation name");
        let target = commit.to_string();
        let allowed = self
            .authorize(actor, authorizer, &name, &target)
            .map_err(|_| CheckError::Denied {
                operation: CHECK.to_owned(),
                target,
            })?;
        let checked = Checked::try_new(None, commit, base, result)
            .map_err(|_| CheckError::BaseIsCommit { commit })?;
        let statement = make_statement(Body::Check(checked), actor, register, signer)?;
        let lock = self.refs_store().lock_writer(&allowed)?;
        for named in std::iter::once(commit).chain(base) {
            match self.objects().kind_of(&named.object_id())? {
                Some(Kind::Commit | Kind::Commit2) => {}
                Some(Kind::Blob | Kind::Tree) | None => {
                    return Err(CheckError::NoSuchCommit { commit: named });
                }
            }
        }
        let hash = self.statement_set().put(&statement)?;
        let entry =
            CheckEntry::new(now(), actor.clone(), commit, base, result).with_statement(hash);
        Ok(self.append_check(&lock, &entry)?)
    }

    /// The checks that the statements tell of, in the order of the entries that name them (see
    /// [`evidence::fold`](crate::evidence::fold)). An entry `check` of a repository format
    /// before 16 names no statement and is not among them (ADR-011).
    ///
    /// # Errors
    ///
    /// Returns [`CheckError::Log`] if the log cannot be read and [`CheckError::Statements`] if
    /// a statement cannot be read for a failure of the file system.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn checks(&self) -> Result<Vec<Check>, CheckError> {
        self.require_oplog("read the checks")?;
        Ok(self.evidence()?.checks)
    }

    /// Every check that an enforcing repository accepts: all of them in the open mode
    /// (`register` is `None`, the writer is a claim), and in an enforcing one those whose
    /// statement carries a signature of the writer that the register accepts, as in
    /// [`counted_checks`](Self::counted_checks). A run that fails counts as well as one that
    /// passes: an unproven failure must not pause `main`.
    ///
    /// # Errors
    ///
    /// Returns [`CheckError::Log`] if the log cannot be read and [`CheckError::Statements`] if
    /// a statement cannot be read for a failure of the file system.
    pub(super) fn proven_checks(
        &self,
        register: Option<&Register>,
    ) -> Result<Vec<Check>, CheckError> {
        let mut found = self.checks()?;
        found.retain(|check| has_proof(register, check));
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
    /// - is, in an enforcing repository (`register` is `Some`), a statement with a signature that
    ///   the register accepts for the writer and the number of the entry that names it
    ///   ([`Register::verify_statement`]). An unsigned statement, and a signature of a key that
    ///   is not the writer's or no longer counts, does not count and is not reported here;
    ///   `stemma op verify` checks the statements and the entries that name them.
    ///
    /// `register` is the register of the base commit of the pull request, the one that counts for
    /// the signatures (ADR-003); at the levels 0 and 1 the results may have other bases, and the
    /// caller decides which register is the right one for them. `None` is the open mode, where
    /// the actor is a claim and the statement is unsigned.
    ///
    /// # Errors
    ///
    /// Returns [`CheckError::NoSuchPull`] for a number that no opening has,
    /// [`CheckError::NotTheHead`] if `commit` is not the head that the opening names,
    /// [`CheckError::TipReachesHead`] at the levels 2 and 3 if `tip` is `commit` or descends from
    /// it (the authors of the change would be none), [`CheckError::History`] at these levels if
    /// the commits cannot be walked, [`CheckError::Statements`] if a statement cannot be read
    /// for a failure of the file system, and [`CheckError::Log`] if the log cannot be read.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn counted_checks(
        &self,
        pull: u64,
        commit: &CommitId,
        tip: &CommitId,
        level: RiskLevel,
        register: Option<&Register>,
    ) -> Result<Vec<Check>, CheckError> {
        self.require_oplog("read the checks")?;
        let evidence = self.evidence()?;
        let pull = evidence
            .pulls
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
        for found in &evidence.checks {
            if found.commit() == commit
                && found.passed()
                && check::applies_at(level, found, tip)
                && check::counts(level, found, pull.opener(), &authors, register)
                && has_proof(register, found)
            {
                counted.push(found.clone());
            }
        }
        Ok(counted)
    }

    /// The last green tip (ADR-019): the newest commit of `main` that a passing run without a
    /// base covered (see [`check::last_green_tip`]). `main_tip` is the current tip of `main`; its
    /// first-parent history is `main`, and a run on a commit outside it does not count.
    ///
    /// In an enforcing repository (`register` is `Some`, the register of the current tip of
    /// `main`) only a run counts whose statement carries a signature of the writer that the
    /// register accepts, as in [`counted_checks`](Self::counted_checks); otherwise any actor
    /// could write the tip. `None` is the open mode, where the actor is a claim and the statement
    /// is unsigned; it must not be passed for an enforcing repository, as the signature would
    /// then not be asked for.
    ///
    /// # Errors
    ///
    /// Returns [`CheckError::Log`] if the log cannot be read, [`CheckError::History`] if the
    /// history of `main` cannot be walked, and [`CheckError::Statements`] if a statement cannot
    /// be read for a failure of the file system.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn last_green_tip(
        &self,
        main_tip: &CommitId,
        register: Option<&Register>,
    ) -> Result<Option<Check>, CheckError> {
        self.require_oplog("read the checks")?;
        let main: Vec<CommitId> =
            history::ancestors(self.objects(), *main_tip, Parents::First, None)?
                .into_iter()
                .map(|(id, _)| id)
                .collect();
        let mut candidates = Vec::new();
        for found in self.checks()? {
            if found.base().is_none()
                && found.passed()
                && main.contains(found.commit())
                && has_proof(register, &found)
            {
                candidates.push(found);
            }
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
}

/// Whether the statement of a check proves its writer: always in the open mode (`register` is
/// `None`), and in an enforcing repository if the register accepts the signature for the
/// writer and the number of the entry that names the statement.
fn has_proof(register: Option<&Register>, check: &Check) -> bool {
    proof_of(register, check.writer(), check.number(), check.statement()).is_ok()
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
    /// The repository has a register, so the statement of a check needs a signature, and the
    /// caller gave no signer.
    SignatureRequired,
    /// The key of the signature is retired and signs nothing new. A revoked key is refused as
    /// [`Proof`](Self::Proof).
    KeyNotActive,
    /// The statement could not be signed, or the signer signed with another key than the one
    /// it named.
    Sign(SealError),
    /// The register does not accept the signature of the statement for the actor.
    Proof(Box<ProofError>),
    /// The statements have no opening with this number.
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
    /// The set of statements could not be read or written. Nothing was written to the log.
    Statements(SetError),
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
            Self::NoSuchPull { number } => write!(f, "there is no pull request {number}"),
            Self::SignatureRequired => {
                f.write_str("the repository has an actor register, so the statement must be signed")
            }
            Self::KeyNotActive => f.write_str(
                "the key is not active and signs no new statement; sign with an active key",
            ),
            Self::Sign(error) => write!(f, "cannot sign the statement: {error}"),
            Self::Proof(error) => {
                write!(f, "the register does not accept the statement: {error}")
            }
            Self::Refs(error) => error.fmt(f),
            Self::Log(error) => error.fmt(f),
            Self::Store(error) => error.fmt(f),
            Self::History(error) => error.fmt(f),
            Self::Statements(error) => error.fmt(f),
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
            Self::Statements(error) => Some(error),
            Self::Sign(error) => Some(error),
            Self::Proof(error) => Some(error.as_ref()),
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

impl From<NotSigned> for CheckError {
    fn from(error: NotSigned) -> Self {
        match error {
            NotSigned::SignatureRequired => Self::SignatureRequired,
            NotSigned::KeyNotActive => Self::KeyNotActive,
            NotSigned::Sign(error) => Self::Sign(error),
            NotSigned::Proof(error) => Self::Proof(error),
        }
    }
}

impl From<SetError> for CheckError {
    fn from(error: SetError) -> Self {
        Self::Statements(error)
    }
}

impl From<EvidenceError> for CheckError {
    fn from(error: EvidenceError) -> Self {
        match error {
            EvidenceError::Log(error) => Self::Log(error),
            EvidenceError::Set(error) => Self::Statements(error),
        }
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
    use crate::repository::statements::by_hand;
    use crate::repository::InitOptions;
    use crate::signature::{self, PayloadKind, PublicKey, SignError, Signature, NAMESPACE};
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
            .open_pull(&feature, &human("mia"), vec![], None, None, None, &AllowAll)
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
            .record_check(f.head, base, result, &human(writer), None, None, &AllowAll)
            .unwrap()
    }

    /// The result of a run on the head as an entry of `writer` whose statement the key of `seed`
    /// signed, written past the rules of the writer.
    fn signed_run(
        f: &Fixture,
        writer: &Actor,
        seed: u8,
        base: Option<CommitId>,
        result: CheckResult,
    ) -> Entry {
        by_hand::check(
            &f.repository,
            writer,
            f.head,
            base,
            result,
            Some(&Seeded::new(seed)),
        )
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
            "stemma-repository 16\n"
        );
        let found = f.repository.checks().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].number(), entry.number());
        // The entry names the statement that the set holds, and the statement is the evidence.
        let hash = check.statement().expect("the entry names its statement");
        assert_eq!(entry.statement(), Some(hash));
        let statement = f.repository.statement_set().get(&hash).unwrap().unwrap();
        assert_eq!(found[0].statement(), &statement);
        assert!(!statement.is_signed(), "the open mode has no key");
        assert_eq!(statement.body().repository(), None);
        assert_eq!(statement.body().commits(), [f.head, f.tip]);
        f.repository.oplog().verify().expect("the chain holds");
    }

    #[test]
    fn a_check_refuses_before_anything_is_written() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let before = f.repository.oplog().entries().unwrap().len();
        let not_stored = CommitId::new(crate::object::Blob::new("not stored").id().object_id());
        let run = |commit, base, authorizer: &dyn Authorizer| {
            f.repository.record_check(
                commit,
                base,
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                authorizer,
            )
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
            .record_check(
                f.head,
                Some(f.tip),
                CheckResult::Pass,
                &agent,
                None,
                None,
                &AllowAll,
            )
            .unwrap();
        assert!(counted(&f, RiskLevel::Two, None).is_empty());
        assert_eq!(counted(&f, RiskLevel::One, None), [entry.number()]);
    }

    #[test]
    fn in_an_enforcing_repository_a_result_counts_only_with_a_signature_of_its_writer() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let register = register();
        let pass = CheckResult::Pass;
        // The writer signs with a key that the register holds for it.
        let signed = f
            .repository
            .record_check(
                f.head,
                Some(f.tip),
                pass,
                &human("ana"),
                Some(&register),
                Some(&Seeded::new(2)),
                &AllowAll,
            )
            .unwrap();
        // A statement of the open mode, and one that the key of another actor signed.
        let unsigned = record(&f, "daniel", Some(f.tip), pass);
        let forged = signed_run(&f, &human("mia"), 2, Some(f.tip), pass);
        assert_eq!(
            counted(&f, RiskLevel::Zero, Some(&register)),
            [signed.number()]
        );
        assert_eq!(
            counted(&f, RiskLevel::Two, Some(&register)),
            [signed.number()]
        );
        // The open mode asks for no signature: Ana and Daniel count, Mia opened the change.
        assert_eq!(
            counted(&f, RiskLevel::Two, None),
            [signed.number(), unsigned.number()]
        );
        assert_eq!(
            counted(&f, RiskLevel::Zero, None),
            [signed.number(), unsigned.number(), forged.number()]
        );
        // An entry whose statement is no statement, and one whose statement is gone, do not
        // count and do not fail.
        let set = f.repository.statement_set();
        let garbage = signed_run(&f, &human("ana"), 2, Some(f.old), pass);
        std::fs::write(
            set.path_of(&garbage.statement().unwrap()),
            "not a statement",
        )
        .unwrap();
        let gone = signed_run(&f, &human("ana"), 2, None, pass);
        std::fs::remove_file(set.path_of(&gone.statement().unwrap())).unwrap();
        assert_eq!(
            counted(&f, RiskLevel::Zero, Some(&register)),
            [signed.number()]
        );
        let listed: Vec<u64> = f
            .repository
            .checks()
            .unwrap()
            .iter()
            .map(Check::number)
            .collect();
        assert_eq!(
            listed,
            [signed.number(), unsigned.number(), forged.number()]
        );
    }

    #[test]
    fn an_enforcing_repository_refuses_a_check_that_is_not_signed_by_its_writer() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let register = register();
        let before = f.repository.oplog().entries().unwrap().len();
        let run = |writer: &str, signer: Option<&dyn Signer>| {
            f.repository.record_check(
                f.head,
                Some(f.tip),
                CheckResult::Pass,
                &human(writer),
                Some(&register),
                signer,
                &AllowAll,
            )
        };
        assert!(matches!(
            run("ana", None),
            Err(CheckError::SignatureRequired)
        ));
        // The key of Mia does not sign for Ana, and a stranger has no entry in the register.
        let error = run("ana", Some(&Seeded::new(3))).unwrap_err();
        assert!(matches!(error, CheckError::Proof(_)), "{error:?}");
        let error = run("zoe", Some(&Seeded::new(2))).unwrap_err();
        assert!(matches!(error, CheckError::Proof(_)), "{error:?}");
        assert_eq!(f.repository.oplog().entries().unwrap().len(), before);
        assert!(
            f.repository.statement_set().hashes().unwrap().len() == 1,
            "only the opening of the fixture is in the set"
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
                None,
                None,
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
            .record_check(
                f.tip,
                None,
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
            .unwrap();
        let found = f.repository.last_green_tip(&main, None).unwrap().unwrap();
        assert_eq!(found.number(), on_line.number());
        assert_ne!(found.number(), on_second_parent.number());
    }

    #[test]
    fn a_failing_run_stands_without_its_statement_and_a_passing_one_does_not() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let register = register();
        let set = f.repository.statement_set();
        let failed = signed_run(&f, &human("ana"), 2, None, CheckResult::Fail);
        let passed = signed_run(&f, &human("ana"), 2, Some(f.tip), CheckResult::Pass);
        for entry in [&failed, &passed] {
            std::fs::remove_file(set.path_of(&entry.statement().unwrap())).unwrap();
        }
        // A failure can only hold a change back, so it is read from the lines of its entry.
        let listed: Vec<(u64, bool)> = f
            .repository
            .checks()
            .unwrap()
            .iter()
            .map(|check| (check.number(), check.passed()))
            .collect();
        assert_eq!(listed, [(failed.number(), false)]);
        // It carries no signature: it counts in the open mode and not in an enforcing one,
        // where a failure without the signature of its writer must not pause `main`.
        let proven = |register| -> Vec<u64> {
            f.repository
                .proven_checks(register)
                .unwrap()
                .iter()
                .map(Check::number)
                .collect()
        };
        assert_eq!(proven(None), [failed.number()]);
        assert!(proven(Some(&register)).is_empty());
        assert!(counted(&f, RiskLevel::Zero, None).is_empty());
    }

    #[test]
    fn a_statement_counts_for_the_writer_whose_key_signed_it_and_for_nobody_else() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let register = register();
        let pass = CheckResult::Pass;
        let signed = signed_run(&f, &human("ana"), 2, Some(f.tip), pass);
        let hash = signed.statement().unwrap();
        let statement = f.repository.statement_set().get(&hash).unwrap().unwrap();
        // An entry of Daniel that names the statement of Ana is a replay: the statement names
        // no actor, and the key is not Daniel's.
        let replay = by_hand::check_naming(
            &f.repository,
            &human("daniel"),
            1,
            (f.head, Some(f.tip), pass),
            &statement,
        );
        assert_eq!(replay.statement(), Some(hash));
        assert_eq!(
            counted(&f, RiskLevel::Zero, Some(&register)),
            [signed.number()]
        );
        // An entry that names the statement and says something else places nothing, in either
        // mode: the statement is the evidence, not the lines of the entry.
        let other_words = by_hand::check_naming(
            &f.repository,
            &human("ana"),
            1,
            (f.head, Some(f.old), pass),
            &statement,
        );
        for mode in [None, Some(&register)] {
            assert!(
                !counted(&f, RiskLevel::Zero, mode).contains(&other_words.number()),
                "{}",
                mode.is_some()
            );
        }
        assert_eq!(
            f.repository
                .last_green_tip(&f.tip, Some(&register))
                .unwrap()
                .map(|check| check.number()),
            None,
            "no run without a base is signed"
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
            .record_check(
                head,
                None,
                CheckResult::Pass,
                &human("ana"),
                None,
                None,
                &AllowAll,
            )
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
    fn a_registered_agent_counts_through_the_register_and_its_own_key() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let register = register_with_bot();
        // The register says that Mia, the opener, sponsors the agent.
        let bot = Actor::agent("bot", "m", "s", "mia").unwrap();
        let write =
            |actor: &Actor, seed: u8| signed_run(&f, actor, seed, Some(f.tip), CheckResult::Pass);
        let good = write(&bot, 4);
        // The key of the sponsor is not the key of the agent.
        write(&bot, 3);
        // The same agent in another session is the same identity, and its key signs for it.
        let other_session = Actor::agent("bot", "m", "s2", "mia").unwrap();
        let session = write(&other_session, 4);
        // A line that claims another sponsor than the register has no valid signature.
        let liar = Actor::agent("bot", "m", "s", "daniel").unwrap();
        write(&liar, 4);
        assert_eq!(
            counted(&f, RiskLevel::Zero, Some(&register)),
            [good.number(), session.number()]
        );
        // The agent is sponsored by the opener, so it is not independent at the levels 2 and 3.
        assert!(counted(&f, RiskLevel::Two, Some(&register)).is_empty());
    }

    #[test]
    fn the_last_green_tip_is_a_commit_of_main_and_counts_only_with_a_signature_of_a_writer() {
        let dir = TempDir::new();
        let f = fixture(&dir);
        let register = register();
        // `main` stands at `old`; its history is `old`, then `tip`. `head` is not on it.
        let run = |commit: CommitId, base: Option<CommitId>, writer: &str, seed, result| {
            let signer = Seeded::new(seed);
            by_hand::check(
                &f.repository,
                &human(writer),
                commit,
                base,
                result,
                Some(&signer),
            )
        };
        let pass = CheckResult::Pass;
        let green = run(f.old, None, "ana", 2, pass);
        // Newer entries that must all be passed over in the enforcing mode:
        let unsigned = by_hand::check(&f.repository, &human("daniel"), f.old, None, pass, None);
        let wrong_key = run(f.old, None, "ana", 3, pass);
        let other_actor = run(f.old, None, "daniel", 2, pass);
        run(f.old, None, "ana", 2, CheckResult::Fail);
        run(f.old, Some(f.tip), "ana", 2, pass);
        run(f.head, None, "ana", 2, pass);
        let rerun_older = run(f.tip, None, "ana", 2, pass);
        let number = |tip: Option<Check>| tip.map(|check| check.number());
        assert_eq!(
            number(
                f.repository
                    .last_green_tip(&f.old, Some(&register))
                    .unwrap()
            ),
            Some(green.number())
        );
        // The open mode asks for no signature: the newest pass on the newest commit of `main`
        // wins, and neither the run on `head` nor the late rerun on the older tip displaces it.
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
