//! The snapshot of the working directory (ADR-010).
//!
//! A snapshot builds the tree of the files that the working-copy commit tracks, as they are in the
//! working directory now. If it differs from the tree of the working-copy commit, a new commit
//! replaces the old one, with the same parents and the same change ID, and the move is recorded in
//! the operation log. A snapshot looks only at tracked files: a file that is not in the tree of the
//! working-copy commit is untracked and is neither saved nor reported. [`Repository::track`]
//! shares the work (`replace_working_copy`) and adds the files that it names.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};

use super::{now, Repository, UpdateError};
use crate::index::{Index, IndexError, WriteTreeError};
use crate::object::{
    Blob, BlobId, Commit, CommitId, CommitParseError, FileMode, Kind, ObjectId, Tree, TreeId,
    TreeParseError,
};
use crate::oplog::{Entry, OperationName};
use crate::refs::{RefError, WriteLock};
use crate::store::{Batch, ObjectStore, StoreError};
use crate::{Actor, Allowed, Authorizer};

/// The operation that the log records for a snapshot.
const OPERATION: &str = "snapshot";

/// A file to save: a file of the tree of the working-copy commit, or one that a caller adds.
pub(super) struct Tracked {
    pub(super) path: String,
    /// The mode that the tree has. For a file that is added it is a placeholder, because the mode
    /// comes from the file system (where it has an executable bit and links).
    pub(super) mode: FileMode,
    /// A caller named this file. If it is gone when it is saved, that is an error and not a file
    /// that leaves the tree.
    pub(super) named: bool,
}

impl Repository {
    /// Saves the tracked files of the working directory as the new working-copy commit, if they
    /// differ from the tree of the current one (ADR-010).
    ///
    /// Only the files in the tree of the working-copy commit are read: a changed file is saved, and
    /// a deleted one leaves the tree, and so does one that a directory has replaced or that lies
    /// below a directory that is now a link (a link is never followed). A tracked path that is a
    /// link itself is saved as the link. A file that is not tracked is not saved. The new commit
    /// keeps the parents, the change ID, and the message of the one it replaces; its author is
    /// `actor` and its time is now. The move is recorded in the log in the order of
    /// [`update_working_copy`](Self::update_working_copy), and the authorizer is asked once, before
    /// anything is read.
    ///
    /// Every tracked file is read completely, and the blobs are read again when the tree is
    /// written, so the cost is that of reading the tracked files twice. There is no cache (see
    /// ADR-010).
    ///
    /// Returns the entry, or `None` if the tree is the same: nothing changed, so there is no commit
    /// and no entry.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::Update`] if the authorizer refuses or the move fails (among others
    /// [`RefError::StaleWorkingCopy`] when another process moved the working-copy commit first: the
    /// snapshot can be repeated), [`SnapshotError::Refs`] if the working-copy commit cannot be
    /// read at the start, [`SnapshotError::Missing`] and [`SnapshotError::Store`] if the object
    /// store lacks an object that the commit needs or fails, [`SnapshotError::Unsupported`] for a
    /// tracked path that is a special file or a link that is not UTF-8, [`SnapshotError::Io`] if a
    /// file cannot be read or is replaced while it is read, and [`SnapshotError::Index`] if the
    /// working directory changes so that one path is a file and a directory. Nothing but objects
    /// is written before the move, and an object that no commit names is harmless.
    ///
    /// The lock of the references is taken after the authorizer agreed and before the first file
    /// is read, and held to the move (ADR-014): a purge cannot start in between, and a lock that
    /// another command holds is `UpdateError::Refs` with [`RefError::Locked`], as is the journal
    /// of a purge that has not finished with [`RefError::PurgePending`]. Both write nothing.
    pub fn snapshot(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Option<Entry>, SnapshotError> {
        let operation =
            OperationName::new(OPERATION).expect("`snapshot` is a valid operation name");
        let replaced = self.replace_working_copy(&operation, actor, authorizer, |_| {
            Ok::<_, SnapshotError>(Vec::new())
        })?;
        match replaced.late {
            Some(error) => Err(SnapshotError::Update(error)),
            None => Ok(replaced.entry),
        }
    }

    /// Saves the tracked files, and the files that `additions` returns, as the new working-copy
    /// commit, and moves the working-copy commit to it if the tree differs. The authorizer is asked
    /// once, and `additions` is called after it agreed, with the root of the working directory. The
    /// tracked files are saved first. An added file that the tree holds already is left out, as is
    /// a repeated one.
    ///
    /// [`Replaced::added`] lists the added files that were saved. One that is gone when it is
    /// saved is left out and is no error, unless the caller named it ([`Tracked::named`]). If the
    /// move succeeded but the directory could not be flushed ([`UpdateError::NotDurable`]), the
    /// call is not an error: the move happened, so the list is still true, and the error is in
    /// [`Replaced::late`] for the caller to report after it has shown the list.
    ///
    /// # Errors
    ///
    /// Returns what [`snapshot`](Self::snapshot) returns, [`SnapshotError::Gone`] for a named file
    /// that is gone, and what `additions` returns.
    pub(super) fn replace_working_copy<E: From<SnapshotError>>(
        &self,
        operation: &OperationName,
        actor: &Actor,
        authorizer: &dyn Authorizer,
        additions: impl FnOnce(&Path) -> Result<Vec<Tracked>, E>,
    ) -> Result<Replaced, E> {
        let allowed = self
            .authorize(actor, authorizer, operation, "working-copy")
            .map_err(SnapshotError::Update)?;
        let lock = self.lock_writer(&allowed)?;
        self.replace_locked(&lock, operation, actor, additions)
    }

    /// Takes the lock of the references for a command that writes, before its first object
    /// (ADR-014). The lock refuses while the journal of a purge exists, and a purge cannot start
    /// while it is held, so no purge deletes an object that the command saves and names.
    pub(super) fn lock_writer(&self, allowed: &Allowed) -> Result<WriteLock, SnapshotError> {
        self.refs_store()
            .lock_writer(allowed)
            .map_err(|error| SnapshotError::Update(UpdateError::Refs(error)))
    }

    /// The [`snapshot`](Self::snapshot) of a command that holds the lock already: `commit` and
    /// `op restore` take it before their snapshot and keep it to their own move.
    ///
    /// # Errors
    ///
    /// Returns what [`snapshot`](Self::snapshot) returns.
    pub(super) fn snapshot_locked(
        &self,
        lock: &WriteLock,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Option<Entry>, SnapshotError> {
        let operation =
            OperationName::new(OPERATION).expect("`snapshot` is a valid operation name");
        // The caller holds the lock already, so the proof has nothing left to open.
        let _ = self
            .authorize(actor, authorizer, &operation, "working-copy")
            .map_err(SnapshotError::Update)?;
        let replaced = self.replace_locked(lock, &operation, actor, |_| {
            Ok::<_, SnapshotError>(Vec::new())
        })?;
        match replaced.late {
            Some(error) => Err(SnapshotError::Update(error)),
            None => Ok(replaced.entry),
        }
    }

    /// The work of [`replace_working_copy`](Self::replace_working_copy) under the lock, after the
    /// authorizer agreed.
    fn replace_locked<E: From<SnapshotError>>(
        &self,
        lock: &WriteLock,
        operation: &OperationName,
        actor: &Actor,
        additions: impl FnOnce(&Path) -> Result<Vec<Tracked>, E>,
    ) -> Result<Replaced, E> {
        let current = self.refs().working_copy().map_err(SnapshotError::Refs)?;
        let commit = read_commit(self.objects(), current)?;
        let tracked = tracked_files(self.objects(), commit.tree())?;
        let mut known: HashSet<String> = tracked.iter().map(|file| file.path.clone()).collect();
        let added: Vec<Tracked> = additions(self.path())?
            .into_iter()
            .filter(|file| known.insert(file.path.clone()))
            .collect();
        let first_added = tracked.len();
        // The new objects are flushed together, before the log entry and the move name them
        // (ADR-007, STEMMA-40).
        let objects = self.objects().batch();
        let mut index = Index::new();
        let mut saved = Vec::new();
        for (position, file) in tracked.into_iter().chain(added).enumerate() {
            if let Some((mode, id)) = save(self.path(), &objects, &file)? {
                if position >= first_added {
                    saved.push(file.path.clone());
                }
                // A tree has no path that is a directory of another, but the working directory
                // can change between the lookups of two files.
                index
                    .insert(file.path, mode, id)
                    .map_err(SnapshotError::Index)?;
            } else if file.named {
                return Err(SnapshotError::Gone { path: file.path }.into());
            }
        }
        #[cfg(test)]
        pause::reached(pause::Stage::Saved);
        let tree = index
            .write_tree(&objects)
            .map_err(SnapshotError::WriteTree)?;
        if tree == commit.tree() {
            // An unchanged tree has all its objects already, unless one was damaged and saved
            // again. The entry that names the working-copy commit was written after its objects
            // were durable (ADR-011), so only the new ones are flushed; flushing the directories
            // of a whole tree would slow every command that takes a snapshot of an unchanged tree.
            finish_new(objects)?;
            return Ok(Replaced {
                entry: None,
                added: saved,
                late: None,
            });
        }
        let replacement = commit.rewrite(tree, actor.clone(), now(), commit.message());
        let new = objects
            .put_commit(&replacement)
            .map_err(|error| SnapshotError::Store(Box::new(error)))?;
        finish(objects)?;
        match self.move_working_copy(lock, operation, current, new, actor) {
            Ok(entry) => Ok(Replaced {
                entry,
                added: saved,
                late: None,
            }),
            Err(error @ UpdateError::NotDurable { .. }) => Ok(Replaced {
                entry: None,
                added: saved,
                late: Some(error),
            }),
            Err(error) => Err(SnapshotError::Update(error).into()),
        }
    }
}

/// The result of [`Repository::replace_working_copy`].
#[derive(Debug)]
pub(super) struct Replaced {
    /// The entry of the move, or `None` if the tree is the same (or the move is in `late`).
    pub(super) entry: Option<Entry>,
    /// The added files that were saved.
    pub(super) added: Vec<String>,
    /// The move happened, but a directory could not be flushed: [`UpdateError::NotDurable`].
    pub(super) late: Option<UpdateError>,
}

/// Finishes the batch of a snapshot.
fn finish(objects: Batch<'_>) -> Result<(), SnapshotError> {
    objects
        .finish()
        .map_err(|error| SnapshotError::Store(Box::new(error)))
}

/// Finishes the batch of a snapshot whose tree did not change. The directories of the objects
/// that the store had are not flushed; those of new objects are.
fn finish_new(objects: Batch<'_>) -> Result<(), SnapshotError> {
    objects
        .finish_new()
        .map_err(|error| SnapshotError::Store(Box::new(error)))
}

/// Reads and parses the commit.
pub(super) fn read_commit(
    objects: &dyn ObjectStore,
    id: CommitId,
) -> Result<Commit, SnapshotError> {
    let raw = read_object(objects, id.object_id(), Kind::Commit)?;
    Commit::parse(raw.body()).map_err(|source| SnapshotError::Commit { id, source })
}

/// Reads an object of the kind, or fails if the store lacks it.
pub(super) fn read_object(
    objects: &dyn ObjectStore,
    id: ObjectId,
    kind: Kind,
) -> Result<crate::store::RawObject, SnapshotError> {
    objects
        .get_expecting(&id, kind)
        .map_err(|error| SnapshotError::Store(Box::new(error)))?
        .ok_or(SnapshotError::Missing { id, kind })
}

/// Lists the files of a tree with their paths and modes, in the order of a depth-first walk.
pub(super) fn tracked_files(
    objects: &dyn ObjectStore,
    root: TreeId,
) -> Result<Vec<Tracked>, SnapshotError> {
    let mut files = Vec::new();
    // A directory is walked when it is popped; the stack holds the directories to do, with the
    // path of each, and the entries are pushed in reverse so that the first one is done first.
    let mut pending = vec![(String::new(), root)];
    while let Some((prefix, id)) = pending.pop() {
        let raw = read_object(objects, id.object_id(), Kind::Tree)?;
        let tree = Tree::parse(raw.body()).map_err(|source| SnapshotError::Tree { id, source })?;
        let mut directories = Vec::new();
        for entry in tree.entries() {
            let path = if prefix.is_empty() {
                entry.name().to_owned()
            } else {
                format!("{prefix}/{}", entry.name())
            };
            match FileMode::from_entry_mode(entry.mode()) {
                Some(mode) => files.push(Tracked {
                    path,
                    mode,
                    named: false,
                }),
                None => directories.push((path, TreeId::new(entry.id()))),
            }
        }
        pending.extend(directories.into_iter().rev());
    }
    Ok(files)
}

/// Looks at a tracked path without following a link, on the path or at its end. Returns `None` if
/// the file is gone, or if a directory on the way is gone or has been replaced by something else:
/// a tracked `a/b` below an `a` that is now a link to another place must not be read through it.
pub(super) fn lookup(
    root: &Path,
    path: &str,
) -> Result<Option<(PathBuf, Metadata)>, SnapshotError> {
    let mut full = root.to_path_buf();
    let mut components = path.split('/').peekable();
    while let Some(component) = components.next() {
        full.push(component);
        let metadata = match fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None)
            }
            Err(source) => return Err(SnapshotError::io("inspect", &full, source)),
        };
        if components.peek().is_some() {
            if !metadata.is_dir() {
                return Ok(None);
            }
        } else {
            return Ok(Some((full, metadata)));
        }
    }
    Ok(None)
}

/// The mode of a regular file, from its executable bit on Unix.
fn file_mode(metadata: &Metadata, tracked: FileMode) -> FileMode {
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode() & 0o111 != 0)
    };
    #[cfg(not(unix))]
    let executable = {
        let _ = metadata;
        None
    };
    regular_file_mode(executable, tracked)
}

/// The mode of a regular file, given its executable bit where the platform has one. Outside Unix
/// (Windows) the mode that the tree has stays. A tracked link stays a link there as well: the
/// checkout writes a link as a plain file that holds its target, as Git does with
/// `core.symlinks=false`, and the file is read back as the target, so a checkout followed by a
/// snapshot changes nothing.
fn regular_file_mode(executable: Option<bool>, tracked: FileMode) -> FileMode {
    match executable {
        Some(true) => FileMode::Executable,
        Some(false) => FileMode::File,
        None => tracked,
    }
}

/// The longest target that a file which stands for a link may hold, `PATH_MAX` on Linux.
const MAX_LINK_TARGET: usize = 4096;

/// Whether the content of a file that stands for a link is a target that a link on Unix can have:
/// UTF-8 as a target that a snapshot reads from a real link, not empty, no longer than
/// [`MAX_LINK_TARGET`], and without a NUL byte or a line break. A line break also catches an editor
/// that adds one at the end, and most content that was written over the file as if it were the
/// target itself.
fn is_link_target(content: &[u8]) -> bool {
    !content.is_empty()
        && content.len() <= MAX_LINK_TARGET
        && !content
            .iter()
            .any(|&byte| matches!(byte, 0 | b'\n' | b'\r'))
        && std::str::from_utf8(content).is_ok()
}

/// Reads the file that `lookup` saw, or returns `None` if it is gone. `fs::read` follows links, so
/// a process that swaps the file or a directory above it for a link between the lookup and the read
/// could lead it out of the working directory; the file that was opened must be the one that was
/// looked at, which is checked by its device and inode where the file system has them.
fn read_file(path: &Path, seen: &Metadata) -> Result<Option<Vec<u8>>, SnapshotError> {
    use std::io::Read;
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(SnapshotError::io("read the file", path, source)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file
            .metadata()
            .map_err(|source| SnapshotError::io("read the file", path, source))?;
        if (opened.dev(), opened.ino()) != (seen.dev(), seen.ino()) {
            return Err(SnapshotError::io(
                "read the file",
                path,
                io::Error::other("the file was replaced while it was read"),
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = seen;
    let mut content = Vec::new();
    file.read_to_end(&mut content)
        .map_err(|source| SnapshotError::io("read the file", path, source))?;
    Ok(Some(content))
}

/// Stores the content of a tracked file as a blob, or returns `None` if the file is gone or a
/// directory has taken its place.
fn save(
    root: &Path,
    objects: &dyn ObjectStore,
    file: &Tracked,
) -> Result<Option<(FileMode, BlobId)>, SnapshotError> {
    let Some((full, metadata)) = lookup(root, &file.path)? else {
        return Ok(None);
    };
    let kind = metadata.file_type();
    let (mode, content) = if kind.is_symlink() {
        let target = fs::read_link(&full)
            .map_err(|source| SnapshotError::io("read the link", &full, source))?;
        let target = target.to_str().ok_or_else(|| SnapshotError::Unsupported {
            path: file.path.clone(),
            what: Unsupported::NonUtf8Link,
        })?;
        (FileMode::Symlink, target.as_bytes().to_vec())
    } else if kind.is_file() {
        let Some(content) = read_file(&full, &metadata)? else {
            return Ok(None);
        };
        let mode = file_mode(&metadata, file.mode);
        if mode == FileMode::Symlink && !is_link_target(&content) {
            return Err(SnapshotError::Unsupported {
                path: file.path.clone(),
                what: Unsupported::LinkFile,
            });
        }
        (mode, content)
    } else if kind.is_dir() {
        // A directory has taken the place of the file; what is below it is not tracked.
        return Ok(None);
    } else {
        return Err(SnapshotError::Unsupported {
            path: file.path.clone(),
            what: Unsupported::SpecialFile,
        });
    };
    let id = objects
        .put_blob(&Blob::new(content))
        .map_err(|error| SnapshotError::Store(Box::new(error)))?;
    Ok(Some((mode, id)))
}

/// Why [`Repository::snapshot`] fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum SnapshotError {
    /// The authorizer refused, or the move of the working-copy commit failed. See
    /// [`UpdateError`] for what is in the log and whether the pointer moved.
    Update(UpdateError),
    /// The working-copy commit cannot be read when the snapshot starts. A [`RefError`] of the
    /// move itself, among them [`RefError::StaleWorkingCopy`], is in [`Update`](Self::Update).
    Refs(RefError),
    /// The object store failed.
    Store(Box<StoreError>),
    /// The object store has no object that the working-copy commit needs.
    Missing {
        /// The ID of the object.
        id: ObjectId,
        /// The kind it should have.
        kind: Kind,
    },
    /// The working-copy commit is not a valid commit.
    Commit {
        /// The commit.
        id: CommitId,
        /// What is wrong with it.
        source: CommitParseError,
    },
    /// A tree of the working-copy commit is not valid.
    Tree {
        /// The tree.
        id: TreeId,
        /// What is wrong with it.
        source: TreeParseError,
    },
    /// A path is a special file (a device, a socket, a pipe), a link, or a name that cannot be
    /// saved. It is not skipped, because that would drop it from the history without a word.
    Unsupported {
        /// The path. For a name that is not UTF-8 it ends in the name in the form of `Debug`.
        path: String,
        /// What it is.
        what: Unsupported,
    },
    /// A file that a caller named is gone, or a directory or a link has taken its place, when it is
    /// saved. A tracked file that is gone leaves the tree instead.
    Gone {
        /// The path.
        path: String,
    },
    /// The paths found in the working directory do not make an index: a path is a file in one
    /// lookup and a directory in another, because the working directory changed meanwhile.
    Index(IndexError),
    /// A file of the working directory cannot be read.
    Io {
        /// What was being done.
        operation: &'static str,
        /// The file involved.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// The tree of the snapshot could not be written.
    WriteTree(WriteTreeError),
}

/// What a snapshot cannot save of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unsupported {
    /// A device, a socket, or a pipe.
    SpecialFile,
    /// A link whose target is not UTF-8.
    NonUtf8Link,
    /// A file whose name is not UTF-8.
    NonUtf8Name,
    /// A plain file that stands for a tracked link where links are not written (Windows), and
    /// whose content is not a target that a link can have.
    LinkFile,
}

impl Unsupported {
    /// What the user can do about it.
    fn remedy(self) -> &'static str {
        match self {
            Self::SpecialFile => "remove it or put a regular file in its place",
            Self::NonUtf8Link => "point the link at a target with a UTF-8 name",
            Self::NonUtf8Name => "rename it",
            Self::LinkFile => "write the target of the link into it, without a line break",
        }
    }
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SpecialFile => "a special file",
            Self::NonUtf8Link => "a link whose target is not UTF-8",
            Self::NonUtf8Name => "a file whose name is not UTF-8",
            Self::LinkFile => "a file that stands for a link and does not hold a valid target",
        })
    }
}

impl SnapshotError {
    pub(super) fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    /// Returns the error of the file system, if that is what failed. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Update(error) => error.io_error(),
            Self::Refs(error) => error.io_error(),
            Self::Store(error) => error.io_error(),
            Self::WriteTree(error) => error.store_error().and_then(StoreError::io_error),
            Self::Missing { .. }
            | Self::Commit { .. }
            | Self::Tree { .. }
            | Self::Unsupported { .. }
            | Self::Gone { .. }
            | Self::Index(_) => None,
        }
    }
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Update(error) => write!(f, "could not record the snapshot: {error}"),
            Self::Refs(error) => write!(f, "could not read the working-copy commit: {error}"),
            Self::Store(error) => write!(f, "the object store failed during the snapshot: {error}"),
            Self::Missing { id, kind } => {
                write!(f, "the store has no {} {id}", kind.name())
            }
            Self::Commit { id, source } => {
                write!(f, "the commit {id} is not valid: {source}")
            }
            Self::Tree { id, source } => write!(f, "the tree {id} is not valid: {source}"),
            Self::Unsupported { path, what } => write!(
                f,
                "`{}` is {what}, which cannot be saved; {}",
                path.escape_debug(),
                what.remedy()
            ),
            Self::Gone { path } => write!(
                f,
                "`{}` is gone, or something else has taken its place",
                path.escape_debug()
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
            Self::Index(error) => write!(
                f,
                "the working directory changed while its files were read, so its paths do not \
                 fit: {error}"
            ),
            Self::WriteTree(error) => {
                write!(f, "could not write the tree of the snapshot: {error}")
            }
        }
    }
}

impl Error for SnapshotError {}

/// The stop points of a writer for a test: a step that runs while the writer holds the lock. A
/// test runs another command there to reproduce an interleaving of two processes (ADR-014).
#[cfg(test)]
pub(super) mod pause {
    use std::cell::RefCell;

    /// Where the step runs.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(in crate::repository) enum Stage {
        /// In a snapshot, after the files are saved and before the tree is written and the
        /// working-copy commit moves.
        Saved,
        /// In `commit` and `op restore`, after their snapshot and before their moves.
        Moves,
    }

    /// A step that a test set, with the stage where it runs.
    type Step = (Stage, Box<dyn FnOnce()>);

    thread_local! {
        static STEP: RefCell<Option<Step>> = RefCell::new(None);
    }

    /// Runs `step` when this thread next reaches `stage`.
    pub(in crate::repository) fn at_next(stage: Stage, step: impl FnOnce() + 'static) {
        STEP.with(|cell| *cell.borrow_mut() = Some((stage, Box::new(step))));
    }

    /// A stop point: runs the step that a test set for `stage`, once.
    pub(in crate::repository) fn reached(stage: Stage) {
        let step = STEP.with(|cell| {
            let mut cell = cell.borrow_mut();
            match cell.take() {
                Some((at, step)) if at == stage => Some(step),
                other => {
                    *cell = other;
                    None
                }
            }
        });
        if let Some(step) = step {
            step();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::InitOptions;
    use crate::testutil::TempDir;
    use crate::{AllowAll, Decision};

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &OperationName, _target: &str) -> Decision {
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

    fn current(repository: &Repository) -> Commit {
        let id = repository.refs().working_copy().unwrap();
        read_commit(repository.objects(), id).unwrap()
    }

    /// Makes the files and a working-copy commit that tracks them, with the message "work", by a
    /// move of the working-copy commit.
    fn track(repository: &Repository, dir: &TempDir, files: &[(&str, &str)]) {
        for (path, content) in files {
            write(dir, path, content);
        }
        let files: Vec<_> = files
            .iter()
            .map(|&(path, content)| (path, FileMode::File, content))
            .collect();
        commit_files(repository, &files);
    }

    /// Makes a working-copy commit whose tree holds the files, given as path, mode, and content,
    /// without writing them, and returns the tree.
    fn commit_files(repository: &Repository, files: &[(&str, FileMode, &str)]) -> TreeId {
        let mut index = Index::new();
        for &(path, mode, content) in files {
            let id = repository
                .objects()
                .put_blob(&Blob::new(content.as_bytes()))
                .unwrap();
            index.insert(path, mode, id).unwrap();
        }
        let tree = index.write_tree(repository.objects()).unwrap();
        let old = repository.refs().working_copy().unwrap();
        let before = current(repository);
        let commit = Commit::new(
            tree,
            before.parents().to_vec(),
            before.change(),
            actor(),
            1,
            "work",
        )
        .unwrap();
        let new = repository.objects().put_commit(&commit).unwrap();
        repository
            .update_working_copy(
                &OperationName::new("track").unwrap(),
                old,
                new,
                &actor(),
                &AllowAll,
            )
            .unwrap();
        tree
    }

    fn snapshot(repository: &Repository) -> Option<Entry> {
        repository.snapshot(&other_actor(), &AllowAll).unwrap()
    }

    #[test]
    fn a_snapshot_puts_its_new_objects_in_place_together_before_its_move() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "one")]);
        write(&dir, "a.txt", "changed");
        let changed = Blob::new("changed").id().object_id();
        let path = dir.path().to_path_buf();
        let objects = repository.objects().clone();
        pause::at_next(pause::Stage::Saved, move || {
            assert!(
                !objects.contains(&changed).unwrap(),
                "the saved blob waits for the others"
            );
            let waiting = fs::read_dir(path.join(".stemma").join("tmp"))
                .unwrap()
                .count();
            assert_eq!(waiting, 1, "the blob is in `tmp/`");
        });
        let entry = snapshot(&repository).expect("the working-copy commit moved");
        assert!(repository.objects().contains(&changed).unwrap());
        let tmp = dir.path().join(".stemma").join("tmp");
        assert_eq!(fs::read_dir(tmp).unwrap().count(), 0);
        // Everything that the moved commit reaches is in place.
        let commit = current(&repository);
        assert_eq!(
            Some(commit.id()),
            entry
                .change()
                .unwrap()
                .working_copy()
                .unwrap()
                .after()
                .copied()
        );
        assert!(repository
            .objects()
            .contains(&commit.tree().object_id())
            .unwrap());
    }

    /// The tracked paths of the working-copy commit with their modes, as a sorted list.
    fn tracked(repository: &Repository) -> Vec<(String, FileMode)> {
        let mut files: Vec<_> = tracked_files(repository.objects(), current(repository).tree())
            .unwrap()
            .into_iter()
            .map(|file| (file.path, file.mode))
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    fn paths(repository: &Repository) -> Vec<String> {
        tracked(repository)
            .into_iter()
            .map(|(path, _)| path)
            .collect()
    }

    #[test]
    fn nothing_changed_makes_no_commit_and_no_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "one"), ("d/b.txt", "two")]);
        let before = repository.refs().working_copy().unwrap();
        let numbers = repository.oplog().numbers().unwrap();
        assert!(snapshot(&repository).is_none());
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(repository.oplog().numbers().unwrap(), numbers);
    }

    #[test]
    fn a_snapshot_of_an_unchanged_tree_flushes_no_directory() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let files: Vec<_> = (0..20)
            .map(|n| (format!("f{n}.txt"), format!("content {n}")))
            .collect();
        let files: Vec<_> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        track(&repository, &dir, &files);
        let before = crate::store::flush_count();
        assert!(snapshot(&repository).is_none());
        assert_eq!(
            crate::store::flush_count() - before,
            0,
            "the objects of an unchanged tree are durable already"
        );
    }

    #[test]
    fn a_changed_file_replaces_the_commit_and_keeps_parents_change_and_message() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "one"), ("b.txt", "two")]);
        let old = repository.refs().working_copy().unwrap();
        let before = current(&repository);
        write(&dir, "a.txt", "changed");
        let entry = snapshot(&repository).unwrap();
        let new = repository.refs().working_copy().unwrap();
        assert_ne!(new, old);
        let after = current(&repository);
        assert_ne!(after.tree(), before.tree());
        assert_eq!(after.parents(), before.parents());
        assert_eq!(after.change(), before.change());
        assert_eq!(after.message(), "work");
        assert_eq!(after.author(), &other_actor());
        // The entry names both commits, so the log keeps the one that was replaced.
        assert_eq!(entry.change().unwrap().operation().as_str(), "snapshot");
        assert_eq!(entry.change().unwrap().actor(), &other_actor());
        let moved = entry.change().unwrap().working_copy().unwrap();
        assert_eq!((moved.before(), moved.after()), (Some(&old), Some(&new)));
        assert!(repository.objects().contains(&old.object_id()).unwrap());
        // The content is in the tree.
        let blob = |commit: &Commit| {
            let raw = repository
                .objects()
                .get_expecting(&commit.tree().object_id(), Kind::Tree)
                .unwrap()
                .unwrap();
            Tree::parse(raw.body())
                .unwrap()
                .entries()
                .iter()
                .find(|entry| entry.name() == "a.txt")
                .unwrap()
                .id()
        };
        assert_eq!(
            blob(&after),
            Blob::new(b"changed".to_vec()).id().object_id()
        );
        // A second snapshot finds nothing more to save.
        assert!(snapshot(&repository).is_none());
    }

    #[test]
    fn a_deleted_file_leaves_the_tree() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "one"), ("d/b.txt", "two")]);
        fs::remove_file(dir.path().join("a.txt")).unwrap();
        assert!(snapshot(&repository).is_some());
        assert_eq!(paths(&repository), ["d/b.txt"]);
        fs::remove_dir_all(dir.path().join("d")).unwrap();
        assert!(snapshot(&repository).is_some());
        assert!(paths(&repository).is_empty());
        assert_eq!(
            current(&repository).tree(),
            Tree::new(Vec::new()).unwrap().id()
        );
    }

    #[test]
    fn an_untracked_file_is_not_saved() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "one")]);
        write(&dir, "new.txt", "untracked");
        write(&dir, "d/also.txt", "untracked");
        assert!(snapshot(&repository).is_none());
        assert_eq!(paths(&repository), ["a.txt"]);
    }

    #[test]
    fn a_directory_in_the_place_of_a_file_drops_the_file() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a", "one"), ("b.txt", "two")]);
        fs::remove_file(dir.path().join("a")).unwrap();
        write(&dir, "a/inside.txt", "untracked");
        assert!(snapshot(&repository).is_some());
        assert_eq!(paths(&repository), ["b.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_bit_is_a_change() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("run.sh", "echo")]);
        fs::set_permissions(dir.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(snapshot(&repository).is_some());
        assert_eq!(
            tracked(&repository),
            [("run.sh".to_owned(), FileMode::Executable)]
        );
        fs::set_permissions(dir.path().join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(snapshot(&repository).is_some());
        assert_eq!(
            tracked(&repository),
            [("run.sh".to_owned(), FileMode::File)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_tracked_link_is_saved_as_its_target() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("target.txt", "one")]);
        // The file becomes a link: the tree then holds the target and not the content.
        fs::remove_file(dir.path().join("target.txt")).unwrap();
        symlink("elsewhere", dir.path().join("target.txt")).unwrap();
        assert!(snapshot(&repository).is_some());
        assert_eq!(
            tracked(&repository),
            [("target.txt".to_owned(), FileMode::Symlink)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_the_place_of_a_directory_is_not_followed() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let outside = TempDir::new();
        write(&outside, "b.txt", "secret");
        let repository = repository(&dir);
        track(&repository, &dir, &[("d/b.txt", "two"), ("c.txt", "three")]);
        fs::remove_dir_all(dir.path().join("d")).unwrap();
        symlink(outside.path(), dir.path().join("d")).unwrap();
        assert!(snapshot(&repository).is_some());
        // The file below the link is gone from the tree, and nothing of the other place was read.
        assert_eq!(paths(&repository), ["c.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_special_file_is_refused_and_nothing_moves() {
        use std::os::unix::net::UnixListener;
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "one"), ("sock", "x")]);
        let before = repository.refs().working_copy().unwrap();
        let numbers = repository.oplog().numbers().unwrap();
        fs::remove_file(dir.path().join("sock")).unwrap();
        let _listener = UnixListener::bind(dir.path().join("sock")).unwrap();
        write(&dir, "a.txt", "changed");
        let error = repository.snapshot(&actor(), &AllowAll).unwrap_err();
        assert!(
            matches!(&error, SnapshotError::Unsupported { path, .. } if path == "sock"),
            "{error}"
        );
        assert!(error.to_string().contains("special file"), "{error}");
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(repository.oplog().numbers().unwrap(), numbers);
    }

    #[test]
    fn a_denied_snapshot_reads_and_writes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "one")]);
        write(&dir, "a.txt", "changed");
        let before = repository.refs().working_copy().unwrap();
        let numbers = repository.oplog().numbers().unwrap();
        let error = repository.snapshot(&actor(), &DenyAll).unwrap_err();
        assert!(
            matches!(error, SnapshotError::Update(UpdateError::Denied { .. })),
            "{error}"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(repository.oplog().numbers().unwrap(), numbers);
        assert!(!repository
            .objects()
            .contains(&Blob::new(b"changed".to_vec()).id().object_id())
            .unwrap());
    }

    #[test]
    fn a_repository_without_a_working_copy_commit_reports_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        fs::remove_file(repository.metadata_dir().join("working-copy")).unwrap();
        let error = repository.snapshot(&actor(), &AllowAll).unwrap_err();
        assert!(
            matches!(error, SnapshotError::Refs(RefError::NoWorkingCopy { .. })),
            "{error}"
        );
    }

    #[test]
    fn a_working_copy_commit_whose_tree_is_missing_is_reported() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "one")]);
        let tree = current(&repository).tree();
        let path = repository.objects().object_path(&tree.object_id());
        fs::remove_file(path).unwrap();
        let error = repository.snapshot(&actor(), &AllowAll).unwrap_err();
        assert!(
            matches!(
                &error,
                SnapshotError::Missing {
                    kind: Kind::Tree,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn the_entry_of_a_snapshot_is_confirmed_by_the_references() {
        use crate::oplog::{outcomes, Current, Outcome};
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "one")]);
        write(&dir, "a.txt", "changed");
        snapshot(&repository).unwrap();
        let entries = repository.oplog().entries().unwrap();
        let working_copy = repository.refs().working_copy().unwrap();
        let head = repository.refs().head().unwrap();
        let known = Current::new(head.clone(), [], true).with_working_copy(Some(working_copy));
        assert_eq!(outcomes(&entries, Some(&known)), [Outcome::Took; 3]);
        // Without the working-copy commit the newest entry stays unknown.
        let unknown = Current::new(head, [], true);
        assert_eq!(outcomes(&entries, Some(&unknown))[2], Outcome::Unknown);
    }

    #[test]
    fn every_snapshot_entry_and_the_commit_it_replaced_stay() {
        // Phase 1 keeps every entry (ADR-011, retention, STEMMA-39): after many snapshots the log
        // still runs without a gap and without a stub, each snapshot entry names the commit that
        // it replaced as its old value, and that commit is still in the store.
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "zero")]);
        let before = repository.oplog().numbers().unwrap().len();
        let mut replaced = Vec::new();
        for round in 1..=5 {
            replaced.push(repository.refs().working_copy().unwrap());
            write(&dir, "a.txt", &format!("round {round}"));
            snapshot(&repository).unwrap();
        }
        let entries = repository.oplog().entries().unwrap();
        let numbers: Vec<_> = entries.iter().map(Entry::number).collect();
        assert_eq!(numbers, (1..=(before + 5) as u64).collect::<Vec<_>>());
        assert!(entries.iter().all(|entry| entry.change().is_some()));
        let snapshots = &entries[before..];
        for (entry, commit) in snapshots.iter().zip(&replaced) {
            let change = entry.change().unwrap();
            assert_eq!(change.operation().as_str(), "snapshot");
            assert_eq!(change.working_copy().unwrap().before(), Some(commit));
            // The old value alone makes it a root: the new value is another commit.
            assert_ne!(change.working_copy().unwrap().after(), Some(commit));
            assert!(entry.commits().contains(commit), "{commit}");
            assert!(repository.objects().contains(&commit.object_id()).unwrap());
        }
    }

    #[test]
    fn a_change_id_is_not_made_again_by_a_snapshot() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "one")]);
        let change = current(&repository).change();
        for round in 0..3 {
            write(&dir, "a.txt", &format!("round {round}"));
            snapshot(&repository).unwrap();
            assert_eq!(current(&repository).change(), change);
        }
    }

    #[test]
    fn nested_paths_are_changed_deleted_and_left_alone_at_depth() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(
            &repository,
            &dir,
            &[
                ("a/b/c/deep.txt", "one"),
                ("a/b/c/other.txt", "two"),
                ("a/b/side.txt", "three"),
                ("top.txt", "four"),
            ],
        );
        write(&dir, "a/b/c/deep.txt", "changed");
        write(&dir, "a/b/c/untracked.txt", "untracked");
        assert!(snapshot(&repository).is_some());
        assert_eq!(
            paths(&repository),
            [
                "a/b/c/deep.txt",
                "a/b/c/other.txt",
                "a/b/side.txt",
                "top.txt"
            ]
        );
        // A deletion that empties a directory takes the directory out of the tree.
        fs::remove_file(dir.path().join("a/b/c/deep.txt")).unwrap();
        fs::remove_file(dir.path().join("a/b/c/other.txt")).unwrap();
        assert!(snapshot(&repository).is_some());
        assert_eq!(paths(&repository), ["a/b/side.txt", "top.txt"]);
        let expected = {
            let mut index = Index::new();
            for (path, content) in [("a/b/side.txt", "three"), ("top.txt", "four")] {
                let id = repository
                    .objects()
                    .put_blob(&Blob::new(content.as_bytes()))
                    .unwrap();
                index.insert(path, FileMode::File, id).unwrap();
            }
            index.write_tree(repository.objects()).unwrap()
        };
        assert_eq!(current(&repository).tree(), expected);
    }

    #[test]
    fn an_empty_file_and_an_empty_untracked_directory() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("empty.txt", "full")]);
        write(&dir, "empty.txt", "");
        fs::create_dir(dir.path().join("hollow")).unwrap();
        assert!(snapshot(&repository).is_some());
        assert_eq!(paths(&repository), ["empty.txt"]);
        assert!(snapshot(&repository).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_content_change_keeps_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("run.sh", "echo")]);
        fs::set_permissions(dir.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        snapshot(&repository).unwrap();
        write(&dir, "run.sh", "echo changed");
        fs::set_permissions(dir.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(snapshot(&repository).is_some());
        assert_eq!(
            tracked(&repository),
            [("run.sh".to_owned(), FileMode::Executable)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_changes_with_its_target_and_back_to_a_file() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("l", "content")]);
        fs::remove_file(dir.path().join("l")).unwrap();
        symlink("first", dir.path().join("l")).unwrap();
        assert!(snapshot(&repository).is_some());
        assert!(snapshot(&repository).is_none());
        fs::remove_file(dir.path().join("l")).unwrap();
        symlink("second", dir.path().join("l")).unwrap();
        assert!(snapshot(&repository).is_some());
        assert_eq!(tracked(&repository), [("l".to_owned(), FileMode::Symlink)]);
        fs::remove_file(dir.path().join("l")).unwrap();
        write(&dir, "l", "content");
        assert!(snapshot(&repository).is_some());
        assert_eq!(tracked(&repository), [("l".to_owned(), FileMode::File)]);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_whose_target_is_not_utf8_is_refused() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("l", "content")]);
        let before = repository.refs().working_copy().unwrap();
        fs::remove_file(dir.path().join("l")).unwrap();
        symlink(OsStr::from_bytes(b"\xff\xfe"), dir.path().join("l")).unwrap();
        let error = repository.snapshot(&actor(), &AllowAll).unwrap_err();
        assert!(
            matches!(
                &error,
                SnapshotError::Unsupported {
                    what: Unsupported::NonUtf8Link,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), before);
    }

    #[cfg(windows)]
    #[test]
    fn a_link_that_a_checkout_writes_as_a_file_stays_a_link() {
        // A tree with a link, as a commit on Unix makes it; the checkout writes it as a plain file.
        let dir = TempDir::new();
        let repository = repository(&dir);
        let files = [
            ("target.txt", FileMode::File, "content"),
            ("link", FileMode::Symlink, "target.txt"),
        ];
        let tree = commit_files(&repository, &files);
        let plan = repository.plan_checkout(None, tree).unwrap();
        repository.apply_checkout(&plan).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("link")).unwrap(),
            "target.txt"
        );
        assert!(snapshot(&repository).is_none());
        // A new target in the file is a new target of the link.
        write(&dir, "link", "elsewhere");
        assert!(snapshot(&repository).is_some());
        assert_eq!(
            tracked(&repository),
            [
                ("link".to_owned(), FileMode::Symlink),
                ("target.txt".to_owned(), FileMode::File),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_checkout_again_finds_the_file_for_a_link_in_place() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let tree = commit_files(&repository, &[("link", FileMode::Symlink, "target.txt")]);
        let plan = repository.plan_checkout(None, tree).unwrap();
        repository.apply_checkout(&plan).unwrap();
        // A restore that stopped after the files plans from no tree and must not write it again.
        let again = repository.plan_checkout(None, tree).unwrap();
        assert!(again.writes().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn a_file_for_a_link_that_does_not_hold_a_target_is_refused() {
        let long = "a".repeat(MAX_LINK_TARGET + 1);
        let contents: [&[u8]; 6] = [
            b"\xff\xfe",
            b"",
            b"tar\0get",
            b"target.txt\n",
            b"target.txt\r\n",
            long.as_bytes(),
        ];
        for content in contents {
            let dir = TempDir::new();
            let repository = repository(&dir);
            commit_files(&repository, &[("link", FileMode::Symlink, "target")]);
            fs::write(dir.path().join("link"), content).unwrap();
            let before = repository.refs().working_copy().unwrap();
            let numbers = repository.oplog().numbers().unwrap();
            let error = repository.snapshot(&actor(), &AllowAll).unwrap_err();
            assert!(
                matches!(
                    &error,
                    SnapshotError::Unsupported {
                        what: Unsupported::LinkFile,
                        ..
                    }
                ),
                "{content:?}: {error}"
            );
            assert_eq!(repository.refs().working_copy().unwrap(), before);
            assert_eq!(repository.oplog().numbers().unwrap(), numbers);
        }
    }

    #[test]
    fn the_mode_of_a_regular_file_follows_the_executable_bit_or_the_tree() {
        for tracked in [FileMode::File, FileMode::Executable, FileMode::Symlink] {
            assert_eq!(regular_file_mode(Some(true), tracked), FileMode::Executable);
            assert_eq!(regular_file_mode(Some(false), tracked), FileMode::File);
            assert_eq!(regular_file_mode(None, tracked), tracked);
        }
    }

    #[test]
    fn a_file_for_a_link_holds_a_target_on_one_line() {
        assert!(is_link_target(b"target.txt"));
        assert!(is_link_target(b"../shared/config"));
        assert!(is_link_target("a".repeat(MAX_LINK_TARGET).as_bytes()));
        for content in [
            &b"\xff\xfe"[..],
            b"",
            b"tar\0get",
            b"target.txt\n",
            b"target.txt\r\n",
            b"key = value\nother = 1\n",
            "a".repeat(MAX_LINK_TARGET + 1).as_bytes(),
        ] {
            assert!(!is_link_target(content), "{content:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_an_io_error_and_nothing_moves() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "one"), ("secret", "two")]);
        let before = repository.refs().working_copy().unwrap();
        let numbers = repository.oplog().numbers().unwrap();
        write(&dir, "a.txt", "changed");
        let secret = dir.path().join("secret");
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fs::read(&secret).is_ok();
        let result = repository.snapshot(&actor(), &AllowAll);
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
        if readable {
            eprintln!("skipped: the permission of a file is not enforced here (root?)");
            return;
        }
        let error = result.unwrap_err();
        assert!(matches!(error, SnapshotError::Io { .. }), "{error}");
        assert!(error.io_error().is_some());
        assert!(error.to_string().contains("secret"), "{error}");
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(repository.oplog().numbers().unwrap(), numbers);
    }

    #[test]
    fn a_file_that_vanishes_before_the_read_is_gone() {
        let dir = TempDir::new();
        let seen = fs::metadata(dir.path()).unwrap();
        assert!(read_file(&dir.path().join("missing"), &seen)
            .unwrap()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_is_not_the_one_that_was_looked_at_is_refused() {
        let dir = TempDir::new();
        write(&dir, "one", "1");
        write(&dir, "two", "2");
        let seen = fs::symlink_metadata(dir.path().join("one")).unwrap();
        let error = read_file(&dir.path().join("two"), &seen).unwrap_err();
        assert!(matches!(error, SnapshotError::Io { .. }), "{error}");
        assert!(error.to_string().contains("replaced"), "{error}");
        let own = fs::symlink_metadata(dir.path().join("one")).unwrap();
        assert_eq!(
            read_file(&dir.path().join("one"), &own).unwrap(),
            Some(b"1".to_vec())
        );
    }

    #[test]
    fn a_denied_move_names_the_working_copy_as_its_target() {
        use std::sync::Mutex;
        struct Record(Mutex<Vec<(String, String)>>);
        impl Authorizer for Record {
            fn authorize(
                &self,
                _actor: &Actor,
                operation: &OperationName,
                target: &str,
            ) -> Decision {
                self.0
                    .lock()
                    .unwrap()
                    .push((operation.to_string(), target.to_owned()));
                Decision::Deny
            }
        }
        let dir = TempDir::new();
        let repository = repository(&dir);
        let old = repository.refs().working_copy().unwrap();
        let record = Record(Mutex::new(Vec::new()));
        let error = repository
            .update_working_copy(
                &OperationName::new("track").unwrap(),
                old,
                old,
                &actor(),
                &record,
            )
            .unwrap_err();
        assert!(matches!(error, UpdateError::Denied { .. }), "{error}");
        assert_eq!(
            *record.0.lock().unwrap(),
            [("track".to_owned(), "working-copy".to_owned())]
        );
    }

    #[test]
    fn snapshots_at_the_same_time_make_one_move_and_one_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        track(&repository, &dir, &[("a.txt", "one")]);
        let before = repository.refs().working_copy().unwrap();
        let numbers = repository.oplog().numbers().unwrap().len();
        write(&dir, "a.txt", "changed");
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..6)
                .map(|_| scope.spawn(|| repository.snapshot(&other_actor(), &AllowAll)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let moved = results
            .iter()
            .filter(|result| matches!(result, Ok(Some(_))))
            .count();
        // The others find nothing left to save, lose the race, or find the lock held.
        for result in &results {
            match result {
                Ok(_)
                | Err(SnapshotError::Update(UpdateError::Refs(
                    RefError::StaleWorkingCopy { .. } | RefError::Locked { .. },
                ))) => {}
                Err(error) => panic!("unexpected: {error}"),
            }
        }
        assert_eq!(moved, 1);
        let after = repository.refs().working_copy().unwrap();
        assert_ne!(after, before);
        assert_eq!(repository.oplog().numbers().unwrap().len(), numbers + 1);
        let entries = repository.oplog().entries().unwrap();
        let moves = entries
            .last()
            .unwrap()
            .change()
            .unwrap()
            .working_copy()
            .unwrap();
        assert_eq!(
            (moves.before(), moves.after()),
            (Some(&before), Some(&after))
        );
    }
}
