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
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use super::snapshot::{lookup, Tracked};
use super::{Repository, SnapshotError, Unsupported, UpdateError};
use crate::ignore::{IgnoreError, IgnoreRules};
use crate::index::path_problem;
use crate::object::{name_problem, FileMode, NameProblem, GIT_DIR};
use crate::oplog::{Entry, OperationName};
use crate::store::is_checkout_temp_name;
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
    /// files below it, without a name that resolves to `.stemma` and without what `.stemmaignore`
    /// excludes (ADR-012); those are skipped, not reported, while a path that names one is an
    /// error. The path `.` alone stands for the
    /// working directory itself; [`normalize_path`] turns what a person types into this form. A
    /// path is never followed through a link, and a link at the end of a path is
    /// tracked as a link. A path that is tracked already is not an error (unless a pattern
    /// excludes it) and adds nothing to the tree, but the tracked files are saved as in
    /// [`snapshot`](Self::snapshot), since the new tree is built from the working directory. The
    /// authorizer is asked once, before anything is read, also before `.stemmaignore`. With no
    /// paths, the call is a snapshot that the log names `track`, and it does not read
    /// `.stemmaignore`.
    ///
    /// # Errors
    ///
    /// Returns [`TrackError::InvalidPath`] for a path that is empty, absolute, ends in `/`, or has
    /// a part that a tree refuses (`..`, `.stemma`, control characters, and `.` in a longer path)
    /// or that the file system reads as more than one name; [`TrackError::InvalidName`] for such a
    /// name below a directory (unless a pattern excludes the file); [`TrackError::NotFound`] for a
    /// path that does not exist, or is gone when it is saved; [`TrackError::Excluded`] for a named
    /// path that a pattern of `.stemmaignore` excludes, with the line of the pattern;
    /// [`TrackError::Ignore`] if `.stemmaignore` is not valid or cannot be read (nothing is
    /// tracked, whatever the paths are); [`TrackError::NotDurable`] if the files are tracked but a
    /// directory could not be flushed (the list of files is in the error); and
    /// [`TrackError::Spelling`] for a path that is not spelled as on disk;
    /// [`TrackError::GitDirectory`] for a named path through `.git`, and
    /// [`TrackError::CheckoutLeftover`] for one that names a temporary file of a checkout. The
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
        let replaced = self.replace_working_copy(&operation, actor, authorizer, |root| {
            // The authorizer agreed already. With no paths the call is a snapshot, which never
            // reads the file (ADR-012). Otherwise a file that cannot be read is an error and not
            // "no patterns": a rule that cannot be read must not become "exclude nothing".
            let rules = if paths.is_empty() {
                IgnoreRules::empty()
            } else {
                IgnoreRules::read(root).map_err(TrackError::Ignore)?
            };
            expand(root, paths, &rules)
        })?;
        match replaced.late {
            Some(error) => Err(TrackError::NotDurable {
                added: replaced.added,
                error: Box::new(error),
            }),
            None => Ok(Tracking {
                entry: replaced.entry,
                added: replaced.added,
            }),
        }
    }
}

/// Turns a path as a person types it into the form of [`Repository::track`]: `./a.txt` and `dir/`
/// are `a.txt` and `dir`, an empty part (`a//b`) and a part `.` (`a/./b`) are dropped, and a
/// path that is only dots and slashes with at least one `.` part (`.`, `./`) is `.`. Nothing
/// else changes: `..` stays, so does an absolute path, and an empty argument or one of slashes
/// only (`""`, `//`) stays as it is, so that the core refuses it. A script with an empty
/// variable must not track the whole directory.
pub fn normalize_path(path: &str) -> String {
    if path.starts_with('/') {
        return path.to_owned();
    }
    let parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if !parts.is_empty() {
        parts.join("/")
    } else if path.split('/').any(|part| part == ".") {
        ".".to_owned()
    } else {
        path.to_owned()
    }
}

/// Turns a path that a person gives relative to `subdirectory`, a directory below the root of the
/// repository as [`Discovered::subdirectory`](super::Discovered::subdirectory) has it, into the
/// form of [`Repository::track`], relative to the root. A `..` at the start of the path leads up from
/// `subdirectory`, to the root and not beyond. Any other `..` stays, and so does one that would
/// leave the repository, and the core refuses such a path: a `..` after a part that may be a link
/// does not lead where the name says. An absolute path stays as it is, and so
/// does an empty argument or one of slashes only, as in [`normalize_path`], so that a script with
/// an empty variable does not track the whole subdirectory. With an empty `subdirectory`, this is
/// [`normalize_path`].
pub fn normalize_path_in(subdirectory: &str, path: &str) -> String {
    if path.starts_with('/') || path.split('/').all(str::is_empty) {
        return path.to_owned();
    }
    let mut base: Vec<&str> = subdirectory
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let mut rest: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let up = rest.iter().take_while(|part| **part == "..").count();
    let taken = up.min(base.len());
    base.truncate(base.len() - taken);
    rest.drain(..taken);
    base.extend(rest);
    if base.is_empty() {
        ".".to_owned()
    } else {
        base.join("/")
    }
}

/// Lists the files that the paths stand for, in the order of their paths. A file at a path is
/// named; a file below a directory is not. A named path that `rules` exclude is an error, and a
/// file below a directory that they exclude is left out.
fn expand(root: &Path, paths: &[String], rules: &IgnoreRules) -> Result<Vec<Tracked>, TrackError> {
    let mut named = BTreeSet::new();
    let mut below = Found::default();
    for path in paths {
        if path == "." {
            // The working directory itself: every file below it. It may be a link to the
            // directory, and it may be gone since the repository was opened.
            match fs::metadata(root) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => return Err(TrackError::NotFound { path: path.clone() }),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(TrackError::NotFound { path: path.clone() });
                }
                Err(source) => return Err(io_error("inspect", root, source)),
            }
            walk(root.to_path_buf(), "", rules, &mut below)?;
            continue;
        }
        if path_problem(path).is_some() || !is_plain(path) {
            return Err(TrackError::InvalidPath { path: path.clone() });
        }
        let Some((full, metadata)) = lookup(root, path)? else {
            return Err(TrackError::NotFound { path: path.clone() });
        };
        check_spelling(root, path)?;
        // The path is spelled as on disk now, and the patterns compare names byte by byte.
        if path.split('/').any(is_git_dir) {
            return Err(TrackError::GitDirectory { path: path.clone() });
        }
        // A checkout writes only files and links under that name, so only a last part that is no
        // directory is one; a directory of that name is ordinary content, as in the walk.
        if !metadata.is_dir() && path.rsplit('/').next().is_some_and(is_checkout_temp_name) {
            return Err(TrackError::CheckoutLeftover { path: path.clone() });
        }
        if let Some(line) = rules.matching(path, metadata.is_dir()) {
            return Err(TrackError::Excluded {
                path: path.clone(),
                line,
            });
        }
        if metadata.is_dir() {
            walk(full, path, rules, &mut below)?;
        } else {
            named.insert(path.clone());
        }
    }
    let named_files = named.iter().map(|path| (path, true));
    let found_files = below.files.iter().map(|path| (path, false));
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

/// What [`walk`] finds below a directory.
#[derive(Debug, Default)]
pub(super) struct Found {
    /// The paths of the files that the rules do not exclude, without `.git`, `.stemma`, and the
    /// temporary files of a checkout.
    pub(super) files: BTreeSet<String>,
    /// The paths of the temporary files that a checkout left behind ([`is_checkout_temp_name`]).
    pub(super) leftovers: BTreeSet<String>,
}

/// Collects the paths of the files below a directory that the rules do not exclude, and apart from
/// them the temporary files that a checkout left ([`Found::leftovers`]), also one that a pattern
/// matches. A directory is listed with the names sorted, and an explicit stack replaces the
/// recursion, so that a deep tree cannot exhaust the stack.
pub(super) fn walk(
    dir: PathBuf,
    prefix: &str,
    rules: &IgnoreRules,
    found: &mut Found,
) -> Result<(), TrackError> {
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
            // The metadata of Git is left out at every depth, as a link file of a worktree or a
            // submodule is, and no pattern can lift that (ADR-012).
            if is_git_dir(name) {
                continue;
            }
            let problem = name_problem(name);
            // A name that resolves to `.stemma` is not a file of this repository, and a tree
            // cannot hold it. A repository inside the directory is skipped with it.
            if problem == Some(NameProblem::ReservedName) {
                continue;
            }
            let full = dir.join(name);
            let metadata = match fs::symlink_metadata(&full) {
                Ok(metadata) => metadata,
                // Gone since the listing: there is nothing to track.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => return Err(io_error("inspect", &full, source)),
            };
            // A temporary file of a checkout that a killed process left holds the content of an
            // old blob, perhaps a secret that was purged since: it is never a file to track, and
            // it is listed so that a person can remove it (ADR-013). This comes before the
            // patterns, so that a pattern that matches its name does not hide it. A checkout
            // writes only files and links there, so a directory of that name is ordinary content.
            if !metadata.is_dir() && is_checkout_temp_name(name) {
                found.leftovers.insert(path);
                continue;
            }
            // An excluded directory is not entered, and an excluded file is not listed, also one
            // whose name a tree refuses: a pattern is the way to leave such a file out.
            if rules.is_ignored(&path, metadata.is_dir()) {
                continue;
            }
            if problem.is_some() {
                return Err(TrackError::InvalidName { path });
            }
            if metadata.is_dir() {
                pending.push((full, path));
            } else {
                found.files.insert(path);
            }
        }
    }
    Ok(())
}

/// Whether the name is the metadata of Git. The comparison ignores the case of ASCII letters, as
/// the file systems of macOS and Windows do.
fn is_git_dir(name: &str) -> bool {
    name.eq_ignore_ascii_case(GIT_DIR)
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
    /// The path is excluded by a pattern of `.stemmaignore` (ADR-012): not even a path that the
    /// caller names is added, and this holds for a path that is tracked already (it stays
    /// tracked). A file below a directory is skipped without a word instead.
    Excluded {
        /// The path as it was given.
        path: String,
        /// The line of the pattern in `.stemmaignore`.
        line: NonZeroUsize,
    },
    /// A part of the path is `.git`, the metadata of Git, which `track` always leaves out
    /// (ADR-012). Unlike [`TrackError::Excluded`] no line of `.stemmaignore` is involved.
    GitDirectory {
        /// The path as it was given.
        path: String,
    },
    /// The path names a temporary file that a checkout left when its process was killed (ADR-013):
    /// a file or a link named `.stemma-checkout-`, a process ID, a counter, and 16 lowercase
    /// hexadecimal digits, which `track` always leaves out, as it holds the content of an old blob.
    CheckoutLeftover {
        /// The path as it was given.
        path: String,
    },
    /// `.stemmaignore` cannot be read, is not a regular file, changed while it was read, or is not
    /// valid. Nothing is tracked, as a rule that cannot be read must not become "exclude
    /// nothing".
    Ignore(IgnoreError),
    /// The working-copy commit moved and the files are tracked, but a directory could not be
    /// flushed to disk, so the change may not survive a crash. Do not repeat the call: read the
    /// pointer.
    NotDurable {
        /// The files that are tracked now and were not before, as in [`Tracking::added`].
        added: Vec<String>,
        /// The error of the move, an [`UpdateError::NotDurable`].
        error: Box<UpdateError>,
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
            Self::NotDurable { error, .. } => error.io_error(),
            Self::Ignore(error) => error.io_error(),
            Self::InvalidPath { .. }
            | Self::InvalidName { .. }
            | Self::Excluded { .. }
            | Self::GitDirectory { .. }
            | Self::CheckoutLeftover { .. }
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
                "`{}` cannot be tracked: a path in the repository is relative to its root, written \
                 with `/`, and has no empty part, no `.` or `..`, and no name that a tree refuses \
                 (such as `.stemma`); a `..` at the start may lead up to the root, not beyond it",
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
            Self::Excluded { path, line } => write!(
                f,
                "`{}` is excluded by line {line} of {}, so `track` leaves it out; remove or change \
                 that line to add it",
                path.escape_debug(),
                crate::ignore::FILE_NAME
            ),
            Self::GitDirectory { path } => write!(
                f,
                "`{}` is part of the metadata of Git, which `track` always leaves out",
                path.escape_debug()
            ),
            Self::CheckoutLeftover { path } => write!(
                f,
                "`{}` is a temporary file that an interrupted `op restore` left, which `track` \
                 always leaves out; remove it, or rename it if it is a file of your own",
                path.escape_debug()
            ),
            Self::Ignore(error) => write!(f, "{error}; nothing was tracked"),
            Self::NotDurable { error, .. } => error.fmt(f),
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
        assert_eq!(entry.change().unwrap().operation().as_str(), "track");
        let moved = entry.change().unwrap().working_copy().unwrap();
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
    fn a_path_is_written_in_the_form_of_the_core() {
        for (given, normal) in [
            ("a.txt", "a.txt"),
            ("./a.txt", "a.txt"),
            ("dir/", "dir"),
            ("./dir//sub/./", "dir/sub"),
            (".", "."),
            ("./", "."),
            ("././", "."),
            ("../a", "../a"),
            ("a/../b", "a/../b"),
            ("/abs/a", "/abs/a"),
            ("", ""),
            ("//", "//"),
        ] {
            assert_eq!(normalize_path(given), normal, "{given:?}");
        }
    }

    #[test]
    fn a_path_relative_to_a_subdirectory_is_written_relative_to_the_root() {
        for (subdirectory, given, normal) in [
            ("", "a.txt", "a.txt"),
            ("", "./dir//sub/./", "dir/sub"),
            ("", ".", "."),
            ("", "../a", "../a"),
            ("", "a/../b", "a/../b"),
            ("d", "a.txt", "d/a.txt"),
            ("d/e", "./a.txt", "d/e/a.txt"),
            ("d", ".", "d"),
            ("d", "./", "d"),
            ("d", "..", "."),
            ("d/e", "../a", "d/a"),
            ("d/e", "../../a", "a"),
            ("d", "../../a", "../a"),
            ("d", "x/../y", "d/x/../y"),
            ("d", "../x/../y", "x/../y"),
            ("d", "/abs/a", "/abs/a"),
            ("d", "", ""),
            ("d", "//", "//"),
        ] {
            assert_eq!(
                normalize_path_in(subdirectory, given),
                normal,
                "{subdirectory:?} {given:?}"
            );
        }
        // At the root, it is the form of `normalize_path`.
        for given in [
            "a.txt", "./a", "dir/", ".", "../a", "a/../b", "/abs", "", "//",
        ] {
            assert_eq!(
                normalize_path_in("", given),
                normalize_path(given),
                "{given:?}"
            );
        }
    }

    #[test]
    fn an_empty_path_is_refused_and_does_not_track_the_directory() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "a");
        for given in ["", "//"] {
            let error = track(&repository, &[&normalize_path(given)]).unwrap_err();
            assert!(matches!(error, TrackError::InvalidPath { .. }), "{error}");
        }
        assert!(paths(&repository).is_empty());
    }

    #[test]
    fn a_dot_with_other_paths_gives_the_same_list_in_any_order() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "a");
        write(&dir, "d/b.txt", "b");
        for order in [vec![".", "a.txt", "d"], vec!["d", "a.txt", "."]] {
            let tracking = track(&repository, &order).unwrap();
            if tracking.entry.is_some() {
                assert_eq!(tracking.added, ["a.txt", "d/b.txt"]);
            }
        }
        assert_eq!(paths(&repository), ["a.txt", "d/b.txt"]);
    }

    #[test]
    fn a_dot_in_an_empty_directory_has_nothing_to_track() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let numbers = repository.oplog().numbers().unwrap();
        let tracking = track(&repository, &["."]).unwrap();
        assert!(tracking.entry.is_none() && tracking.added.is_empty());
        assert_eq!(repository.oplog().numbers().unwrap(), numbers);
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
        assert_eq!(entry.change().unwrap().operation().as_str(), "snapshot");
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

    // Windows does not allow a line feed in a file name, so the file cannot be created there.
    #[cfg(unix)]
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

    // ---- the exclusion file (ADR-012)

    fn ignore(dir: &TempDir, text: &str) {
        write(dir, ".stemmaignore", text);
    }

    #[test]
    fn a_file_below_a_directory_that_a_pattern_excludes_is_skipped_without_a_word() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "*.log\ntarget/\n");
        write(&dir, "a.txt", "a");
        write(&dir, "d/x.log", "log");
        write(&dir, "d/y.txt", "y");
        write(&dir, "target/debug/z", "z");

        let tracking = track(&repository, &["."]).unwrap();

        assert_eq!(tracking.added, [".stemmaignore", "a.txt", "d/y.txt"]);
        assert_eq!(paths(&repository), [".stemmaignore", "a.txt", "d/y.txt"]);
    }

    #[test]
    fn a_path_that_is_named_and_excluded_is_an_error_with_the_line() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "# build\n*.log\ntarget/\n");
        write(&dir, "d/x.log", "log");
        write(&dir, "target/z", "z");
        let entries = repository.oplog().entries().unwrap().len();

        for (path, line) in [("d/x.log", 2), ("target", 3), ("target/z", 3)] {
            let error = track(&repository, &[path]).unwrap_err();
            assert!(
                matches!(&error, TrackError::Excluded { path: p, line: l }
                    if p == path && l.get() == line),
                "{path}: {error}"
            );
            assert!(
                error
                    .to_string()
                    .contains(&format!("line {line} of .stemmaignore")),
                "{error}"
            );
        }
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert!(paths(&repository).is_empty());
    }

    #[test]
    fn an_excluded_directory_is_not_entered() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "/vendor\n");
        // A name that a tree refuses would stop the call, but it is below an excluded directory.
        write(&dir, "vendor/a\u{200B}b", "x");
        write(&dir, "src/a.rs", "a");

        let tracking = track(&repository, &["."]).unwrap();

        assert_eq!(tracking.added, [".stemmaignore", "src/a.rs"]);
    }

    #[test]
    fn what_is_tracked_already_stays_tracked_and_is_saved_after_the_file_excludes_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "secret.env", "one");
        track(&repository, &["secret.env"]).unwrap();
        ignore(&dir, "*.env\n");
        write(&dir, "secret.env", "two");

        let tracking = track(&repository, &[".stemmaignore"]).unwrap();

        assert_eq!(tracking.added, [".stemmaignore"]);
        assert!(paths(&repository).contains(&"secret.env".to_owned()));
        // The new content is in the new commit: the snapshot saves tracked files.
        assert_eq!(content(&repository, "secret.env"), b"two");
        let again = repository.snapshot(&actor(), &AllowAll).unwrap();
        assert!(
            again.is_none(),
            "a second snapshot has nothing left to save"
        );
    }

    #[test]
    fn the_file_is_never_excluded_by_its_own_wildcard() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, ".stemma*\n");
        write(&dir, "a.txt", "a");

        let tracking = track(&repository, &["."]).unwrap();

        assert!(
            tracking.added.contains(&".stemmaignore".to_owned()),
            "{tracking:?}"
        );
    }

    #[test]
    fn an_invalid_file_stops_the_call_before_anything_is_tracked() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "*.log\n\n");
        write(&dir, "a.txt", "a");
        let entries = repository.oplog().entries().unwrap().len();

        let error = track(&repository, &["a.txt"]).unwrap_err();

        assert!(matches!(&error, TrackError::Ignore(_)), "{error}");
        assert!(error.to_string().contains("line 2: empty line"), "{error}");
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert!(paths(&repository).is_empty());
    }

    #[test]
    fn an_invalid_file_does_not_stop_a_snapshot() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, &["a.txt"]).unwrap();
        ignore(&dir, "!broken\n");
        write(&dir, "a.txt", "two");

        let entry = repository.snapshot(&actor(), &AllowAll).unwrap();

        assert!(entry.is_some());
        assert_eq!(content(&repository, "a.txt"), b"two");
    }

    #[test]
    fn a_denied_call_does_not_read_the_file() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "!broken\n");

        write(&dir, "a.txt", "a");

        let error = repository
            .track(&["a.txt".to_owned()], &actor(), &DenyAll)
            .unwrap_err();

        assert!(matches!(error, TrackError::Snapshot(_)), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_as_the_file_stops_the_call() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "rules", "*.log\n");
        std::os::unix::fs::symlink("rules", dir.path().join(".stemmaignore")).unwrap();

        let error = track(&repository, &["rules"]).unwrap_err();

        assert!(matches!(&error, TrackError::Ignore(_)), "{error}");
        assert!(error.to_string().contains("not a regular file"), "{error}");
    }

    /// The content of a tracked file at the top of the working-copy tree.
    fn content(repository: &Repository, path: &str) -> Vec<u8> {
        let tree = current(repository).tree();
        let raw = repository
            .objects()
            .get_expecting(&tree.object_id(), crate::object::Kind::Tree)
            .unwrap()
            .unwrap();
        let tree = crate::object::Tree::parse(raw.body()).unwrap();
        let entry = tree
            .entries()
            .iter()
            .find(|entry| entry.name() == path)
            .unwrap_or_else(|| panic!("`{path}` is not tracked"));
        repository
            .objects()
            .get_expecting(&entry.id(), crate::object::Kind::Blob)
            .unwrap()
            .unwrap()
            .body()
            .to_vec()
    }

    #[test]
    fn a_named_directory_leaves_out_the_excluded_files_below_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "*.log\n");
        write(&dir, "d/x.log", "log");
        write(&dir, "d/y.txt", "y");

        let tracking = track(&repository, &["d"]).unwrap();

        assert_eq!(tracking.added, ["d/y.txt"]);
    }

    #[test]
    fn an_unanchored_pattern_reaches_a_nested_directory_and_a_rooted_one_does_not() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "target/\n/vendor\n");
        write(&dir, "a/target/z", "z");
        write(&dir, "vendor/v", "v");
        write(&dir, "a/vendor/x", "x");

        let tracking = track(&repository, &["."]).unwrap();

        assert_eq!(tracking.added, [".stemmaignore", "a/vendor/x"]);
    }

    #[test]
    fn the_metadata_of_git_is_left_out_at_every_depth_without_a_pattern() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "a");
        write(&dir, ".git/HEAD", "ref");
        write(&dir, "vendor/lib/.git", "gitdir: ../../.git/modules/lib");
        write(&dir, "vendor/lib/x.txt", "x");

        let tracking = track(&repository, &["."]).unwrap();

        assert_eq!(tracking.added, ["a.txt", "vendor/lib/x.txt"]);
    }

    #[test]
    fn a_file_that_an_interrupted_checkout_left_is_never_tracked() {
        // It holds the content of an old blob, which may be a secret that was purged since.
        let dir = TempDir::new();
        let repository = repository(&dir);
        let left = ".stemma-checkout-4711-3-0123456789abcdef";
        write(&dir, "a.txt", "a");
        write(&dir, left, "key=hunter2");
        write(&dir, &format!("d/{left}"), "old");
        // A name of the working copy with the same start is an ordinary file.
        write(&dir, ".stemma-checkout-notes", "n");
        let tracking = track(&repository, &["."]).unwrap();
        assert_eq!(tracking.added, [".stemma-checkout-notes", "a.txt"]);
        // A name of it is an error, also when a pattern matches it: the error says what it is,
        // and not that a line excludes it.
        ignore(&dir, ".stemma-*\n");
        for path in [left.to_owned(), format!("d/{left}")] {
            let error = track(&repository, &[&path]).unwrap_err();
            assert!(
                matches!(&error, TrackError::CheckoutLeftover { path: at } if *at == path),
                "{error}"
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "`{path}` is a temporary file that an interrupted `op restore` left, which \
                     `track` always leaves out; remove it, or rename it if it is a file of your \
                     own"
                )
            );
        }
    }

    #[test]
    fn a_directory_with_the_name_of_a_leftover_is_ordinary_content() {
        // A checkout writes only files and links under that name, never a directory.
        let dir = TempDir::new();
        let repository = repository(&dir);
        let name = ".stemma-checkout-4711-3-0123456789abcdef";
        write(&dir, &format!("{name}/x.txt"), "x");
        let tracking = track(&repository, &["."]).unwrap();
        assert_eq!(tracking.added, [format!("{name}/x.txt")]);
        // A path through it is no error either.
        write(&dir, &format!("{name}/y.txt"), "y");
        let tracking = track(&repository, &[&format!("{name}/y.txt")]).unwrap();
        assert_eq!(tracking.added, [format!("{name}/y.txt")]);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_an_interrupted_checkout_left_is_a_leftover_also_to_a_directory() {
        // A checkout writes a link under that name too, and the link is not followed.
        let dir = TempDir::new();
        let repository = repository(&dir);
        let left = ".stemma-checkout-4711-3-0123456789abcdef";
        write(&dir, "real/inside.txt", "x");
        std::os::unix::fs::symlink("real", dir.path().join(left)).unwrap();
        let mut found = Found::default();
        walk(
            dir.path().to_path_buf(),
            "",
            &IgnoreRules::default(),
            &mut found,
        )
        .unwrap();
        assert_eq!(found.leftovers.iter().collect::<Vec<_>>(), [left]);
        assert!(!found.files.iter().any(|path| path.starts_with(left)));
        let error = track(&repository, &[left]).unwrap_err();
        assert!(
            matches!(&error, TrackError::CheckoutLeftover { path } if path == left),
            "{error}"
        );
    }

    #[test]
    fn a_pattern_cannot_bring_the_metadata_of_git_back() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "*.txt\n");
        write(&dir, ".git/config", "c");

        let tracking = track(&repository, &["."]).unwrap();

        assert_eq!(tracking.added, [".stemmaignore"]);
    }

    #[test]
    fn a_named_path_in_the_metadata_of_git_is_an_error() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, ".git/HEAD", "ref");
        write(&dir, "a.txt", "a");
        let entries = repository.oplog().entries().unwrap().len();

        for path in [".git", ".git/HEAD"] {
            let error = track(&repository, &["a.txt", path]).unwrap_err();
            assert!(
                matches!(error, TrackError::GitDirectory { .. }),
                "{path}: {error}"
            );
        }

        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert!(paths(&repository).is_empty());
    }

    #[test]
    fn one_excluded_path_among_several_stops_the_call_and_tracks_none() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "*.log\n");
        write(&dir, "a.txt", "a");
        write(&dir, "d/x.log", "log");
        let entries = repository.oplog().entries().unwrap().len();

        let error = track(&repository, &["a.txt", "d/x.log"]).unwrap_err();

        assert!(matches!(error, TrackError::Excluded { .. }), "{error}");
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert!(paths(&repository).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_not_a_directory_for_a_pattern_with_a_trailing_slash() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "target/\n");
        write(&dir, "real/x", "x");
        std::os::unix::fs::symlink("real", dir.path().join("target")).unwrap();

        let tracking = track(&repository, &["."]).unwrap();

        assert_eq!(tracking.added, [".stemmaignore", "real/x", "target"]);
    }

    #[test]
    fn a_pattern_compares_bytes_so_another_case_is_another_name() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "Target/\n");
        write(&dir, "target/z", "z");

        // The pattern is the person's text, and a name is the name on disk (ADR-012).
        let tracking = track(&repository, &["target"]).unwrap();

        assert_eq!(tracking.added, ["target/z"]);
    }

    #[test]
    fn a_file_whose_name_a_tree_refuses_is_left_out_when_a_pattern_excludes_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "*.log\n");
        write(&dir, "d/a\u{200B}b.log", "x");
        write(&dir, "d/ok.txt", "ok");

        let tracking = track(&repository, &["."]).unwrap();

        assert_eq!(tracking.added, [".stemmaignore", "d/ok.txt"]);
    }

    #[test]
    fn a_file_whose_name_a_tree_refuses_still_stops_the_call_when_no_pattern_excludes_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "*.log\n");
        write(&dir, "d/a\u{200B}b.txt", "x");

        let error = track(&repository, &["."]).unwrap_err();

        assert!(matches!(&error, TrackError::InvalidName { .. }), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_file_with_a_carriage_return_in_its_name_can_be_excluded_by_a_question_mark() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        // macOS writes `Icon\r` into a folder that has its own icon.
        ignore(&dir, "Icon?\n");
        write(&dir, "d/Icon\r", "icon");
        write(&dir, "d/a.txt", "a");

        let tracking = track(&repository, &["."]).unwrap();

        assert_eq!(tracking.added, [".stemmaignore", "d/a.txt"]);
    }

    #[test]
    fn a_call_without_paths_does_not_read_an_invalid_file() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, &["a.txt"]).unwrap();
        ignore(&dir, "!broken\n");
        write(&dir, "a.txt", "two");

        let tracking = track(&repository, &[]).unwrap();

        assert!(tracking.entry.is_some() && tracking.added.is_empty());
        assert_eq!(content(&repository, "a.txt"), b"two");
    }

    #[test]
    fn the_errors_say_what_to_do_and_that_nothing_was_tracked() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        ignore(&dir, "*.log\n");
        write(&dir, "x.log", "log");
        let excluded = track(&repository, &["x.log"]).unwrap_err();
        assert!(
            excluded
                .to_string()
                .contains("remove or change that line to add it"),
            "{excluded}"
        );
        ignore(&dir, "*.log\n\n");
        let invalid = track(&repository, &["x.log"]).unwrap_err();
        assert!(
            invalid.to_string().ends_with("; nothing was tracked"),
            "{invalid}"
        );
        assert!(invalid.io_error().is_none());
    }
}
