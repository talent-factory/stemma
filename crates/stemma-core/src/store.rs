//! Object storage: the [`ObjectStore`] trait and a file system implementation (ADR-007).
//!
//! A store keeps objects by ID. It does not interpret them: it stores the object kind and the
//! encoded body, and gives both back. [`ObjectStore::get_tree`] and [`ObjectStore::get_commit`]
//! add the strict readers of ADR-005 on top; the store itself does not interpret a body.
//!
//! [`FsObjectStore`] keeps one file per object ("loose objects") below the repository's
//! metadata directory:
//!
//! ```text
//! <root>/objects/<algorithm>/<first 2 hex digits>/<remaining 62 hex digits>
//! ```
//!
//! The file holds exactly the input that was hashed: `<kind> <length>`, a NUL byte, and the
//! body. There is no compression. A read hashes the content again and rejects an object that
//! does not match its ID, so a damaged object is reported and never passed on.

use std::cell::RefCell;
use std::collections::hash_map::RandomState;
use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::object::{
    hash_encoding, Blob, BlobId, Commit, CommitId, CommitParseError, Kind, ObjectId, Tree, TreeId,
    TreeParseError, METADATA_DIR, PREFIX,
};

/// Why a stored object is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CorruptReason {
    /// There is something other than a regular file at the path of the object: a directory, a
    /// symbolic link, or a special file.
    NotARegularFile,
    /// The content has no NUL byte that ends the header.
    NoHeaderEnd,
    /// The header is not valid UTF-8.
    HeaderNotUtf8,
    /// The header is not of the form `<kind> <length>`.
    HeaderShape,
    /// The kind in the header is not a known kind. Carries the kind as written.
    UnknownKind(String),
    /// The header is not the canonical header for its kind and the length of the body, for
    /// example because the length is wrong, has a leading zero, or has a plus sign.
    HeaderMismatch {
        /// The header as stored, without the NUL byte.
        found: String,
        /// The header that the kind and the body require, without the NUL byte.
        expected: String,
    },
    /// The content hashes to another ID than the one it is stored under.
    HashMismatch {
        /// The ID the stored content hashes to.
        actual: ObjectId,
    },
}

impl fmt::Display for CorruptReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotARegularFile => f.write_str("the path is not a regular file"),
            Self::NoHeaderEnd => f.write_str("the header is not terminated by a NUL byte"),
            Self::HeaderNotUtf8 => f.write_str("the header is not valid UTF-8"),
            Self::HeaderShape => f.write_str("the header is not `<kind> <length>`"),
            Self::UnknownKind(kind) => write!(f, "unknown object kind `{}`", kind.escape_debug()),
            Self::HeaderMismatch { found, expected } => write!(
                f,
                "the header is `{}`, but the content requires `{}`",
                found.escape_debug(),
                expected.escape_debug()
            ),
            Self::HashMismatch { actual } => {
                write!(f, "the content hashes to {actual}, not to the ID")
            }
        }
    }
}

/// Reasons why a store operation fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum StoreError {
    /// The underlying storage failed. The cause is the field `source`, and its message ends the
    /// message of this error. Use [`StoreError::io_error`] to reach it through a wrapper.
    Io {
        /// What the store was doing, for example "read the object file".
        operation: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// A stored object is damaged: it is not in canonical form or does not match its ID.
    Corrupt {
        /// The ID the object was stored under.
        id: ObjectId,
        /// The file that holds the object.
        path: PathBuf,
        /// What is wrong with it.
        reason: CorruptReason,
    },
    /// The object exists, but it is not of the kind the caller expected.
    KindMismatch {
        /// The ID that was requested.
        id: ObjectId,
        /// The kind the caller expected.
        expected: Kind,
        /// The kind the object has.
        found: Kind,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
            Self::Corrupt { id, path, reason } => {
                write!(f, "object {id} is corrupt (`{}`): {reason}", path.display())
            }
            Self::KindMismatch {
                id,
                expected,
                found,
            } => write!(
                f,
                "object {id} is a {}, not a {}",
                found.name(),
                expected.name()
            ),
        }
    }
}

impl StoreError {
    /// Returns the error of the file system if this is [`StoreError::Io`]. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation); this does, for a
    /// caller that maps [`io::ErrorKind`] to a result such as an exit code.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Corrupt { .. } | Self::KindMismatch { .. } => None,
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes that of the I/O error, so the source is its own cause.
            Self::Io { source, .. } => source.source(),
            Self::Corrupt { .. } | Self::KindMismatch { .. } => None,
        }
    }
}

/// Why [`ObjectStore::get_tree`] or [`ObjectStore::get_commit`] fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReadError {
    /// The store could not return the object: the storage failed, the object is damaged, or it
    /// has another kind. The message and the cause are those of the store error. Boxed because
    /// the unboxed error is too large for clippy's `result_large_err` lint.
    Store(Box<StoreError>),
    /// The object is a tree and its content is intact, but its body is not in canonical form.
    /// The writers of this crate always produce canonical bodies.
    Tree {
        /// The ID the tree was requested by.
        id: TreeId,
        /// What is wrong with the body.
        source: TreeParseError,
    },
    /// The object is a commit and its content is intact, but its body is not in canonical form.
    /// The writers of this crate always produce canonical bodies.
    Commit {
        /// The ID the commit was requested by.
        id: CommitId,
        /// What is wrong with the body.
        source: CommitParseError,
    },
    /// The reader accepted the body, but the value it returned has another ID than the one the
    /// object was requested by. This means that the reader accepted a body that it would not
    /// have written. It is a defect of the reader and is reported instead of returning a value
    /// that does not match its ID.
    IdMismatch {
        /// The ID the object was requested by.
        requested: ObjectId,
        /// The ID of the value that was read.
        parsed: ObjectId,
    },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(source) => write!(f, "{source}"),
            Self::Tree { id, source } => write!(f, "tree {id} is not canonical: {source}"),
            Self::Commit { id, source } => write!(f, "commit {id} is not canonical: {source}"),
            Self::IdMismatch { requested, parsed } => write!(
                f,
                "object {requested} was read as a value with the ID {parsed}: \
                 the reader accepted a body that is not canonical"
            ),
        }
    }
}

impl ReadError {
    /// Returns the error of the store if the object could not be returned: the storage failed,
    /// the object is damaged, or it has another kind. `None` for a body that is not canonical.
    pub fn store_error(&self) -> Option<&StoreError> {
        match self {
            Self::Store(source) => Some(source),
            Self::Tree { .. } | Self::Commit { .. } | Self::IdMismatch { .. } => None,
        }
    }
}

impl Error for ReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // Transparent: the message is that of the store error, so its cause is the cause here.
            Self::Store(source) => source.source(),
            // The message of each variant includes the message of the cause, so the source is the
            // cause of the cause (see the crate documentation).
            Self::Tree { source, .. } => source.source(),
            Self::Commit { source, .. } => source.source(),
            Self::IdMismatch { .. } => None,
        }
    }
}

/// Fails unless the value that a reader returned has the ID the object was requested by. The
/// store has checked that the body hashes to the requested ID, and a reader promises that the
/// value encodes back to the body it accepted, so the IDs are equal. The check enforces the
/// promise in every build and not only in debug builds.
fn confirm_id(requested: ObjectId, parsed: ObjectId) -> Result<(), ReadError> {
    if requested == parsed {
        Ok(())
    } else {
        Err(ReadError::IdMismatch { requested, parsed })
    }
}

/// Returns a function that wraps an I/O error with the operation and the path.
fn io_error<'a>(
    operation: &'static str,
    path: &'a Path,
) -> impl FnOnce(io::Error) -> StoreError + 'a {
    move |source| StoreError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

/// An object as stored: its kind and its encoded body. A store only returns an object whose
/// content hashes to the ID it was requested by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawObject {
    kind: Kind,
    body: Vec<u8>,
}

impl RawObject {
    /// Returns the object kind.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Returns the encoded body.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Returns the encoded body and drops the rest.
    pub fn into_body(self) -> Vec<u8> {
        self.body
    }
}

pub(crate) mod sealed {
    /// Keeps [`ObjectStore`](super::ObjectStore) and [`RefStore`](crate::refs::RefStore) from
    /// being implemented outside this crate.
    pub trait Sealed {}
}

/// Storage for objects, addressed by their ID.
///
/// The trait is sealed: only this crate implements it. Storing is idempotent. The same object
/// always gets the same ID, and the ID is computed by the store from the kind and the body, so
/// a caller cannot store an object under another ID. A store never exposes a partly written
/// object, and [`get`](Self::get) only returns content that hashes to the requested ID.
///
/// After an operation that returns an error, an object may or may not have been stored. Storing
/// it again is always safe.
pub trait ObjectStore: sealed::Sealed {
    /// Stores a blob and returns its ID, which equals [`Blob::id`].
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if the storage fails, and [`StoreError::Corrupt`] if an entry
    /// that is not a regular file blocks the path of the object.
    fn put_blob(&self, blob: &Blob) -> Result<BlobId, StoreError>;

    /// Stores a tree and returns its ID, which equals [`Tree::id`].
    ///
    /// # Errors
    ///
    /// See [`put_blob`](Self::put_blob).
    fn put_tree(&self, tree: &Tree) -> Result<TreeId, StoreError>;

    /// Stores a commit and returns its ID, which equals [`Commit::id`].
    ///
    /// # Errors
    ///
    /// See [`put_blob`](Self::put_blob).
    fn put_commit(&self, commit: &Commit) -> Result<CommitId, StoreError>;

    /// Returns the object with the ID, or `None` if the store does not have it. The content has
    /// been checked: its header is canonical and it hashes to the ID.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Corrupt`] if the stored object is damaged, and
    /// [`StoreError::Io`] if the storage fails.
    fn get(&self, id: &ObjectId) -> Result<Option<RawObject>, StoreError>;

    /// Like [`get`](Self::get), but fails if the object is not of the expected kind.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::KindMismatch`] if the object has another kind, and the errors of
    /// [`get`](Self::get).
    fn get_expecting(&self, id: &ObjectId, kind: Kind) -> Result<Option<RawObject>, StoreError> {
        match self.get(id)? {
            Some(object) if object.kind() != kind => Err(StoreError::KindMismatch {
                id: *id,
                expected: kind,
                found: object.kind(),
            }),
            other => Ok(other),
        }
    }

    /// Returns whether an object file exists for the ID. It does not check the content, so it can
    /// be `true` for an object that [`get`](Self::get) reports as corrupt.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if the storage fails.
    fn contains(&self, id: &ObjectId) -> Result<bool, StoreError>;

    /// Returns the ID as a [`BlobId`] if the store has an object with this ID and it is a blob,
    /// and `None` if the store does not have it. This is the checked way from an untyped ID, for
    /// example one that was read from text, to a typed one. The object is read and its content is
    /// checked as in [`get`](Self::get) (canonical header, hash), because the kind is known only
    /// from the stored object. The body is not decoded, so a typed ID does not mean that a tree or
    /// a commit is in canonical form. The result holds at the time of the call: it says that the
    /// object existed with this kind then, not that it stays in the store.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::KindMismatch`] if the object is not a blob, and the errors of
    /// [`get`](Self::get).
    fn resolve_blob(&self, id: &ObjectId) -> Result<Option<BlobId>, StoreError> {
        Ok(self
            .get_expecting(id, Kind::Blob)?
            .map(|_| BlobId::new(*id)))
    }

    /// Like [`resolve_blob`](Self::resolve_blob), for a tree.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::KindMismatch`] if the object is not a tree, and the errors of
    /// [`get`](Self::get).
    fn resolve_tree(&self, id: &ObjectId) -> Result<Option<TreeId>, StoreError> {
        Ok(self
            .get_expecting(id, Kind::Tree)?
            .map(|_| TreeId::new(*id)))
    }

    /// Like [`resolve_blob`](Self::resolve_blob), for a commit.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::KindMismatch`] if the object is not a commit, and the errors of
    /// [`get`](Self::get).
    fn resolve_commit(&self, id: &ObjectId) -> Result<Option<CommitId>, StoreError> {
        Ok(self
            .get_expecting(id, Kind::Commit)?
            .map(|_| CommitId::new(*id)))
    }

    /// Returns the tree with the ID, or `None` if the store does not have it. The object is read
    /// and checked as in [`get`](Self::get), its kind is confirmed, and its body is read by
    /// [`Tree::parse`]. The entries of the result are claims about the objects they name; see
    /// [`Tree::verify`] for the check against this store.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError::Store`] with [`StoreError::KindMismatch`] if the object is not a
    /// tree, and with the other errors of [`get`](Self::get). Returns [`ReadError::Tree`] if the
    /// body is not canonical, and [`ReadError::IdMismatch`] if the reader returned a tree with
    /// another ID, which a correct reader never does. To get the [`StoreError`] out of
    /// [`ReadError::Store`], use [`ReadError::store_error`].
    fn get_tree(&self, id: &TreeId) -> Result<Option<Tree>, ReadError> {
        let Some(object) = self
            .get_expecting(&id.object_id(), Kind::Tree)
            .map_err(|source| ReadError::Store(Box::new(source)))?
        else {
            return Ok(None);
        };
        let tree =
            Tree::parse(object.body()).map_err(|source| ReadError::Tree { id: *id, source })?;
        confirm_id(id.object_id(), tree.id().object_id())?;
        Ok(Some(tree))
    }

    /// Like [`get_tree`](Self::get_tree), for a commit. The IDs of the tree and of the parents in
    /// the result are claims; see [`Commit::verify`].
    ///
    /// # Errors
    ///
    /// Returns [`ReadError::Store`] with [`StoreError::KindMismatch`] if the object is not a
    /// commit, and with the other errors of [`get`](Self::get). Returns [`ReadError::Commit`] if
    /// the body is not canonical, and [`ReadError::IdMismatch`] as [`get_tree`](Self::get_tree)
    /// does.
    fn get_commit(&self, id: &CommitId) -> Result<Option<Commit>, ReadError> {
        let Some(object) = self
            .get_expecting(&id.object_id(), Kind::Commit)
            .map_err(|source| ReadError::Store(Box::new(source)))?
        else {
            return Ok(None);
        };
        let commit =
            Commit::parse(object.body()).map_err(|source| ReadError::Commit { id: *id, source })?;
        confirm_id(id.object_id(), commit.id().object_id())?;
        Ok(Some(commit))
    }
}

/// Counter that keeps temporary file names unique within a process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How often a write tries a new temporary name when the name is taken.
const TEMP_ATTEMPTS: usize = 16;

/// Whether a damaged object file is replaced when the same object is stored again. A rename
/// replaces an existing file on Unix. Elsewhere the result is not known, so the error is
/// reported instead (ADR-007).
const REPLACE_DAMAGED: bool = cfg!(unix);

/// Loose objects in a directory tree (ADR-007).
///
/// `root` is the repository's metadata directory and has to exist: the store creates `tmp` and
/// `objects` below it, but never `root` itself, so a wrong path is an error and not an empty
/// store.
///
/// An object is written to a temporary file below `<root>/tmp/`, flushed to disk, made
/// read-only, and then renamed to its final path. A reader therefore sees either no object or a
/// complete one, provided that `tmp` and `objects` are on one file system. On Unix the
/// directories that were created, and the directory that received the object, are flushed as
/// well. Elsewhere only the atomicity holds, not the durability of the directory entries.
#[derive(Debug, Clone)]
pub struct FsObjectStore {
    root: PathBuf,
}

impl sealed::Sealed for FsObjectStore {}

impl FsObjectStore {
    /// Creates a store that keeps its files below `root`, the repository's metadata directory.
    /// The directory is not checked here; every operation checks it.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the path of the file for the object ID.
    pub(crate) fn object_path(&self, id: &ObjectId) -> PathBuf {
        let hex = id.hex();
        self.root
            .join("objects")
            .join(id.algorithm())
            .join(&hex[..2])
            .join(&hex[2..])
    }

    /// Lists the IDs of the objects in the store, in no particular order, without reading them. A
    /// file whose path is not that of an ID is left out, as no ID can name it and no read reaches
    /// it; the files in `tmp/` are not objects. The scan of a purge needs the list (ADR-014).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if a directory cannot be listed, if a directory of the fan out is
    /// a link (a read follows it), and if a name is that of an ID in capitals (a file system that
    /// ignores case finds it): in both cases its objects exist for a read but not for the list.
    pub(crate) fn ids(&self) -> Result<Vec<ObjectId>, StoreError> {
        self.require_root()?;
        let algorithm = self.root.join("objects").join(PREFIX.trim_end_matches(':'));
        let mut ids = Vec::new();
        let fan_outs = match fs::read_dir(&algorithm) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ids),
            Err(error) => return Err(io_error("list the objects in", &algorithm)(error)),
        };
        for fan_out in fan_outs {
            let fan_out = fan_out.map_err(io_error("list the objects in", &algorithm))?;
            let path = fan_out.path();
            let kind = fan_out.file_type().map_err(io_error("inspect", &path))?;
            if kind.is_symlink() {
                return Err(StoreError::Io {
                    operation: "list the objects in",
                    path,
                    source: io::Error::new(
                        io::ErrorKind::InvalidData,
                        "a directory of the object store is a link",
                    ),
                });
            }
            if !kind.is_dir() {
                continue;
            }
            let prefix = fan_out.file_name();
            for object in fs::read_dir(&path).map_err(io_error("list the objects in", &path))? {
                let object = object.map_err(io_error("list the objects in", &path))?;
                let name = object.file_name();
                let (Some(prefix), Some(rest)) = (prefix.to_str(), name.to_str()) else {
                    continue;
                };
                let text = format!("{PREFIX}{prefix}{rest}");
                if let Ok(id) = ObjectId::from_str(&text) {
                    if self.object_path(&id) == object.path() {
                        ids.push(id);
                    }
                } else if ObjectId::from_str(&text.to_ascii_lowercase()).is_ok() {
                    return Err(StoreError::Io {
                        operation: "list the objects in",
                        path: object.path(),
                        source: io::Error::new(
                            io::ErrorKind::InvalidData,
                            "the name of an object is not in lower case",
                        ),
                    });
                }
            }
        }
        Ok(ids)
    }

    /// Returns the kind of a stored object from its header, without reading or checking the body,
    /// or `None` if the store has no object with the ID. The scan of a purge needs only the kind
    /// of each object and would otherwise read and hash every blob (ADR-014). A body that does not
    /// match its ID is found only by a full read.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Corrupt`] for a path that is not a regular file and for a header that
    /// is not one that [`Kind::header`] writes, and [`StoreError::Io`] if the file cannot be read.
    pub(crate) fn kind_of(&self, id: &ObjectId) -> Result<Option<Kind>, StoreError> {
        // The longest header: the longest kind name, a space, the digits of `usize::MAX`, NUL.
        const MAX_HEADER: u64 = 6 + 1 + 20 + 1;
        let path = self.object_path(id);
        let corrupt = |reason| StoreError::Corrupt {
            id: *id,
            path: path.clone(),
            reason,
        };
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.require_root()?;
                return Ok(None);
            }
            Err(error) => return Err(io_error("inspect the object file", &path)(error)),
        };
        if !metadata.is_file() {
            return Err(corrupt(CorruptReason::NotARegularFile));
        }
        let mut head = Vec::new();
        match File::open(&path) {
            Ok(file) => file
                .take(MAX_HEADER)
                .read_to_end(&mut head)
                .map_err(io_error("read the object file", &path))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("read the object file", &path)(error)),
        };
        let nul = head
            .iter()
            .position(|&byte| byte == 0)
            .ok_or_else(|| corrupt(CorruptReason::NoHeaderEnd))?;
        let header =
            std::str::from_utf8(&head[..nul]).map_err(|_| corrupt(CorruptReason::HeaderNotUtf8))?;
        let (kind_name, length) = header
            .split_once(' ')
            .ok_or_else(|| corrupt(CorruptReason::HeaderShape))?;
        let kind = Kind::from_name(kind_name)
            .ok_or_else(|| corrupt(CorruptReason::UnknownKind(kind_name.to_owned())))?;
        let expected = length
            .parse::<usize>()
            .ok()
            .and_then(|length| u64::try_from(length).ok().map(|body| (length, body)))
            .filter(|(length, body)| {
                kind.header(*length) == format!("{header}\0")
                    && metadata.len() == (nul as u64 + 1 + body)
            });
        if expected.is_none() {
            let found = metadata.len().saturating_sub(nul as u64 + 1);
            return Err(corrupt(CorruptReason::HeaderMismatch {
                found: header.to_owned(),
                expected: kind
                    .header(usize::try_from(found).unwrap_or(usize::MAX))
                    .trim_end_matches('\0')
                    .to_owned(),
            }));
        }
        Ok(Some(kind))
    }

    /// Removes the file of an object and flushes its directory, so that the removal survives a
    /// crash. Only the purge deletes objects (ADR-014), and only those that nothing kept reaches.
    /// Returns `false` if the store has no file for the ID, so a repeated call is harmless. A link
    /// at the path is removed as a link and never followed; a directory there is an error. An
    /// object is read-only, which Windows refuses to remove, so the flag is cleared first there.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Corrupt`] if the path is a directory, and [`StoreError::Io`] if the
    /// file cannot be inspected or removed, or the directory cannot be flushed.
    pub(crate) fn remove(&self, id: &ObjectId) -> Result<bool, StoreError> {
        let path = self.object_path(id);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.require_root()?;
                return Ok(false);
            }
            Err(error) => return Err(io_error("inspect the object file", &path)(error)),
        };
        if metadata.is_dir() {
            return Err(StoreError::Corrupt {
                id: *id,
                path,
                reason: CorruptReason::NotARegularFile,
            });
        }
        if cfg!(windows) && metadata.permissions().readonly() {
            let mut permissions = metadata.permissions();
            #[expect(
                clippy::permissions_set_readonly_false,
                reason = "Windows has no other bits, and the file is removed next"
            )]
            permissions.set_readonly(false);
            fs::set_permissions(&path, permissions)
                .map_err(io_error("make the object file writable", &path))?;
        }
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_error("remove the object file", &path)(error)),
        }
        let fan_out = path
            .parent()
            .expect("the path of an object has a directory");
        sync_directory(fan_out)?;
        Ok(true)
    }

    /// Fails unless `root` is an existing directory.
    fn require_root(&self) -> Result<(), StoreError> {
        let operation = "open the object store directory";
        let metadata = fs::metadata(&self.root).map_err(io_error(operation, &self.root))?;
        if metadata.is_dir() {
            Ok(())
        } else {
            Err(StoreError::Io {
                operation,
                path: self.root.clone(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "not a directory"),
            })
        }
    }

    /// Stores an object of any kind and returns its ID. Only the typed methods of the trait are
    /// public, because the body of a tree or a commit has to come from a value that was checked.
    ///
    /// An object that exists and reads back correctly is left alone. A damaged one is replaced
    /// where the platform allows it (see [`REPLACE_DAMAGED`]); a directory or another entry that
    /// is not a regular file is an error.
    pub(crate) fn put_raw(&self, kind: Kind, body: &[u8]) -> Result<ObjectId, StoreError> {
        let id = hash_encoding(kind, body);
        self.require_root()?;
        if self.present(&id)? {
            return Ok(id);
        }
        let (file, temp) = self.write_temp(kind, body)?;
        let fan_out = self.place(&id, file, temp)?;
        sync_directory(&fan_out)?;
        Ok(id)
    }

    /// Starts a [`Batch`]: objects that are flushed together when it is finished.
    pub(crate) fn batch(&self) -> Batch<'_> {
        Batch {
            store: self,
            pending: RefCell::new(HashMap::new()),
        }
    }

    /// Whether the object is in place and reads back correctly, so that it need not be written. A
    /// damaged object counts as absent where it can be replaced (see [`REPLACE_DAMAGED`]); else
    /// its error is returned, as is the error for an entry that is not a regular file.
    fn present(&self, id: &ObjectId) -> Result<bool, StoreError> {
        match self.get(id) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(StoreError::Corrupt { ref reason, .. })
                if REPLACE_DAMAGED && *reason != CorruptReason::NotARegularFile =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Writes the file of an object (header and body) to a new temporary file below
    /// `<root>/tmp/`, without a flush. The file is removed when the guard is dropped.
    fn write_temp(&self, kind: Kind, body: &[u8]) -> Result<(File, TempFile), StoreError> {
        let temp_dir = self.root.join("tmp");
        create_dirs_synced(&self.root, &temp_dir)?;
        let (mut file, temp) = create_temp(&temp_dir, &mut unique_temp_name)?;
        file.write_all(kind.header(body.len()).as_bytes())
            .and_then(|()| file.write_all(body))
            .map_err(io_error("write the temporary file", &temp.path))?;
        Ok((file, temp))
    }

    /// Makes the temporary file of an object read-only, flushes it, and renames it to the path of
    /// the object, after creating its directory. Returns that directory, which is not flushed
    /// here: the caller flushes it once for all the objects that it moved there.
    ///
    /// If the rename fails but another writer has stored the same object meanwhile, the call
    /// succeeds, because the content is the same by construction. Any other failure is reported
    /// with the original error. On Unix a rename replaces a file that is already there, so two
    /// writers that race leave one file with identical content.
    fn place(&self, id: &ObjectId, file: File, mut temp: TempFile) -> Result<PathBuf, StoreError> {
        let path = self.object_path(id);
        let fan_out = path
            .parent()
            .ok_or_else(|| StoreError::Io {
                operation: "find the directory of the object",
                path: path.clone(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "the path has no parent"),
            })?
            .to_path_buf();
        create_dirs_synced(&self.root, &fan_out)?;
        let mut permissions = file
            .metadata()
            .map_err(io_error("inspect the temporary file", &temp.path))?
            .permissions();
        permissions.set_readonly(true);
        file.set_permissions(permissions)
            .map_err(io_error("make the temporary file read-only", &temp.path))?;
        file.sync_all()
            .map_err(io_error("flush the temporary file", &temp.path))?;
        drop(file);

        match fs::rename(&temp.path, &path) {
            Ok(()) => temp.disarm(),
            Err(error) => {
                // Another writer may have stored the object in the meantime. Only an object that
                // reads back correctly counts; the temporary file is removed on drop.
                return match self.get(id) {
                    Ok(Some(_)) => Ok(fan_out),
                    _ => Err(StoreError::Io {
                        operation: "move the object into place at",
                        path,
                        source: error,
                    }),
                };
            }
        }
        Ok(fan_out)
    }

    /// Reads the file at `path` as the object with the ID and checks it, as [`get`] does for
    /// the file of the object. `None` if there is no file.
    ///
    /// [`get`]: ObjectStore::get
    fn read_checked(&self, id: &ObjectId, path: &Path) -> Result<Option<RawObject>, StoreError> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.require_root()?;
                return Ok(None);
            }
            Err(error) => return Err(io_error("inspect the object file", path)(error)),
        };
        let corrupt = |reason| StoreError::Corrupt {
            id: *id,
            path: path.to_path_buf(),
            reason,
        };
        if !metadata.is_file() {
            return Err(corrupt(CorruptReason::NotARegularFile));
        }
        let mut bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            // The object was removed after the check above.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("read the object file", path)(error)),
        };

        let nul = bytes
            .iter()
            .position(|&byte| byte == 0)
            .ok_or_else(|| corrupt(CorruptReason::NoHeaderEnd))?;
        let header = std::str::from_utf8(&bytes[..nul])
            .map_err(|_| corrupt(CorruptReason::HeaderNotUtf8))?;
        let (kind_name, _) = header
            .split_once(' ')
            .ok_or_else(|| corrupt(CorruptReason::HeaderShape))?;
        let kind = Kind::from_name(kind_name)
            .ok_or_else(|| corrupt(CorruptReason::UnknownKind(kind_name.to_owned())))?;
        // The header must be exactly the one the kind and the body length produce. That rejects
        // a wrong length and a non-canonical one such as `05` or `+5`.
        let expected = kind.header(bytes.len() - nul - 1);
        if bytes[..=nul] != *expected.as_bytes() {
            return Err(corrupt(CorruptReason::HeaderMismatch {
                found: header.to_owned(),
                expected: expected.trim_end_matches('\0').to_owned(),
            }));
        }
        let actual = hash_encoding(kind, &bytes[nul + 1..]);
        if actual != *id {
            return Err(corrupt(CorruptReason::HashMismatch { actual }));
        }
        // Reuse the buffer instead of copying the body.
        bytes.drain(..=nul);
        Ok(Some(RawObject { kind, body: bytes }))
    }
}

impl ObjectStore for FsObjectStore {
    fn put_blob(&self, blob: &Blob) -> Result<BlobId, StoreError> {
        self.put_raw(Kind::Blob, blob.content()).map(BlobId::new)
    }

    fn put_tree(&self, tree: &Tree) -> Result<TreeId, StoreError> {
        self.put_raw(Kind::Tree, &tree.encode()).map(TreeId::new)
    }

    fn put_commit(&self, commit: &Commit) -> Result<CommitId, StoreError> {
        self.put_raw(Kind::Commit, &commit.encode())
            .map(CommitId::new)
    }

    fn get(&self, id: &ObjectId) -> Result<Option<RawObject>, StoreError> {
        self.read_checked(id, &self.object_path(id))
    }

    fn contains(&self, id: &ObjectId) -> Result<bool, StoreError> {
        let path = self.object_path(id);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.require_root()?;
                Ok(false)
            }
            Err(error) => Err(io_error("inspect the object file", &path)(error)),
        }
    }
}

/// Objects that are written together and flushed together (ADR-007): each is written to a
/// temporary file below `tmp/` without a flush, and [`finish`](Self::finish) flushes every file,
/// moves it into place, and then flushes each directory that received one, once. The one flush of
/// a directory for many objects is what a batch saves; the first `track` of many files was slowed
/// by one per object (STEMMA-40).
///
/// Until it is finished, a batch answers [`get`](ObjectStore::get) and
/// [`contains`](ObjectStore::contains) for its own objects as well, so that a tree can be checked
/// against the blobs of the same batch. Nothing outside the batch sees them. A batch that is
/// dropped unfinished, because a step failed, removes its files; after a crash they stay in `tmp/`
/// until the next writer takes the lock (ADR-011). So a crash loses the objects of an unfinished
/// batch and nothing else: the caller finishes it before a log entry or a pointer names one of
/// them (ADR-011), and an object that nothing names is harmless (ADR-007).
///
/// The caller holds the lock of the references, as every writer of `tmp/` does, so that the
/// cleanup of `tmp/` cannot remove a file of the batch.
pub(crate) struct Batch<'a> {
    store: &'a FsObjectStore,
    /// The temporary file of each object that is not in place yet.
    pending: RefCell<HashMap<ObjectId, TempFile>>,
}

impl Batch<'_> {
    /// Adds the object, unless the batch or the store has it already. A damaged object in the
    /// store is replaced where the platform allows it, as [`FsObjectStore::put_raw`] does.
    fn put_raw(&self, kind: Kind, body: &[u8]) -> Result<ObjectId, StoreError> {
        let id = hash_encoding(kind, body);
        if self.pending.borrow().contains_key(&id) {
            return Ok(id);
        }
        self.store.require_root()?;
        if self.store.present(&id)? {
            return Ok(id);
        }
        let (file, temp) = self.store.write_temp(kind, body)?;
        drop(file);
        self.pending.borrow_mut().insert(id, temp);
        Ok(id)
    }

    /// Flushes every object of the batch and moves it into place, then flushes the directories
    /// that received one. After an error, the objects that were moved stay, which is harmless, and
    /// the files of the others are removed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if a file cannot be opened, flushed, or moved, or a directory
    /// cannot be created or flushed, and [`StoreError::Corrupt`] as [`FsObjectStore::put_raw`]
    /// does for an entry in the way.
    pub(crate) fn finish(self) -> Result<(), StoreError> {
        let mut directories = BTreeSet::new();
        for (id, temp) in self.pending.into_inner() {
            // The file is opened again to flush it, so that a large batch keeps no file open.
            let file = OpenOptions::new()
                .write(true)
                .open(&temp.path)
                .map_err(io_error("open the temporary file", &temp.path))?;
            directories.insert(self.store.place(&id, file, temp)?);
        }
        for directory in &directories {
            sync_directory(directory)?;
        }
        Ok(())
    }
}

impl sealed::Sealed for Batch<'_> {}

impl ObjectStore for Batch<'_> {
    fn put_blob(&self, blob: &Blob) -> Result<BlobId, StoreError> {
        self.put_raw(Kind::Blob, blob.content()).map(BlobId::new)
    }

    fn put_tree(&self, tree: &Tree) -> Result<TreeId, StoreError> {
        self.put_raw(Kind::Tree, &tree.encode()).map(TreeId::new)
    }

    fn put_commit(&self, commit: &Commit) -> Result<CommitId, StoreError> {
        self.put_raw(Kind::Commit, &commit.encode())
            .map(CommitId::new)
    }

    fn get(&self, id: &ObjectId) -> Result<Option<RawObject>, StoreError> {
        match self.pending.borrow().get(id) {
            Some(temp) => self.store.read_checked(id, &temp.path),
            None => self.store.get(id),
        }
    }

    fn contains(&self, id: &ObjectId) -> Result<bool, StoreError> {
        if self.pending.borrow().contains_key(id) {
            return Ok(true);
        }
        self.store.contains(id)
    }
}

/// A temporary file that is removed on drop unless it was moved into place. The guard exists only
/// for a file that this call created, so it can never remove the file of another writer.
pub(crate) struct TempFile {
    pub(crate) path: PathBuf,
    armed: bool,
}

impl TempFile {
    /// Keeps the file: it has been moved, or is meant to stay.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            // Best effort: a file that stays behind is harmless, because temporary names are
            // never reused, and the next writer that takes the lock removes it (ADR-011).
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Returns a temporary name that is unique in this process and, with overwhelming probability,
/// across processes: the process ID, a counter, and a random number from the per-process random
/// state of the standard library. It is not meant to be unguessable.
pub(crate) fn unique_temp_name() -> String {
    let random = RandomState::new().build_hasher().finish();
    format!(
        "{}-{}-{random:016x}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Creates a new file in `dir` under a name from `next_name`. If the name is taken, it asks for
/// the next one, up to [`TEMP_ATTEMPTS`] times. A file that already exists is never touched.
pub(crate) fn create_temp(
    dir: &Path,
    next_name: &mut dyn FnMut() -> String,
) -> Result<(File, TempFile), StoreError> {
    let operation = "create the temporary file";
    let mut last = dir.to_path_buf();
    for _ in 0..TEMP_ATTEMPTS {
        let path = dir.join(next_name());
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, TempFile { path, armed: true })),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last = path,
            Err(error) => return Err(io_error(operation, &path)(error)),
        }
    }
    Err(StoreError::Io {
        operation,
        path: last,
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "every temporary name was taken",
        ),
    })
}

/// Removes the scratch that writers left behind (ADR-011): every name in `<root>/tmp/` that is not
/// a directory, and, if `root` is named [`METADATA_DIR`], every directory next to it whose name is
/// exactly one that `init` gives its temporary directory (ADR-009, [`is_init_staging_name`]). The
/// caller holds the lock of the references, and every writer of `tmp/` in this crate holds that
/// lock while it writes, so no name there belongs to a write in progress. A caller of the library
/// that stores objects through `Repository::objects()` without the lock is not protected. An
/// `init` that still builds its directory next to an existing `.stemma` fails, at the latest at
/// its rename.
///
/// It is best effort: a name that cannot be removed stays, and the next writer tries again, as a
/// cleanup must not stop a change; [`scratch_left`] lists what stayed. An entry is never followed:
/// a symbolic link in `tmp/` is removed as a name, one next to `.stemma` stays, and a directory in
/// `tmp/`, which no writer makes, stays. A directory of the working copy whose name only starts
/// like the one of `init` is content and stays.
pub(crate) fn remove_scratch(root: &Path) {
    if let Ok(entries) = fs::read_dir(root.join("tmp")) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| !kind.is_dir()) {
                remove_scratch_file(&entry.path());
            }
        }
    }
    if root.file_name() != Some(METADATA_DIR.as_ref()) {
        return;
    }
    let Some(Ok(entries)) = root.parent().map(fs::read_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let named = entry.file_name().to_str().is_some_and(is_init_staging_name);
        if named && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Removes one scratch file, best effort. An object is made read-only before its rename, and
/// Windows may refuse to remove a read-only file, so the flag is cleared there first, as the purge
/// does for an object. On Unix the permission of the directory decides.
fn remove_scratch_file(path: &Path) {
    if cfg!(windows) {
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.is_file() && metadata.permissions().readonly() {
                let mut permissions = metadata.permissions();
                #[expect(
                    clippy::permissions_set_readonly_false,
                    reason = "Windows has no other bits, and the file is removed next"
                )]
                permissions.set_readonly(false);
                let _ = fs::set_permissions(path, permissions);
            }
        }
    }
    let _ = fs::remove_file(path);
}

/// The names in `<root>/tmp/` that are not directories, sorted: what [`remove_scratch`] could not
/// remove when it is called right after it under the same lock. A missing `tmp/` holds none, and
/// one that cannot be read is reported as itself, so the caller does not take it for empty.
pub(crate) fn scratch_left(root: &Path) -> Vec<PathBuf> {
    let tmp = root.join("tmp");
    let entries = match fs::read_dir(&tmp) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => return vec![tmp],
    };
    let mut left: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    left.sort();
    left
}

/// The prefix of the temporary directory that `init` builds next to its final place (ADR-009).
pub(crate) const INIT_STAGING_PREFIX: &str = ".stemma-init-";

/// The prefix of the temporary file that a checkout writes next to its target before it renames
/// it into place (ADR-013).
pub(crate) const CHECKOUT_TEMP_PREFIX: &str = ".stemma-checkout-";

/// Whether `name` is exactly a name that `init` gives its temporary directory: the prefix
/// [`INIT_STAGING_PREFIX`] and a name of [`unique_temp_name`]. A name of the working copy such as
/// `.stemma-init-notes` is not one.
pub(crate) fn is_init_staging_name(name: &str) -> bool {
    is_scratch_name(name, INIT_STAGING_PREFIX)
}

/// Whether `name` is exactly a name that a checkout gives its temporary file: the prefix
/// [`CHECKOUT_TEMP_PREFIX`] and a name of [`unique_temp_name`]. A process that is killed between
/// writing and renaming the file leaves it behind, with the content of an old blob, and so does a
/// failed write whose file cannot be removed (ADR-013).
pub(crate) fn is_checkout_temp_name(name: &str) -> bool {
    is_scratch_name(name, CHECKOUT_TEMP_PREFIX)
}

/// Whether `name` is `prefix` followed by a name of [`unique_temp_name`]: a decimal process ID, a
/// decimal counter, and 16 lowercase hexadecimal digits, separated by `-`.
fn is_scratch_name(name: &str, prefix: &str) -> bool {
    let decimal = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let mut parts = rest.split('-');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(pid), Some(counter), Some(random), None) => {
            decimal(pid)
                && decimal(counter)
                && random.len() == 16
                && random
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }
        _ => false,
    }
}

/// Creates `dir` and every missing directory above it, and flushes the new directory entries.
/// `root` has to exist and `dir` has to be below it.
pub(crate) fn create_dirs_synced(root: &Path, dir: &Path) -> Result<(), StoreError> {
    let mut missing: Vec<&Path> = Vec::new();
    let mut current = dir;
    loop {
        match fs::symlink_metadata(current) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(current);
                current = current
                    .parent()
                    .filter(|p| p.starts_with(root))
                    .ok_or_else(|| StoreError::Io {
                        operation: "find an existing parent of",
                        path: dir.to_path_buf(),
                        source: io::Error::new(io::ErrorKind::InvalidInput, "outside the root"),
                    })?;
            }
            Err(error) => return Err(io_error("inspect the directory", current)(error)),
        }
    }
    for created in missing.iter().rev() {
        match fs::create_dir(created) {
            Ok(()) => {}
            // Another writer was faster. Whether it is a directory shows when it is used.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error("create the directory", created)(error)),
        }
    }
    // The topmost new directory is an entry in an existing one, and every new directory holds
    // the entry of the next one.
    if let Some(topmost) = missing.last().and_then(|top| top.parent()) {
        sync_directory(topmost)?;
    }
    for created in &missing {
        sync_directory(created)?;
    }
    Ok(())
}

/// Flushes a directory so that a rename or a new entry in it survives a crash.
pub(crate) fn sync_directory(dir: &Path) -> Result<(), StoreError> {
    flush_directory(dir).map_err(io_error("flush the directory", dir))
}

/// Flushes a directory with the error of the file system as it is (see [`sync_directory`]).
#[cfg(unix)]
pub(crate) fn flush_directory(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Flushes a directory on Windows. `CreateFileW` opens a directory only with
/// `FILE_FLAG_BACKUP_SEMANTICS`, and `FlushFileBuffers` needs a handle with the right to add a
/// subdirectory or the right to add a file. Write access as a whole (`GENERIC_WRITE`) is not asked
/// for: a user may create a folder below a drive root without the other rights on the root. The
/// right to add a subdirectory is tried first, the right to add a file second. A file system that
/// cannot flush a directory reports an error, as on Unix.
#[cfg(windows)]
pub(crate) fn flush_directory(dir: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    // The values from `winbase.h` and `winnt.h`; the standard library does not export them. For a
    // directory, `FILE_APPEND_DATA` is the right to add a subdirectory and `FILE_WRITE_DATA` the
    // right to add a file.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_APPEND_DATA: u32 = 0x0004;
    const FILE_WRITE_DATA: u32 = 0x0002;
    let open = |access| {
        OpenOptions::new()
            .access_mode(access)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
    };
    let directory = match open(FILE_APPEND_DATA) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => open(FILE_WRITE_DATA),
        opened => opened,
    }?;
    directory.sync_all()
}

/// No-op on targets that are neither Unix nor Windows (WASI, for example), where the store has no
/// way to flush a directory: the rename is atomic but not made durable.
#[cfg(not(any(unix, windows)))]
pub(crate) fn flush_directory(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;
    use crate::change::sample_change;
    use crate::object::{FileMode, TreeEntry};
    use crate::testutil::{overwrite, TempDir};
    use crate::Actor;

    const HELLO_ID: &str = "b3:9db8039d1b509628ad5b125ea89780b69fcde5c272215d690a2e4dc69eac75ad";
    const EMPTY_TREE_ID: &str =
        "b3:06b8469ad46d7051ad63f82809ef66c6d738d08cc3f1a5c842f3d66c05f64cf7";

    /// The ID of the exact bytes, computed with the hash function and not with the code under
    /// test.
    fn id_of(bytes: &[u8]) -> ObjectId {
        format!("b3:{}", blake3::hash(bytes).to_hex())
            .parse()
            .expect("valid ID")
    }

    fn id(text: &str) -> ObjectId {
        text.parse().expect("valid ID")
    }

    /// The number of files in `tmp`; a missing directory holds none.
    fn temp_entries(dir: &TempDir) -> usize {
        fs::read_dir(dir.path().join("tmp")).map_or(0, |entries| entries.count())
    }

    fn reason(result: Result<Option<RawObject>, StoreError>) -> CorruptReason {
        match result {
            Err(StoreError::Corrupt { reason, .. }) => reason,
            other => panic!("expected a corrupt object, got {other:?}"),
        }
    }

    // Contract

    /// Runs on every platform: on Windows it shows that a directory opens for the flush and that
    /// a directory that cannot be opened is an error instead of a skipped flush (STEMMA-84).
    #[test]
    fn a_directory_is_flushed_and_one_that_cannot_be_opened_is_an_error() {
        let dir = TempDir::new();
        fs::write(dir.path().join("entry"), b"x").unwrap();
        sync_directory(dir.path()).unwrap();
        let missing = dir.path().join("missing");
        match sync_directory(&missing) {
            Err(StoreError::Io {
                operation, path, ..
            }) => {
                assert_eq!(operation, "flush the directory");
                assert_eq!(path, missing);
            }
            other => panic!("expected an I/O error, got {other:?}"),
        }
    }

    #[test]
    fn the_scratch_that_stays_is_listed_sorted_without_directories() {
        let dir = TempDir::new();
        assert!(scratch_left(dir.path()).is_empty(), "no `tmp/` holds none");
        let tmp = dir.path().join("tmp");
        fs::create_dir(&tmp).unwrap();
        fs::write(tmp.join("b"), b"").unwrap();
        fs::write(tmp.join("a"), b"").unwrap();
        fs::create_dir(tmp.join("directory")).unwrap();
        assert_eq!(scratch_left(dir.path()), [tmp.join("a"), tmp.join("b")]);
        remove_scratch(dir.path());
        assert!(scratch_left(dir.path()).is_empty());
        assert!(tmp.join("directory").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn a_scratch_directory_that_cannot_be_read_is_named_and_not_taken_for_empty() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let tmp = dir.path().join("tmp");
        fs::create_dir(&tmp).unwrap();
        fs::write(tmp.join("a"), b"").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fs::read_dir(&tmp).is_ok();
        let left = scratch_left(dir.path());
        remove_scratch(dir.path());
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        if readable {
            eprintln!("skipped: the permission of a directory is not enforced here (root?)");
            return;
        }
        assert_eq!(left, std::slice::from_ref(&tmp));
        assert!(tmp.join("a").exists(), "what cannot be reached stays");
    }

    #[test]
    fn only_the_names_that_init_gives_its_directory_are_its_names() {
        let made = format!("{INIT_STAGING_PREFIX}{}", unique_temp_name());
        assert!(is_init_staging_name(&made), "{made}");
        assert!(is_init_staging_name(".stemma-init-1-0-0123456789abcdef"));
        let made = format!("{CHECKOUT_TEMP_PREFIX}{}", unique_temp_name());
        assert!(is_checkout_temp_name(&made), "{made}");
        assert!(!is_checkout_temp_name(".stemma-checkout-notes"));
        assert!(!is_init_staging_name(&made), "the prefixes are told apart");
        for name in [
            ".stemma-init-",
            ".stemma-init-notes",
            ".stemma-init-1-0",
            ".stemma-init-1-0-stale",
            ".stemma-init-1-0-0123456789ABCDEF",
            ".stemma-init-1-0-0123456789abcde",
            ".stemma-init-1-0-0123456789abcdef0",
            ".stemma-init-1-0-0123456789abcdef-x",
            ".stemma-init--0-0123456789abcdef",
            ".stemma-init-a-0-0123456789abcdef",
            "stemma-init-1-0-0123456789abcdef",
        ] {
            assert!(!is_init_staging_name(name), "{name}");
        }
    }

    #[test]
    fn the_trait_is_object_safe_and_the_store_can_be_shared() {
        fn takes_a_trait_object(_: &dyn ObjectStore) {}
        fn is_send_and_sync<T: Send + Sync>() {}
        let dir = TempDir::new();
        takes_a_trait_object(&dir.store());
        is_send_and_sync::<FsObjectStore>();
    }

    #[test]
    fn ids_lists_every_object_and_nothing_else() {
        let dir = TempDir::new();
        let store = dir.store();
        assert_eq!(store.ids().unwrap(), []);
        let mut stored = vec![
            ObjectId::from(store.put_blob(&Blob::new("one")).unwrap()),
            ObjectId::from(store.put_blob(&Blob::new("two")).unwrap()),
            ObjectId::from(store.put_tree(&Tree::new(vec![]).unwrap()).unwrap()),
        ];
        let fan_out = store.object_path(&stored[0]);
        let fan_out = fan_out.parent().unwrap();
        fs::write(fan_out.join("not-an-id"), "junk").unwrap();
        fs::write(dir.path().join("objects/b3/stray"), "junk").unwrap();
        let mut ids = store.ids().unwrap();
        ids.sort();
        stored.sort();
        assert_eq!(ids, stored);
    }

    #[test]
    fn remove_deletes_an_object_once_and_is_harmless_when_repeated() {
        let dir = TempDir::new();
        let store = dir.store();
        let id = store.put_blob(&Blob::new("hello")).unwrap().object_id();
        assert!(store.remove(&id).unwrap());
        assert!(!store.contains(&id).unwrap());
        assert!(!store.remove(&id).unwrap(), "a second call finds nothing");
    }

    #[test]
    fn remove_refuses_a_directory_at_the_path_of_an_object() {
        let dir = TempDir::new();
        let store = dir.store();
        let id = Blob::new("hello").id().object_id();
        fs::create_dir_all(store.object_path(&id)).unwrap();
        let error = store.remove(&id).unwrap_err();
        assert!(matches!(error, StoreError::Corrupt { .. }), "{error:?}");
    }

    #[test]
    fn kind_of_reads_the_kind_from_the_header_and_rejects_a_damaged_one() {
        let dir = TempDir::new();
        let store = dir.store();
        let blob = ObjectId::from(store.put_blob(&Blob::new("one")).unwrap());
        let tree = ObjectId::from(store.put_tree(&Tree::new(vec![]).unwrap()).unwrap());
        assert_eq!(store.kind_of(&blob).unwrap(), Some(Kind::Blob));
        assert_eq!(store.kind_of(&tree).unwrap(), Some(Kind::Tree));
        let absent = id_of(b"absent");
        assert_eq!(store.kind_of(&absent).unwrap(), None);

        for content in [&b"junk"[..], b"blob 9\0one", b"tree 01\0", b"thing 0\0"] {
            let path = store.object_path(&absent);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            assert!(
                matches!(store.kind_of(&absent), Err(StoreError::Corrupt { .. })),
                "{content:?}"
            );
            fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn ids_refuses_a_name_in_capitals() {
        let dir = TempDir::new();
        let store = dir.store();
        let upper = dir.path().join("objects/b3/AB");
        fs::create_dir_all(&upper).unwrap();
        fs::write(upper.join("c".repeat(62)), "x").unwrap();
        assert!(matches!(store.ids(), Err(StoreError::Io { .. })));
        fs::remove_dir_all(&upper).unwrap();
        let lower = dir.path().join("objects/b3/ab");
        fs::create_dir_all(&lower).unwrap();
        fs::write(lower.join("C".repeat(62)), "x").unwrap();
        assert!(matches!(store.ids(), Err(StoreError::Io { .. })));
    }

    #[test]
    fn kind_of_does_not_follow_a_link_and_rejects_a_header_without_its_shape() {
        let dir = TempDir::new();
        let store = dir.store();
        let absent = id_of(b"absent");
        let path = store.object_path(&absent);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for (content, reason) in [
            (&b"\xff\xfe 0\0"[..], CorruptReason::HeaderNotUtf8),
            (b"blob\0", CorruptReason::HeaderShape),
        ] {
            fs::write(&path, content).unwrap();
            match store.kind_of(&absent) {
                Err(StoreError::Corrupt { reason: found, .. }) => assert_eq!(found, reason),
                other => panic!("{content:?}: {other:?}"),
            }
            fs::remove_file(&path).unwrap();
        }
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            store.kind_of(&absent),
            Err(StoreError::Corrupt {
                reason: CorruptReason::NotARegularFile,
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn ids_refuses_a_fan_out_directory_that_is_a_link() {
        let dir = TempDir::new();
        let store = dir.store();
        store.put_blob(&Blob::new("one")).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("objects/b3/zz")).unwrap();
        assert!(matches!(store.ids(), Err(StoreError::Io { .. })));
    }

    #[test]
    fn put_blob_returns_the_blob_id_and_get_returns_the_content() {
        let dir = TempDir::new();
        let store = dir.store();
        let blob = Blob::new("hello");
        let stored = store.put_blob(&blob).unwrap();
        assert_eq!(stored, blob.id());
        assert_eq!(stored.to_string(), HELLO_ID);
        let object = store
            .get(&ObjectId::from(stored))
            .unwrap()
            .expect("object exists");
        assert_eq!(object.kind(), Kind::Blob);
        assert_eq!(object.body(), b"hello");
        assert_eq!(object.into_body(), b"hello");
    }

    #[test]
    fn a_tree_with_entries_is_stored_under_its_own_id() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = Tree::new(vec![TreeEntry::file(
            "a.txt",
            FileMode::File,
            BlobId::new(id(HELLO_ID)),
        )
        .unwrap()])
        .unwrap();
        let stored = store.put_tree(&tree).unwrap();
        assert_eq!(stored, tree.id());
        let object = store.get(&ObjectId::from(stored)).unwrap().unwrap();
        assert_eq!(object.kind(), Kind::Tree);
        assert_eq!(
            object.body(),
            format!("100644 a.txt\n{HELLO_ID}\n").as_bytes()
        );
    }

    #[test]
    fn a_commit_is_stored_under_its_own_id() {
        let dir = TempDir::new();
        let store = dir.store();
        let author = Actor::human("daniel").unwrap();
        let commit = Commit::new(
            TreeId::new(id(EMPTY_TREE_ID)),
            vec![],
            sample_change(),
            author,
            1_700_000_000,
            "m",
        )
        .unwrap();
        let stored = store.put_commit(&commit).unwrap();
        assert_eq!(stored, commit.id());
        let object = store.get(&ObjectId::from(stored)).unwrap().unwrap();
        assert_eq!(object.kind(), Kind::Commit);
        assert_eq!(object.body(), commit.encode());
    }

    #[test]
    fn the_kind_is_part_of_the_identity() {
        let dir = TempDir::new();
        let store = dir.store();
        let kinds = [Kind::Blob, Kind::Tree, Kind::Commit];
        let ids: Vec<ObjectId> = kinds
            .iter()
            .map(|&kind| store.put_raw(kind, b"same body").unwrap())
            .collect();
        for (i, stored) in ids.iter().enumerate() {
            for other in &ids[i + 1..] {
                assert_ne!(stored, other);
            }
            let object = store.get(stored).unwrap().unwrap();
            assert_eq!(object.kind(), kinds[i]);
            assert_eq!(object.body(), b"same body");
        }
    }

    #[test]
    fn empty_and_binary_bodies_roundtrip() {
        let dir = TempDir::new();
        let store = dir.store();
        for body in [&b""[..], &b"a\0b\xff\n\0"[..], &[0u8; 300][..]] {
            let stored = store.put_raw(Kind::Blob, body).unwrap();
            assert_eq!(store.get(&stored).unwrap().unwrap().body(), body);
        }
    }

    #[test]
    fn get_expecting_checks_the_kind() {
        let dir = TempDir::new();
        let store = dir.store();
        let stored = store.put_blob(&Blob::new("hello")).unwrap();
        let object = store
            .get_expecting(&ObjectId::from(stored), Kind::Blob)
            .unwrap()
            .unwrap();
        assert_eq!(object.body(), b"hello");
        let error = store
            .get_expecting(&ObjectId::from(stored), Kind::Tree)
            .unwrap_err();
        assert!(matches!(
            error,
            StoreError::KindMismatch {
                expected: Kind::Tree,
                found: Kind::Blob,
                ..
            }
        ));
        assert!(
            error.to_string().contains("is a blob, not a tree"),
            "{error}"
        );
        let absent = Blob::new("absent").id();
        assert!(store
            .get_expecting(&ObjectId::from(absent), Kind::Tree)
            .unwrap()
            .is_none());
    }

    #[test]
    fn resolve_returns_the_typed_id_only_for_the_matching_kind() {
        let dir = TempDir::new();
        let store = dir.store();
        let blob = store.put_blob(&Blob::new("hello")).unwrap();
        let tree = store.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
        let author = Actor::human("daniel").unwrap();
        let commit = store
            .put_commit(
                &Commit::new(tree, vec![], sample_change(), author, 1_700_000_000, "m").unwrap(),
            )
            .unwrap();
        let (blob_raw, tree_raw, commit_raw) = (
            ObjectId::from(blob),
            ObjectId::from(tree),
            ObjectId::from(commit),
        );

        assert_eq!(store.resolve_blob(&blob_raw).unwrap(), Some(blob));
        assert_eq!(store.resolve_tree(&tree_raw).unwrap(), Some(tree));
        assert_eq!(store.resolve_commit(&commit_raw).unwrap(), Some(commit));

        // Every other pairing of an ID and a kind is a mismatch that names both kinds.
        let mismatch = |error: StoreError, expected: Kind, found: Kind| {
            assert!(
                matches!(
                    error,
                    StoreError::KindMismatch { expected: e, found: f, .. }
                        if e == expected && f == found
                ),
                "{error}"
            );
        };
        mismatch(
            store.resolve_tree(&blob_raw).unwrap_err(),
            Kind::Tree,
            Kind::Blob,
        );
        mismatch(
            store.resolve_commit(&blob_raw).unwrap_err(),
            Kind::Commit,
            Kind::Blob,
        );
        mismatch(
            store.resolve_blob(&tree_raw).unwrap_err(),
            Kind::Blob,
            Kind::Tree,
        );
        mismatch(
            store.resolve_commit(&tree_raw).unwrap_err(),
            Kind::Commit,
            Kind::Tree,
        );
        mismatch(
            store.resolve_blob(&commit_raw).unwrap_err(),
            Kind::Blob,
            Kind::Commit,
        );
        mismatch(
            store.resolve_tree(&commit_raw).unwrap_err(),
            Kind::Tree,
            Kind::Commit,
        );
    }

    #[test]
    fn resolve_of_an_absent_object_is_none_for_every_kind() {
        let dir = TempDir::new();
        let store = dir.store();
        let absent = ObjectId::from(Blob::new("absent").id());
        assert_eq!(store.resolve_blob(&absent).unwrap(), None);
        assert_eq!(store.resolve_tree(&absent).unwrap(), None);
        assert_eq!(store.resolve_commit(&absent).unwrap(), None);
    }

    #[test]
    fn resolve_reports_a_damaged_object_for_every_kind_and_does_not_hide_it_as_absent() {
        let dir = TempDir::new();
        let store = dir.store();
        let stored = ObjectId::from(store.put_blob(&Blob::new("hello")).unwrap());
        overwrite(&store.object_path(&stored), b"blob 5\0other");
        let is_corrupt = |error: StoreError| matches!(error, StoreError::Corrupt { .. });
        // The content is checked before the kind is compared, so the damage is reported even when
        // the caller asks for another kind than the object has.
        assert!(is_corrupt(store.resolve_blob(&stored).unwrap_err()));
        assert!(is_corrupt(store.resolve_tree(&stored).unwrap_err()));
        assert!(is_corrupt(store.resolve_commit(&stored).unwrap_err()));
    }

    #[test]
    fn missing_object_is_none_and_not_contained() {
        let dir = TempDir::new();
        let store = dir.store();
        let absent = Blob::new("absent").id();
        assert!(store.get(&ObjectId::from(absent)).unwrap().is_none());
        assert!(!store.contains(&ObjectId::from(absent)).unwrap());
        store.put_blob(&Blob::new("absent")).unwrap();
        assert!(store.contains(&ObjectId::from(absent)).unwrap());
    }

    // On-disk format

    #[test]
    fn the_layout_and_the_file_content_are_pinned_by_literals() {
        let dir = TempDir::new();
        let store = dir.store();
        store.put_blob(&Blob::new("hello")).unwrap();
        let path = dir
            .path()
            .join("objects")
            .join("b3")
            .join("9d")
            .join("b8039d1b509628ad5b125ea89780b69fcde5c272215d690a2e4dc69eac75ad");
        assert_eq!(fs::read(path).unwrap(), b"blob 5\0hello");

        let tree = Tree::new(Vec::new()).unwrap();
        store.put_tree(&tree).unwrap();
        let path = dir
            .path()
            .join("objects/b3/06/b8469ad46d7051ad63f82809ef66c6d738d08cc3f1a5c842f3d66c05f64cf7");
        assert_eq!(fs::read(path).unwrap(), b"tree 0\0");
    }

    #[test]
    fn a_commit_file_is_the_header_and_the_encoding() {
        let dir = TempDir::new();
        let store = dir.store();
        let author = Actor::human("daniel").unwrap();
        let commit = Commit::new(
            TreeId::new(id(EMPTY_TREE_ID)),
            vec![],
            sample_change(),
            author,
            1_700_000_000,
            "m",
        )
        .unwrap();
        let stored = store.put_commit(&commit).unwrap();
        let body = format!(
            "tree {EMPTY_TREE_ID}\nchange ch:00112233445566778899aabbccddeeff\nauthor human daniel\ntime 1700000000\n\nm"
        );
        let content = format!("commit {}\0{body}", body.len());
        // The ID is the hash of the file content, computed here without the code under test.
        assert_eq!(stored, CommitId::new(id_of(content.as_bytes())));
        let hex = blake3::hash(content.as_bytes()).to_hex().to_string();
        let path = dir
            .path()
            .join("objects")
            .join("b3")
            .join(&hex[..2])
            .join(&hex[2..]);
        assert_eq!(fs::read(path).unwrap(), content.as_bytes());
    }

    #[test]
    fn stored_objects_are_read_only() {
        let dir = TempDir::new();
        let store = dir.store();
        let stored = store.put_blob(&Blob::new("fixed")).unwrap();
        let metadata = fs::metadata(store.object_path(&ObjectId::from(stored))).unwrap();
        assert!(metadata.permissions().readonly());
    }

    // Verified reads

    #[test]
    fn every_kind_of_damage_gets_its_own_reason() {
        let hello = Blob::new("hello").id();
        let cases: Vec<(&[u8], CorruptReason)> = vec![
            // The body differs, or the end of it.
            (
                b"blob 5\0jello",
                CorruptReason::HashMismatch {
                    actual: id_of(b"blob 5\0jello"),
                },
            ),
            (
                b"blob 5\0hellp",
                CorruptReason::HashMismatch {
                    actual: id_of(b"blob 5\0hellp"),
                },
            ),
            // The kind differs from the one the ID was made with.
            (
                b"tree 5\0hello",
                CorruptReason::HashMismatch {
                    actual: id_of(b"tree 5\0hello"),
                },
            ),
            // A valid object stored under the ID of another one.
            (
                b"blob 3\0abc",
                CorruptReason::HashMismatch {
                    actual: Blob::new("abc").id().object_id(),
                },
            ),
            // The length in the header.
            (
                b"blob 5\0hel",
                CorruptReason::HeaderMismatch {
                    found: "blob 5".into(),
                    expected: "blob 3".into(),
                },
            ),
            (
                b"blob 5\0hello!",
                CorruptReason::HeaderMismatch {
                    found: "blob 5".into(),
                    expected: "blob 6".into(),
                },
            ),
            (
                b"blob 6\0hello",
                CorruptReason::HeaderMismatch {
                    found: "blob 6".into(),
                    expected: "blob 5".into(),
                },
            ),
            (
                b"blob 05\0hello",
                CorruptReason::HeaderMismatch {
                    found: "blob 05".into(),
                    expected: "blob 5".into(),
                },
            ),
            (
                b"blob +5\0hello",
                CorruptReason::HeaderMismatch {
                    found: "blob +5".into(),
                    expected: "blob 5".into(),
                },
            ),
            (
                b"blob -5\0hello",
                CorruptReason::HeaderMismatch {
                    found: "blob -5".into(),
                    expected: "blob 5".into(),
                },
            ),
            (
                b"blob  5\0hello",
                CorruptReason::HeaderMismatch {
                    found: "blob  5".into(),
                    expected: "blob 5".into(),
                },
            ),
            (
                b"blob 5 \0hello",
                CorruptReason::HeaderMismatch {
                    found: "blob 5 ".into(),
                    expected: "blob 5".into(),
                },
            ),
            (
                b"blob 99999999999999999999\0hello",
                CorruptReason::HeaderMismatch {
                    found: "blob 99999999999999999999".into(),
                    expected: "blob 5".into(),
                },
            ),
            // The kind in the header.
            (b"blub 5\0hello", CorruptReason::UnknownKind("blub".into())),
            (b"bloc 5\0hello", CorruptReason::UnknownKind("bloc".into())),
            (b"Blob 5\0hello", CorruptReason::UnknownKind("Blob".into())),
            // The shape of the header.
            (b"blob5\0hello", CorruptReason::HeaderShape),
            (b"\0hello", CorruptReason::HeaderShape),
            (b"bl\xffb 5\0hello", CorruptReason::HeaderNotUtf8),
            // The header is cut off, or the file is empty.
            (b"blob 5", CorruptReason::NoHeaderEnd),
            (b"blob ", CorruptReason::NoHeaderEnd),
            (b"blo", CorruptReason::NoHeaderEnd),
            (b"", CorruptReason::NoHeaderEnd),
        ];
        for (bytes, expected) in cases {
            let dir = TempDir::new();
            let store = dir.store();
            store.put_blob(&Blob::new("hello")).unwrap();
            overwrite(&store.object_path(&ObjectId::from(hello)), bytes);
            assert_eq!(
                reason(store.get(&ObjectId::from(hello))),
                expected,
                "{bytes:?}"
            );
        }
    }

    #[test]
    fn contains_does_not_check_the_content() {
        let dir = TempDir::new();
        let store = dir.store();
        let stored = store.put_blob(&Blob::new("hello")).unwrap();
        overwrite(&store.object_path(&ObjectId::from(stored)), b"");
        assert!(store.contains(&ObjectId::from(stored)).unwrap());
        assert!(store.get(&ObjectId::from(stored)).is_err());
    }

    #[test]
    fn a_directory_at_the_object_path_is_corrupt_and_not_contained() {
        let dir = TempDir::new();
        let store = dir.store();
        let stored = Blob::new("hello").id();
        fs::create_dir_all(store.object_path(&ObjectId::from(stored))).unwrap();
        assert_eq!(
            reason(store.get(&ObjectId::from(stored))),
            CorruptReason::NotARegularFile
        );
        assert!(!store.contains(&ObjectId::from(stored)).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_at_the_object_path_is_not_followed() {
        let dir = TempDir::new();
        let store = dir.store();
        let other = store.put_blob(&Blob::new("other")).unwrap();
        let stored = Blob::new("hello").id();
        let path = store.object_path(&ObjectId::from(stored));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(store.object_path(&ObjectId::from(other)), &path).unwrap();
        assert_eq!(
            reason(store.get(&ObjectId::from(stored))),
            CorruptReason::NotARegularFile
        );
        assert!(!store.contains(&ObjectId::from(stored)).unwrap());
        assert!(matches!(
            store.put_blob(&Blob::new("hello")),
            Err(StoreError::Corrupt {
                reason: CorruptReason::NotARegularFile,
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_file_system_is_an_io_error_and_not_a_missing_object() {
        let dir = TempDir::new();
        let store = dir.store();
        // A file where a directory should be: the path below it cannot be looked up.
        fs::write(dir.path().join("objects"), b"not a directory").unwrap();
        let stored = Blob::new("hello").id();
        let error = store.get(&ObjectId::from(stored)).unwrap_err();
        assert!(matches!(error, StoreError::Io { .. }), "{error:?}");
        assert!(store.contains(&ObjectId::from(stored)).is_err());
    }

    // The store directory

    #[test]
    fn a_missing_root_is_an_error_and_is_not_created() {
        let dir = TempDir::new();
        let root = dir.path().join("typo");
        let store = FsObjectStore::new(&root);
        let stored = Blob::new("hello").id();
        for error in [
            store.get(&ObjectId::from(stored)).unwrap_err(),
            store.contains(&ObjectId::from(stored)).unwrap_err(),
            store.put_blob(&Blob::new("hello")).unwrap_err(),
        ] {
            assert!(
                matches!(&error, StoreError::Io { operation, .. }
                    if *operation == "open the object store directory"),
                "{error:?}"
            );
        }
        assert!(!root.exists(), "the store must not create its root");
    }

    #[test]
    fn a_root_that_is_a_file_is_an_error() {
        let dir = TempDir::new();
        let root = dir.path().join("file");
        fs::write(&root, b"x").unwrap();
        let store = FsObjectStore::new(&root);
        assert!(store.put_blob(&Blob::new("hello")).is_err());
        assert!(store.get(&ObjectId::from(Blob::new("hello").id())).is_err());
    }

    // Writing

    #[test]
    fn storing_twice_is_idempotent_and_leaves_no_temporary_files() {
        let dir = TempDir::new();
        let store = dir.store();
        let first = store.put_blob(&Blob::new("same")).unwrap();
        let second = store.put_blob(&Blob::new("same")).unwrap();
        assert_eq!(first, second);
        assert_eq!(temp_entries(&dir), 0);
    }

    #[cfg(unix)]
    #[test]
    fn an_intact_object_is_not_rewritten() {
        use std::os::unix::fs::MetadataExt;
        let dir = TempDir::new();
        let store = dir.store();
        let stored = store.put_blob(&Blob::new("same")).unwrap();
        let before = fs::metadata(store.object_path(&ObjectId::from(stored)))
            .unwrap()
            .ino();
        store.put_blob(&Blob::new("same")).unwrap();
        let after = fs::metadata(store.object_path(&ObjectId::from(stored)))
            .unwrap()
            .ino();
        assert_eq!(before, after, "the file was replaced");
    }

    #[cfg(unix)]
    #[test]
    fn storing_an_object_again_replaces_a_damaged_file() {
        for damage in [&b""[..], b"blob 5\0jello", b"blob 5\0hel", b"garbage"] {
            let dir = TempDir::new();
            let store = dir.store();
            let stored = store.put_blob(&Blob::new("hello")).unwrap();
            overwrite(&store.object_path(&ObjectId::from(stored)), damage);
            assert!(store.get(&ObjectId::from(stored)).is_err());
            assert_eq!(store.put_blob(&Blob::new("hello")).unwrap(), stored);
            assert_eq!(
                store.get(&ObjectId::from(stored)).unwrap().unwrap().body(),
                b"hello"
            );
            let metadata = fs::metadata(store.object_path(&ObjectId::from(stored))).unwrap();
            assert!(metadata.permissions().readonly());
            assert_eq!(temp_entries(&dir), 0);
        }
    }

    // Batch

    #[test]
    fn a_batch_stores_the_objects_that_single_writes_store() {
        let single = TempDir::new();
        let batched = TempDir::new();
        let entry = |name: &str, id: BlobId| TreeEntry::file(name, FileMode::File, id).unwrap();
        let objects = |store: &dyn ObjectStore| {
            let one = store.put_blob(&Blob::new("one")).unwrap();
            let two = store.put_blob(&Blob::new("two")).unwrap();
            let again = store.put_blob(&Blob::new("one")).unwrap();
            assert_eq!(one, again);
            let tree = store
                .put_tree(&Tree::new(vec![entry("a", one), entry("b", two)]).unwrap())
                .unwrap();
            vec![one.object_id(), two.object_id(), tree.object_id()]
        };
        let expected = objects(&single.store());
        let store = batched.store();
        let batch = store.batch();
        assert_eq!(objects(&batch), expected);
        for id in &expected {
            assert!(
                !store.contains(id).unwrap(),
                "nothing is in place before the finish"
            );
            assert!(batch.contains(id).unwrap());
            assert!(batch.get(id).unwrap().is_some());
        }
        assert_eq!(temp_entries(&batched), 3, "one file for each object");
        batch.finish().unwrap();
        for id in &expected {
            let path = store.object_path(id);
            assert_eq!(
                fs::read(&path).unwrap(),
                fs::read(single.store().object_path(id)).unwrap()
            );
            assert!(fs::metadata(&path).unwrap().permissions().readonly());
        }
        assert_eq!(temp_entries(&batched), 0);
    }

    #[test]
    fn a_batch_writes_no_object_that_the_store_has() {
        let dir = TempDir::new();
        let store = dir.store();
        let stored = store.put_blob(&Blob::new("hello")).unwrap();
        let batch = store.batch();
        assert_eq!(batch.put_blob(&Blob::new("hello")).unwrap(), stored);
        assert_eq!(temp_entries(&dir), 0);
        batch.finish().unwrap();
        assert_eq!(
            store.get(&stored.object_id()).unwrap().unwrap().body(),
            b"hello"
        );
    }

    #[test]
    fn a_batch_that_is_dropped_leaves_no_object_and_removes_its_files() {
        let dir = TempDir::new();
        let store = dir.store();
        let batch = store.batch();
        let id = batch.put_blob(&Blob::new("hello")).unwrap();
        assert_eq!(temp_entries(&dir), 1);
        drop(batch);
        assert!(!store.contains(&id.object_id()).unwrap());
        assert_eq!(temp_entries(&dir), 0);
    }

    #[test]
    fn a_batch_replaces_a_damaged_object_where_a_single_write_does() {
        let dir = TempDir::new();
        let store = dir.store();
        let stored = store.put_blob(&Blob::new("hello")).unwrap();
        overwrite(&store.object_path(&stored.object_id()), b"garbage");
        let batch = store.batch();
        let result = batch.put_blob(&Blob::new("hello"));
        if REPLACE_DAMAGED {
            assert_eq!(result.unwrap(), stored);
            assert_eq!(
                batch.get(&stored.object_id()).unwrap().unwrap().body(),
                b"hello",
                "the batch reads its own copy"
            );
            batch.finish().unwrap();
            assert_eq!(
                store.get(&stored.object_id()).unwrap().unwrap().body(),
                b"hello"
            );
        } else {
            assert!(matches!(result, Err(StoreError::Corrupt { .. })));
        }
    }

    #[test]
    fn a_batch_that_cannot_move_an_object_names_it_and_removes_its_file() {
        let dir = TempDir::new();
        let store = dir.store();
        let blocked = Blob::new("blocked").id();
        let blocked_path = store.object_path(&blocked.object_id());
        let batch = store.batch();
        batch.put_blob(&Blob::new("blocked")).unwrap();
        // A directory takes the path of the object after it was written to the batch.
        fs::create_dir_all(blocked_path.join("inner")).unwrap();
        let error = batch.finish().unwrap_err();
        assert!(
            matches!(&error, StoreError::Io { path, .. } if *path == blocked_path),
            "{error:?}"
        );
        assert_eq!(temp_entries(&dir), 0, "the file of the object is removed");
    }

    #[test]
    fn a_directory_at_the_object_path_blocks_storing_and_stays() {
        let dir = TempDir::new();
        let store = dir.store();
        let stored = Blob::new("hello").id();
        fs::create_dir_all(store.object_path(&ObjectId::from(stored))).unwrap();
        let error = store.put_blob(&Blob::new("hello")).unwrap_err();
        assert!(
            matches!(
                error,
                StoreError::Corrupt {
                    reason: CorruptReason::NotARegularFile,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(store.object_path(&ObjectId::from(stored)).is_dir());
    }

    #[test]
    fn a_failed_write_reports_the_path_and_leaves_no_temporary_file() {
        let dir = TempDir::new();
        let store = dir.store();
        // A file where the fan-out directory should be.
        let stored = Blob::new("hello").id();
        let fan_out = store
            .object_path(&ObjectId::from(stored))
            .parent()
            .unwrap()
            .to_path_buf();
        fs::create_dir_all(fan_out.parent().unwrap()).unwrap();
        fs::write(&fan_out, b"in the way").unwrap();
        let error = store.put_blob(&Blob::new("hello")).unwrap_err();
        assert!(matches!(error, StoreError::Io { .. }), "{error:?}");
        assert!(error.to_string().contains("objects"), "{error}");
        // The message has the message of the I/O error, which has no cause of its own here.
        assert!(error.source().is_none());
        assert!(!store.object_path(&ObjectId::from(stored)).exists());
        assert_eq!(temp_entries(&dir), 0);
    }

    #[test]
    fn a_temporary_directory_that_is_a_file_is_an_error() {
        let dir = TempDir::new();
        let store = dir.store();
        fs::write(dir.path().join("tmp"), b"in the way").unwrap();
        let error = store.put_blob(&Blob::new("hello")).unwrap_err();
        assert!(matches!(error, StoreError::Io { .. }), "{error:?}");
        assert!(!store
            .contains(&ObjectId::from(Blob::new("hello").id()))
            .unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_rename_is_reported_with_its_error_and_the_temporary_file_is_removed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let store = dir.store();
        let stored = Blob::new("hello").id();
        let fan_out = store
            .object_path(&ObjectId::from(stored))
            .parent()
            .unwrap()
            .to_path_buf();
        fs::create_dir_all(&fan_out).unwrap();
        fs::set_permissions(&fan_out, fs::Permissions::from_mode(0o555)).unwrap();
        // A process with the right to write anyway (root) cannot provoke the failure.
        let writable = fs::write(fan_out.join("probe"), b"x").is_ok();
        let result = store.put_blob(&Blob::new("hello"));
        fs::set_permissions(&fan_out, fs::Permissions::from_mode(0o755)).unwrap();
        if writable {
            return;
        }
        match result {
            Err(StoreError::Io {
                operation, source, ..
            }) => {
                assert_eq!(operation, "move the object into place at");
                assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected an I/O error, got {other:?}"),
        }
        assert_eq!(temp_entries(&dir), 0);
        assert!(!store.contains(&ObjectId::from(stored)).unwrap());
    }

    #[test]
    fn a_taken_temporary_name_is_skipped_and_the_other_file_is_left_alone() {
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join("tmp")).unwrap();
        let taken = dir.path().join("tmp").join("taken");
        fs::write(&taken, b"belongs to another writer").unwrap();
        let mut names = ["taken", "taken", "free"].into_iter().map(String::from);
        let (_file, temp) =
            create_temp(&dir.path().join("tmp"), &mut || names.next().unwrap()).unwrap();
        assert_eq!(temp.path.file_name().unwrap(), "free");
        assert_eq!(fs::read(&taken).unwrap(), b"belongs to another writer");
    }

    #[test]
    fn running_out_of_temporary_names_is_an_error_and_removes_nothing() {
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join("tmp")).unwrap();
        let taken = dir.path().join("tmp").join("taken");
        fs::write(&taken, b"belongs to another writer").unwrap();
        let result = create_temp(&dir.path().join("tmp"), &mut || "taken".to_owned());
        match result {
            Err(StoreError::Io { source, .. }) => {
                assert_eq!(source.kind(), io::ErrorKind::AlreadyExists);
            }
            Err(error) => panic!("expected an I/O error, got {error:?}"),
            Ok(_) => panic!("expected an error"),
        }
        assert_eq!(fs::read(&taken).unwrap(), b"belongs to another writer");
    }

    #[test]
    fn a_temporary_file_is_removed_on_drop_unless_it_was_moved() {
        let dir = TempDir::new();
        let kept = dir.path().join("kept");
        let removed = dir.path().join("removed");
        fs::write(&kept, b"x").unwrap();
        fs::write(&removed, b"x").unwrap();
        drop(TempFile {
            path: kept.clone(),
            armed: false,
        });
        drop(TempFile {
            path: removed.clone(),
            armed: true,
        });
        assert!(kept.exists());
        assert!(!removed.exists());
    }

    #[test]
    fn temporary_names_are_unique() {
        let names: std::collections::HashSet<String> =
            (0..1000).map(|_| unique_temp_name()).collect();
        assert_eq!(names.len(), 1000);
    }

    #[test]
    fn writers_that_race_on_one_object_all_succeed_and_leave_one_intact_file() {
        let dir = TempDir::new();
        let store = dir.store();
        let expected = Blob::new("raced").id();
        let barrier = Barrier::new(8);
        thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        store.put_blob(&Blob::new("raced"))
                    })
                })
                .collect();
            for handle in handles {
                assert_eq!(handle.join().unwrap().unwrap(), expected);
            }
        });
        assert_eq!(
            store
                .get(&ObjectId::from(expected))
                .unwrap()
                .unwrap()
                .body(),
            b"raced"
        );
        assert_eq!(temp_entries(&dir), 0);
    }

    #[test]
    fn writers_of_different_objects_do_not_disturb_each_other() {
        let dir = TempDir::new();
        let store = dir.store();
        thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|n| {
                    let store = &store;
                    scope.spawn(move || {
                        let blob = Blob::new(format!("object {n}"));
                        (blob.id(), store.put_blob(&blob).unwrap())
                    })
                })
                .collect();
            for handle in handles {
                let (expected, stored) = handle.join().unwrap();
                assert_eq!(stored, expected);
            }
        });
        for n in 0..16 {
            let blob = Blob::new(format!("object {n}"));
            let object = store.get(&ObjectId::from(blob.id())).unwrap().unwrap();
            assert_eq!(object.body(), blob.content());
        }
        assert_eq!(temp_entries(&dir), 0);
    }

    #[test]
    fn a_reader_sees_no_object_or_a_complete_one_while_writers_run() {
        let dir = TempDir::new();
        let store = dir.store();
        let blob = Blob::new("x".repeat(100_000));
        let expected = blob.id();
        let done = std::sync::atomic::AtomicBool::new(false);
        thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut seen = 0;
                while !done.load(Ordering::Relaxed) {
                    match store.get(&ObjectId::from(expected)) {
                        Ok(None) => {}
                        Ok(Some(object)) => {
                            assert_eq!(object.body(), blob.content());
                            seen += 1;
                        }
                        Err(error) => panic!("a reader saw a partial object: {error}"),
                    }
                }
                seen
            });
            let writers: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| store.put_blob(&blob).unwrap()))
                .collect();
            for writer in writers {
                assert_eq!(writer.join().unwrap(), expected);
            }
            done.store(true, Ordering::Relaxed);
            reader.join().unwrap();
        });
        assert!(store.get(&ObjectId::from(expected)).unwrap().is_some());
    }

    // Errors

    #[test]
    fn errors_name_the_operation_the_path_and_the_object() {
        let dir = TempDir::new();
        let store = dir.store();
        let stored = store.put_blob(&Blob::new("hello")).unwrap();
        let path = store.object_path(&ObjectId::from(stored));
        overwrite(&path, b"blob 5\0jello");
        let error = store.get(&ObjectId::from(stored)).unwrap_err();
        let text = error.to_string();
        assert!(text.contains(&stored.to_string()), "{text}");
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(text.contains("hashes to"), "{text}");
        assert!(error.source().is_none());

        let io = StoreError::Io {
            operation: "read the object file",
            path: PathBuf::from("a/b"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        };
        let text = io.to_string();
        assert!(
            text.contains("read the object file") && text.contains("a/b"),
            "{text}"
        );
        // The message has the message of the cause, and the source is the cause of the cause.
        assert!(text.ends_with(": denied"), "{text}");
        assert!(io.source().is_none());
    }

    #[test]
    fn corrupt_reasons_are_readable() {
        let shown = [
            (CorruptReason::NotARegularFile, "not a regular file"),
            (CorruptReason::NoHeaderEnd, "NUL"),
            (CorruptReason::HeaderNotUtf8, "UTF-8"),
            (CorruptReason::HeaderShape, "<kind> <length>"),
            (CorruptReason::UnknownKind("a\nb".into()), "a\\nb"),
            (
                CorruptReason::HeaderMismatch {
                    found: "blob 05".into(),
                    expected: "blob 5".into(),
                },
                "`blob 05`, but the content requires `blob 5`",
            ),
        ];
        for (reason, fragment) in shown {
            let text = reason.to_string();
            assert!(text.contains(fragment), "{text}");
        }
    }

    // ---- get_tree and get_commit

    fn author() -> crate::Actor {
        crate::Actor::human("daniel").unwrap()
    }

    #[test]
    fn get_tree_returns_the_stored_tree_and_none_for_an_absent_one() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = Tree::new(vec![
            TreeEntry::file("a", FileMode::File, BlobId::new(id(HELLO_ID))).unwrap(),
            TreeEntry::file("b", FileMode::Symlink, BlobId::new(id(HELLO_ID))).unwrap(),
        ])
        .unwrap();
        let stored = store.put_tree(&tree).unwrap();
        assert_eq!(store.get_tree(&stored).unwrap(), Some(tree));
        let absent = Tree::new(vec![]).unwrap().id();
        assert_eq!(store.get_tree(&absent).unwrap(), None);
    }

    #[test]
    fn get_commit_returns_the_stored_commit_and_none_for_an_absent_one() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = store.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
        let parent = Commit::new(tree, vec![], sample_change(), author(), 1, "first").unwrap();
        let parent_id = store.put_commit(&parent).unwrap();
        let commit = Commit::new(
            tree,
            vec![parent_id],
            sample_change(),
            author(),
            2,
            "second\n\nbody",
        )
        .unwrap();
        let stored = store.put_commit(&commit).unwrap();
        assert_eq!(store.get_commit(&stored).unwrap(), Some(commit.clone()));
        let absent = Commit::new(tree, vec![stored], sample_change(), author(), 3, "x")
            .unwrap()
            .id();
        assert_eq!(store.get_commit(&absent).unwrap(), None);
    }

    #[test]
    fn get_tree_and_get_commit_report_an_object_of_another_kind() {
        let dir = TempDir::new();
        let store = dir.store();
        let blob = store.put_blob(&Blob::new("hello")).unwrap();
        let tree = store.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
        let blob_as_tree = TreeId::new(ObjectId::from(blob));
        let tree_as_commit = CommitId::new(ObjectId::from(tree));
        match store.get_tree(&blob_as_tree).unwrap_err() {
            ReadError::Store(source) => assert!(
                matches!(
                    *source,
                    StoreError::KindMismatch {
                        expected: Kind::Tree,
                        found: Kind::Blob,
                        ..
                    }
                ),
                "{source}"
            ),
            other => panic!("unexpected error: {other}"),
        }
        match store.get_commit(&tree_as_commit).unwrap_err() {
            ReadError::Store(source) => assert!(
                matches!(
                    *source,
                    StoreError::KindMismatch {
                        expected: Kind::Commit,
                        found: Kind::Tree,
                        ..
                    }
                ),
                "{source}"
            ),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn get_tree_and_get_commit_report_a_damaged_object_as_a_store_error() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = store.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
        let commit = store
            .put_commit(&Commit::new(tree, vec![], sample_change(), author(), 1, "m").unwrap())
            .unwrap();
        overwrite(&store.object_path(&ObjectId::from(tree)), b"tree 1\0x");
        overwrite(&store.object_path(&ObjectId::from(commit)), b"commit 1\0x");
        match store.get_tree(&tree).unwrap_err() {
            ReadError::Store(source) => {
                assert!(matches!(*source, StoreError::Corrupt { .. }), "{source}")
            }
            other => panic!("unexpected error: {other}"),
        }
        match store.get_commit(&commit).unwrap_err() {
            ReadError::Store(source) => {
                assert!(matches!(*source, StoreError::Corrupt { .. }), "{source}")
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_non_canonical_body_is_reported_with_the_id_and_the_reason() {
        let dir = TempDir::new();
        let store = dir.store();
        // The content hashes to its ID, so the store accepts it; only the body is not canonical.
        let tree = TreeId::new(store.put_raw(Kind::Tree, b"100644 a\nnot-an-id\n").unwrap());
        let commit = CommitId::new(store.put_raw(Kind::Commit, b"tree x\n").unwrap());
        match store.get_tree(&tree).unwrap_err() {
            ReadError::Tree { id, source } => {
                assert_eq!(id, tree);
                assert!(
                    matches!(source, TreeParseError::InvalidObjectId { .. }),
                    "{source}"
                );
            }
            other => panic!("unexpected error: {other}"),
        }
        match store.get_commit(&commit).unwrap_err() {
            ReadError::Commit { id, source } => {
                assert_eq!(id, commit);
                assert!(
                    matches!(source, CommitParseError::InvalidObjectId { .. }),
                    "{source}"
                );
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn read_errors_say_what_failed_and_carry_their_cause() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = TreeId::new(store.put_raw(Kind::Tree, b"100644 a\nnot-an-id\n").unwrap());
        let error = store.get_tree(&tree).unwrap_err();
        let text = error.to_string();
        assert!(
            text.starts_with(&format!("tree {tree} is not canonical: ")),
            "{text}"
        );
        assert!(error.source().is_none());
        let blob = store.put_blob(&Blob::new("hello")).unwrap();
        let error = store
            .get_tree(&TreeId::new(ObjectId::from(blob)))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("object {blob} is a blob, not a tree")
        );
        assert!(error.source().is_none());
    }

    fn hello_blob() -> BlobId {
        BlobId::new(id(HELLO_ID))
    }

    #[test]
    fn the_roundtrip_covers_every_mode_the_empty_tree_and_many_parents() {
        let dir = TempDir::new();
        let store = dir.store();
        let empty = Tree::new(vec![]).unwrap();
        let empty_id = store.put_tree(&empty).unwrap();
        assert_eq!(store.get_tree(&empty_id).unwrap(), Some(empty));
        let tree = Tree::new(vec![
            TreeEntry::file("exec", FileMode::Executable, hello_blob()).unwrap(),
            TreeEntry::file("file", FileMode::File, hello_blob()).unwrap(),
            TreeEntry::file("link", FileMode::Symlink, hello_blob()).unwrap(),
            TreeEntry::directory("sub", empty_id).unwrap(),
        ])
        .unwrap();
        let tree_id = store.put_tree(&tree).unwrap();
        assert_eq!(store.get_tree(&tree_id).unwrap(), Some(tree));
        let mut parents = Vec::new();
        for n in 0..5 {
            let parent = Commit::new(
                empty_id,
                vec![],
                sample_change(),
                author(),
                n,
                format!("p{n}"),
            )
            .unwrap();
            parents.push(store.put_commit(&parent).unwrap());
        }
        let commit = Commit::new(tree_id, parents, sample_change(), author(), 9, "merge").unwrap();
        let commit_id = store.put_commit(&commit).unwrap();
        assert_eq!(store.get_commit(&commit_id).unwrap(), Some(commit));
    }

    #[test]
    fn each_kind_of_non_canonical_body_keeps_its_own_reason() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree_body = |entries: &[(&str, &str)]| {
            entries
                .iter()
                .map(|(name, id)| format!("100644 {name}\n{id}\n"))
                .collect::<String>()
        };
        let unsorted = tree_body(&[("b", HELLO_ID), ("a", HELLO_ID)]);
        let duplicate = tree_body(&[("a", HELLO_ID), ("a", HELLO_ID)]);
        let cut = tree_body(&[("a", HELLO_ID)]).trim_end().to_owned();
        let tree_of = |body: &str| TreeId::new(store.put_raw(Kind::Tree, body.as_bytes()).unwrap());
        assert!(matches!(
            store.get_tree(&tree_of(&unsorted)),
            Err(ReadError::Tree {
                source: TreeParseError::OutOfOrder { .. },
                ..
            })
        ));
        assert!(matches!(
            store.get_tree(&tree_of(&duplicate)),
            Err(ReadError::Tree {
                source: TreeParseError::OutOfOrder { .. },
                ..
            })
        ));
        assert!(matches!(
            store.get_tree(&tree_of(&cut)),
            Err(ReadError::Tree {
                source: TreeParseError::Malformed { .. },
                ..
            })
        ));
        let commit_of =
            |body: &str| CommitId::new(store.put_raw(Kind::Commit, body.as_bytes()).unwrap());
        let tree = Tree::new(vec![]).unwrap().id();
        let no_author = format!("tree {tree}\n");
        let not_utf8 = commit_of_bytes(&store, b"tree \xff\n");
        assert!(matches!(
            store.get_commit(&commit_of(&no_author)),
            Err(ReadError::Commit {
                source: CommitParseError::Malformed { .. },
                ..
            })
        ));
        assert!(matches!(
            store.get_commit(&not_utf8),
            Err(ReadError::Commit {
                source: CommitParseError::NotUtf8 { .. },
                ..
            })
        ));
    }

    fn commit_of_bytes(store: &FsObjectStore, body: &[u8]) -> CommitId {
        CommitId::new(store.put_raw(Kind::Commit, body).unwrap())
    }

    #[test]
    fn a_commit_that_is_not_canonical_has_a_message_and_a_cause() {
        let dir = TempDir::new();
        let store = dir.store();
        let commit = commit_of_bytes(&store, b"tree x\n");
        let error = store.get_commit(&commit).unwrap_err();
        let text = error.to_string();
        assert!(
            text.starts_with(&format!("commit {commit} is not canonical: ")),
            "{text}"
        );
        let ReadError::Commit { source, .. } = &error else {
            panic!("unexpected error: {error}");
        };
        assert!(text.ends_with(&source.to_string()), "{text}");
        assert!(error.source().is_none());
    }

    #[test]
    fn a_storage_failure_shows_through_the_transparent_store_variant() {
        use crate::testutil::{FaultyStore, DISK_MESSAGE};
        let dir = TempDir::new();
        let tree = dir.store().put_tree(&Tree::new(vec![]).unwrap()).unwrap();
        // The store fails the same way on every system (see `FaultyStore`).
        let store = FaultyStore::new(dir.store(), true, false);
        let error = store.get_tree(&tree).unwrap_err();
        let ReadError::Store(inner) = &error else {
            panic!("unexpected error: {error}");
        };
        assert!(matches!(**inner, StoreError::Io { .. }), "{inner}");
        // The message is that of the store error, including the one of the I/O error.
        assert_eq!(
            error.to_string(),
            format!("could not read the object file `objects/b3/00/faulty`: {DISK_MESSAGE}")
        );
        // The chain goes on below the I/O error, whose message the text has.
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("the deepest cause")
        );
        // The typed error is reached by the accessors.
        let io = error.store_error().and_then(StoreError::io_error).unwrap();
        assert_eq!(io.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn a_value_with_another_id_than_requested_is_reported_in_every_build() {
        let a = id(HELLO_ID);
        let b = Tree::new(vec![]).unwrap().id().object_id();
        assert!(confirm_id(a, a).is_ok());
        let error = confirm_id(a, b).unwrap_err();
        assert!(
            matches!(&error, ReadError::IdMismatch { requested, parsed } if *requested == a && *parsed == b)
        );
        assert_eq!(
            error.to_string(),
            format!(
                "object {a} was read as a value with the ID {b}: \
                 the reader accepted a body that is not canonical"
            )
        );
        assert!(error.source().is_none());
    }
}
