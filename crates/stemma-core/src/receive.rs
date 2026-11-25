//! The receiving side of an object stream (ADR-023): the client of a fetch and the hub of an
//! upload.
//!
//! A receiver reads the stream with [`ObjectStreamReader`], computes the ID of every object
//! itself, and stores it through the write path of the object store (ADR-007). It trusts
//! nothing that the sender says about an object: a damaged object is another object, stored under
//! another ID, and the [connectivity check](Repository::check_connectivity) reports the expected
//! one as missing.
//!
//! - **Bounds.** [`Bounds`] names the longest body of an object and the longest stream that the
//!   receiver accepts. A longer object is refused before its body is read
//!   ([`StreamError::TooLarge`]), a longer stream with [`ReceiveError::RequestTooLarge`].
//! - **Its own directory in `tmp/`.** A receiving operation holds no lock of the references. The
//!   writer that takes that lock removes every file in `tmp/` and leaves directories (ADR-011),
//!   so the temporary files of a receiving operation lie in a directory of their own,
//!   `tmp/receive-<pid>-<n>-<random>/`, which it removes at its end. The directory holds the
//!   file `guard`, on which the operation holds a lock of the system while it runs. A directory
//!   whose guard is free was left by a process that ended, and the next receiving operation
//!   removes it, as does [`Repository::remove_receive_leftovers`].
//! - **Connectivity.** The check walks from the new tips down to commits that are reachable from
//!   the own references (branches, tags, and remote references) and compares the trees of a
//!   commit with those of its parents. Every object that it meets must be present, of the right
//!   kind, and readable by the strict readers. It also meets objects that were in the store
//!   before, which may be left from an interrupted transfer, and it does not ask where an object
//!   came from. An object that it does not reach stays in the store unnamed and is no error.
//! - **The journal of a running fetch.** A [`Fetch`] records in the file `fetch-pending` of the
//!   metadata directory which commits arrived completely. A fetch that is interrupted leaves the
//!   file, and the next one checks the recorded commits again and sends those that hold as
//!   further `have` lines ([`Fetch::have`]).
//!
//! The form of the journal is strict text:
//!
//! ```text
//! stemma-fetch 1
//! commit <id>                     zero or more, in the order in which they arrived
//! ```
//!
//! A last line without a line feed is left by a process that ended while it wrote. It is cut
//! off when the journal is opened, if it is the beginning of a line that a fetch writes; any
//! other end is refused as damage, like every other content that no fetch writes. A commit may
//! stand more than once: one that was recorded, lost its objects, and arrived again.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::conflict::AnyCommit;
use crate::fault;
use crate::history::{read_commit, HistoryError};
use crate::object::{CommitId, EntryMode, Kind, ObjectId, Tree, TreeEntry, TreeId, PREFIX};
use crate::protocol::{ObjectStreamReader, StreamError};
use crate::refs::RefError;
use crate::repository::Repository;
use crate::store::{
    create_dirs_synced, flush_directory, is_scratch_name, unique_temp_name, Batch, FsObjectStore,
    ObjectStore, ReadError, StoreError,
};

/// The longest body of an object that a receiver accepts unless its caller names another bound:
/// 1 GiB. The store holds an object in memory while it writes and reads it (ADR-007), so the
/// bound is also what one object may cost the receiver in memory.
pub const MAX_OBJECT_LEN: u64 = 1 << 30;

/// The longest object stream that a receiver accepts unless its caller names another bound:
/// 16 GiB, counted in the bytes of the stream as the messages carry it, without the compression
/// of the transport. A clone is one stream, and a history has up to a few gigabytes (ADR-015).
pub const MAX_REQUEST_LEN: u64 = 1 << 34;

/// The prefix of the name of the directory of a receiving operation in `tmp/`; a name of
/// [`unique_temp_name`] follows.
const RECEIVE_DIR_PREFIX: &str = "receive-";

/// The file in the directory of a receiving operation on which the operation holds a lock of
/// the system while it runs.
const GUARD: &str = "guard";

/// The file of the journal of a running fetch in the metadata directory.
pub(crate) const FETCH_JOURNAL: &str = "fetch-pending";

/// The first line of the journal, with the version of its form.
const JOURNAL_HEADER: &str = "stemma-fetch 1";

/// The longest journal that is read. A journal names one commit in a line of 75 bytes, so the
/// bound holds more commits than a history at the target scale has (ADR-015); a larger file is
/// damage.
const MAX_JOURNAL_LEN: u64 = 1 << 30;

/// How often a receiving operation tries a new name for its directory.
const DIR_ATTEMPTS: usize = 16;

/// How often a fetch opens the journal again because a fetch that ended removed the file that it
/// had opened.
const JOURNAL_ATTEMPTS: usize = 16;

/// The count of objects of a stream since the last batch, with those that the store had already,
/// from which a receiver makes them durable at the next commit of the stream. The tests take a
/// small count, so that a short stream fills a batch.
const FLUSH_OBJECTS: u64 = if cfg!(test) { 4 } else { 1024 };

/// The bytes of the bodies of these objects from which a receiver makes them durable at the
/// next commit of the stream.
const FLUSH_BYTES: u64 = 64 << 20;

/// The upper bounds of a receiver (ADR-023). Both are counted in the bytes that the messages
/// carry: the compression of a transport is taken off before, so a small compressed body cannot
/// pass a long stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    max_object_len: u64,
    max_request_len: u64,
}

impl Bounds {
    /// Bounds with these values, in bytes: the longest body of one object, and the longest
    /// stream, counted from its first line to its end.
    pub const fn new(max_object_len: u64, max_request_len: u64) -> Self {
        Self {
            max_object_len,
            max_request_len,
        }
    }

    /// The longest body of an object in bytes.
    pub const fn max_object_len(&self) -> u64 {
        self.max_object_len
    }

    /// The longest stream in bytes.
    pub const fn max_request_len(&self) -> u64 {
        self.max_request_len
    }
}

impl Default for Bounds {
    /// [`MAX_OBJECT_LEN`] and [`MAX_REQUEST_LEN`].
    fn default() -> Self {
        Self::new(MAX_OBJECT_LEN, MAX_REQUEST_LEN)
    }
}

/// What a receiving operation stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Received {
    /// The number of objects that the stream carried. An object that the store had already, or
    /// that the stream carried twice, counts each time.
    pub objects: u64,
    /// The number of commits among them, of the kind `commit` or `commit2`.
    pub commits: u64,
    /// The bytes of the bodies of the objects.
    pub bytes: u64,
}

/// Why the connectivity check refuses a commit (ADR-023). The errors inside are boxed, because
/// the unboxed ones are too large for clippy's `result_large_err` lint.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConnectivityError {
    /// An object that the walk reached is not in the store. A damaged object of a stream shows
    /// here: the receiver stored it under the ID of its content, so the ID that a tree or a
    /// commit names is absent.
    Missing {
        /// The object that is absent.
        id: ObjectId,
        /// The kind that the object must have. [`Kind::Commit`] stands for a commit of either
        /// kind.
        expected: Kind,
        /// The commit or the tree that names it, or `None` if it is a tip that the caller named.
        needed_by: Option<ObjectId>,
    },
    /// An object that the walk reached has another kind than the one that names it claims.
    WrongKind {
        /// The object.
        id: ObjectId,
        /// The kind that the object must have. [`Kind::Commit`] stands for a commit of either
        /// kind.
        expected: Kind,
        /// The kind it has.
        found: Kind,
        /// The commit or the tree that names it, or `None` if it is a tip that the caller named.
        needed_by: Option<ObjectId>,
    },
    /// A tree or a commit that the walk reached is intact as an object, and the strict reader
    /// refuses its body (ADR-005), or returns a value with another ID than the one it was read
    /// by ([`ReadError::IdMismatch`], a defect of the reader). The source is never
    /// [`ReadError::Store`]: an error of the store is [`Store`](Self::Store), and another kind
    /// is [`WrongKind`](Self::WrongKind).
    Unreadable {
        /// The commit or the tree that names it, or `None` if it is a tip that the caller named.
        needed_by: Option<ObjectId>,
        /// The refusal of the reader, which names the object.
        source: Box<ReadError>,
    },
    /// The store could not answer: the storage failed, or the file of an object is damaged.
    Store(Box<StoreError>),
    /// The references of the repository could not be read.
    Refs(Box<RefError>),
    /// The history below the own references cannot be read. The check takes a commit that an own
    /// reference reaches as complete (ADR-015) and reads this history only to learn which
    /// commits these are.
    Own(Box<HistoryError>),
}

impl ConnectivityError {
    /// Whether the objects do not hold together: one is missing, has the wrong kind, or is
    /// refused by a strict reader. A hub answers that with the request to upload again
    /// (ADR-023). `false` for a failure of the storage or of the own repository, and for a
    /// reader that returned a value with another ID ([`ReadError::IdMismatch`]), which is a
    /// defect of the reader that no upload mends.
    pub fn is_incomplete(&self) -> bool {
        match self {
            Self::Missing { .. } | Self::WrongKind { .. } => true,
            // A reader that returns a value with another ID has a defect, and no upload
            // mends that.
            Self::Unreadable { source, .. } => !matches!(**source, ReadError::IdMismatch { .. }),
            Self::Store(_) | Self::Refs(_) | Self::Own(_) => false,
        }
    }

    /// Whether the file of an object that the walk reached is damaged.
    fn is_damaged_object(&self) -> bool {
        matches!(self, Self::Store(source) if matches!(**source, StoreError::Corrupt { .. }))
    }

    /// Returns the error of the store if the store could not answer, also inside the error of
    /// the references or of the own history. `None` for an object that is missing, has the
    /// wrong kind, or has a body that is not canonical.
    pub fn store_error(&self) -> Option<&StoreError> {
        match self {
            Self::Missing { .. } | Self::WrongKind { .. } => None,
            Self::Unreadable { source, .. } => source.store_error(),
            Self::Store(source) => Some(source),
            Self::Refs(source) => source.store_error(),
            Self::Own(source) => source.store_error(),
        }
    }

    /// Returns the error of the file system if that is what failed, in the store or while the
    /// references were read (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Refs(source) => source.io_error(),
            Self::Missing { .. }
            | Self::WrongKind { .. }
            | Self::Unreadable { .. }
            | Self::Store(_)
            | Self::Own(_) => self.store_error().and_then(StoreError::io_error),
        }
    }
}

/// The words for "needed by" at the end of a message.
struct NeededBy(Option<ObjectId>);

impl fmt::Display for NeededBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(id) => write!(f, ", which {id} names"),
            None => Ok(()),
        }
    }
}

impl fmt::Display for ConnectivityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing {
                id,
                expected,
                needed_by,
            } => write!(
                f,
                "the {} {id} is missing{}",
                expected.name(),
                NeededBy(*needed_by)
            ),
            Self::WrongKind {
                id,
                expected,
                found,
                needed_by,
            } => write!(
                f,
                "object {id} is a {}, not a {}{}",
                found.name(),
                expected.name(),
                NeededBy(*needed_by)
            ),
            Self::Unreadable { needed_by, source } => match needed_by {
                Some(id) => write!(f, "{source} ({id} names the object)"),
                None => write!(f, "{source}"),
            },
            Self::Store(source) => write!(f, "{source}"),
            Self::Refs(source) => write!(f, "{source}"),
            Self::Own(source) => write!(
                f,
                "cannot read the history below the references of this repository: {source}"
            ),
        }
    }
}

impl Error for ConnectivityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message includes that of the cause, so the source is the cause of the cause (see
        // the crate documentation).
        match self {
            Self::Missing { .. } | Self::WrongKind { .. } => None,
            Self::Unreadable { source, .. } => source.source(),
            Self::Store(source) => source.source(),
            Self::Refs(source) => source.source(),
            Self::Own(source) => source.source(),
        }
    }
}

/// Why the journal of a running fetch fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum FetchJournalError {
    /// The file of the journal could not be read or written.
    Io {
        /// What was being done, for example "write the journal of the fetch".
        operation: &'static str,
        /// The file or the directory.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// The journal is not one that a fetch writes. Nothing depends on the file but the
    /// resumption: removing it by hand costs the objects that arrived a second transfer.
    Malformed {
        /// The file of the journal.
        path: PathBuf,
        /// The number of the line, from 1.
        line: usize,
    },
    /// Another fetch runs in this repository and holds the journal.
    Running {
        /// The file of the journal.
        path: PathBuf,
    },
}

impl fmt::Display for FetchJournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
            Self::Malformed { path, line } => write!(
                f,
                "the journal of the fetch `{}` is damaged at line {line}; a fetch writes \
                 `{JOURNAL_HEADER}` and lines `commit <id>`. Remove the file to fetch without \
                 the resumption",
                path.display()
            ),
            Self::Running { path } => write!(
                f,
                "another fetch runs in this repository and holds its journal `{}`",
                path.display()
            ),
        }
    }
}

impl FetchJournalError {
    /// Returns the error of the file system if this is [`FetchJournalError::Io`] (see the crate
    /// documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Malformed { .. } | Self::Running { .. } => None,
        }
    }
}

impl Error for FetchJournalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes that of the I/O error, so the source is its own cause.
            Self::Io { source, .. } => source.source(),
            Self::Malformed { .. } | Self::Running { .. } => None,
        }
    }
}

/// Why a receiving operation fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReceiveError {
    /// The reader of the stream refused it, or its carrier failed. A body above the bound of an
    /// object is [`StreamError::TooLarge`], refused before any of it is read.
    Stream(StreamError),
    /// The stream is longer than the bound of a request.
    RequestTooLarge {
        /// The bound of the receiver, in bytes.
        limit: u64,
    },
    /// An object could not be stored, or the directory of the receiving operation in `tmp/`
    /// could not be made. Boxed, as in [`ConnectivityError`].
    Store(Box<StoreError>),
    /// The connectivity check of a fetch failed: a commit of the stream is not connected (an
    /// object below it is missing, has the wrong kind, or is refused by a strict reader;
    /// [`ConnectivityError::is_incomplete`]), or the check itself could not be made, because
    /// the storage failed, the history below the own references cannot be read, or a reader
    /// returned a value with another ID. Only a fetch
    /// checks while it receives.
    Connectivity(ConnectivityError),
    /// The journal of the fetch failed.
    Journal(FetchJournalError),
    /// The repository refuses the operation, as a bare repository refuses a fetch, or its
    /// references cannot be read.
    Refs(Box<RefError>),
}

impl ReceiveError {
    /// Whether a bound of the receiver refused the stream: an object or the stream is too
    /// large. The transport of a hub answers that with its own status (ADR-023).
    pub fn exceeds_bound(&self) -> bool {
        matches!(
            self,
            Self::RequestTooLarge { .. } | Self::Stream(StreamError::TooLarge { .. })
        )
    }

    /// Returns the error of the store if the store could not answer, also inside the error of
    /// the connectivity check or of the references.
    pub fn store_error(&self) -> Option<&StoreError> {
        match self {
            Self::Store(source) => Some(source),
            Self::Connectivity(source) => source.store_error(),
            Self::Refs(source) => source.store_error(),
            Self::Stream(_) | Self::RequestTooLarge { .. } | Self::Journal(_) => None,
        }
    }

    /// Returns the error of the file system of the repository if that is what failed: in the
    /// store, the references, or the journal (see the crate documentation). An error of the
    /// carrier of the stream is not one of them, so that a connection that broke is not taken
    /// for a full disk; [`StreamError::io_error`] returns it.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Store(source) => source.io_error(),
            Self::Connectivity(source) => source.io_error(),
            Self::Journal(source) => source.io_error(),
            Self::Refs(source) => source.io_error(),
            Self::Stream(_) | Self::RequestTooLarge { .. } => None,
        }
    }
}

impl fmt::Display for ReceiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stream(source) => write!(f, "{source}"),
            Self::RequestTooLarge { limit } => write!(
                f,
                "the object stream is longer than the limit of {limit} bytes"
            ),
            Self::Store(source) => write!(f, "{source}"),
            Self::Connectivity(source) => write!(f, "{source}"),
            Self::Journal(source) => write!(f, "{source}"),
            Self::Refs(source) => write!(f, "{source}"),
        }
    }
}

impl Error for ReceiveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // Transparent: the message is that of the inner error, so its cause is the cause here.
        match self {
            Self::Stream(source) => source.source(),
            Self::RequestTooLarge { .. } => None,
            Self::Store(source) => source.source(),
            Self::Connectivity(source) => source.source(),
            Self::Journal(source) => source.source(),
            Self::Refs(source) => source.source(),
        }
    }
}

impl From<StoreError> for ReceiveError {
    fn from(source: StoreError) -> Self {
        Self::Store(Box::new(source))
    }
}

impl From<RefError> for ReceiveError {
    fn from(source: RefError) -> Self {
        Self::Refs(Box::new(source))
    }
}

impl From<RefError> for ConnectivityError {
    fn from(source: RefError) -> Self {
        Self::Refs(Box::new(source))
    }
}

impl From<StoreError> for ConnectivityError {
    fn from(source: StoreError) -> Self {
        Self::Store(Box::new(source))
    }
}

impl From<ConnectivityError> for ReceiveError {
    fn from(source: ConnectivityError) -> Self {
        Self::Connectivity(source)
    }
}

impl From<FetchJournalError> for ReceiveError {
    fn from(source: FetchJournalError) -> Self {
        Self::Journal(source)
    }
}

/// Whether `name` is exactly a name that a receiving operation gives its directory in `tmp/`.
fn is_receive_dir_name(name: &str) -> bool {
    is_scratch_name(name, RECEIVE_DIR_PREFIX)
}

/// The directory of one receiving operation in `tmp/`, with the lock of the system on its file
/// `guard`. The directory is removed on drop.
struct ReceiveDir {
    path: PathBuf,
    /// The open guard file, which holds the lock. `None` only while the value is dropped.
    guard: Option<File>,
}

impl ReceiveDir {
    /// Makes a new directory `tmp/receive-<pid>-<n>-<random>/` below the metadata directory
    /// `root`, with the file `guard`, and takes the lock on it.
    ///
    /// A cleanup that looks at the directory before the lock is held takes it for one that was
    /// left, and removes it. The lock therefore counts only if the guard is still in place once
    /// it is held; else another name is tried. If the lock cannot be taken for another reason
    /// than a holder, as on a file system without locks of files, the directory is used without
    /// the lock. A cleanup that cannot lock the guard either leaves the directory; one that can,
    /// after a failure that passed, removes it, and the operation then fails when it moves its
    /// objects into place, without losing a stored object.
    fn create(root: &Path) -> Result<Self, StoreError> {
        let tmp = root.join("tmp");
        create_dirs_synced(root, &tmp)?;
        let io = |operation: &'static str, path: &Path, source: io::Error| StoreError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        };
        let mut last = tmp.clone();
        for _ in 0..DIR_ATTEMPTS {
            let path = tmp.join(format!("{RECEIVE_DIR_PREFIX}{}", unique_temp_name()));
            match fault::create_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last = path;
                    continue;
                }
                Err(error) => return Err(io("create the directory", &path, error)),
            }
            let guard_path = path.join(GUARD);
            let guard = match fault::create_new(&guard_path) {
                Ok(guard) => guard,
                // A cleanup removed the directory while it was empty.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    last = path;
                    continue;
                }
                Err(error) => {
                    let _ = fault::remove_dir(&path);
                    return Err(io("create the guard file", &guard_path, error));
                }
            };
            match guard.try_lock() {
                Ok(()) if fs::symlink_metadata(&guard_path).is_ok() => {
                    return Ok(Self {
                        path,
                        guard: Some(guard),
                    });
                }
                // A cleanup held the guard, or removed it before the lock was taken here: it
                // removes the directory.
                Ok(()) | Err(TryLockError::WouldBlock) => last = path,
                Err(TryLockError::Error(_)) => {
                    return Ok(Self {
                        path,
                        guard: Some(guard),
                    });
                }
            }
        }
        Err(io(
            "create the directory",
            &last,
            io::Error::new(io::ErrorKind::AlreadyExists, "every name was taken"),
        ))
    }

    /// Ends as a process that is killed does: the lock is released, and the directory stays
    /// with what it holds.
    #[cfg(test)]
    fn crash(mut self) -> PathBuf {
        self.guard = None;
        let path = std::mem::take(&mut self.path);
        std::mem::forget(self);
        path
    }
}

impl Drop for ReceiveDir {
    fn drop(&mut self) {
        // Windows removes no file that is open, so the guard is closed first. Best effort: a
        // directory that stays has a free guard, and the next receiving operation removes it.
        self.guard = None;
        let _ = fault::remove_dir_all(&self.path);
    }
}

/// Removes the directories that receiving operations left in `<root>/tmp/`: every directory
/// whose name is exactly one that a receiving operation makes and whose guard is free. A
/// directory whose guard is held belongs to an operation that runs and stays, and so does one
/// whose guard cannot be opened or locked, because nothing says that its operation has ended.
/// A directory without a guard is removed if it is empty: its operation ended before it made
/// the guard, or makes it next and then tries another name.
///
/// It is best effort, like the cleanup of `tmp/` under the lock (ADR-011): a directory that
/// cannot be removed stays, and the next call tries again. A link is never followed.
fn remove_stale_receive_dirs(root: &Path) {
    let Ok(entries) = fault::read_dir(&root.join("tmp")) else {
        return;
    };
    for entry in entries.flatten() {
        let named = entry.file_name().to_str().is_some_and(is_receive_dir_name);
        if !named || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        match crate::open_without_following(&path.join(GUARD)) {
            Ok(guard) => {
                if guard.try_lock().is_ok() && fault::remove_dir_all(&path).is_err() {
                    // Windows removes no file that is open.
                    drop(guard);
                    let _ = fault::remove_dir_all(&path);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let _ = fault::remove_dir(&path);
            }
            Err(_) => {}
        }
    }
}

/// Whether the kind is a commit of either kind.
fn is_commit(kind: Kind) -> bool {
    matches!(kind, Kind::Commit | Kind::Commit2)
}

/// Reads an object stream and stores its objects, in batches that are flushed together.
///
/// The objects are written to temporary files in a directory of this call below `tmp/`. At a
/// commit of the stream, once [`FLUSH_OBJECTS`] objects or [`FLUSH_BYTES`] bytes have gathered,
/// and at the end of the stream, they are made durable and moved into place, and `durable` is
/// called with the commits among them, in the order of the stream. The same happens when the
/// stream fails: every object that was read whole is stored, and `durable` hears of its commits,
/// before the error of the stream is returned. An error of `durable` ends the call and goes
/// before the error of the stream.
fn receive_stream<R: BufRead>(
    store: &FsObjectStore,
    root: &Path,
    stream: R,
    bounds: Bounds,
    mut durable: impl FnMut(&[CommitId]) -> Result<(), ReceiveError>,
) -> Result<Received, ReceiveError> {
    remove_stale_receive_dirs(root);
    let dir = ReceiveDir::create(root)?;
    // One byte more than the bound: a stream that fills the bound exactly leaves it unread.
    let mut limited = stream.take(bounds.max_request_len.saturating_add(1));
    let mut received = Received::default();
    let read = read_frames(
        store,
        &dir,
        &mut limited,
        bounds.max_object_len,
        &mut received,
        &mut durable,
    )?;
    // The reader ends at the bound as at the end of its carrier: between two frames, inside
    // one, or inside the first line. Nothing of the bound is left only if the stream was longer.
    let too_large = limited.limit() == 0;
    match read {
        Ok(()) | Err(StreamError::CutOff { .. } | StreamError::FirstLine) if too_large => {
            Err(ReceiveError::RequestTooLarge {
                limit: bounds.max_request_len,
            })
        }
        Ok(()) => Ok(received),
        Err(error) => Err(ReceiveError::Stream(error)),
    }
}

/// The loop of [`receive_stream`]: reads the frames of `stream` and stores their objects. The
/// inner result is how the stream ended; the objects that were read whole are durable then, and
/// `durable` has heard of their commits. The outer error is one of the store or of `durable`.
fn read_frames(
    store: &FsObjectStore,
    dir: &ReceiveDir,
    stream: &mut impl BufRead,
    max_object_len: u64,
    received: &mut Received,
    durable: &mut impl FnMut(&[CommitId]) -> Result<(), ReceiveError>,
) -> Result<Result<(), StreamError>, ReceiveError> {
    let mut reader = match ObjectStreamReader::new(stream, max_object_len) {
        Ok(reader) => reader,
        Err(error) => return Ok(Err(error)),
    };
    let mut batch = store.batch_in(dir.path.clone());
    let mut commits = Vec::new();
    let mut pending_objects = 0;
    let mut pending_bytes = 0;
    let read = loop {
        let object = match reader.next_object() {
            Ok(Some(object)) => object,
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        };
        let id = batch.put_raw(object.kind, &object.body)?;
        let length = object.body.len() as u64;
        received.objects += 1;
        received.bytes += length;
        pending_objects += 1;
        pending_bytes += length;
        if is_commit(object.kind) {
            received.commits += 1;
            commits.push(CommitId::new(id));
            if pending_objects >= FLUSH_OBJECTS || pending_bytes >= FLUSH_BYTES {
                let full = std::mem::replace(&mut batch, store.batch_in(dir.path.clone()));
                finish(full, &mut commits, durable)?;
                pending_objects = 0;
                pending_bytes = 0;
            }
        }
    };
    finish(batch, &mut commits, durable)?;
    Ok(read)
}

/// Makes the objects of a batch durable and reports its commits, which are taken out of
/// `commits`.
fn finish(
    batch: Batch<'_>,
    commits: &mut Vec<CommitId>,
    durable: &mut impl FnMut(&[CommitId]) -> Result<(), ReceiveError>,
) -> Result<(), ReceiveError> {
    // The directory of an object that the store had already is flushed too: the sender is told
    // that the object is durable, whoever placed it.
    batch.finish()?;
    durable(&std::mem::take(commits))
}

/// The connectivity check (ADR-023) with what it has learned so far, so that a receiver that
/// checks commit by commit checks no commit, tree, or blob twice.
struct Connectivity<'a> {
    store: &'a FsObjectStore,
    /// Commits that an own reference reaches, as far as the walk below the references has come.
    own: HashSet<CommitId>,
    /// The commits of `own` whose parents are not read yet.
    frontier: Vec<(CommitId, Option<CommitId>)>,
    /// Commits that this check found complete.
    verified: HashSet<CommitId>,
    /// The trees and blobs of the commits in `verified` that this check has looked at.
    checked: HashSet<ObjectId>,
}

/// A step of the walk over commits: a commit to look at, with the commit that names it, or a
/// commit whose parents are all complete.
enum Step {
    Enter(CommitId, Option<CommitId>),
    Exit(CommitId, Box<AnyCommit>),
}

impl<'a> Connectivity<'a> {
    /// A check over the store, with `tips`, the commits that the own references name
    /// ([`own_tips`]), as the commits that are taken as complete with everything below them
    /// (ADR-015).
    fn new(store: &'a FsObjectStore, tips: &BTreeSet<CommitId>) -> Self {
        Self {
            store,
            own: tips.iter().copied().collect(),
            frontier: tips.iter().map(|&tip| (tip, None)).collect(),
            verified: HashSet::new(),
            checked: HashSet::new(),
        }
    }

    /// Whether an own reference reaches the commit. The history below the references is read
    /// only until the commit is found: a commit that a reference names costs no read. The
    /// answer "no", which every new commit gets, needs all of that history, so the first new
    /// commit of a check costs one read of every commit below the own references, and the
    /// later ones none.
    fn is_own(&mut self, id: CommitId) -> Result<bool, ConnectivityError> {
        while !self.own.contains(&id) {
            let Some((next, child)) = self.frontier.pop() else {
                return Ok(false);
            };
            let commit = read_commit(self.store, next, child)
                .map_err(|error| ConnectivityError::Own(Box::new(error)))?;
            for &parent in commit.parents() {
                if self.own.insert(parent) {
                    self.frontier.push((parent, Some(next)));
                }
            }
        }
        Ok(true)
    }

    /// Whether the commit is complete: this check found it so, or an own reference reaches it.
    fn is_complete(&mut self, id: CommitId) -> Result<bool, ConnectivityError> {
        Ok(self.verified.contains(&id) || self.is_own(id)?)
    }

    /// Checks that everything below `tip` is in the store: walks from it down to commits that
    /// are complete, and checks each commit on the way after its parents.
    fn ensure_complete(&mut self, tip: CommitId) -> Result<(), ConnectivityError> {
        let mut steps = vec![Step::Enter(tip, None)];
        while let Some(step) = steps.pop() {
            match step {
                Step::Enter(id, child) => {
                    if self.is_complete(id)? {
                        continue;
                    }
                    let needed_by = child.map(CommitId::object_id);
                    let commit = match self.store.get_any_commit(&id) {
                        Ok(Some(commit)) => commit,
                        Ok(None) => {
                            return Err(ConnectivityError::Missing {
                                id: id.object_id(),
                                expected: Kind::Commit,
                                needed_by,
                            })
                        }
                        Err(error) => return Err(unreadable(error, needed_by)),
                    };
                    let parents = commit.parents().to_vec();
                    steps.push(Step::Exit(id, Box::new(commit)));
                    for parent in parents {
                        if !self.is_complete(parent)? {
                            steps.push(Step::Enter(parent, Some(id)));
                        }
                    }
                }
                // Between the two steps of a commit only its ancestors are looked at, so a
                // commit that two children name is complete when it is entered again.
                Step::Exit(id, commit) => {
                    self.check_trees(id, &commit)?;
                    self.verified.insert(id);
                }
            }
        }
        Ok(())
    }

    /// Checks the trees of a commit whose parents are complete: every tree and every blob below
    /// them that the trees of the parents do not hold at the same path.
    ///
    /// The trees of the parents only save work. A parent or a tree of a parent that cannot be
    /// read saves none, and the walk then checks what it would have left out, so a failure of
    /// such a read is not reported here.
    fn check_trees(&mut self, id: CommitId, commit: &AnyCommit) -> Result<(), ConnectivityError> {
        let mut bases = Vec::new();
        for parent in commit.parents() {
            if let Ok(Some(parent)) = self.store.get_any_commit(parent) {
                bases.extend(parent.tree_sum().terms().iter().copied());
            }
        }
        // What this commit adds counts as checked only once all of it is checked.
        let mut seen: HashSet<ObjectId> = HashSet::new();
        // A tree, the trees of the parents at its path, and the object that names it. The walk
        // keeps its own stack, so the depth of a tree that a sender chose costs no stack of the
        // program.
        let mut trees: Vec<(TreeId, Vec<TreeId>, ObjectId)> = commit
            .tree_sum()
            .terms()
            .iter()
            .map(|&tree| (tree, bases.clone(), id.object_id()))
            .collect();
        while let Some((tree_id, bases, needed_by)) = trees.pop() {
            let object = tree_id.object_id();
            if bases.contains(&tree_id) || self.checked.contains(&object) || !seen.insert(object) {
                continue;
            }
            let tree = match self.store.get_tree(&tree_id) {
                Ok(Some(tree)) => tree,
                Ok(None) => {
                    return Err(ConnectivityError::Missing {
                        id: object,
                        expected: Kind::Tree,
                        needed_by: Some(needed_by),
                    })
                }
                Err(error) => return Err(unreadable(error, Some(needed_by))),
            };
            let bases: Vec<Tree> = bases
                .iter()
                .filter_map(|base| self.store.get_tree(base).ok().flatten())
                .collect();
            let bases: Vec<HashMap<&str, &TreeEntry>> = bases
                .iter()
                .map(|base| {
                    base.entries()
                        .iter()
                        .map(|entry| (entry.name(), entry))
                        .collect()
                })
                .collect();
            for entry in tree.entries() {
                let is_directory = entry.mode() == EntryMode::Directory;
                // The entries of the parents with this name and of the same sort. One with the
                // same ID is complete, because its commit is.
                let below: Vec<ObjectId> = bases
                    .iter()
                    .filter_map(|base| base.get(entry.name()))
                    .filter(|found| (found.mode() == EntryMode::Directory) == is_directory)
                    .map(|found| found.id())
                    .collect();
                if below.contains(&entry.id()) {
                    continue;
                }
                if is_directory {
                    let below = below.into_iter().map(TreeId::new).collect();
                    trees.push((TreeId::new(entry.id()), below, object));
                } else if !self.checked.contains(&entry.id()) && !seen.contains(&entry.id()) {
                    self.check_blob(entry.id(), object)?;
                    seen.insert(entry.id());
                }
            }
        }
        self.checked.extend(seen);
        Ok(())
    }

    /// Checks that the store has the object as a blob. Only the header of the file is read: a
    /// blob has no reader that could refuse its body, and a body that does not match its ID is
    /// found by a full read alone, as in the scan of a purge (ADR-014). An object of a stream
    /// was hashed when it arrived.
    fn check_blob(&self, id: ObjectId, needed_by: ObjectId) -> Result<(), ConnectivityError> {
        match self.store.kind_of(&id) {
            Ok(Some(Kind::Blob)) => Ok(()),
            Ok(Some(found)) => Err(ConnectivityError::WrongKind {
                id,
                expected: Kind::Blob,
                found,
                needed_by: Some(needed_by),
            }),
            Ok(None) => Err(ConnectivityError::Missing {
                id,
                expected: Kind::Blob,
                needed_by: Some(needed_by),
            }),
            Err(error) => Err(error.into()),
        }
    }
}

/// The error for a tree or a commit that the store did not return as a value.
fn unreadable(error: ReadError, needed_by: Option<ObjectId>) -> ConnectivityError {
    match error {
        ReadError::Store(source) => match *source {
            StoreError::KindMismatch {
                id,
                expected,
                found,
            } => ConnectivityError::WrongKind {
                id,
                expected,
                found,
                needed_by,
            },
            source => source.into(),
        },
        source => ConnectivityError::Unreadable {
            needed_by,
            source: Box::new(source),
        },
    }
}

/// The commits that the references of the repository name: its branches, its tags, and its
/// remote references.
fn own_tips(repository: &Repository) -> Result<BTreeSet<CommitId>, RefError> {
    let refs = repository.refs();
    let mut tips = BTreeSet::new();
    tips.extend(refs.branches()?.into_iter().map(|(_, commit)| commit));
    tips.extend(refs.tags()?.into_iter().map(|(_, commit)| commit));
    tips.extend(
        refs.remote_refs()?
            .into_iter()
            .map(|(_, reference)| reference.commit),
    );
    Ok(tips)
}

/// Whether `tail`, the bytes of a journal after its last line feed, is the beginning of a line
/// that a fetch writes: of the first line if `first`, else of a line `commit <id>`. Empty bytes
/// are. A process that ended while it wrote leaves such a tail and no other.
fn is_cut_line(tail: &[u8], first: bool) -> bool {
    if first {
        return format!("{JOURNAL_HEADER}\n").as_bytes().starts_with(tail);
    }
    let keyword = b"commit ";
    let Some(id) = tail.strip_prefix(keyword) else {
        return keyword.starts_with(tail);
    };
    let prefix = PREFIX.as_bytes();
    id.len() <= prefix.len() + 64
        && id.iter().enumerate().all(|(index, &byte)| {
            prefix
                .get(index)
                .map_or(matches!(byte, b'0'..=b'9' | b'a'..=b'f'), |&expected| {
                    byte == expected
                })
        })
}

/// Whether `path` still leads to the open `file`: on Unix by the device and the number of the
/// file, elsewhere by a name that exists. `false` if the name is gone or cannot be inspected.
fn names_file(path: &Path, file: &File) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        matches!(
            (fs::symlink_metadata(path), file.metadata()),
            (Ok(named), Ok(open)) if named.dev() == open.dev() && named.ino() == open.ino()
        )
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        fs::symlink_metadata(path).is_ok()
    }
}

/// The open journal of a running fetch, with the lock of the system on it.
struct Journal {
    path: PathBuf,
    file: File,
    /// The length that the file had before a write that failed, if what the write left could
    /// not be cut off again. The next write cuts there first.
    torn: Option<u64>,
}

impl Journal {
    fn io(&self, operation: &'static str) -> impl FnOnce(io::Error) -> FetchJournalError + '_ {
        move |source| FetchJournalError::Io {
            operation,
            path: self.path.clone(),
            source,
        }
    }

    /// Opens the journal in the metadata directory `root`, or makes it, takes the lock on it,
    /// and returns the commits that it records, each once, in the order of the file. A last line
    /// without a line feed is removed from the file if it is the beginning of a line that a fetch
    /// writes ([`is_cut_line`]), and refused as damage if not.
    ///
    /// A fetch that ends removes the name of the journal while it holds the lock. A lock that
    /// is taken on a file that was opened just before counts therefore only if the name still
    /// leads to that file; else the journal is opened again.
    ///
    /// If the lock cannot be taken for another reason than a fetch that holds it, as on a file
    /// system without locks of files, the journal is used without the lock. Two fetches at once
    /// may then lose lines of each other or remove the journal of the other. The reader refuses
    /// a journal whose lines were torn; a line that is lost costs a second transfer of its
    /// commit and nothing else, because a recorded commit is checked again before it counts.
    fn open(root: &Path) -> Result<(Self, Vec<CommitId>), FetchJournalError> {
        let path = root.join(FETCH_JOURNAL);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        let mut attempts = 0;
        let file = loop {
            let file =
                fault::open_with(&options, &path).map_err(|source| FetchJournalError::Io {
                    operation: "open the journal of the fetch",
                    path: path.clone(),
                    source,
                })?;
            if matches!(file.try_lock(), Err(TryLockError::WouldBlock)) {
                return Err(FetchJournalError::Running { path });
            }
            attempts += 1;
            // After the last attempt the file is used as it is: the fetches that end one after
            // the other so fast lose a resumption at most.
            if names_file(&path, &file) || attempts == JOURNAL_ATTEMPTS {
                break file;
            }
        };
        let mut journal = Self {
            path,
            file,
            torn: None,
        };
        let mut bytes = Vec::new();
        (&journal.file)
            .take(MAX_JOURNAL_LEN + 1)
            .read_to_end(&mut bytes)
            .map_err(journal.io("read the journal of the fetch"))?;
        if bytes.len() as u64 > MAX_JOURNAL_LEN {
            return Err(FetchJournalError::Malformed {
                path: journal.path,
                line: 1,
            });
        }
        // What ends with a line feed was written whole; the rest is a line that was cut off, if
        // it is the beginning of a line that a fetch writes.
        let whole = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |end| end + 1);
        let commits = journal.parse(&bytes[..whole])?;
        if !is_cut_line(&bytes[whole..], whole == 0) {
            let line = bytes[..whole].iter().filter(|&&byte| byte == b'\n').count() + 1;
            return Err(FetchJournalError::Malformed {
                path: journal.path,
                line,
            });
        }
        if whole < bytes.len() {
            journal
                .file
                .set_len(whole as u64)
                .map_err(journal.io("cut the last line of the journal of the fetch"))?;
        }
        if whole == 0 {
            journal.append(&format!("{JOURNAL_HEADER}\n"))?;
            // The entry of a new file is flushed. A file that was there and empty, or cut in its
            // first line, takes the same way, where the flush does no harm.
            flush_directory(root).map_err(|source| FetchJournalError::Io {
                operation: "flush the directory",
                path: root.to_path_buf(),
                source,
            })?;
        }
        Ok((journal, commits))
    }

    /// Reads the whole lines of a journal. No line at all is a journal that is new or whose
    /// first line was cut off.
    fn parse(&self, bytes: &[u8]) -> Result<Vec<CommitId>, FetchJournalError> {
        let malformed = |line| FetchJournalError::Malformed {
            path: self.path.clone(),
            line,
        };
        let mut lines = bytes.split_inclusive(|&byte| byte == b'\n').enumerate();
        match lines.next() {
            None => return Ok(Vec::new()),
            Some((_, first)) if first == format!("{JOURNAL_HEADER}\n").as_bytes() => {}
            Some(_) => return Err(malformed(1)),
        }
        let mut seen = HashSet::new();
        let mut commits = Vec::new();
        for (index, line) in lines {
            let commit = std::str::from_utf8(line)
                .ok()
                .and_then(|line| line.strip_suffix('\n'))
                .and_then(|line| line.strip_prefix("commit "))
                .and_then(|id| id.parse::<ObjectId>().ok())
                // The ID of an object is canonical only in one form, which `Display` writes.
                .filter(|id| line == format!("commit {id}\n").as_bytes())
                .map(CommitId::new)
                .ok_or_else(|| malformed(index + 1))?;
            if seen.insert(commit) {
                commits.push(commit);
            }
        }
        Ok(commits)
    }

    /// Writes `text` at the end of the journal and flushes the file. If the write or the flush
    /// fails, what
    /// it wrote is cut off again, so that a later write does not end a torn line with its own
    /// line feed. If that fails too, the next write cuts first, and fails if it cannot.
    fn append(&mut self, text: &str) -> Result<(), FetchJournalError> {
        let operation = "write the journal of the fetch";
        if let Some(end) = self.torn {
            self.file
                .set_len(end)
                .map_err(self.io("cut a torn line of the journal of the fetch"))?;
            self.torn = None;
        }
        let end = self
            .file
            .seek(SeekFrom::End(0))
            .map_err(self.io(operation))?;
        let written = self
            .file
            .write_all(text.as_bytes())
            .and_then(|()| self.file.sync_all());
        if written.is_err() && self.file.set_len(end).is_err() {
            self.torn = Some(end);
        }
        written.map_err(self.io(operation))
    }

    /// Records commits that arrived completely.
    fn record(&mut self, commits: &[CommitId]) -> Result<(), FetchJournalError> {
        if commits.is_empty() {
            return Ok(());
        }
        let lines: String = commits
            .iter()
            .map(|commit| format!("commit {commit}\n"))
            .collect();
        self.append(&lines)
    }

    /// Removes the journal and flushes its directory.
    fn remove(self) -> Result<(), FetchJournalError> {
        let Self { path, file, .. } = self;
        let io = |operation: &'static str, at: &Path| {
            let at = at.to_path_buf();
            move |source| FetchJournalError::Io {
                operation,
                path: at,
                source,
            }
        };
        // The name goes while the lock is held: a fetch that opened the file before finds the
        // lock held or the name gone (see `open`), and one that comes after makes a new file.
        // Windows may refuse to remove a file that is open; only there the removal is tried
        // again once the file is closed, without the lock.
        let mut removed = fault::remove_file(&path);
        drop(file);
        if cfg!(windows)
            && matches!(&removed, Err(error) if error.kind() != io::ErrorKind::NotFound)
        {
            removed = fault::remove_file(&path);
        }
        match removed {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io("remove the journal of the fetch", &path)(error)),
        }
        let root = path.parent().unwrap_or(&path);
        flush_directory(root).map_err(io("flush the directory", root))
    }
}

/// A running fetch on its receiving side (ADR-023): the commits to send as `have`, the object
/// streams of the answer, the connectivity check, and the journal that lets an interrupted
/// fetch resume. [`Repository::begin_fetch`] starts one.
///
/// The steps of a fetch, of which this type owns the middle:
///
/// 1. The caller reads the list of references of the remote and sends a request with the commits
///    it wants and [`have`](Self::have).
/// 2. [`receive`](Self::receive) stores the answer. It checks the connectivity commit by commit
///    and records each complete commit in the journal.
/// 3. [`check`](Self::check) confirms that every wanted commit is complete.
/// 4. The caller moves its remote references, and only then, and calls
///    [`finish`](Self::finish), which removes the journal.
///
/// A fetch that fails at step 2 or 3 is dropped. Its journal stays, and the next fetch sends the
/// recorded commits that still hold as further `have` lines ([`have`](Self::have)), so the
/// remote sends only what is still missing.
/// The value holds the lock of the system on the journal, where the file system has one: a
/// second fetch in the same repository is refused while it lives
/// ([`FetchJournalError::Running`]), and on Windows no other handle reads the file meanwhile.
#[must_use = "a fetch that is dropped keeps its journal for the next one; `finish` removes it"]
pub struct Fetch<'a> {
    repository: &'a Repository,
    connectivity: Connectivity<'a>,
    journal: Journal,
    have: BTreeSet<CommitId>,
    resumed: Vec<CommitId>,
}

impl fmt::Debug for Fetch<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fetch")
            .field("journal", &self.journal.path)
            .field("have", &self.have)
            .field("resumed", &self.resumed)
            .finish_non_exhaustive()
    }
}

impl Fetch<'_> {
    /// The commits to send as `have`: the commits that the branches, the tags, and the remote
    /// references of the repository name, and the commits of the journal that still hold and
    /// that no other of them has as a parent; a recorded commit that an own reference reaches by
    /// now is not among them. The set is that of the start of the fetch: a
    /// commit that [`receive`](Self::receive) records afterwards is not added.
    pub fn have(&self) -> &BTreeSet<CommitId> {
        &self.have
    }

    /// The commits that an earlier fetch recorded in the journal and that the check found
    /// complete again when this fetch began, in the order of the journal. A recorded commit
    /// whose objects are gone since, that reaches an object whose file is damaged, or that an
    /// own reference reaches by now, is left out.
    pub fn resumed(&self) -> &[CommitId] {
        &self.resumed
    }

    /// Stores the objects of a stream, checks every commit of it for connectivity once its
    /// objects are durable, and records in the journal those that are complete and were not
    /// before: a commit that a reference reaches, or that this fetch found complete already (at
    /// its start from the journal, or since), gets no line. A recorded commit that did not hold
    /// at the start gets a second line when it arrives again. May be called for several streams.
    ///
    ///
    /// When the stream is cut off or fails, the objects that were read whole are stored, and
    /// their commits are checked and recorded, before the error is returned: what arrived is
    /// kept for the next fetch.
    ///
    /// # Errors
    ///
    /// [`ReceiveError::Stream`] if the reader refuses the stream, a body is above the bound of
    /// an object, or the carrier fails, [`ReceiveError::RequestTooLarge`] if the stream is
    /// longer than its bound, [`ReceiveError::Store`] if an object cannot be stored,
    /// [`ReceiveError::Connectivity`] for a commit of the stream that is not complete, which
    /// ends the call after the commits before it are recorded, and if the check cannot be made
    /// (the storage fails, or the history below the own references cannot be read), and
    /// [`ReceiveError::Journal`] if the journal cannot be written. If several fail, the first
    /// of this order is returned: the store, the connectivity, the journal, the bound of the
    /// request, the stream. The bound goes before a stream that only ends there; a frame that
    /// the reader refuses at the bound is an error of the stream. An error of the stream is
    /// therefore not returned if the objects that arrived before it cannot be stored, checked,
    /// or recorded.
    pub fn receive<R: BufRead>(
        &mut self,
        stream: R,
        bounds: Bounds,
    ) -> Result<Received, ReceiveError> {
        let Self {
            repository,
            connectivity,
            journal,
            ..
        } = self;
        receive_stream(
            repository.objects(),
            &repository.metadata_dir(),
            stream,
            bounds,
            |commits| {
                let mut complete = Vec::new();
                let mut checked = Ok(());
                for &commit in commits {
                    // A commit that was complete before needs no line: a reference reaches
                    // it, or the journal names it already.
                    let found = connectivity
                        .is_complete(commit)
                        .and_then(|known| connectivity.ensure_complete(commit).map(|()| !known));
                    match found {
                        Ok(true) => complete.push(commit),
                        Ok(false) => {}
                        Err(error) => {
                            checked = Err(ReceiveError::Connectivity(error));
                            break;
                        }
                    }
                }
                // What the sender delivered goes before a failure to write it down.
                let recorded = journal.record(&complete);
                checked?;
                recorded.map_err(ReceiveError::Journal)
            },
        )
    }

    /// Checks that every commit of `want` is complete: the walk of the connectivity check from
    /// each of them. It reads objects that were in the store before as well, so a wanted commit
    /// that no stream of this fetch carried is checked like one that arrived. The caller moves a
    /// remote reference only to a commit that passed.
    ///
    /// A commit that this fetch found complete before is not read again, so the answer is that
    /// of the time when it was read, without the lock of the references. A purge that removes
    /// an object of such a commit between that read and the move of the reference is not seen
    /// here; the caller that moves the references under the lock closes the gap with
    /// [`Repository::check_connectivity`] under that lock, or accepts it.
    ///
    /// # Errors
    ///
    /// See [`Repository::check_connectivity`], without [`ConnectivityError::Refs`]: the
    /// references were read when the fetch began.
    pub fn check(&mut self, want: &BTreeSet<CommitId>) -> Result<(), ConnectivityError> {
        want.iter()
            .try_for_each(|&commit| self.connectivity.ensure_complete(commit))
    }

    /// Ends the fetch after the remote references have moved: removes the journal.
    ///
    /// # Errors
    ///
    /// [`FetchJournalError::Io`] if the file cannot be removed or its directory cannot be
    /// flushed. The fetch itself has happened then, and the next one reads the journal that
    /// stayed.
    pub fn finish(self) -> Result<(), FetchJournalError> {
        self.journal.remove()
    }
}

impl Repository {
    /// Stores the objects of a stream without naming them: the upload of a push on a hub
    /// (ADR-023). The ID of every object is computed here. When the call returns, every object
    /// of the stream is durable, also one that the store had already, so the call is idempotent.
    /// A purge that runs at the same time may remove an object that the store had; the check of
    /// the ref update then reports it as missing.
    ///
    /// Nothing is checked beyond the frames and the bounds. The objects stay unnamed until a
    /// ref update names them, and [`check_connectivity`](Self::check_connectivity) is the check
    /// of that update. The call takes no lock of the references and runs beside other uploads
    /// and beside a writer that holds the lock; it works on a bare repository and on one with a
    /// working copy alike.
    ///
    /// # Errors
    ///
    /// [`ReceiveError::Stream`] if the reader refuses the stream, a body is above the bound of
    /// an object, or the carrier fails, [`ReceiveError::RequestTooLarge`] if the stream is
    /// longer than its bound, and [`ReceiveError::Store`] if an object cannot be stored. After
    /// an error of the stream the objects that were read whole are stored; they are unnamed and
    /// harmless.
    pub fn receive_objects<R: BufRead>(
        &self,
        stream: R,
        bounds: Bounds,
    ) -> Result<Received, ReceiveError> {
        receive_stream(self.objects(), &self.metadata_dir(), stream, bounds, |_| {
            Ok(())
        })
    }

    /// The connectivity check of ADR-023: walks from every commit of `tips` down to commits that
    /// a branch, a tag, or a remote reference of this repository reaches, and compares the trees
    /// of each commit on the way with those of its parents. Every commit, tree, and blob that
    /// the walk meets must be in the store with the right kind, and every tree and commit must
    /// be readable by the strict readers (ADR-005). Of a blob the header is read, not the body.
    ///
    /// The walk does not ask where an object came from: an object that was in the store before
    /// is checked like one of an upload, and a commit whose trees lie in the store already
    /// passes. A commit that an own reference reaches is taken as complete and not checked
    /// (ADR-015). An object that the walk does not reach is no error.
    ///
    /// The cost is the new commits and trees, the header of every new blob, and the trees of
    /// the parents at the changed paths, and, as soon as the walk meets a commit that no own
    /// reference reaches, one read of every commit below the own references: only the whole of
    /// that history says that a new commit is not part of it.
    ///
    /// A hub calls this for the new tip of a ref update before it accepts it (ADR-025). It
    /// takes no lock; a caller that names the tips afterwards holds the lock of the references
    /// from before the call, so that no purge removes an object in between (ADR-014).
    ///
    /// # Errors
    ///
    /// [`ConnectivityError::Missing`], [`ConnectivityError::WrongKind`], and
    /// [`ConnectivityError::Unreadable`] for the first object that does not hold,
    /// [`ConnectivityError::Store`] if the storage fails or the file of an object is damaged,
    /// [`ConnectivityError::Refs`] if the references cannot be read, and
    /// [`ConnectivityError::Own`] if the history below them cannot be read.
    pub fn check_connectivity(&self, tips: &[CommitId]) -> Result<(), ConnectivityError> {
        let own = own_tips(self)?;
        let mut connectivity = Connectivity::new(self.objects(), &own);
        tips.iter()
            .try_for_each(|&tip| connectivity.ensure_complete(tip))
    }

    /// Removes the directories in `tmp/` that receiving operations left when their process
    /// ended: those whose guard is free. A directory of an operation that runs stays. Every
    /// receiving operation does the same at its start, so a hub calls this only to clean up
    /// without waiting for the next upload, for example when it starts. Best effort: a directory
    /// that cannot be removed stays for the next call.
    pub fn remove_receive_leftovers(&self) {
        remove_stale_receive_dirs(&self.metadata_dir());
    }

    /// Begins a fetch on its receiving side: opens the journal `fetch-pending`, or makes it,
    /// and checks the commits that an earlier fetch recorded in it once more, so that the
    /// journal is a hint and never a proof. See [`Fetch`] for the steps.
    ///
    /// # Errors
    ///
    /// [`ReceiveError::Refs`] with [`RefError::Bare`] in a bare repository, which has no remote
    /// and writes no `fetch` entry, and if the references cannot be read,
    /// [`ReceiveError::Journal`] if the journal cannot be opened, is damaged, or is held by
    /// another fetch, and [`ReceiveError::Connectivity`] if the storage fails, the history
    /// below the own references cannot be read, or a reader returns a value with another ID
    /// while the recorded commits are checked. A recorded commit that is not complete any more,
    /// or that reaches an object whose file is damaged, is no error: it is not resumed and
    /// arrives again.
    pub fn begin_fetch(&self) -> Result<Fetch<'_>, ReceiveError> {
        self.require_oplog("fetch").map_err(ReceiveError::from)?;
        // The references are read before the journal is made: a refusal leaves no file.
        let mut have = own_tips(self)?;
        let mut connectivity = Connectivity::new(self.objects(), &have);
        let (journal, recorded) = Journal::open(&self.metadata_dir())?;
        let mut resumed = Vec::new();
        for commit in recorded {
            match connectivity.ensure_complete(commit) {
                Ok(()) if connectivity.verified.contains(&commit) => resumed.push(commit),
                // An own reference reaches it by now.
                Ok(()) => {}
                // Its objects are gone since, as after a purge, or the file of one is damaged:
                // it is fetched again. The transfer writes a damaged file anew where the store
                // can replace it; else the fetch fails there, with the damage as its error.
                Err(error) if error.is_incomplete() || error.is_damaged_object() => {}
                Err(error) => return Err(ReceiveError::Connectivity(error)),
            }
        }
        // A commit says that everything below it is there, so the parents need no line.
        let mut parents = HashSet::new();
        for commit in &resumed {
            if let Ok(Some(commit)) = self.objects().get_any_commit(commit) {
                parents.extend(commit.parents().iter().copied());
            }
        }
        have.extend(resumed.iter().filter(|commit| !parents.contains(commit)));
        Ok(Fetch {
            repository: self,
            connectivity,
            journal,
            have,
            resumed,
        })
    }
}

#[cfg(test)]
mod tests;
