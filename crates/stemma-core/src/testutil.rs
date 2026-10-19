//! Helpers shared by the unit tests of the crate.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::store::FsObjectStore;

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
