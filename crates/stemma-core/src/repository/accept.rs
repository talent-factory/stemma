//! The accepting operation of a hub (ADR-025): [`Repository::accept_ref_update`] takes a signed
//! ref update and answers with the receipt or with a typed refusal. The checks, their order, and
//! the rules for branches, tags, and `main` stand in the documentation of that method, which is
//! the public one.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use super::revision::require_unreserved;
use super::{GateError, PolicyError, Repository};
use crate::allowed::Allowed;
use crate::conflict::AnyCommit;
use crate::history::HistoryError;
use crate::merge_base::merge_bases;
use crate::object::CommitId;
use crate::policy::Policy;
use crate::protocol::record::{
    read_record, Acceptance, Chain, ChainError, HubAddress, Move, RecordEntry, RecordParseError,
    RefUpdate, Stale, WriteError,
};
use crate::protocol::refusal::{
    RefOperation, RefusalReason, Refused, RefusedReference, RefusedReferences,
};
use crate::protocol::{ErrorMessage, QualifiedName, Reason, RepositoryId};
use crate::pull::TARGET;
use crate::receive::ConnectivityError;
use crate::refs::{BranchName, HeldLock, RefError, RefKind, WriteLock};
use crate::register::{Identity, Mode, ModeError, Register, RegisterAuthorizer, Verified};
use crate::signature::Signer;
use crate::store::{ObjectStore, ReadError, StoreError};
use crate::{lease, AllowAll, Authorizer};

/// The destination of the record of ref updates of one repository (ADR-025). The layout of a
/// repository holds no file for the record (ADR-009): the hub keeps it where it keeps its data
/// and passes the way to it with every call.
///
/// The accepting operation calls both methods under the lock of the references, so no other
/// writer of the same repository runs between them. The reader of a range
/// ([`record_in`](crate::repository::record_in)) calls [`read`](Self::read) without the lock.
pub trait RecordDestination {
    /// The whole record: the bytes of every entry whose [`append`](Self::append) returned `Ok`,
    /// in their order, and nothing else. A repository without an accepted update has an empty
    /// record.
    ///
    /// The call may run without the lock, while an `append` runs. It never returns bytes of an
    /// append that has not returned, or that failed and was cut back: an implementation keeps
    /// the length of the durable part, which it sets after the sync, and reads up to it.
    ///
    /// # Errors
    ///
    /// The error of the storage.
    fn read(&mut self) -> io::Result<Vec<u8>>;

    /// Appends the bytes of one entry and makes them durable (for a file, a sync). When the call
    /// returns `Ok`, the entry survives a crash: the references move after it, and the receipt
    /// leaves the hub.
    ///
    /// # Errors
    ///
    /// The error of the storage. The record is then as it was before the call: an
    /// implementation cuts back what it wrote, because a part of an entry makes the record
    /// unreadable, and a whole entry without its moves is completed by the next call.
    fn append(&mut self, entry: &[u8]) -> io::Result<()>;
}

/// What a hub passes to the accepting operation with a ref update or a statement (ADR-025): who
/// it is, which repository the update was sent to, where the record lies, its time, and its
/// signer. The slices of the gate add to it, so a caller outside the crate builds it with
/// [`new`](Self::new) and the methods `with_...`. The destination of the statements is a
/// parameter of [`accept_statement`](Repository::accept_statement) of its own; for a ref update
/// it is part of the context ([`with_statements`](Self::with_statements)), because the gate on a
/// hub counts the evidence of a move of `main` from it.
#[non_exhaustive]
pub struct AcceptContext<'a> {
    /// The public address under which the hub was called. An update that names another address
    /// was signed for another hub.
    pub hub: &'a HubAddress,
    /// The ID that the hub assigned to the repository (ADR-027).
    pub repository: RepositoryId,
    /// The record of the repository.
    pub record: &'a mut dyn RecordDestination,
    /// The time of the hub, in seconds since the Unix epoch (UTC). The entry carries it, and the
    /// leases are counted by it.
    pub time: i64,
    /// The signer of the hub (ADR-026). The core never holds the key.
    pub signer: &'a dyn Signer,
    // The gate on a hub (STEMMA-303).
    /// The held statements of the repository, which the gate on a hub folds for a move of
    /// `main`. `None` holds no statement: every move of `main` after the first push is then
    /// refused for want of a pull request. [`accept_statement`](Repository::accept_statement)
    /// takes its destination as a parameter and does not read this one.
    pub statements: Option<&'a mut dyn StatementDestination>,
    /// The policies that count beside the policy of the tip of `main`, combined with it by the
    /// maximum: the namespace policy of ADR-027, when it comes. Empty by default.
    pub policies: &'a [Policy],
}

impl<'a> AcceptContext<'a> {
    /// The context of one call: the address under which the hub was called, the ID of the
    /// repository, its record, the time of the hub in seconds since the Unix epoch, and the
    /// signer of the hub.
    pub fn new(
        hub: &'a HubAddress,
        repository: RepositoryId,
        record: &'a mut dyn RecordDestination,
        time: i64,
        signer: &'a dyn Signer,
    ) -> Self {
        Self {
            hub,
            repository,
            record,
            time,
            signer,
            statements: None,
            policies: &[],
        }
    }

    // The gate on a hub (STEMMA-303).

    /// The context with the held statements of the repository, which the gate on a hub folds
    /// for a move of `main`.
    #[must_use]
    pub fn with_statements(mut self, statements: &'a mut dyn StatementDestination) -> Self {
        self.statements = Some(statements);
        self
    }

    /// The context with policies that count beside the policy of the tip of `main`, combined
    /// with it by the maximum (ADR-027).
    #[must_use]
    pub fn with_policies(mut self, policies: &'a [Policy]) -> Self {
        self.policies = policies;
        self
    }
}

impl fmt::Debug for AcceptContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The record and the signer are ways to a storage and to a key, which have no text.
        f.debug_struct("AcceptContext")
            .field("hub", &self.hub)
            .field("repository", &self.repository)
            .field("time", &self.time)
            .field("policies", &self.policies)
            .finish_non_exhaustive()
    }
}

/// The step at the destination of the record that failed ([`AcceptError::RecordIo`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordStep {
    /// The record was read.
    Read,
    /// The entry of the update was appended.
    Append,
}

/// Why a ref update was not accepted. [`Stale`](Self::Stale) and [`Refused`](Self::Refused)
/// are answers to the sender ([`answer`](Self::answer)); every other variant is a failure of
/// the hub, which the sender cannot mend.
#[derive(Debug)]
#[non_exhaustive]
pub enum AcceptError {
    /// The repository has a working copy. Its references move through the operation log
    /// (ADR-011), not through the record of a hub.
    NotBare {
        /// The root of the repository.
        path: PathBuf,
    },
    /// The update does not continue the chains of its names. No entry was written and no
    /// reference of this update moved; an earlier update may have been completed.
    Stale(Stale),
    /// The update is refused. No entry was written and no reference of this update moved; an
    /// earlier update may have been completed.
    Refused(Refused),
    /// The lock of the references is held by another writer ([`RefError::Locked`]), or the
    /// references failed. Nothing was written, unless an earlier update was being completed.
    Refs(Box<RefError>),
    /// The record could not be read or the entry could not be appended. No reference of this
    /// update moved.
    RecordIo {
        /// What was tried.
        step: RecordStep,
        /// The error of the destination.
        source: io::Error,
    },
    /// The record is damaged: an entry is refused by its reader.
    Record(Box<RecordParseError>),
    /// The record is damaged: its entries do not follow one another, or it is the record of
    /// another repository.
    Chain(Box<ChainError>),
    /// A reference is not where the record leaves it: something moved it past the accepting
    /// operation. No entry was written and no reference of this update moved.
    Diverged {
        /// The qualified name.
        name: QualifiedName,
        /// The commit that the record leaves the reference at, or `None`.
        recorded: Option<CommitId>,
        /// The commit of the reference, or `None`.
        found: Option<CommitId>,
    },
    /// The register that counts cannot be read, or `main` holds none. It is the register at the
    /// tip of `main`, of the pushed commit of a first push, or of the commit that the last entry
    /// of the record names when that entry is completed.
    Register(Box<ModeError>),
    /// The policy at the tip of `main` could not be read from the store. A policy that is
    /// missing or not valid is [`Refused`](Self::Refused) instead.
    Policy {
        /// What the reader of the policy reported.
        source: Box<PolicyError>,
        /// Whether the store is damaged: an object on the way to the policy is missing or not
        /// in canonical form. `false` if the storage failed, so that a later call can succeed.
        damaged: bool,
    },
    /// The last entry of the record stopped before its moves, and the register that it names
    /// does not allow them: the commit holds no register, the register does not hold the key of
    /// the update as an active key of the actor that the entry names, or its authorizer refuses
    /// a move. The operation writes no such entry, so its moves are not completed.
    LastEntry {
        /// The number of the entry.
        number: u64,
    },
    /// The connectivity check could not run: the storage failed. An update whose objects do not
    /// hold together is [`Refused`](Self::Refused) instead.
    Connectivity(Box<ConnectivityError>),
    /// The history between the old and the new commit of a branch cannot be read.
    History(Box<HistoryError>),
    /// The entry could not be made: the signer of the hub failed or its signature does not
    /// verify, the time of the hub lies before the time of the last entry
    /// ([`ChainError::Time`]: its clock stepped back), or the record holds the largest number.
    /// No entry was written and no reference of this update moved; an earlier update may have
    /// been completed.
    Write(Box<WriteError>),
    // The gate on a hub (STEMMA-303).
    /// The held statements, which the gate on a hub folds for a move of `main`, cannot be read
    /// or are damaged. No entry was written and no reference of this update moved.
    Statements(Box<StatementError>),
    /// The verdict about `main` could not be reached: the store failed while the tree merge,
    /// the difference, or the history was read. A verdict that refuses is
    /// [`Refused`](Self::Refused) instead.
    Verdict(Box<GateError>),
    /// The entry is in the record, and a reference did not move: the storage failed. The update
    /// is accepted, and the next call for this repository completes its moves from the entry.
    NotCompleted {
        /// The entry that was written.
        receipt: Box<RecordEntry>,
        /// Why a reference did not move.
        source: Box<RefError>,
    },
}

impl AcceptError {
    /// The error message that answers the sender (ADR-023): [`ErrorMessage::Stale`] for a stale
    /// update and [`ErrorMessage::Refused`] for a refused one. `None` for a failure of the hub.
    pub fn answer(&self) -> Option<ErrorMessage> {
        match self {
            Self::Stale(stale) => Some(ErrorMessage::Stale(stale.clone())),
            Self::Refused(refused) => Some(ErrorMessage::Refused(refused.clone())),
            _ => None,
        }
    }

    /// The entry of an update that is accepted although the call failed
    /// ([`NotCompleted`](Self::NotCompleted)): it is in the record, and the next call makes its
    /// moves. [`answer`](Self::answer) is `None` for it, because the hub sends no receipt
    /// before the references moved; a hub that drops the entry here loses nothing, as the
    /// sender reads the outcome from the list of references. `None` for every other error.
    pub fn receipt(&self) -> Option<&RecordEntry> {
        match self {
            Self::NotCompleted { receipt, .. } => Some(receipt),
            _ => None,
        }
    }

    /// The error of the file system, if that is what failed.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::RecordIo { source, .. } => Some(source),
            Self::Refs(source) | Self::NotCompleted { source, .. } => source.io_error(),
            Self::Connectivity(source) => source.io_error(),
            Self::Write(source) => source.io_error(),
            Self::Statements(source) => source.io_error(),
            Self::Verdict(source) => io_error_of(source),
            Self::Register(source) => match &**source {
                ModeError::Io(error) => Some(error),
                _ => None,
            },
            Self::NotBare { .. }
            | Self::Stale(_)
            | Self::Refused(_)
            | Self::Record(_)
            | Self::Chain(_)
            | Self::Diverged { .. }
            | Self::Policy { .. }
            | Self::LastEntry { .. }
            | Self::History(_) => None,
        }
    }
}

impl fmt::Display for AcceptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let or_none = |commit: &Option<CommitId>| {
            commit.map_or_else(|| "no commit".to_owned(), |commit| commit.to_string())
        };
        match self {
            Self::NotBare { path } => write!(
                f,
                "{} has a working copy; a ref update is accepted by a bare repository only",
                path.display()
            ),
            Self::Stale(stale) => stale.fmt(f),
            Self::Refused(refused) => refused.fmt(f),
            Self::Refs(error) => error.fmt(f),
            Self::RecordIo { step, source } => {
                let step = match step {
                    RecordStep::Read => "read",
                    RecordStep::Append => "append to",
                };
                write!(f, "cannot {step} the record of ref updates: {source}")
            }
            Self::Record(error) => write!(f, "the record of ref updates is damaged: {error}"),
            Self::Chain(error) => write!(f, "the record of ref updates is damaged: {error}"),
            Self::Diverged {
                name,
                recorded,
                found,
            } => write!(
                f,
                "the reference {name} is at {}, and the record of ref updates leaves it at {}",
                or_none(found),
                or_none(recorded)
            ),
            Self::Register(error) => {
                write!(
                    f,
                    "cannot read the register that counts for the ref update: {error}"
                )
            }
            Self::Policy { source, .. } => source.fmt(f),
            Self::LastEntry { number } => write!(
                f,
                "the register that the record entry {number} names does not allow its moves, so \
                 they are not completed"
            ),
            Self::Connectivity(error) => error.fmt(f),
            Self::History(error) => error.fmt(f),
            Self::Write(error) => error.fmt(f),
            Self::Statements(error) => error.fmt(f),
            Self::Verdict(error) => {
                write!(f, "cannot reach the verdict about `main`: {error}")
            }
            Self::NotCompleted { receipt, source } => write!(
                f,
                "the record entry {} is written, but a reference did not move: {source}",
                receipt.number()
            ),
        }
    }
}

// Each message is or includes the message of its cause, so no source is named (see the crate
// documentation).
impl Error for AcceptError {}

/// The error of the file system behind a failure of the verdict, if that is what failed.
fn io_error_of(error: &GateError) -> Option<&io::Error> {
    let mut source: Option<&(dyn Error + 'static)> = Some(error);
    while let Some(found) = source {
        if let Some(io) = found.downcast_ref::<io::Error>() {
            return Some(io);
        }
        source = found.source();
    }
    None
}

impl From<RefError> for AcceptError {
    fn from(source: RefError) -> Self {
        Self::Refs(Box::new(source))
    }
}

impl From<Refused> for AcceptError {
    fn from(refused: Refused) -> Self {
        Self::Refused(refused)
    }
}

/// The qualified name of `main`.
fn main_reference() -> QualifiedName {
    let name = BranchName::try_new(TARGET).expect("`main` is a branch name");
    QualifiedName::new(RefKind::Branch, name)
}

/// A reference with the reason that this operation found for it.
fn refused_reference(name: QualifiedName, reason: RefusalReason) -> RefusedReference {
    RefusedReference::try_new(name, reason)
        .expect("the operation gives a reference only a reason of its kind")
}

/// The refusal of one reference.
fn one_refused(name: &QualifiedName, reason: RefusalReason) -> AcceptError {
    Refused::References(
        RefusedReferences::try_new(vec![refused_reference(name.clone(), reason)])
            .expect("one reference is at least one, and it is named once"),
    )
    .into()
}

/// The refusal of the references of `refused`, which is not empty.
fn all_refused(refused: BTreeMap<QualifiedName, RefusalReason>) -> AcceptError {
    let references = refused
        .into_iter()
        .map(|(name, reason)| refused_reference(name, reason))
        .collect();
    Refused::References(
        RefusedReferences::try_new(references)
            .expect("the map is not empty, and a key of a map stands once"),
    )
    .into()
}

/// The operation that the authorizer is asked for: `push` for a branch, `tag` to set a tag,
/// and `untag` to delete one.
fn operation_of(moved: &Move) -> RefOperation {
    match (moved.name().kind(), moved.new_commit()) {
        (RefKind::Branch, _) => RefOperation::Push,
        (RefKind::Tag, Some(_)) => RefOperation::Tag,
        (RefKind::Tag, None) => RefOperation::Untag,
    }
}

/// Whether one name holds the place of the other in a directory of references: they are equal
/// or one is a directory above the other, compared without the case of ASCII letters, as the
/// reference store compares them.
fn in_the_way(one: &BranchName, other: &BranchName) -> bool {
    let (one, other) = (
        one.as_str().to_ascii_lowercase(),
        other.as_str().to_ascii_lowercase(),
    );
    let (short, long) = if one.len() <= other.len() {
        (one, other)
    } else {
        (other, one)
    };
    long == short
        || long
            .strip_prefix(&short)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The lock of the references through one call: held from the start, and open for a write
/// from the first answer of the authorizer on (ADR-002). One of the two fields is always set.
struct Lock {
    held: Option<HeldLock>,
    writer: Option<WriteLock>,
}

impl Lock {
    fn new(held: HeldLock) -> Self {
        Self {
            held: Some(held),
            writer: None,
        }
    }

    /// The lock for a writer. The first call opens it with the proof that the authorizer
    /// allowed an operation; a later call returns the lock that is open already.
    fn writer(&mut self, allowed: &Allowed) -> &WriteLock {
        if let Some(held) = self.held.take() {
            self.writer = Some(held.into_writer(allowed));
        }
        self.writer
            .as_ref()
            .expect("the lock is held or open for a writer")
    }
}

/// Who sends a ref update, by the register that counts for it.
struct Sender {
    /// The commit whose register was read: the tip of `main`, or the pushed commit of the first
    /// push.
    register: CommitId,
    /// Whether `main` does not exist, so that this is the first push.
    first: bool,
    mode: Mode,
    verified: Verified,
}

impl Sender {
    /// The register that counts for the update.
    fn register_of(&self) -> &Register {
        match &self.mode {
            Mode::Enforcing(register) => register,
            Mode::Open => unreachable!("a hub judges by an enforcing register only"),
        }
    }

    /// Asks the authorizer of the register, and `also`, for the move of one reference. Both
    /// must allow it.
    fn ask(&self, moved: &Move, also: &dyn Authorizer) -> Result<Allowed, RefusalReason> {
        let operation = operation_of(moved);
        let asked = operation.operation_name();
        let target = moved.name().name().as_str();
        let actor = self.verified.actor();
        let authorizer = RegisterAuthorizer::new(&self.mode, Some(&self.verified));
        match (
            Allowed::ask(&authorizer, actor, &asked, target),
            Allowed::ask(also, actor, &asked, target),
        ) {
            (Some(allowed), Some(_)) => Ok(allowed),
            _ => Err(RefusalReason::Denied { operation }),
        }
    }
}

/// What the record says about a name: the actor and the time of its last entry, who is the
/// write owner of a branch, and the commit that this entry left.
struct Owner {
    actor: Identity,
    time: i64,
    /// The commit that the entry left the reference at, or `None` if it deleted it. A branch
    /// that was deleted has no write owner.
    commit: Option<CommitId>,
}

/// The record of a repository, as it was read.
struct Record {
    chain: Chain,
    /// The entries, in their order. The last one's moves may not be made yet.
    entries: Vec<RecordEntry>,
    owners: BTreeMap<QualifiedName, Owner>,
}

impl Record {
    fn read(
        destination: &mut dyn RecordDestination,
        repository: RepositoryId,
    ) -> Result<Self, AcceptError> {
        let bytes = destination.read().map_err(|source| AcceptError::RecordIo {
            step: RecordStep::Read,
            source,
        })?;
        let entries = read_record(&bytes).map_err(|error| AcceptError::Record(Box::new(error)))?;
        let mut chain = Chain::new(repository);
        let mut owners = BTreeMap::new();
        for entry in &entries {
            chain
                .accept(entry)
                .map_err(|error| AcceptError::Chain(Box::new(error)))?;
            for moved in entry.update().payload().moves() {
                owners.insert(
                    moved.name().clone(),
                    Owner {
                        actor: entry.actor().clone(),
                        time: entry.time(),
                        commit: moved.new_commit(),
                    },
                );
            }
        }
        Ok(Self {
            chain,
            entries,
            owners,
        })
    }

    /// The last entry, whose moves may not be made yet.
    fn last(&self) -> Option<&RecordEntry> {
        self.entries.last()
    }
}

impl Repository {
    /// Accepts a ref update on a bare repository (ADR-025): checks it, writes the record entry
    /// with the signature of the hub, moves the references, and returns the entry, which is the
    /// receipt. All references of the update move, or none does, and only an accepted update is
    /// recorded.
    ///
    /// The operation is one call, and it takes the lock of the references itself, in the middle:
    ///
    /// - **Before the lock** it compares the address of the hub and the ID of the repository
    ///   with the payload. Nothing of the repository is read: the signature was checked when
    ///   the update was read, and everything else depends on state that only the lock holds
    ///   still.
    /// - **The lock** is held from then on without a proof, which opens no write. It becomes
    ///   the lock of a writer with the answer of the authorizer (ADR-002), which is asked under
    ///   it: for the update of this call after all its checks, and for an earlier update that
    ///   stopped before that one is completed.
    /// - **Under the lock** it reads the record, completes an update that stopped after its
    ///   entry, and checks that the references are where the record leaves them. Then it checks,
    ///   in this order: `previous` and `old` against the chain of every name, the names, the
    ///   connectivity of the new commits, the sender against the register at the tip of `main`
    ///   before the update, and for every reference the authorizer and the rule of its kind.
    ///   Then it writes the record entry with the signature of the hub, moves the references,
    ///   and returns the entry as the receipt.
    ///
    /// A refusal writes no entry and moves no reference of its update: the record holds
    /// accepted updates only. This version judges branches and tags:
    ///
    /// - **A branch.** Its write owner is the actor of the last record entry of its name. The
    ///   lease runs from the time of the hub in that entry, for the duration that the policy at
    ///   the tip of `main` names. Before its end another actor is refused; after it, whoever
    ///   stands in the register takes the branch. The write owner makes any move, and the entry
    ///   says whether it was a fast-forward. A name whose last entry deleted the branch has no
    ///   write owner.
    /// - **A tag.** It is set and deleted by the authorizer, with the operations `tag` and
    ///   `untag`, and never moved from one commit to another (ADR-008).
    /// - **`main`.** It is set by the first push into an empty repository, whose pushed register
    ///   must hold the key of the update as an active key of an owner (ADR-024). Every later
    ///   update of `main` goes through the gate on a hub (STEMMA-303): the authorizer for `push`,
    ///   and then the verdict about `main` that the local gate reaches as well. `M` must have the
    ///   single parent `T`, its evidence must name the opening of an open pull request that the
    ///   hub holds ([`AcceptContext::with_statements`]), and its tree must be the clean merge of
    ///   the head onto `T`; the level, the checks, the approvals, and the bounds are counted over
    ///   the held statements in the order of the hub, by the register and the policy of `T` and
    ///   the policies of the context. A deletion of `main` is refused as
    ///   [`RefusalReason::Gate`], every verdict as [`RefusalReason::Verdict`]. The first version
    ///   computes the verdict under the lock.
    ///
    /// The operation demands nothing from the sender that only a clone has: no operation log, no
    /// anchor, and no ID of a log (ADR-025). It reads the whole record under the lock; the
    /// trigger for keeping its state between two calls is the measured holding time of the lock.
    ///
    /// The objects that the update names were uploaded before
    /// ([`receive_objects`](Self::receive_objects)); the call checks their connectivity under
    /// the lock. It takes the lock of the references itself and never waits for it.
    ///
    /// An update that stopped after its entry and before its last move, because the process
    /// ended or the storage failed, is completed by the next call for the repository, before
    /// that call judges its own update.
    ///
    /// # Errors
    ///
    /// [`AcceptError::Stale`] if a reference of the update does not name the last record entry
    /// of its name and the commit that this entry left, with every such reference and its state,
    /// and [`AcceptError::Refused`] with the typed reason if the update names another hub or
    /// another repository, the register does not hold its key, the repository has no `main` and
    /// the update does not set it, or a reference is refused. Both are answered to the sender
    /// ([`AcceptError::answer`]).
    ///
    /// Every other error is a failure of the hub: [`AcceptError::NotBare`] for a repository with
    /// a working copy, [`AcceptError::Refs`] if the lock is held ([`RefError::Locked`]) or the
    /// references cannot be read, [`AcceptError::RecordIo`], [`AcceptError::Record`],
    /// [`AcceptError::Chain`], and [`AcceptError::Diverged`] for a record that cannot be read
    /// or written, is damaged, or does not agree with the references,
    /// [`AcceptError::Register`] if the store cannot return the register that counts, or if
    /// the register at the tip of `main` is not
    /// valid, or is not there, [`AcceptError::Policy`], [`AcceptError::Connectivity`], and
    /// [`AcceptError::History`] if the store fails, [`AcceptError::LastEntry`] if an earlier
    /// update that stopped cannot be completed, [`AcceptError::Write`] if the signer fails, the
    /// time of the hub lies before the time of the last entry, or the record is full, and
    /// [`AcceptError::NotCompleted`] if the entry is written and a reference did not move.
    /// [`AcceptError::Statements`] if the held statements cannot be read or are damaged, and
    /// [`AcceptError::Verdict`] if the store fails under the verdict about `main`.
    pub fn accept_ref_update(
        &self,
        update: &RefUpdate,
        context: AcceptContext<'_>,
    ) -> Result<RecordEntry, AcceptError> {
        self.accept_ref_update_with(update, context, &AllowAll)
    }

    /// [`accept_ref_update`](Self::accept_ref_update) with one more authorizer, which is asked
    /// beside the one of the register and can only refuse more. The tests pass one that denies.
    pub(crate) fn accept_ref_update_with(
        &self,
        update: &RefUpdate,
        mut context: AcceptContext<'_>,
        also: &dyn Authorizer,
    ) -> Result<RecordEntry, AcceptError> {
        if !self.is_bare() {
            return Err(AcceptError::NotBare {
                path: self.path().to_path_buf(),
            });
        }
        let payload = update.payload();
        if payload.hub() != context.hub {
            return Err(Refused::Hub {
                called: context.hub.clone(),
            }
            .into());
        }
        if payload.repository() != context.repository {
            return Err(Refused::Repository {
                id: context.repository,
            }
            .into());
        }
        // Nothing but the form was judged so far. The lock is held from here on, and it opens
        // a write only with the answer of the authorizer (ADR-002), which is asked under it.
        let mut lock = Lock::new(self.refs_store().lock_held()?);

        let mut record = Record::read(context.record, context.repository)?;
        if let Some(last) = record.last() {
            self.complete(&mut lock, last)?;
        }
        self.require_agreement(&record)?;
        match record.chain.check(update) {
            Ok(()) => {}
            Err(ChainError::Stale(stale)) => return Err(AcceptError::Stale(stale)),
            Err(error) => return Err(AcceptError::Chain(Box::new(error))),
        }

        let moves = payload.moves();
        let mut refused = self.refused_names(moves)?;
        for moved in moves {
            let Some(new) = moved.new_commit() else {
                continue;
            };
            if refused.contains_key(moved.name()) {
                continue;
            }
            match self.check_connectivity(&[new]) {
                Ok(()) => {}
                Err(error) if error.is_incomplete() => {
                    let reason = RefusalReason::Incomplete(Reason::from_message(&error));
                    refused.insert(moved.name().clone(), reason);
                }
                Err(error) => return Err(AcceptError::Connectivity(Box::new(error))),
            }
        }
        let main = main_reference();
        let main_tip = self.main_tip()?;
        if main_tip.is_none() && refused.contains_key(&main) {
            // The register of the first push lies in the commit that is refused.
            return Err(all_refused(refused));
        }
        let sender = self.sender_of(update, main_tip)?;
        let identity = Identity::of(sender.verified.actor());
        let mut policies = Vec::new();
        let mut proof = None;
        for moved in moves {
            if refused.contains_key(moved.name()) {
                continue;
            }
            let verdict = match sender.ask(moved, also) {
                Ok(allowed) if moved.name() == &main && !sender.first => {
                    // The held statements are read only for a move of `main` that the
                    // authorizer allowed, so a refusal stays a refusal.
                    let held = match &mut context.statements {
                        Some(statements) => statement::read_destination(
                            &mut **statements,
                            record.chain.last_number(),
                        )
                        .map_err(|error| AcceptError::Statements(Box::new(error)))?,
                        None => Vec::new(),
                    };
                    let state = main::HubState {
                        tip: sender.register,
                        record: &record,
                        held: &held,
                        register: sender.register_of(),
                        policies: context.policies,
                        now: context.time,
                    };
                    let verdict = self.gate_on_hub(moved, &state)?;
                    if verdict.is_ok() {
                        proof = Some(allowed);
                    }
                    // The verdict rests on the policy of `T`.
                    verdict.map(|()| Some(sender.register))
                }
                Ok(allowed) => {
                    proof = Some(allowed);
                    self.write_ownership(moved, &record, &sender, &identity, context.time)?
                }
                Err(reason) => Err(reason),
            };
            match verdict {
                Ok(policy) => policies.extend(policy),
                Err(reason) => {
                    refused.insert(moved.name().clone(), reason);
                }
            }
        }
        let allowed = match proof {
            Some(allowed) if refused.is_empty() => allowed,
            // An update names at least one reference, so one that names none that is refused
            // was allowed for every one of them.
            _ => return Err(all_refused(refused)),
        };
        policies.dedup();

        let mut fast_forward = BTreeSet::new();
        for moved in moves {
            if let (Some(old), Some(new)) = (moved.old_commit(), moved.new_commit()) {
                let bases = merge_bases(self.objects(), old, new)
                    .map_err(|error| AcceptError::History(Box::new(error)))?;
                if bases == [old] {
                    fast_forward.insert(moved.name().clone());
                }
            }
        }
        let acceptance = Acceptance {
            time: context.time,
            register: sender.register,
            policies,
            fast_forward,
            actor: identity,
        };
        // The entry is built in memory and handed to the destination whole, so the destination
        // makes it durable in one step, before the first reference moves.
        let mut bytes = Vec::new();
        let entry = record
            .chain
            .append(update, acceptance, context.signer, &mut bytes)
            .map_err(|error| AcceptError::Write(Box::new(error)))?;
        let writer = lock.writer(&allowed);
        context
            .record
            .append(&bytes)
            .map_err(|source| AcceptError::RecordIo {
                step: RecordStep::Append,
                source,
            })?;
        match self.move_references(writer, update) {
            Ok(()) => Ok(entry),
            Err(source) => Err(AcceptError::NotCompleted {
                receipt: Box::new(entry),
                source: Box::new(source),
            }),
        }
    }

    /// The tip of `main`, or `None` in a repository that no first push has reached.
    fn main_tip(&self) -> Result<Option<CommitId>, AcceptError> {
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        Ok(self.refs().branch(&main)?)
    }

    /// Who sends the update: the entry of the register at the tip of `main` that holds its key
    /// as an active key, or, without a `main`, the owner in the register of the commit that the
    /// update sets `main` to (ADR-024). The connectivity of that commit was checked before,
    /// because its register is read.
    fn sender_of(
        &self,
        update: &RefUpdate,
        main_tip: Option<CommitId>,
    ) -> Result<Sender, AcceptError> {
        let unreadable = |error: ModeError| AcceptError::Register(Box::new(error));
        if let Some(tip) = main_tip {
            // A hub holds enforcing repositories only, so a `main` without a register is no
            // state that an accepted update left.
            let register = Register::at_commit(self.objects(), &tip)
                .map_err(unreadable)?
                .ok_or_else(|| unreadable(ModeError::RegisterMissing))?;
            let verified = register
                .verify_ref_update(update)
                .ok_or(Refused::Key { register: tip })?;
            return Ok(Sender {
                register: tip,
                first: false,
                mode: Mode::Enforcing(register),
                verified,
            });
        }
        let main = main_reference();
        let pushed = update
            .payload()
            .moves()
            .iter()
            .find(|moved| moved.name() == &main)
            .and_then(Move::new_commit)
            .ok_or(Refused::NoMain)?;
        let incomplete = |error: &dyn fmt::Display| {
            one_refused(
                &main,
                RefusalReason::Incomplete(Reason::from_message(error)),
            )
        };
        // The connectivity check passes a commit that records an open conflict, which `main` never
        // holds (ADR-019) and which has no one tree to read a register from.
        match self.objects().get_any_commit(&pushed) {
            Ok(Some(AnyCommit::Commit2(_))) => {
                let reason = Reason::from_message(&"the commit records an open conflict");
                return Err(one_refused(&main, RefusalReason::BadRegister(reason)));
            }
            Ok(_) => {}
            Err(error) => return Err(unreadable(ModeError::Store(error))),
        }
        let register = match Register::at_commit(self.objects(), &pushed) {
            Ok(Some(register)) => register,
            Ok(None) => return Err(one_refused(&main, RefusalReason::NoRegister)),
            Err(error @ ModeError::Missing) => return Err(incomplete(&error)),
            Err(error @ (ModeError::Parse(_) | ModeError::NotRegisterFile)) => {
                let reason = RefusalReason::BadRegister(Reason::from_message(&error));
                return Err(one_refused(&main, reason));
            }
            Err(error) => return Err(unreadable(error)),
        };
        let verified = register
            .verify_ref_update(update)
            .filter(|verified| register.is_owner(verified.actor()))
            .ok_or_else(|| one_refused(&main, RefusalReason::NotOwner))?;
        Ok(Sender {
            register: pushed,
            first: true,
            mode: Mode::Enforcing(register),
            verified,
        })
    }

    /// The references of an update whose names cannot be used, each with the reason: a new name
    /// with a reserved prefix, and a new name that another reference holds, in the store or in
    /// the update itself.
    fn refused_names(
        &self,
        moves: &[Move],
    ) -> Result<BTreeMap<QualifiedName, RefusalReason>, AcceptError> {
        let mut refused = BTreeMap::new();
        for (index, moved) in moves.iter().enumerate() {
            let name = moved.name();
            let created = moved.old_commit().is_none();
            if !created {
                continue;
            }
            match self.refs_store().recorded(name.kind(), name.name()) {
                Ok(_) => {}
                Err(error @ RefError::Conflict { .. }) => {
                    // What is in the way is a reference or a directory of them, so it has a
                    // name; anything else in the directories of the references is damage.
                    let with = match &error {
                        RefError::Conflict { with, .. } => with.parse::<BranchName>().ok(),
                        _ => None,
                    };
                    let Some(with) = with else {
                        return Err(error.into());
                    };
                    refused.insert(name.clone(), RefusalReason::Conflict(with));
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
            if require_unreserved(name.name().clone()).is_err() {
                refused.insert(name.clone(), RefusalReason::Reserved);
                continue;
            }
            // Two names of one update can stand in each other's way without a file in the
            // store: the later one is refused, so the entry never names a move that cannot be
            // made.
            let before = moves[..index].iter().find(|before| {
                before.name().kind() == name.kind()
                    && before.new_commit().is_some()
                    && in_the_way(before.name().name(), name.name())
            });
            if let Some(before) = before {
                let reason = RefusalReason::Conflict(before.name().name().clone());
                refused.insert(name.clone(), reason);
            }
        }
        Ok(refused)
    }

    /// The rule of the write ownership for the move of one reference (ADR-025). A tag and
    /// `main` have none. Returns the commit of the policy if the verdict rests on one: the
    /// duration of a lease that has ended. The inner error is the refusal of the reference.
    ///
    /// # Errors
    ///
    /// [`AcceptError::Policy`] if the policy cannot be read from the store. A policy that is
    /// missing, no file, or not valid is a refusal ([`RefusalReason::Policy`]): the sender can
    /// read the same commit and see it.
    fn write_ownership(
        &self,
        moved: &Move,
        record: &Record,
        sender: &Sender,
        identity: &Identity,
        now: i64,
    ) -> Result<Result<Option<CommitId>, RefusalReason>, AcceptError> {
        if moved.name().kind() != RefKind::Branch {
            return Ok(Ok(None));
        }
        let owner = match record.owners.get(moved.name()) {
            Some(owner) if owner.commit.is_some() && &owner.actor != identity => owner,
            _ => return Ok(Ok(None)),
        };
        let policy = match self.policy_at(&sender.register, true) {
            Ok(policy) => policy,
            Err(
                error @ (PolicyError::Missing | PolicyError::NotAFile | PolicyError::Invalid(_)),
            ) => return Ok(Err(RefusalReason::Policy(Reason::from_message(&error)))),
            Err(error) => {
                return Err(AcceptError::Policy {
                    damaged: self.policy_is_damaged(&sender.register),
                    source: Box::new(error),
                })
            }
        };
        let expires = lease::expiry(owner.time, policy.lease_minutes());
        if now < expires {
            return Ok(Err(RefusalReason::Leased {
                holder: owner.actor.clone(),
                expires,
            }));
        }
        Ok(Ok(Some(sender.register)))
    }

    /// Completes the moves of the last entry of the record, if its update stopped between the
    /// entry and its last move (ADR-025). A reference of the entry is at its new value already,
    /// or still at its old one; then the moves are made again, which changes only the
    /// references that did not move.
    ///
    /// # Errors
    ///
    /// [`AcceptError::Diverged`] for a reference that is at neither value: no stop of this
    /// operation leaves it there.
    fn complete(&self, lock: &mut Lock, last: &RecordEntry) -> Result<(), AcceptError> {
        let mut stopped = false;
        for moved in last.update().payload().moves() {
            let name = moved.name();
            let found = match self.refs_store().recorded(name.kind(), name.name()) {
                Ok(found) => found,
                // The name was free when the entry was written, so what holds it came later,
                // past this operation. It is in the way of a reference that the entry sets.
                Err(RefError::Conflict { .. }) if moved.new_commit().is_some() => {
                    return Err(AcceptError::Diverged {
                        name: name.clone(),
                        recorded: moved.new_commit(),
                        found: None,
                    });
                }
                Err(RefError::Conflict { .. }) => None,
                Err(error) => return Err(error.into()),
            };
            if found == moved.new_commit() {
                continue;
            }
            if found != moved.old_commit() {
                return Err(AcceptError::Diverged {
                    name: name.clone(),
                    recorded: moved.new_commit(),
                    found,
                });
            }
            stopped = true;
        }
        if stopped {
            let allowed = self.allowed_again(last)?;
            self.move_references(lock.writer(&allowed), last.update())?;
        }
        Ok(())
    }

    /// The proof for the moves of an entry that is in the record: the question to the
    /// authorizer that was asked when the update was accepted, asked again by the register that
    /// the entry names. The lock opens for a write with a proof only (ADR-002), and the proof of
    /// the call that wrote the entry ended with that call.
    ///
    /// # Errors
    ///
    /// [`AcceptError::Register`] if the store cannot return the register of the entry, and
    /// [`AcceptError::LastEntry`] if that register does not allow the moves (see there).
    fn allowed_again(&self, last: &RecordEntry) -> Result<Allowed, AcceptError> {
        let unstanding = || AcceptError::LastEntry {
            number: last.number(),
        };
        let register = Register::at_commit(self.objects(), &last.register())
            .map_err(|error| AcceptError::Register(Box::new(error)))?
            .ok_or_else(unstanding)?;
        let verified = register
            .verify_ref_update(last.update())
            .filter(|verified| &Identity::of(verified.actor()) == last.actor())
            .ok_or_else(unstanding)?;
        let sender = Sender {
            register: last.register(),
            first: false,
            mode: Mode::Enforcing(register),
            verified,
        };
        let mut proof = None;
        for moved in last.update().payload().moves() {
            proof = Some(sender.ask(moved, &AllowAll).map_err(|_| unstanding())?);
        }
        proof.ok_or_else(unstanding)
    }

    /// Checks that the branches and the tags are where the record leaves them: every reference
    /// of the store is the commit of the last entry of its name, and every name that the record
    /// leaves at a commit is a reference. The record is the order of the hub, so an update is
    /// judged only on a store that agrees with it.
    fn require_agreement(&self, record: &Record) -> Result<(), AcceptError> {
        let refs = self.refs();
        let branches = refs
            .branches()?
            .into_iter()
            .map(|found| (RefKind::Branch, found));
        let tags = refs.tags()?.into_iter().map(|found| (RefKind::Tag, found));
        let found: BTreeMap<QualifiedName, CommitId> = branches
            .chain(tags)
            .map(|(kind, (name, commit))| (QualifiedName::new(kind, name), commit))
            .collect();
        let recorded = record
            .owners
            .iter()
            .filter_map(|(name, owner)| owner.commit.map(|commit| (name, commit)));
        let names: BTreeSet<&QualifiedName> = found
            .keys()
            .chain(recorded.clone().map(|(name, _)| name))
            .collect();
        for name in names {
            let recorded = record.owners.get(name).and_then(|owner| owner.commit);
            let found = found.get(name).copied();
            if found != recorded {
                return Err(AcceptError::Diverged {
                    name: name.clone(),
                    recorded,
                    found,
                });
            }
        }
        Ok(())
    }

    /// Whether a policy that could not be read is damage of the store: the commit, its tree, or
    /// the file of the policy is missing or not in canonical form. The error of the policy
    /// carries text only, so the objects are read once more, and only what that read finds
    /// counts as damage. A failure of the storage is none, and neither is a read that succeeds
    /// now: the first failure was then one of the storage that passed.
    fn policy_is_damaged(&self, tip: &CommitId) -> bool {
        let store = self.objects();
        let damage =
            |error: &ReadError| !matches!(error.store_error(), Some(StoreError::Io { .. }));
        let commit = match store.get_commit(tip) {
            Ok(Some(commit)) => commit,
            Ok(None) => return true,
            Err(error) => return damage(&error),
        };
        let tree = match store.get_tree(&commit.tree()) {
            Ok(Some(tree)) => tree,
            Ok(None) => return true,
            Err(error) => return damage(&error),
        };
        let Some(entry) = tree
            .entries()
            .iter()
            .find(|entry| entry.name() == crate::policy::FILE_NAME)
        else {
            // The first read found a file of the policy here.
            return false;
        };
        match store.get(&entry.id()) {
            Ok(Some(_)) | Err(StoreError::Io { .. }) => false,
            Ok(None) | Err(_) => true,
        }
    }

    /// Makes the references agree with an update whose entry is in the record: the moves of an
    /// accepted update, and the completion of one that stopped. The deletions come first. No
    /// accepted update needs that order today, because a new name that a reference of the store
    /// holds is refused even if the same update deletes that reference; the order keeps the
    /// moves right if that rule is ever relaxed.
    fn move_references(&self, lock: &WriteLock, update: &RefUpdate) -> Result<(), RefError> {
        let moves = update.payload().moves();
        let deletions = moves.iter().filter(|moved| moved.new_commit().is_none());
        let others = moves.iter().filter(|moved| moved.new_commit().is_some());
        for moved in deletions.chain(others) {
            let name = moved.name();
            self.refs_store().set_recorded(
                lock,
                name.kind(),
                name.name(),
                moved.new_commit(),
                self.objects(),
            )?;
        }
        Ok(())
    }
}

// The second entry of the accepting operation, for a signed statement (STEMMA-299).
mod statement;

// The gate on a hub (STEMMA-303).
mod main;

pub use statement::{
    held_in, record_in, statements_of_open_pulls, RangeError, ReadAccess, StatementDestination,
    StatementError,
};

#[cfg(test)]
mod tests;
