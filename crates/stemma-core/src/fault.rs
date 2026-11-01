//! Injected file system failures (STEMMA-187).
//!
//! A test that wants a write or a read to fail used to remove the permissions of a directory. That
//! works on Unix only, it does nothing for a privileged user, and a Windows runner skipped it. The
//! file system calls of the store, the references, and the log go through the functions of this
//! module instead. Without an injection they are the calls of the standard library; with one, they
//! fail with `PermissionDenied` for the paths that the injection names.
//!
//! There are two kinds of injection:
//!
//! - `deny_writes_in(dir)`: every call that adds, moves, or removes a name directly in `dir`
//!   fails. It stands for a directory with the mode `0o555`.
//! - `deny_reads_of(path)`: opening `path` or listing it fails. It stands for the mode `0o000`.
//!
//! In a unit test, a guard arms the injection for the thread until it is dropped. A test of the
//! program as a whole starts a process that no guard reaches; for it, the feature
//! `fault-injection` reads the variable [`ENV`] once, as `write:<dir>` or `read:<path>`, several
//! separated by the path separator of the platform. A build without the feature and outside of
//! tests has neither the functions of injection nor the variable: the wrappers are the calls
//! of the standard library.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

#[cfg(any(test, feature = "fault-injection"))]
mod armed {
    use std::cell::RefCell;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    /// The name of the variable that arms injections for a whole process.
    pub const ENV: &str = "STEMMA_FAULT";

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Rule {
        Write(PathBuf),
        Read(PathBuf),
    }

    thread_local! {
        static RULES: RefCell<Vec<Rule>> = const { RefCell::new(Vec::new()) };
    }

    fn from_env() -> &'static [Rule] {
        static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
        RULES.get_or_init(|| {
            let Some(value) = std::env::var_os(ENV) else {
                return Vec::new();
            };
            std::env::split_paths(&value)
                .filter_map(|item| {
                    let item = item.to_str()?.to_owned();
                    let (kind, path) = item.split_once(':')?;
                    match kind {
                        "write" => Some(Rule::Write(PathBuf::from(path))),
                        "read" => Some(Rule::Read(PathBuf::from(path))),
                        _ => None,
                    }
                })
                .collect()
        })
    }

    fn denied() -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, "injected failure")
    }

    fn any(matches: impl Fn(&Rule) -> bool) -> bool {
        RULES.with(|rules| rules.borrow().iter().any(&matches)) || from_env().iter().any(matches)
    }

    pub fn check_write(path: &Path) -> io::Result<()> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        if any(|rule| matches!(rule, Rule::Write(dir) if dir == parent)) {
            return Err(denied());
        }
        Ok(())
    }

    pub fn check_read(path: &Path) -> io::Result<()> {
        if any(|rule| matches!(rule, Rule::Read(denied) if denied == path)) {
            return Err(denied());
        }
        Ok(())
    }

    /// Ends the injection that created it when it is dropped, also when a test panics.
    #[cfg(test)]
    #[must_use = "the injection ends when the guard is dropped"]
    pub struct Guard(usize);

    #[cfg(test)]
    fn arm(rule: Rule) -> Guard {
        RULES.with(|rules| {
            let mut rules = rules.borrow_mut();
            rules.push(rule);
            Guard(rules.len() - 1)
        })
    }

    #[cfg(test)]
    impl Drop for Guard {
        fn drop(&mut self) {
            RULES.with(|rules| rules.borrow_mut().truncate(self.0));
        }
    }

    /// Makes every call that adds, moves, or removes a name directly in `dir` fail with
    /// `PermissionDenied` on this thread. Guards must be dropped in reverse order.
    #[cfg(test)]
    pub fn deny_writes_in(dir: &Path) -> Guard {
        arm(Rule::Write(dir.to_path_buf()))
    }

    /// Makes opening and listing `path` fail with `PermissionDenied` on this thread. Guards must
    /// be dropped in reverse order.
    #[cfg(test)]
    pub fn deny_reads_of(path: &Path) -> Guard {
        arm(Rule::Read(path.to_path_buf()))
    }
}

#[cfg(test)]
pub(crate) use armed::{deny_reads_of, deny_writes_in};

#[cfg(any(test, feature = "fault-injection"))]
use armed::{check_read, check_write};

#[cfg(not(any(test, feature = "fault-injection")))]
fn check_write(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(any(test, feature = "fault-injection")))]
fn check_read(_: &Path) -> io::Result<()> {
    Ok(())
}

/// [`fs::rename`], failing if the injection denies writes in the directory of `to` or of `from`.
pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
    check_write(from)?;
    check_write(to)?;
    fs::rename(from, to)
}

/// [`fs::hard_link`], failing if the injection denies writes in the directory of `to`.
pub(crate) fn hard_link(from: &Path, to: &Path) -> io::Result<()> {
    check_write(to)?;
    fs::hard_link(from, to)
}

/// [`fs::read`], failing if the injection denies reads of `path`.
pub(crate) fn read(path: &Path) -> io::Result<Vec<u8>> {
    check_read(path)?;
    fs::read(path)
}

/// [`File::open`], failing if the injection denies reads of `path`.
pub(crate) fn open(path: &Path) -> io::Result<File> {
    check_read(path)?;
    File::open(path)
}

/// Opens the guard file at `path` for reading and writing, creating it, and falls back to
/// reading only, as the lock does. Fails if the injection denies reads of `path`.
pub(crate) fn open_or_create(path: &Path) -> io::Result<File> {
    check_read(path)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .or_else(|_| File::open(path))
}

/// [`fs::remove_file`], failing if the injection denies writes in the directory of `path`.
pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    check_write(path)?;
    fs::remove_file(path)
}

/// [`fs::create_dir`], failing if the injection denies writes in the parent of `path`.
pub(crate) fn create_dir(path: &Path) -> io::Result<()> {
    check_write(path)?;
    fs::create_dir(path)
}

/// Opens `path` for writing as a new file, as `OpenOptions::new().write(true).create_new(true)`
/// does, failing if the injection denies writes in its directory.
pub(crate) fn create_new(path: &Path) -> io::Result<File> {
    check_write(path)?;
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// [`fs::read_dir`], failing if the injection denies reads of `path`.
pub(crate) fn read_dir(path: &Path) -> io::Result<fs::ReadDir> {
    check_read(path)?;
    fs::read_dir(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn a_denied_directory_refuses_a_new_name_and_others_stay_open() {
        let dir = TempDir::new();
        let sealed = dir.path().join("sealed");
        let open = dir.path().join("open");
        fs::create_dir(&sealed).unwrap();
        fs::create_dir(&open).unwrap();
        {
            let _guard = deny_writes_in(&sealed);
            let error = create_new(&sealed.join("a")).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(!sealed.join("a").exists());
            create_new(&open.join("a")).unwrap();
            let error = rename(&open.join("a"), &sealed.join("b")).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
        create_new(&sealed.join("a")).unwrap();
    }

    #[test]
    fn a_denied_read_refuses_the_listing_and_ends_with_the_guard() {
        let dir = TempDir::new();
        {
            let _guard = deny_reads_of(dir.path());
            let error = read_dir(dir.path()).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
        assert!(read_dir(dir.path()).is_ok());
    }
}
