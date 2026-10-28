//! The best common ancestors of two commits (the merge bases), as a pure function over the object
//! store.
//!
//! The function walks the full ancestor sets of both tips, intersects them, and removes every
//! candidate that is an ancestor of another one. It reads no commit date, keeps no cache, and
//! changes no format: a commit date is self-reported (see [`crate::object::Commit`]) and cannot order a walk, and
//! the cost of the full walk is linear in the size of the history. A generation number would
//! shorten the walk; ADR-017 names it as an optimisation for the day that a measurement asks for
//! it.
//!
//! A history with a criss-cross merge has more than one best common ancestor. The function returns
//! all of them, and the caller combines them.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;

use crate::object::CommitId;
use crate::store::{ObjectStore, ReadError};

/// Returns the best common ancestors of two commits, sorted by ID.
///
/// A commit counts as an ancestor of itself. The result is empty if the two commits share no
/// ancestor, has one commit for a linear history or a simple fork, and has the tip itself if one
/// of the commits is an ancestor of the other. No commit of the result is an ancestor of another
/// one, and `merge_bases(store, a, b)` equals `merge_bases(store, b, a)`.
///
/// # Errors
///
/// Returns [`MergeBaseError::Missing`] if the store lacks a commit that the walk reaches, and
/// [`MergeBaseError::Read`] if the store cannot return one.
pub fn merge_bases(
    store: &impl ObjectStore,
    a: CommitId,
    b: CommitId,
) -> Result<Vec<CommitId>, MergeBaseError> {
    let mut walk = Walk::new(store);
    let from_a = walk.ancestors(&[a])?;
    let from_b = walk.ancestors(&[b])?;
    let candidates: Vec<CommitId> = from_a.intersection(&from_b).copied().collect();

    // A candidate that is an ancestor of another one is not a best ancestor. The proper ancestors
    // of all candidates are the ancestors of their parents, found in one walk.
    let mut parents = Vec::new();
    for candidate in &candidates {
        parents.extend_from_slice(&walk.parents(*candidate)?);
    }
    let shadowed = walk.ancestors(&parents)?;

    let mut best: Vec<CommitId> = candidates
        .into_iter()
        .filter(|candidate| !shadowed.contains(candidate))
        .collect();
    best.sort();
    Ok(best)
}

/// Reads the parents of commits from a store and remembers them, so that the second walk does not
/// read a commit again.
struct Walk<'a, S> {
    store: &'a S,
    parents: HashMap<CommitId, Vec<CommitId>>,
}

impl<'a, S: ObjectStore> Walk<'a, S> {
    fn new(store: &'a S) -> Self {
        Self {
            store,
            parents: HashMap::new(),
        }
    }

    fn parents(&mut self, id: CommitId) -> Result<Vec<CommitId>, MergeBaseError> {
        if let Some(known) = self.parents.get(&id) {
            return Ok(known.clone());
        }
        let commit = self
            .store
            .get_commit(&id)?
            .ok_or(MergeBaseError::Missing(id))?;
        let parents = commit.parents().to_vec();
        self.parents.insert(id, parents.clone());
        Ok(parents)
    }

    /// The commits reachable from `starts` through parents, the starts included.
    fn ancestors(&mut self, starts: &[CommitId]) -> Result<HashSet<CommitId>, MergeBaseError> {
        let mut seen: HashSet<CommitId> = starts.iter().copied().collect();
        let mut pending: Vec<CommitId> = seen.iter().copied().collect();
        while let Some(id) = pending.pop() {
            for parent in self.parents(id)? {
                if seen.insert(parent) {
                    pending.push(parent);
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
    Missing(CommitId),
    /// The store could not return a commit. The message and the cause are those of the read error.
    Read(ReadError),
}

impl fmt::Display for MergeBaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(id) => write!(f, "commit {id} is not in the object store"),
            Self::Read(source) => write!(f, "{source}"),
        }
    }
}

impl Error for MergeBaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Missing(_) => None,
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
    use super::*;
    use crate::change::sample_change;
    use crate::object::{Commit, Tree};
    use crate::properties::{cases, seed, Rng};
    use crate::testutil::{FaultyStore, TempDir};
    use crate::Actor;

    /// Stores a commit with the given parents. `n` makes the commit unique.
    fn commit(store: &impl ObjectStore, parents: &[CommitId], n: i64) -> CommitId {
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

    #[test]
    fn a_commit_and_itself_have_that_commit_as_the_base() {
        let dir = TempDir::new();
        let store = dir.store();
        let root = commit(&store, &[], 0);
        assert_eq!(merge_bases(&store, root, root).unwrap(), vec![root]);
    }

    #[test]
    fn the_tip_is_the_base_when_one_side_is_an_ancestor_of_the_other() {
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
        // The merge joins both sides, so the sides are ancestors of the base and not bases.
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
    fn a_long_linear_history_is_walked_in_full() {
        let dir = TempDir::new();
        let store = dir.store();
        let mut tip = commit(&store, &[], 0);
        let mut middle = tip;
        for n in 1..2_000 {
            tip = commit(&store, &[tip], n);
            if n == 1_000 {
                middle = tip;
            }
        }
        assert_eq!(merge_bases(&store, tip, middle).unwrap(), vec![middle]);
    }

    #[test]
    fn an_absent_commit_is_reported() {
        let dir = TempDir::new();
        let store = dir.store();
        let other = TempDir::new();
        let absent = commit(&other.store(), &[], 0);
        let present = commit(&store, &[], 1);
        let error = merge_bases(&store, present, absent).unwrap_err();
        assert!(matches!(error, MergeBaseError::Missing(id) if id == absent));
        assert!(error.to_string().contains(&absent.to_string()));
    }

    #[test]
    fn a_failing_store_is_reported_with_its_message() {
        let dir = TempDir::new();
        let root = commit(&dir.store(), &[], 0);
        let store = FaultyStore::new(dir.store(), true, false);
        let error = merge_bases(&store, root, root).unwrap_err();
        assert!(matches!(error, MergeBaseError::Read(_)));
        assert!(error.to_string().contains(crate::testutil::DISK_MESSAGE));
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
                let expected: Vec<CommitId> = {
                    let mut bases: Vec<CommitId> = reference_bases(&shape, a, b)
                        .into_iter()
                        .map(|i| ids[i])
                        .collect();
                    bases.sort();
                    bases
                };
                assert_eq!(
                    found,
                    expected,
                    "seed {}, round {round}, {a} and {b}",
                    seed()
                );
                // Symmetry.
                assert_eq!(merge_bases(&store, ids[b], ids[a]).unwrap(), found);
                // The result is an antichain: no base is an ancestor of another one.
                for &x in &found {
                    for &y in &found {
                        if x != y {
                            let from_y = Walk::new(&store).ancestors(&[y]).unwrap();
                            assert!(!from_y.contains(&x), "seed {}, round {round}", seed());
                        }
                    }
                }
                // The tip is the base if one side is an ancestor of the other.
                if reference_ancestors(&shape, a).contains(&b) {
                    assert_eq!(found, vec![ids[b]]);
                }
            }
        }
    }
}
