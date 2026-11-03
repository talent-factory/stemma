//! The working-copy view of a commit with an open conflict (ADR-010).
//!
//! A conflict is data in the commit: a sum of trees (ADR-005). The working directory, the
//! checkout layer, and `diff` work on one tree, so [`view`] turns the sum into the tree that shows
//! it: every path whose values do not cancel holds the text of [`marker_text`] as a file, and every
//! other path holds its value. The view of a commit without a conflict is its tree, and nothing is
//! written.
//!
//! The way back is [`resolve`]: Stemma does not parse markers. The snapshot keeps a conflicted
//! path only if the file in the working directory equals the text that [`view`] showed; any other
//! file, and a file that is gone, resolves the path with exactly that file. A half-edited marker
//! therefore counts as a resolution.
//!
//! A *conflicted path* is a name whose values in the terms of the sum do not cancel and are not
//! all directories. A directory is entered, not shown, so the names below it are conflicted paths
//! on their own. Where a directory stands against a file or against nothing in some terms, the
//! directory term has no content of its own, and the text shows it as the line
//! `(directory <id>)`, so that two different directories never cancel; the files below it stay in
//! the commit and are not written.

use std::collections::BTreeSet;

use super::snapshot::{read_object, SnapshotError};
use crate::conflict::{marker_text, Merge};
use crate::index::Index;
use crate::merge::Slot;
use crate::object::{Blob, BlobId, EntryMode, FileMode, Kind, Tree, TreeEntry, TreeId};
use crate::store::ObjectStore;

/// A path with an open conflict, as `status` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConflictedFile {
    /// The path, relative to the working directory and written with `/`.
    pub path: String,
    /// The number of sides of the conflict at this path, after the terms that cancel are gone.
    pub sides: usize,
}

impl ConflictedFile {
    /// Names a conflicted path with the number of its sides.
    pub fn new(path: impl Into<String>, sides: usize) -> Self {
        Self {
            path: path.into(),
            sides,
        }
    }
}

/// A conflicted path of a view: its values, and the file that shows them.
#[derive(Debug, Clone)]
pub(super) struct ConflictedPath {
    pub(super) path: String,
    /// The values of the path in the terms of the sum, simplified, in the order of the sum.
    terms: Merge<Option<Slot>>,
    /// The file that the view holds at the path: its mode and its content. `None` if the view
    /// holds nothing there.
    shown: Option<(FileMode, BlobId)>,
    /// The values differ in the mode only, so the view holds the content that all of them have.
    /// The mode of the file then counts as well; otherwise only the content does.
    mode_counts: bool,
}

impl ConflictedPath {
    pub(super) fn sides(&self) -> usize {
        self.terms.sides()
    }

    /// Whether the file in the working directory still shows the conflict: it is the file of the
    /// view, byte for byte (and, if the values differ in the mode only, with the same mode).
    fn is_unchanged(&self, found: Option<(FileMode, BlobId)>) -> bool {
        match (self.shown, found) {
            (None, None) => true,
            (Some((mode, id)), Some((found_mode, found_id))) => {
                id == found_id && (!self.mode_counts || mode == found_mode)
            }
            _ => false,
        }
    }
}

/// The tree that shows a sum of trees, and the paths that are conflicted in it.
#[derive(Debug, Clone)]
pub(super) struct View {
    /// The tree of the working copy: the tree itself for a sum of one tree.
    pub(super) tree: TreeId,
    /// The conflicted paths, sorted by path. Empty for a sum of one tree.
    pub(super) conflicts: Vec<ConflictedPath>,
}

impl View {
    /// The conflicted paths in the form that `status` reports.
    pub(super) fn files(&self) -> Vec<ConflictedFile> {
        self.conflicts
            .iter()
            .map(|path| ConflictedFile {
                path: path.path.clone(),
                sides: path.sides(),
            })
            .collect()
    }
}

/// Builds the view of a sum of trees. The text of a conflicted path and the trees that hold it are
/// written to the store; for a sum of one tree nothing is.
pub(super) fn view(objects: &dyn ObjectStore, sum: &Merge<TreeId>) -> Result<View, SnapshotError> {
    let sum = sum.simplify();
    if let [tree] = sum.terms() {
        return Ok(View {
            tree: *tree,
            conflicts: Vec::new(),
        });
    }
    let mut conflicts = Vec::new();
    let top = sum.map(|tree| Some(*tree));
    let tree = match walk(objects, "", &top, &mut conflicts)? {
        Some(tree) => tree,
        None => put_tree(objects, Vec::new())?,
    };
    conflicts.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(View { tree, conflicts })
}

/// Builds the tree of a sum of trees, level by level. A term that is `None` is the empty tree.
/// Returns `None` for a tree without entries, so that an empty directory is not written.
fn walk(
    objects: &dyn ObjectStore,
    prefix: &str,
    sum: &Merge<Option<TreeId>>,
    conflicts: &mut Vec<ConflictedPath>,
) -> Result<Option<TreeId>, SnapshotError> {
    let sum = sum.simplify();
    if let [only] = sum.terms() {
        // A tree that cancels down to one is not read. It holds no conflict.
        return Ok(*only);
    }
    let trees: Vec<Tree> = sum
        .terms()
        .iter()
        .map(|id| read_tree(objects, *id))
        .collect::<Result<_, _>>()?;
    let names: BTreeSet<&str> = trees
        .iter()
        .flat_map(|tree| tree.entries().iter().map(TreeEntry::name))
        .collect();
    let mut entries = Vec::new();
    for name in names {
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let slots = Merge::try_from_terms(
            trees
                .iter()
                .map(|tree| {
                    tree.entries()
                        .binary_search_by(|entry| entry.name().cmp(name))
                        .ok()
                        .map(|at| Slot {
                            mode: tree.entries()[at].mode(),
                            id: tree.entries()[at].id(),
                        })
                })
                .collect(),
        )
        .expect("the trees are the terms of a sum")
        .simplify();
        if let [value] = slots.terms() {
            if let Some(slot) = value {
                entries.push(entry_of(name, *slot));
            }
            continue;
        }
        let directories = slots
            .terms()
            .iter()
            .all(|slot| slot.is_none_or(|slot| slot.mode == EntryMode::Directory));
        if directories {
            let below = slots.map(|slot| slot.map(|slot| TreeId::new(slot.id)));
            if let Some(tree) = walk(objects, &path, &below, conflicts)? {
                entries.push(TreeEntry::directory(name, tree).expect(VALID));
            }
            continue;
        }
        let (shown, mode_counts) = show(objects, &slots)?;
        if let Some((mode, id)) = shown {
            entries.push(TreeEntry::file(name, mode, id).expect(VALID));
        }
        conflicts.push(ConflictedPath {
            path,
            terms: slots,
            shown,
            mode_counts,
        });
    }
    if entries.is_empty() {
        return Ok(None);
    }
    put_tree(objects, entries).map(Some)
}

/// The file that shows the values of one path, and whether its mode counts. The text is the one of
/// [`marker_text`]. If the contents cancel and only the modes differ, there is no text to show:
/// the file holds the content that all of them have, and its mode counts.
fn show(
    objects: &dyn ObjectStore,
    slots: &Merge<Option<Slot>>,
) -> Result<(Option<(FileMode, BlobId)>, bool), SnapshotError> {
    let contents: Vec<Option<Vec<u8>>> = slots
        .terms()
        .iter()
        .map(|slot| match slot {
            Some(slot) if slot.mode == EntryMode::Directory => Ok(Some(
                format!("(directory {})\n", TreeId::new(slot.id)).into_bytes(),
            )),
            Some(slot) => {
                let raw = read_object(objects, slot.id, Kind::Blob)?;
                Ok(Some(raw.into_body()))
            }
            None => Ok(None),
        })
        .collect::<Result<_, SnapshotError>>()?;
    let sum = Merge::try_from_terms(contents.iter().map(Option::as_deref).collect())
        .expect("the terms of a sum");
    let (text, mode_counts) = match marker_text(&sum) {
        Some(text) => (Some(text), false),
        None => (sum.simplify().terms()[0].map(<[u8]>::to_vec), true),
    };
    let Some(text) = text else {
        return Ok((None, mode_counts));
    };
    let id = objects
        .put_blob(&Blob::new(text))
        .map_err(|error| SnapshotError::Store(Box::new(error)))?;
    // The first value that is a file gives the mode, and a link shows as a plain file.
    let mode = slots
        .terms()
        .iter()
        .flatten()
        .find_map(|slot| match slot.mode {
            EntryMode::Executable => Some(FileMode::Executable),
            EntryMode::File => Some(FileMode::File),
            EntryMode::Symlink | EntryMode::Directory => None,
        })
        .unwrap_or(FileMode::File);
    Ok((Some((mode, id)), mode_counts))
}

/// Decides the conflicted paths of a view against the files that a snapshot found, and returns the
/// sum of trees that the snapshot records.
///
/// `index` holds the files of the working directory. A conflicted path whose file is the one that
/// the view showed keeps its values; every other path takes the file as it is (or its absence), in
/// every term. If no conflicted path is kept, the sum is the one tree of the index.
pub(super) fn resolve(
    objects: &dyn ObjectStore,
    conflicts: &[ConflictedPath],
    index: &Index,
) -> Result<Merge<TreeId>, SnapshotError> {
    let kept: Vec<&ConflictedPath> = conflicts
        .iter()
        .filter(|path| {
            let found = index
                .get(&path.path)
                .map(|entry| (entry.mode(), entry.id()));
            // Files below the path mean that a directory has taken the place of the file: that
            // resolves the path, whatever the view showed there (or did not show).
            let below = format!("{}/", path.path);
            path.is_unchanged(found) && !index.iter().any(|(other, _)| other.starts_with(&below))
        })
        .collect();
    let mut resolved = index.clone();
    for path in &kept {
        resolved.remove(&path.path);
    }
    if kept.is_empty() {
        let tree = resolved
            .write_tree(objects)
            .map_err(SnapshotError::WriteTree)?;
        return Ok(Merge::resolved(tree));
    }
    // Every kept path has the number of sides of the widest one, so a narrower one is padded.
    let sides = kept.iter().map(|path| path.sides()).max().unwrap_or(1);
    let padded: Vec<(&str, Merge<Option<Slot>>)> = kept
        .iter()
        .map(|path| (path.path.as_str(), path.terms.padded(sides)))
        .collect();
    let mut trees = Vec::with_capacity(2 * sides - 1);
    for term in 0..2 * sides - 1 {
        let mut index = resolved.clone();
        for (path, terms) in &padded {
            if let Some(slot) = terms.terms()[term] {
                insert_slot(objects, &mut index, path, slot)?;
            }
        }
        trees.push(
            index
                .write_tree(objects)
                .map_err(SnapshotError::WriteTree)?,
        );
    }
    Ok(Merge::try_from_terms(trees)
        .expect("an odd number of terms")
        .simplify())
}

/// Puts the value of a path into the index: a file as it is, a directory as the files below it.
fn insert_slot(
    objects: &dyn ObjectStore,
    index: &mut Index,
    path: &str,
    slot: Slot,
) -> Result<(), SnapshotError> {
    let mut pending = vec![(path.to_owned(), slot)];
    while let Some((path, slot)) = pending.pop() {
        match FileMode::from_entry_mode(slot.mode) {
            Some(mode) => index
                .insert(path, mode, BlobId::new(slot.id))
                .map_err(SnapshotError::Index)?,
            None => {
                let tree = read_tree(objects, Some(TreeId::new(slot.id)))?;
                for entry in tree.entries() {
                    pending.push((
                        format!("{path}/{}", entry.name()),
                        Slot {
                            mode: entry.mode(),
                            id: entry.id(),
                        },
                    ));
                }
            }
        }
    }
    Ok(())
}

fn entry_of(name: &str, slot: Slot) -> TreeEntry {
    match FileMode::from_entry_mode(slot.mode) {
        Some(mode) => TreeEntry::file(name, mode, BlobId::new(slot.id)),
        None => TreeEntry::directory(name, TreeId::new(slot.id)),
    }
    .expect(VALID)
}

/// Reads a tree, or the empty tree for `None`.
fn read_tree(objects: &dyn ObjectStore, id: Option<TreeId>) -> Result<Tree, SnapshotError> {
    let Some(id) = id else {
        return Ok(Tree::try_new(Vec::new()).expect("a tree without entries has no duplicate"));
    };
    let raw = read_object(objects, id.object_id(), Kind::Tree)?;
    Tree::parse(raw.body()).map_err(|source| SnapshotError::Tree { id, source })
}

/// The entries come from trees that were read, so their names are valid and distinct.
const VALID: &str = "the names of an existing tree are valid";

fn put_tree(objects: &dyn ObjectStore, entries: Vec<TreeEntry>) -> Result<TreeId, SnapshotError> {
    let tree = Tree::try_new(entries).expect(VALID);
    objects
        .put_tree(&tree)
        .map_err(|error| SnapshotError::Store(Box::new(error)))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::snapshot::read_any_commit;
    use super::*;
    use crate::conflict::AnyCommit;
    use crate::object::{CommitId, ObjectId};
    use crate::oplog::OperationName;
    use crate::repository::{CommitError, InitOptions, Repository};
    use crate::testutil::{conflicted_commit, TempDir};
    use crate::{Actor, AllowAll};

    fn actor() -> Actor {
        Actor::human("daniel").unwrap()
    }

    /// A repository whose working-copy commit is the conflicted commit of
    /// [`conflicted_commit`], with the files in the working directory as a restore writes them.
    fn conflicted(dir: &TempDir) -> (Repository, CommitId, Vec<u8>) {
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
        let (id, text) = conflicted_commit(&repository);
        let current = repository.refs().working_copy().unwrap();
        repository
            .update_working_copy(
                &OperationName::try_new("snapshot").unwrap(),
                current,
                id,
                &actor(),
                &AllowAll,
            )
            .unwrap();
        fs::write(dir.path().join("a.txt"), &text).unwrap();
        fs::write(dir.path().join("b.txt"), "same\n").unwrap();
        (repository, id, text)
    }

    fn working_copy(repository: &Repository) -> AnyCommit {
        let id = repository.refs().working_copy().unwrap();
        read_any_commit(repository.objects(), id).unwrap()
    }

    /// The content of a file in the root of a tree.
    fn content(repository: &Repository, tree: TreeId, name: &str) -> String {
        let raw = read_object(repository.objects(), tree.object_id(), Kind::Tree).unwrap();
        let tree = Tree::parse(raw.body()).unwrap();
        let entry = tree.entries().iter().find(|e| e.name() == name).unwrap();
        let blob = read_object(repository.objects(), entry.id(), Kind::Blob).unwrap();
        String::from_utf8(blob.into_body()).unwrap()
    }

    fn conflicts_of(repository: &Repository, commit: &AnyCommit) -> Vec<ConflictedFile> {
        view(repository.objects(), &commit.tree_sum())
            .unwrap()
            .files()
    }

    #[test]
    fn unchanged_markers_keep_the_conflict() {
        let dir = TempDir::new();
        let (repository, id, _) = conflicted(&dir);
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
        assert_eq!(repository.refs().working_copy().unwrap(), id);
    }

    #[test]
    fn a_change_to_another_file_keeps_the_conflict_and_changes_every_tree() {
        let dir = TempDir::new();
        let (repository, id, _) = conflicted(&dir);
        fs::write(dir.path().join("b.txt"), "changed\n").unwrap();
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_some());
        let commit = working_copy(&repository);
        assert!(commit.is_conflicted());
        assert_ne!(commit.id(), id);
        let sum = commit.tree_sum();
        assert_eq!(sum.terms().len(), 3);
        for tree in sum.terms() {
            assert_eq!(content(&repository, *tree, "b.txt"), "changed\n");
        }
        let files = conflicts_of(&repository, &commit);
        assert_eq!(
            files,
            vec![ConflictedFile {
                path: "a.txt".to_owned(),
                sides: 2
            }]
        );
    }

    #[test]
    fn edited_text_resolves_the_path_with_exactly_that_text() {
        let dir = TempDir::new();
        let (repository, _, _) = conflicted(&dir);
        fs::write(dir.path().join("a.txt"), "merged\n").unwrap();
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_some());
        let AnyCommit::Commit(commit) = working_copy(&repository) else {
            panic!("the last conflict is resolved, so the commit has one tree");
        };
        assert_eq!(content(&repository, commit.tree(), "a.txt"), "merged\n");
        assert_eq!(content(&repository, commit.tree(), "b.txt"), "same\n");
    }

    #[test]
    fn a_half_edited_marker_counts_as_a_resolution() {
        let dir = TempDir::new();
        let (repository, _, text) = conflicted(&dir);
        // The closing marker is gone: the text is not the one that Stemma wrote.
        let text = String::from_utf8(text).unwrap();
        let half = text.strip_suffix(">>>>>>>\n").unwrap();
        fs::write(dir.path().join("a.txt"), half).unwrap();
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_some());
        let AnyCommit::Commit(commit) = working_copy(&repository) else {
            panic!("a half-edited marker resolves the path");
        };
        assert_eq!(content(&repository, commit.tree(), "a.txt"), half);
    }

    #[test]
    fn a_deleted_file_resolves_the_path_by_deleting_it() {
        let dir = TempDir::new();
        let (repository, _, _) = conflicted(&dir);
        fs::remove_file(dir.path().join("a.txt")).unwrap();
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_some());
        let AnyCommit::Commit(commit) = working_copy(&repository) else {
            panic!("a deleted file resolves the path");
        };
        let raw = read_object(repository.objects(), commit.tree().object_id(), Kind::Tree).unwrap();
        let names: Vec<_> = Tree::parse(raw.body())
            .unwrap()
            .entries()
            .iter()
            .map(|entry| entry.name().to_owned())
            .collect();
        assert_eq!(names, ["b.txt"]);
    }

    #[test]
    fn status_reports_the_conflicted_paths_with_their_sides() {
        let dir = TempDir::new();
        let (repository, _, _) = conflicted(&dir);
        let status = repository.status(&actor(), &AllowAll).unwrap();
        assert!(status.snapshot.is_none(), "the markers are unchanged");
        assert_eq!(
            status.conflicts,
            vec![ConflictedFile {
                path: "a.txt".to_owned(),
                sides: 2
            }]
        );
        let paths: Vec<_> = status.changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "b.txt"], "the commit has no parent");
        fs::write(dir.path().join("a.txt"), "merged\n").unwrap();
        let status = repository.status(&actor(), &AllowAll).unwrap();
        assert!(status.conflicts.is_empty());
    }

    #[test]
    fn a_commit_with_an_open_conflict_succeeds_and_starts_a_conflicted_working_copy() {
        let dir = TempDir::new();
        let (repository, _, _) = conflicted(&dir);
        let committed = repository.commit("merge", &actor(), &AllowAll).unwrap();
        assert_eq!(committed.conflict_sides, Some(2));
        let named = read_any_commit(repository.objects(), committed.commit).unwrap();
        let next = working_copy(&repository);
        assert!(named.is_conflicted() && next.is_conflicted());
        assert_eq!(named.message(), "merge");
        assert_eq!(next.parents(), [committed.commit]);
        assert_eq!(next.tree_sum(), named.tree_sum());
        // The new working-copy commit holds the same conflict, so there is nothing to commit.
        let error = repository.commit("again", &actor(), &AllowAll).unwrap_err();
        assert!(matches!(error, CommitError::NothingToCommit), "{error}");
    }

    fn blob(repository: &Repository, content: &str) -> BlobId {
        repository.objects().put_blob(&Blob::new(content)).unwrap()
    }

    #[test]
    fn a_directory_against_a_file_shows_as_a_line_and_is_kept_as_a_directory() {
        let dir = TempDir::new();
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
        let store = repository.objects();
        let put =
            |entries: Vec<TreeEntry>| store.put_tree(&Tree::try_new(entries).unwrap()).unwrap();
        let file = put(vec![TreeEntry::file(
            "a",
            FileMode::File,
            blob(&repository, "file\n"),
        )
        .unwrap()]);
        let base = put(vec![]);
        let inner = put(vec![TreeEntry::file(
            "b",
            FileMode::File,
            blob(&repository, "inside\n"),
        )
        .unwrap()]);
        let directory_inner = inner;
        let directory = put(vec![TreeEntry::directory("a", inner).unwrap()]);
        let sum = Merge::try_from_terms(vec![file, base, directory]).unwrap();
        let shown = view(store, &sum).unwrap();
        assert_eq!(shown.conflicts.len(), 1);
        assert_eq!(shown.conflicts[0].path, "a");
        let text = content(&repository, shown.tree, "a");
        assert!(text.contains("+++++++ side 1\nfile\n"), "{text}");
        assert!(
            text.contains(&format!("+++++++ side 2\n(directory {directory_inner})\n")),
            "{text}"
        );

        // The file of the view, unchanged, keeps the three trees, and the directory comes back.
        let mut index = Index::new();
        let shown_blob = blob(&repository, &text);
        index.insert("a", FileMode::File, shown_blob).unwrap();
        let kept = resolve(store, &shown.conflicts, &index).unwrap();
        assert_eq!(kept.terms().len(), 3);
        assert_eq!(kept.terms()[0], file);
        assert_eq!(kept.terms()[2], directory);
        // Another text resolves the path with that text, in one tree.
        let mut index = Index::new();
        index
            .insert("a", FileMode::File, blob(&repository, "mine\n"))
            .unwrap();
        let resolved = resolve(store, &shown.conflicts, &index).unwrap();
        assert!(resolved.is_resolved());
    }

    #[test]
    fn values_that_differ_only_in_the_mode_show_their_content_and_the_mode_counts() {
        let dir = TempDir::new();
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
        let store = repository.objects();
        let same = blob(&repository, "same\n");
        let put = |mode: FileMode| {
            let entry = TreeEntry::file("a", mode, same).unwrap();
            store
                .put_tree(&Tree::try_new(vec![entry]).unwrap())
                .unwrap()
        };
        let sum = Merge::try_from_terms(vec![
            put(FileMode::Executable),
            put(FileMode::File),
            put(FileMode::Symlink),
        ])
        .unwrap();
        let shown = view(store, &sum).unwrap();
        assert_eq!(content(&repository, shown.tree, "a"), "same\n");
        let path = &shown.conflicts[0];
        assert!(path.is_unchanged(Some((FileMode::Executable, same))));
        assert!(!path.is_unchanged(Some((FileMode::File, same))));
        assert!(!path.is_unchanged(None));
    }

    #[test]
    fn a_commit_without_a_conflict_is_its_own_view_and_writes_nothing() {
        let dir = TempDir::new();
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
        let tree = TreeId::new(ObjectId::from_digest(blake3::hash(b"no tree behind it")));
        let shown = view(repository.objects(), &Merge::resolved(tree)).unwrap();
        assert_eq!(shown.tree, tree);
        assert!(shown.conflicts.is_empty());
    }

    /// A tree of the given files, written through an index.
    fn tree_of(repository: &Repository, files: &[(&str, &str)]) -> TreeId {
        let mut index = Index::new();
        for (path, text) in files {
            index
                .insert(*path, FileMode::File, blob(repository, text))
                .unwrap();
        }
        index.write_tree(repository.objects()).unwrap()
    }

    /// Five trees: `dir/a.txt` has two sides (its sum is padded), `b.txt` has three, and
    /// `dir/other.txt` is the same in every tree.
    fn two_paths(repository: &Repository) -> Merge<TreeId> {
        let tree = |a: &str, b: &str| {
            tree_of(
                repository,
                &[("dir/a.txt", a), ("b.txt", b), ("dir/other.txt", "same\n")],
            )
        };
        Merge::try_from_terms(vec![
            tree("a1\n", "b1\n"),
            tree("a0\n", "b0\n"),
            tree("a2\n", "b2\n"),
            tree("a2\n", "b3\n"),
            tree("a2\n", "b4\n"),
        ])
        .unwrap()
    }

    fn shown(shown: &View, path: &str) -> BlobId {
        let file = shown.conflicts.iter().find(|c| c.path == path).unwrap();
        file.shown.unwrap().1
    }

    #[test]
    fn paths_with_different_numbers_of_sides_in_nested_directories_keep_their_own_terms() {
        let dir = TempDir::new();
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
        let store = repository.objects();
        let sum = two_paths(&repository);
        let view = view(store, &sum).unwrap();
        let sides: Vec<_> = view
            .files()
            .into_iter()
            .map(|f| (f.path, f.sides))
            .collect();
        assert_eq!(
            sides,
            [("b.txt".to_owned(), 3), ("dir/a.txt".to_owned(), 2)]
        );

        let mut index = Index::new();
        index
            .insert("b.txt", FileMode::File, shown(&view, "b.txt"))
            .unwrap();
        index
            .insert("dir/a.txt", FileMode::File, shown(&view, "dir/a.txt"))
            .unwrap();
        index
            .insert(
                "dir/other.txt",
                FileMode::File,
                blob(&repository, "changed\n"),
            )
            .unwrap();

        // Nothing is resolved: the widest path decides the number of sides, the other is padded
        // and keeps its own terms, and the view of the result is the same text.
        let kept = resolve(store, &view.conflicts, &index).unwrap();
        assert_eq!(kept.terms().len(), 5);
        let again = self::view(store, &kept).unwrap();
        assert_eq!(again.files(), view.files());
        for path in ["b.txt", "dir/a.txt"] {
            assert_eq!(shown(&again, path), shown(&view, path));
        }
        for tree in kept.terms() {
            let raw = read_object(store, tree.object_id(), Kind::Tree).unwrap();
            let root = Tree::parse(raw.body()).unwrap();
            let entry = root.entries().iter().find(|e| e.name() == "dir").unwrap();
            assert_eq!(
                content(&repository, TreeId::new(entry.id()), "other.txt"),
                "changed\n"
            );
        }

        // Editing the wide path leaves the narrow one with its two sides.
        index
            .insert("b.txt", FileMode::File, blob(&repository, "mine\n"))
            .unwrap();
        let narrow = resolve(store, &view.conflicts, &index).unwrap();
        assert_eq!(narrow.terms().len(), 3);
        let after = self::view(store, &narrow).unwrap();
        assert_eq!(after.files(), [ConflictedFile::new("dir/a.txt", 2)]);
        assert_eq!(content(&repository, after.tree, "b.txt"), "mine\n");

        // Editing the narrow path leaves the wide one with its three sides.
        index
            .insert("b.txt", FileMode::File, shown(&view, "b.txt"))
            .unwrap();
        index
            .insert("dir/a.txt", FileMode::File, blob(&repository, "merged\n"))
            .unwrap();
        let wide = resolve(store, &view.conflicts, &index).unwrap();
        assert_eq!(wide.terms().len(), 5);
        let after = self::view(store, &wide).unwrap();
        assert_eq!(after.files(), [ConflictedFile::new("b.txt", 3)]);
    }

    #[test]
    fn two_different_directories_do_not_cancel_against_a_file() {
        let dir = TempDir::new();
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
        let store = repository.objects();
        let with_dir = |text: &str| tree_of(&repository, &[("a/b", text)]);
        let file = tree_of(&repository, &[("a", "file\n")]);
        let sum = Merge::try_from_terms(vec![with_dir("x\n"), with_dir("y\n"), file]).unwrap();
        let shown = view(store, &sum).unwrap();
        assert_eq!(shown.files(), [ConflictedFile::new("a", 2)]);
        let text = content(&repository, shown.tree, "a");
        assert!(text.starts_with("<<<<<<< conflict\n"), "{text}");
        assert!(text.contains("(directory b3:"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_new_mode_on_a_kept_conflict_file_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let (repository, id, _) = conflicted(&dir);
        let path = dir.path().join("a.txt");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
        assert_eq!(repository.refs().working_copy().unwrap(), id);
    }

    #[test]
    fn a_tracked_directory_in_the_place_of_a_conflicted_file_resolves_the_path() {
        let dir = TempDir::new();
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
        let store = repository.objects();
        let file = tree_of(&repository, &[("a", "file\n")]);
        let base = tree_of(&repository, &[]);
        let nested = tree_of(&repository, &[("a/x", "x\n")]);
        let sum = Merge::try_from_terms(vec![file, base, nested]).unwrap();
        let shown = view(store, &sum).unwrap();
        let mut index = Index::new();
        index
            .insert("a/x", FileMode::File, blob(&repository, "mine\n"))
            .unwrap();
        let resolved = resolve(store, &shown.conflicts, &index).unwrap();
        assert!(resolved.is_resolved());
    }
}
