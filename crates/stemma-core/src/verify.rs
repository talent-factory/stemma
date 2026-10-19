//! Checking the claims of a tree or a commit against a store.
//!
//! The readers [`Tree::parse`] and [`Commit::parse`] claim the kind of each ID they find: the mode
//! of an entry says whether it names a tree or a blob, and the headers of a commit name a tree and
//! parents. A reader has no store, so it cannot know. [`Tree::verify`] and [`Commit::verify`]
//! check the claims of one object against a store, as [`Index::verify`](crate::index::Index::verify)
//! does for an index.
//!
//! The check is shallow: it looks at the objects that the tree or commit names directly, and does
//! not follow them. Each object is read and verified completely (see
//! [`ObjectStore::resolve_blob`]), so the cost grows with the size of the objects named. The
//! result holds at the time of the call.

use std::error::Error;
use std::fmt;

use crate::object::{Commit, EntryMode, ObjectId, Tree};
use crate::store::{ObjectStore, StoreError};

/// Why a tree fails [`Tree::verify`]. Carries the name of the first failing entry, in name order.
#[derive(Debug)]
#[non_exhaustive]
pub enum TreeVerifyError {
    /// The store has no object with the ID of the entry.
    Missing {
        /// The name of the entry.
        name: String,
        /// The ID the entry names.
        id: ObjectId,
    },
    /// The store could not resolve the ID with the kind the mode of the entry claims: the object
    /// has another kind ([`StoreError::KindMismatch`]), it is damaged, or the storage failed.
    Store {
        /// The name of the entry.
        name: String,
        /// What the store reported. Boxed because the unboxed error is too large for clippy's
        /// `result_large_err` lint.
        source: Box<StoreError>,
    },
}

impl fmt::Display for TreeVerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { name, id } => {
                write!(f, "`{}`: the store has no object {id}", name.escape_debug())
            }
            Self::Store { name, source } => write!(f, "`{}`: {source}", name.escape_debug()),
        }
    }
}

impl Error for TreeVerifyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes that of the store error; see the crate documentation.
            Self::Store { source, .. } => source.source(),
            Self::Missing { .. } => None,
        }
    }
}

impl Tree {
    /// Checks that every entry names an object that the store has, with the kind that its mode
    /// claims: a tree for a directory, a blob for every other mode. See the module documentation
    /// for what the check covers. Every entry is read and hashed completely, so a tree of large
    /// files costs as much as reading them.
    ///
    /// # Errors
    ///
    /// Returns the first problem in name order: [`TreeVerifyError::Missing`] if the store has no
    /// such object, and [`TreeVerifyError::Store`] if the object has another kind, is damaged, or
    /// the storage fails.
    pub fn verify(&self, store: &dyn ObjectStore) -> Result<(), TreeVerifyError> {
        for entry in self.entries() {
            let id = entry.id();
            let found = if entry.mode() == EntryMode::Directory {
                store.resolve_tree(&id).map(|found| found.is_some())
            } else {
                store.resolve_blob(&id).map(|found| found.is_some())
            };
            match found {
                Ok(true) => {}
                Ok(false) => {
                    return Err(TreeVerifyError::Missing {
                        name: entry.name().to_owned(),
                        id,
                    })
                }
                Err(source) => {
                    return Err(TreeVerifyError::Store {
                        name: entry.name().to_owned(),
                        source: Box::new(source),
                    })
                }
            }
        }
        Ok(())
    }
}

/// What a commit refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommitReference {
    /// The tree of the commit.
    Tree,
    /// A parent.
    Parent {
        /// The position in the list of parents, counted from 0 as in
        /// [`Commit::parents`]. The `Display` form counts from 1: index 0 is "parent 1".
        index: usize,
    },
}

impl fmt::Display for CommitReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tree => f.write_str("the tree"),
            Self::Parent { index } => write!(f, "parent {}", index + 1),
        }
    }
}

/// Why a commit fails [`Commit::verify`].
#[derive(Debug)]
#[non_exhaustive]
pub enum CommitVerifyError {
    /// The store has no object with the ID of the reference.
    Missing {
        /// What the commit refers to.
        reference: CommitReference,
        /// The ID it names.
        id: ObjectId,
    },
    /// The store could not resolve the ID with the kind the commit claims: the object has another
    /// kind ([`StoreError::KindMismatch`]), it is damaged, or the storage failed.
    Store {
        /// What the commit refers to.
        reference: CommitReference,
        /// What the store reported. Boxed for the same reason as in [`TreeVerifyError`].
        source: Box<StoreError>,
    },
}

impl fmt::Display for CommitVerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { reference, id } => {
                write!(f, "{reference}: the store has no object {id}")
            }
            Self::Store { reference, source } => write!(f, "{reference}: {source}"),
        }
    }
}

impl Error for CommitVerifyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes that of the store error; see the crate documentation.
            Self::Store { source, .. } => source.source(),
            Self::Missing { .. } => None,
        }
    }
}

impl Commit {
    /// Checks that the tree is a tree and every parent is a commit that the store has. See the
    /// module documentation for what the check covers. A store that holds only part of the
    /// history, such as a shallow copy, fails for the parents it lacks.
    ///
    /// # Errors
    ///
    /// Returns the first problem, the tree before the parents and the parents in order:
    /// [`CommitVerifyError::Missing`] if the store has no such object, and
    /// [`CommitVerifyError::Store`] if the object has another kind, is damaged, or the storage
    /// fails.
    pub fn verify(&self, store: &dyn ObjectStore) -> Result<(), CommitVerifyError> {
        let tree = ObjectId::from(self.tree());
        let references = std::iter::once((CommitReference::Tree, tree)).chain(
            self.parents().iter().enumerate().map(|(index, parent)| {
                (CommitReference::Parent { index }, ObjectId::from(*parent))
            }),
        );
        for (reference, id) in references {
            let found = match reference {
                CommitReference::Tree => store.resolve_tree(&id).map(|found| found.is_some()),
                CommitReference::Parent { .. } => {
                    store.resolve_commit(&id).map(|found| found.is_some())
                }
            };
            match found {
                Ok(true) => {}
                Ok(false) => return Err(CommitVerifyError::Missing { reference, id }),
                Err(source) => {
                    return Err(CommitVerifyError::Store {
                        reference,
                        source: Box::new(source),
                    })
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Blob, BlobId, CommitId, FileMode, TreeEntry, TreeId};
    use crate::store::FsObjectStore;
    use crate::testutil::{overwrite, TempDir};
    use crate::Actor;

    fn commit(tree: TreeId, parents: Vec<CommitId>) -> Commit {
        Commit::new(tree, parents, Actor::human("daniel").unwrap(), 0, "m").unwrap()
    }

    fn empty_tree(store: &FsObjectStore) -> TreeId {
        store.put_tree(&Tree::new(vec![]).unwrap()).unwrap()
    }

    fn absent_blob() -> BlobId {
        Blob::new("absent").id()
    }

    fn absent_tree() -> TreeId {
        Tree::new(vec![
            TreeEntry::file("x", FileMode::File, absent_blob()).unwrap()
        ])
        .unwrap()
        .id()
    }

    // ---- Tree::verify

    #[test]
    fn a_tree_of_stored_objects_verifies() {
        let dir = TempDir::new();
        let store = dir.store();
        let blob = store.put_blob(&Blob::new("hello")).unwrap();
        let sub = empty_tree(&store);
        let tree = Tree::new(vec![
            TreeEntry::file("a", FileMode::File, blob).unwrap(),
            TreeEntry::file("b", FileMode::Executable, blob).unwrap(),
            TreeEntry::file("c", FileMode::Symlink, blob).unwrap(),
            TreeEntry::directory("d", sub).unwrap(),
        ])
        .unwrap();
        assert!(tree.verify(&store).is_ok());
        assert!(Tree::new(vec![]).unwrap().verify(&store).is_ok());
    }

    #[test]
    fn a_tree_entry_with_no_object_is_missing_for_a_file_and_for_a_directory() {
        let dir = TempDir::new();
        let store = dir.store();
        let file = Tree::new(vec![
            TreeEntry::file("f", FileMode::File, absent_blob()).unwrap()
        ])
        .unwrap();
        match file.verify(&store).unwrap_err() {
            TreeVerifyError::Missing { name, id } => {
                assert_eq!(name, "f");
                assert_eq!(id, ObjectId::from(absent_blob()));
            }
            other => panic!("unexpected error: {other}"),
        }
        let directory = Tree::new(vec![TreeEntry::directory("d", absent_tree()).unwrap()]).unwrap();
        match directory.verify(&store).unwrap_err() {
            TreeVerifyError::Missing { name, id } => {
                assert_eq!(name, "d");
                assert_eq!(id, ObjectId::from(absent_tree()));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_tree_entry_of_the_wrong_kind_is_a_store_error_in_both_directions() {
        let dir = TempDir::new();
        let store = dir.store();
        let blob = store.put_blob(&Blob::new("hello")).unwrap();
        let sub = empty_tree(&store);
        // A file that names a tree, and a directory that names a blob. The typed constructors
        // forbid this, so the claims are made the way a reader makes them.
        let file_names_tree = BlobId::new(ObjectId::from(sub));
        let directory_names_blob = TreeId::new(ObjectId::from(blob));
        for (entry, name) in [
            (
                TreeEntry::file("f", FileMode::File, file_names_tree).unwrap(),
                "f",
            ),
            (
                TreeEntry::directory("d", directory_names_blob).unwrap(),
                "d",
            ),
        ] {
            match Tree::new(vec![entry]).unwrap().verify(&store).unwrap_err() {
                TreeVerifyError::Store {
                    name: found,
                    source,
                } => {
                    assert_eq!(found, name);
                    assert!(
                        matches!(*source, StoreError::KindMismatch { .. }),
                        "{source}"
                    );
                }
                other => panic!("unexpected error: {other}"),
            }
        }
    }

    #[test]
    fn a_damaged_object_in_a_tree_is_a_store_error_and_not_missing() {
        let dir = TempDir::new();
        let store = dir.store();
        let blob = store.put_blob(&Blob::new("hello")).unwrap();
        overwrite(&store.object_path(&ObjectId::from(blob)), b"blob 5\0other");
        let tree = Tree::new(vec![TreeEntry::file("f", FileMode::File, blob).unwrap()]).unwrap();
        match tree.verify(&store).unwrap_err() {
            TreeVerifyError::Store { source, .. } => {
                assert!(matches!(*source, StoreError::Corrupt { .. }), "{source}");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_tree_reports_the_first_problem_in_name_order() {
        let dir = TempDir::new();
        let store = dir.store();
        let sub = empty_tree(&store);
        let wrong = BlobId::new(ObjectId::from(sub));
        let entries = |first: TreeEntry, second: TreeEntry| {
            Tree::new(vec![second, first]).unwrap().verify(&store)
        };
        // `a` is missing and `b` has the wrong kind, and the reverse.
        let missing = |name| TreeEntry::file(name, FileMode::File, absent_blob()).unwrap();
        let kind = |name| TreeEntry::file(name, FileMode::File, wrong).unwrap();
        assert!(matches!(
            entries(missing("a"), kind("b")),
            Err(TreeVerifyError::Missing { .. })
        ));
        assert!(matches!(
            entries(kind("a"), missing("b")),
            Err(TreeVerifyError::Store { .. })
        ));
    }

    #[test]
    fn tree_verify_errors_escape_the_name_and_carry_their_cause() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = Tree::new(vec![TreeEntry::file(
            "say\"hi",
            FileMode::File,
            absent_blob(),
        )
        .unwrap()])
        .unwrap();
        let error = tree.verify(&store).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("`say\\\"hi`: the store has no object b3:"),
            "{error}"
        );
        assert!(error.source().is_none());
        let sub = empty_tree(&store);
        let wrong = BlobId::new(ObjectId::from(sub));
        let tree = Tree::new(vec![
            TreeEntry::file("say\"hi", FileMode::File, wrong).unwrap()
        ])
        .unwrap();
        let error = tree.verify(&store).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("`say\\\"hi`: object {sub} is a tree, not a blob")
        );
        // The message has the message of the store error, so the chain ends here.
        assert!(error.source().is_none());
    }

    #[test]
    fn a_tree_checks_every_entry_and_not_only_the_first() {
        let dir = TempDir::new();
        let store = dir.store();
        let blob = store.put_blob(&Blob::new("hello")).unwrap();
        let sub = empty_tree(&store);
        let good = |name| TreeEntry::file(name, FileMode::File, blob).unwrap();
        let verify = |last: TreeEntry| {
            Tree::new(vec![
                good("a"),
                TreeEntry::directory("b", sub).unwrap(),
                last,
            ])
            .unwrap()
            .verify(&store)
        };
        let missing = TreeEntry::file("c", FileMode::File, absent_blob()).unwrap();
        let wrong = TreeEntry::file("c", FileMode::File, BlobId::new(ObjectId::from(sub))).unwrap();
        for (entry, is_missing) in [(missing, true), (wrong, false)] {
            match (verify(entry).unwrap_err(), is_missing) {
                (TreeVerifyError::Missing { name, .. }, true) => assert_eq!(name, "c"),
                (TreeVerifyError::Store { name, .. }, false) => assert_eq!(name, "c"),
                (other, _) => panic!("unexpected error: {other}"),
            }
        }
        // A damaged blob after good entries is a store error as well.
        let damaged = store.put_blob(&Blob::new("damaged")).unwrap();
        overwrite(&store.object_path(&ObjectId::from(damaged)), b"blob 1\0x");
        let entry = TreeEntry::file("c", FileMode::File, damaged).unwrap();
        assert!(matches!(
            verify(entry),
            Err(TreeVerifyError::Store { ref name, .. }) if name == "c"
        ));
        assert!(verify(good("c")).is_ok());
    }

    // ---- Commit::verify

    #[test]
    fn a_commit_of_stored_objects_verifies() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = empty_tree(&store);
        let first = store.put_commit(&commit(tree, vec![])).unwrap();
        let second = store.put_commit(&commit(tree, vec![first])).unwrap();
        assert!(commit(tree, vec![]).verify(&store).is_ok());
        assert!(commit(tree, vec![first, second]).verify(&store).is_ok());
    }

    #[test]
    fn a_commit_with_no_tree_or_no_parent_is_missing_with_its_position() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = empty_tree(&store);
        let parent = store.put_commit(&commit(tree, vec![])).unwrap();
        let absent_parent = commit(absent_tree(), vec![]).id();
        match commit(absent_tree(), vec![]).verify(&store).unwrap_err() {
            CommitVerifyError::Missing { reference, id } => {
                assert_eq!(reference, CommitReference::Tree);
                assert_eq!(id, ObjectId::from(absent_tree()));
            }
            other => panic!("unexpected error: {other}"),
        }
        match commit(tree, vec![parent, absent_parent])
            .verify(&store)
            .unwrap_err()
        {
            CommitVerifyError::Missing { reference, id } => {
                assert_eq!(reference, CommitReference::Parent { index: 1 });
                assert_eq!(id, ObjectId::from(absent_parent));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_commit_whose_tree_or_parent_has_the_wrong_kind_is_a_store_error() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = empty_tree(&store);
        let parent = store.put_commit(&commit(tree, vec![])).unwrap();
        let parent_as_tree = TreeId::new(ObjectId::from(parent));
        let tree_as_parent = CommitId::new(ObjectId::from(tree));
        for (bad, reference) in [
            (commit(parent_as_tree, vec![]), CommitReference::Tree),
            (
                commit(tree, vec![parent, tree_as_parent]),
                CommitReference::Parent { index: 1 },
            ),
        ] {
            match bad.verify(&store).unwrap_err() {
                CommitVerifyError::Store {
                    reference: found,
                    source,
                } => {
                    assert_eq!(found, reference);
                    assert!(
                        matches!(*source, StoreError::KindMismatch { .. }),
                        "{source}"
                    );
                }
                other => panic!("unexpected error: {other}"),
            }
        }
    }

    #[test]
    fn a_commit_checks_the_tree_before_the_parents() {
        let dir = TempDir::new();
        let store = dir.store();
        let absent_parent = commit(absent_tree(), vec![]).id();
        match commit(absent_tree(), vec![absent_parent])
            .verify(&store)
            .unwrap_err()
        {
            CommitVerifyError::Missing { reference, .. } => {
                assert_eq!(reference, CommitReference::Tree);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_commit_store_error_names_the_reference_and_prints_the_store_error_once() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = empty_tree(&store);
        let parent = store.put_commit(&commit(tree, vec![])).unwrap();
        let tree_as_parent = CommitId::new(ObjectId::from(tree));
        let error = commit(tree, vec![parent, tree_as_parent])
            .verify(&store)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("parent 2: object {tree} is a tree, not a commit")
        );
        // The message has the message of the store error, so the chain ends here.
        assert!(error.source().is_none());
        let parent_as_tree = TreeId::new(ObjectId::from(parent));
        let error = commit(parent_as_tree, vec![]).verify(&store).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("the tree: object {parent} is a commit, not a tree")
        );
        assert!(error.source().is_none());
    }

    #[test]
    fn commit_verify_errors_name_the_reference_and_carry_their_cause() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = empty_tree(&store);
        let absent_parent = commit(tree, vec![]).id();
        let error = commit(tree, vec![absent_parent])
            .verify(&store)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("parent 1: the store has no object {absent_parent}")
        );
        assert!(error.source().is_none());
        let error = commit(TreeId::new(ObjectId::from(absent_parent)), vec![])
            .verify(&store)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("the tree: the store has no object"),
            "{error}"
        );
        assert_eq!(CommitReference::Parent { index: 2 }.to_string(), "parent 3");
    }
}
