//! Object storage: the [`ObjectStore`] trait and a file system implementation (ADR-007).
//!
//! A store keeps objects by ID. It does not interpret them: it stores the object kind and the
//! encoded body, and gives both back. Readers for trees and commits come later.
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
//! does not match its ID, so corruption is detected instead of passed on.

use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::object::{hash_encoding, Blob, Commit, Kind, ObjectId, Tree};

/// Reasons why a store operation fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum StoreError {
    /// The underlying storage failed.
    Io(io::Error),
    /// A stored object is damaged: it is not in canonical form or does not match its ID.
    Corrupt {
        /// The ID the object was stored under.
        id: ObjectId,
        /// What is wrong with it.
        reason: &'static str,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "object store I/O error: {error}"),
            Self::Corrupt { id, reason } => write!(f, "object {id} is corrupt: {reason}"),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Corrupt { .. } => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// An object as stored: its kind and its encoded body.
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

/// Storage for objects, addressed by their ID.
///
/// Storing is idempotent: the same kind and body always give the same ID, and storing an
/// object that exists already changes nothing. An implementation must not overwrite an object
/// and must not expose a partly written one.
pub trait ObjectStore {
    /// Stores an object and returns its ID.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if the storage fails.
    fn put_raw(&self, kind: Kind, body: &[u8]) -> Result<ObjectId, StoreError>;

    /// Returns the object with the ID, or `None` if the store does not have it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Corrupt`] if the stored object is damaged, and
    /// [`StoreError::Io`] if the storage fails.
    fn get(&self, id: &ObjectId) -> Result<Option<RawObject>, StoreError>;

    /// Returns whether the store has the object. It does not check the content.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if the storage fails.
    fn contains(&self, id: &ObjectId) -> Result<bool, StoreError>;

    /// Stores a blob. The returned ID equals [`Blob::id`].
    ///
    /// # Errors
    ///
    /// See [`put_raw`](Self::put_raw).
    fn put_blob(&self, blob: &Blob) -> Result<ObjectId, StoreError> {
        self.put_raw(Kind::Blob, blob.content())
    }

    /// Stores a tree. The returned ID equals [`Tree::id`].
    ///
    /// # Errors
    ///
    /// See [`put_raw`](Self::put_raw).
    fn put_tree(&self, tree: &Tree) -> Result<ObjectId, StoreError> {
        self.put_raw(Kind::Tree, &tree.encode())
    }

    /// Stores a commit. The returned ID equals [`Commit::id`].
    ///
    /// # Errors
    ///
    /// See [`put_raw`](Self::put_raw).
    fn put_commit(&self, commit: &Commit) -> Result<ObjectId, StoreError> {
        self.put_raw(Kind::Commit, &commit.encode())
    }
}

/// Counter that keeps temporary file names unique within a process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Loose objects in a directory tree (ADR-007).
///
/// An object is written to a temporary file below `<root>/tmp/`, flushed to disk, made
/// read-only, and then renamed to its final path. A reader therefore sees either no object or
/// a complete one. The store creates the directories it needs, so `root` itself must exist.
#[derive(Debug, Clone)]
pub struct FsObjectStore {
    root: PathBuf,
}

impl FsObjectStore {
    /// Creates a store that keeps its files below `root`, the repository's metadata directory.
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
}

impl ObjectStore for FsObjectStore {
    fn put_raw(&self, kind: Kind, body: &[u8]) -> Result<ObjectId, StoreError> {
        let id = hash_encoding(kind, body);
        let path = self.object_path(&id);
        if path.try_exists()? {
            return Ok(id);
        }

        let mut content = kind.header(body.len()).into_bytes();
        content.extend_from_slice(body);

        let temp_dir = self.root.join("tmp");
        fs::create_dir_all(&temp_dir)?;
        let temp_path = temp_dir.join(format!(
            "{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = publish(&temp_path, &path, &content);
        if result.is_err() {
            // The temporary file may not exist; there is nothing more to do about it.
            let _ = fs::remove_file(&temp_path);
        }
        result?;
        Ok(id)
    }

    fn get(&self, id: &ObjectId) -> Result<Option<RawObject>, StoreError> {
        let bytes = match fs::read(self.object_path(id)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let corrupt = |reason: &'static str| StoreError::Corrupt { id: *id, reason };

        let nul = bytes
            .iter()
            .position(|&byte| byte == 0)
            .ok_or_else(|| corrupt("the header is not terminated by a NUL byte"))?;
        let header = std::str::from_utf8(&bytes[..nul])
            .map_err(|_| corrupt("the header is not valid UTF-8"))?;
        let (kind_name, length) = header
            .split_once(' ')
            .ok_or_else(|| corrupt("the header is not `<kind> <length>`"))?;
        let kind = Kind::from_name(kind_name).ok_or_else(|| corrupt("unknown object kind"))?;
        let body = &bytes[nul + 1..];
        // Compare as text so that a length such as `05` or `+5` is rejected as well.
        if length != body.len().to_string() {
            return Err(corrupt("the length in the header does not match the body"));
        }
        if hash_encoding(kind, body) != *id {
            return Err(corrupt("the content does not match the object ID"));
        }
        Ok(Some(RawObject {
            kind,
            body: body.to_vec(),
        }))
    }

    fn contains(&self, id: &ObjectId) -> Result<bool, StoreError> {
        Ok(self.object_path(id).try_exists()?)
    }
}

/// Writes `content` to `temp_path`, flushes it, makes it read-only, and renames it to `dest`.
///
/// If another writer created `dest` in the meantime, the content is the same by construction,
/// so the temporary file is dropped and the call succeeds.
fn publish(temp_path: &Path, dest: &Path, content: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)?;
    file.write_all(content)?;
    file.sync_all()?;
    drop(file);

    let mut permissions = fs::metadata(temp_path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(temp_path, permissions)?;

    let parent = dest
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "object path has no parent"))?;
    fs::create_dir_all(parent)?;
    if let Err(error) = fs::rename(temp_path, dest) {
        if dest.try_exists()? {
            return fs::remove_file(temp_path);
        }
        return Err(error);
    }

    // Flush the directory entry, and the entry of the fan-out directory in its parent.
    sync_directory(parent)?;
    if let Some(grandparent) = parent.parent() {
        sync_directory(grandparent)?;
    }
    Ok(())
}

/// Flushes a directory so that a rename or a new entry in it survives a crash.
#[cfg(unix)]
fn sync_directory(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Directories cannot be opened for flushing on this platform; the rename is left as is.
#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory below the system temp directory that is removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "stemma-store-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Replaces a read-only object file with other content.
    fn overwrite(path: &Path, bytes: &[u8]) {
        fs::remove_file(path).expect("remove object");
        fs::write(path, bytes).expect("write object");
    }

    #[test]
    fn put_blob_returns_the_blob_id_and_get_returns_the_content() {
        let dir = TempDir::new();
        let store = FsObjectStore::new(dir.path());
        let blob = Blob::new("hello");
        let id = store.put_blob(&blob).unwrap();
        assert_eq!(id, blob.id());
        let object = store.get(&id).unwrap().expect("object exists");
        assert_eq!(object.kind(), Kind::Blob);
        assert_eq!(object.body(), b"hello");
        assert_eq!(object.into_body(), b"hello");
    }

    #[test]
    fn file_layout_and_content_are_pinned() {
        let dir = TempDir::new();
        let store = FsObjectStore::new(dir.path());
        let id = store.put_blob(&Blob::new("hello")).unwrap();
        let text = id.to_string();
        let hex = text.strip_prefix("b3:").expect("b3 prefix");
        let path = dir
            .path()
            .join("objects")
            .join("b3")
            .join(&hex[..2])
            .join(&hex[2..]);
        assert_eq!(fs::read(path).unwrap(), b"blob 5\0hello");
    }

    #[test]
    fn storing_twice_is_idempotent_and_leaves_no_temporary_files() {
        let dir = TempDir::new();
        let store = FsObjectStore::new(dir.path());
        let first = store.put_blob(&Blob::new("same")).unwrap();
        let second = store.put_blob(&Blob::new("same")).unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read_dir(dir.path().join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn stored_objects_are_read_only() {
        let dir = TempDir::new();
        let store = FsObjectStore::new(dir.path());
        let id = store.put_blob(&Blob::new("fixed")).unwrap();
        let metadata = fs::metadata(store.object_path(&id)).unwrap();
        assert!(metadata.permissions().readonly());
    }

    #[test]
    fn missing_object_is_none_and_not_contained() {
        let dir = TempDir::new();
        let store = FsObjectStore::new(dir.path());
        let id = Blob::new("absent").id();
        assert!(store.get(&id).unwrap().is_none());
        assert!(!store.contains(&id).unwrap());
        store.put_blob(&Blob::new("absent")).unwrap();
        assert!(store.contains(&id).unwrap());
    }

    #[test]
    fn tree_is_stored_under_its_own_id() {
        let dir = TempDir::new();
        let store = FsObjectStore::new(dir.path());
        let tree = Tree::new(Vec::new()).unwrap();
        let id = store.put_tree(&tree).unwrap();
        assert_eq!(id, tree.id());
        let object = store.get(&id).unwrap().unwrap();
        assert_eq!(object.kind(), Kind::Tree);
        assert_eq!(object.body(), tree.encode());
    }

    #[test]
    fn the_kind_is_part_of_the_identity() {
        let dir = TempDir::new();
        let store = FsObjectStore::new(dir.path());
        let kinds = [Kind::Blob, Kind::Tree, Kind::Commit];
        let ids: Vec<ObjectId> = kinds
            .iter()
            .map(|&kind| store.put_raw(kind, b"same body").unwrap())
            .collect();
        for (i, id) in ids.iter().enumerate() {
            for other in &ids[i + 1..] {
                assert_ne!(id, other);
            }
            let object = store.get(id).unwrap().unwrap();
            assert_eq!(object.kind(), kinds[i]);
            assert_eq!(object.body(), b"same body");
        }
    }

    #[test]
    fn empty_and_binary_bodies_roundtrip() {
        let dir = TempDir::new();
        let store = FsObjectStore::new(dir.path());
        for body in [&b""[..], &b"a\0b\xff\n\0"[..]] {
            let id = store.put_raw(Kind::Blob, body).unwrap();
            assert_eq!(store.get(&id).unwrap().unwrap().body(), body);
        }
    }

    #[test]
    fn damaged_objects_are_reported_as_corrupt() {
        let damaged: [&[u8]; 8] = [
            b"blob 5\0jello",  // body differs from the ID
            b"blob 5\0hel",    // truncated
            b"blob 5\0hello!", // too long
            b"blob 5",         // no NUL byte
            b"blub 5\0hello",  // unknown kind
            b"tree 5\0hello",  // kind differs from the ID
            b"blob 05\0hello", // length not canonical
            b"",               // empty file
        ];
        for bytes in damaged {
            let dir = TempDir::new();
            let store = FsObjectStore::new(dir.path());
            let id = store.put_blob(&Blob::new("hello")).unwrap();
            overwrite(&store.object_path(&id), bytes);
            let result = store.get(&id);
            assert!(
                matches!(result, Err(StoreError::Corrupt { .. })),
                "{bytes:?}: {result:?}"
            );
        }
    }

    #[test]
    fn corrupt_error_names_the_object() {
        let dir = TempDir::new();
        let store = FsObjectStore::new(dir.path());
        let id = store.put_blob(&Blob::new("hello")).unwrap();
        overwrite(&store.object_path(&id), b"blob 5\0jello");
        let message = store.get(&id).unwrap_err().to_string();
        assert!(message.contains(&id.to_string()), "{message}");
    }

    #[test]
    fn io_errors_are_not_mistaken_for_missing_objects() {
        let dir = TempDir::new();
        let store = FsObjectStore::new(dir.path());
        let id = Blob::new("hello").id();
        // A directory where the object file should be: reading it fails, but it is not missing.
        fs::create_dir_all(store.object_path(&id)).unwrap();
        assert!(matches!(store.get(&id), Err(StoreError::Io(_))));
    }
}
