//! What the working copy holds that its parent does not, and what is not tracked (ADR-010).
//!
//! [`Repository::status`] takes the snapshot of the tracked files, as every command does that does
//! not only read the log, so the working-copy commit is what the tracked files hold now. It then
//! compares the tree of that commit with the tree of its parent: a file that the commit adds,
//! changes, or deletes. The files in the working directory that are not in the tree of the
//! working-copy commit and that no pattern of the exclusion file (ADR-012) excludes are untracked.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io;

use super::snapshot::{read_commit, read_object, tracked_files};
use super::track::walk;
use super::{Repository, SnapshotError, TrackError, UpdateError};
use crate::ignore::{IgnoreError, IgnoreRules};
use crate::object::{CommitId, EntryMode, Kind, Tree, TreeEntry, TreeId};
use crate::oplog::{Entry, OperationName};
use crate::refs::{Head, RefError};
use crate::store::ObjectStore;
use crate::{Actor, Authorizer};

/// The operation that the authorizer is asked about.
const OPERATION: &str = "status";

/// How a file of the working-copy commit differs from the parent. A file that becomes a directory
/// (or the reverse) is a `Deleted` and the `Added` files below the directory, as the paths differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FileChangeKind {
    /// The file is in the working-copy commit and not in the parent.
    Added,
    /// The file is in both, with another content, or another mode (the executable bit, or a link
    /// in place of a file).
    Modified,
    /// The file is in the parent and not in the working-copy commit.
    Deleted,
}

impl FileChangeKind {
    /// The letter that `stemma status` writes: `A`, `M`, or `D`.
    pub fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
        }
    }
}

/// A file that the working-copy commit changes against its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileChange {
    /// The path, relative to the working directory and written with `/`.
    pub path: String,
    /// What the commit does to it.
    pub kind: FileChangeKind,
}

/// The snapshot that a call made: an entry in the log, and the working-copy commit moved.
#[derive(Debug)]
#[non_exhaustive]
pub struct Saved {
    /// The entry of the snapshot.
    pub entry: Entry,
    /// The pointer moved, but a directory could not be flushed to disk, so the move may not
    /// survive a crash. The caller reports it after it has shown the result.
    pub late: Option<RefError>,
}

/// What [`Repository::status`] found.
#[derive(Debug)]
#[must_use]
#[non_exhaustive]
pub struct Status {
    /// What HEAD names: a branch (which may not exist yet), or a commit.
    pub head: Head,
    /// The working-copy commit after the snapshot.
    pub working_copy: CommitId,
    /// The parent that the changes are against, or `None` if the working-copy commit has none (the
    /// changes are then against the empty tree). A commit with several parents is compared with
    /// the first.
    pub parent: Option<CommitId>,
    /// The files that the working-copy commit changes, sorted by path.
    pub changes: Vec<FileChange>,
    /// The files in the working directory that are not in the tree of the working-copy commit and
    /// that no pattern of `.stemmaignore` excludes, sorted by path. A directory is not listed,
    /// its files are.
    pub untracked: Vec<String>,
    /// The snapshot that this call made, or `None` if the tracked files were as the working-copy
    /// commit has them.
    pub snapshot: Option<Saved>,
}

impl Status {
    /// The flush error of the snapshot, if its move may not survive a crash: the caller reports it
    /// after it has shown the status, and the exit code says that the work is done.
    pub fn late(&self) -> Option<&RefError> {
        self.snapshot.as_ref().and_then(|saved| saved.late.as_ref())
    }
}

impl Repository {
    /// Takes a snapshot of the tracked files, and reports what the working-copy commit changes
    /// against its parent and what is not tracked (ADR-010).
    ///
    /// The authorizer is asked for `status` on the working copy before anything is read, and
    /// again for the snapshot. The exclusion file is read and the working directory is walked
    /// **before** the snapshot, so an invalid file, a name that a tree refuses (and no pattern
    /// excludes), or a name that is not UTF-8 stops the call before it writes anything: those are
    /// the inputs where [`track`](Self::track) stops, and here nothing is left behind. The snapshot
    /// is the one of [`snapshot`](Self::snapshot): it adds an entry to the operation log if a
    /// tracked file changed, which is why the call has an actor.
    ///
    /// The references are read one after the other and not under a lock, so a process that moves
    /// a pointer in between can make HEAD and the working-copy commit come from two states. A
    /// move of the working-copy commit during the snapshot is refused by the snapshot
    /// ([`RefError::StaleWorkingCopy`]) and not reported wrongly.
    ///
    /// # Errors
    ///
    /// Returns [`StatusError::Update`] if the authorizer refuses `status`,
    /// [`StatusError::Ignore`] and [`StatusError::Walk`] for the exclusion file and the working
    /// directory (nothing is written), and [`StatusError::Snapshot`] if the snapshot fails. After
    /// the snapshot, [`StatusError::Refs`] and [`StatusError::Read`] cover reading the references,
    /// the commits, and the trees; they carry what the snapshot did ([`StatusError::saved`]). A
    /// snapshot that moved the pointer but could not flush it is not an error: see
    /// [`Saved::late`].
    pub fn status(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Status, StatusError> {
        let operation = OperationName::new(OPERATION).expect("`status` is a valid operation name");
        self.authorize(actor, authorizer, &operation, "working-copy")
            .map_err(StatusError::Update)?;
        // What does not depend on the snapshot is read first, so that its failures leave nothing.
        let rules = IgnoreRules::read(self.path()).map_err(StatusError::Ignore)?;
        let mut found = BTreeSet::new();
        walk(self.path().to_path_buf(), "", &rules, &mut found).map_err(StatusError::Walk)?;
        let saved = match self.snapshot(actor, authorizer) {
            Ok(entry) => entry.map(|entry| Saved { entry, late: None }),
            // The move happened: the entry is in the error, and it is a result and not a failure.
            Err(SnapshotError::Update(UpdateError::NotDurable { entry, source })) => Some(Saved {
                entry: *entry,
                late: Some(source),
            }),
            Err(error) => return Err(StatusError::Snapshot(error)),
        };
        match self.read_state() {
            Ok((head, working_copy, parent, changes, tracked)) => Ok(Status {
                head,
                working_copy,
                parent,
                changes,
                untracked: found.difference(&tracked).cloned().collect(),
                snapshot: saved,
            }),
            Err(Failure::Refs(source)) => Err(StatusError::Refs {
                source,
                saved: saved.map(Box::new),
            }),
            Err(Failure::Read(source)) => Err(StatusError::Read {
                source,
                saved: saved.map(Box::new),
            }),
        }
    }

    /// What the status needs after the snapshot: HEAD, the working-copy commit and its parent, the
    /// changes between their trees, and the paths of the files of the working-copy tree.
    #[allow(clippy::type_complexity)]
    fn read_state(
        &self,
    ) -> Result<
        (
            Head,
            CommitId,
            Option<CommitId>,
            Vec<FileChange>,
            BTreeSet<String>,
        ),
        Failure,
    > {
        let working_copy = self.refs().working_copy().map_err(Failure::Refs)?;
        let head = self.refs().head().map_err(Failure::Refs)?;
        let commit = read_commit(self.objects(), working_copy).map_err(Failure::Read)?;
        let parent = commit.parents().first().copied();
        let old = match parent {
            Some(parent) => Some(
                read_commit(self.objects(), parent)
                    .map_err(Failure::Read)?
                    .tree(),
            ),
            None => None,
        };
        let changes = compare(self.objects(), old, commit.tree()).map_err(Failure::Read)?;
        let tracked = tracked_files(self.objects(), commit.tree())
            .map_err(Failure::Read)?
            .into_iter()
            .map(|file| file.path)
            .collect();
        Ok((head, working_copy, parent, changes, tracked))
    }
}

/// A failure after the snapshot.
enum Failure {
    Refs(RefError),
    Read(SnapshotError),
}

/// Compares the tree `new` with `old` (`None` is the empty tree) and returns the files that
/// differ, sorted by path.
///
/// Both trees are read directory by directory, with an explicit stack, and a directory that has
/// the same ID in both is skipped without being read, so the cost grows with what differs.
fn compare(
    objects: &dyn ObjectStore,
    old: Option<TreeId>,
    new: TreeId,
) -> Result<Vec<FileChange>, SnapshotError> {
    let mut changes = Vec::new();
    let mut pending = vec![(String::new(), old, Some(new))];
    while let Some((prefix, old, new)) = pending.pop() {
        let old_tree = read_tree(objects, old)?;
        let new_tree = read_tree(objects, new)?;
        let mut left = old_tree.entries().iter().peekable();
        let mut right = new_tree.entries().iter().peekable();
        loop {
            // The entries of a tree are sorted by name, so two walks side by side meet equal
            // names together.
            let order = match (left.peek(), right.peek()) {
                (None, None) => break,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(a), Some(b)) => a.name().cmp(b.name()),
            };
            let (before, after) = match order {
                Ordering::Less => (left.next(), None),
                Ordering::Greater => (None, right.next()),
                Ordering::Equal => (left.next(), right.next()),
            };
            let name = match (before, after) {
                (Some(entry), _) | (None, Some(entry)) => entry.name(),
                (None, None) => unreachable!("`order` says that a side has an entry"),
            };
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let directory = |entry: &TreeEntry| entry.mode() == EntryMode::Directory;
            match (before, after) {
                (Some(a), Some(b)) if directory(a) && directory(b) => {
                    if a.id() != b.id() {
                        pending.push((path, Some(TreeId::new(a.id())), Some(TreeId::new(b.id()))));
                    }
                }
                (Some(a), Some(b)) if !directory(a) && !directory(b) => {
                    if a.mode() != b.mode() || a.id() != b.id() {
                        changes.push(FileChange {
                            path,
                            kind: FileChangeKind::Modified,
                        });
                    }
                }
                _ => {
                    // Only on one side, or a file on one side and a directory on the other: what
                    // the old side has goes, and what the new side has comes.
                    if let Some(entry) = before {
                        if directory(entry) {
                            pending.push((path.clone(), Some(TreeId::new(entry.id())), None));
                        } else {
                            changes.push(FileChange {
                                path: path.clone(),
                                kind: FileChangeKind::Deleted,
                            });
                        }
                    }
                    if let Some(entry) = after {
                        if directory(entry) {
                            pending.push((path, None, Some(TreeId::new(entry.id()))));
                        } else {
                            changes.push(FileChange {
                                path,
                                kind: FileChangeKind::Added,
                            });
                        }
                    }
                }
            }
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

/// Reads a tree, or the empty tree for `None`.
fn read_tree(objects: &dyn ObjectStore, id: Option<TreeId>) -> Result<Tree, SnapshotError> {
    let Some(id) = id else {
        return Ok(Tree::new(Vec::new()).expect("a tree without entries has no duplicate"));
    };
    let raw = read_object(objects, id.object_id(), Kind::Tree)?;
    Tree::parse(raw.body()).map_err(|source| SnapshotError::Tree { id, source })
}

/// Why [`Repository::status`] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum StatusError {
    /// The authorizer refused `status`. Nothing was read or written.
    Update(UpdateError),
    /// `.stemmaignore` cannot be read or is not valid. Nothing was written: it is read before the
    /// snapshot.
    Ignore(IgnoreError),
    /// The working directory could not be listed, or holds a name that a tree refuses (and no
    /// pattern excludes) or that is not UTF-8. Only these two cases and an error of the file
    /// system occur, and nothing was written. The message is the one of [`Repository::track`].
    Walk(TrackError),
    /// The snapshot failed. If it moved the pointer, [`pointer_moved`](Self::pointer_moved) says
    /// so.
    Snapshot(SnapshotError),
    /// HEAD or the working-copy commit could not be read after the snapshot.
    Refs {
        /// What failed.
        source: RefError,
        /// What the snapshot did before: its entry is in the log.
        saved: Option<Box<Saved>>,
    },
    /// A commit or a tree could not be read or compared after the snapshot.
    Read {
        /// What failed.
        source: SnapshotError,
        /// What the snapshot did before: its entry is in the log.
        saved: Option<Box<Saved>>,
    },
}

impl StatusError {
    /// What the snapshot did before this failure, if it moved the working-copy commit. The failure
    /// is then one of reading, and the entry is in the log.
    pub fn saved(&self) -> Option<&Saved> {
        match self {
            Self::Refs { saved, .. } | Self::Read { saved, .. } => saved.as_deref(),
            _ => None,
        }
    }

    /// The entry that the failure left in the log, if one: the snapshot's.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Snapshot(SnapshotError::Update(error)) => error.entry(),
            other => other.saved().map(|saved| &saved.entry),
        }
    }

    /// Whether the working-copy commit moved although the call failed. The next call finds the
    /// tracked files saved, so there is nothing to repeat.
    pub fn pointer_moved(&self) -> bool {
        match self {
            Self::Snapshot(SnapshotError::Update(error)) => error.pointer_moved(),
            other => other.saved().is_some(),
        }
    }

    /// Returns the error of the file system, if that is what failed. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Update(error) => error.io_error(),
            Self::Ignore(error) => error.io_error(),
            Self::Walk(error) => error.io_error(),
            Self::Snapshot(error) | Self::Read { source: error, .. } => error.io_error(),
            Self::Refs { source, .. } => source.io_error(),
        }
    }
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Update(error) => error.fmt(f),
            Self::Ignore(error) => write!(f, "{error}; nothing was written"),
            Self::Walk(error) => write!(f, "{error}; nothing was written"),
            Self::Snapshot(error) => error.fmt(f),
            Self::Refs { source, saved } => write!(
                f,
                "could not read the references: {source}{}",
                saved_note(saved)
            ),
            Self::Read { source, saved } => write!(
                f,
                "could not read the working-copy commit or its parent: {source}{}",
                saved_note(saved)
            ),
        }
    }
}

/// The sentence that says the snapshot of the call stays.
fn saved_note(saved: &Option<Box<Saved>>) -> String {
    match saved {
        Some(saved) => format!(
            "; the tracked files were saved before as entry {} of the operation log",
            saved.entry.number()
        ),
        None => String::new(),
    }
}

impl Error for StatusError {}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
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

    fn other() -> Actor {
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

    fn track(repository: &Repository, path: &str) {
        repository
            .track(&[path.to_owned()], &actor(), &AllowAll)
            .unwrap();
    }

    fn commit(repository: &Repository, message: &str) {
        let _ = repository.commit(message, &actor(), &AllowAll).unwrap();
    }

    fn status(repository: &Repository) -> Status {
        repository.status(&other(), &AllowAll).unwrap()
    }

    fn kinds(status: &Status) -> Vec<(char, &str)> {
        status
            .changes
            .iter()
            .map(|change| (change.kind.letter(), change.path.as_str()))
            .collect()
    }

    #[test]
    fn a_new_repository_is_clean_and_names_head_and_no_parent() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let entries = repository.oplog().entries().unwrap().len();

        let status = status(&repository);

        assert!(status.changes.is_empty() && status.untracked.is_empty());
        assert_eq!(status.parent, None);
        assert_eq!(
            status.working_copy,
            repository.refs().working_copy().unwrap()
        );
        assert!(matches!(&status.head, Head::Branch(name) if name.as_str() == "main"));
        assert!(status.snapshot.is_none() && status.late().is_none());
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[test]
    fn before_the_first_commit_every_tracked_file_is_added() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "b.txt", "b");
        write(&dir, "d/a.txt", "a");
        track(&repository, "b.txt");
        track(&repository, "d");

        let status = status(&repository);

        assert_eq!(kinds(&status), [('A', "b.txt"), ('A', "d/a.txt")]);
    }

    #[test]
    fn after_a_commit_the_new_working_copy_has_no_changes() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        commit(&repository, "one");

        let status = status(&repository);

        assert!(status.changes.is_empty(), "{:?}", status.changes);
        assert!(status.parent.is_some());
    }

    #[test]
    fn added_modified_and_deleted_files_are_listed_by_path_with_their_letters() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "keep.txt", "keep");
        write(&dir, "change.txt", "one");
        write(&dir, "gone.txt", "gone");
        write(&dir, "d/deep/x.txt", "x");
        write(&dir, "d/deep/y.txt", "y");
        track(&repository, ".");
        commit(&repository, "base");
        write(&dir, "change.txt", "two");
        write(&dir, "d/deep/y.txt", "yy");
        fs::remove_file(dir.path().join("gone.txt")).unwrap();
        write(&dir, "new.txt", "new");
        track(&repository, "new.txt");

        let status = status(&repository);

        assert_eq!(
            kinds(&status),
            [
                ('M', "change.txt"),
                ('M', "d/deep/y.txt"),
                ('D', "gone.txt"),
                ('A', "new.txt"),
            ]
        );
        // `track` saved the changes already, so there is nothing left for the snapshot.
        assert!(status.snapshot.is_none());
    }

    #[test]
    fn a_whole_directory_that_is_added_or_deleted_lists_its_files() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "old/a.txt", "a");
        write(&dir, "old/sub/b.txt", "b");
        track(&repository, "old");
        commit(&repository, "base");
        fs::remove_dir_all(dir.path().join("old")).unwrap();
        write(&dir, "fresh/c.txt", "c");
        track(&repository, "fresh");

        let status = status(&repository);

        assert_eq!(
            kinds(&status),
            [
                ('A', "fresh/c.txt"),
                ('D', "old/a.txt"),
                ('D', "old/sub/b.txt"),
            ]
        );
    }

    #[test]
    fn a_file_that_becomes_a_directory_is_a_delete_and_adds() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "x", "file");
        track(&repository, "x");
        commit(&repository, "base");
        fs::remove_file(dir.path().join("x")).unwrap();
        write(&dir, "x/inner.txt", "inner");
        track(&repository, "x");

        let status = status(&repository);

        assert_eq!(kinds(&status), [('D', "x"), ('A', "x/inner.txt")]);
    }

    #[cfg(unix)]
    #[test]
    fn a_change_of_the_executable_bit_or_to_a_link_is_a_modification() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "run.sh", "echo");
        write(&dir, "swap", "file");
        track(&repository, ".");
        commit(&repository, "base");
        let path = dir.path().join("run.sh");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(dir.path().join("swap")).unwrap();
        std::os::unix::fs::symlink("run.sh", dir.path().join("swap")).unwrap();

        let status = status(&repository);

        assert_eq!(kinds(&status), [('M', "run.sh"), ('M', "swap")]);
    }

    #[test]
    fn a_file_that_has_the_old_content_again_is_no_change() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        commit(&repository, "base");
        write(&dir, "a.txt", "two");
        let _ = status(&repository);
        write(&dir, "a.txt", "one");

        let status = status(&repository);

        assert!(status.changes.is_empty(), "{:?}", status.changes);
    }

    #[test]
    fn the_files_that_are_not_tracked_are_listed_one_by_one_and_sorted() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "a");
        track(&repository, "a.txt");
        write(&dir, "z.txt", "z");
        write(&dir, "notes/one.md", "1");
        write(&dir, "notes/deep/two.md", "2");
        write(&dir, "a.txt.bak", "b");

        let status = status(&repository);

        assert_eq!(
            status.untracked,
            ["a.txt.bak", "notes/deep/two.md", "notes/one.md", "z.txt"]
        );
        assert_eq!(kinds(&status), [('A', "a.txt")]);
    }

    #[test]
    fn a_tracked_file_below_a_tracked_directory_is_not_untracked_and_a_new_one_is() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "d/old.txt", "old");
        track(&repository, "d");
        write(&dir, "d/new.txt", "new");

        let status = status(&repository);

        assert_eq!(status.untracked, ["d/new.txt"]);
    }

    #[test]
    fn what_the_exclusion_file_excludes_is_not_untracked_but_the_file_itself_is() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, ".stemmaignore", "*.log\ntarget/\n");
        write(&dir, "a.txt", "a");
        write(&dir, "d/x.log", "log");
        write(&dir, "target/debug/z", "z");

        let status = status(&repository);

        assert_eq!(status.untracked, [".stemmaignore", "a.txt"]);
    }

    #[test]
    fn a_tracked_file_that_a_pattern_excludes_stays_in_the_changes() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "secret.env", "one");
        track(&repository, "secret.env");
        write(&dir, ".stemmaignore", "*.env\n");

        let status = status(&repository);

        assert_eq!(kinds(&status), [('A', "secret.env")]);
        assert_eq!(status.untracked, [".stemmaignore"]);
    }

    #[test]
    fn the_metadata_directory_is_never_listed() {
        let dir = TempDir::new();
        let repository = repository(&dir);

        assert!(status(&repository).untracked.is_empty());
    }

    #[test]
    fn an_invalid_exclusion_file_stops_the_status_and_a_missing_one_does_not() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, ".stemmaignore", "!broken\n");

        let error = repository.status(&other(), &AllowAll).unwrap_err();

        assert!(matches!(&error, StatusError::Ignore(_)), "{error}");
        assert!(error.to_string().contains("line 1: negation"), "{error}");
    }

    #[test]
    fn a_name_that_a_tree_refuses_stops_the_status_unless_a_pattern_excludes_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "d/a\u{200B}b.txt", "x");

        let error = repository.status(&other(), &AllowAll).unwrap_err();
        assert!(
            matches!(&error, StatusError::Walk(TrackError::InvalidName { .. })),
            "{error}"
        );

        write(&dir, ".stemmaignore", "*.txt\n");
        let status = status(&repository);
        assert_eq!(status.untracked, [".stemmaignore"]);
    }

    #[test]
    fn a_change_is_saved_by_a_snapshot_with_the_actor_of_the_call_and_only_once() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        write(&dir, "a.txt", "two");
        let entries = repository.oplog().entries().unwrap().len();

        let first = status(&repository);
        let second = status(&repository);

        let snapshot = first.snapshot.expect("the changed file is saved");
        assert!(snapshot.late.is_none());
        assert_eq!(snapshot.entry.change().actor(), &other());
        assert_eq!(snapshot.entry.change().operation().as_str(), "snapshot");
        assert!(second.snapshot.is_none());
        assert_eq!(repository.oplog().entries().unwrap().len(), entries + 1);
        assert_eq!(first.working_copy, second.working_copy);
    }

    #[test]
    fn a_refusal_by_the_authorizer_changes_nothing_and_reads_nothing_else() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, ".stemmaignore", "!broken\n");
        let entries = repository.oplog().entries().unwrap().len();

        let error = repository.status(&other(), &DenyAll).unwrap_err();

        assert!(
            matches!(&error, StatusError::Update(UpdateError::Denied { operation, .. })
                if operation == "status"),
            "{error}"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[test]
    fn a_detached_head_is_named() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        commit(&repository, "one");
        let tip = repository.refs().resolve_head().unwrap().unwrap();
        let operation = OperationName::new("checkout").unwrap();
        repository
            .set_head(&operation, &Head::Detached(tip), &actor(), &AllowAll)
            .unwrap();

        let status = status(&repository);

        assert_eq!(status.head, Head::Detached(tip));
    }

    /// The file below `dir` whose name ends with `suffix`.
    fn find(dir: &std::path::Path, suffix: &str) -> Option<std::path::PathBuf> {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if let Some(found) = find(&path, suffix) {
                    return Some(found);
                }
            } else if path.to_string_lossy().ends_with(suffix) {
                return Some(path);
            }
        }
        None
    }

    /// The tree of a commit, and the ID of its entry `name`.
    fn tree_and_entry(repository: &Repository, commit: CommitId, name: &str) -> (TreeId, TreeId) {
        let tree = read_commit(repository.objects(), commit).unwrap().tree();
        let raw = read_object(repository.objects(), tree.object_id(), Kind::Tree).unwrap();
        let parsed = Tree::parse(raw.body()).unwrap();
        let entry = parsed.entries().iter().find(|e| e.name() == name).unwrap();
        (tree, TreeId::new(entry.id()))
    }

    #[test]
    fn a_directory_with_the_same_id_on_both_sides_is_not_read() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "big/a.txt", "a");
        write(&dir, "big/sub/b.txt", "b");
        write(&dir, "deep/one/two/x.txt", "x");
        write(&dir, "deep/one/two/y.txt", "y");
        write(&dir, "small.txt", "s");
        track(&repository, ".");
        commit(&repository, "base");
        let parent = repository.refs().resolve_head().unwrap().unwrap();
        write(&dir, "small.txt", "t");
        write(&dir, "deep/one/two/y.txt", "yy");
        let working = status(&repository).working_copy;
        let (old_tree, _) = tree_and_entry(&repository, parent, "big");
        let (new_tree, big) = tree_and_entry(&repository, working, "big");
        // The tree object of `big` is gone: a comparison that reads it fails.
        let hex = big.object_id().to_string();
        let file = find(&dir.path().join(".stemma/objects"), &hex[hex.len() - 8..]).unwrap();
        fs::remove_file(file).unwrap();

        let changes = compare(repository.objects(), Some(old_tree), new_tree).unwrap();

        let found: Vec<_> = changes
            .iter()
            .map(|change| (change.kind.letter(), change.path.as_str()))
            .collect();
        assert_eq!(found, [('M', "deep/one/two/y.txt"), ('M', "small.txt")]);
        // A directory that differs is read, so the same store fails when it is missing.
        let (_, deep) = tree_and_entry(&repository, working, "deep");
        let hex = deep.object_id().to_string();
        fs::remove_file(find(&dir.path().join(".stemma/objects"), &hex[hex.len() - 8..]).unwrap())
            .unwrap();
        assert!(compare(repository.objects(), Some(old_tree), new_tree).is_err());
    }

    #[test]
    fn a_directory_that_becomes_a_file_is_a_delete_below_and_an_add() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "x/inner.txt", "inner");
        track(&repository, "x");
        commit(&repository, "base");
        fs::remove_dir_all(dir.path().join("x")).unwrap();
        write(&dir, "x", "now a file");
        track(&repository, "x");

        let status = status(&repository);

        assert_eq!(kinds(&status), [('A', "x"), ('D', "x/inner.txt")]);
    }

    #[test]
    fn a_directory_next_to_names_that_sort_around_a_slash_is_paired_by_name() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a/x", "x");
        write(&dir, "a-b", "b");
        write(&dir, "a.txt", "t");
        track(&repository, ".");
        commit(&repository, "base");
        write(&dir, "a/x", "xx");
        write(&dir, "a-c", "c");
        fs::remove_file(dir.path().join("a.txt")).unwrap();
        track(&repository, "a-c");

        let status = status(&repository);

        assert_eq!(kinds(&status), [('A', "a-c"), ('D', "a.txt"), ('M', "a/x")]);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_whose_target_changes_and_a_file_that_loses_its_executable_bit_are_modified() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "run.sh", "echo");
        fs::set_permissions(dir.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("one", dir.path().join("link")).unwrap();
        track(&repository, ".");
        commit(&repository, "base");
        fs::set_permissions(dir.path().join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_file(dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink("two", dir.path().join("link")).unwrap();

        let status = status(&repository);

        assert_eq!(kinds(&status), [('M', "link"), ('M', "run.sh")]);
    }

    #[test]
    fn deleting_every_tracked_file_lists_them_all_as_deleted() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "a");
        write(&dir, "d/b.txt", "b");
        track(&repository, ".");
        commit(&repository, "base");
        fs::remove_file(dir.path().join("a.txt")).unwrap();
        fs::remove_dir_all(dir.path().join("d")).unwrap();

        let status = status(&repository);

        assert_eq!(kinds(&status), [('D', "a.txt"), ('D', "d/b.txt")]);
        assert!(status.untracked.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_directory_and_a_dangling_link_are_untracked_files_and_an_empty_directory_is_not()
    {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "real/x", "x");
        std::os::unix::fs::symlink("real", dir.path().join("to-dir")).unwrap();
        std::os::unix::fs::symlink("gone", dir.path().join("dangling")).unwrap();
        fs::create_dir(dir.path().join("empty")).unwrap();

        let status = status(&repository);

        assert_eq!(status.untracked, ["dangling", "real/x", "to-dir"]);
    }

    #[test]
    fn a_tracked_file_that_is_deleted_is_a_delete_and_one_that_returns_is_a_change_not_untracked() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        commit(&repository, "base");
        fs::remove_file(dir.path().join("a.txt")).unwrap();
        let gone = status(&repository);
        assert_eq!(kinds(&gone), [('D', "a.txt")]);
        assert!(gone.untracked.is_empty());

        // It is not in the tree of the working-copy commit any more, so it is untracked now.
        write(&dir, "a.txt", "back");
        let back = status(&repository);

        assert_eq!(kinds(&back), [('D', "a.txt")]);
        assert_eq!(back.untracked, ["a.txt"]);
    }

    #[test]
    fn the_changes_of_a_call_are_those_of_the_snapshot_that_it_made() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        commit(&repository, "base");
        write(&dir, "a.txt", "two");

        let status = status(&repository);

        assert!(status.snapshot.is_some());
        assert_eq!(kinds(&status), [('M', "a.txt")]);
    }

    #[test]
    fn a_touched_file_with_the_same_content_makes_no_snapshot() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let entries = repository.oplog().entries().unwrap().len();
        write(&dir, "a.txt", "one");

        let status = status(&repository);

        assert!(status.snapshot.is_none());
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[test]
    fn a_detached_head_is_compared_like_any_other() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        commit(&repository, "one");
        let tip = repository.refs().resolve_head().unwrap().unwrap();
        let operation = OperationName::new("checkout").unwrap();
        repository
            .set_head(&operation, &Head::Detached(tip), &actor(), &AllowAll)
            .unwrap();
        write(&dir, "a.txt", "two");

        let status = status(&repository);

        assert_eq!(status.head, Head::Detached(tip));
        assert_eq!(status.parent, Some(tip));
        assert_eq!(kinds(&status), [('M', "a.txt")]);
    }

    #[test]
    fn a_refusal_for_the_snapshot_only_comes_after_the_reads_and_writes_nothing() {
        struct OnlySnapshotDenied;
        impl Authorizer for OnlySnapshotDenied {
            fn authorize(&self, _actor: &Actor, operation: &str, _target: &str) -> Decision {
                if operation == "snapshot" {
                    Decision::Deny
                } else {
                    Decision::Allow
                }
            }
        }
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        write(&dir, "a.txt", "two");
        let entries = repository.oplog().entries().unwrap().len();

        let error = repository
            .status(&other(), &OnlySnapshotDenied)
            .unwrap_err();

        assert!(
            matches!(
                &error,
                StatusError::Snapshot(SnapshotError::Update(UpdateError::Denied { .. }))
            ),
            "{error}"
        );
        assert!(!error.pointer_moved() && error.entry().is_none());
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[test]
    fn an_invalid_exclusion_file_and_a_refused_name_leave_no_snapshot_behind() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        write(&dir, "a.txt", "two");
        write(&dir, ".stemmaignore", "!broken\n");
        let entries = repository.oplog().entries().unwrap().len();

        let error = repository.status(&other(), &AllowAll).unwrap_err();

        assert!(matches!(&error, StatusError::Ignore(_)), "{error}");
        assert!(
            error.to_string().ends_with("; nothing was written"),
            "{error}"
        );
        assert!(!error.pointer_moved());
        // The change in `a.txt` is still not saved: the call stopped first.
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);

        write(&dir, ".stemmaignore", "*.log\n");
        write(&dir, "d/a\u{200B}b.txt", "x");
        let error = repository.status(&other(), &AllowAll).unwrap_err();
        assert!(matches!(&error, StatusError::Walk(_)), "{error}");
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[cfg(unix)]
    #[test]
    fn a_snapshot_that_cannot_be_flushed_is_a_result_with_its_entry_and_the_flush_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        commit(&repository, "base");
        write(&dir, "a.txt", "two");
        // The metadata directory may be written and entered, but not read: the pointer moves, and
        // the flush of the directory cannot open it.
        let metadata = repository.metadata_dir();
        fs::set_permissions(&metadata, fs::Permissions::from_mode(0o300)).unwrap();
        let probe = fs::read_dir(&metadata).is_err();
        let result = repository.status(&other(), &AllowAll);
        fs::set_permissions(&metadata, fs::Permissions::from_mode(0o755)).unwrap();
        if !probe {
            return;
        }

        let status = result.unwrap();

        let saved = status.snapshot.as_ref().expect("the snapshot is a result");
        assert!(matches!(&saved.late, Some(RefError::NotFlushed { .. })));
        assert!(status.late().is_some());
        assert_eq!(saved.entry.change().operation().as_str(), "snapshot");
        assert_eq!(
            status.working_copy,
            repository.refs().working_copy().unwrap()
        );
        assert_eq!(kinds(&status), [('M', "a.txt")]);
    }

    #[test]
    fn a_failure_of_reading_after_the_snapshot_carries_the_snapshot_that_stays() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        commit(&repository, "base");
        write(&dir, "a.txt", "two");
        // The parent of the working-copy commit is the commit that the snapshot keeps: remove it.
        let parent = repository.refs().resolve_head().unwrap().unwrap();
        let hex = parent.object_id().to_string();
        fs::remove_file(find(&dir.path().join(".stemma/objects"), &hex[hex.len() - 8..]).unwrap())
            .unwrap();

        let error = repository.status(&other(), &AllowAll).unwrap_err();

        assert!(
            matches!(&error, StatusError::Read { saved: Some(_), .. }),
            "{error}"
        );
        assert!(error.pointer_moved());
        let entry = error.entry().expect("the snapshot entry is in the log");
        assert_eq!(entry.change().operation().as_str(), "snapshot");
        assert!(
            error
                .to_string()
                .contains(&format!("saved before as entry {}", entry.number())),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("could not read the working-copy commit"),
            "{error}"
        );
    }

    #[test]
    fn the_error_helpers_cover_every_variant() {
        let entry = {
            let dir = TempDir::new();
            repository(&dir).oplog().entries().unwrap().remove(0)
        };
        let io = || RefError::Io {
            operation: "read",
            path: "x".into(),
            source: io::Error::other("disk"),
        };
        let saved = || {
            Some(Box::new(Saved {
                entry: entry.clone(),
                late: None,
            }))
        };
        let refs = StatusError::Refs {
            source: io(),
            saved: saved(),
        };
        assert!(refs.pointer_moved() && refs.entry() == Some(&entry) && refs.io_error().is_some());
        assert!(
            refs.to_string()
                .starts_with("could not read the references: "),
            "{refs}"
        );
        let bare = StatusError::Refs {
            source: RefError::Locked { path: "x".into() },
            saved: None,
        };
        assert!(!bare.pointer_moved() && bare.entry().is_none() && bare.io_error().is_none());
        let not_durable = StatusError::Snapshot(SnapshotError::Update(UpdateError::NotDurable {
            entry: Box::new(entry.clone()),
            source: RefError::NotFlushed {
                path: "x".into(),
                source: io::Error::other("disk"),
            },
        }));
        assert!(not_durable.pointer_moved() && not_durable.entry().is_some());
        assert!(not_durable.io_error().is_some());
        for plain in [
            StatusError::Update(UpdateError::Denied {
                operation: "status".into(),
                target: "working-copy".into(),
            }),
            StatusError::Ignore(IgnoreError::Changed { path: "x".into() }),
            StatusError::Walk(TrackError::InvalidName { path: "x".into() }),
        ] {
            assert!(!plain.pointer_moved() && plain.entry().is_none(), "{plain}");
            assert!(plain.saved().is_none());
        }
    }
}
