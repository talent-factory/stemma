//! What the working copy holds that its parent does not, and what is not tracked (ADR-010).
//!
//! [`Repository::status`] takes the snapshot first, as every command does, so the working-copy
//! commit is what the tracked files hold now. It then compares the tree of that commit with the
//! tree of its parent: a file that the commit adds, changes, or deletes. The files that are in the
//! directory and in neither the tree nor the exclusion file (ADR-012) are untracked.

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

/// How a file of the working-copy commit differs from the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChangeKind {
    /// The file is in the working-copy commit and not in the parent.
    Added,
    /// The file is in both, with another content, or another mode (the executable bit, or a link
    /// in place of a file).
    Modified,
    /// The file is in the parent and not in the working-copy commit.
    Deleted,
}

impl ChangeKind {
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
    pub kind: ChangeKind,
}

/// What [`Repository::status`] found.
#[derive(Debug)]
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
    /// The files in the working directory that are not tracked and that `.stemmaignore` does not
    /// exclude, sorted by path. A directory is not listed, its files are.
    pub untracked: Vec<String>,
    /// The entry of the snapshot that this call made, or `None` if the tracked files were as the
    /// working-copy commit has them.
    pub snapshot: Option<Entry>,
    /// The snapshot moved the working-copy commit, but a directory could not be flushed
    /// ([`UpdateError::NotDurable`]). The status is complete; the caller reports this after it
    /// has shown it.
    pub late: Option<UpdateError>,
}

impl Repository {
    /// Takes a snapshot of the tracked files, and reports what the working-copy commit changes
    /// against its parent and what is not tracked (ADR-010).
    ///
    /// The snapshot is the one of [`snapshot`](Self::snapshot): it adds an entry to the operation
    /// log if a tracked file changed, which is why the call has an actor. The authorizer is asked
    /// for `status` on the working copy, and again for the snapshot. The untracked files are the
    /// files below the directory that are in no tree and no pattern of `.stemmaignore` excludes;
    /// the walk stops where [`track`](Self::track) would stop, at a name that a tree refuses
    /// (unless a pattern excludes it) and at a name that is not UTF-8. The exclusion file is read
    /// after the snapshot, and an invalid one is an error here, as for `track`.
    ///
    /// # Errors
    ///
    /// Returns [`StatusError::Update`] if the authorizer refuses `status`, [`StatusError::Snapshot`]
    /// for the snapshot and for reading the commits and trees, [`StatusError::Refs`] for HEAD and
    /// the working-copy commit, [`StatusError::Ignore`] for an exclusion file that cannot be read,
    /// and [`StatusError::Walk`] for the working directory. A snapshot that moved the pointer but
    /// could not flush it is not an error: see [`Status::late`].
    pub fn status(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Status, StatusError> {
        let operation = OperationName::new(OPERATION).expect("`status` is a valid operation name");
        self.authorize(actor, authorizer, &operation, "working-copy")
            .map_err(StatusError::Update)?;
        let before = self.refs().working_copy().map_err(StatusError::Refs)?;
        let (snapshot, late) = match self.snapshot(actor, authorizer) {
            Ok(entry) => (entry, None),
            Err(SnapshotError::Update(error @ UpdateError::NotDurable { .. })) => {
                (None, Some(error))
            }
            Err(error) => return Err(StatusError::Snapshot(error)),
        };
        // After the snapshot the pointer is the commit that it made; if it was not durable, the
        // pointer is still the new one, and the entry is in the error.
        let working_copy = self.refs().working_copy().map_err(StatusError::Refs)?;
        debug_assert!(snapshot.is_none() || working_copy != before);
        let head = self.refs().head().map_err(StatusError::Refs)?;
        let commit = read_commit(self.objects(), working_copy).map_err(StatusError::Snapshot)?;
        let parent = commit.parents().first().copied();
        let old = match parent {
            Some(parent) => Some(
                read_commit(self.objects(), parent)
                    .map_err(StatusError::Snapshot)?
                    .tree(),
            ),
            None => None,
        };
        let changes = compare(self.objects(), old, commit.tree()).map_err(StatusError::Snapshot)?;
        let tracked: BTreeSet<String> = tracked_files(self.objects(), commit.tree())
            .map_err(StatusError::Snapshot)?
            .into_iter()
            .map(|file| file.path)
            .collect();
        let rules = IgnoreRules::read(self.path()).map_err(StatusError::Ignore)?;
        let mut found = BTreeSet::new();
        walk(self.path().to_path_buf(), "", &rules, &mut found).map_err(StatusError::Walk)?;
        let untracked = found.difference(&tracked).cloned().collect();
        Ok(Status {
            head,
            working_copy,
            parent,
            changes,
            untracked,
            snapshot,
            late,
        })
    }
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
            let Some(name) = before.or(after).map(TreeEntry::name) else {
                break;
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
                            kind: ChangeKind::Modified,
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
                                kind: ChangeKind::Deleted,
                            });
                        }
                    }
                    if let Some(entry) = after {
                        if directory(entry) {
                            pending.push((path, None, Some(TreeId::new(entry.id()))));
                        } else {
                            changes.push(FileChange {
                                path,
                                kind: ChangeKind::Added,
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
    /// The authorizer refused `status`. No entry was written.
    Update(UpdateError),
    /// The snapshot failed, or a commit or a tree could not be read.
    Snapshot(SnapshotError),
    /// HEAD or the working-copy commit could not be read.
    Refs(RefError),
    /// `.stemmaignore` cannot be read or is not valid.
    Ignore(IgnoreError),
    /// The working directory could not be listed, or holds a name that a tree refuses (and no
    /// pattern excludes) or that is not UTF-8.
    Walk(TrackError),
}

impl StatusError {
    /// Returns the error of the file system, if that is what failed. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Update(error) => error.io_error(),
            Self::Snapshot(error) => error.io_error(),
            Self::Refs(error) => error.io_error(),
            Self::Ignore(error) => error.io_error(),
            Self::Walk(error) => error.io_error(),
        }
    }

    /// Whether a pointer moved although the call failed: see [`UpdateError::pointer_moved`]. It
    /// is the snapshot that moves it, and a failure after it leaves its entry in the log.
    pub fn pointer_moved(&self) -> bool {
        matches!(self, Self::Snapshot(SnapshotError::Update(error)) if error.pointer_moved())
    }
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Update(error) => error.fmt(f),
            Self::Snapshot(error) => error.fmt(f),
            Self::Refs(error) => write!(f, "could not read the references: {error}"),
            Self::Ignore(error) => error.fmt(f),
            Self::Walk(error) => error.fmt(f),
        }
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
        assert!(status.snapshot.is_none() && status.late.is_none());
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
        assert_eq!(snapshot.change().actor(), &other());
        assert_eq!(snapshot.change().operation().as_str(), "snapshot");
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

    #[test]
    fn a_directory_with_the_same_id_on_both_sides_is_not_compared() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "big/a.txt", "a");
        write(&dir, "big/sub/b.txt", "b");
        write(&dir, "small.txt", "s");
        track(&repository, ".");
        commit(&repository, "base");
        write(&dir, "small.txt", "t");

        let status = status(&repository);

        assert_eq!(kinds(&status), [('M', "small.txt")]);
    }

    #[test]
    fn the_error_exposes_the_file_system_error_and_whether_a_pointer_moved() {
        let moved = StatusError::Snapshot(SnapshotError::Update(UpdateError::NotDurable {
            entry: Box::new({
                let dir = TempDir::new();
                repository(&dir).oplog().entries().unwrap().remove(0)
            }),
            source: RefError::NotFlushed {
                path: "x".into(),
                source: io::Error::other("disk"),
            },
        }));
        assert!(moved.pointer_moved());
        assert!(moved.io_error().is_some());
        assert!(!StatusError::Refs(RefError::Locked { path: "x".into() }).pointer_moved());
    }
}
