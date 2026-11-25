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
use super::{PolicyError, Repository};
use crate::allowed::Allowed;
use crate::conflict::AnyCommit;
use crate::history::HistoryError;
use crate::merge_base::merge_bases;
use crate::object::CommitId;
use crate::oplog::OperationName;
use crate::protocol::record::{
    read_record, Acceptance, Chain, ChainError, HubAddress, Move, RecordEntry, RecordParseError,
    RefUpdate, Stale, WriteError,
};
use crate::protocol::refusal::{RefusalReason, Refused, RefusedReference, RefusedReferences};
use crate::protocol::{ErrorMessage, QualifiedName, Reason, RepositoryId};
use crate::pull::TARGET;
use crate::receive::ConnectivityError;
use crate::refs::{BranchName, RefError, RefKind, WriteLock};
use crate::register::{Identity, Mode, ModeError, Register, RegisterAuthorizer, Verified};
use crate::signature::Signer;
use crate::store::ObjectStore;
use crate::{lease, AllowAll, Authorizer};

/// The destination of the record of ref updates of one repository (ADR-025). The layout of a
/// repository holds no file for the record (ADR-009): the hub keeps it where it keeps its data
/// and passes the way to it with every call.
///
/// The accepting operation calls both methods under the lock of the references, so no other
/// writer of the same repository runs between them. There is one exception: in a repository
/// without `main` it calls [`read`](Self::read) once before it takes the lock, to learn whether
/// a first push stopped after its entry. That read may meet the entry of another process half
/// written; the operation takes nothing from a record that it cannot read there and reads again
/// under the lock.
pub trait RecordDestination {
    /// The whole record: the bytes of every entry that [`append`](Self::append) took, in their
    /// order, and nothing else. A repository without an accepted update has an empty record.
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

/// What a hub passes to the accepting operation with a ref update (ADR-025): who it is, which
/// repository the update was sent to, where the record lies, its time, and its signer. The
/// slices of the gate add to it (the namespace policy, the statements), so a caller outside
/// the crate builds it with [`new`](Self::new).
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
        }
    }
}

impl fmt::Debug for AcceptContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The record and the signer are ways to a storage and to a key, which have no text.
        f.debug_struct("AcceptContext")
            .field("hub", &self.hub)
            .field("repository", &self.repository)
            .field("time", &self.time)
            .finish_non_exhaustive()
    }
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
    /// The lock of the references or the references failed. Nothing was written, unless an
    /// earlier update was being completed.
    Refs(Box<RefError>),
    /// The record could not be read or the entry could not be appended. No reference of this
    /// update moved.
    RecordIo {
        /// What was tried.
        operation: &'static str,
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
    /// The register at the tip of `main` cannot be read, or `main` holds none. For a first
    /// push it is the register of the pushed commit that the store could not return.
    Register(Box<ModeError>),
    /// The policy at the tip of `main` could not be read: the storage failed. A policy that is
    /// missing or not valid is [`Refused`](Self::Refused) instead.
    Policy(Box<PolicyError>),
    /// The connectivity check could not run: the storage failed. An update whose objects do not
    /// hold together is [`Refused`](Self::Refused) instead.
    Connectivity(Box<ConnectivityError>),
    /// The history between the old and the new commit of a branch cannot be read.
    History(Box<HistoryError>),
    /// The entry could not be made: the signer of the hub failed, or its signature does not
    /// verify. Nothing was written.
    Write(Box<WriteError>),
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
            | Self::Policy(_)
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
            Self::RecordIo { operation, source } => {
                write!(f, "cannot {operation} the record of ref updates: {source}")
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
                    "cannot read the register at the tip of `{TARGET}`: {error}"
                )
            }
            Self::Policy(error) => error.fmt(f),
            Self::Connectivity(error) => error.fmt(f),
            Self::History(error) => error.fmt(f),
            Self::Write(error) => error.fmt(f),
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

/// The refusal of one reference.
fn one_refused(name: &QualifiedName, reason: RefusalReason) -> AcceptError {
    Refused::References(
        RefusedReferences::try_new(vec![RefusedReference {
            name: name.clone(),
            reason,
        }])
        .expect("one reference is at least one, and it is named once"),
    )
    .into()
}

/// The refusal of the references of `refused`, which is not empty.
fn all_refused(refused: BTreeMap<QualifiedName, RefusalReason>) -> AcceptError {
    let references = refused
        .into_iter()
        .map(|(name, reason)| RefusedReference { name, reason })
        .collect();
    Refused::References(
        RefusedReferences::try_new(references)
            .expect("the map is not empty, and a key of a map stands once"),
    )
    .into()
}

/// The name of the operation that the authorizer is asked for: `push` for a branch, `tag` to
/// set a tag, and `untag` to delete one.
fn operation_of(moved: &Move) -> OperationName {
    let name = match (moved.name().kind(), moved.new_commit()) {
        (RefKind::Branch, _) => "push",
        (RefKind::Tag, Some(_)) => "tag",
        (RefKind::Tag, None) => "untag",
    };
    OperationName::try_new(name).expect("a valid operation name")
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
    /// Asks the authorizer of the register, and `also`, for the move of one reference. Both
    /// must allow it.
    fn ask(&self, moved: &Move, also: &dyn Authorizer) -> Result<Allowed, RefusalReason> {
        let operation = operation_of(moved);
        let target = moved.name().name().as_str();
        let actor = self.verified.actor();
        let authorizer = RegisterAuthorizer::new(&self.mode, Some(&self.verified));
        match (
            Allowed::ask(&authorizer, actor, &operation, target),
            Allowed::ask(also, actor, &operation, target),
        ) {
            (Some(allowed), Some(_)) => Ok(allowed),
            _ => Err(RefusalReason::Denied { operation }),
        }
    }

    /// Asks for every reference of the update, and returns the proof that all were allowed.
    fn ask_all(&self, update: &RefUpdate, also: &dyn Authorizer) -> Result<Allowed, AcceptError> {
        let mut refused = BTreeMap::new();
        let mut proof = None;
        for moved in update.payload().moves() {
            match self.ask(moved, also) {
                Ok(allowed) => proof = Some(allowed),
                Err(reason) => {
                    refused.insert(moved.name().clone(), reason);
                }
            }
        }
        match proof {
            Some(allowed) if refused.is_empty() => Ok(allowed),
            _ => Err(all_refused(refused)),
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
    /// The last entry, whose moves may not be made yet.
    last: Option<RecordEntry>,
    owners: BTreeMap<QualifiedName, Owner>,
}

impl Record {
    fn read(
        destination: &mut dyn RecordDestination,
        repository: RepositoryId,
    ) -> Result<Self, AcceptError> {
        let bytes = destination.read().map_err(|source| AcceptError::RecordIo {
            operation: "read",
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
            last: entries.into_iter().next_back(),
            owners,
        })
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
    ///   with the payload, finds the sender by the references as they are, and asks the
    ///   authorizer once, because the lock is opened with the proof of that question only
    ///   (ADR-002). This pass can refuse an update for its sender (the key, the first push, the
    ///   authorizer) before the lock is taken, so such an update is answered as refused even if
    ///   it is stale as well. It accepts nothing: everything is decided again under the lock.
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
    ///   update of `main` is refused: it belongs to the merge gate, which comes with its own
    ///   slice.
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
    /// [`AcceptError::History`] if the store fails, [`AcceptError::Write`] if the signer fails,
    /// and
    /// [`AcceptError::NotCompleted`] if the entry is written and a reference did not move.
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
        context: AcceptContext<'_>,
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
        // The lock is opened with the proof that the authorizer allowed the operation
        // (ADR-002), so the sender is asked once by the references as they are. Everything is
        // decided again under the lock.
        let tip = match self.main_tip()? {
            Some(tip) => Some(tip),
            None => self.main_of_stopped_first_push(context.record, context.repository)?,
        };
        let allowed = self.sender_of(update, tip, false)?.ask_all(update, also)?;
        let lock = self.refs_store().lock_writer(&allowed)?;

        let mut record = Record::read(context.record, context.repository)?;
        if let Some(last) = &record.last {
            self.complete(&lock, last)?;
        }
        self.require_agreement(&record)?;
        match record.chain.check(update) {
            Ok(()) => {}
            Err(ChainError::Stale(stale)) => return Err(AcceptError::Stale(stale)),
            Err(error) => return Err(AcceptError::Chain(Box::new(error))),
        }

        let moves = payload.moves();
        let mut refused = self.refused_names(&lock, moves)?;
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
        let sender = self.sender_of(update, main_tip, true)?;
        let identity = Identity::of(sender.verified.actor());
        let mut policies = Vec::new();
        for moved in moves {
            if refused.contains_key(moved.name()) {
                continue;
            }
            let verdict = if moved.name() == &main && !sender.first {
                Err(RefusalReason::Gate)
            } else {
                match sender.ask(moved, also) {
                    Ok(_) => {
                        self.write_ownership(moved, &record, &sender, &identity, context.time)?
                    }
                    Err(reason) => Err(reason),
                }
            };
            match verdict {
                Ok(policy) => policies.extend(policy),
                Err(reason) => {
                    refused.insert(moved.name().clone(), reason);
                }
            }
        }
        if !refused.is_empty() {
            return Err(all_refused(refused));
        }
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
        context
            .record
            .append(&bytes)
            .map_err(|source| AcceptError::RecordIo {
                operation: "append to",
                source,
            })?;
        match self.move_references(&lock, update) {
            Ok(()) => Ok(entry),
            Err(source) => Err(AcceptError::NotCompleted {
                receipt: Box::new(entry),
                source: Box::new(source),
            }),
        }
    }

    /// The commit that the record leaves `main` at, for the pass before the lock in a
    /// repository without `main`: a first push that stopped after its entry left no reference,
    /// and the record names it. Without this the pass would answer `no-main` to every other
    /// sender, and the completion under the lock would never be reached.
    ///
    /// The record is read without the lock here, so a first push of another process may be
    /// writing its entry. A record that cannot be read is therefore no damage yet: beside a
    /// writer the answer is that the references are locked, as it would be a moment later, and
    /// otherwise the record counts as unknown and the pass under the lock reads it again.
    fn main_of_stopped_first_push(
        &self,
        record: &mut dyn RecordDestination,
        repository: RepositoryId,
    ) -> Result<Option<CommitId>, AcceptError> {
        match Record::read(record, repository) {
            Ok(record) => Ok(record
                .owners
                .get(&main_reference())
                .and_then(|main| main.commit)),
            Err(_) => {
                self.refs_store().refuse_beside_writer()?;
                Ok(None)
            }
        }
    }

    /// The tip of `main`, or `None` in a repository that no first push has reached.
    fn main_tip(&self) -> Result<Option<CommitId>, AcceptError> {
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        Ok(self.refs().branch(&main)?)
    }

    /// Who sends the update: the entry of the register at the tip of `main` that holds its key
    /// as an active key, or, without a `main`, the owner in the register of the commit that the
    /// update sets `main` to (ADR-024). `checked` says that the connectivity of that commit was
    /// checked already; without it the commit is checked here, because its register is read.
    fn sender_of(
        &self,
        update: &RefUpdate,
        main_tip: Option<CommitId>,
        checked: bool,
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
        if !checked {
            match self.check_connectivity(&[pushed]) {
                Ok(()) => {}
                Err(error) if error.is_incomplete() => return Err(incomplete(&error)),
                Err(error) => return Err(AcceptError::Connectivity(Box::new(error))),
            }
        }
        // The check above passes a commit that records an open conflict, which `main` never
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
        lock: &WriteLock,
        moves: &[Move],
    ) -> Result<BTreeMap<QualifiedName, RefusalReason>, AcceptError> {
        let mut refused = BTreeMap::new();
        for (index, moved) in moves.iter().enumerate() {
            let name = moved.name();
            let created = moved.old_commit().is_none();
            if !created {
                continue;
            }
            match self.refs_store().recorded(lock, name.kind(), name.name()) {
                Ok(_) => {}
                Err(RefError::Conflict { with, .. }) => {
                    let reason = RefusalReason::Conflict(Reason::from_message(&with));
                    refused.insert(name.clone(), reason);
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
                let reason = RefusalReason::Conflict(Reason::from_message(before.name().name()));
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
            Err(error) => return Err(AcceptError::Policy(Box::new(error))),
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
    fn complete(&self, lock: &WriteLock, last: &RecordEntry) -> Result<(), AcceptError> {
        let mut stopped = false;
        for moved in last.update().payload().moves() {
            let name = moved.name();
            let found = match self.refs_store().recorded(lock, name.kind(), name.name()) {
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
            self.move_references(lock, last.update())?;
        }
        Ok(())
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

#[cfg(test)]
mod tests;
