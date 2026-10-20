//! Tracking files (ADR-010).
//!
//! A file that is not in the tree of the working-copy commit is untracked, and a snapshot does not
//! save it. [`Repository::track`] confirms files: it saves them in the next working-copy commit
//! together with the files that were tracked already, so a track never drops a change to a tracked
//! file.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::snapshot::{lookup, Tracked};
use super::{Repository, SnapshotError};
use crate::index::path_problem;
use crate::object::{name_problem, FileMode, METADATA_DIR};
use crate::oplog::{Entry, OperationName};
use crate::{Actor, Authorizer};

/// The operation that the log records for tracking files.
const OPERATION: &str = "track";

/// What [`Repository::track`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracking {
    /// The entry of the move of the working-copy commit, or `None` if the tree is the same.
    pub entry: Option<Entry>,
    /// The files that are tracked now and were not before, in the order of their paths. A path
    /// that was tracked already is not in it.
    pub added: Vec<String>,
}

impl Repository {
    /// Tracks the files at `paths` and saves them, with the tracked files, as the new working-copy
    /// commit (ADR-010).
    ///
    /// A path is relative to the working directory of the repository and is written with `/`. A
    /// directory stands for the files below it, without a directory named `.stemma`. A path is
    /// never followed through a link, and a link at the end of a path is tracked as a link. A path
    /// that is tracked already is not an error and changes nothing, but the tracked files are saved
    /// as in [`snapshot`](Self::snapshot), since the new tree is built from the working directory.
    /// The authorizer is asked once, before anything is read. No path is looked at when `paths` is
    /// empty, and the call is then a snapshot with another name in the log.
    ///
    /// # Errors
    ///
    /// Returns [`TrackError::InvalidPath`] for a path that is empty, absolute, or has a component
    /// that a tree refuses (`.`, `..`, `.stemma`, control characters), and
    /// [`TrackError::NotFound`] for a path that does not exist. Both are found before anything is
    /// written. [`TrackError::Snapshot`] carries every other failure, as [`snapshot`](Self::snapshot)
    /// reports them: a special file or a name that is not UTF-8 stops the call.
    pub fn track(
        &self,
        paths: &[String],
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Tracking, TrackError> {
        let operation = OperationName::new(OPERATION).expect("`track` is a valid operation name");
        let replaced =
            self.replace_working_copy(&operation, actor, authorizer, |root, known| {
                expand(root, paths, known)
            })?;
        Ok(Tracking {
            entry: replaced.entry,
            added: replaced.added,
        })
    }
}

/// Lists the files that the paths stand for and that are not tracked yet, in the order of their
/// paths.
fn expand(
    root: &Path,
    paths: &[String],
    known: &std::collections::HashSet<String>,
) -> Result<Vec<Tracked>, TrackError> {
    let mut found = BTreeSet::new();
    for path in paths {
        if path_problem(path).is_some() || !is_plain(path) {
            return Err(TrackError::InvalidPath { path: path.clone() });
        }
        let Some((full, metadata)) = lookup(root, path)? else {
            return Err(TrackError::NotFound { path: path.clone() });
        };
        if metadata.is_dir() {
            walk(full, path, &mut found)?;
        } else {
            found.insert(path.clone());
        }
    }
    Ok(found
        .into_iter()
        .filter(|path| !known.contains(path))
        .map(|path| Tracked {
            path,
            mode: FileMode::File,
        })
        .collect())
}

/// Tells whether the file system reads every part of the path as one name below the directory it
/// is joined to. Where `\` or `:` separates or starts a path (Windows), a part such as `a\..\b` or
/// `C:x` is more than one name, or leaves the directory; the tree rules do not know that.
fn is_plain(path: &str) -> bool {
    Path::new(path)
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
        && Path::new(path).components().count() == path.split('/').count()
}

/// Collects the paths of the files below a directory. A directory is listed with the names sorted,
/// and an explicit stack replaces the recursion, so that a deep tree cannot exhaust the stack.
fn walk(dir: PathBuf, prefix: &str, found: &mut BTreeSet<String>) -> Result<(), TrackError> {
    let mut pending = vec![(dir, prefix.to_owned())];
    while let Some((dir, prefix)) = pending.pop() {
        let mut names = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|source| io_error("list", &dir, source))? {
            let entry = entry.map_err(|source| io_error("list", &dir, source))?;
            names.push(entry.file_name());
        }
        names.sort();
        for name in names {
            let Some(name) = name.to_str() else {
                return Err(SnapshotError::Unsupported {
                    path: format!("{prefix}/{}", name.to_string_lossy()),
                    what: super::Unsupported::NonUtf8Name,
                }
                .into());
            };
            // A repository inside the directory is not part of the files of this one.
            if name == METADATA_DIR {
                continue;
            }
            let path = format!("{prefix}/{name}");
            if name_problem(name).is_some() {
                return Err(TrackError::InvalidPath { path });
            }
            let full = dir.join(name);
            let metadata = match fs::symlink_metadata(&full) {
                Ok(metadata) => metadata,
                // Gone since the listing: there is nothing to track.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => return Err(io_error("inspect", &full, source)),
            };
            if metadata.is_dir() {
                pending.push((full, path));
            } else {
                found.insert(path);
            }
        }
    }
    Ok(())
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> TrackError {
    SnapshotError::io(operation, path, source).into()
}

/// Why [`Repository::track`] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum TrackError {
    /// The path is empty, absolute, or has a component that a tree refuses.
    InvalidPath {
        /// The path as it was given, or found below a directory.
        path: String,
    },
    /// There is no file or directory at the path.
    NotFound {
        /// The path as it was given.
        path: String,
    },
    /// Anything else, as [`Repository::snapshot`] reports it.
    Snapshot(SnapshotError),
}

impl From<SnapshotError> for TrackError {
    fn from(error: SnapshotError) -> Self {
        Self::Snapshot(error)
    }
}

impl TrackError {
    /// Returns the error of the file system, if that is what failed. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Snapshot(error) => error.io_error(),
            Self::InvalidPath { .. } | Self::NotFound { .. } => None,
        }
    }
}

impl fmt::Display for TrackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { path } => write!(
                f,
                "`{}` cannot be tracked: a path is relative, written with `/`, and has no empty \
                 component, no `.` or `..`, and no name that a tree refuses (such as `.stemma`)",
                path.escape_debug()
            ),
            Self::NotFound { path } => {
                write!(f, "there is nothing at `{}` to track", path.escape_debug())
            }
            Self::Snapshot(error) => error.fmt(f),
        }
    }
}

impl Error for TrackError {}

#[cfg(test)]
mod tests {
    use super::super::snapshot::{read_commit, tracked_files};
    use super::*;
    use crate::object::Commit;
    use crate::repository::InitOptions;
    use crate::store::ObjectStore;
    use crate::testutil::TempDir;
    use crate::{AllowAll, Decision};

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &str, _target: &str) -> Decision {
            Decision::Deny
        }
    }

    fn actor() -> Actor {
        Actor::human("daniel").unwrap()
    }

    fn other_actor() -> Actor {
        Actor::human("alice").unwrap()
    }

    fn repository(dir: &TempDir) -> Repository {
        Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap()
    }

    fn write(dir: &TempDir, path: &str, content: &str) {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    fn track(repository: &Repository, paths: &[&str]) -> Result<Tracking, TrackError> {
        let paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();
        repository.track(&paths, &other_actor(), &AllowAll)
    }

    fn current(repository: &Repository) -> Commit {
        let id = repository.refs().working_copy().unwrap();
        read_commit(repository.objects(), id).unwrap()
    }

    fn paths(repository: &Repository) -> Vec<String> {
        let mut files: Vec<String> =
            tracked_files(repository.objects(), current(repository).tree())
                .unwrap()
                .into_iter()
                .map(|file| file.path)
                .collect();
        files.sort();
        files
    }

    #[test]
    fn a_new_file_is_tracked_and_the_commit_keeps_its_parents_change_and_message() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let before = current(&repository);
        let old = repository.refs().working_copy().unwrap();
        write(&dir, "a.txt", "one");
        write(&dir, "other.txt", "untracked");
        let tracking = track(&repository, &["a.txt"]).unwrap();
        assert_eq!(tracking.added, ["a.txt"]);
        assert_eq!(paths(&repository), ["a.txt"]);
        let after = current(&repository);
        assert_eq!(after.parents(), before.parents());
        assert_eq!(after.change(), before.change());
        assert_eq!(after.message(), before.message());
        assert_eq!(after.author(), &other_actor());
        let entry = tracking.entry.unwrap();
        assert_eq!(entry.change().operation().as_str(), "track");
        let moved = entry.change().working_copy().unwrap();
        let new = repository.refs().working_copy().unwrap();
        assert_eq!((moved.before(), moved.after()), (Some(&old), Some(&new)));
    }

    #[test]
    fn a_path_that_is_tracked_already_changes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, &["a.txt"]).unwrap();
        let before = repository.refs().working_copy().unwrap();
        let numbers = repository.oplog().numbers().unwrap();
        let tracking = track(&repository, &["a.txt"]).unwrap();
        assert!(tracking.entry.is_none() && tracking.added.is_empty());
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(repository.oplog().numbers().unwrap(), numbers);
    }

    #[test]
    fn a_track_also_saves_the_changes_of_the_tracked_files() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, &["a.txt"]).unwrap();
        write(&dir, "a.txt", "changed");
        write(&dir, "b.txt", "new");
        let tracking = track(&repository, &["b.txt"]).unwrap();
        assert_eq!(tracking.added, ["b.txt"]);
        // A second track finds the tracked file as it is now: nothing is left to save.
        assert!(track(&repository, &["a.txt"]).unwrap().entry.is_none());
    }

    #[test]
    fn a_directory_stands_for_the_files_below_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "d/b.txt", "b");
        write(&dir, "d/sub/c.txt", "c");
        write(&dir, "d/.stemma/config", "a repository inside");
        write(&dir, "outside.txt", "x");
        let tracking = track(&repository, &["d"]).unwrap();
        assert_eq!(tracking.added, ["d/b.txt", "d/sub/c.txt"]);
        assert_eq!(paths(&repository), ["d/b.txt", "d/sub/c.txt"]);
    }

    #[test]
    fn a_path_that_does_not_exist_is_refused_and_nothing_moves() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        let before = repository.refs().working_copy().unwrap();
        let numbers = repository.oplog().numbers().unwrap();
        let error = track(&repository, &["a.txt", "missing.txt"]).unwrap_err();
        assert!(
            matches!(&error, TrackError::NotFound { path } if path == "missing.txt"),
            "{error}"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(repository.oplog().numbers().unwrap(), numbers);
    }

    #[test]
    fn a_path_that_breaks_a_rule_is_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        for path in [
            "",
            "/a.txt",
            "../a.txt",
            "./a.txt",
            "a//b",
            ".stemma/x",
            "a/",
        ] {
            let error = track(&repository, &[path]).unwrap_err();
            assert!(
                matches!(&error, TrackError::InvalidPath { path: p } if p == path),
                "{path:?}: {error}"
            );
        }
    }

    #[test]
    fn a_path_is_plain_if_every_part_is_one_name() {
        assert!(is_plain("a/b.txt") && is_plain("a"));
        #[cfg(windows)]
        for path in ["a\\..\\b", "C:x", "C:\\x", "\\\\server\\share"] {
            assert!(!is_plain(path), "{path}");
        }
        #[cfg(unix)]
        assert!(is_plain("a\\b"), "a backslash is a name character here");
    }

    #[cfg(unix)]
    #[test]
    fn a_path_through_a_link_is_not_followed() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let outside = TempDir::new();
        write(&outside, "secret.txt", "secret");
        let repository = repository(&dir);
        symlink(outside.path(), dir.path().join("d")).unwrap();
        let error = track(&repository, &["d/secret.txt"]).unwrap_err();
        assert!(matches!(error, TrackError::NotFound { .. }), "{error}");
        assert!(paths(&repository).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_at_the_end_of_a_path_is_tracked_as_a_link() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let repository = repository(&dir);
        symlink("elsewhere", dir.path().join("l")).unwrap();
        let tracking = track(&repository, &["l"]).unwrap();
        assert_eq!(tracking.added, ["l"]);
        let files = tracked_files(repository.objects(), current(&repository).tree()).unwrap();
        assert_eq!(files[0].mode, FileMode::Symlink);
    }

    #[cfg(unix)]
    #[test]
    fn a_special_file_below_a_directory_stops_the_call_and_nothing_moves() {
        use std::os::unix::net::UnixListener;
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "d/a.txt", "one");
        UnixListener::bind(dir.path().join("d/sock")).unwrap();
        let before = repository.refs().working_copy().unwrap();
        let error = track(&repository, &["d"]).unwrap_err();
        assert!(
            matches!(
                &error,
                TrackError::Snapshot(SnapshotError::Unsupported { path, .. }) if path == "d/sock"
            ),
            "{error}"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), before);
    }

    #[test]
    fn a_denied_track_reads_and_writes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        let before = repository.refs().working_copy().unwrap();
        let numbers = repository.oplog().numbers().unwrap();
        let error = repository
            .track(&["a.txt".to_owned()], &actor(), &DenyAll)
            .unwrap_err();
        assert!(
            matches!(
                error,
                TrackError::Snapshot(SnapshotError::Update(
                    crate::repository::UpdateError::Denied { .. }
                ))
            ),
            "{error}"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(repository.oplog().numbers().unwrap(), numbers);
        let blob = crate::object::Blob::new(b"one".to_vec()).id().object_id();
        assert!(!repository.objects().contains(&blob).unwrap());
    }

    #[test]
    fn an_empty_list_of_paths_is_a_snapshot_of_the_tracked_files() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, &["a.txt"]).unwrap();
        assert!(track(&repository, &[]).unwrap().entry.is_none());
        write(&dir, "a.txt", "changed");
        let tracking = track(&repository, &[]).unwrap();
        assert!(tracking.entry.is_some() && tracking.added.is_empty());
    }

    #[test]
    fn a_tracked_file_is_saved_by_the_next_snapshot() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, &["a.txt"]).unwrap();
        write(&dir, "a.txt", "changed");
        assert!(repository
            .snapshot(&other_actor(), &AllowAll)
            .unwrap()
            .is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_stops_the_call() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dir = TempDir::new();
        let repository = repository(&dir);
        fs::create_dir(dir.path().join("d")).unwrap();
        let name = dir.path().join("d").join(OsStr::from_bytes(b"\xff\xfe"));
        if fs::write(name, "x").is_err() {
            eprintln!("skipped: this file system refuses names that are not UTF-8");
            return;
        }
        let error = track(&repository, &["d"]).unwrap_err();
        assert!(
            matches!(
                error,
                TrackError::Snapshot(SnapshotError::Unsupported {
                    what: super::super::Unsupported::NonUtf8Name,
                    ..
                })
            ),
            "{error}"
        );
    }
}
