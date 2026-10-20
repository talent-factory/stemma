//! Tracking files (ADR-010).
//!
//! A file that is not in the tree of the working-copy commit is untracked, and a snapshot does not
//! save it. [`Repository::track`] confirms files: it saves them in the next working-copy commit
//! together with the files that were tracked already, so a track never drops a change to a tracked
//! file.

use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::snapshot::{lookup, Tracked};
use super::{Repository, SnapshotError, Unsupported};
use crate::index::path_problem;
use crate::object::{name_problem, FileMode, NameProblem};
use crate::oplog::{Entry, OperationName};
use crate::{Actor, Authorizer};

/// The operation that the log records for tracking files.
const OPERATION: &str = "track";

/// What [`Repository::track`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Tracking {
    /// The entry of the move of the working-copy commit, or `None` if the tree is the same. A file
    /// in [`added`](Self::added) changes the tree, so `None` goes with an empty list.
    pub entry: Option<Entry>,
    /// The files that were not tracked and are saved now, in the order of their paths. A path that
    /// was tracked already is not in it. Under concurrent changes of the working directory it is
    /// what was saved: a file below a directory that is gone when it is saved is left out, and a
    /// file that was named and is gone is an error.
    pub added: Vec<String>,
}

impl Repository {
    /// Tracks the files at `paths` and saves them, with the tracked files, as the new working-copy
    /// commit (ADR-010).
    ///
    /// A path is relative to the working directory of the repository, is written with `/`, and
    /// spells every name as it is on disk (a file system that ignores case would take another
    /// spelling for the same file, and the tree would hold it twice). A directory stands for the
    /// files below it, without a name that resolves to `.stemma`; those are skipped, not
    /// reported. The path `.` stands for the working directory itself. A path is never followed through a link, and a link at the end of a path is
    /// tracked as a link. A path that is tracked already is not an error and adds nothing to the
    /// tree, but the tracked files are saved as in [`snapshot`](Self::snapshot), since the new
    /// tree is built from the working directory. The authorizer is asked once, before anything is
    /// read. With no paths, the call is a snapshot that the log names `track`.
    ///
    /// # Errors
    ///
    /// Returns [`TrackError::InvalidPath`] for a path that is empty, absolute, ends in `/`, or has
    /// a part that a tree refuses (`.`, `..`, `.stemma`, control characters) or that the file
    /// system reads as more than one name; [`TrackError::InvalidName`] for such a name below a
    /// directory; [`TrackError::NotFound`] for a path that does not exist, or is gone when it is
    /// saved; and [`TrackError::Spelling`] for a path that is not spelled as on disk. The
    /// working-copy commit moves only after every file is saved, so none of these moves it. The
    /// paths are checked before any file is saved, except that a named file can be gone when its
    /// turn comes. [`TrackError::Snapshot`] carries every other failure, as
    /// [`snapshot`](Self::snapshot) reports them: a special file or a name that is not UTF-8
    /// stops the call.
    pub fn track(
        &self,
        paths: &[String],
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Tracking, TrackError> {
        let operation = OperationName::new(OPERATION).expect("`track` is a valid operation name");
        self.replace_working_copy(&operation, actor, authorizer, |root| expand(root, paths))
    }
}

/// Lists the files that the paths stand for, in the order of their paths. A file at a path is
/// named; a file below a directory is not.
fn expand(root: &Path, paths: &[String]) -> Result<Vec<Tracked>, TrackError> {
    let mut named = BTreeSet::new();
    let mut below = BTreeSet::new();
    for path in paths {
        if path == "." {
            // The working directory itself: every file below it.
            walk(root.to_path_buf(), "", &mut below)?;
            continue;
        }
        if path_problem(path).is_some() || !is_plain(path) {
            return Err(TrackError::InvalidPath { path: path.clone() });
        }
        let Some((full, metadata)) = lookup(root, path)? else {
            return Err(TrackError::NotFound { path: path.clone() });
        };
        check_spelling(root, path)?;
        if metadata.is_dir() {
            walk(full, path, &mut below)?;
        } else {
            named.insert(path.clone());
        }
    }
    let named_files = named.iter().map(|path| (path, true));
    let found_files = below.iter().map(|path| (path, false));
    let mut files: Vec<Tracked> = named_files
        .chain(found_files)
        .map(|(path, named)| Tracked {
            path: path.clone(),
            mode: FileMode::File,
            named,
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|later, earlier| {
        // A file that is named and found below a directory too stays named.
        let same = later.path == earlier.path;
        if same {
            earlier.named |= later.named;
        }
        same
    });
    Ok(files)
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

/// Checks that every name of an existing path is spelled as the directory has it. On a file system
/// that ignores case, `a.txt` reaches `A.txt`, and the tree would hold the file under both.
fn check_spelling(root: &Path, path: &str) -> Result<(), TrackError> {
    let mut dir = root.to_path_buf();
    let mut spelled = Vec::new();
    let mut differs = false;
    for part in path.split('/') {
        let mut exact = false;
        let mut other = None;
        for entry in fs::read_dir(&dir).map_err(|source| io_error("list", &dir, source))? {
            let name = entry
                .map_err(|source| io_error("list", &dir, source))?
                .file_name();
            if name == OsStr::new(part) {
                exact = true;
                break;
            }
            if other.is_none() {
                other = name
                    .to_str()
                    .filter(|name| name.to_lowercase() == part.to_lowercase())
                    .map(str::to_owned);
            }
        }
        if exact {
            spelled.push(part.to_owned());
        } else {
            differs = true;
            match other {
                Some(name) => spelled.push(name),
                None => {
                    // The file system found the name by another rule, such as a Unicode form.
                    return Err(TrackError::Spelling {
                        path: path.to_owned(),
                        on_disk: None,
                    });
                }
            }
        }
        dir.push(part);
    }
    if differs {
        return Err(TrackError::Spelling {
            path: path.to_owned(),
            on_disk: Some(spelled.join("/")),
        });
    }
    Ok(())
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
                    path: join(&prefix, &format!("{name:?}")),
                    what: Unsupported::NonUtf8Name,
                }
                .into());
            };
            let path = join(&prefix, name);
            match name_problem(name) {
                None => {}
                // A name that resolves to `.stemma` is not a file of this repository, and a
                // tree cannot hold it. A repository inside the directory is skipped with it.
                Some(NameProblem::ReservedName) => continue,
                Some(_) => return Err(TrackError::InvalidName { path }),
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

/// The path of a name below a directory; the working directory itself has the empty path.
fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> TrackError {
    SnapshotError::io(operation, path, source).into()
}

/// Why [`Repository::track`] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum TrackError {
    /// The path is empty, absolute, ends in `/`, or has a part that a tree refuses or that the file
    /// system reads as more than one name.
    InvalidPath {
        /// The path as it was given.
        path: String,
    },
    /// A name below a directory that a tree refuses (a control character, an invisible character,
    /// a name of dots). The user did not type it, so it is not `InvalidPath`.
    InvalidName {
        /// The path of the file that has the name.
        path: String,
    },
    /// There is no file or directory at the path, a part before its last name is not a directory,
    /// or a file that was named is gone when it is saved.
    NotFound {
        /// The path as it was given.
        path: String,
    },
    /// A name of the path differs from the name in the directory, in the case of its letters or
    /// in another way that the file system ignores.
    Spelling {
        /// The path as it was given.
        path: String,
        /// The path as the directories spell it, if a name differs in case only.
        on_disk: Option<String>,
    },
    /// Anything else, as [`Repository::snapshot`] reports it.
    Snapshot(SnapshotError),
}

impl From<SnapshotError> for TrackError {
    fn from(error: SnapshotError) -> Self {
        match error {
            SnapshotError::Gone { path } => Self::NotFound { path },
            other => Self::Snapshot(other),
        }
    }
}

impl TrackError {
    /// Returns the error of the file system, if that is what failed. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Snapshot(error) => error.io_error(),
            Self::InvalidPath { .. }
            | Self::InvalidName { .. }
            | Self::NotFound { .. }
            | Self::Spelling { .. } => None,
        }
    }
}

impl fmt::Display for TrackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { path } => write!(
                f,
                "`{}` cannot be tracked: a path is relative, written with `/`, and has no empty \
                 part, no `.` or `..`, and no name that a tree refuses (such as `.stemma`)",
                path.escape_debug()
            ),
            Self::InvalidName { path } => write!(
                f,
                "`{}` cannot be tracked: its name is one that a tree refuses; rename it",
                path.escape_debug()
            ),
            Self::NotFound { path } => write!(
                f,
                "there is nothing at `{}` to track, or a part before its last name is not a \
                 directory",
                path.escape_debug()
            ),
            Self::Spelling {
                path,
                on_disk: Some(on_disk),
            } => write!(
                f,
                "`{}` is not spelled as on disk; use `{}`",
                path.escape_debug(),
                on_disk.escape_debug()
            ),
            Self::Spelling {
                path,
                on_disk: None,
            } => write!(
                f,
                "`{}` is not spelled as on disk; list the directory and use its name",
                path.escape_debug()
            ),
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
    fn a_dot_stands_for_the_whole_working_directory() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "a");
        write(&dir, "d/b.txt", "b");
        write(&dir, "d/sub/c.txt", "c");
        let tracking = track(&repository, &["."]).unwrap();
        assert_eq!(tracking.added, ["a.txt", "d/b.txt", "d/sub/c.txt"]);
        // The metadata directory is no file of the repository.
        assert_eq!(paths(&repository), ["a.txt", "d/b.txt", "d/sub/c.txt"]);
        assert!(track(&repository, &["."]).unwrap().entry.is_none());
    }

    #[test]
    fn a_dot_in_front_of_a_name_is_still_refused_in_the_core() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "a");
        let error = track(&repository, &["./a.txt"]).unwrap_err();
        assert!(matches!(error, TrackError::InvalidPath { .. }), "{error}");
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
        // A trailing `/` is an empty last part.
        let not_refused: Vec<_> = [
            "",
            "/a.txt",
            "../a.txt",
            "./a.txt",
            "a//b",
            ".stemma/x",
            "a/",
        ]
        .into_iter()
        .filter(|path| {
            !matches!(
                track(&repository, &[path]),
                Err(TrackError::InvalidPath { path: p }) if p == *path
            )
        })
        .collect();
        assert!(
            not_refused.is_empty(),
            "not refused as invalid: {not_refused:?}"
        );
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

    #[test]
    fn a_snapshot_after_a_track_finds_nothing_and_keeps_its_own_name_in_the_log() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, &["a.txt"]).unwrap();
        assert!(repository
            .snapshot(&other_actor(), &AllowAll)
            .unwrap()
            .is_none());
        write(&dir, "a.txt", "changed");
        let entry = repository
            .snapshot(&other_actor(), &AllowAll)
            .unwrap()
            .unwrap();
        assert_eq!(entry.change().operation().as_str(), "snapshot");
    }

    #[test]
    fn a_directory_that_is_partly_tracked_adds_only_the_rest() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "d/a.txt", "one");
        track(&repository, &["d/a.txt"]).unwrap();
        write(&dir, "d/a.txt", "changed");
        write(&dir, "d/b.txt", "two");
        let tracking = track(&repository, &["d"]).unwrap();
        assert_eq!(tracking.added, ["d/b.txt"]);
        assert_eq!(paths(&repository), ["d/a.txt", "d/b.txt"]);
        // The change to the tracked file was saved by the same call.
        assert!(repository
            .snapshot(&other_actor(), &AllowAll)
            .unwrap()
            .is_none());
    }

    #[test]
    fn overlapping_and_unsorted_paths_give_a_sorted_list_without_repeats() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        for path in ["z.txt", "a.txt", "d/b.txt", "d/c.txt"] {
            write(&dir, path, "x");
        }
        let tracking = track(&repository, &["z.txt", "a.txt", "d", "d/b.txt", "a.txt"]).unwrap();
        assert_eq!(tracking.added, ["a.txt", "d/b.txt", "d/c.txt", "z.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_directory_is_tracked_as_a_link_and_not_followed() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "real/inside.txt", "x");
        symlink("real", dir.path().join("l")).unwrap();
        let tracking = track(&repository, &["l"]).unwrap();
        assert_eq!(tracking.added, ["l"]);
        let files = tracked_files(repository.objects(), current(&repository).tree()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].mode, FileMode::Symlink);
        let error = track(&repository, &["l/inside.txt"]).unwrap_err();
        assert!(matches!(error, TrackError::NotFound { .. }), "{error}");
    }

    #[test]
    fn a_name_below_a_directory_that_a_tree_refuses_is_named_in_the_error() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "d/ok.txt", "x");
        write(&dir, "d/bad\nname", "x");
        let error = track(&repository, &["d"]).unwrap_err();
        assert!(
            matches!(&error, TrackError::InvalidName { path } if path.starts_with("d/bad")),
            "{error}"
        );
        assert!(paths(&repository).is_empty());
    }

    #[test]
    fn a_name_that_resolves_to_the_metadata_directory_is_skipped_below_a_directory() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "d/a.txt", "x");
        write(&dir, "d/.Stemma/config", "x");
        let tracking = track(&repository, &["d"]).unwrap();
        assert_eq!(tracking.added, ["d/a.txt"]);
    }

    #[test]
    fn a_path_not_spelled_as_on_disk_is_refused_with_the_name_there() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "Docs/Readme.txt", "x");
        // On a file system that tells the two apart, the path is not there; on one that does not,
        // it is there under another spelling. Both are refused, and nothing is tracked.
        let error = track(&repository, &["docs/readme.txt"]).unwrap_err();
        match &error {
            TrackError::NotFound { .. } => {}
            TrackError::Spelling { on_disk, .. } => {
                assert_eq!(on_disk.as_deref(), Some("Docs/Readme.txt"));
            }
            other => panic!("unexpected: {other}"),
        }
        assert!(paths(&repository).is_empty());
        assert_eq!(
            track(&repository, &["Docs/Readme.txt"])
                .unwrap()
                .added
                .len(),
            1
        );
    }

    #[test]
    fn a_file_with_a_tracked_name_in_another_case_is_not_tracked_twice() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "A.txt", "x");
        track(&repository, &["A.txt"]).unwrap();
        let _ = track(&repository, &["a.txt"]);
        assert_eq!(paths(&repository), ["A.txt"]);
    }

    #[test]
    fn a_named_file_that_is_gone_when_it_is_saved_is_an_error_and_a_found_one_is_left_out() {
        use super::super::snapshot::Tracked;
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "kept.txt", "x");
        let operation = OperationName::new("track").unwrap();
        let before = repository.refs().working_copy().unwrap();
        let add = |paths: &'static [(&'static str, bool)]| {
            repository.replace_working_copy(&operation, &actor(), &AllowAll, move |_| {
                Ok::<_, TrackError>(
                    paths
                        .iter()
                        .map(|(path, named)| Tracked {
                            path: (*path).to_owned(),
                            mode: FileMode::File,
                            named: *named,
                        })
                        .collect(),
                )
            })
        };
        let error = add(&[("kept.txt", true), ("ghost.txt", true)]).unwrap_err();
        assert!(
            matches!(&error, TrackError::NotFound { path } if path == "ghost.txt"),
            "{error}"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        let tracking = add(&[("kept.txt", false), ("ghost.txt", false)]).unwrap();
        assert_eq!(tracking.added, ["kept.txt"]);
        assert!(tracking.entry.is_some());
    }

    #[test]
    fn an_added_file_that_is_known_or_repeated_is_saved_once() {
        use super::super::snapshot::Tracked;
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "x");
        track(&repository, &["a.txt"]).unwrap();
        write(&dir, "b.txt", "y");
        let operation = OperationName::new("track").unwrap();
        let tracking = repository
            .replace_working_copy(&operation, &actor(), &AllowAll, |_| {
                Ok::<_, TrackError>(
                    ["a.txt", "b.txt", "b.txt"]
                        .map(|path| Tracked {
                            path: path.to_owned(),
                            mode: FileMode::File,
                            named: false,
                        })
                        .into(),
                )
            })
            .unwrap();
        assert_eq!(tracking.added, ["b.txt"]);
    }

    #[test]
    fn an_index_error_says_what_changed_and_has_no_file_system_error() {
        use crate::index::Index;
        use crate::object::Blob;
        let id = Blob::new(b"x".to_vec()).id();
        let mut index = Index::new();
        index.insert("f", FileMode::File, id).unwrap();
        let conflict = index.insert("f/x", FileMode::File, id).unwrap_err();
        let error = SnapshotError::Index(conflict);
        assert!(error.to_string().contains("changed while"), "{error}");
        assert!(error.io_error().is_none());
    }

    #[test]
    fn a_remedy_fits_what_cannot_be_saved() {
        let name = SnapshotError::Unsupported {
            path: "d/x".to_owned(),
            what: Unsupported::NonUtf8Name,
        };
        assert!(name.to_string().contains("rename it"), "{name}");
        let special = SnapshotError::Unsupported {
            path: "sock".to_owned(),
            what: Unsupported::SpecialFile,
        };
        assert!(special.to_string().contains("regular file"), "{special}");
    }
}
