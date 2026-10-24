//! The checkout layer: writing a tree into the working directory (ADR-013, ADR-005).
//!
//! [`Repository::plan_checkout`] compares the tree the working directory has with the tree it
//! should have and finds every conflict it can see before a file is touched.
//! [`Repository::apply_checkout`] then removes, creates, and replaces files. The layer has no
//! command of its own: it is meant for the restore (ADR-013) and later for the checkout of a
//! branch.
//!
//! Only a path that the old or the new tree names is changed. The content of a file that is not
//! tracked is never read, written, or removed; the layer only looks at what is in the place of a
//! path it needs. A path of the new tree that is new, and that `.stemmaignore` excludes, is an
//! error, as is any path with a part that names `.stemma` or `.git` that the checkout changes or
//! removes, in the old tree or in the new one. A file that the old tree tracks is changed or
//! removed even if `.stemmaignore` excludes it now (ADR-012: tracked stays tracked). The caller
//! starts with a snapshot, so that the files of the old tree are what the directory holds; the
//! layer does not compare their content.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use super::snapshot::{lookup, read_object, SnapshotError};
use super::Repository;
use crate::ignore::{IgnoreError, IgnoreRules};
use crate::object::{BlobId, FileMode, Kind, Tree, TreeId, GIT_DIR, METADATA_DIR};
use crate::store::{unique_temp_name, ObjectStore};

/// Whether the file systems of the platform usually ignore the case of names (macOS and Windows).
/// Two paths of the new tree that differ only in case then name one file.
const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// The prefix of the temporary files that the layer writes next to their targets.
use crate::store::CHECKOUT_TEMP_PREFIX as TEMP_PREFIX;

/// The files of a tree, by path.
type Files = BTreeMap<String, (FileMode, BlobId)>;

/// One file that a checkout writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWrite {
    path: String,
    mode: FileMode,
    blob: BlobId,
    replaces: bool,
}

impl PlannedWrite {
    /// The path, relative to the working directory, written with `/`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The mode of the file.
    pub fn mode(&self) -> FileMode {
        self.mode
    }

    /// The content.
    pub fn blob(&self) -> BlobId {
        self.blob
    }

    /// Whether the old tree has the path, so that an existing file there is replaced and not in
    /// the way.
    pub fn replaces(&self) -> bool {
        self.replaces
    }
}

/// What a checkout would do: the files to write and the files to remove. The plan is built by
/// [`Repository::plan_checkout`], which found no conflict, and is carried out once by
/// [`Repository::apply_checkout`] on the repository it was built for. A dry run could print it and
/// go no further.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutPlan {
    /// The working directory the plan was made for.
    root: PathBuf,
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

    /// Whether the old tree and the new one name the same files, so that there is nothing to do.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.removals.is_empty()
    }
}

/// The files that a checkout removes, to ask whether a path is one of them.
struct Removed {
    exact: BTreeSet<String>,
    /// The same paths in lower case, used where the file system ignores the case of names: the
    /// new tree may name a file `readme` where the old one had `README`.
    folded: BTreeSet<String>,
}

impl Removed {
    fn new(paths: &[String]) -> Self {
        Self {
            exact: paths.iter().cloned().collect(),
            folded: paths.iter().map(|path| path.to_lowercase()).collect(),
        }
    }

    fn contains(&self, path: &str) -> bool {
        self.exact.contains(path)
            || (CASE_INSENSITIVE && self.folded.contains(&path.to_lowercase()))
    }
}

/// How far an apply got, to tell the caller what a stop left behind.
#[derive(Default)]
struct Progress {
    removed: usize,
    written: usize,
}

impl Repository {
    /// Plans the checkout that brings the working directory from the tree `from` to the tree `to`.
    /// `from` is `None` if the directory has no tracked files. The call does not change the
    /// working directory.
    ///
    /// Every blob that the plan writes is read once to prove that the store has it, so a missing
    /// object is found before the first file is written; the apply reads it again.
    ///
    /// # Errors
    ///
    /// Returns [`CheckoutError::Reserved`] for a path of either tree with a part that is `.stemma`
    /// or `.git` or an alias of one (see `is_metadata_name`), [`CheckoutError::Name`] for a name
    /// that the layer knows Windows cannot hold (only on Windows), [`CheckoutError::Excluded`] for
    /// a new path that `.stemmaignore` excludes, [`CheckoutError::CaseCollision`] for two paths
    /// that name one file where the case of names is ignored, [`CheckoutError::ThroughLink`] for a
    /// path below a link, [`CheckoutError::InTheWay`] for a file or a directory that is not
    /// tracked, and [`CheckoutError::Snapshot`] for a tree or a blob that the store lacks or that
    /// is damaged, and for a file that cannot be inspected. [`CheckoutError::Ignore`] is the error
    /// of reading `.stemmaignore`.
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
        for path in &removals {
            // The old tree may be one that was imported: it must not reach into the metadata.
            check_metadata(path)?;
        }
        let removed = Removed::new(&removals);
        let mut writes = Vec::new();
        for (path, &(mode, blob)) in &new {
            let previous = old.get(path);
            if previous == Some(&(mode, blob)) {
                continue;
            }
            check_name(path)?;
            // A file that the old tree tracks is changed even if a pattern excludes it now.
            if previous.is_none() {
                if let Some(line) = rules.matching(path, false) {
                    return Err(CheckoutError::Excluded {
                        path: path.clone(),
                        line,
                    });
                }
            }
            // A file that is there already with this content and mode is done: this lets a
            // restore that stopped after the files be run again.
            if previous.is_none()
                && !removed.contains(path)
                && self.is_there_already(path, mode, blob)?
            {
                continue;
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
            if let Some((path, other)) = case_collision(&entries_of(&new)) {
                return Err(CheckoutError::CaseCollision { path, other });
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
            root: self.path().to_path_buf(),
            writes,
            removals,
            kept: directories_of(new.keys().map(String::as_str)),
        })
    }

    /// Carries out the plan: removes the files that the new tree lacks and the directories that
    /// this emptied, then writes the files of the new tree. The removals come first because a new
    /// file or directory may take the place of an old one.
    ///
    /// A file is written to a temporary file in its own directory and renamed, so a reader sees
    /// the old content or the new one. A directory is removed only if it is empty and the new
    /// tree does not need it. A directory in the place of a new file is removed if it holds no
    /// file. The plan is not trusted to be current: a directory above a file to write that is a
    /// link now, and a file that has appeared since the plan where a new file goes, stop the call.
    /// A file to remove that lies below a link is skipped, as it is not the file of the old tree,
    /// and so is one that is gone. `.stemmaignore` is not read again.
    ///
    /// A stop leaves the files done so far. Nothing is rolled back: the next snapshot reads what
    /// is there as an ordinary change of the actor (ADR-013). A stop after the first file was
    /// removed or written is reported as [`CheckoutError::Interrupted`], which says how many files
    /// were removed and written; a stop before that, after directories were created or removed,
    /// is reported as it is. A killed process can leave a temporary file `.stemma-checkout-*` next to its
    /// target. Directories are not flushed.
    ///
    /// # Errors
    ///
    /// Returns [`CheckoutError::ForeignPlan`] if the plan was made for another working directory,
    /// before anything is touched, [`CheckoutError::ThroughLink`], [`CheckoutError::InTheWay`], and
    /// [`CheckoutError::ChangedSincePlan`] for what has changed since the plan, and
    /// [`CheckoutError::Snapshot`] for a blob that cannot be read and a file system that fails,
    /// all of them as [`CheckoutError::Interrupted`] once a file was removed or written.
    pub fn apply_checkout(&self, plan: &CheckoutPlan) -> Result<(), CheckoutError> {
        if plan.root != self.path() {
            return Err(CheckoutError::ForeignPlan {
                made_for: plan.root.clone(),
            });
        }
        let mut progress = Progress::default();
        self.apply_steps(plan, &mut progress).map_err(|source| {
            if progress.removed + progress.written == 0 {
                source
            } else {
                CheckoutError::Interrupted {
                    removed: progress.removed,
                    written: progress.written,
                    source: Box::new(source),
                }
            }
        })
    }

    fn apply_steps(
        &self,
        plan: &CheckoutPlan,
        progress: &mut Progress,
    ) -> Result<(), CheckoutError> {
        let root = self.path();
        for path in &plan.removals {
            if let Some((full, metadata)) = lookup(root, path)? {
                if metadata.is_dir() {
                    return Err(CheckoutError::InTheWay { path: path.clone() });
                }
                remove_file(&full)?;
                progress.removed += 1;
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
                remove_directory_if_empty(root, directory)?;
            }
        }
        for write in &plan.writes {
            self.write_file(write)?;
            progress.written += 1;
        }
        Ok(())
    }

    /// Whether the working directory has the file already, with the content and the mode of the
    /// tree. A link on the way or a different kind of file is not a match.
    fn is_there_already(
        &self,
        path: &str,
        mode: FileMode,
        blob: BlobId,
    ) -> Result<bool, CheckoutError> {
        let Some((full, metadata)) = lookup(self.path(), path)? else {
            return Ok(false);
        };
        // The file system may read another spelling of the name as this file (case, a form of
        // Unicode normalization, a short name). Only a file under exactly this name counts as
        // done, so that no alias is taken for a file that a removal of the old tree deletes.
        let exact = full
            .parent()
            .zip(full.file_name())
            .map(|(parent, name)| {
                fs::read_dir(parent).map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .any(|entry| entry.file_name() == name)
                })
            })
            .transpose()
            .map_err(|source| io_error("list", &full, source))?
            .unwrap_or(false);
        if !exact {
            return Ok(false);
        }
        let expected = read_object(self.objects(), blob.object_id(), Kind::Blob)?;
        if mode == FileMode::Symlink {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                return Ok(metadata.file_type().is_symlink()
                    && fs::read_link(&full)
                        .is_ok_and(|target| target.as_os_str().as_bytes() == expected.body()));
            }
        }
        if !metadata.is_file() {
            return Ok(false);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = metadata.permissions().mode() & 0o111 != 0;
            if executable != (mode == FileMode::Executable) {
                return Ok(false);
            }
        }
        Ok(fs::read(&full).is_ok_and(|content| content == expected.body()))
    }

    /// Checks the place of a path that is written: no link on the way, nothing untracked in it.
    fn check_place(
        &self,
        path: &str,
        tracked: bool,
        removed: &Removed,
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
                // A file or a link that the old tree has and the new one lacks makes room for a
                // directory, as the removals come first.
                if !metadata.is_dir() && removed.contains(&so_far) {
                    return Ok(());
                }
                if metadata.file_type().is_symlink() {
                    return Err(CheckoutError::ThroughLink {
                        path: path.to_owned(),
                    });
                }
                if !metadata.is_dir() {
                    return Err(CheckoutError::InTheWay { path: so_far });
                }
            } else if metadata.is_dir() {
                if let Some(blocker) = blocker(&full, &so_far, removed)? {
                    return Err(CheckoutError::InTheWay { path: blocker });
                }
            } else if !tracked && !removed.contains(&so_far) {
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
        // The plan accepted a directory in the place of the file only if it holds no file.
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.is_dir() => remove_empty_directories(&target, &write.path)?,
            Ok(_) | Err(_) => {}
        }
        let content = read_object(self.objects(), write.blob.object_id(), Kind::Blob)?;
        let temp = parent.join(format!("{TEMP_PREFIX}{}", unique_temp_name()));
        let _cleanup = TempFile(&temp);
        create_content(&temp, write.mode, content.body())?;
        // The window between this check and the rename remains: a file that appears in it is
        // replaced (open item of ADR-013).
        match fs::symlink_metadata(&target) {
            Ok(metadata) if !write.replaces || metadata.is_dir() => {
                return Err(CheckoutError::ChangedSincePlan {
                    path: write.path.clone(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error("inspect", &target, source)),
        }
        fs::rename(&temp, &target)
            .map_err(|source| io_error("rename the temporary file to", &target, source))
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

/// A temporary file that is removed when it goes out of scope, also on a panic. It is gone after
/// a rename, and a failed removal is not reported: nothing can be done about it at that point.
struct TempFile<'a>(&'a Path);

impl Drop for TempFile<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
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

/// The first file or link below the directory that the checkout does not remove, or `None` if the
/// directory holds nothing but directories and files that go. It is the thing in the way when a
/// file takes the place of the directory. An empty directory does not block: the apply removes it.
fn blocker(
    directory: &Path,
    path: &str,
    removed: &Removed,
) -> Result<Option<String>, CheckoutError> {
    let entries = fs::read_dir(directory).map_err(|source| io_error("list", directory, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io_error("list", directory, source))?;
        let child = format!("{path}/{}", entry.file_name().to_string_lossy());
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|source| io_error("inspect", &entry.path(), source))?;
        if metadata.is_dir() {
            if let Some(found) = blocker(&entry.path(), &child, removed)? {
                return Ok(Some(found));
            }
        } else if !removed.contains(&child) {
            return Ok(Some(child));
        }
    }
    Ok(None)
}

/// Removes the directory and the directories below it, which must hold no file.
fn remove_empty_directories(directory: &Path, path: &str) -> Result<(), CheckoutError> {
    let entries = fs::read_dir(directory).map_err(|source| io_error("list", directory, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io_error("list", directory, source))?;
        let child = format!("{path}/{}", entry.file_name().to_string_lossy());
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|source| io_error("inspect", &entry.path(), source))?;
        if !metadata.is_dir() {
            return Err(CheckoutError::InTheWay { path: child });
        }
        remove_empty_directories(&entry.path(), &child)?;
    }
    fs::remove_dir(directory).map_err(|source| io_error("remove the directory", directory, source))
}

/// Removes a directory of the old tree if it is empty. It walks down from the root one part at a
/// time and stops at anything that is not a directory, so that a link on the way is not followed.
/// A directory that holds an untracked file stays, and that is no error.
fn remove_directory_if_empty(root: &Path, directory: &str) -> Result<(), CheckoutError> {
    let mut full = root.to_path_buf();
    for part in directory.split('/') {
        full.push(part);
        match fs::symlink_metadata(&full) {
            Ok(metadata) if metadata.is_dir() => {}
            _ => return Ok(()),
        }
    }
    match fs::remove_dir(&full) {
        Ok(()) => Ok(()),
        Err(source) => match fs::read_dir(&full) {
            Ok(entries) => {
                if entries.count() > 0 {
                    Ok(())
                } else {
                    Err(io_error("remove the directory", &full, source))
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(io_error("remove the directory", &full, source)),
        },
    }
}

/// Rejects a path that names the metadata of Stemma or of Git, or a name that the platform cannot
/// hold.
fn check_name(path: &str) -> Result<(), CheckoutError> {
    check_metadata(path)?;
    for part in path.split('/') {
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

/// Rejects a path with a part that names the metadata of Stemma or of Git.
fn check_metadata(path: &str) -> Result<(), CheckoutError> {
    if path.split('/').any(is_metadata_name) {
        return Err(CheckoutError::Reserved {
            path: path.to_owned(),
        });
    }
    Ok(())
}

/// Whether the name is `.stemma` or `.git`, or an alias of one on a common file system. It reads
/// the name as Windows does (the same rules as the reserved name of ADR-005): a backslash
/// separates parts, a colon starts a stream that is dropped, trailing dots and spaces are
/// dropped, case is folded by Unicode uppercase (which catches the long s), and the short names
/// `STEMMA~N` and `GIT~N` count as well.
fn is_metadata_name(name: &str) -> bool {
    name.split('\\').any(|segment| {
        let file_name = segment.split(':').next().unwrap_or(segment);
        let folded = file_name.trim_end_matches(['.', ' ']).to_uppercase();
        [METADATA_DIR, GIT_DIR].iter().any(|metadata| {
            let metadata = metadata.to_uppercase();
            let short = format!("{}~", metadata.trim_start_matches('.'));
            folded == metadata
                || folded.strip_prefix(&short).is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
                })
        })
    })
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

/// The files of a tree and every directory above them.
fn entries_of(files: &Files) -> BTreeSet<String> {
    let mut entries = directories_of(files.keys().map(String::as_str));
    entries.extend(files.keys().cloned());
    entries
}

/// The first two entries that differ only in case, if there are any. A file `A` and a directory
/// `a` collide as two files do, and so do `A/x` and `a/y`, which share the directory.
fn case_collision(entries: &BTreeSet<String>) -> Option<(String, String)> {
    let mut seen: HashMap<String, &str> = HashMap::new();
    for path in entries {
        if let Some(other) = seen.insert(path.to_lowercase(), path) {
            return Some((path.clone(), other.to_owned()));
        }
    }
    None
}

/// Writes the content of a file or a link to a new path.
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

/// Sets the permissions to 0o755 for an executable file and to 0o644 for any other, whatever the
/// umask says. Where there is no executable bit, the mode stays as it is; a
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
    /// A path has a part that is the metadata of Stemma or of Git, or an alias of one (case,
    /// trailing dots and spaces, a short name `GIT~1`, a stream, a backslash).
    Reserved {
        /// The path.
        path: String,
    },
    /// A name of the new tree that the layer knows Windows cannot hold.
    Name {
        /// The path.
        path: String,
        /// Why.
        reason: &'static str,
    },
    /// A new path of the new tree is excluded by `.stemmaignore`.
    Excluded {
        /// The path.
        path: String,
        /// The line of the pattern.
        line: NonZeroUsize,
    },
    /// Two entries of the new tree name one file on a file system that ignores case.
    CaseCollision {
        /// The path.
        path: String,
        /// The other path.
        other: String,
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
    /// A file appeared, or a directory took the place of a file, since the plan was made. It also
    /// reports the second of two names that the file system treats as one (for example two
    /// normalization forms of one name).
    ChangedSincePlan {
        /// The path.
        path: String,
    },
    /// The plan was made for another working directory. Nothing was touched.
    ForeignPlan {
        /// The directory the plan was made for.
        made_for: PathBuf,
    },
    /// The apply stopped after it had changed the directory. The files named are done, and the
    /// rest is not; nothing is rolled back.
    Interrupted {
        /// How many files were removed.
        removed: usize,
        /// How many files were written.
        written: usize,
        /// Why it stopped.
        source: Box<CheckoutError>,
    },
}

impl CheckoutError {
    /// Returns the error of the file system, if that is what failed.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Snapshot(error) => error.io_error(),
            Self::Ignore(error) => error.io_error(),
            Self::Interrupted { source, .. } => source.io_error(),
            _ => None,
        }
    }
}

impl fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot(error) => write!(f, "could not read or write the files: {error}"),
            Self::Ignore(error) => write!(f, "could not read the exclusion file: {error}"),
            Self::Reserved { path } => write!(
                f,
                "`{}` is the metadata of Stemma or of Git and is never touched",
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
            Self::CaseCollision { path, other } => write!(
                f,
                "`{}` and `{}` differ only in case, and this system treats them as one file",
                path.escape_debug(),
                other.escape_debug()
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
            Self::ChangedSincePlan { path } => write!(
                f,
                "`{}` changed after the plan was made, or another file already has its name; \
                 nothing was replaced there",
                path.escape_debug()
            ),
            Self::ForeignPlan { made_for } => write!(
                f,
                "the plan was made for the directory `{}` and is not applied to another one",
                made_for.display()
            ),
            Self::Interrupted {
                removed,
                written,
                source,
            } => write!(
                f,
                "{source}; it stopped after {removed} files were removed and {written} written, \
                 and nothing is rolled back"
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

    /// Every path below the directory, files and directories, sorted.
    fn listing(dir: &TempDir) -> Vec<String> {
        fn walk(base: &Path, prefix: &str, out: &mut Vec<String>) {
            for entry in fs::read_dir(base).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                if name == ".stemma" {
                    continue;
                }
                out.push(path.clone());
                if entry.file_type().unwrap().is_dir() {
                    walk(&entry.path(), &path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(dir.path(), "", &mut out);
        out.sort();
        out
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
            matches!(&error, CheckoutError::InTheWay { path } if path == "d/untracked"),
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
    fn the_metadata_of_git_is_never_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        for path in [".git/config", "x/.GIT/config", "x/.git. /f", "GIT~1/y"] {
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
        assert_eq!(listing(&dir), Vec::<String>::new(), "a plan writes nothing");
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
        let CheckoutError::Interrupted {
            removed,
            written,
            source,
        } = &error
        else {
            panic!("expected an interrupted checkout: {error}");
        };
        assert_eq!((*removed, *written), (0, 1));
        assert!(
            matches!(&**source, CheckoutError::ChangedSincePlan { path } if path == "new"),
            "{error}"
        );
        assert!(error
            .to_string()
            .contains("0 files were removed and 1 written"));
        assert_eq!(read(&dir, "new"), "mine");
        assert_eq!(read(&dir, "first"), "1", "what was done stays (ADR-013)");
        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .count();
        assert_eq!(leftovers, 0, "the temporary file is removed");
    }

    fn entries(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn two_entries_that_differ_only_in_case_collide() {
        assert_eq!(
            case_collision(&entries(&["a/B", "a/b", "c"])),
            Some(("a/b".to_owned(), "a/B".to_owned()))
        );
        assert_eq!(case_collision(&entries(&["a", "b"])), None);
        // The same directory in two spellings, and a file against a directory.
        let files: Files = ["A/x", "a/y"]
            .into_iter()
            .map(|path| (path.to_owned(), (FileMode::File, Blob::new("").id())))
            .collect();
        assert!(case_collision(&entries_of(&files)).is_some());
        let files: Files = ["A", "a/y"]
            .into_iter()
            .map(|path| (path.to_owned(), (FileMode::File, Blob::new("").id())))
            .collect();
        assert!(case_collision(&entries_of(&files)).is_some());
    }

    #[test]
    fn a_plan_with_a_case_collision_is_refused_where_the_case_is_ignored() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let to = tree(&repository, &[plain("A/x", "1"), plain("a/y", "2")]);
        let result = repository.plan_checkout(None, to);
        if CASE_INSENSITIVE {
            assert!(matches!(result, Err(CheckoutError::CaseCollision { .. })));
        } else {
            assert!(result.is_ok());
        }
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
            ".git",
            ".GIT",
            ".git.",
            ".git ",
            ".stemma",
            ".Stemma.",
            ".stemma ",
            "GIT~1",
            "git~12.",
            "STEMMA~1",
            "x\\.git\\y",
            ".git:stream",
            ".stemma::$INDEX_ALLOCATION",
        ] {
            assert!(is_metadata_name(name), "{name}");
        }
        for name in [
            ".gitignore",
            ".github",
            "git",
            ".stemmaignore",
            "GIT~",
            "git~a",
            "a:.git",
        ] {
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

    #[test]
    fn a_removal_inside_the_metadata_is_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        for path in [".git/config", ".GIT/config", "GIT~1/config"] {
            write(&dir, path.replace("GIT~1", ".git").as_str(), "keep");
            let from = tree(&repository, &[plain(path, "keep"), plain("a", "1")]);
            let to = tree(&repository, &[plain("a", "1")]);
            let error = repository.plan_checkout(Some(from), to).unwrap_err();
            assert!(
                matches!(error, CheckoutError::Reserved { .. }),
                "{path}: {error}"
            );
        }
        assert_eq!(read(&dir, ".git/config"), "keep");
    }

    #[test]
    fn a_tracked_file_is_changed_and_removed_although_a_pattern_excludes_it_now() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, ".stemmaignore", "*.log\nbuild/\n");
        checkout(
            &dir,
            &repository,
            &[
                plain("a.log", "1"),
                plain("build/out", "2"),
                plain("keep", "k"),
            ],
            &[plain("a.log", "changed"), plain("keep", "k")],
        )
        .unwrap();
        assert_eq!(read(&dir, "a.log"), "changed");
        assert!(!exists(&dir, "build/out"));
    }

    #[test]
    fn a_plan_lists_exactly_what_it_will_do() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "gone", "1");
        write(&dir, "same", "s");
        write(&dir, "changed", "old");
        let from = tree(
            &repository,
            &[
                plain("changed", "old"),
                plain("gone", "1"),
                plain("same", "s"),
            ],
        );
        let to = tree(
            &repository,
            &[
                plain("changed", "new"),
                plain("fresh/f", "2"),
                plain("same", "s"),
            ],
        );
        let plan = repository.plan_checkout(Some(from), to).unwrap();
        let writes: Vec<(&str, bool)> = plan
            .writes()
            .iter()
            .map(|write| (write.path(), write.replaces()))
            .collect();
        assert_eq!(writes, [("changed", true), ("fresh/f", false)]);
        assert_eq!(plan.removals(), ["gone".to_owned()]);
        assert!(!plan.is_empty());
        assert_eq!(read(&dir, "gone"), "1", "a plan changes nothing");
        assert_eq!(read(&dir, "changed"), "old");
    }

    #[test]
    fn only_the_mode_changes() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let from = tree(&repository, &[("f", FileMode::File, "x")]);
        let to = tree(&repository, &[("f", FileMode::Executable, "x")]);
        write(&dir, "f", "x");
        let plan = repository.plan_checkout(Some(from), to).unwrap();
        assert_eq!(plan.writes().len(), 1);
        assert!(plan.writes()[0].replaces());
        repository.apply_checkout(&plan).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join("f"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
    }

    #[test]
    fn a_name_in_another_case_replaces_the_old_one() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        checkout(
            &dir,
            &repository,
            &[plain("README", "1"), plain("Docs/a", "2")],
            &[plain("readme", "1"), plain("docs/a", "2")],
        )
        .unwrap();
        assert_eq!(read(&dir, "readme"), "1");
        assert_eq!(read(&dir, "docs/a"), "2");
        assert!(
            listing(&dir)
                .iter()
                .all(|path| path != "README" || CASE_INSENSITIVE),
            "{:?}",
            listing(&dir)
        );
    }

    #[test]
    fn an_empty_untracked_directory_does_not_stop_a_file_from_taking_its_place() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        fs::create_dir_all(dir.path().join("b/cache/deep")).unwrap();
        checkout(
            &dir,
            &repository,
            &[plain("b/c", "1")],
            &[plain("b", "now a file")],
        )
        .unwrap();
        assert_eq!(read(&dir, "b"), "now a file");
    }

    #[test]
    fn an_untracked_file_deep_in_the_way_is_named() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "b/cache/x", "mine");
        let error =
            checkout(&dir, &repository, &[plain("b/c", "1")], &[plain("b", "f")]).unwrap_err();
        assert!(
            matches!(&error, CheckoutError::InTheWay { path } if path == "b/cache/x"),
            "{error}"
        );
        assert_eq!(read(&dir, "b/c"), "1", "nothing was removed");
    }

    #[test]
    fn a_plan_is_not_applied_to_another_working_directory() {
        let (dir, other) = (TempDir::new(), TempDir::new());
        let (repository, foreign) = (repository(&dir), repository(&other));
        let to = tree(&repository, &[plain("a", "1")]);
        let plan = repository.plan_checkout(None, to).unwrap();
        // The other repository needs the objects only if the check failed.
        let error = foreign.apply_checkout(&plan).unwrap_err();
        assert!(
            matches!(error, CheckoutError::ForeignPlan { .. }),
            "{error}"
        );
        assert!(!exists(&other, "a"));
        repository.apply_checkout(&plan).unwrap();
        assert_eq!(read(&dir, "a"), "1");
    }

    #[test]
    fn an_untracked_file_with_the_content_and_mode_of_the_tree_is_done_already() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "same", "1");
        write(&dir, "other", "mine");
        let to = tree(&repository, &[plain("same", "1"), plain("other", "theirs")]);
        // `other` has other content and is in the way; `same` alone would be fine.
        let error = repository.plan_checkout(None, to).unwrap_err();
        assert!(
            matches!(&error, CheckoutError::InTheWay { path } if path == "other"),
            "{error}"
        );
        let to = tree(&repository, &[plain("same", "1")]);
        let plan = repository.plan_checkout(None, to).unwrap();
        assert!(plan.is_empty(), "{plan:?}");
        assert_eq!(read(&dir, "same"), "1");
    }

    #[test]
    fn a_file_that_the_new_tree_renames_by_case_is_not_taken_for_done() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let files = [plain("README", "1")];
        write(&dir, "README", "1");
        let from = tree(&repository, &files);
        let to = tree(&repository, &[plain("readme", "1")]);
        let plan = repository.plan_checkout(Some(from), to).unwrap();
        repository.apply_checkout(&plan).unwrap();
        assert_eq!(read(&dir, "readme"), "1");
    }

    #[test]
    fn a_name_in_another_normalization_form_is_never_taken_for_done() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let (decomposed, composed) = ("cafe\u{301}.txt", "caf\u{e9}.txt");
        write(&dir, decomposed, "1");
        let from = tree(&repository, &[plain(decomposed, "1")]);
        let to = tree(&repository, &[plain(composed, "1")]);
        // Where the file system reads both names as one file, the plan must refuse or the apply
        // must leave a file; it must never report success and have deleted the file.
        if let Ok(plan) = repository.plan_checkout(Some(from), to) {
            repository.apply_checkout(&plan).unwrap();
            assert_eq!(read(&dir, composed), "1");
        }
    }

    #[test]
    fn the_text_of_an_error_escapes_the_path() {
        let error = CheckoutError::InTheWay {
            path: "a\nb".to_owned(),
        };
        assert!(!error.to_string().contains('\n'));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_the_place_of_a_tracked_file_is_replaced_and_its_target_is_not_written() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let repository = repository(&dir);
        write(&outside, "secret", "outside");
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("f")).unwrap();
        let from = tree(&repository, &[plain("f", "x")]);
        let to = tree(&repository, &[plain("f", "y")]);
        let plan = repository.plan_checkout(Some(from), to).unwrap();
        repository.apply_checkout(&plan).unwrap();
        assert_eq!(read(&dir, "f"), "y");
        assert!(!fs::symlink_metadata(dir.path().join("f"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(read(&outside, "secret"), "outside");
    }

    #[cfg(unix)]
    #[test]
    fn a_removed_path_that_is_a_link_to_a_directory_removes_the_link_only() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let repository = repository(&dir);
        write(&outside, "inner/f", "outside");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("x")).unwrap();
        let from = tree(&repository, &[plain("x", "was a file"), plain("keep", "k")]);
        let to = tree(&repository, &[plain("keep", "k")]);
        let plan = repository.plan_checkout(Some(from), to).unwrap();
        repository.apply_checkout(&plan).unwrap();
        assert!(!exists(&dir, "x"));
        assert_eq!(read(&outside, "inner/f"), "outside");
    }

    #[cfg(unix)]
    #[test]
    fn an_emptied_directory_is_not_removed_through_a_link() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let repository = repository(&dir);
        fs::create_dir(outside.path().join("b")).unwrap();
        // The old tree has `a/b/f`; `a` is a link to a place that holds an empty `b`.
        std::os::unix::fs::symlink(outside.path(), dir.path().join("a")).unwrap();
        let from = tree(&repository, &[plain("a/b/f", "1"), plain("keep", "k")]);
        let to = tree(&repository, &[plain("keep", "k")]);
        let plan = repository.plan_checkout(Some(from), to).unwrap();
        repository.apply_checkout(&plan).unwrap();
        assert!(
            outside.path().join("b").is_dir(),
            "the directory outside is not touched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_tracked_link_can_become_a_directory() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "v/old", "1");
        std::os::unix::fs::symlink("v", dir.path().join("lib")).unwrap();
        let from = tree(&repository, &[("lib", FileMode::Symlink, "v")]);
        let to = tree(&repository, &[plain("lib/x", "2")]);
        let plan = repository.plan_checkout(Some(from), to).unwrap();
        repository.apply_checkout(&plan).unwrap();
        assert_eq!(read(&dir, "lib/x"), "2");
        assert_eq!(
            read(&dir, "v/old"),
            "1",
            "the target of the link is untouched"
        );
    }
}
