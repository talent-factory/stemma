//! Checking the claims of an index, a tree, a commit, or the references against a store.
//!
//! The readers [`Tree::parse`] and [`Commit::parse`] claim the kind of each ID they find: the mode
//! of an entry says whether it names a tree or a blob, and the headers of a commit name a tree and
//! parents. A reader has no store, so it cannot know. [`Tree::verify`] and [`Commit::verify`]
//! check the claims of one object against a store, as [`Index::verify`](crate::index::Index::verify)
//! does for an index. [`FsRefStore::verify`](crate::refs::FsRefStore::verify) checks the commits
//! that the references name (ADR-008).
//!
//! A check that fails gives a [`VerifyError`] for the place of the claim: [`IndexVerifyError`],
//! [`TreeVerifyError`], [`CommitVerifyError`], and [`RefVerifyError`] are that one type, so that a
//! caller handles the error of a check in one way, with `match` and with `store_error()`.
//!
//! The check is shallow: it looks at the objects that the index, the tree, the commit, or the
//! references name directly, and does not follow them. Each object is read and verified
//! completely (see [`ObjectStore::resolve_blob`]), so the cost grows with the size of the objects
//! named. The result holds at the time of the call; for the references, which are read without
//! the lock, see [`FsRefStore::verify`](crate::refs::FsRefStore::verify).

use std::error::Error;
use std::fmt;

use crate::object::{Commit, Commit2, EntryMode, ObjectId, Tree};
use crate::oplog::Pointer;
use crate::store::{ObjectStore, StoreError};

/// The path of an entry of an index. It is a different thing from the name of an entry of a tree
/// ([`EntryName`]): a path has `/`, a name has none. Its `Display` form is the path in backticks
/// and escaped, because a path may contain anything but control characters, and so that it cannot
/// pass for a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPath(pub String);

impl EntryPath {
    /// Returns the path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EntryPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`", self.0.escape_debug())
    }
}

/// The name of an entry of a tree. It is a different thing from the path of an entry of an index
/// ([`EntryPath`]). Its `Display` form is the name in backticks and escaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryName(pub String);

impl EntryName {
    /// Returns the name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EntryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`", self.0.escape_debug())
    }
}

/// Why a check of the claims of an object or a reference against a store fails. Carries the
/// place of one failing claim: the check of an index, a tree, or a commit reports the first in
/// the order of the object, and the check of the references reports each reference that fails.
/// The `Display` form of the place starts the message: [`IndexVerifyError`] for an index
/// ([`EntryPath`]), [`TreeVerifyError`] for a tree ([`EntryName`]), [`CommitVerifyError`] for a
/// commit ([`CommitReference`]), and [`RefVerifyError`] for a reference ([`Pointer`]) are this
/// type for their place.
#[derive(Debug)]
#[non_exhaustive]
pub enum VerifyError<L> {
    /// The store has no object with the ID that the claim names.
    Missing {
        /// Where the claim is.
        at: L,
        /// The ID the claim names.
        id: ObjectId,
    },
    /// The store could not resolve the ID with the kind that the claim makes: the object has
    /// another kind ([`StoreError::KindMismatch`]), it is damaged, or the storage failed.
    Store {
        /// Where the claim is.
        at: L,
        /// What the store reported. Boxed because the unboxed error is too large for clippy's
        /// `result_large_err` lint.
        source: Box<StoreError>,
    },
}

/// Why an index fails [`Index::verify`](crate::index::Index::verify). The place is the path of
/// the first failing entry, in index order.
pub type IndexVerifyError = VerifyError<EntryPath>;

/// Why a tree fails [`Tree::verify`]. The place is the name of the first failing entry, in name
/// order.
pub type TreeVerifyError = VerifyError<EntryName>;

/// Why a commit fails [`Commit::verify`]. The place is the tree or the parent.
pub type CommitVerifyError = VerifyError<CommitReference>;

/// Why a reference fails [`FsRefStore::verify`](crate::refs::FsRefStore::verify). The place is
/// the pointer: HEAD, the working-copy commit, or a branch.
pub type RefVerifyError = VerifyError<Pointer>;

impl<L: fmt::Debug + fmt::Display> VerifyError<L> {
    /// Turns the answer of a store to "is there an object of the claimed kind" into the result of
    /// a check: found is `Ok`, not found is `Missing`, and an error of the store is `Store`.
    ///
    /// The place is made by a function, so that it costs nothing when the claim holds.
    pub(crate) fn check(
        at: impl FnOnce() -> L,
        id: ObjectId,
        found: Result<bool, StoreError>,
    ) -> Result<(), Self> {
        match found {
            Ok(true) => Ok(()),
            Ok(false) => Err(Self::Missing { at: at(), id }),
            Err(source) => Err(Self::Store {
                at: at(),
                source: Box::new(source),
            }),
        }
    }

    /// Returns the place of the claim that failed.
    pub fn at(&self) -> &L {
        match self {
            Self::Missing { at, .. } | Self::Store { at, .. } => at,
        }
    }

    /// Returns the error of the store, unless the store has no such object.
    pub fn store_error(&self) -> Option<&StoreError> {
        match self {
            Self::Store { source, .. } => Some(source),
            Self::Missing { .. } => None,
        }
    }
}

impl<L: fmt::Debug + fmt::Display> fmt::Display for VerifyError<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { at, id } => write!(f, "{at}: the store has no object {id}"),
            Self::Store { at, source } => write!(f, "{at}: {source}"),
        }
    }
}

impl<L: fmt::Debug + fmt::Display> Error for VerifyError<L> {
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
    /// Returns the first problem in name order: [`VerifyError::Missing`] if the store has no
    /// such object, and [`VerifyError::Store`] if the object has another kind, is damaged, or
    /// the storage fails.
    pub fn verify(&self, store: &dyn ObjectStore) -> Result<(), TreeVerifyError> {
        for entry in self.entries() {
            let id = entry.id();
            let found = if entry.mode() == EntryMode::Directory {
                store.resolve_tree(&id).map(|found| found.is_some())
            } else {
                store.resolve_blob(&id).map(|found| found.is_some())
            };
            VerifyError::check(|| EntryName(entry.name().to_owned()), id, found)?;
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
    /// A tree of the list of a [`Commit2`].
    ListedTree {
        /// The position in the list of trees, counted from 0 as in [`Commit2::trees`]. The
        /// `Display` form counts from 1: index 0 is "tree 1".
        index: usize,
    },
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
            Self::ListedTree { index } => write!(f, "tree {}", index + 1),
            Self::Parent { index } => write!(f, "parent {}", index + 1),
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
    /// [`VerifyError::Missing`] if the store has no such object, and
    /// [`VerifyError::Store`] if the object has another kind, is damaged, or the storage
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
                CommitReference::Tree | CommitReference::ListedTree { .. } => {
                    store.resolve_tree(&id).map(|found| found.is_some())
                }
                CommitReference::Parent { .. } => {
                    store.resolve_commit(&id).map(|found| found.is_some())
                }
            };
            VerifyError::check(|| reference, id, found)?;
        }
        Ok(())
    }
}

impl Commit2 {
    /// Checks that every tree of the list is a tree and every parent is a commit that the store
    /// has, as [`Commit::verify`] does for one tree. The trees are checked in the order of the
    /// list, before the parents.
    ///
    /// # Errors
    ///
    /// Returns the first problem as [`Commit::verify`] does, with [`CommitReference::ListedTree`]
    /// as the place of a tree.
    pub fn verify(&self, store: &dyn ObjectStore) -> Result<(), CommitVerifyError> {
        let trees =
            self.trees().iter().enumerate().map(|(index, tree)| {
                (CommitReference::ListedTree { index }, ObjectId::from(*tree))
            });
        let parents =
            self.parents().iter().enumerate().map(|(index, parent)| {
                (CommitReference::Parent { index }, ObjectId::from(*parent))
            });
        for (reference, id) in trees.chain(parents) {
            let found = match reference {
                CommitReference::Parent { .. } => {
                    store.resolve_commit(&id).map(|found| found.is_some())
                }
                _ => store.resolve_tree(&id).map(|found| found.is_some()),
            };
            VerifyError::check(|| reference, id, found)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::sample_change;
    use crate::object::{Blob, BlobId, CommitId, FileMode, TreeEntry, TreeId};
    use crate::store::FsObjectStore;
    use crate::testutil::{overwrite, TempDir};
    use crate::Actor;

    fn commit(tree: TreeId, parents: Vec<CommitId>) -> Commit {
        Commit::try_new(
            tree,
            parents,
            sample_change(),
            Actor::human("daniel").unwrap(),
            0,
            "m",
        )
        .unwrap()
    }

    fn empty_tree(store: &FsObjectStore) -> TreeId {
        store.put_tree(&Tree::try_new(vec![]).unwrap()).unwrap()
    }

    fn absent_blob() -> BlobId {
        Blob::new("absent").id()
    }

    fn absent_tree() -> TreeId {
        Tree::try_new(vec![
            TreeEntry::file("x", FileMode::File, absent_blob()).unwrap()
        ])
        .unwrap()
        .id()
    }

    // ---- VerifyError

    #[test]
    fn the_errors_of_an_index_a_tree_and_a_commit_are_one_generic_type_with_three_places() {
        use std::any::TypeId;
        assert_eq!(
            TypeId::of::<IndexVerifyError>(),
            TypeId::of::<VerifyError<EntryPath>>()
        );
        assert_eq!(
            TypeId::of::<TreeVerifyError>(),
            TypeId::of::<VerifyError<EntryName>>()
        );
        assert_eq!(
            TypeId::of::<CommitVerifyError>(),
            TypeId::of::<VerifyError<CommitReference>>()
        );
        // The path of an index entry and the name of a tree entry are not mixed up: a function
        // for the error of an index does not take the error of a tree.
        assert_ne!(
            TypeId::of::<IndexVerifyError>(),
            TypeId::of::<TreeVerifyError>()
        );
        assert_ne!(
            TypeId::of::<TreeVerifyError>(),
            TypeId::of::<CommitVerifyError>()
        );
    }

    #[test]
    fn a_check_turns_the_answer_of_a_store_into_a_result() {
        let id = ObjectId::from(absent_blob());
        assert!(VerifyError::check(|| EntryName("a".to_owned()), id, Ok(true)).is_ok());
        match VerifyError::check(|| EntryName("a".to_owned()), id, Ok(false)).unwrap_err() {
            VerifyError::Missing { at, id: found } => assert_eq!((at.as_str(), found), ("a", id)),
            other => panic!("unexpected error: {other}"),
        }
        let failure = StoreError::KindMismatch {
            id,
            expected: crate::object::Kind::Blob,
            found: crate::object::Kind::Tree,
        };
        let error = VerifyError::check(|| CommitReference::Tree, id, Err(failure)).unwrap_err();
        assert!(matches!(error, VerifyError::Store { .. }), "{error}");
        assert!(matches!(
            error.store_error(),
            Some(StoreError::KindMismatch { .. })
        ));
        assert_eq!(
            error.to_string(),
            format!("the tree: object {id} is a tree, not a blob")
        );
    }

    #[test]
    fn a_place_is_written_as_the_start_of_the_message() {
        let id = ObjectId::from(absent_blob());
        let missing = |at: EntryPath| VerifyError::Missing { at, id }.to_string();
        // A path or a name is in backticks and escaped, so that it cannot pass for a message.
        assert!(missing(EntryPath("a\"b\n".to_owned()))
            .starts_with("`a\\\"b\\n`: the store has no object "));
        let by_name = VerifyError::Missing {
            at: EntryName("a\"b\n".to_owned()),
            id,
        };
        assert!(by_name
            .to_string()
            .starts_with("`a\\\"b\\n`: the store has no object "));
        assert_eq!(by_name.at().as_str(), "a\"b\n");
        let parent = VerifyError::Missing {
            at: CommitReference::Parent { index: 0 },
            id,
        };
        assert!(parent
            .to_string()
            .starts_with("parent 1: the store has no object "));
        assert_eq!(*parent.at(), CommitReference::Parent { index: 0 });
    }

    #[test]
    fn the_place_is_made_only_when_a_claim_fails() {
        let id = ObjectId::from(absent_blob());
        let place = |made: &std::cell::Cell<usize>| {
            made.set(made.get() + 1);
            EntryName("a".to_owned())
        };
        let made = std::cell::Cell::new(0);
        assert!(VerifyError::check(|| place(&made), id, Ok(true)).is_ok());
        assert_eq!(made.get(), 0, "a claim that holds makes no place");
        assert!(VerifyError::check(|| place(&made), id, Ok(false)).is_err());
        assert_eq!(made.get(), 1);
    }

    // ---- Tree::verify

    #[test]
    fn a_tree_of_stored_objects_verifies() {
        let dir = TempDir::new();
        let store = dir.store();
        let blob = store.put_blob(&Blob::new("hello")).unwrap();
        let sub = empty_tree(&store);
        let tree = Tree::try_new(vec![
            TreeEntry::file("a", FileMode::File, blob).unwrap(),
            TreeEntry::file("b", FileMode::Executable, blob).unwrap(),
            TreeEntry::file("c", FileMode::Symlink, blob).unwrap(),
            TreeEntry::directory("d", sub).unwrap(),
        ])
        .unwrap();
        assert!(tree.verify(&store).is_ok());
        assert!(Tree::try_new(vec![]).unwrap().verify(&store).is_ok());
    }

    #[test]
    fn a_tree_entry_with_no_object_is_missing_for_a_file_and_for_a_directory() {
        let dir = TempDir::new();
        let store = dir.store();
        let file = Tree::try_new(vec![
            TreeEntry::file("f", FileMode::File, absent_blob()).unwrap()
        ])
        .unwrap();
        match file.verify(&store).unwrap_err() {
            TreeVerifyError::Missing { at: name, id } => {
                assert_eq!(name.as_str(), "f");
                assert_eq!(id, ObjectId::from(absent_blob()));
            }
            other => panic!("unexpected error: {other}"),
        }
        let directory =
            Tree::try_new(vec![TreeEntry::directory("d", absent_tree()).unwrap()]).unwrap();
        match directory.verify(&store).unwrap_err() {
            TreeVerifyError::Missing { at: name, id } => {
                assert_eq!(name.as_str(), "d");
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
            match Tree::try_new(vec![entry])
                .unwrap()
                .verify(&store)
                .unwrap_err()
            {
                TreeVerifyError::Store { at: found, source } => {
                    assert_eq!(found.as_str(), name);
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
        let tree =
            Tree::try_new(vec![TreeEntry::file("f", FileMode::File, blob).unwrap()]).unwrap();
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
            Tree::try_new(vec![second, first]).unwrap().verify(&store)
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
        let tree = Tree::try_new(vec![TreeEntry::file(
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
        let tree = Tree::try_new(vec![
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
            Tree::try_new(vec![
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
                (TreeVerifyError::Missing { at: name, .. }, true) => assert_eq!(name.as_str(), "c"),
                (TreeVerifyError::Store { at: name, .. }, false) => assert_eq!(name.as_str(), "c"),
                (other, _) => panic!("unexpected error: {other}"),
            }
        }
        // A damaged blob after good entries is a store error as well.
        let damaged = store.put_blob(&Blob::new("damaged")).unwrap();
        overwrite(&store.object_path(&ObjectId::from(damaged)), b"blob 1\0x");
        let entry = TreeEntry::file("c", FileMode::File, damaged).unwrap();
        assert!(matches!(
            verify(entry),
            Err(TreeVerifyError::Store { ref at, .. }) if at.as_str() == "c"
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
            CommitVerifyError::Missing { at: reference, id } => {
                assert_eq!(reference, CommitReference::Tree);
                assert_eq!(id, ObjectId::from(absent_tree()));
            }
            other => panic!("unexpected error: {other}"),
        }
        match commit(tree, vec![parent, absent_parent])
            .verify(&store)
            .unwrap_err()
        {
            CommitVerifyError::Missing { at: reference, id } => {
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
                CommitVerifyError::Store { at: found, source } => {
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
    fn a_commit2_verifies_every_tree_of_its_list_and_names_the_position() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = empty_tree(&store);
        let parent = store.put_commit(&commit(tree, vec![])).unwrap();
        let commit2 = |trees: Vec<TreeId>, parents: Vec<CommitId>| {
            Commit2::try_new(
                trees,
                parents,
                sample_change(),
                Actor::human("daniel").unwrap(),
                0,
                "m",
            )
            .unwrap()
        };
        assert!(commit2(vec![tree; 3], vec![parent]).verify(&store).is_ok());
        // The first tree that is absent is named, in the order of the list.
        let absent = absent_tree();
        match commit2(vec![tree, tree, absent, tree, absent], vec![])
            .verify(&store)
            .unwrap_err()
        {
            CommitVerifyError::Missing { at, id } => {
                assert_eq!(at, CommitReference::ListedTree { index: 2 });
                assert_eq!(at.to_string(), "tree 3");
                assert_eq!(id, ObjectId::from(absent));
            }
            other => panic!("unexpected error: {other}"),
        }
        // A tree of the list that is a commit is a store error with its position.
        let parent_as_tree = TreeId::new(ObjectId::from(parent));
        match commit2(vec![tree, parent_as_tree, tree], vec![])
            .verify(&store)
            .unwrap_err()
        {
            CommitVerifyError::Store { at, source } => {
                assert_eq!(at, CommitReference::ListedTree { index: 1 });
                assert!(
                    matches!(*source, StoreError::KindMismatch { .. }),
                    "{source}"
                );
            }
            other => panic!("unexpected error: {other}"),
        }
        // The trees are checked before the parents.
        let absent_parent = commit(absent, vec![]).id();
        match commit2(vec![tree, tree, absent], vec![absent_parent])
            .verify(&store)
            .unwrap_err()
        {
            CommitVerifyError::Missing { at, .. } => {
                assert_eq!(at, CommitReference::ListedTree { index: 2 });
            }
            other => panic!("unexpected error: {other}"),
        }
        match commit2(vec![tree; 3], vec![parent, absent_parent])
            .verify(&store)
            .unwrap_err()
        {
            CommitVerifyError::Missing { at, .. } => {
                assert_eq!(at, CommitReference::Parent { index: 1 });
            }
            other => panic!("unexpected error: {other}"),
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
            CommitVerifyError::Missing { at: reference, .. } => {
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
