//! The anchors on a hub (ADR-026, STEMMA-300): [`Repository::accept_anchor`] takes the anchor of
//! an operation log and answers with its confirmation or with a typed refusal, and
//! [`anchors_in`] answers the request that reads the anchors of a log.
//!
//! The operation shares the lock of the references with the accepting operation of a ref update,
//! so the record number that a confirmation names places it in the one order of the hub: a log
//! entry that a confirmed anchor covers stands before every later record entry (ADR-026).

use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use super::{AcceptContext, AcceptError, Lock, Record, RecordStep};
use crate::protocol::anchor::{
    read_held_anchors, Anchor, AnchorLines, AnchorRefusal, AnchorState, HeldAnchor,
    HeldAnchorParseError, LogId,
};
use crate::protocol::record::SealError;
use crate::protocol::ErrorMessage;
use crate::register::{Identity, ModeError, Register};
use crate::repository::{ReadAccess, Repository};

/// The destination of the anchors of one log of a repository (ADR-026): the held anchors one
/// after the other, in the order of their numbers. The layout of a repository holds no file for
/// them (ADR-009); the hub keeps them where it keeps its data, per repository and log ID, and
/// passes the way to the list of the log that an anchor names with every call.
///
/// [`Repository::accept_anchor`] calls both methods under the lock of the references. The
/// reader of the anchors ([`anchors_in`]) calls [`read`](Self::read) without the lock, so `read`
/// answers what is durable and nothing that is still being written.
pub trait AnchorDestination {
    /// The held anchors: the bytes of every one whose [`append`](Self::append) returned `Ok`,
    /// in their order, and nothing else. A log without an anchor has none.
    ///
    /// # Errors
    ///
    /// The error of the storage.
    fn read(&mut self) -> io::Result<Vec<u8>>;

    /// Appends the bytes of one held anchor and makes them durable. When the call returns
    /// `Ok`, the anchor survives a crash and the answer leaves the hub.
    ///
    /// # Errors
    ///
    /// The error of the storage. The destination is then as it was before the call: an
    /// implementation cuts back what it wrote.
    fn append(&mut self, held: &[u8]) -> io::Result<()>;
}

/// Why an anchor was not confirmed. [`Refused`](Self::Refused) is the answer to the sender
/// ([`answer`](Self::answer)); every other variant is a failure of the hub.
#[derive(Debug)]
#[non_exhaustive]
pub enum AnchorError {
    /// The repository has a working copy; anchors are held by a bare repository on a hub.
    NotBare {
        /// The root of the repository.
        path: PathBuf,
    },
    /// The anchor is refused. A contradicting anchor ([`AnchorRefusal::Contradicts`]) is in the
    /// list as evidence; every other refusal wrote nothing.
    Refused(AnchorRefusal),
    /// A failure that the accepting operation of a ref update shares: the lock, the record, the
    /// references, or the register at the tip of `main`.
    Hub(Box<AcceptError>),
    /// The list of the anchors could not be read, or the anchor could not be appended.
    AnchorsIo {
        /// What was tried.
        step: RecordStep,
        /// The error of the destination.
        source: io::Error,
    },
    /// The list of the anchors is damaged: a held anchor is refused by its reader.
    Anchors(Box<HeldAnchorParseError>),
    /// The list of the anchors is damaged: a held anchor names another log or another
    /// repository, or its number is out of place. Carries its position, from 1.
    Order {
        /// The position of the held anchor in the list.
        position: usize,
    },
    /// The time of the hub lies before the time of the last held anchor: its clock stepped back.
    Time {
        /// The time of the hub.
        now: i64,
        /// The time of the last held anchor.
        last: i64,
    },
    /// The anchor could not be sealed: the signer of the hub failed or its signature does not
    /// verify.
    Seal(Box<SealError>),
}

impl AnchorError {
    /// The error message that answers the sender: [`ErrorMessage::AnchorRefused`] for a refused
    /// anchor, `None` for a failure of the hub.
    pub fn answer(&self) -> Option<ErrorMessage> {
        match self {
            Self::Refused(refusal) => Some(ErrorMessage::AnchorRefused(refusal.clone())),
            _ => None,
        }
    }

    /// The error of the file system, if that is what failed.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::AnchorsIo { source, .. } => Some(source),
            Self::Hub(error) => error.io_error(),
            Self::Seal(error) => error.io_error(),
            Self::NotBare { .. }
            | Self::Refused(_)
            | Self::Anchors(_)
            | Self::Order { .. }
            | Self::Time { .. } => None,
        }
    }
}

impl fmt::Display for AnchorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotBare { path } => write!(
                f,
                "{} has a working copy; an anchor is held by a bare repository only",
                path.display()
            ),
            Self::Refused(refusal) => refusal.fmt(f),
            Self::Hub(error) => error.fmt(f),
            Self::AnchorsIo { step, source } => {
                let step = match step {
                    RecordStep::Read => "read",
                    RecordStep::Append => "append to",
                };
                write!(f, "cannot {step} the anchors of the log: {source}")
            }
            Self::Anchors(error) => write!(f, "the anchors of the log are damaged: {error}"),
            Self::Order { position } => write!(
                f,
                "the anchors of the log are damaged: held anchor {position} names another log or \
                 repository, or is out of place"
            ),
            Self::Time { now, last } => write!(
                f,
                "the time of the hub, {now}, lies before the time of the last held anchor, {last}"
            ),
            Self::Seal(error) => write!(f, "cannot seal the anchor: {error}"),
        }
    }
}

// Each message is or includes the message of its cause.
impl Error for AnchorError {}

impl From<AcceptError> for AnchorError {
    fn from(error: AcceptError) -> Self {
        Self::Hub(Box::new(error))
    }
}

impl From<AnchorRefusal> for AnchorError {
    fn from(refusal: AnchorRefusal) -> Self {
        Self::Refused(refusal)
    }
}

/// Reads the list of the anchors of `log` and checks that each names the log and the repository
/// and has its place.
fn read_list(
    destination: &mut dyn AnchorDestination,
    log: LogId,
    repository: crate::protocol::RepositoryId,
) -> Result<Vec<HeldAnchor>, AnchorError> {
    let bytes = destination
        .read()
        .map_err(|source| AnchorError::AnchorsIo {
            step: RecordStep::Read,
            source,
        })?;
    let held = read_held_anchors(&bytes).map_err(|error| AnchorError::Anchors(Box::new(error)))?;
    for (index, one) in held.iter().enumerate() {
        if one.log() != log
            || one.anchor().payload().repository() != repository
            || one.number() != index as u64 + 1
        {
            return Err(AnchorError::Order {
                position: index + 1,
            });
        }
    }
    Ok(held)
}

impl Repository {
    /// Takes the anchor of an operation log on a bare repository (ADR-026): checks it, keeps it
    /// in the list of its log with the lines and the signature of the hub, and returns the held
    /// anchor, which in the state `confirmed` is the confirmation.
    ///
    /// The checks, in this order:
    ///
    /// - **Before the lock** the address of the hub and the ID of the repository are compared
    ///   with the payload. The signature of the actor was checked when the anchor was read.
    /// - **Under the lock of the references**, the same lock as that of a ref update, the record
    ///   is read and an update that stopped after its entry is completed, as the accepting
    ///   operation of a statement does. The key of the anchor must be an active key of the
    ///   register at the tip of `main`.
    /// - **The list of the log** is read from `anchors`, which the caller chose by the log ID of
    ///   the anchor. The anchor is compared with the last confirmed anchor of the list: the same
    ///   entry is the same anchor, and its confirmation is returned again without a new entry; a
    ///   larger number is confirmed; the same number with another hash, or a smaller number,
    ///   contradicts it.
    ///
    /// A contradicting anchor is appended in the state `contradicts`, with the signature of the
    /// actor as evidence, and the call fails with [`AnchorRefusal::Contradicts`], which names
    /// the anchor that the hub holds; the same anchor sent again is answered alike and not
    /// appended twice. It blocks nothing: ref updates count by their proofs. The
    /// hub sees no entry of the log, only a number and a hash. The confirmation names the time
    /// of the hub and the number of the last record entry, which places it in the order of the
    /// hub.
    ///
    /// # Errors
    ///
    /// [`AnchorError::Refused`] with the typed reason if the anchor names another hub or another
    /// repository, the repository has no `main`, the register does not hold its key as an
    /// active key, or the anchor contradicts the list of its log. It is answered to the sender
    /// ([`AnchorError::answer`]).
    ///
    /// Every other error is a failure of the hub: [`AnchorError::NotBare`] for a repository with
    /// a working copy, [`AnchorError::Hub`] for the lock, the record, the references, and the
    /// register, [`AnchorError::AnchorsIo`], [`AnchorError::Anchors`], and
    /// [`AnchorError::Order`] for a list that cannot be read or written or is damaged,
    /// [`AnchorError::Time`] for a clock that stepped back, and [`AnchorError::Seal`] if the
    /// signer fails.
    pub fn accept_anchor(
        &self,
        anchor: &Anchor,
        context: AcceptContext<'_>,
        anchors: &mut dyn AnchorDestination,
    ) -> Result<HeldAnchor, AnchorError> {
        if !self.is_bare() {
            return Err(AnchorError::NotBare {
                path: self.path().to_path_buf(),
            });
        }
        let payload = anchor.payload();
        if payload.hub() != context.hub {
            return Err(AnchorRefusal::Hub {
                called: context.hub.clone(),
            }
            .into());
        }
        if payload.repository() != context.repository {
            return Err(AnchorRefusal::Repository {
                id: context.repository,
            }
            .into());
        }
        // The lock is held from here on and opens no write: an anchor moves no reference,
        // unless an earlier update is completed.
        let mut lock = Lock::new(self.refs_store().lock_held().map_err(AcceptError::from)?);
        let record = Record::read(context.record, context.repository)?;
        if let Some(last) = record.last() {
            self.complete(&mut lock, last)?;
        }
        self.require_agreement(&record)?;
        let tip = self.main_tip()?.ok_or(AnchorRefusal::NoMain)?;
        let unreadable = |error: ModeError| AcceptError::Register(Box::new(error));
        let register = Register::at_commit(self.objects(), &tip)
            .map_err(unreadable)?
            .ok_or_else(|| unreadable(ModeError::RegisterMissing))?;
        let verified = register
            .verify_anchor(anchor)
            .ok_or(AnchorRefusal::Key { register: tip })?;

        let held = read_list(anchors, payload.log(), context.repository)?;
        let confirmed = held.iter().rev().find(|held| held.is_confirmed());
        // A contradiction that the list holds already is evidence enough: the same anchor sent
        // again is answered alike and not appended twice.
        if let Some(last) = confirmed {
            if held
                .iter()
                .any(|one| !one.is_confirmed() && one.anchor() == anchor)
            {
                return Err(AnchorRefusal::Contradicts { held: last.tip() }.into());
            }
        }
        let state = match confirmed {
            Some(last) if last.tip() == payload.tip() => return Ok(last.clone()),
            Some(last) if payload.tip().number <= last.tip().number => AnchorState::Contradicts,
            _ => AnchorState::Confirmed,
        };
        if let Some(last) = held
            .last()
            .map(HeldAnchor::time)
            .filter(|last| context.time < *last)
        {
            return Err(AnchorError::Time {
                now: context.time,
                last,
            });
        }
        let lines = AnchorLines {
            number: held.len() as u64 + 1,
            time: context.time,
            register: tip,
            record: record.chain.last_number(),
            actor: Identity::of(verified.actor()),
            state,
        };
        let sealed = HeldAnchor::seal(anchor.clone(), lines, context.signer)
            .map_err(|error| AnchorError::Seal(Box::new(error)))?;
        anchors
            .append(&sealed.encode())
            .map_err(|source| AnchorError::AnchorsIo {
                step: RecordStep::Append,
                source,
            })?;
        match (state, confirmed) {
            (AnchorState::Contradicts, Some(last)) => {
                Err(AnchorRefusal::Contradicts { held: last.tip() }.into())
            }
            _ => Ok(sealed),
        }
    }
}

/// The anchors of one log, for the request that reads them (ADR-026). The reading takes no
/// lock; the destination answers only what is durable ([`AnchorDestination::read`]). The
/// anchors are readable by everyone who may read the repository, the contradicting ones too.
///
/// # Errors
///
/// [`AnchorError::Refused`] is never returned. [`AnchorError::AnchorsIo`] if the destination
/// cannot be read, and [`AnchorError::Anchors`] and [`AnchorError::Order`] if it is damaged.
/// For [`ReadAccess::Denied`] the result is `Ok(None)`, before the destination is read: the
/// answer is [`ErrorMessage::NotReadable`].
pub fn anchors_in(
    destination: &mut dyn AnchorDestination,
    log: LogId,
    repository: crate::protocol::RepositoryId,
    access: ReadAccess,
) -> Result<Option<Vec<HeldAnchor>>, AnchorError> {
    if access == ReadAccess::Denied {
        return Ok(None);
    }
    read_list(destination, log, repository).map(Some)
}

#[cfg(test)]
mod tests;
