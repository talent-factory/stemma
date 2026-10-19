//! The snapshot of the working directory (ADR-010).
//!
//! A snapshot builds the tree of the files that the working-copy commit tracks, as they are in the
//! working directory now. If it differs from the tree of the working-copy commit, a new commit
//! replaces the old one, with the same parents and the same change ID, and the move is recorded in
//! the operation log. Only tracked files are looked at: a file that is not in the tree of the
//! working-copy commit is untracked and is neither saved nor reported.

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
use crate::refs::RefError;
use crate::store::{ObjectStore, StoreError};
use crate::{Actor, Authorizer};

/// The operation that the log records for a snapshot.
const OPERATION: &str = "snapshot";

/// A file that the working-copy commit tracks.
struct Tracked {
    path: String,
    mode: FileMode,
}

impl Repository {
    /// Saves the tracked files of the working directory as the new working-copy commit, if they
    /// differ from the tree of the current one (ADR-010).
    ///
    /// Only the files in the tree of the working-copy commit are read: a changed file is saved, and
    /// a deleted one (or one that a directory or a link to a directory has replaced) leaves the
    /// tree. A file that is not tracked is not saved. The new commit keeps the parents, the change
    /// ID, and the message of the one it replaces; its author is `actor` and its time is now. The
    /// move is recorded in the log in the order of
    /// [`update_working_copy`](Self::update_working_copy), and the authorizer is asked once, before
    /// anything is read.
    ///
    /// Every tracked file is read completely, and the blobs are read again when the tree is
    /// written, so the cost is that of reading the tracked files twice. There is no cache.
    ///
    /// Returns the entry, or `None` if the tree is the same: nothing changed, so there is no commit
    /// and no entry.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::Update`] if the authorizer refuses or the move fails (among others
    /// [`RefError::StaleWorkingCopy`] when another process moved the working-copy commit first: the
    /// snapshot can be repeated), [`SnapshotError::Refs`] if the working-copy commit cannot be
    /// read, [`SnapshotError::Missing`] and [`SnapshotError::Store`] if the object store lacks an
    /// object that the commit needs or fails, [`SnapshotError::Unsupported`] for a tracked path
    /// that is a special file or a link that is not UTF-8, and [`SnapshotError::Io`] if a file
    /// cannot be read. Nothing but objects is written before the move, and an object that no commit
    /// names is harmless.
    pub fn snapshot(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Option<Entry>, SnapshotError> {
        let operation =
            OperationName::new(OPERATION).expect("`snapshot` is a valid operation name");
        self.authorize(actor, authorizer, &operation, "working-copy")
            .map_err(SnapshotError::Update)?;
        let current = self.refs().working_copy().map_err(SnapshotError::Refs)?;
        let commit = read_commit(self.objects(), current)?;
        let mut index = Index::new();
        for file in tracked_files(self.objects(), commit.tree())? {
            if let Some((mode, id)) = save(self.path(), self.objects(), &file)? {
                index
                    .insert(file.path, mode, id)
                    .map_err(SnapshotError::Index)?;
            }
        }
        let tree = index
            .write_tree(self.objects())
            .map_err(SnapshotError::WriteTree)?;
        if tree == commit.tree() {
            return Ok(None);
        }
        let replacement = Commit::new(
            tree,
            commit.parents().to_vec(),
            commit.change(),
            actor.clone(),
            now(),
            commit.message(),
        )
        .expect("the parents of a commit are distinct");
        let new = self
            .objects()
            .put_commit(&replacement)
            .map_err(|error| SnapshotError::Store(Box::new(error)))?;
        self.move_working_copy(&operation, current, new, actor)
            .map_err(SnapshotError::Update)
    }
}

/// Reads and parses the commit.
fn read_commit(objects: &dyn ObjectStore, id: CommitId) -> Result<Commit, SnapshotError> {
    let raw = read_object(objects, id.object_id(), Kind::Commit)?;
    Commit::parse(raw.body()).map_err(|source| SnapshotError::Commit { id, source })
}

/// Reads an object of the kind, or fails if the store lacks it.
fn read_object(
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
fn tracked_files(objects: &dyn ObjectStore, root: TreeId) -> Result<Vec<Tracked>, SnapshotError> {
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
                Some(mode) => files.push(Tracked { path, mode }),
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
fn lookup(root: &Path, path: &str) -> Result<Option<(PathBuf, Metadata)>, SnapshotError> {
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

/// The mode of a regular file. Where the file system has no executable bit, the mode that the tree
/// has stays.
fn file_mode(metadata: &Metadata, tracked: FileMode) -> FileMode {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tracked;
        if metadata.permissions().mode() & 0o111 == 0 {
            FileMode::File
        } else {
            FileMode::Executable
        }
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        match tracked {
            FileMode::Symlink => FileMode::File,
            other => other,
        }
    }
}

/// Stores the content of a tracked file as a blob, or returns `None` if the file is gone.
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
            what: "a link whose target is not UTF-8",
        })?;
        (FileMode::Symlink, target.as_bytes().to_vec())
    } else if kind.is_file() {
        let content =
            fs::read(&full).map_err(|source| SnapshotError::io("read the file", &full, source))?;
        (file_mode(&metadata, file.mode), content)
    } else if kind.is_dir() {
        // A directory has taken the place of the file; what is below it is not tracked.
        return Ok(None);
    } else {
        return Err(SnapshotError::Unsupported {
            path: file.path.clone(),
            what: "a special file",
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
    /// The working-copy commit cannot be read.
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
    /// A tracked path is a special file (a device, a socket, a pipe) or a link that cannot be
    /// saved. It is not skipped, because that would drop it from the history without a word.
    Unsupported {
        /// The tracked path.
        path: String,
        /// What it is.
        what: &'static str,
    },
    /// A file of the working directory cannot be read.
    Io {
        /// What was being done.
        operation: &'static str,
        /// The file involved.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// The paths of the tree do not make an index, which the rules of the tree rule out.
    Index(IndexError),
    /// The tree of the snapshot could not be written.
    WriteTree(WriteTreeError),
}

impl SnapshotError {
    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
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
                write!(f, "the working-copy commit {id} is not valid: {source}")
            }
            Self::Tree { id, source } => write!(f, "the tree {id} is not valid: {source}"),
            Self::Unsupported { path, what } => write!(
                f,
                "`{}` is tracked, but it is {what}, which a snapshot cannot save",
                path.escape_debug()
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
            Self::Index(error) => write!(f, "the tracked paths make no index: {error}"),
            Self::WriteTree(error) => {
                write!(f, "could not write the tree of the snapshot: {error}")
            }
        }
    }
}

impl Error for SnapshotError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::ChangeId;
    use crate::repository::InitOptions;
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

    fn current(repository: &Repository) -> Commit {
        let id = repository.refs().working_copy().unwrap();
        read_commit(repository.objects(), id).unwrap()
    }

    /// Makes the files and a working-copy commit that tracks them, with the message "work", by
    /// the move that a command will make; there is no command to track files yet.
    fn track(repository: &Repository, dir: &TempDir, files: &[(&str, &str)]) {
        let mut index = Index::new();
        for (path, content) in files {
            write(dir, path, content);
            let id = repository
                .objects()
                .put_blob(&Blob::new(content.as_bytes()))
                .unwrap();
            index.insert(*path, FileMode::File, id).unwrap();
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
    }

    fn snapshot(repository: &Repository) -> Option<Entry> {
        repository.snapshot(&other_actor(), &AllowAll).unwrap()
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
        assert_eq!(entry.change().operation().as_str(), "snapshot");
        assert_eq!(entry.change().actor(), &other_actor());
        let moved = entry.change().working_copy().unwrap();
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
        assert_ne!(change, ChangeId::generate().unwrap());
    }
}
