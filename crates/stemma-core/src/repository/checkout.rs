//! The checkout layer: writing a tree into the working directory (ADR-013, ADR-005).
//!
//! [`Repository::plan_checkout`] compares the tree the working directory has with the tree it
//! should have and finds every conflict before a file is touched. [`Repository::apply_checkout`]
//! then removes, creates, and replaces files. The layer has no command of its own: the restore
//! uses it, and so will the checkout of a branch.
//!
//! Only a path that the old or the new tree names is touched. A file that is not tracked, a path
//! that `.stemmaignore` excludes, `.stemma`, and `.git` are never read, written, or removed, and
//! a path of the new tree that lies in the way of one of them is an error. The caller starts with
//! a snapshot, so that the files of the old tree are what the directory holds; the layer does not
//! compare their content.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::Path;

use super::snapshot::{lookup, read_object, SnapshotError};
use super::Repository;
use crate::ignore::{IgnoreError, IgnoreRules};
use crate::object::{BlobId, FileMode, Kind, Tree, TreeId, GIT_DIR, METADATA_DIR};
use crate::store::{unique_temp_name, ObjectStore};

/// Whether the file systems of the platform usually ignore the case of names (macOS and Windows).
/// Two paths of the new tree that differ only in case then name one file.
const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// The prefix of the temporary files that the layer writes next to their targets.
const TEMP_PREFIX: &str = ".stemma-checkout-";

/// The files of a tree, by path.
type Files = BTreeMap<String, (FileMode, BlobId)>;

/// One file that a checkout writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWrite {
    /// The path, relative to the working directory, written with `/`.
    pub path: String,
    /// The mode of the file.
    pub mode: FileMode,
    /// The content.
    pub blob: BlobId,
    /// Whether the old tree has the path, so that an existing file there is replaced and not in
    /// the way.
    pub replaces: bool,
}

/// What a checkout would do: the files to write and the files to remove. The plan is built by
/// [`Repository::plan_checkout`], which found no conflict, and is carried out by
/// [`Repository::apply_checkout`]. A dry run prints it and goes no further.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutPlan {
    writes: Vec<PlannedWrite>,
    removals: Vec<String>,
    /// Every directory that the new tree needs, so that the layer does not remove it.
    kept: BTreeSet<String>,
}

impl CheckoutPlan {
    /// The files to create or to change, in the order of their paths.
    pub fn writes(&self) -> &[PlannedWrite] {
        &self.writes
    }

    /// The files to remove, in the order of their paths.
    pub fn removals(&self) -> &[String] {
        &self.removals
    }

    /// Whether the directory already has the new tree.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.removals.is_empty()
    }
}

impl Repository {
    /// Plans the checkout that brings the working directory from the tree `from` to the tree `to`.
    /// `from` is `None` if the directory has no tracked files. Nothing is written.
    ///
    /// Every blob that the plan writes is read once to prove that the store has it, so a missing
    /// object is found before the first file is written; the apply reads it again.
    ///
    /// # Errors
    ///
    /// Returns [`CheckoutError::Reserved`] for a path with a part that is `.stemma` or `.git`,
    /// [`CheckoutError::Name`] for a name that Windows cannot hold (only on Windows),
    /// [`CheckoutError::Excluded`] for a path that `.stemmaignore` excludes,
    /// [`CheckoutError::Collision`] for two paths that name one file where the case of names is
    /// ignored, [`CheckoutError::ThroughLink`] for a path below a link,
    /// [`CheckoutError::InTheWay`] for a file or a directory that is not tracked, and
    /// [`CheckoutError::Snapshot`] for a tree or a blob that the store lacks or that is damaged,
    /// and for a file that cannot be inspected. [`CheckoutError::Ignore`] is the error of reading
    /// `.stemmaignore`.
    pub fn plan_checkout(
        &self,
        from: Option<TreeId>,
        to: TreeId,
    ) -> Result<CheckoutPlan, CheckoutError> {
        let objects = self.objects();
        let old = match from {
            Some(tree) => flatten(objects, tree)?,
            None => Files::new(),
        };
        let new = flatten(objects, to)?;
        let rules = IgnoreRules::read(self.path()).map_err(CheckoutError::Ignore)?;
        let removals: Vec<String> = old
            .keys()
            .filter(|path| !new.contains_key(*path))
            .cloned()
            .collect();
        let removed: BTreeSet<&str> = removals.iter().map(String::as_str).collect();
        let mut writes = Vec::new();
        for (path, &(mode, blob)) in &new {
            let previous = old.get(path);
            if previous == Some(&(mode, blob)) {
                continue;
            }
            check_name(path)?;
            if let Some(line) = rules.matching(path, false) {
                return Err(CheckoutError::Excluded {
                    path: path.clone(),
                    line,
                });
            }
            self.check_place(path, previous.is_some(), &removed)?;
            read_object(objects, blob.object_id(), Kind::Blob)?;
            writes.push(PlannedWrite {
                path: path.clone(),
                mode,
                blob,
                replaces: previous.is_some(),
            });
        }
        if CASE_INSENSITIVE {
            if let Some((path, other)) = case_collision(new.keys().map(String::as_str)) {
                return Err(CheckoutError::Collision {
                    path,
                    other: Some(other),
                });
            }
        }
        for path in &removals {
            // A file below a link is not the file the old tree had, and the link is not followed.
            if let Some((_, metadata)) = lookup(self.path(), path)? {
                if metadata.is_dir() {
                    return Err(CheckoutError::InTheWay { path: path.clone() });
                }
            }
        }
        Ok(CheckoutPlan {
            writes,
            removals,
            kept: directories_of(new.keys().map(String::as_str)),
        })
    }

    /// Carries out the plan: removes the files that the new tree lacks and the directories that
    /// this emptied, then writes the files of the new tree.
    ///
    /// A file is written to a temporary file in its own directory and renamed, so a reader sees
    /// the old content or the new one. A directory is removed only if it is empty and the new
    /// tree does not need it. The plan is not trusted to be current: a path that is a link, or a
    /// new file where an untracked one has appeared since, stops the call.
    ///
    /// A stop leaves the files done so far. Nothing is rolled back: the next snapshot reads what
    /// is there as an ordinary change of the actor (ADR-013). Directories are not flushed.
    ///
    /// # Errors
    ///
    /// Returns [`CheckoutError::ThroughLink`], [`CheckoutError::InTheWay`], and
    /// [`CheckoutError::Collision`] for what has changed since the plan, and
    /// [`CheckoutError::Snapshot`] for a blob that cannot be read and a file system that fails.
    pub fn apply_checkout(&self, plan: &CheckoutPlan) -> Result<(), CheckoutError> {
        let root = self.path();
        for path in &plan.removals {
            if let Some((full, metadata)) = lookup(root, path)? {
                if metadata.is_dir() {
                    return Err(CheckoutError::InTheWay { path: path.clone() });
                }
                remove_file(&full)?;
            }
        }
        // The deepest directory first, so that a directory whose last subdirectory went is empty.
        let mut emptied: BTreeSet<&str> = BTreeSet::new();
        for path in &plan.removals {
            let mut rest = path.as_str();
            while let Some((parent, _)) = rest.rsplit_once('/') {
                emptied.insert(parent);
                rest = parent;
            }
        }
        for directory in emptied.iter().rev() {
            if !plan.kept.contains(*directory) {
                // Fails if the directory holds an untracked file, which stays. Any other failure
                // leaves an empty directory, which is harmless, or is met again by the write.
                let _ = fs::remove_dir(root.join(directory));
            }
        }
        for write in &plan.writes {
            self.write_file(write)?;
        }
        Ok(())
    }

    /// Checks the place of a path that is written: no link on the way, nothing untracked in it.
    fn check_place(
        &self,
        path: &str,
        tracked: bool,
        removed: &BTreeSet<&str>,
    ) -> Result<(), CheckoutError> {
        let mut full = self.path().to_path_buf();
        let mut so_far = String::new();
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            if !so_far.is_empty() {
                so_far.push('/');
            }
            so_far.push_str(part);
            full.push(part);
            let metadata = match fs::symlink_metadata(&full) {
                Ok(metadata) => metadata,
                // Nothing here, so nothing below it either.
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(source) => return Err(io_error("inspect", &full, source)),
            };
            if parts.peek().is_some() {
                if metadata.file_type().is_symlink() {
                    return Err(CheckoutError::ThroughLink {
                        path: path.to_owned(),
                    });
                }
                if !metadata.is_dir() && !removed.contains(so_far.as_str()) {
                    return Err(CheckoutError::InTheWay { path: so_far });
                }
                // A file that the old tree has and the new one lacks makes room for a directory.
                if !metadata.is_dir() {
                    return Ok(());
                }
            } else if metadata.is_dir() {
                if !directory_is_emptied(&full, &so_far, removed)? {
                    return Err(CheckoutError::InTheWay { path: so_far });
                }
            } else if !tracked {
                return Err(CheckoutError::InTheWay { path: so_far });
            }
        }
        Ok(())
    }

    /// Writes one file through a temporary file in its directory.
    fn write_file(&self, write: &PlannedWrite) -> Result<(), CheckoutError> {
        let target = self.path().join(&write.path);
        let parent = target.parent().unwrap_or(self.path());
        self.make_directories(&write.path)?;
        let content = read_object(self.objects(), write.blob.object_id(), Kind::Blob)?;
        let temp = parent.join(format!("{TEMP_PREFIX}{}", unique_temp_name()));
        let result = (|| -> Result<(), CheckoutError> {
            create_content(&temp, write.mode, content.body())?;
            // The window between this check and the rename remains: a file that appears in it is
            // replaced (open item of ADR-013).
            if !write.replaces && fs::symlink_metadata(&target).is_ok() {
                return Err(CheckoutError::Collision {
                    path: write.path.clone(),
                    other: None,
                });
            }
            fs::rename(&temp, &target)
                .map_err(|source| io_error("rename the temporary file to", &target, source))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    /// Creates the directories above a path, one by one, and never goes through a link.
    fn make_directories(&self, path: &str) -> Result<(), CheckoutError> {
        let mut full = self.path().to_path_buf();
        let mut so_far = String::new();
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                break;
            }
            if !so_far.is_empty() {
                so_far.push('/');
            }
            so_far.push_str(part);
            full.push(part);
            match fs::symlink_metadata(&full) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(CheckoutError::ThroughLink {
                        path: path.to_owned(),
                    });
                }
                Ok(_) => return Err(CheckoutError::InTheWay { path: so_far }),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&full)
                        .map_err(|source| io_error("create the directory", &full, source))?;
                }
                Err(source) => return Err(io_error("inspect", &full, source)),
            }
        }
        Ok(())
    }
}

/// Lists the files of a tree with path, mode, and blob.
fn flatten(objects: &dyn ObjectStore, root: TreeId) -> Result<Files, SnapshotError> {
    let mut files = Files::new();
    let mut pending = vec![(String::new(), root)];
    while let Some((prefix, id)) = pending.pop() {
        let raw = read_object(objects, id.object_id(), Kind::Tree)?;
        let tree = Tree::parse(raw.body()).map_err(|source| SnapshotError::Tree { id, source })?;
        for entry in tree.entries() {
            let path = if prefix.is_empty() {
                entry.name().to_owned()
            } else {
                format!("{prefix}/{}", entry.name())
            };
            match FileMode::from_entry_mode(entry.mode()) {
                Some(mode) => {
                    files.insert(path, (mode, BlobId::new(entry.id())));
                }
                None => pending.push((path, TreeId::new(entry.id()))),
            }
        }
    }
    Ok(files)
}

/// Every directory above a path of the set.
fn directories_of<'a>(paths: impl Iterator<Item = &'a str>) -> BTreeSet<String> {
    let mut directories = BTreeSet::new();
    for path in paths {
        let mut rest = path;
        while let Some((parent, _)) = rest.rsplit_once('/') {
            if !directories.insert(parent.to_owned()) {
                break;
            }
            rest = parent;
        }
    }
    directories
}

/// Whether every file below the directory is one that the checkout removes, so that the directory
/// is empty when it is replaced by a file.
fn directory_is_emptied(
    directory: &Path,
    path: &str,
    removed: &BTreeSet<&str>,
) -> Result<bool, CheckoutError> {
    let entries = fs::read_dir(directory).map_err(|source| io_error("list", directory, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io_error("list", directory, source))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Ok(false);
        };
        let child = format!("{path}/{name}");
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|source| io_error("inspect", &entry.path(), source))?;
        let emptied = if metadata.is_dir() {
            directory_is_emptied(&entry.path(), &child, removed)?
        } else {
            removed.contains(child.as_str())
        };
        if !emptied {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Rejects a path that names the metadata of Stemma or of Git, or a name that the platform cannot
/// hold.
fn check_name(path: &str) -> Result<(), CheckoutError> {
    for part in path.split('/') {
        if is_metadata_name(part) {
            return Err(CheckoutError::Reserved {
                path: path.to_owned(),
            });
        }
        if cfg!(windows) {
            if let Some(reason) = dangerous_on_windows(part) {
                return Err(CheckoutError::Name {
                    path: path.to_owned(),
                    reason,
                });
            }
        }
    }
    Ok(())
}

/// Whether the name is `.stemma` or `.git`, as a file system that ignores case and drops trailing
/// dots and spaces would read it.
fn is_metadata_name(name: &str) -> bool {
    let trimmed = name.trim_end_matches(['.', ' ']);
    trimmed.eq_ignore_ascii_case(METADATA_DIR) || trimmed.eq_ignore_ascii_case(GIT_DIR)
}

/// Why Windows cannot hold the name as it is, or `None` if it can. The tree format allows `\` and
/// `:` in names (ADR-005), and Windows reads them as a separator and as a stream. A device name
/// stays one with an extension, as in `NUL.txt`; a name that ends in a dot or a space loses it.
fn dangerous_on_windows(name: &str) -> Option<&'static str> {
    if name.contains('\\') {
        return Some("it contains a backslash, which Windows reads as a separator");
    }
    if name.contains(':') {
        return Some("it contains a colon, which Windows reads as a data stream");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("Windows drops a trailing dot or space");
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Some("it contains a character that Windows does not allow");
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|digit| {
                matches!(
                    digit,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            });
    device.then_some("it is the name of a device")
}

/// The first two paths that differ only in case, if there are any.
fn case_collision<'a>(paths: impl Iterator<Item = &'a str>) -> Option<(String, String)> {
    let mut seen: HashMap<String, &str> = HashMap::new();
    for path in paths {
        if let Some(other) = seen.insert(path.to_lowercase(), path) {
            return Some((path.to_owned(), other.to_owned()));
        }
    }
    None
}

/// Writes the content of a file or a link to a new file.
fn create_content(path: &Path, mode: FileMode, content: &[u8]) -> Result<(), CheckoutError> {
    #[cfg(unix)]
    if mode == FileMode::Symlink {
        use std::os::unix::ffi::OsStrExt;
        return std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(content), path)
            .map_err(|source| io_error("create the link", path, source));
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| io_error("create", path, source))?;
    file.write_all(content)
        .and_then(|()| set_mode(&file, mode))
        .and_then(|()| file.sync_all())
        .map_err(|source| io_error("write", path, source))
}

/// Sets the executable bit as the tree has it. Where there is none, the mode stays as it is; a
/// link is written as a plain file there, as the snapshot reads it.
fn set_mode(file: &fs::File, mode: FileMode) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bits = if mode == FileMode::Executable {
            0o755
        } else {
            0o644
        };
        file.set_permissions(fs::Permissions::from_mode(bits))
    }
    #[cfg(not(unix))]
    {
        let _ = (file, mode);
        Ok(())
    }
}

fn remove_file(path: &Path) -> Result<(), CheckoutError> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(io_error("remove", path, error))
        }
        _ => Ok(()),
    }
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> CheckoutError {
    CheckoutError::Snapshot(SnapshotError::io(operation, path, source))
}

/// Why a checkout is refused or stops.
#[derive(Debug)]
#[non_exhaustive]
pub enum CheckoutError {
    /// A tree or a blob is missing or damaged, or a file cannot be inspected, read, or written.
    Snapshot(SnapshotError),
    /// `.stemmaignore` cannot be read.
    Ignore(IgnoreError),
    /// A path of the new tree has a part that is the metadata of Stemma or of Git.
    Reserved {
        /// The path.
        path: String,
    },
    /// A name of the new tree that the platform cannot hold.
    Name {
        /// The path.
        path: String,
        /// Why.
        reason: &'static str,
    },
    /// A path of the new tree is excluded by `.stemmaignore`.
    Excluded {
        /// The path.
        path: String,
        /// The line of the pattern.
        line: NonZeroUsize,
    },
    /// Two paths of the new tree name one file on a file system that ignores case, or a new file
    /// appeared where the plan found nothing.
    Collision {
        /// The path.
        path: String,
        /// The other path, if there is one.
        other: Option<String>,
    },
    /// A directory above the path is a link. The layer never writes through a link.
    ThroughLink {
        /// The path of the file to write.
        path: String,
    },
    /// A file or a directory that is not tracked is where the new tree needs its own, or a
    /// directory is where the old tree had a file.
    InTheWay {
        /// The path of what is in the way.
        path: String,
    },
}

impl CheckoutError {
    /// Returns the error of the file system, if that is what failed.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Snapshot(error) => error.io_error(),
            Self::Ignore(error) => error.io_error(),
            _ => None,
        }
    }
}

impl fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot(error) => write!(f, "could not write the files: {error}"),
            Self::Ignore(error) => write!(f, "could not read the exclusion file: {error}"),
            Self::Reserved { path } => write!(
                f,
                "`{}` is the metadata of Stemma or of Git and is never written",
                path.escape_debug()
            ),
            Self::Name { path, reason } => write!(
                f,
                "`{}` cannot be written on this system: {reason}",
                path.escape_debug()
            ),
            Self::Excluded { path, line } => write!(
                f,
                "`{}` is excluded by line {line} of .stemmaignore and is not written",
                path.escape_debug()
            ),
            Self::Collision {
                path,
                other: Some(other),
            } => write!(
                f,
                "`{}` and `{}` differ only in case, and this system treats them as one file",
                path.escape_debug(),
                other.escape_debug()
            ),
            Self::Collision { path, other: None } => write!(
                f,
                "`{}` appeared while the files were written; nothing was replaced there",
                path.escape_debug()
            ),
            Self::ThroughLink { path } => write!(
                f,
                "`{}` lies below a link and is not written through it",
                path.escape_debug()
            ),
            Self::InTheWay { path } => write!(
                f,
                "`{}` is not tracked and is in the way; move or remove it, or track it",
                path.escape_debug()
            ),
        }
    }
}

impl Error for CheckoutError {}

impl From<SnapshotError> for CheckoutError {
    fn from(error: SnapshotError) -> Self {
        Self::Snapshot(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Blob, TreeEntry};
    use crate::repository::InitOptions;
    use crate::testutil::TempDir;
    use crate::{Actor, AllowAll};

    fn repository(dir: &TempDir) -> Repository {
        let actor = Actor::human("daniel").unwrap();
        Repository::init(dir.path(), &InitOptions::default(), &actor, &AllowAll).unwrap()
    }

    /// Stores the tree of the files, given as path, mode, and content.
    fn tree(repository: &Repository, files: &[(&str, FileMode, &str)]) -> TreeId {
        fn build(repository: &Repository, files: &[(&str, FileMode, &str)]) -> TreeId {
            let mut entries = Vec::new();
            let mut directories: BTreeMap<&str, Vec<(&str, FileMode, &str)>> = BTreeMap::new();
            for &(path, mode, content) in files {
                match path.split_once('/') {
                    Some((directory, rest)) => {
                        directories
                            .entry(directory)
                            .or_default()
                            .push((rest, mode, content));
                    }
                    None => {
                        let blob = repository
                            .objects()
                            .put_blob(&Blob::new(content.as_bytes()))
                            .unwrap();
                        entries.push(TreeEntry::file(path, mode, blob).unwrap());
                    }
                }
            }
            for (directory, inner) in directories {
                let id = build(repository, &inner);
                entries.push(TreeEntry::directory(directory, id).unwrap());
            }
            let tree = Tree::new(entries).unwrap();
            repository.objects().put_tree(&tree).unwrap()
        }
        build(repository, files)
    }

    fn plain<'a>(path: &'a str, content: &'a str) -> (&'a str, FileMode, &'a str) {
        (path, FileMode::File, content)
    }

    fn write(dir: &TempDir, path: &str, content: &str) {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    fn read(dir: &TempDir, path: &str) -> String {
        fs::read_to_string(dir.path().join(path)).unwrap()
    }

    fn exists(dir: &TempDir, path: &str) -> bool {
        fs::symlink_metadata(dir.path().join(path)).is_ok()
    }

    /// Brings the directory to the files of `from` and checks out `to`.
    fn checkout(
        dir: &TempDir,
        repository: &Repository,
        from: &[(&str, FileMode, &str)],
        to: &[(&str, FileMode, &str)],
    ) -> Result<(), CheckoutError> {
        for (path, _, content) in from {
            write(dir, path, content);
        }
        let from = tree(repository, from);
        let to = tree(repository, to);
        let plan = repository.plan_checkout(Some(from), to)?;
        repository.apply_checkout(&plan)
    }

    #[test]
    fn files_are_added_changed_and_removed_at_depth() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        checkout(
            &dir,
            &repository,
            &[plain("a", "1"), plain("d/b", "2"), plain("d/e/c", "3")],
            &[
                plain("a", "changed"),
                plain("d/new", "4"),
                plain("x/y/z", "5"),
            ],
        )
        .unwrap();
        assert_eq!(read(&dir, "a"), "changed");
        assert_eq!(read(&dir, "d/new"), "4");
        assert_eq!(read(&dir, "x/y/z"), "5");
        assert!(!exists(&dir, "d/b"));
        assert!(
            !exists(&dir, "d/e"),
            "the directory that this emptied is removed"
        );
        assert!(
            exists(&dir, "d"),
            "a directory that the new tree needs stays"
        );
    }

    #[test]
    fn a_checkout_from_nothing_writes_every_file() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let to = tree(&repository, &[plain("a/b", "1"), plain("c", "2")]);
        let plan = repository.plan_checkout(None, to).unwrap();
        assert_eq!(plan.writes().len(), 2);
        repository.apply_checkout(&plan).unwrap();
        assert_eq!(read(&dir, "a/b"), "1");
        assert_eq!(read(&dir, "c"), "2");
    }

    #[test]
    fn the_same_tree_is_an_empty_plan() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let files = [plain("a", "1")];
        write(&dir, "a", "1");
        let id = tree(&repository, &files);
        let plan = repository.plan_checkout(Some(id), id).unwrap();
        assert!(plan.is_empty());
    }

    #[test]
    fn what_the_trees_do_not_name_is_left_alone() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "untracked", "mine");
        write(&dir, "d/untracked", "mine");
        write(&dir, ".stemmaignore", "*.log\n");
        write(&dir, "build.log", "log");
        checkout(
            &dir,
            &repository,
            &[plain("d/tracked", "1")],
            &[plain("other", "2")],
        )
        .unwrap();
        assert_eq!(read(&dir, "untracked"), "mine");
        assert_eq!(read(&dir, "d/untracked"), "mine");
        assert_eq!(read(&dir, "build.log"), "log");
        assert!(!exists(&dir, "d/tracked"));
        assert!(
            exists(&dir, "d"),
            "a directory with an untracked file stays"
        );
        assert!(repository.metadata_dir().is_dir());
    }

    #[test]
    fn an_untracked_file_in_the_way_stops_the_plan_and_nothing_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "new", "mine");
        let error = checkout(
            &dir,
            &repository,
            &[plain("a", "1")],
            &[plain("a", "2"), plain("new", "theirs")],
        )
        .unwrap_err();
        assert!(
            matches!(&error, CheckoutError::InTheWay { path } if path == "new"),
            "{error}"
        );
        assert_eq!(read(&dir, "new"), "mine");
        assert_eq!(
            read(&dir, "a"),
            "1",
            "the change that was fine is not made either"
        );
    }

    #[test]
    fn an_untracked_file_where_a_directory_is_needed_is_in_the_way() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "d", "mine");
        let error = checkout(&dir, &repository, &[], &[plain("d/f", "1")]).unwrap_err();
        assert!(
            matches!(&error, CheckoutError::InTheWay { path } if path == "d"),
            "{error}"
        );
        assert_eq!(read(&dir, "d"), "mine");
    }

    #[test]
    fn an_untracked_file_in_a_directory_that_becomes_a_file_is_in_the_way() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "d/untracked", "mine");
        let error = checkout(
            &dir,
            &repository,
            &[plain("d/f", "1")],
            &[plain("d", "now a file")],
        )
        .unwrap_err();
        assert!(
            matches!(&error, CheckoutError::InTheWay { path } if path == "d"),
            "{error}"
        );
        assert_eq!(read(&dir, "d/f"), "1");
        assert_eq!(read(&dir, "d/untracked"), "mine");
    }

    #[test]
    fn a_file_and_a_directory_change_places() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        checkout(
            &dir,
            &repository,
            &[plain("a", "file"), plain("b/c", "inside")],
            &[plain("a/x", "inside"), plain("b", "file")],
        )
        .unwrap();
        assert_eq!(read(&dir, "a/x"), "inside");
        assert_eq!(read(&dir, "b"), "file");
    }

    #[test]
    fn a_path_that_is_excluded_is_not_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, ".stemmaignore", "secret/\n");
        let error = checkout(&dir, &repository, &[], &[plain("secret/key", "1")]).unwrap_err();
        assert!(
            matches!(&error, CheckoutError::Excluded { path, line } if path == "secret/key" && line.get() == 1),
            "{error}"
        );
        assert!(!exists(&dir, "secret"));
    }

    #[test]
    fn the_metadata_of_stemma_and_git_is_never_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        for path in [".git/config", "x/.GIT/config", "x/.git. /f"] {
            let to = tree(&repository, &[plain(path, "1")]);
            let error = repository.plan_checkout(None, to).unwrap_err();
            assert!(
                matches!(error, CheckoutError::Reserved { .. }),
                "{path}: {error}"
            );
        }
        assert!(!exists(&dir, ".git"));
    }

    #[test]
    fn a_blob_that_the_store_lacks_stops_the_plan_before_any_write() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let present = tree(&repository, &[plain("a", "1")]);
        let absent = Blob::new("stored nowhere").id();
        let entries = vec![
            TreeEntry::file("lost", FileMode::File, absent).unwrap(),
            TreeEntry::directory("sub", present).unwrap(),
        ];
        let to = repository
            .objects()
            .put_tree(&Tree::new(entries).unwrap())
            .unwrap();
        let error = repository.plan_checkout(None, to).unwrap_err();
        assert!(
            matches!(
                error,
                CheckoutError::Snapshot(SnapshotError::Missing { .. })
            ),
            "{error}"
        );
        assert!(!exists(&dir, "sub"));
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_bit_and_a_link_are_written() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let repository = repository(&dir);
        checkout(
            &dir,
            &repository,
            &[],
            &[
                ("run", FileMode::Executable, "#!/bin/sh\n"),
                ("plain", FileMode::File, "x"),
                ("link", FileMode::Symlink, "plain"),
            ],
        )
        .unwrap();
        let mode = |path: &str| {
            fs::metadata(dir.path().join(path))
                .unwrap()
                .permissions()
                .mode()
        };
        assert_eq!(mode("run") & 0o111, 0o111);
        assert_eq!(mode("plain") & 0o111, 0);
        assert_eq!(
            fs::read_link(dir.path().join("link")).unwrap(),
            Path::new("plain")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_changed_link_is_replaced_and_not_followed() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "target", "keep");
        std::os::unix::fs::symlink("target", dir.path().join("link")).unwrap();
        let from = tree(&repository, &[("link", FileMode::Symlink, "target")]);
        let to = tree(&repository, &[plain("link", "now a file")]);
        let plan = repository.plan_checkout(Some(from), to).unwrap();
        repository.apply_checkout(&plan).unwrap();
        assert_eq!(read(&dir, "link"), "now a file");
        assert_eq!(read(&dir, "target"), "keep");
    }

    #[cfg(unix)]
    #[test]
    fn nothing_is_written_or_removed_through_a_link() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let repository = repository(&dir);
        write(&outside, "f", "outside");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("d")).unwrap();
        let error = checkout(&dir, &repository, &[], &[plain("d/new", "1")]).unwrap_err();
        assert!(
            matches!(error, CheckoutError::ThroughLink { .. }),
            "{error}"
        );
        assert!(!exists(&outside, "new"));
        // The old tree names `d/f`, which is below the link now: it is not the file, so not removed.
        let from = tree(&repository, &[plain("d/f", "outside")]);
        let to = tree(&repository, &[plain("other", "1")]);
        let plan = repository.plan_checkout(Some(from), to).unwrap();
        repository.apply_checkout(&plan).unwrap();
        assert_eq!(read(&outside, "f"), "outside");
        assert_eq!(read(&dir, "other"), "1");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_appears_after_the_plan_stops_the_apply() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let repository = repository(&dir);
        let to = tree(&repository, &[plain("d/new", "1")]);
        let plan = repository.plan_checkout(None, to).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("d")).unwrap();
        let error = repository.apply_checkout(&plan).unwrap_err();
        assert!(
            matches!(error, CheckoutError::ThroughLink { .. }),
            "{error}"
        );
        assert!(!exists(&outside, "new"));
    }

    #[test]
    fn a_file_that_appears_after_the_plan_is_not_replaced() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let to = tree(&repository, &[plain("first", "1"), plain("new", "theirs")]);
        let plan = repository.plan_checkout(None, to).unwrap();
        write(&dir, "new", "mine");
        let error = repository.apply_checkout(&plan).unwrap_err();
        assert!(matches!(&error, CheckoutError::Collision { path, other: None } if path == "new"));
        assert_eq!(read(&dir, "new"), "mine");
        assert_eq!(read(&dir, "first"), "1", "what was done stays (ADR-013)");
        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .count();
        assert_eq!(leftovers, 0, "the temporary file is removed");
    }

    #[test]
    fn two_paths_that_differ_only_in_case_collide() {
        assert_eq!(
            case_collision(["a/B", "a/b", "c"].into_iter()),
            Some(("a/b".to_owned(), "a/B".to_owned()))
        );
        assert_eq!(case_collision(["a", "b"].into_iter()), None);
    }

    #[test]
    fn the_names_that_windows_cannot_hold_are_found() {
        for name in [
            "a\\b", "a:b", "a:stream", "nul", "NUL.txt", "Com1", "lpt9.log", "a.", "a ", "a<b",
            "q?",
        ] {
            assert!(dangerous_on_windows(name).is_some(), "{name}");
        }
        for name in ["a", "console", "com10", "com0", "nulls", "a.b", ".hidden"] {
            assert_eq!(dangerous_on_windows(name), None, "{name}");
        }
    }

    #[test]
    fn the_names_of_the_metadata_are_found_as_a_file_system_reads_them() {
        for name in [
            ".git", ".GIT", ".git.", ".git ", ".stemma", ".Stemma.", ".stemma ",
        ] {
            assert!(is_metadata_name(name), "{name}");
        }
        for name in [".gitignore", ".github", "git", ".stemmaignore"] {
            assert!(!is_metadata_name(name), "{name}");
        }
    }

    #[test]
    fn a_directory_in_the_place_of_a_removed_file_is_in_the_way() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        fs::create_dir(dir.path().join("a")).unwrap();
        let from = tree(&repository, &[plain("a", "1")]);
        let to = tree(&repository, &[plain("b", "2")]);
        let error = repository.plan_checkout(Some(from), to).unwrap_err();
        assert!(
            matches!(&error, CheckoutError::InTheWay { path } if path == "a"),
            "{error}"
        );
    }
}
