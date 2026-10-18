//! Object storage: the [`ObjectStore`] trait and a file system implementation (ADR-007).
//!
//! A store keeps objects by ID. It does not interpret them: it stores the object kind and the
//! encoded body, and gives both back. It does not decode trees or commits.
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

use std::collections::hash_map::RandomState;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::object::{hash_encoding, Blob, BlobId, Commit, CommitId, Kind, ObjectId, Tree, TreeId};

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
    /// The underlying storage failed. The cause is available through [`Error::source`].
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
                operation, path, ..
            } => write!(f, "could not {operation} `{}`", path.display()),
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

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Corrupt { .. } | Self::KindMismatch { .. } => None,
        }
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

mod sealed {
    /// Keeps [`ObjectStore`](super::ObjectStore) from being implemented outside this crate.
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
    fn object_path(&self, id: &ObjectId) -> PathBuf {
        let hex = id.hex();
        self.root
            .join("objects")
            .join(id.algorithm())
            .join(&hex[..2])
            .join(&hex[2..])
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
        let path = self.object_path(&id);
        self.require_root()?;
        match self.get(&id) {
            Ok(Some(_)) => return Ok(id),
            Ok(None) => {}
            Err(StoreError::Corrupt { ref reason, .. })
                if REPLACE_DAMAGED && *reason != CorruptReason::NotARegularFile => {}
            Err(error) => return Err(error),
        }

        let mut content = kind.header(body.len()).into_bytes();
        content.extend_from_slice(body);
        self.publish(&id, &path, &content)?;
        Ok(id)
    }

    /// Writes `content` to a temporary file, flushes it, makes it read-only, and renames it to
    /// `path`.
    ///
    /// If the rename fails but another writer has stored the same object meanwhile, the call
    /// succeeds, because the content is the same by construction. Any other failure is reported
    /// with the original error. On Unix a rename replaces a file that is already there, so two
    /// writers that race leave one file with identical content.
    fn publish(&self, id: &ObjectId, path: &Path, content: &[u8]) -> Result<(), StoreError> {
        let temp_dir = self.root.join("tmp");
        let fan_out = path.parent().ok_or_else(|| StoreError::Io {
            operation: "find the directory of the object",
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "the path has no parent"),
        })?;
        create_dirs_synced(&self.root, &temp_dir)?;
        create_dirs_synced(&self.root, fan_out)?;

        let (mut file, mut temp) = create_temp(&temp_dir, &mut unique_temp_name)?;
        file.write_all(content)
            .map_err(io_error("write the temporary file", &temp.path))?;
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

        match fs::rename(&temp.path, path) {
            Ok(()) => temp.armed = false,
            Err(error) => {
                // Another writer may have stored the object in the meantime. Only an object that
                // reads back correctly counts; the temporary file is removed on drop.
                return match self.get(id) {
                    Ok(Some(_)) => Ok(()),
                    _ => Err(StoreError::Io {
                        operation: "move the object into place at",
                        path: path.to_path_buf(),
                        source: error,
                    }),
                };
            }
        }
        sync_directory(fan_out)
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
        let path = self.object_path(id);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.require_root()?;
                return Ok(None);
            }
            Err(error) => return Err(io_error("inspect the object file", &path)(error)),
        };
        let corrupt = |reason| StoreError::Corrupt {
            id: *id,
            path: path.clone(),
            reason,
        };
        if !metadata.is_file() {
            return Err(corrupt(CorruptReason::NotARegularFile));
        }
        let mut bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            // The object was removed after the check above.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("read the object file", &path)(error)),
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

/// A temporary file that is removed on drop unless it was moved into place. The guard exists only
/// for a file that this call created, so it can never remove the file of another writer.
struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            // Best effort: a file that stays behind is harmless, because temporary names are
            // never reused.
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Returns a temporary name that is unique in this process and, with overwhelming probability,
/// across processes: the process ID, a counter, and a random number from the per-process random
/// state of the standard library. It is not meant to be unguessable.
fn unique_temp_name() -> String {
    let random = RandomState::new().build_hasher().finish();
    format!(
        "{}-{}-{random:016x}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Creates a new file in `dir` under a name from `next_name`. If the name is taken, it asks for
/// the next one, up to [`TEMP_ATTEMPTS`] times. A file that already exists is never touched.
fn create_temp(
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

/// Creates `dir` and every missing directory above it, and flushes the new directory entries.
/// `root` has to exist and `dir` has to be below it.
fn create_dirs_synced(root: &Path, dir: &Path) -> Result<(), StoreError> {
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
#[cfg(unix)]
fn sync_directory(dir: &Path) -> Result<(), StoreError> {
    File::open(dir)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error("flush the directory", dir))
}

/// No-op on platforms that cannot flush a directory: the rename is atomic but not made durable.
#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;
    use crate::object::{FileMode, TreeEntry};
    use crate::Actor;

    const HELLO_ID: &str = "b3:9db8039d1b509628ad5b125ea89780b69fcde5c272215d690a2e4dc69eac75ad";
    const EMPTY_TREE_ID: &str =
        "b3:06b8469ad46d7051ad63f82809ef66c6d738d08cc3f1a5c842f3d66c05f64cf7";

    /// A directory below the system temp directory that is removed on drop. A directory left by
    /// an earlier run that crashed is cleared first.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "stemma-store-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn store(&self) -> FsObjectStore {
            FsObjectStore::new(self.path())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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

    /// Replaces a read-only object file with other content.
    fn overwrite(path: &Path, bytes: &[u8]) {
        fs::remove_file(path).expect("remove object");
        fs::write(path, bytes).expect("write object");
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

    #[test]
    fn the_trait_is_object_safe_and_the_store_can_be_shared() {
        fn takes_a_trait_object(_: &dyn ObjectStore) {}
        fn is_send_and_sync<T: Send + Sync>() {}
        let dir = TempDir::new();
        takes_a_trait_object(&dir.store());
        is_send_and_sync::<FsObjectStore>();
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
        let author = Actor::Human {
            id: "daniel".into(),
        };
        let commit = Commit::new(
            TreeId::new(id(EMPTY_TREE_ID)),
            vec![],
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
        let author = Actor::Human {
            id: "daniel".into(),
        };
        let commit = Commit::new(
            TreeId::new(id(EMPTY_TREE_ID)),
            vec![],
            author,
            1_700_000_000,
            "m",
        )
        .unwrap();
        let stored = store.put_commit(&commit).unwrap();
        let body = format!("tree {EMPTY_TREE_ID}\nauthor human daniel\ntime 1700000000\n\nm");
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
        assert!(error.source().is_some());
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
        // The cause is the source and is not repeated in the message.
        assert!(!text.contains("denied"), "{text}");
        assert_eq!(io.source().unwrap().to_string(), "denied");
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
}
