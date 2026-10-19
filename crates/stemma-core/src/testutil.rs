//! Helpers shared by the unit tests of the crate.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::object::{Blob, BlobId, Commit, CommitId, ObjectId, Tree, TreeId};
use crate::store::{sealed, FsObjectStore, ObjectStore, RawObject, StoreError};

/// A directory below the system temp directory that is removed on drop. A directory left by an
/// earlier run that crashed is cleared first.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "stemma-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn store(&self) -> FsObjectStore {
        FsObjectStore::new(self.path())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Replaces a read-only object file with other content.
pub(crate) fn overwrite(path: &Path, bytes: &[u8]) {
    fs::remove_file(path).expect("remove object");
    fs::write(path, bytes).expect("write object");
}

/// The deepest link of the chain that [`FaultyStore`] makes: it is the cause of [`DiskError`].
#[derive(Debug)]
pub(crate) struct DeepCause;

impl fmt::Display for DeepCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the deepest cause")
    }
}

impl Error for DeepCause {}

/// An error of the file system with a cause of its own. The `io::Error` that holds it prints
/// this message, and its `source` is [`DeepCause`], so that a chain has a link below the one
/// that the message of a wrapper prints.
#[derive(Debug)]
pub(crate) struct DiskError;

impl fmt::Display for DiskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the disk failed")
    }
}

impl Error for DiskError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&DeepCause)
    }
}

/// The message that [`FaultyStore`] errors end in.
pub(crate) const DISK_MESSAGE: &str = "the disk failed";

/// A store that fails the same way on every system, which a real file system does not: the same
/// failure of the file system has another error kind on Windows than on Unix. Reading fails when
/// `fail_get` is set and writing a tree when `fail_put` is set; everything else goes to a real
/// store, so a test can build the state that it needs first.
pub(crate) struct FaultyStore {
    pub(crate) inner: FsObjectStore,
    pub(crate) fail_get: bool,
    pub(crate) fail_put: bool,
}

impl FaultyStore {
    pub(crate) fn new(inner: FsObjectStore, fail_get: bool, fail_put: bool) -> Self {
        Self {
            inner,
            fail_get,
            fail_put,
        }
    }

    fn failure() -> StoreError {
        StoreError::Io {
            operation: "read the object file",
            path: PathBuf::from("objects/b3/00/faulty"),
            source: io::Error::other(DiskError),
        }
    }
}

impl sealed::Sealed for FaultyStore {}

impl ObjectStore for FaultyStore {
    fn put_blob(&self, blob: &Blob) -> Result<BlobId, StoreError> {
        self.inner.put_blob(blob)
    }

    fn put_tree(&self, tree: &Tree) -> Result<TreeId, StoreError> {
        if self.fail_put {
            Err(Self::failure())
        } else {
            self.inner.put_tree(tree)
        }
    }

    fn put_commit(&self, commit: &Commit) -> Result<CommitId, StoreError> {
        self.inner.put_commit(commit)
    }

    fn get(&self, id: &ObjectId) -> Result<Option<RawObject>, StoreError> {
        if self.fail_get {
            Err(Self::failure())
        } else {
            self.inner.get(id)
        }
    }

    fn contains(&self, id: &ObjectId) -> Result<bool, StoreError> {
        self.inner.contains(id)
    }
}
