//! The second entry of the accepting operation (ADR-025): [`Repository::accept_statement`] takes
//! a signed statement and answers with the statement as the hub holds it, with the lines of the
//! hub, or with a typed refusal. Beside it stand the readers of a range of the statements and of
//! the record ([`held_in`], [`record_in`]), which answer the two reading requests, and the
//! selection of the statements that a fetch brings into a clone ([`statements_of_open_pulls`]).
//!
//! The rules are the ones of the local writer of phase 4, asked against the register at the tip
//! of `main` on the hub; they stand in the documentation of the method, which is the public one.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use super::{AcceptContext, AcceptError, Lock, Record, RecordDestination, RecordStep};
use crate::allowed::Allowed;
use crate::evidence::{self, Evidence, Merge, Placed};
use crate::history::{self, HistoryError, Parents};
use crate::lease;
use crate::object::CommitId;
use crate::oplog::{OperationName, CHECK, PR_APPROVE, PR_CLOSE, PR_OPEN};
use crate::protocol::held::{
    read_held, HeldParseError, HeldStatement, HubLines, Range, StatementRefusal,
};
use crate::protocol::record::{read_record, RecordEntry, RecordParseError, SealError};
use crate::protocol::statement::{Body, Statement, StatementHash};
use crate::protocol::{ErrorMessage, QualifiedName, Reason};
use crate::pull::{self, TARGET};
use crate::refs::{BranchName, RefKind, WriteLock};
use crate::register::{
    Identity, IdentityKind, Mode, ModeError, Register, RegisterAuthorizer, Verified,
};
use crate::repository::{PolicyError, Repository};
use crate::signature::PublicKey;
use crate::statements::SetError;
use crate::store::ObjectStore;
use crate::{Actor, AllowAll, Authorizer};

/// The destination of the statements that a hub holds for one repository (ADR-025): the held
/// statements one after the other, in the order of their numbers. The layout of a repository
/// holds no file for them (ADR-009); the hub keeps them where it keeps its data and passes the
/// way to them with every call, as it does for the record ([`RecordDestination`]).
///
/// [`Repository::accept_statement`] calls both methods under the lock of the references. The
/// reader of a range ([`held_in`]) calls [`read`](Self::read) without the lock, beside a
/// writer, so `read` answers what is durable and nothing that is still being written.
pub trait StatementDestination {
    /// The held statements: the bytes of every one whose [`append`](Self::append) returned
    /// `Ok`, in their order, and nothing else. A repository without an accepted statement has
    /// none.
    ///
    /// The call may run without the lock, while an `append` runs. It never returns bytes of an
    /// append that has not returned, or that failed and was cut back: an implementation keeps
    /// the length of the durable part, which it sets after the sync, and reads up to it. A
    /// lock-free reader would otherwise serve a held statement that the hub then denies holding.
    /// What `read` returns beyond the held statements, a part of one left by a crash, is damage,
    /// which the readers report.
    ///
    /// # Errors
    ///
    /// The error of the storage.
    fn read(&mut self) -> io::Result<Vec<u8>>;

    /// Appends the bytes of one held statement and makes them durable. When the call returns
    /// `Ok`, the statement survives a crash and the answer leaves the hub.
    ///
    /// # Errors
    ///
    /// The error of the storage. The destination is then as it was before the call: an
    /// implementation cuts back what it wrote, because a part of a held statement makes the
    /// list unreadable.
    fn append(&mut self, held: &[u8]) -> io::Result<()>;
}

/// Why a statement was not accepted. [`Refused`](Self::Refused) is the answer to the sender
/// ([`answer`](Self::answer)); every other variant is a failure of the hub.
#[derive(Debug)]
#[non_exhaustive]
pub enum StatementError {
    /// The repository has a working copy. A clone keeps its statements through the operation
    /// log (ADR-011), not as a hub.
    NotBare {
        /// The root of the repository.
        path: PathBuf,
    },
    /// The statement is refused. Nothing was written, unless an earlier ref update was being
    /// completed.
    Refused(StatementRefusal),
    /// A failure that the accepting operation of a ref update shares: the lock is held, the
    /// record cannot be read or is damaged, the references do not agree with it, the register
    /// at the tip of `main` cannot be read, the policy cannot be read from the store, or an
    /// earlier ref update that stopped cannot be completed. It is never
    /// [`AcceptError::Stale`] or [`AcceptError::Refused`].
    Hub(Box<AcceptError>),
    /// The held statements could not be read or the new one could not be appended.
    StatementsIo {
        /// What was tried.
        step: RecordStep,
        /// The error of the destination.
        source: io::Error,
    },
    /// The held statements are damaged: one is refused by its reader.
    Statements(Box<HeldParseError>),
    /// The held statements are damaged: one breaks the order that the writer keeps. It carries
    /// another number than its place, a `record` line or a time before the one of the statement
    /// before it, or a `record` line after the last entry of the record.
    Order {
        /// The position of the held statement, from 1.
        position: usize,
        /// The number that it carries.
        number: u64,
    },
    /// The time of the hub lies before the time of the last held statement or of the last
    /// record entry: its clock stepped back. Nothing was written.
    Time {
        /// The time of the hub.
        now: i64,
        /// The time that it lies before.
        last: i64,
    },
    /// The history that a rule reads cannot be read from the store.
    History(Box<HistoryError>),
    /// The lines of the hub could not be signed: the signer failed or its signature does not
    /// verify. Nothing was written.
    Seal(Box<SealError>),
}

impl StatementError {
    /// The error message that answers the sender (ADR-023): [`ErrorMessage::StatementRefused`]
    /// for a refused statement, and `None` for a failure of the hub.
    pub fn answer(&self) -> Option<ErrorMessage> {
        match self {
            Self::Refused(refusal) => Some(ErrorMessage::StatementRefused(refusal.clone())),
            _ => None,
        }
    }

    /// The error of the file system, if that is what failed.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::StatementsIo { source, .. } => Some(source),
            Self::Hub(error) => error.io_error(),
            Self::Seal(error) => error.io_error(),
            Self::NotBare { .. }
            | Self::Refused(_)
            | Self::Statements(_)
            | Self::Order { .. }
            | Self::Time { .. }
            | Self::History(_) => None,
        }
    }
}

impl fmt::Display for StatementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotBare { path } => write!(
                f,
                "{} has a working copy; a statement is accepted by a bare repository only",
                path.display()
            ),
            Self::Refused(refusal) => refusal.fmt(f),
            Self::Hub(error) => error.fmt(f),
            Self::StatementsIo { step, source } => {
                let step = match step {
                    RecordStep::Read => "read",
                    RecordStep::Append => "append to",
                };
                write!(f, "cannot {step} the statements of the hub: {source}")
            }
            Self::Statements(error) => write!(f, "the statements of the hub are damaged: {error}"),
            Self::Order { position, number } => write!(
                f,
                "the statements of the hub are damaged: held statement {position}, number \
                 {number}, breaks their order"
            ),
            Self::Time { now, last } => write!(
                f,
                "the time of the hub ({now}) lies before the time of its last statement or \
                 record entry ({last}); its clock stepped back"
            ),
            Self::History(error) => error.fmt(f),
            Self::Seal(error) => write!(f, "cannot sign the lines of the hub: {error}"),
        }
    }
}

// Each message is or includes the message of its cause, so no source is named (see the crate
// documentation).
impl Error for StatementError {}

impl From<AcceptError> for StatementError {
    fn from(error: AcceptError) -> Self {
        Self::Hub(Box::new(error))
    }
}

impl From<StatementRefusal> for StatementError {
    fn from(refusal: StatementRefusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<HistoryError> for StatementError {
    fn from(error: HistoryError) -> Self {
        Self::History(Box::new(error))
    }
}

/// Whether the requester of a reading request may read the repository. The hub decides it from
/// its sign-in and its namespaces (ADR-024, ADR-027); the core takes the answer as an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadAccess {
    /// The requester may read the repository.
    Granted,
    /// The requester may not read it, or it does not exist.
    Denied,
}

/// Why a range of the statements or of the record was not read. [`NotReadable`](Self::NotReadable)
/// is the answer to the requester ([`answer`](Self::answer)); every other variant is a failure
/// of the hub.
#[derive(Debug)]
#[non_exhaustive]
pub enum RangeError {
    /// The requester may not read the repository ([`ReadAccess::Denied`]).
    NotReadable,
    /// The destination could not be read.
    Io(io::Error),
    /// A held statement is damaged.
    Held(Box<HeldParseError>),
    /// An entry of the record is damaged.
    Record(Box<RecordParseError>),
    /// The held statements or the record are damaged: an entry breaks the order that the writer
    /// keeps (see [`StatementError::Order`]; for the record, its number alone).
    Order {
        /// The position of the held statement or the entry, from 1.
        position: usize,
        /// The number that it carries.
        number: u64,
    },
}

impl RangeError {
    /// The error message that answers the requester: [`ErrorMessage::NotReadable`], and `None`
    /// for a failure of the hub.
    pub fn answer(&self) -> Option<ErrorMessage> {
        match self {
            Self::NotReadable => Some(ErrorMessage::NotReadable),
            _ => None,
        }
    }
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotReadable => f.write_str("the requester may not read the repository"),
            Self::Io(error) => write!(f, "cannot read the destination: {error}"),
            Self::Held(error) => write!(f, "the statements of the hub are damaged: {error}"),
            Self::Record(error) => write!(f, "the record of ref updates is damaged: {error}"),
            Self::Order { position, number } => write!(
                f,
                "the statements or the record of the hub are damaged: their entry {position}, \
                 number {number}, breaks their order"
            ),
        }
    }
}

// Each message includes the message of its cause.
impl Error for RangeError {}

/// The held statements of a range, for the request that reads them (ADR-025). The reading
/// takes no lock; the destination answers only what is durable
/// ([`StatementDestination::read`]).
///
/// # Errors
///
/// [`RangeError::NotReadable`] for [`ReadAccess::Denied`], before the destination is read;
/// [`RangeError::Io`] if the destination cannot be read, and [`RangeError::Held`] and
/// [`RangeError::Order`] if it is damaged, which includes a part of a held statement that a
/// crash left at its end.
pub fn held_in(
    destination: &mut dyn StatementDestination,
    range: &Range,
    access: ReadAccess,
) -> Result<Vec<HeldStatement>, RangeError> {
    if access == ReadAccess::Denied {
        return Err(RangeError::NotReadable);
    }
    let bytes = destination.read().map_err(RangeError::Io)?;
    let held = read_held(&bytes).map_err(|error| RangeError::Held(Box::new(error)))?;
    if let Some((position, number)) = out_of_order(&held, None) {
        return Err(RangeError::Order { position, number });
    }
    Ok(held
        .into_iter()
        .filter(|held| range.contains(held.number()))
        .collect())
}

/// The entries of the record in a range, for the request that reads them (ADR-025). It reads
/// as [`held_in`] does, without the lock ([`RecordDestination::read`]). The chains of the
/// record are checked by the accepting operation and by the reader of the record in a clone;
/// this reader checks that the numbers follow one another.
///
/// # Errors
///
/// [`RangeError::NotReadable`] for [`ReadAccess::Denied`], before the destination is read;
/// [`RangeError::Io`] if the record cannot be read, and [`RangeError::Record`] and
/// [`RangeError::Order`] if it is damaged.
pub fn record_in(
    destination: &mut dyn RecordDestination,
    range: &Range,
    access: ReadAccess,
) -> Result<Vec<RecordEntry>, RangeError> {
    if access == ReadAccess::Denied {
        return Err(RangeError::NotReadable);
    }
    let bytes = destination.read().map_err(RangeError::Io)?;
    let entries = read_record(&bytes).map_err(|error| RangeError::Record(Box::new(error)))?;
    let numbers: Vec<u64> = entries.iter().map(RecordEntry::number).collect();
    if let Some((position, number)) = first_out_of_place(&numbers) {
        return Err(RangeError::Order { position, number });
    }
    Ok(entries
        .into_iter()
        .filter(|entry| range.contains(entry.number()))
        .collect())
}

/// The qualified name of a branch.
fn branch_reference(branch: &BranchName) -> QualifiedName {
    QualifiedName::new(RefKind::Branch, branch.clone())
}

/// The held statements and the merges of the record in the one order of the hub (ADR-025): a
/// held statement comes after the record entry that its `record` line names and before the next
/// one, and the held statements among themselves by their numbers. The fold takes one sequence
/// of numbers, so every step gets its place in that order as its number.
///
/// A merge is an entry that moves `main` with the opening of a pull request as its evidence;
/// its commit is the head that the opening names. The actor of a statement or of an entry is
/// built from its identity ([`Actors::of`]), so no step is left out: a closing or a merge of an
/// actor that a later register no longer holds still ends its pull request.
fn placed_in_order(
    held: &[HeldStatement],
    entries: &[RecordEntry],
    actors: &Actors<'_>,
) -> (Vec<Placed>, Vec<Merge>) {
    enum Step<'a> {
        Entry(&'a RecordEntry),
        Held(&'a HeldStatement),
    }
    let mut steps: Vec<((u64, u8, u64), Step<'_>)> = entries
        .iter()
        .map(|entry| ((entry.number(), 0, 0), Step::Entry(entry)))
        .chain(
            held.iter()
                .map(|held| ((held.record(), 1, held.number()), Step::Held(held))),
        )
        .collect();
    steps.sort_by_key(|(key, _)| *key);
    let main = branch_reference(&BranchName::try_new(TARGET).expect("`main` is a branch name"));
    let head_of = |pull: StatementHash| {
        held.iter()
            .find(|held| held.hash() == pull)
            .and_then(|held| match held.statement().body() {
                Body::Opening(opening) => Some(opening.head()),
                _ => None,
            })
    };
    let mut placed = Vec::new();
    let mut merges = Vec::new();
    for (number, (_, step)) in (1..).zip(steps) {
        match step {
            Step::Held(held) => {
                let actor = actors.of(held.actor(), held.register());
                let statement = held.statement().clone();
                placed.push(Placed::new(number, held.time(), actor, statement));
            }
            Step::Entry(entry) => {
                let merged = entry
                    .update()
                    .payload()
                    .moves()
                    .iter()
                    .find(|moved| moved.name() == &main && moved.new_commit().is_some())
                    .and_then(|moved| moved.evidence());
                let Some(pull) = merged else {
                    continue;
                };
                // An opening that the hub does not hold names no pull request that the fold
                // knows, and the fold skips its merge.
                if let Some(commit) = head_of(pull) {
                    merges.push(Merge {
                        pull,
                        number,
                        time: entry.time(),
                        by: actors.of(entry.actor(), entry.register()),
                        commit,
                    });
                }
            }
        }
    }
    (placed, merges)
}

/// Where the fold takes the actors of the identities in the lines of the hub from.
struct Actors<'a> {
    /// The register at the tip of `main`.
    register: &'a Register,
    /// The store, which holds the register commits that the lines name, as far as they are
    /// not gone.
    store: &'a dyn ObjectStore,
}

impl Actors<'_> {
    /// The actor of `identity`, from the lines of a held statement or an entry that name the
    /// register commit `at`. The lines name an identity of the register of their time, so the
    /// step is never dropped: that would change the state of a pull request. The actor is the
    /// one that the register at the tip of `main` names, else the one of the register at `at`,
    /// whose sponsor of an agent is the one of its time. Where neither holds the identity, it is
    /// the identity alone, an agent with its own ID in the place of the sponsor: the rules of
    /// the fold and of the writers compare identities, but the independence of a check
    /// ([`crate::check::is_independent`]) reads the sponsor, and the gate on a hub must ask the
    /// register for such an agent itself.
    fn of(&self, identity: &Identity, at: CommitId) -> Actor {
        self.register
            .actor_of(identity)
            .or_else(|| {
                Register::at_commit(self.store, &at)
                    .ok()
                    .flatten()
                    .and_then(|register| register.actor_of(identity))
            })
            .unwrap_or_else(|| {
                let id = identity.id();
                match identity.kind() {
                    IdentityKind::Human => Actor::human(id),
                    IdentityKind::Agent => Actor::agent(id, "-", "-", id),
                }
                .expect("an identity of the lines of the hub is a valid actor")
            })
    }
}

/// What the held statements of a hub and its record add up to (see [`placed_in_order`]).
fn evidence_of(held: &[HeldStatement], entries: &[RecordEntry], actors: &Actors<'_>) -> Evidence {
    let (placed, merges) = placed_in_order(held, entries, actors);
    evidence::fold(&placed, &merges)
}

/// The held statements that a fetch brings into the set of the clone (ADR-025), in the order
/// of their numbers: those of the open pull requests (the openings, the approvals that name
/// them, and the checks of their heads) and every closing. A closing ends a pull request that
/// a clone may hold from an earlier fetch, and the set of a clone only grows, so without it the
/// clone would keep the pull request open. The rest is history that the clone does not need for
/// a verdict.
///
/// `held` and `record` are what the hub answered to the two reading requests from number 1 on,
/// `register` is the one at the tip of `main`, and `store` the store of the clone; both name
/// the actors of the identities.
pub fn statements_of_open_pulls(
    held: &[HeldStatement],
    record: &[RecordEntry],
    register: &Register,
    store: &dyn ObjectStore,
) -> Vec<HeldStatement> {
    let evidence = evidence_of(held, record, &Actors { register, store });
    let open: Vec<_> = evidence
        .pulls
        .iter()
        .filter(|pull| pull.is_open())
        .collect();
    held.iter()
        .filter(|held| match held.statement().body() {
            Body::Opening(_) => open.iter().any(|pull| pull.id() == held.hash()),
            Body::Approval(approval) => open.iter().any(|pull| pull.id() == approval.pull()),
            Body::Closing(_) => true,
            Body::Check(checked) => open.iter().any(|pull| *pull.head() == checked.commit()),
            Body::Override(_) => false,
        })
        .cloned()
        .collect()
}

/// The operation that the authorizer is asked for a statement, and its target.
fn operation_of(statement: &Statement) -> Option<(&'static str, String)> {
    match statement.body() {
        Body::Opening(opening) => Some((PR_OPEN, opening.branch().as_str().to_owned())),
        Body::Approval(approval) => Some((PR_APPROVE, format!("pull {}", approval.pull()))),
        Body::Closing(closing) => Some((PR_CLOSE, format!("pull {}", closing.pull()))),
        Body::Check(checked) => Some((CHECK, checked.commit().to_string())),
        Body::Override(_) => None,
    }
}

/// Everything that the rules of a statement read, under the lock.
struct State<'a> {
    record: &'a Record,
    tip: CommitId,
    register: &'a Register,
    verified: &'a Verified,
    identity: Identity,
    evidence: Evidence,
    now: i64,
}

impl Repository {
    /// Accepts a signed statement on a bare repository (ADR-025): checks it by the rules of the
    /// local writer of phase 4 against the register at the tip of `main`, keeps it with the
    /// lines of the hub signed by the signer of the hub, and returns it as the hub holds it.
    ///
    /// The lines of the hub are its running number in the repository, the time of the hub, the
    /// tip of `main` whose register was used, the number of the last record entry, and the
    /// identity that the register names for the key. The record number places the statement in
    /// the one order of the hub: it lies before every record entry with a larger number
    /// ([`HeldStatement::lies_before`]), so before a retirement or a revocation that such an
    /// entry brought to `main`.
    ///
    /// The call takes the lock of the references, as [`accept_ref_update`](Self::accept_ref_update)
    /// does, so statements and ref updates have one order. Before the lock only the form is
    /// judged: the repository that the statement names, its signature, and its kind. Under the
    /// lock it reads the record, completes a ref update that stopped after its entry, checks
    /// that the references agree with the record, reads the held statements, and then checks:
    ///
    /// - the signature and an active key of the register at the tip of `main`, and the
    ///   authorizer for `pr-open`, `pr-approve`, `pr-close`, or `check`;
    /// - **an opening**: the opener is none of the reviewers, the branch is at the head of the
    ///   opening on the hub, the opener is the write owner of the branch where a lease applies
    ///   (the actor of the last record entry of the branch, before the end of the lease that
    ///   the policy at the tip of `main` names), the branch has no open pull request, every
    ///   reviewer is in the register, and the repaired commit is reached by a reference;
    /// - **an approval**: the pull request is known and open, its branch is at the approved
    ///   head on the hub, and the approver is neither the opener nor a writer of a commit that
    ///   the pull request brings (its author, or the actor of the record entry that first
    ///   brought it under a reference);
    /// - **a check result**: the commit and the base are reached by a branch or a tag;
    /// - **a closing**: the pull request is known and open, and the closer is the opener or an
    ///   owner.
    ///
    /// A check that would not count for a change is still accepted: the verdict at the merge
    /// counts everything again. An owner override is refused as a kind that this version does
    /// not accept.
    ///
    /// A statement that the hub holds already is answered with the lines that it holds it with,
    /// and nothing is written, so a sender whose answer was lost can submit again. The one
    /// exception is an opening whose pull request ended: it is final, and the same opening
    /// submitted again is refused as [`StatementRefusal::NotOpen`], as the local writer refuses
    /// it.
    ///
    /// # Errors
    ///
    /// [`StatementError::Refused`] with the typed reason, which is answered to the sender
    /// ([`StatementError::answer`]). Every other error is a failure of the hub:
    /// [`StatementError::NotBare`] for a repository with a working copy,
    /// [`StatementError::Hub`] for a failure that the accepting operation of a ref update
    /// shares (the lock, the record, the references, the register, the policy),
    /// [`StatementError::StatementsIo`], [`StatementError::Statements`], and
    /// [`StatementError::Order`] for held statements that cannot be read or written or are
    /// damaged, [`StatementError::Time`] for a clock that stepped back,
    /// [`StatementError::History`] if the store fails, and [`StatementError::Seal`] if the
    /// signer fails.
    pub fn accept_statement(
        &self,
        statement: &Statement,
        context: AcceptContext<'_>,
        statements: &mut dyn StatementDestination,
    ) -> Result<HeldStatement, StatementError> {
        self.accept_statement_with(statement, context, statements, &AllowAll)
    }

    /// [`accept_statement`](Self::accept_statement) with one more authorizer, which is asked
    /// beside the one of the register and can only refuse more. The tests pass one that denies.
    pub(crate) fn accept_statement_with(
        &self,
        statement: &Statement,
        context: AcceptContext<'_>,
        statements: &mut dyn StatementDestination,
        also: &dyn Authorizer,
    ) -> Result<HeldStatement, StatementError> {
        if !self.is_bare() {
            return Err(StatementError::NotBare {
                path: self.path().to_path_buf(),
            });
        }
        if statement.body().repository() != Some(context.repository) {
            return Err(StatementRefusal::Repository {
                id: context.repository,
            }
            .into());
        }
        if !statement.is_signed() {
            return Err(StatementRefusal::Unsigned.into());
        }
        let Some((operation, target)) = operation_of(statement) else {
            return Err(StatementRefusal::Kind(statement.kind()).into());
        };
        // Nothing but the form was judged so far. The lock is held from here on and opens no
        // write: a statement moves no reference, unless an earlier update is completed.
        let mut lock = Lock::new(self.refs_store().lock_held().map_err(AcceptError::from)?);
        let record = Record::read(context.record, context.repository)?;
        if let Some(last) = record.last() {
            self.complete(&mut lock, last)?;
        }
        self.require_agreement(&record)?;
        let held = read_destination(statements, record.chain.last_number())?;
        let tip = self.main_tip()?.ok_or(StatementRefusal::NoMain)?;
        // A hub holds enforcing repositories only, so a `main` without a register is no state
        // that an accepted update left.
        let unreadable = |error: ModeError| AcceptError::Register(Box::new(error));
        let register = Register::at_commit(self.objects(), &tip)
            .map_err(unreadable)?
            .ok_or_else(|| unreadable(ModeError::RegisterMissing))?;
        let actors = Actors {
            register: &register,
            store: self.objects(),
        };
        let evidence = evidence_of(&held, &record.entries, &actors);
        let hash = statement.hash();
        if let Some(same) = held.iter().find(|held| held.hash() == hash) {
            // An opening whose pull request ended is final: the same opening submitted again
            // is refused as the local writer refuses it, so the sender does not take a pull
            // request for open that is not.
            let ended = matches!(statement.body(), Body::Opening(_))
                && evidence
                    .pulls
                    .iter()
                    .any(|pull| pull.id() == hash && !pull.is_open());
            if ended {
                return Err(StatementRefusal::NotOpen.into());
            }
            return Ok(same.clone());
        }
        let verified = register
            .verify_statement_key(statement)
            .ok_or(StatementRefusal::Key { register: tip })?;
        let operation = OperationName::try_new(operation).expect("the operation is a name");
        let mode = Mode::Enforcing(register.clone());
        let authorizer = RegisterAuthorizer::new(&mode, Some(&verified));
        let actor = verified.actor();
        if Allowed::ask(&authorizer, actor, &operation, &target).is_none()
            || Allowed::ask(also, actor, &operation, &target).is_none()
        {
            return Err(StatementRefusal::Denied(operation).into());
        }

        let state = State {
            record: &record,
            tip,
            register: &register,
            verified: &verified,
            identity: Identity::of(actor),
            evidence,
            now: context.time,
        };
        self.judge(statement, &state)?;

        let last = held
            .last()
            .map(HeldStatement::time)
            .into_iter()
            .chain(record.last().map(RecordEntry::time))
            .max();
        if let Some(last) = last.filter(|last| context.time < *last) {
            return Err(StatementError::Time {
                now: context.time,
                last,
            });
        }
        let lines = HubLines {
            number: held.len() as u64 + 1,
            time: context.time,
            register: tip,
            record: record.chain.last_number(),
            actor: state.identity,
        };
        let held = HeldStatement::seal(statement.clone(), lines, context.signer)
            .map_err(|error| StatementError::Seal(Box::new(error)))?;
        statements
            .append(&held.encode())
            .map_err(|source| StatementError::StatementsIo {
                step: RecordStep::Append,
                source,
            })?;
        Ok(held)
    }

    /// The rules of the kind of the statement (see [`accept_statement`](Self::accept_statement)).
    fn judge(&self, statement: &Statement, state: &State<'_>) -> Result<(), StatementError> {
        let tip_of = |branch: &BranchName| -> Result<Option<CommitId>, StatementError> {
            Ok(self.refs().branch(branch).map_err(AcceptError::from)?)
        };
        let pull_of = |pull: StatementHash| {
            let found = state
                .evidence
                .pulls
                .iter()
                .find(|known| known.id() == pull)
                .ok_or(StatementRefusal::NoPull)?;
            if found.is_open() {
                Ok(found)
            } else {
                Err(StatementRefusal::NotOpen)
            }
        };
        match statement.body() {
            Body::Opening(opening) => {
                if opening.reviewers().contains(&state.identity) {
                    return Err(StatementRefusal::SelfReview.into());
                }
                let tip = tip_of(opening.branch())?;
                if tip != Some(opening.head()) {
                    return Err(StatementRefusal::Head(tip).into());
                }
                if lease::applies(Some(state.register), false) {
                    self.require_write_owner(opening.branch(), state)?;
                }
                if let Some(open) = pull::open_for(&state.evidence.pulls, opening.branch()) {
                    return Err(StatementRefusal::AlreadyOpen(open.id()).into());
                }
                if let Some(unknown) = opening
                    .reviewers()
                    .iter()
                    .find(|reviewer| state.register.entry(reviewer).is_none())
                {
                    return Err(StatementRefusal::UnknownReviewer(unknown.clone()).into());
                }
                self.require_reachable(opening.repairs().into_iter().collect())
            }
            Body::Approval(approval) => {
                let pull = pull_of(approval.pull())?;
                let tip = tip_of(pull.branch())?;
                if tip != Some(approval.head()) {
                    return Err(StatementRefusal::Head(tip).into());
                }
                let opener = Identity::of(pull.opener());
                if opener == state.identity
                    || self
                        .writers(approval.head(), state)?
                        .contains(&state.identity)
                {
                    return Err(StatementRefusal::SelfApproval.into());
                }
                Ok(())
            }
            Body::Check(_) => self.require_reachable(statement.body().commits()),
            Body::Closing(closing) => {
                let pull = pull_of(closing.pull())?;
                let opener = Identity::of(pull.opener());
                if opener != state.identity && !state.register.is_owner(state.verified.actor()) {
                    return Err(StatementRefusal::NotCloser.into());
                }
                Ok(())
            }
            // Refused before the lock.
            Body::Override(_) => Err(StatementRefusal::Kind(statement.kind()).into()),
        }
    }

    /// The rule of the write ownership for an opening (ADR-025): the opener is the actor of the
    /// last record entry of the branch, and the lease that this entry began has not ended by
    /// the time of the hub. A branch that the record left deleted has no write owner.
    fn require_write_owner(
        &self,
        branch: &BranchName,
        state: &State<'_>,
    ) -> Result<(), StatementError> {
        let owner = state
            .record
            .owners
            .get(&branch_reference(branch))
            .filter(|owner| owner.commit.is_some() && owner.actor == state.identity)
            .ok_or(StatementRefusal::NotWriteOwner)?;
        let policy = match self.policy_at(&state.tip, true) {
            Ok(policy) => policy,
            Err(
                error @ (PolicyError::Missing | PolicyError::NotAFile | PolicyError::Invalid(_)),
            ) => return Err(StatementRefusal::Policy(Reason::from_message(&error)).into()),
            Err(error) => {
                return Err(AcceptError::Policy {
                    damaged: self.policy_is_damaged(&state.tip),
                    source: Box::new(error),
                }
                .into())
            }
        };
        if state.now < lease::expiry(owner.time, policy.lease_minutes()) {
            Ok(())
        } else {
            Err(StatementRefusal::NotWriteOwner.into())
        }
    }

    /// Refuses the first commit of `commits` that no branch and no tag of the repository
    /// reaches (ADR-023): a statement about an object that the hub holds unnamed, or not at
    /// all, would stand for a commit that nobody can fetch.
    fn require_reachable(&self, commits: Vec<CommitId>) -> Result<(), StatementError> {
        if commits.is_empty() {
            return Ok(());
        }
        let refs = self.refs();
        let branches = refs.branches().map_err(AcceptError::from)?;
        let tags = refs.tags().map_err(AcceptError::from)?;
        let mut reached = HashSet::new();
        for (_, tip) in branches.into_iter().chain(tags) {
            if reached.contains(&tip) {
                continue;
            }
            for (id, _) in history::ancestors(self.objects(), tip, Parents::All, None)? {
                reached.insert(id);
            }
        }
        match commits.into_iter().find(|commit| !reached.contains(commit)) {
            Some(commit) => Err(StatementRefusal::Unreachable(commit).into()),
            None => Ok(()),
        }
    }

    /// The writers of the commits that a pull request with the head `head` brings, those that
    /// `head` reaches and `main` does not: the author of such a commit, and the actor of the
    /// record entry that first brought such a commit under a branch or a tag, directly or
    /// through a descendant. It is the rule of the local writer (ADR-004 rule 4), with the
    /// record of the hub in the place of the operation log of a clone.
    fn writers(&self, head: CommitId, state: &State<'_>) -> Result<Vec<Identity>, StatementError> {
        let store = self.objects();
        let own = history::ancestors(store, head, Parents::All, None)?;
        let known: HashSet<CommitId> = history::ancestors(store, state.tip, Parents::All, None)?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let mut writers: Vec<Identity> = Vec::new();
        let mut brought = HashSet::new();
        for (id, commit) in &own {
            if known.contains(id) {
                continue;
            }
            brought.insert(*id);
            let author = Identity::of(commit.author());
            if !writers.contains(&author) {
                writers.push(author);
            }
        }
        let mut reached: HashSet<CommitId> = HashSet::new();
        for entry in &state.record.entries {
            for new in entry
                .update()
                .payload()
                .moves()
                .iter()
                .filter_map(|moved| moved.new_commit())
            {
                // The walk stops at a commit that was reached before, so every commit is read
                // once over the whole record. A commit that the store does not hold any more
                // reaches only itself.
                let mut first = false;
                let mut stack = vec![(new, None)];
                while let Some((id, child)) = stack.pop() {
                    if !reached.insert(id) {
                        continue;
                    }
                    first |= brought.contains(&id);
                    match history::read_commit(store, id, child) {
                        Ok(commit) => {
                            stack.extend(commit.parents().iter().map(|&parent| (parent, Some(id))));
                        }
                        // The record keeps no commit alive (ADR-025), so the commit that an
                        // entry left may be gone; a missing parent of a commit that is there is
                        // damage of the history.
                        Err(HistoryError::Missing { child: None, .. }) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                if first && !writers.contains(entry.actor()) {
                    writers.push(entry.actor().clone());
                }
            }
        }
        Ok(writers)
    }

    /// Puts the statements that a fetch received into the set of the clone (ADR-025), for the
    /// entry `fetch` that names them (ADR-011). `held` is what [`statements_of_open_pulls`]
    /// selected from the answer of the hub; every held statement must be signed by `hub`, the
    /// key of the hub that the clone pinned (ADR-026). The caller holds the lock of the
    /// references, under which the set is written (ADR-009), and writes the entry after this
    /// call; a statement that no entry names counts for nothing. Returns the hashes in the
    /// order of `held`.
    ///
    /// # Errors
    ///
    /// [`ReceivedError::OtherKey`] with the number of the first held statement that another key
    /// signed, before anything is written, and [`ReceivedError::Set`] if the set cannot be
    /// written.
    #[allow(dead_code)] // The fetch that calls it comes with its own slice (STEMMA-295).
    pub(crate) fn keep_received_statements(
        &self,
        _lock: &WriteLock,
        held: &[HeldStatement],
        hub: &PublicKey,
    ) -> Result<Vec<StatementHash>, ReceivedError> {
        if let Some(other) = held.iter().find(|held| &held.hub_key() != hub) {
            return Err(ReceivedError::OtherKey {
                number: other.number(),
            });
        }
        let set = self.statement_set();
        held.iter()
            .map(|held| set.put(held.statement()).map_err(ReceivedError::Set))
            .collect()
    }
}

/// Why the statements that a fetch received were not kept
/// ([`Repository::keep_received_statements`]).
#[derive(Debug)]
#[allow(dead_code)] // The fetch that reports it comes with its own slice (STEMMA-295).
pub(crate) enum ReceivedError {
    /// A held statement is signed by another key than the pinned key of the hub.
    OtherKey {
        /// The number of the held statement.
        number: u64,
    },
    /// The set of statements cannot be written.
    Set(SetError),
}

/// The held statements of the destination, checked for their numbers.
fn read_destination(
    statements: &mut dyn StatementDestination,
    last_entry: u64,
) -> Result<Vec<HeldStatement>, StatementError> {
    let bytes = statements
        .read()
        .map_err(|source| StatementError::StatementsIo {
            step: RecordStep::Read,
            source,
        })?;
    let held = read_held(&bytes).map_err(|error| StatementError::Statements(Box::new(error)))?;
    if let Some((position, number)) = out_of_order(&held, Some(last_entry)) {
        return Err(StatementError::Order { position, number });
    }
    Ok(held)
}

/// The position, from 1, and the number of the first held statement that breaks a rule of the
/// writer: it carries another number than its place, a `record` line before the one of the
/// statement before it or after `last_entry`, the last number of the record where the caller
/// knows it, or a time before the one of the statement before it. The writer never writes such
/// a statement, so the order of the hub stays the order of the numbers.
fn out_of_order(held: &[HeldStatement], last_entry: Option<u64>) -> Option<(usize, u64)> {
    let numbers: Vec<u64> = held.iter().map(HeldStatement::number).collect();
    if let Some(found) = first_out_of_place(&numbers) {
        return Some(found);
    }
    held.iter()
        .enumerate()
        .find(|(index, one)| {
            let before = index.checked_sub(1).map(|before| &held[before]);
            before
                .is_some_and(|before| one.record() < before.record() || one.time() < before.time())
                || last_entry.is_some_and(|last| one.record() > last)
        })
        .map(|(index, one)| (index + 1, one.number()))
}

/// The position, from 1, and the number of the first number that is not its position.
fn first_out_of_place(numbers: &[u64]) -> Option<(usize, u64)> {
    (1u64..)
        .zip(numbers)
        .position(|(expected, number)| *number != expected)
        .map(|index| (index + 1, numbers[index]))
}

#[cfg(test)]
mod tests;
