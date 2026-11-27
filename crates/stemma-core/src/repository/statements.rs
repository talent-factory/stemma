//! The statements of a clone (ADR-025, ADR-009, STEMMA-290): the set in `statements/`, the
//! entries of the log that place its statements, and what `op verify` checks about both.
//!
//! The evidence about a pull request is signed statements, and the log records the act: the
//! entries `pr-open`, `pr-close`, `pr-approve`, and `check` name the hash of the statement that
//! they made (ADR-011). A statement of the set counts in this clone only through such an entry,
//! which gives it a number, a time, and an actor ([`Placed`]). [`Repository::evidence`] reads
//! the entries, places the statements, and folds them with the one function that a hub calls
//! for its set as well ([`evidence::fold`]).
//!
//! The reading is tolerant, as every reading of evidence is: an entry whose statement is
//! missing, is damaged, or says something else than the entry places nothing, so what it would
//! have been does not count. `stemma op verify` is the reader that reports it
//! ([`Repository::verify_statements`]).

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use super::Repository;
use crate::evidence::{self, Evidence, Merge, Placed};
use crate::oplog::{CheckResult, Entry, OpLogError, PullKind, Record};
use crate::protocol::statement::{Body, Checked, Closing, Statement, StatementHash, StatementKind};
use crate::refs::RefError;
use crate::register::{Identity, ProofError, Register};
use crate::statements::{SetError, StatementSet, STATEMENT_DIR};
use crate::Actor;

impl Repository {
    /// The set of the statements of this clone: the directory `statements/` of the metadata
    /// directory, with `tmp/` as its scratch. A caller that writes it holds the lock of the
    /// references (ADR-011).
    pub(crate) fn statement_set(&self) -> StatementSet {
        let metadata = self.metadata_dir();
        StatementSet::new(metadata.join(STATEMENT_DIR), metadata.join("tmp"))
    }

    /// What the statements of this clone add up to: the pull requests, the approvals, and the
    /// checks, folded from the statements that the entries of the log place.
    ///
    /// # Errors
    ///
    /// `register` is the register of an enforcing repository, by which an opening counts
    /// (see [`placed`](Self::placed)); `None` is the open mode, and it is also what a caller
    /// passes that must see every opening the log places, as the purge does.
    ///
    /// Returns [`EvidenceError::Log`] if the log cannot be read and [`EvidenceError::Set`] if a
    /// statement cannot be read for a failure of the file system. A statement that is missing
    /// or damaged is no error; it does not count.
    pub(crate) fn evidence(&self, register: Option<&Register>) -> Result<Evidence, EvidenceError> {
        let entries = self.oplog().entries().map_err(EvidenceError::Log)?;
        self.evidence_of(&entries, register)
            .map_err(EvidenceError::Set)
    }

    /// [`evidence`](Self::evidence) for entries that the caller read already.
    pub(crate) fn evidence_of(
        &self,
        entries: &[Entry],
        register: Option<&Register>,
    ) -> Result<Evidence, SetError> {
        let (placed, merges) = self.placed(entries, register)?;
        Ok(evidence::fold(&placed, &merges))
    }

    /// The statements that the entries place, and the merges that they record, both in the
    /// order of the entries. An entry places its statement if the set holds it, the strict
    /// reader accepts it, and it says what the lines of the entry say ([`disagreement`]).
    ///
    /// In an enforcing repository (`register` is `Some`) an opening is placed only if the
    /// register accepts its signature for the actor and the number of the entry
    /// ([`Register::verify_statement`]): an opening lets a change through, so one without a
    /// signature, or with the signature of a key that is not the opener's, makes no pull
    /// request. It is the reader's twin of the rule of `open_pull`. An approval and a check
    /// are placed as they are; the rules that count them ask the register.
    ///
    /// An entry that places nothing counts for nothing, with one exception: what says something
    /// against a change stays ([`held_back`]). A closing and a failing run are placed from the
    /// lines of the entry where their statement cannot be, as a stand-in
    /// ([`Placed::stand_in`]), so losing a file of the set never opens a pull request again
    /// and never takes a red tip away, in either mode. For the same reason a closing is placed
    /// whatever its signature, and one whose signature the register does not accept is a
    /// stand-in as well; `op verify` reports it. A stand-in does only what makes things
    /// stricter: the gate keeps the reservation of a pull request that it ended, and a failing
    /// run that stands in only makes its commit red (ADR-019).
    pub(super) fn placed(
        &self,
        entries: &[Entry],
        register: Option<&Register>,
    ) -> Result<(Vec<Placed>, Vec<Merge>), SetError> {
        let set = self.statement_set();
        let openings = openings_of(entries);
        let mut placed = Vec::new();
        let mut merges = Vec::new();
        for entry in entries {
            if let Some(merge) = merge_of(entry, &openings) {
                merges.push(merge);
            }
            let Some(hash) = entry.statement() else {
                continue;
            };
            let read = match set.get(&hash) {
                Ok(statement) => statement,
                Err(error @ SetError::Io { .. }) => return Err(error),
                Err(_) => None,
            };
            let read = read.filter(|statement| disagreement(entry, statement, &openings).is_none());
            let (number, time, actor) = (entry.number(), entry.time(), entry.actor().clone());
            match read {
                Some(statement) => {
                    let proven = || proof_of(register, &actor, number, &statement).is_ok();
                    match statement.body() {
                        Body::Opening(_) if !proven() => {}
                        Body::Closing(_) if !proven() => {
                            placed.push(Placed::stand_in(number, time, actor, statement));
                        }
                        _ => placed.push(Placed::new(number, time, actor, statement)),
                    }
                }
                None => {
                    if let Some(statement) = held_back(entry, &openings) {
                        placed.push(Placed::stand_in(number, time, actor, statement));
                    }
                }
            }
        }
        Ok((placed, merges))
    }

    /// Checks the statements of this clone and the entries that name them, for
    /// `stemma op verify` (ADR-011, ADR-025):
    ///
    /// - every file of the set is a statement that the strict reader accepts, under the name of
    ///   its hash, with a signature that verifies for the key that it names;
    /// - every entry that names a statement names one that the set holds, of the kind of the
    ///   entry, that says what the lines of the entry say;
    /// - in an enforcing repository (`register` is `Some`), the signature of every such
    ///   statement is one of a key that the register holds for the actor of the entry and that
    ///   counts for the number of the entry
    ///   ([`Register::verify_statement`]).
    ///
    /// Three things are no damage. A statement that no entry names counts for nothing; a writer
    /// that stopped between the statement and its entry leaves one, and so does a purge that
    /// replaced the entry by a stub (ADR-014). An unsigned statement in an enforcing repository
    /// proves nothing wrong, as a repository that left the open mode holds some: an opening, an
    /// approval, and a passing run without a signature do not count there, and a closing ends
    /// its pull request all the same, because it can only hold a change back; the report
    /// counts all of them. And an approval of a key that was retired or
    /// revoked since is void, which the recount shows. In the open mode (`register` is `None`)
    /// the actor is a claim and no key is asked.
    ///
    /// `entries` are the entries of the log, which the caller verified.
    ///
    /// # Errors
    ///
    /// Returns [`StatementCheckError`] with the first problem: of the set in the order of the
    /// hashes, then of the entries in the order of their numbers.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn verify_statements(
        &self,
        entries: &[Entry],
        register: Option<&Register>,
    ) -> Result<StatementsVerified, StatementCheckError> {
        self.require_oplog("verify the statements")
            .map_err(StatementCheckError::Refs)?;
        let set = self.statement_set();
        let mut held: HashMap<StatementHash, Statement> = HashMap::new();
        for hash in set.hashes().map_err(StatementCheckError::Set)? {
            let statement = set
                .get(&hash)
                .map_err(StatementCheckError::Set)?
                .ok_or(StatementCheckError::Vanished { hash })?;
            held.insert(hash, statement);
        }
        let openings = openings_of(entries);
        let mut named: Vec<StatementHash> = Vec::new();
        let mut unsigned = 0;
        for entry in entries {
            let Some(hash) = entry.statement() else {
                continue;
            };
            let number = entry.number();
            let statement = held
                .get(&hash)
                .ok_or(StatementCheckError::Missing { number, hash })?;
            if let Some(reason) = disagreement(entry, statement, &openings) {
                return Err(StatementCheckError::Disagrees { number, reason });
            }
            named.push(hash);
            let Some(register) = register else {
                continue;
            };
            match register.verify_statement(entry.actor(), statement, Some(number)) {
                Ok(_) => {}
                Err(ProofError::Unsigned) => unsigned += 1,
                Err(ProofError::KeyNoLongerCounts { .. })
                    if statement.kind() == StatementKind::Approval => {}
                Err(error) => return Err(StatementCheckError::Proof { number, error }),
            }
        }
        named.sort_unstable();
        named.dedup();
        Ok(StatementsVerified {
            statements: held.len(),
            unnamed: held.len() - named.len(),
            unsigned,
        })
    }
}

/// What the log says about the opening with the number of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Opened {
    /// The entry is a `pr-open` that names this statement: the local name of a pull request and
    /// its name everywhere.
    Named(StatementHash),
    /// The entry is a stub. A purge replaced it (ADR-014), and what it named cannot be told
    /// any more.
    Purged,
}

/// The opening behind each number that an entry `pr-close`, `pr-approve`, or `pr-merge` can
/// name: every entry `pr-open` that names a statement, and every stub.
fn openings_of(entries: &[Entry]) -> HashMap<u64, Opened> {
    entries
        .iter()
        .filter_map(|entry| {
            let opened = match entry.record() {
                Record::Stub(_) => Opened::Purged,
                Record::Pull(pull) if pull.kind() == PullKind::Open => {
                    Opened::Named(pull.statement()?)
                }
                _ => return None,
            };
            Some((entry.number(), opened))
        })
        .collect()
}

/// The merge that an entry `pr-merge` records, if its pull request has an opening that names a
/// statement.
fn merge_of(entry: &Entry, openings: &HashMap<u64, Opened>) -> Option<Merge> {
    let pull = entry.pull().filter(|pull| pull.kind() == PullKind::Merge)?;
    let Opened::Named(opening) = *openings.get(&pull.pull()?)? else {
        return None;
    };
    Some(Merge {
        pull: opening,
        number: entry.number(),
        time: entry.time(),
        by: entry.actor().clone(),
        commit: *pull.commit()?,
    })
}

/// Why `statement` is not what `entry` says it made, or `None` if the two agree: the kind is
/// the one of the entry, and every line of the entry says what the statement says. The entry
/// repeats the lines so that the log reads without the set (`op log`) and a purge finds the
/// commits that an entry names (ADR-014); the statement is the evidence, so a difference makes
/// the entry place nothing.
///
/// An entry names a pull request by the number of its `pr-open` entry and a statement by the
/// hash of its opening; `openings` joins the two. Where a purge replaced the opening by a stub,
/// the number cannot be compared any more and is taken as it is: the fold then knows no such
/// pull request, so nothing counts for it. The reviewers of an entry are actors and those of an
/// opening are identities, and they agree as sets of identities.
pub(crate) fn disagreement(
    entry: &Entry,
    statement: &Statement,
    openings: &HashMap<u64, Opened>,
) -> Option<Disagreement> {
    let names = |number: Option<u64>, pull: StatementHash| match number
        .and_then(|number| openings.get(&number))
    {
        Some(Opened::Named(opening)) => *opening == pull,
        Some(Opened::Purged) => true,
        None => false,
    };
    let agrees = match (entry.record(), statement.body()) {
        (Record::Pull(pull), Body::Opening(opening)) if pull.kind() == PullKind::Open => {
            let mut reviewers: Vec<Identity> = pull.reviewers().iter().map(Identity::of).collect();
            reviewers.sort_by_cached_key(ToString::to_string);
            pull.branch() == Some(opening.branch())
                && pull.commit() == Some(&opening.head())
                && pull.repairs().copied() == opening.repairs()
                && reviewers == opening.reviewers()
        }
        (Record::Pull(pull), Body::Closing(closing)) if pull.kind() == PullKind::Close => {
            names(pull.pull(), closing.pull())
        }
        (Record::Pull(pull), Body::Approval(approval)) if pull.kind() == PullKind::Approve => {
            names(pull.pull(), approval.pull()) && pull.commit() == Some(&approval.head())
        }
        (Record::Check(check), Body::Check(checked)) => {
            *check.commit() == checked.commit()
                && check.base().copied() == checked.base()
                && check.result() == checked.result()
        }
        _ => return Some(Disagreement::Kind),
    };
    (!agrees).then_some(Disagreement::Lines)
}

/// What an entry says against a change, as an unsigned statement made from the lines of the
/// entry: the closing of a pull request, and a failing run of a check. It stands in where the
/// statement that the entry names is missing, is damaged, or says something else, which
/// `op verify` reports. An opening, an approval, and a passing run let a change through, so
/// they count only with their statement.
///
/// The stand-in carries no signature, and it gives no more than the entry gave before the
/// statements, for each kind:
///
/// - A closing ends its pull request in both modes. The fold never counted a closing against
///   the register, and the entry `pr-close` had no proof.
/// - A failing run makes its commit red in both modes, and with that it pauses a change of
///   level 2 or 3. That is all it does where a register is asked: it does not move the
///   verified tip, it ends no row of invalidated runs of another change, and it opens no red
///   window for a declared repair, because each of those can help a change, and a stand-in
///   can be made by anyone who writes the log, under any actor. In the open mode, where every
///   run is a claim, it counts as any failure does. Whether a lost statement was signed
///   cannot be told any more, and keeping the red tip is the safe side.
fn held_back(entry: &Entry, openings: &HashMap<u64, Opened>) -> Option<Statement> {
    let body = match entry.record() {
        Record::Pull(pull) if pull.kind() == PullKind::Close => {
            match openings.get(&pull.pull()?)? {
                Opened::Named(opening) => Body::Closing(Closing::new(None, *opening)),
                Opened::Purged => return None,
            }
        }
        Record::Check(check) if check.result() == CheckResult::Fail => Body::Check(
            Checked::try_new(None, *check.commit(), check.base().copied(), check.result()).ok()?,
        ),
        _ => return None,
    };
    Some(Statement::unsigned(body))
}

/// Whether the signature of a statement counts for `actor` at `number`: always in the open mode
/// (`register` is `None`), where the actor is a claim, and in an enforcing repository if the
/// register accepts it ([`Register::verify_statement`]). The error is the reason of the
/// register; an unsigned statement is refused there as [`ProofError::Unsigned`].
pub(crate) fn proof_of(
    register: Option<&Register>,
    actor: &Actor,
    number: u64,
    statement: &Statement,
) -> Result<(), ProofError> {
    match register {
        None => Ok(()),
        Some(register) => register
            .verify_statement(actor, statement, Some(number))
            .map(|_| ()),
    }
}

/// How an entry and the statement that it names disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disagreement {
    /// The statement is of another kind than the entry: an opening under an entry `check`,
    /// for example.
    Kind,
    /// The statement is of the kind of the entry and says something else than its lines:
    /// another branch, commit, pull request, reviewer, base, or result.
    Lines,
}

impl fmt::Display for Disagreement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Kind => "the statement is of another kind than the entry",
            Self::Lines => "the statement says something else than the lines of the entry",
        })
    }
}

/// What [`Repository::verify_statements`] checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementsVerified {
    /// How many statements the set holds.
    pub statements: usize,
    /// How many of them no entry of the log names. They count for nothing.
    pub unnamed: usize,
    /// How many entries of an enforcing repository name a statement without a signature. An
    /// opening, an approval, and the result of a check among them do not count there; a
    /// closing still ends its pull request. Always 0 in the open mode.
    pub unsigned: usize,
}

/// Why the evidence of a clone cannot be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum EvidenceError {
    /// The log could not be read. See [`OpLogError`].
    Log(OpLogError),
    /// A statement could not be read for a failure of the file system.
    Set(SetError),
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Log(error) => error.fmt(f),
            Self::Set(error) => error.fmt(f),
        }
    }
}

impl Error for EvidenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Log(error) => Some(error),
            Self::Set(error) => Some(error),
        }
    }
}

/// A statement, or an entry that names one, that does not hold.
#[derive(Debug)]
#[non_exhaustive]
pub enum StatementCheckError {
    /// The repository is bare.
    Refs(RefError),
    /// The set cannot be read, or one of its files is no statement under the name of its hash.
    Set(SetError),
    /// A statement was listed and is gone: the set changed while it was read.
    Vanished {
        /// The hash of the statement.
        hash: StatementHash,
    },
    /// An entry names a statement that the set does not hold.
    Missing {
        /// The number of the entry.
        number: u64,
        /// The hash that it names.
        hash: StatementHash,
    },
    /// An entry and the statement that it names do not say the same.
    Disagrees {
        /// The number of the entry.
        number: u64,
        /// The reason.
        reason: Disagreement,
    },
    /// The register does not accept the signature of a statement for the actor of its entry.
    Proof {
        /// The number of the entry.
        number: u64,
        /// The reason.
        error: ProofError,
    },
}

impl fmt::Display for StatementCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refs(error) => error.fmt(f),
            Self::Set(error) => error.fmt(f),
            Self::Vanished { hash } => write!(
                f,
                "the statement {hash} was removed while the statements were read"
            ),
            Self::Missing { number, hash } => write!(
                f,
                "entry {number} names the statement {hash}, which the repository does not hold"
            ),
            Self::Disagrees { number, reason } => {
                write!(f, "statement of entry {number}: {reason}")
            }
            Self::Proof { number, error } => write!(f, "statement of entry {number}: {error}"),
        }
    }
}

impl Error for StatementCheckError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Refs(error) => Some(error),
            _ => None,
        }
    }
}

/// Entries and statements that are written past every rule of the writers, for the tests of the
/// readers: a log can be edited by hand, so the recount, the rule table, and `op verify` trust
/// no writer (CODING_STANDARDS, "A rule of the writer has its twin in the reader").
#[cfg(test)]
pub(crate) mod by_hand {
    use super::*;
    use crate::object::CommitId;
    use crate::oplog::{CheckEntry, OperationName, PullEntry};
    use crate::protocol::statement::{Approval, Opening};
    use crate::refs::BranchName;
    use crate::signature::Signer;
    use crate::AllowAll;

    /// The statement of `body`, signed by `signer`, or unsigned without one. Nothing asks whose
    /// key it is.
    pub(crate) fn statement(body: Body, signer: Option<&dyn Signer>) -> Statement {
        match signer {
            Some(signer) => Statement::sign(body, signer).unwrap(),
            None => Statement::unsigned(body),
        }
    }

    fn lock(repository: &Repository, actor: &Actor) -> crate::refs::WriteLock {
        let name = OperationName::try_new("by-hand").unwrap();
        let allowed = repository
            .authorize(actor, &AllowAll, &name, "by hand")
            .unwrap();
        repository.refs_store().lock_writer(&allowed).unwrap()
    }

    /// Writes an entry `check` of `actor` with the time `time` that names `statement`, which is
    /// put into the set.
    pub(crate) fn check_naming(
        repository: &Repository,
        actor: &Actor,
        time: i64,
        run: (CommitId, Option<CommitId>, CheckResult),
        statement: &Statement,
    ) -> Entry {
        let (commit, base, result) = run;
        let hash = repository.statement_set().put(statement).unwrap();
        let entry = CheckEntry::new(time, actor.clone(), commit, base, result).with_statement(hash);
        repository
            .append_check(&lock(repository, actor), &entry)
            .unwrap()
    }

    /// Writes the result of a check as an entry of `actor` with the time `time`, which the
    /// clock does not give, and a statement that `signer` signed (or nobody).
    pub(crate) fn check_at(
        repository: &Repository,
        actor: &Actor,
        time: i64,
        run: (CommitId, Option<CommitId>, CheckResult),
        signer: Option<&dyn Signer>,
    ) -> Entry {
        let (commit, base, result) = run;
        let checked = Checked::try_new(None, commit, base, result).unwrap();
        let statement = statement(Body::Check(checked), signer);
        check_naming(repository, actor, time, run, &statement)
    }

    /// Writes an entry `check` of `actor` that names a statement which was never put into the
    /// set: what a writer of the log makes who wants a stand-in (`held_back`).
    pub(crate) fn check_without_statement(
        repository: &Repository,
        actor: &Actor,
        run: (CommitId, Option<CommitId>, CheckResult),
    ) -> Entry {
        let (commit, base, result) = run;
        let checked = Checked::try_new(None, commit, base, result).unwrap();
        let hash = Statement::unsigned(Body::Check(checked)).hash();
        let time = super::super::now();
        let entry = CheckEntry::new(time, actor.clone(), commit, base, result).with_statement(hash);
        repository
            .append_check(&lock(repository, actor), &entry)
            .unwrap()
    }

    /// Writes the result of a check as an entry of `actor` and a statement that `signer` signed
    /// (or nobody).
    pub(crate) fn check(
        repository: &Repository,
        actor: &Actor,
        commit: CommitId,
        base: Option<CommitId>,
        result: CheckResult,
        signer: Option<&dyn Signer>,
    ) -> Entry {
        let time = super::super::now();
        check_at(repository, actor, time, (commit, base, result), signer)
    }

    /// Writes an entry of a pull request of `actor` that names `statement`.
    pub(crate) fn pull_naming(
        repository: &Repository,
        actor: &Actor,
        entry: PullEntry,
        statement: &Statement,
    ) -> Entry {
        let hash = repository.statement_set().put(statement).unwrap();
        let entry = entry.with_statement(hash).unwrap();
        repository
            .append_pull(&lock(repository, actor), &entry)
            .unwrap()
    }

    /// Writes a closing of the pull request `number` as an entry of `actor` and a statement
    /// that `signer` signed (or nobody).
    pub(crate) fn close(
        repository: &Repository,
        actor: &Actor,
        number: u64,
        signer: Option<&dyn Signer>,
    ) -> Entry {
        let closing = Closing::new(None, id_of(repository, number));
        let statement = statement(Body::Closing(closing), signer);
        let entry = PullEntry::close(super::super::now(), actor.clone(), number).unwrap();
        pull_naming(repository, actor, entry, &statement)
    }

    /// Writes the opening of a pull request for `branch` at `head` as an entry of `actor` and a
    /// statement that `signer` signed (or nobody), past the lease and every other rule.
    pub(crate) fn open(
        repository: &Repository,
        actor: &Actor,
        branch: &BranchName,
        head: CommitId,
        repairs: Option<CommitId>,
        signer: Option<&dyn Signer>,
    ) -> Entry {
        let opening = Opening::try_new(None, branch.clone(), head, Vec::new(), repairs).unwrap();
        let statement = statement(Body::Opening(opening), signer);
        let time = super::super::now();
        let entry = PullEntry::open(
            time,
            actor.clone(),
            branch.clone(),
            head,
            Vec::new(),
            repairs,
        )
        .unwrap();
        pull_naming(repository, actor, entry, &statement)
    }

    /// The name of the pull request `number` of this clone.
    pub(crate) fn id_of(repository: &Repository, number: u64) -> StatementHash {
        let entries = repository.oplog().entries().unwrap();
        match openings_of(&entries).get(&number) {
            Some(Opened::Named(opening)) => *opening,
            other => panic!("the pull request {number} has no opening: {other:?}"),
        }
    }

    /// Writes an approval of `head` for the pull request `number` as an entry of `actor` and a
    /// statement that `signer` signed (or nobody).
    pub(crate) fn approve(
        repository: &Repository,
        actor: &Actor,
        number: u64,
        head: CommitId,
        signer: Option<&dyn Signer>,
    ) -> Entry {
        let approval = Approval::new(None, id_of(repository, number), head);
        let statement = statement(Body::Approval(approval), signer);
        let entry = PullEntry::approve(super::super::now(), actor.clone(), number, head).unwrap();
        pull_naming(repository, actor, entry, &statement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::CommitId;
    use crate::oplog::{CheckEntry, PullEntry};
    use crate::protocol::statement::Opening;
    use crate::refs::BranchName;
    use crate::repository::InitOptions;
    use crate::testutil::TempDir;
    use crate::AllowAll;

    fn daniel() -> Actor {
        Actor::human("daniel").unwrap()
    }

    fn repository(dir: &TempDir) -> (Repository, CommitId) {
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &daniel(), &AllowAll).unwrap();
        let commit = repository.refs().working_copy().unwrap();
        (repository, commit)
    }

    #[test]
    fn the_entries_of_an_older_format_are_read_and_are_no_evidence() {
        let dir = TempDir::new();
        let (repository, commit) = repository(&dir);
        let branch = BranchName::try_new("feature").unwrap();
        // The entries as a repository of the formats 9 to 15 holds them: without a statement.
        let open = PullEntry::open(1, daniel(), branch, commit, vec![], None).unwrap();
        let number = repository.oplog().append_pull(&open).unwrap().number();
        let approve = PullEntry::approve(2, Actor::human("mia").unwrap(), number, commit).unwrap();
        repository.oplog().append_pull(&approve).unwrap();
        let check = CheckEntry::new(3, daniel(), commit, None, CheckResult::Pass);
        repository.oplog().append_check(&check).unwrap();
        // They are entries of the chain, and `op verify` finds nothing wrong with them.
        let entries = repository.oplog().verify().expect("the chain holds");
        assert_eq!(entries.len(), 4);
        let verified = repository.verify_statements(&entries, None).unwrap();
        assert_eq!(
            verified,
            StatementsVerified {
                statements: 0,
                unnamed: 0,
                unsigned: 0
            }
        );
        // The fold reads statements only: the pull request is opened again, the check is
        // recorded again.
        let evidence = repository.evidence(None).unwrap();
        assert_eq!(evidence, Evidence::default());
        assert!(repository.pull_requests(None).unwrap().is_empty());
        assert!(repository.checks().unwrap().is_empty());
        assert!(repository.last_green_tip(&commit, None).unwrap().is_none());
    }

    #[test]
    fn an_entry_places_its_statement_only_if_the_two_say_the_same() {
        let dir = TempDir::new();
        let (repository, commit) = repository(&dir);
        let branch = BranchName::try_new("feature").unwrap();
        let opening = |name: &str| {
            let branch = BranchName::try_new(name).unwrap();
            Statement::unsigned(Body::Opening(
                Opening::try_new(None, branch, commit, Vec::new(), None).unwrap(),
            ))
        };
        let entry = || PullEntry::open(1, daniel(), branch.clone(), commit, vec![], None).unwrap();
        // The entry says `feature`, and its statement says `other`.
        let wrong = by_hand::pull_naming(&repository, &daniel(), entry(), &opening("other"));
        // The entry is an opening, and its statement is the result of a check.
        let checked = Checked::try_new(None, commit, None, CheckResult::Pass).unwrap();
        let kind = by_hand::pull_naming(
            &repository,
            &daniel(),
            entry(),
            &Statement::unsigned(Body::Check(checked)),
        );
        assert_eq!(repository.evidence(None).unwrap(), Evidence::default());
        let entries = repository.oplog().verify().unwrap();
        let error = repository.verify_statements(&entries, None).unwrap_err();
        assert!(
            matches!(error, StatementCheckError::Disagrees { number, .. } if number == wrong.number()),
            "{error:?}"
        );
        assert!(error.to_string().contains("says something else"), "{error}");
        assert!(kind.number() > wrong.number());
        // An entry and a statement that agree make the pull request.
        let right = by_hand::pull_naming(&repository, &daniel(), entry(), &opening("feature"));
        let pulls = repository.pull_requests(None).unwrap();
        assert_eq!(pulls.len(), 1);
        assert_eq!(pulls[0].number(), right.number());
        assert_eq!(pulls[0].id(), opening("feature").hash());
    }

    #[test]
    fn a_statement_that_no_entry_names_and_a_name_that_is_no_hash_are_told_apart() {
        let dir = TempDir::new();
        let (repository, commit) = repository(&dir);
        let checked = Checked::try_new(None, commit, None, CheckResult::Pass).unwrap();
        let hash = repository
            .statement_set()
            .put(&Statement::unsigned(Body::Check(checked)))
            .unwrap();
        let entries = repository.oplog().verify().unwrap();
        // A writer that stopped before its entry leaves this: no damage, and no evidence.
        let verified = repository.verify_statements(&entries, None).unwrap();
        assert_eq!((verified.statements, verified.unnamed), (1, 1));
        assert!(repository.checks().unwrap().is_empty());
        // A file that is no statement is damage, whether an entry names it or not.
        let set = repository.statement_set();
        std::fs::write(set.dir().join("notes"), "x").unwrap();
        let error = repository.verify_statements(&entries, None).unwrap_err();
        assert!(
            matches!(error, StatementCheckError::Set(SetError::UnexpectedFile(_))),
            "{error:?}"
        );
        std::fs::remove_file(set.dir().join("notes")).unwrap();
        std::fs::write(set.path_of(&hash), "not a statement").unwrap();
        let error = repository.verify_statements(&entries, None).unwrap_err();
        assert!(
            matches!(error, StatementCheckError::Set(SetError::Malformed { .. })),
            "{error:?}"
        );
    }
}
