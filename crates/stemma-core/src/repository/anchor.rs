//! The anchors of the operation log in a clone (ADR-026, STEMMA-300): the ID of the log, the
//! anchor that the clone signs, the last confirmation that it keeps, `op verify` against the
//! anchors that a hub holds, and the register as it counts in this log.
//!
//! Two files of the metadata directory belong to it (ADR-009, version 19 of the format):
//!
//! ```text
//! <root>/log-id               the ID of the log: `stemma-log-id 1` and 32 hex digits
//! <root>/anchors/<remote>     the last confirmation of the hub of a remote, as the hub sent it
//! ```
//!
//! `init` and `clone` write the ID; a repository from before gets it when it is first needed
//! ([`Repository::log_id`]). The confirmation is kept per remote, as each hub confirms the
//! anchors that it holds, and it is a held anchor in the state `confirmed`, which carries the
//! signature of the hub, so the file checks itself.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{raise_format, Repository, FORMAT_VERSION};
use crate::fault;
use crate::object::CommitId;
use crate::oplog::{Entry, EntryHash, OpLogError, Record};
use crate::protocol::anchor::{Anchor, AnchorPayload, HeldAnchor, LogId, Point};
use crate::protocol::record::{HubAddress, SealError};
use crate::protocol::ParseError;
use crate::refs::RefError;
use crate::register::{Identity, KeyEntry, KeyState, Register};
use crate::remote::{HubKeyError, RemoteError, RemoteName};
use crate::repository::FETCH;
use crate::signature::Signer;
use crate::store::{flush_directory, unique_temp_name, StoreError};

/// The name of the file of the ID of the log, in the metadata directory that every worktree
/// shares.
pub(super) const LOG_ID_FILE: &str = "log-id";

/// The first line of the file of the ID of the log.
const LOG_ID_HEADER: &str = "stemma-log-id 1";

/// The directory of the metadata directory that holds the last confirmation per remote.
const ANCHORS_DIR: &str = "anchors";

/// The longest file of a confirmation that is read. A held anchor is far below it.
const MAX_CONFIRMATION_LEN: u64 = 64 * 1024;

/// The content of the file of the ID of the log.
pub(super) fn log_id_content(log: LogId) -> String {
    format!("{LOG_ID_HEADER}\n{log}\n")
}

/// Reads the content of the file of the ID of the log, and nothing but what
/// [`log_id_content`] writes.
fn parse_log_id(bytes: &[u8]) -> Option<LogId> {
    let text = std::str::from_utf8(bytes).ok()?;
    let rest = text.strip_prefix(LOG_ID_HEADER)?.strip_prefix('\n')?;
    let id = rest.strip_suffix('\n')?;
    let log: LogId = id.parse().ok()?;
    (log_id_content(log) == text).then_some(log)
}

/// What [`Repository::verify_anchors`] found: the anchors of this log that the hub confirmed and
/// that the log still holds, and the contradicting anchors, which the hub keeps as evidence and
/// which every reader of the repository sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorCheck {
    /// How many confirmed anchors the log holds.
    pub confirmed: usize,
    /// The entry of the last confirmed anchor, or `None` if the hub confirmed none.
    pub last: Option<Point>,
    /// The anchors of this log that the hub holds as contradicting: a log of this ID that sent
    /// another hash for a number, or a smaller number, such as a restored backup. They block
    /// nothing; they are shown.
    pub contradictions: Vec<HeldAnchor>,
}

/// What is wrong with the log against an anchor of a hub, or with a held anchor itself.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AnchorFinding {
    /// The log ends before the entry of a confirmed anchor: its tail was cut. Carries the entry
    /// and the number of the last entry of the log.
    Cut {
        /// The entry of the anchor.
        anchor: Point,
        /// The number of the last entry of the log, or `None` for an empty log.
        last: Option<u64>,
    },
    /// The entry of a confirmed anchor has another hash, or is outside the hash chain: the log
    /// was rewritten. A stub keeps the hash of the entry that it replaces, so a purge is no
    /// rewrite. Carries the entry and the hash found.
    Rewritten {
        /// The entry of the anchor.
        anchor: Point,
        /// The hash of the entry in the log, or `None` for an entry outside the chain.
        found: Option<EntryHash>,
    },
    /// A held anchor is not signed by the pinned key of the hub. Carries its position, from 1.
    HubKey {
        /// The position of the held anchor.
        position: usize,
    },
    /// A held anchor names another log or another repository. Carries its position, from 1.
    Other {
        /// The position of the held anchor.
        position: usize,
    },
}

impl fmt::Display for AnchorFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cut { anchor, last } => match last {
                Some(last) => write!(
                    f,
                    "the hub confirmed entry {} with the hash {}, and the log ends at entry \
                     {last}: its tail was cut",
                    anchor.number, anchor.hash
                ),
                None => write!(
                    f,
                    "the hub confirmed entry {} with the hash {}, and the log is empty",
                    anchor.number, anchor.hash
                ),
            },
            Self::Rewritten { anchor, found } => match found {
                Some(found) => write!(
                    f,
                    "the hub confirmed entry {} with the hash {}, and the entry has the hash \
                     {found}: the log was rewritten",
                    anchor.number, anchor.hash
                ),
                None => write!(
                    f,
                    "the hub confirmed entry {} with the hash {}, and the entry is outside the \
                     hash chain: the log was rewritten",
                    anchor.number, anchor.hash
                ),
            },
            Self::HubKey { position } => write!(
                f,
                "held anchor {position} is not signed by the pinned key of the hub"
            ),
            Self::Other { position } => write!(
                f,
                "held anchor {position} names another log or another repository"
            ),
        }
    }
}

impl Error for AnchorFinding {}

/// Why an operation on the anchors of the log of a clone failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum LogAnchorError {
    /// The repository is bare, or the lock of the references is held or fails.
    Refs(Box<RefError>),
    /// The log cannot be read, or its chain does not hold.
    OpLog(Box<OpLogError>),
    /// A file of the anchors cannot be read or written.
    Io {
        /// What was tried.
        operation: &'static str,
        /// The path.
        path: PathBuf,
        /// The error of the operating system.
        source: io::Error,
    },
    /// The repository could not be moved to the format that knows the ID of the log.
    Format(Box<StoreError>),
    /// The file of the ID of the log is not in canonical form.
    LogIdDamaged {
        /// The path of the file.
        path: PathBuf,
    },
    /// The file of the last confirmation of a remote is not a held anchor.
    ConfirmationDamaged {
        /// The path of the file.
        path: PathBuf,
        /// What the reader reported.
        error: Box<ParseError>,
    },
    /// The configuration of a remote cannot be read.
    Remote(Box<RemoteError>),
    /// There is no remote of that name.
    NoRemote(RemoteName),
    /// The configuration of the remote holds no key of the hub, so no confirmation can be
    /// checked.
    HubKey(HubKeyError),
    /// The log has no entry in the hash chain, so there is nothing to anchor. Entries of
    /// versions 1 and 2 of the log stay uncovered by an anchor (ADR-026).
    Unchained,
    /// The log does not hold the entry of the last confirmation with its hash: found before the
    /// anchor is signed, so a local accident is found locally. Carries what is wrong.
    Diverged(AnchorFinding),
    /// The anchor could not be signed.
    Seal(Box<SealError>),
    /// A confirmation is refused: it is a contradicting anchor, it is not signed by the pinned
    /// key, it names another log or repository, or its entry is not in this log as it stands.
    Confirmation(AnchorFinding),
    /// The anchor is in the state `contradicts`, which confirms nothing.
    NotConfirmed,
    /// The confirmation names an earlier entry than the one that is kept: the kept number only
    /// grows.
    Older {
        /// The entry of the kept confirmation.
        kept: Point,
        /// The entry of the offered one.
        offered: Point,
    },
    /// The register of a commit that a `fetch` entry brought cannot be read.
    Register(Box<crate::register::ModeError>),
}

impl fmt::Display for LogAnchorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refs(error) => error.fmt(f),
            Self::OpLog(error) => error.fmt(f),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "cannot {operation} {}: {source}", path.display()),
            Self::Format(error) => error.fmt(f),
            Self::LogIdDamaged { path } => write!(
                f,
                "{} is not `{LOG_ID_HEADER}` and the ID of the log",
                path.display()
            ),
            Self::ConfirmationDamaged { path, error } => {
                write!(f, "{} is not a held anchor: {error}", path.display())
            }
            Self::Remote(error) => error.fmt(f),
            Self::NoRemote(name) => write!(f, "there is no remote `{name}`"),
            Self::HubKey(error) => error.fmt(f),
            Self::Unchained => f.write_str(
                "no entry of the log is in the hash chain yet, so there is no anchor; the next \
                 entry starts it",
            ),
            Self::Diverged(finding) => write!(
                f,
                "the log does not agree with its last confirmed anchor, so no anchor is sent: \
                 {finding}"
            ),
            Self::Seal(error) => write!(f, "cannot sign the anchor: {error}"),
            Self::Confirmation(finding) => write!(f, "the confirmation is refused: {finding}"),
            Self::NotConfirmed => f.write_str(
                "the anchor is held as contradicting the log on the hub, which confirms nothing",
            ),
            Self::Older { kept, offered } => write!(
                f,
                "the confirmation names entry {}, before entry {} of the kept one",
                offered.number, kept.number
            ),
            Self::Register(error) => write!(
                f,
                "cannot read the register of a commit that a fetch brought: {error}"
            ),
        }
    }
}

// Each message is or includes the message of its cause.
impl Error for LogAnchorError {}

impl From<RefError> for LogAnchorError {
    fn from(error: RefError) -> Self {
        Self::Refs(Box::new(error))
    }
}

impl From<OpLogError> for LogAnchorError {
    fn from(error: OpLogError) -> Self {
        Self::OpLog(Box::new(error))
    }
}

impl From<RemoteError> for LogAnchorError {
    fn from(error: RemoteError) -> Self {
        Self::Remote(Box::new(error))
    }
}

fn io_error(operation: &'static str, path: &Path) -> impl FnOnce(io::Error) -> LogAnchorError {
    let path = path.to_path_buf();
    move |source| LogAnchorError::Io {
        operation,
        path,
        source,
    }
}

/// Reads a small file, or `None` if there is none.
fn read_small(path: &Path, operation: &'static str) -> Result<Option<Vec<u8>>, LogAnchorError> {
    let file = match fault::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(operation, path)(error)),
    };
    let mut bytes = Vec::new();
    io::Read::read_to_end(
        &mut io::Read::take(file, MAX_CONFIRMATION_LEN + 1),
        &mut bytes,
    )
    .map_err(io_error(operation, path))?;
    Ok(Some(bytes))
}

/// Writes `bytes` as the file `name` of `dir`: into a temporary file of `scratch`, flushed, and
/// renamed into place, and the directory is flushed, so a reader sees the old or the new file.
fn write_atomic(
    dir: &Path,
    scratch: &Path,
    name: &str,
    bytes: &[u8],
) -> Result<(), LogAnchorError> {
    for made in [scratch, dir] {
        match fault::create_dir(made) {
            Ok(()) => {
                let parent = made.parent().unwrap_or(made);
                flush_directory(parent).map_err(io_error("flush the directory", parent))?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error("make the directory", made)(error)),
        }
    }
    let temp = scratch.join(unique_temp_name());
    let written = (|| {
        let mut file = fault::create_new(&temp)?;
        io::Write::write_all(&mut file, bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(io_error("write the temporary file", &temp)(error));
    }
    let path = dir.join(name);
    if let Err(error) = fault::rename(&temp, &path) {
        let _ = fs::remove_file(&temp);
        return Err(io_error("move the file into place at", &path)(error));
    }
    flush_directory(dir).map_err(io_error("flush the directory", dir))
}

/// Checks that `entries` hold the entry of `point` with its hash.
fn holds(entries: &[Entry], point: Point) -> Result<(), AnchorFinding> {
    let last = entries.last().map(Entry::number);
    let Some(entry) = entries.iter().find(|entry| entry.number() == point.number) else {
        return Err(AnchorFinding::Cut {
            anchor: point,
            last,
        });
    };
    match entry.hash() {
        Some(hash) if hash == point.hash => Ok(()),
        found => Err(AnchorFinding::Rewritten {
            anchor: point,
            found,
        }),
    }
}

/// Checks the anchors of the log `log` that a hub holds against `entries` (ADR-026): every held
/// anchor names the log and the repository and is signed by `hub_key`, and every confirmed one
/// names an entry that the log holds with that hash.
fn check_anchors(
    entries: &[Entry],
    log: LogId,
    repository: crate::protocol::RepositoryId,
    hub_key: &crate::signature::PublicKey,
    held: &[HeldAnchor],
) -> Result<AnchorCheck, AnchorFinding> {
    let mut check = AnchorCheck {
        confirmed: 0,
        last: None,
        contradictions: Vec::new(),
    };
    for (index, anchor) in held.iter().enumerate() {
        let position = index + 1;
        if anchor.hub_key() != *hub_key {
            return Err(AnchorFinding::HubKey { position });
        }
        if anchor.log() != log || anchor.anchor().payload().repository() != repository {
            return Err(AnchorFinding::Other { position });
        }
        if anchor.is_confirmed() {
            holds(entries, anchor.tip())?;
            check.confirmed += 1;
            if check
                .last
                .is_none_or(|last| last.number < anchor.tip().number)
            {
                check.last = Some(anchor.tip());
            }
        } else {
            check.contradictions.push(anchor.clone());
        }
    }
    Ok(check)
}

impl Repository {
    /// The path of a file of the metadata directory that every worktree shares.
    fn shared_path(&self, name: &str) -> PathBuf {
        self.shared.join(name)
    }

    /// The ID of the operation log (ADR-026): read from the file `log-id`, which `init` and
    /// `clone` write. A repository from before gets its ID here, when it is first needed: under
    /// the lock of the references the file is read again, a random ID is made, the repository
    /// moves to the format that knows the file (ADR-009), and the file is written. Every
    /// worktree shares the log and so its ID.
    ///
    /// # Errors
    ///
    /// [`LogAnchorError::LogIdDamaged`] for a file that is not in canonical form,
    /// [`LogAnchorError::Refs`] for the lock, [`LogAnchorError::Format`] if the format cannot
    /// be raised, and [`LogAnchorError::Io`] if the file cannot be read or written or the
    /// random source fails.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn log_id(&self) -> Result<LogId, LogAnchorError> {
        self.require_oplog("read the ID of the log")?;
        if let Some(log) = self.read_log_id()? {
            return Ok(log);
        }
        let _lock = self.refs_store().lock_held()?;
        if let Some(log) = self.read_log_id()? {
            return Ok(log);
        }
        let path = self.shared_path(LOG_ID_FILE);
        let log = LogId::generate().map_err(io_error("make the ID of the log for", &path))?;
        raise_format(&self.shared, FORMAT_VERSION)
            .map_err(|error| LogAnchorError::Format(Box::new(error)))?;
        write_atomic(
            &self.shared,
            &self.shared.join("tmp"),
            LOG_ID_FILE,
            log_id_content(log).as_bytes(),
        )?;
        Ok(log)
    }

    /// The ID of the log if the file names one, without making one.
    fn read_log_id(&self) -> Result<Option<LogId>, LogAnchorError> {
        let path = self.shared_path(LOG_ID_FILE);
        match read_small(&path, "read the ID of the log in")? {
            None => Ok(None),
            Some(bytes) => parse_log_id(&bytes)
                .map(Some)
                .ok_or(LogAnchorError::LogIdDamaged { path }),
        }
    }

    /// The last confirmation of the hub of `remote` that the clone keeps, or `None` before the
    /// first one. It is checked against the pinned key when it is kept, and its own signatures
    /// when it is read.
    ///
    /// # Errors
    ///
    /// [`LogAnchorError::Io`] if the file cannot be read, and
    /// [`LogAnchorError::ConfirmationDamaged`] if it is not a held anchor.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn last_confirmation(
        &self,
        remote: &RemoteName,
    ) -> Result<Option<HeldAnchor>, LogAnchorError> {
        self.require_oplog("read the last confirmation of an anchor")?;
        let path = self.shared.join(ANCHORS_DIR).join(remote.as_str());
        match read_small(&path, "read the confirmation in")? {
            None => Ok(None),
            Some(bytes) => HeldAnchor::parse(&bytes).map(Some).map_err(|error| {
                LogAnchorError::ConfirmationDamaged {
                    path,
                    error: Box::new(error),
                }
            }),
        }
    }

    /// The anchor of the last entry of the log for the hub of `remote` (ADR-026), signed by
    /// `signer`, the key of the actor. `hub` is the address under which the hub is called, which
    /// the anchor binds, as a ref update does.
    ///
    /// Before it signs, the log is checked against the last confirmation that the clone keeps
    /// for the remote: the log must still hold that entry with its hash, so an accident (a
    /// restored backup, a cut tail) is found here and not sent. The anchor names that
    /// confirmation, so the hub holds it as evidence beside the new tip.
    ///
    /// # Errors
    ///
    /// [`LogAnchorError::NoRemote`] and [`LogAnchorError::Remote`] for the configuration, the
    /// errors of [`log_id`](Self::log_id) and of
    /// [`last_confirmation`](Self::last_confirmation), [`LogAnchorError::OpLog`] if the log or
    /// its chain cannot be read, [`LogAnchorError::Unchained`] if its last entry is outside the
    /// chain, [`LogAnchorError::Diverged`] if it does not hold the last confirmation, and
    /// [`LogAnchorError::Seal`] if the signer fails.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn anchor(
        &self,
        remote: &RemoteName,
        hub: HubAddress,
        signer: &dyn Signer,
    ) -> Result<Anchor, LogAnchorError> {
        self.require_oplog("anchor the log")?;
        let config = self
            .remote(remote)?
            .ok_or_else(|| LogAnchorError::NoRemote(remote.clone()))?;
        let log = self.log_id()?;
        let entries = self.oplog().verify()?;
        let last = entries.last().ok_or(LogAnchorError::Unchained)?;
        let tip = Point {
            number: last.number(),
            hash: last.hash().ok_or(LogAnchorError::Unchained)?,
        };
        let confirmed = match self.last_confirmation(remote)? {
            Some(kept) => {
                holds(&entries, kept.tip()).map_err(LogAnchorError::Diverged)?;
                Some(kept.tip())
            }
            None => None,
        };
        let key = signer
            .public_key()
            .map_err(|error| LogAnchorError::Seal(Box::new(SealError::Sign(error))))?;
        let payload = AnchorPayload::try_new(hub, config.repository, log, tip, confirmed, key)
            .expect("the log holds the confirmed entry, so it lies at or before the tip");
        payload
            .sign(signer)
            .map_err(|error| LogAnchorError::Seal(Box::new(error)))
    }

    /// Keeps `held`, the confirmation that the hub of `remote` answered to an anchor, as the last
    /// confirmation of the remote (ADR-026). It is checked first: it is confirmed, signed by the
    /// pinned key of the hub, names this log and the repository of the remote, its entry is in
    /// the log with its hash, and it names no earlier entry than the kept one. Under the lock of
    /// the references the repository moves to the format that knows the file (ADR-009), and the
    /// file `anchors/<remote>` is replaced as a whole.
    ///
    /// # Errors
    ///
    /// [`LogAnchorError::NotConfirmed`] for a contradicting anchor,
    /// [`LogAnchorError::HubKey`] if the remote has no pinned key,
    /// [`LogAnchorError::Confirmation`] for another key, log, or repository, or an entry that
    /// the log does not hold, [`LogAnchorError::Older`], the errors of the configuration, the
    /// log, and its ID, and [`LogAnchorError::Refs`], [`LogAnchorError::Format`], and
    /// [`LogAnchorError::Io`] for the write.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn keep_confirmation(
        &self,
        remote: &RemoteName,
        held: &HeldAnchor,
    ) -> Result<(), LogAnchorError> {
        self.require_oplog("keep the confirmation of an anchor")?;
        if !held.is_confirmed() {
            return Err(LogAnchorError::NotConfirmed);
        }
        let config = self
            .remote(remote)?
            .ok_or_else(|| LogAnchorError::NoRemote(remote.clone()))?;
        let key = config.pinned_key().map_err(LogAnchorError::HubKey)?;
        let log = self.log_id()?;
        let entries = self.oplog().verify()?;
        check_anchors(
            &entries,
            log,
            config.repository,
            key,
            std::slice::from_ref(held),
        )
        .map_err(LogAnchorError::Confirmation)?;
        let _lock = self.refs_store().lock_held()?;
        if let Some(kept) = self.last_confirmation(remote)? {
            if held.tip().number < kept.tip().number {
                return Err(LogAnchorError::Older {
                    kept: kept.tip(),
                    offered: held.tip(),
                });
            }
        }
        raise_format(&self.shared, FORMAT_VERSION)
            .map_err(|error| LogAnchorError::Format(Box::new(error)))?;
        write_atomic(
            &self.shared.join(ANCHORS_DIR),
            &self.shared.join("tmp"),
            remote.as_str(),
            &held.encode(),
        )
    }

    /// `op verify` against the anchors that the hub of `remote` holds for this log (ADR-026):
    /// `held` is the answer to the request that reads them. Every held anchor must be signed by
    /// the pinned key and name this log and the repository; every confirmed one must name an
    /// entry that the log holds with that hash. A stub keeps the hash of the entry that it
    /// replaces, so an anchor stays checkable after a purge. The contradicting anchors are
    /// returned, as the hub shows them to every reader.
    ///
    /// Only who holds the log can make this check; the hub sees no entry. Entries of versions 1
    /// and 2 of the log are outside the chain, so no anchor covers them.
    ///
    /// # Errors
    ///
    /// [`LogAnchorError::Confirmation`] with the first finding: a cut tail
    /// ([`AnchorFinding::Cut`]), a rewritten entry ([`AnchorFinding::Rewritten`]), or a held
    /// anchor of another key, log, or repository. The errors of the configuration, the pinned
    /// key, the log, and its ID otherwise.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn verify_anchors(
        &self,
        remote: &RemoteName,
        held: &[HeldAnchor],
    ) -> Result<AnchorCheck, LogAnchorError> {
        self.require_oplog("verify the log against the anchors of a hub")?;
        let config = self
            .remote(remote)?
            .ok_or_else(|| LogAnchorError::NoRemote(remote.clone()))?;
        let key = config.pinned_key().map_err(LogAnchorError::HubKey)?;
        let log = self.log_id()?;
        let entries = self.oplog().verify()?;
        check_anchors(&entries, log, config.repository, key, held)
            .map_err(LogAnchorError::Confirmation)
    }

    /// The register as it counts for the entries of this log (ADR-026): a bound that the
    /// register names for another log is the number of the first `fetch` entry of `entries`
    /// that brought a commit whose register holds that bound, so the entries before it count
    /// and the ones from it on do not. If no `fetch` entry brought it, no entry of this log
    /// stands before the bound. A bound of this log and a bound without a log (version 1 of the
    /// register) keep their number. `entries` are the entries of the log, which the caller
    /// verified. The ID of the log is read, not made: a log without one has every bound with a
    /// log as one of another log.
    ///
    /// # Errors
    ///
    /// [`LogAnchorError::LogIdDamaged`] and [`LogAnchorError::Io`] for the file of the ID, and
    /// [`LogAnchorError::Register`] if the register of a fetched commit cannot be read.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn register_for_log(
        &self,
        register: &Register,
        entries: &[Entry],
    ) -> Result<Register, LogAnchorError> {
        self.require_oplog("read the register for the log")?;
        let own = self.read_log_id()?;
        let foreign = register
            .entries()
            .iter()
            .flat_map(|entry| entry.keys())
            .any(|held| held.log.is_some() && held.log != own);
        if !foreign {
            return Ok(register.clone());
        }
        // The fetched commits in the order of the log, each with the number of its entry.
        let mut fetched: Vec<(u64, CommitId)> = Vec::new();
        for entry in entries {
            let Record::Change(change) = entry.record() else {
                continue;
            };
            if change.operation().as_str() != FETCH {
                continue;
            }
            for (_, moved) in change.remotes() {
                if let Some(reference) = moved.after() {
                    fetched.push((entry.number(), reference.commit));
                }
            }
        }
        let mut registers: HashMap<CommitId, Option<Register>> = HashMap::new();
        for (_, commit) in &fetched {
            if registers.contains_key(commit) {
                continue;
            }
            let read = match Register::at_commit(self.objects(), commit) {
                Ok(read) => read,
                // A fetched commit that the store no longer holds, as after a purge, brought
                // nothing that can be shown.
                Err(crate::register::ModeError::Missing) => None,
                Err(error) => return Err(LogAnchorError::Register(Box::new(error))),
            };
            registers.insert(*commit, read);
        }
        let brought = |identity: &Identity, held: &KeyEntry| -> u64 {
            fetched
                .iter()
                .find(|(_, commit)| {
                    registers
                        .get(commit)
                        .and_then(Option::as_ref)
                        .and_then(|fetched| fetched.entry(identity))
                        .is_some_and(|entry| {
                            entry.keys().iter().any(|other| {
                                other.key == held.key
                                    && other.log == held.log
                                    && same_bound(other.state, held.state)
                            })
                        })
                })
                .map_or(1, |(number, _)| *number)
        };
        Ok(register.for_log(own, brought))
    }
}

/// Whether two states are the same bound: both retired or both revoked, from the same number.
fn same_bound(fetched: KeyState, wanted: KeyState) -> bool {
    match (fetched, wanted) {
        (KeyState::Retired { from: a }, KeyState::Retired { from: b })
        | (KeyState::Revoked { from: a }, KeyState::Revoked { from: b }) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
