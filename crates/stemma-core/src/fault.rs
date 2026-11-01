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
//! - `deny_writes_in(dir)`: every call that adds, moves, or removes an existing name directly in
//!   `dir` fails. It stands for a directory with the mode `0o555`, so removing a name that is not
//!   there reports `NotFound`, as the file system does. The directory itself and the names below
//!   a subdirectory are not covered, except by `remove_dir_all`, which removes the tree name by
//!   name in the order of the names and stops at the first denied one, as the file system does.
//! - `deny_modifying_in(dir)`: opening an existing file directly in `dir` for writing fails. It
//!   stands for files with the mode `0o444`; adding or removing a name there still works.
//! - `deny_reads_of(path)`: opening `path` or listing it fails. It matches that one path; a read
//!   of a name below it still works, which the mode `0o000` of a directory would stop.
//!
//! A path in a rule is compared as it is and, if that fails, as the file system names it
//! (`fs::canonicalize`), so `/var/...` and `/private/var/...` on macOS meet.
//!
//! In a unit test, a guard arms the injection for the thread until it is dropped. A test of the
//! program as a whole starts a process that no guard reaches; for it, the feature
//! `fault-injection` reads the variable `STEMMA_FAULT` once, as entries `write=<dir>`,
//! `modify=<dir>`, or `read=<path>`, separated by the path-list separator of the platform (`:` on Unix, `;` on
//! Windows). An entry that does not parse stops the process with a message: a harness that does
//! not inject must not look like one that does. A build without the feature and outside of tests
//! has neither the functions of injection nor the variable: the wrappers are the calls of the
//! standard library.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

#[cfg(any(test, feature = "fault-injection"))]
mod armed {
    use std::cell::RefCell;
    use std::ffi::OsStr;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    /// The name of the variable that arms injections for a whole process.
    pub const ENV: &str = "STEMMA_FAULT";

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum Rule {
        Write(PathBuf),
        Read(PathBuf),
        Modify(PathBuf),
    }

    thread_local! {
        /// The rules of the guards of this thread, each with the number of its guard.
        static RULES: RefCell<Vec<(u64, Rule)>> = const { RefCell::new(Vec::new()) };
    }

    /// The path as the file system names it, or as given if it does not exist (yet).
    fn named(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// Reads the rules of the variable `STEMMA_FAULT`: entries `write=<dir>`, `modify=<dir>`, and
    /// `read=<path>`, separated like the entries of `PATH`.
    pub(super) fn parse(value: &OsStr) -> Result<Vec<Rule>, String> {
        std::env::split_paths(value)
            .filter(|item| !item.as_os_str().is_empty())
            .map(|item| {
                let text = item
                    .to_str()
                    .ok_or_else(|| format!("{item:?} is not UTF-8"))?;
                let (kind, path) = text.split_once('=').ok_or_else(|| {
                    format!(
                        "`{text}` has no `=`; use `write=<dir>`, `modify=<dir>`, or `read=<path>`"
                    )
                })?;
                match kind {
                    "write" => Ok(Rule::Write(named(Path::new(path)))),
                    "modify" => Ok(Rule::Modify(named(Path::new(path)))),
                    "read" => Ok(Rule::Read(named(Path::new(path)))),
                    other => Err(format!(
                        "`{other}` is none of `write`, `modify`, and `read`"
                    )),
                }
            })
            .collect()
    }

    fn from_env() -> &'static [Rule] {
        static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
        RULES.get_or_init(|| match std::env::var_os(ENV) {
            None => Vec::new(),
            Some(value) => parse(&value).unwrap_or_else(|message| panic!("{ENV}: {message}")),
        })
    }

    fn denied() -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, "injected failure")
    }

    /// Whether a rule matches `actual`, where `wanted` says how a rule names its path.
    fn any(wanted: impl Fn(&Rule) -> Option<&Path>, actual: &Path) -> bool {
        let same = |rule: &Path| rule == actual || named(actual) == rule;
        RULES.with(|rules| {
            rules
                .borrow()
                .iter()
                .any(|(_, rule)| wanted(rule).is_some_and(same))
        }) || from_env().iter().any(|rule| wanted(rule).is_some_and(same))
    }

    fn write_dir(rule: &Rule) -> Option<&Path> {
        match rule {
            Rule::Write(dir) => Some(dir),
            Rule::Read(_) | Rule::Modify(_) => None,
        }
    }

    fn modify_dir(rule: &Rule) -> Option<&Path> {
        match rule {
            Rule::Modify(dir) => Some(dir),
            Rule::Read(_) | Rule::Write(_) => None,
        }
    }

    fn read_path(rule: &Rule) -> Option<&Path> {
        match rule {
            Rule::Read(path) => Some(path),
            Rule::Write(_) | Rule::Modify(_) => None,
        }
    }

    /// Whether any injection is armed for this thread or the process.
    pub fn active() -> bool {
        RULES.with(|rules| !rules.borrow().is_empty()) || !from_env().is_empty()
    }

    pub fn check_write(path: &Path) -> io::Result<()> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        if any(write_dir, parent) {
            return Err(denied());
        }
        Ok(())
    }

    /// [`check_write`] for the removal of a name: a name that is not there is `NotFound` for the
    /// real call, also in a directory that refuses changes.
    pub fn check_removal(path: &Path) -> io::Result<()> {
        match check_write(path) {
            Err(_)
                if fs::symlink_metadata(path)
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                Ok(())
            }
            checked => checked,
        }
    }

    /// For opening the existing file `path` for writing.
    pub fn check_modify(path: &Path) -> io::Result<()> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        if any(modify_dir, parent) {
            return Err(denied());
        }
        Ok(())
    }

    pub fn check_read(path: &Path) -> io::Result<()> {
        if any(read_path, path) {
            return Err(denied());
        }
        Ok(())
    }

    /// Ends the injection that created it when it is dropped, also when a test panics, and in
    /// any order of several guards. It belongs to the thread that armed it.
    #[cfg(test)]
    #[must_use = "the injection ends when the guard is dropped"]
    pub struct Guard {
        number: u64,
        /// A guard is not sent to another thread: its rule lives in this one.
        _thread: std::marker::PhantomData<*const ()>,
    }

    #[cfg(test)]
    fn arm(rule: Rule) -> Guard {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let number = NEXT.fetch_add(1, Ordering::Relaxed);
        RULES.with(|rules| rules.borrow_mut().push((number, rule)));
        Guard {
            number,
            _thread: std::marker::PhantomData,
        }
    }

    #[cfg(test)]
    impl Drop for Guard {
        fn drop(&mut self) {
            RULES.with(|rules| {
                rules
                    .borrow_mut()
                    .retain(|(number, _)| *number != self.number)
            });
        }
    }

    /// Makes every call that adds, moves, or removes an existing name directly in `dir` fail with
    /// `PermissionDenied` on this thread, until the guard is dropped.
    #[cfg(test)]
    pub fn deny_writes_in(dir: &Path) -> Guard {
        arm(Rule::Write(named(dir)))
    }

    /// Makes opening an existing file directly in `dir` for writing fail with `PermissionDenied`
    /// on this thread, until the guard is dropped.
    #[cfg(test)]
    pub fn deny_modifying_in(dir: &Path) -> Guard {
        arm(Rule::Modify(named(dir)))
    }

    /// Makes opening and listing `path` fail with `PermissionDenied` on this thread, until the
    /// guard is dropped.
    #[cfg(test)]
    pub fn deny_reads_of(path: &Path) -> Guard {
        arm(Rule::Read(named(path)))
    }
}

#[cfg(test)]
pub(crate) use armed::{deny_modifying_in, deny_reads_of, deny_writes_in};

#[cfg(any(test, feature = "fault-injection"))]
use armed::{active, check_modify, check_read, check_removal, check_write};

#[cfg(not(any(test, feature = "fault-injection")))]
fn check_write(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(any(test, feature = "fault-injection")))]
fn check_removal(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(any(test, feature = "fault-injection")))]
fn check_modify(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(any(test, feature = "fault-injection")))]
fn active() -> bool {
    false
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

/// `std::os::unix::fs::symlink`, failing if the injection denies writes in the directory of
/// `link`.
#[cfg(unix)]
pub(crate) fn symlink(original: &std::ffi::OsStr, link: &Path) -> io::Result<()> {
    check_write(link)?;
    std::os::unix::fs::symlink(original, link)
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
/// reading only, as the lock does. Fails if the injection denies reads of `path`; a file that is
/// not there is created only if the injection does not deny writes in its directory.
pub(crate) fn open_or_create(path: &Path) -> io::Result<File> {
    check_read(path)?;
    let allowed = if path.exists() {
        Ok(())
    } else {
        check_write(path)
    };
    allowed
        .and_then(|()| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
        })
        .or_else(|_| File::open(path))
}

/// Opens the existing file at `path` for writing without changing it, as the flush of a temporary
/// file does, failing if the injection denies modifying files in its directory.
pub(crate) fn open_for_write(path: &Path) -> io::Result<File> {
    check_modify(path)?;
    OpenOptions::new().write(true).open(path)
}

/// [`fs::remove_file`], failing if the injection denies writes in the directory of `path` and the
/// name is there.
pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    check_removal(path)?;
    fs::remove_file(path)
}

/// [`fs::remove_dir`], failing if the injection denies writes in the parent of `path` and the
/// name is there.
pub(crate) fn remove_dir(path: &Path) -> io::Result<()> {
    check_removal(path)?;
    fs::remove_dir(path)
}

/// [`fs::remove_dir_all`], failing if the injection denies writes in the parent of `path` or of
/// any name below it. With an injection armed, the tree goes name by name, in the order of the
/// names, and stops at the first denied one: what came before stays removed, as with the real
/// call.
pub(crate) fn remove_dir_all(path: &Path) -> io::Result<()> {
    check_removal(path)?;
    if active() {
        remove_tree(path)
    } else {
        fs::remove_dir_all(path)
    }
}

/// Removes the content of `path` and then `path`, checking every name before it goes.
#[cfg(any(test, feature = "fault-injection"))]
fn remove_tree(path: &Path) -> io::Result<()> {
    let mut children = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
    children.sort_by_key(fs::DirEntry::file_name);
    for entry in children {
        let child = entry.path();
        if entry.file_type()?.is_dir() {
            remove_tree(&child)?;
        } else {
            check_write(&child)?;
            fs::remove_file(&child)?;
        }
    }
    fs::remove_dir(path)
}

#[cfg(not(any(test, feature = "fault-injection")))]
fn remove_tree(_: &Path) -> io::Result<()> {
    unreachable!("no injection is armed")
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
    use super::armed::{parse, Rule};
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

    #[test]
    fn guards_end_their_own_rule_in_any_order() {
        let dir = TempDir::new();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let outer = deny_writes_in(&first);
        let inner = deny_writes_in(&second);
        drop(outer);
        assert!(create_new(&first.join("a")).is_ok());
        assert!(
            create_new(&second.join("a")).is_err(),
            "the inner rule stays"
        );
        drop(inner);
        assert!(create_new(&second.join("a")).is_ok());
    }

    #[test]
    fn removing_a_name_that_is_not_there_is_not_found_in_a_denied_directory() {
        let dir = TempDir::new();
        fs::write(dir.path().join("there"), b"").unwrap();
        let _guard = deny_writes_in(dir.path());
        let missing = remove_file(&dir.path().join("missing")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        let present = remove_file(&dir.path().join("there")).unwrap_err();
        assert_eq!(present.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_missing_guard_file_is_not_created_in_a_denied_directory() {
        let dir = TempDir::new();
        let guard = dir.path().join("guard");
        let _denied = deny_writes_in(dir.path());
        let error = open_or_create(&guard).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!guard.exists());
    }

    #[test]
    fn a_denied_tree_is_removed_as_far_as_the_file_system_would_and_a_missing_one_is_not_found() {
        let dir = TempDir::new();
        let tree = dir.path().join("tree");
        let inner = tree.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(tree.join("a"), b"").unwrap();
        fs::write(tree.join("z"), b"").unwrap();
        fs::write(inner.join("stuck"), b"").unwrap();
        let guard = deny_writes_in(&inner);
        let error = remove_dir_all(&tree).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            !tree.join("a").exists(),
            "the names before the denied one went"
        );
        assert!(inner.join("stuck").exists());
        assert!(tree.join("z").exists(), "the names after it stay");
        drop(guard);
        remove_dir_all(&tree).unwrap();
        assert!(!tree.exists());
        let missing = remove_dir_all(&dir.path().join("gone")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_denied_modification_refuses_opening_an_existing_file_for_writing_only() {
        let dir = TempDir::new();
        let file = dir.path().join("file");
        fs::write(&file, b"kept").unwrap();
        {
            let _guard = deny_modifying_in(dir.path());
            let error = open_for_write(&file).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            create_new(&dir.path().join("new")).unwrap();
            remove_file(&file).unwrap();
        }
        fs::write(&file, b"kept").unwrap();
        assert!(open_for_write(&file).is_ok());
    }

    #[test]
    fn the_rules_of_the_variable_are_read_with_the_separator_of_the_platform() {
        let value = std::env::join_paths(["write=/a/b", "read=/c", "modify=/d"]).unwrap();
        assert_eq!(
            parse(&value).unwrap(),
            [
                Rule::Write("/a/b".into()),
                Rule::Read("/c".into()),
                Rule::Modify("/d".into())
            ]
        );
        assert_eq!(parse("".as_ref()).unwrap(), []);
    }

    #[test]
    fn a_rule_of_the_variable_that_does_not_parse_is_an_error_and_not_no_rule() {
        for bad in ["write:/a", "writes=/a", "Write=/a", "/a"] {
            assert!(parse(bad.as_ref()).is_err(), "{bad}");
        }
    }
}
