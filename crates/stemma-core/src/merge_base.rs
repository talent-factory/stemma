//! The best common ancestors of two commits (the merge bases), as a pure function over the object
//! store.
//!
//! The function walks the full ancestor sets of both tips, intersects them, and removes every
//! candidate that is an ancestor of another one. It uses no commit date, keeps no cache, and
//! changes no format: a commit date is self-reported (see [`crate::object::Commit`]) and cannot
//! order a walk, and the cost of the full walk is linear in the commits reachable from the two
//! tips. A generation number would shorten the walk; ADR-017 names it as an optimisation for the
//! day that a measurement asks for it.
//!
//! A history with a criss-cross merge has more than one best common ancestor. The function returns
//! all of them, and the caller combines them.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::rc::Rc;

use crate::object::CommitId;
use crate::store::{ObjectStore, ReadError, StoreError};

/// Returns the best common ancestors of two commits, sorted by ID.
///
/// A commit counts as an ancestor of itself. The result is empty if the two commits share no
/// ancestor, has one commit for a linear history or a simple fork, and has the older of the two
/// commits itself if one of them is an ancestor of the other. No commit of the result is an
/// ancestor of another one, and `merge_bases(store, a, b)` equals `merge_bases(store, b, a)`.
///
/// # Errors
///
/// Returns [`MergeBaseError::Missing`] if the store lacks a commit that the walk reaches, and
/// [`MergeBaseError::Read`] if the store cannot return one intact.
pub fn merge_bases(
    store: &dyn ObjectStore,
    a: CommitId,
    b: CommitId,
) -> Result<Vec<CommitId>, MergeBaseError> {
    let mut walk = Walk::new(store);
    let from_a = walk.ancestors(&[a])?;
    let from_b = walk.ancestors(&[b])?;
    // Sorted before the walk below, so that the error of a damaged history does not depend on the
    // order of a hash set.
    let mut candidates: Vec<CommitId> = from_a.intersection(&from_b).copied().collect();
    candidates.sort();

    // A candidate that is an ancestor of another one is not a best ancestor. The proper ancestors
    // of all candidates are the ancestors of their parents, found in one walk.
    let mut parents = Vec::new();
    for candidate in &candidates {
        parents.extend_from_slice(&walk.parents(*candidate, None)?);
    }
    let shadowed = walk.ancestors(&parents)?;

    candidates.retain(|candidate| !shadowed.contains(candidate));
    Ok(candidates)
}

/// Reads the parents of commits from a store and remembers them, so that no commit is read twice.
/// The memory cannot go stale: a commit is addressed by its content and never changes.
struct Walk<'a> {
    store: &'a dyn ObjectStore,
    parents: HashMap<CommitId, Rc<[CommitId]>>,
}

impl<'a> Walk<'a> {
    fn new(store: &'a dyn ObjectStore) -> Self {
        Self {
            store,
            parents: HashMap::new(),
        }
    }

    /// The parents of `id`. `child` is the commit that `id` was reached from, for the error: a
    /// commit that is absent is a defect of the history if it is a parent, and a wrong argument
    /// if it is a tip.
    fn parents(
        &mut self,
        id: CommitId,
        child: Option<CommitId>,
    ) -> Result<Rc<[CommitId]>, MergeBaseError> {
        if let Some(known) = self.parents.get(&id) {
            return Ok(Rc::clone(known));
        }
        // A commit that a reachable commit names must exist, so `None` is an error here, while
        // the store returns it as a normal answer.
        let commit = self
            .store
            .get_commit(&id)?
            .ok_or(MergeBaseError::Missing { id, child })?;
        let parents: Rc<[CommitId]> = commit.parents().into();
        self.parents.insert(id, Rc::clone(&parents));
        Ok(parents)
    }

    /// The commits reachable from `starts` through parents, the starts included.
    fn ancestors(&mut self, starts: &[CommitId]) -> Result<HashSet<CommitId>, MergeBaseError> {
        let mut seen = HashSet::new();
        let mut pending = Vec::new();
        for &start in starts {
            if seen.insert(start) {
                pending.push((start, None));
            }
        }
        while let Some((id, child)) = pending.pop() {
            for &parent in self.parents(id, child)?.iter() {
                if seen.insert(parent) {
                    pending.push((parent, Some(id)));
                }
            }
        }
        Ok(seen)
    }
}

/// Why [`merge_bases`] fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum MergeBaseError {
    /// The store does not hold a commit that the walk reached: a tip, or a parent of a commit.
    Missing {
        /// The commit that is absent.
        id: CommitId,
        /// The commit that names it as a parent, or `None` if it is one of the two tips.
        child: Option<CommitId>,
    },
    /// The store could not return a commit. The message and the cause are those of the read error.
    Read(ReadError),
}

impl MergeBaseError {
    /// Returns the error of the store if the store could not return a commit: the storage failed,
    /// the object is damaged, or it has another kind. `None` for a commit that is absent and for
    /// one whose body is not in canonical form.
    pub fn store_error(&self) -> Option<&StoreError> {
        match self {
            Self::Read(source) => source.store_error(),
            Self::Missing { .. } => None,
        }
    }
}

impl fmt::Display for MergeBaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { id, child: None } => {
                write!(f, "commit {id} is not in the object store")
            }
            Self::Missing {
                id,
                child: Some(child),
            } => write!(
                f,
                "commit {id}, a parent of commit {child}, is not in the object store"
            ),
            Self::Read(source) => write!(f, "{source}"),
        }
    }
}

impl Error for MergeBaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Missing { .. } => None,
            // Transparent: the message is that of the read error, so its cause is the cause here.
            Self::Read(source) => source.source(),
        }
    }
}

impl From<ReadError> for MergeBaseError {
    fn from(source: ReadError) -> Self {
        Self::Read(source)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::change::sample_change;
    use crate::object::{Blob, BlobId, Commit, ObjectId, Tree, TreeId};
    use crate::properties::{cases, seed, Rng};
    use crate::store::{sealed, RawObject};
    use crate::testutil::{FaultyStore, TempDir, DISK_MESSAGE};
    use crate::Actor;

    /// Stores a commit with the given parents. `n` makes the commit unique.
    fn commit(store: &dyn ObjectStore, parents: &[CommitId], n: i64) -> CommitId {
        let tree = store.put_tree(&Tree::try_new(vec![]).unwrap()).unwrap();
        let commit = Commit::try_new(
            tree,
            parents.to_vec(),
            sample_change(),
            Actor::human("daniel").unwrap(),
            n,
            format!("commit {n}"),
        )
        .unwrap();
        store.put_commit(&commit).unwrap()
    }

    fn sorted(mut ids: Vec<CommitId>) -> Vec<CommitId> {
        ids.sort();
        ids
    }

    /// A store that counts the objects it returns, to show that the walk reads each commit once.
    struct CountingStore<'a> {
        inner: &'a dyn ObjectStore,
        reads: Cell<usize>,
    }

    impl sealed::Sealed for CountingStore<'_> {}

    impl ObjectStore for CountingStore<'_> {
        fn put_blob(&self, blob: &Blob) -> Result<BlobId, StoreError> {
            self.inner.put_blob(blob)
        }

        fn put_tree(&self, tree: &Tree) -> Result<TreeId, StoreError> {
            self.inner.put_tree(tree)
        }

        fn put_commit(&self, commit: &Commit) -> Result<CommitId, StoreError> {
            self.inner.put_commit(commit)
        }

        fn get(&self, id: &ObjectId) -> Result<Option<RawObject>, StoreError> {
            self.reads.set(self.reads.get() + 1);
            self.inner.get(id)
        }

        fn contains(&self, id: &ObjectId) -> Result<bool, StoreError> {
            self.inner.contains(id)
        }
    }

    #[test]
    fn a_commit_and_itself_have_that_commit_as_the_base() {
        let dir = TempDir::new();
        let store = dir.store();
        let root = commit(&store, &[], 0);
        assert_eq!(merge_bases(&store, root, root).unwrap(), vec![root]);
    }

    #[test]
    fn the_ancestor_is_the_base_when_one_side_is_an_ancestor_of_the_other() {
        let dir = TempDir::new();
        let store = dir.store();
        let root = commit(&store, &[], 0);
        let middle = commit(&store, &[root], 1);
        let tip = commit(&store, &[middle], 2);
        assert_eq!(merge_bases(&store, root, tip).unwrap(), vec![root]);
        assert_eq!(merge_bases(&store, tip, middle).unwrap(), vec![middle]);
    }

    #[test]
    fn a_fork_has_the_commit_where_it_forks_as_the_only_base() {
        let dir = TempDir::new();
        let store = dir.store();
        let root = commit(&store, &[], 0);
        let fork = commit(&store, &[root], 1);
        let left = commit(&store, &[fork], 2);
        let right = commit(&store, &[fork], 3);
        let left_tip = commit(&store, &[left], 4);
        assert_eq!(merge_bases(&store, left_tip, right).unwrap(), vec![fork]);
        assert_eq!(merge_bases(&store, right, left_tip).unwrap(), vec![fork]);
    }

    #[test]
    fn histories_without_a_common_ancestor_have_no_base() {
        let dir = TempDir::new();
        let store = dir.store();
        let one = commit(&store, &[], 0);
        let other = commit(&store, &[], 1);
        assert_eq!(merge_bases(&store, one, other).unwrap(), vec![]);
    }

    #[test]
    fn a_merge_commit_is_the_base_of_what_continues_after_it() {
        let dir = TempDir::new();
        let store = dir.store();
        let root = commit(&store, &[], 0);
        let left = commit(&store, &[root], 1);
        let right = commit(&store, &[root], 2);
        let merge = commit(&store, &[left, right], 3);
        let one = commit(&store, &[merge], 4);
        let two = commit(&store, &[merge], 5);
        assert_eq!(merge_bases(&store, one, two).unwrap(), vec![merge]);
        // `left` is an ancestor of `merge`, so it is the base. `root` is common as well, and
        // `left` shadows it.
        assert_eq!(merge_bases(&store, merge, left).unwrap(), vec![left]);
    }

    #[test]
    fn a_criss_cross_has_two_bases() {
        let dir = TempDir::new();
        let store = dir.store();
        let root = commit(&store, &[], 0);
        let a = commit(&store, &[root], 1);
        let b = commit(&store, &[root], 2);
        // Each side merges the other one: neither merge is an ancestor of the other.
        let x = commit(&store, &[a, b], 3);
        let y = commit(&store, &[b, a], 4);
        let expected = sorted(vec![a, b]);
        assert_eq!(merge_bases(&store, x, y).unwrap(), expected);
        assert_eq!(merge_bases(&store, y, x).unwrap(), expected);
    }

    #[test]
    fn a_long_linear_history_is_walked_in_full_and_reads_each_commit_once() {
        const LENGTH: i64 = 2_000;
        let dir = TempDir::new();
        let store = dir.store();
        let mut tip = commit(&store, &[], 0);
        let mut middle = tip;
        for n in 1..LENGTH {
            tip = commit(&store, &[tip], n);
            if n == LENGTH / 2 {
                middle = tip;
            }
        }
        let counting = CountingStore {
            inner: &store,
            reads: Cell::new(0),
        };
        assert_eq!(merge_bases(&counting, tip, middle).unwrap(), vec![middle]);
        // A linear cost, not a quadratic one: the walk reads each commit once.
        assert_eq!(counting.reads.get(), LENGTH as usize);
    }

    #[test]
    fn an_absent_tip_is_reported_for_either_argument() {
        let dir = TempDir::new();
        let store = dir.store();
        let other = TempDir::new();
        let absent = commit(&other.store(), &[], 0);
        let present = commit(&store, &[], 1);
        for (a, b) in [(present, absent), (absent, present)] {
            let error = merge_bases(&store, a, b).unwrap_err();
            assert!(
                matches!(error, MergeBaseError::Missing { id, child: None } if id == absent),
                "{error}"
            );
            assert!(error.to_string().contains(&absent.to_string()));
            assert!(error.store_error().is_none());
        }
    }

    #[test]
    fn an_absent_parent_is_reported_with_the_commit_that_names_it() {
        let dir = TempDir::new();
        let store = dir.store();
        let other = TempDir::new();
        let absent = commit(&other.store(), &[], 0);
        let child = commit(&store, &[absent], 1);
        let tip = commit(&store, &[child], 2);
        let error = merge_bases(&store, tip, tip).unwrap_err();
        assert!(
            matches!(error, MergeBaseError::Missing { id, child: Some(c) } if id == absent && c == child),
            "{error}"
        );
        let text = error.to_string();
        assert!(text.contains(&absent.to_string()), "{text}");
        assert!(text.contains(&child.to_string()), "{text}");
    }

    #[test]
    fn a_failing_store_is_reported_with_its_message_once_and_keeps_its_cause() {
        let dir = TempDir::new();
        let root = commit(&dir.store(), &[], 0);
        let store = FaultyStore::new(dir.store(), true, false);
        let error = merge_bases(&store, root, root).unwrap_err();
        assert!(matches!(error, MergeBaseError::Read(_)));
        let text = error.to_string();
        assert!(text.ends_with(DISK_MESSAGE), "{text}");
        assert_eq!(text.matches(DISK_MESSAGE).count(), 1, "{text}");
        // The chain follows the convention of the crate: one link below the message.
        let first = error.source().expect("the cause of the cause");
        assert_eq!(first.to_string(), "the deepest cause");
        assert!(first.source().is_none());
        assert!(error.store_error().is_some());
    }

    /// Every ancestor of `id`, `id` included, computed without the store.
    fn reference_ancestors(history: &[Vec<usize>], id: usize) -> HashSet<usize> {
        let mut seen = HashSet::from([id]);
        let mut pending = vec![id];
        while let Some(next) = pending.pop() {
            for &parent in &history[next] {
                if seen.insert(parent) {
                    pending.push(parent);
                }
            }
        }
        seen
    }

    /// The bases by the definition: common ancestors that are no proper ancestor of another one.
    fn reference_bases(history: &[Vec<usize>], a: usize, b: usize) -> Vec<usize> {
        let from_a = reference_ancestors(history, a);
        let from_b = reference_ancestors(history, b);
        let common: Vec<usize> = from_a.intersection(&from_b).copied().collect();
        let mut best: Vec<usize> = common
            .iter()
            .copied()
            .filter(|&c| {
                !common
                    .iter()
                    .any(|&other| other != c && reference_ancestors(history, other).contains(&c))
            })
            .collect();
        best.sort_unstable();
        best
    }

    #[test]
    fn generated_histories_agree_with_the_definition_and_keep_the_laws() {
        const COMMITS: usize = 40;
        let histories = (cases() / 400).max(1);
        // How often the generator produced the interesting results, so that the test cannot pass
        // on trivial histories only.
        let (mut none, mut several) = (0, 0);
        for round in 0..histories {
            let mut rng = Rng::new(seed(), 0x6D62 + round as u64);
            let dir = TempDir::new();
            let store = dir.store();
            // Commit i has zero, one, or two distinct parents among the commits before it, so the
            // generated graphs contain forks, merges, several roots, and criss-crosses.
            let mut shape: Vec<Vec<usize>> = Vec::new();
            let mut ids: Vec<CommitId> = Vec::new();
            for i in 0..COMMITS {
                let mut parents = Vec::new();
                if i > 0 {
                    for _ in 0..rng.below(3) {
                        let parent = rng.below(i);
                        if !parents.contains(&parent) {
                            parents.push(parent);
                        }
                    }
                }
                let parent_ids: Vec<CommitId> = parents.iter().map(|&p| ids[p]).collect();
                ids.push(commit(&store, &parent_ids, i as i64));
                shape.push(parents);
            }
            for _ in 0..200 {
                let (a, b) = (rng.below(COMMITS), rng.below(COMMITS));
                let found = merge_bases(&store, ids[a], ids[b]).unwrap();
                let expected = sorted(
                    reference_bases(&shape, a, b)
                        .into_iter()
                        .map(|i| ids[i])
                        .collect(),
                );
                assert_eq!(
                    found,
                    expected,
                    "seed {}, round {round}, {a} and {b}",
                    seed()
                );
                none += usize::from(found.is_empty());
                several += usize::from(found.len() >= 2);
                // Symmetry.
                assert_eq!(merge_bases(&store, ids[b], ids[a]).unwrap(), found);
                // The result is an antichain: no base is an ancestor of another one.
                let bases = reference_bases(&shape, a, b);
                for &x in &bases {
                    for &y in &bases {
                        assert!(
                            x == y || !reference_ancestors(&shape, y).contains(&x),
                            "seed {}, round {round}, {x} below {y}",
                            seed()
                        );
                    }
                }
                // The older commit is the base if one side is an ancestor of the other.
                if reference_ancestors(&shape, a).contains(&b) {
                    assert_eq!(found, vec![ids[b]]);
                }
                if reference_ancestors(&shape, b).contains(&a) {
                    assert_eq!(found, vec![ids[a]]);
                }
            }
        }
        assert!(none > 0, "no generated pair without a common ancestor");
        assert!(several > 0, "no generated pair with several bases");
    }
}
