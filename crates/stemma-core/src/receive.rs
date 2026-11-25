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
//! A last line without a line feed is left by a process that ended while it wrote, and is cut
//! off when the journal is opened. A commit may stand more than once: one that was recorded,
//! lost its objects, and arrived again.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::conflict::AnyCommit;
use crate::fault;
use crate::history::{read_commit, HistoryError};
use crate::object::{CommitId, EntryMode, Kind, ObjectId, Tree, TreeEntry, TreeId};
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

/// The count of objects in temporary files from which a receiver makes them durable at the next
/// commit of the stream. The tests take a small count, so that a short stream fills a batch.
const FLUSH_OBJECTS: u64 = if cfg!(test) { 4 } else { 1024 };

/// The bytes of object bodies in temporary files from which a receiver makes them durable at the
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
    /// refuses its body (ADR-005).
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
    /// (ADR-023). `false` for a failure of the storage or of the own repository.
    pub fn is_incomplete(&self) -> bool {
        match self {
            Self::Missing { .. } | Self::WrongKind { .. } | Self::Unreadable { .. } => true,
            Self::Store(_) | Self::Refs(_) | Self::Own(_) => false,
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
            Self::Unreadable { needed_by, source } => {
                write!(f, "{source}{}", NeededBy(*needed_by))
            }
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
    /// A commit of the stream is not connected: an object below it is missing, has the wrong
    /// kind, or is refused by a strict reader. Only a fetch checks while it receives.
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
    /// it is held; else another name is tried. On a file system that cannot lock a file the
    /// directory is used without the lock: a cleanup on the same file system cannot lock the
    /// guard either and leaves the directory.
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
/// checks commit by commit reads no object twice.
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
    /// A check for the repository, with its branches, tags, and remote references as the
    /// references that are taken as complete (ADR-015).
    fn new(repository: &'a Repository) -> Result<Self, ConnectivityError> {
        let tips = own_tips(repository)?;
        Ok(Self {
            store: repository.objects(),
            own: tips.iter().copied().collect(),
            frontier: tips.into_iter().map(|tip| (tip, None)).collect(),
            verified: HashSet::new(),
            checked: HashSet::new(),
        })
    }

    /// Whether an own reference reaches the commit. The history below the references is read
    /// only as far as the answer needs it: a commit that a reference names costs no read.
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
                // A commit that two children name is entered twice and checked once.
                Step::Exit(id, commit) => {
                    if !self.verified.contains(&id) {
                        self.check_trees(id, &commit)?;
                        self.verified.insert(id);
                    }
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

/// The open journal of a running fetch, with the lock of the system on it.
struct Journal {
    path: PathBuf,
    file: File,
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
    /// that was cut off is removed from the file.
    ///
    /// On a file system that cannot lock a file the journal is used without the lock, and two
    /// fetches at once may then mix their lines; the reader refuses such a journal.
    fn open(root: &Path) -> Result<(Self, Vec<CommitId>), FetchJournalError> {
        let path = root.join(FETCH_JOURNAL);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        let file = fault::open_with(&options, &path).map_err(|source| FetchJournalError::Io {
            operation: "open the journal of the fetch",
            path: path.clone(),
            source,
        })?;
        if matches!(file.try_lock(), Err(TryLockError::WouldBlock)) {
            return Err(FetchJournalError::Running { path });
        }
        let mut journal = Self { path, file };
        let mut bytes = Vec::new();
        (&journal.file)
            .take(MAX_JOURNAL_LEN + 1)
            .read_to_end(&mut bytes)
            .map_err(journal.io("read the journal of the fetch"))?;
        // What ends with a line feed was written whole; the rest is a line that was cut off.
        let whole = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |end| end + 1);
        let commits = journal.parse(&bytes[..whole])?;
        if whole < bytes.len() {
            journal
                .file
                .set_len(whole as u64)
                .map_err(journal.io("cut the last line of the journal of the fetch"))?;
        }
        if whole == 0 {
            journal.append(&format!("{JOURNAL_HEADER}\n"))?;
            // The entry of a new file is flushed; that of a file that was there needs none.
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
        if bytes.len() as u64 > MAX_JOURNAL_LEN {
            return Err(malformed(1));
        }
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

    /// Writes `text` at the end of the journal and flushes the file.
    fn append(&mut self, text: &str) -> Result<(), FetchJournalError> {
        let operation = "write the journal of the fetch";
        self.file
            .seek(SeekFrom::End(0))
            .and_then(|_| self.file.write_all(text.as_bytes()))
            .and_then(|()| self.file.sync_all())
            .map_err(self.io(operation))
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
        let Self { path, file } = self;
        // Windows removes no file that is open.
        drop(file);
        let io = |operation: &'static str, at: &Path| {
            let at = at.to_path_buf();
            move |source| FetchJournalError::Io {
                operation,
                path: at,
                source,
            }
        };
        match fault::remove_file(&path) {
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
/// recorded commits as further `have` lines, so the remote sends only what is still missing.
/// The value holds the lock of the system on the journal: a second fetch in the same repository
/// is refused while it lives ([`FetchJournalError::Running`]), and on Windows no other handle
/// reads the file meanwhile.
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
    /// that no other of them has as a parent.
    pub fn have(&self) -> &BTreeSet<CommitId> {
        &self.have
    }

    /// The commits that an earlier fetch recorded in the journal and that the check found
    /// complete again when this fetch began, in the order of the journal. A recorded commit
    /// whose objects are gone since, or that an own reference reaches by now, is left out.
    pub fn resumed(&self) -> &[CommitId] {
        &self.resumed
    }

    /// Stores the objects of a stream, checks every commit of it for connectivity once its
    /// objects are durable, and records in the journal those that are complete and were not
    /// before: a commit that a reference reaches or that the journal names gets no line. May be
    /// called for several streams.
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
    /// ends the call after the commits before it are recorded, and [`ReceiveError::Journal`] if
    /// the journal cannot be written.
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
                journal.record(&complete)?;
                checked
            },
        )
    }

    /// Checks that every commit of `want` is complete: the walk of the connectivity check from
    /// each of them. It reads objects that were in the store before as well, so a wanted commit
    /// that no stream of this fetch carried is checked like one that arrived. The caller moves a
    /// remote reference only to a commit that passed.
    ///
    /// # Errors
    ///
    /// See [`Repository::check_connectivity`].
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
    /// passes. A commit that an own reference reaches is taken as complete and not read
    /// (ADR-015). An object that the walk does not reach is no error.
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
        let mut connectivity = Connectivity::new(self)?;
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
    /// another fetch, and [`ReceiveError::Connectivity`] if the storage fails or the history
    /// below the own references cannot be read while the recorded commits are checked. A
    /// recorded commit that is not complete any more is no error.
    pub fn begin_fetch(&self) -> Result<Fetch<'_>, ReceiveError> {
        self.require_oplog("fetch").map_err(ReceiveError::from)?;
        // The references are read before the journal is made: a refusal leaves no file.
        let mut connectivity = Connectivity::new(self)?;
        let (journal, recorded) = Journal::open(&self.metadata_dir())?;
        let mut resumed = Vec::new();
        for commit in recorded {
            match connectivity.ensure_complete(commit) {
                Ok(()) if connectivity.verified.contains(&commit) => resumed.push(commit),
                // An own reference reaches it by now.
                Ok(()) => {}
                // Its objects are gone since, as after a purge: it is fetched again.
                Err(error) if error.is_incomplete() => {}
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
        let mut have = own_tips(self)?;
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
