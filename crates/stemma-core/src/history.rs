//! The ancestors of a commit in the order that `stemma log` shows them, as a pure function over
//! the object store, and the error that the walks over the history share ([`HistoryError`]).
//!
//! The order is topological: a commit comes before all of its parents. Where several commits are
//! ready at once, the one with the newest commit date comes first, and equal dates are ordered by
//! the greater ID, so that the order depends on the history alone. The date is self-reported (see
//! [`crate::object::Commit`]) and only breaks ties: it never decides whether a child comes before
//! its parent, so a wrong date changes the look of the list and not its correctness.
//!
//! The function keeps no cache and changes no format. It reads each reachable commit once. With
//! [`Parents::First`] it follows the first parent only and reads no more commits than `limit`
//! allows. With [`Parents::All`] the order needs the whole reachable set, because a later branch
//! may reach a commit again, so the walk reads all of it before it lists the first `limit`
//! commits.

use std::collections::{BinaryHeap, HashMap};
use std::error::Error;
use std::fmt;

use crate::object::{Commit, CommitId};
use crate::store::{ObjectStore, ReadError, StoreError};

/// Which parents of a commit [`ancestors`] follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parents {
    /// Every parent: the whole history that leads to the start.
    All,
    /// The first parent only, so that a merge hides the history it brought in.
    First,
}

/// Returns the commit `start` and its ancestors, children before parents, at most `limit` of them.
/// A `limit` of `Some(0)` gives an empty list; `None` lists every commit.
///
/// With [`Parents::First`], the result is the chain from `start` down to a root, or to the commit
/// where `limit` cuts it. The list of any `limit` is the beginning of the list without one.
///
/// # Errors
///
/// Returns [`HistoryError::Missing`] if the store lacks a commit that the walk reaches, and
/// [`HistoryError::Read`] if the store cannot return one intact.
pub fn ancestors(
    store: &dyn ObjectStore,
    start: CommitId,
    parents: Parents,
    limit: Option<usize>,
) -> Result<Vec<(CommitId, Commit)>, HistoryError> {
    match parents {
        Parents::First => first_parent_chain(store, start, limit),
        Parents::All => topological(store, start, limit),
    }
}

/// Reads the commit `id`. `child` is the commit that `id` was reached from, for the error: a
/// commit that is absent is a defect of the history if it is a parent, and a wrong start if it is
/// the start.
pub(crate) fn read_commit(
    store: &dyn ObjectStore,
    id: CommitId,
    child: Option<CommitId>,
) -> Result<Commit, HistoryError> {
    // A commit that a reachable commit names must exist, so `None` is an error here, while the
    // store returns it as a normal answer.
    store
        .get_commit(&id)?
        .ok_or(HistoryError::Missing { id, child })
}

fn first_parent_chain(
    store: &dyn ObjectStore,
    start: CommitId,
    limit: Option<usize>,
) -> Result<Vec<(CommitId, Commit)>, HistoryError> {
    let mut chain = Vec::new();
    let mut next = Some((start, None));
    while let Some((id, child)) = next {
        if limit.is_some_and(|limit| chain.len() >= limit) {
            break;
        }
        let commit = read_commit(store, id, child)?;
        next = commit.parents().first().map(|&parent| (parent, Some(id)));
        chain.push((id, commit));
    }
    Ok(chain)
}

fn topological(
    store: &dyn ObjectStore,
    start: CommitId,
    limit: Option<usize>,
) -> Result<Vec<(CommitId, Commit)>, HistoryError> {
    // The first pass reads every reachable commit once and counts, for each commit, the edges from
    // its children that are reachable too (a commit names each parent once, so an edge is a
    // child). A commit is ready when all of them are listed.
    let mut commits: HashMap<CommitId, Commit> = HashMap::new();
    let mut waiting: HashMap<CommitId, usize> = HashMap::new();
    let mut pending = vec![(start, None)];
    while let Some((id, child)) = pending.pop() {
        if commits.contains_key(&id) {
            continue;
        }
        let commit = read_commit(store, id, child)?;
        for &parent in commit.parents() {
            *waiting.entry(parent).or_insert(0) += 1;
            pending.push((parent, Some(id)));
        }
        commits.insert(id, commit);
    }

    // The start has no child in the set, so it is the first one ready.
    let mut ready = BinaryHeap::new();
    ready.push((commits[&start].timestamp(), start));
    let mut listed = Vec::new();
    while let Some((_, id)) = ready.pop() {
        if limit.is_some_and(|limit| listed.len() >= limit) {
            break;
        }
        let commit = commits
            .remove(&id)
            .expect("a ready commit is read and not yet listed");
        for parent in commit.parents() {
            let count = waiting.get_mut(parent).expect("every parent was counted");
            *count -= 1;
            if *count == 0 {
                ready.push((commits[parent].timestamp(), *parent));
            }
        }
        listed.push((id, commit));
    }
    Ok(listed)
}

/// Why a walk over the history fails: [`ancestors`] and [`merge_bases`](crate::merge_base).
#[derive(Debug)]
#[non_exhaustive]
pub enum HistoryError {
    /// The store does not hold a commit that the walk reached: the start, or a parent of a commit.
    Missing {
        /// The commit that is absent.
        id: CommitId,
        /// The commit that names it as a parent, or `None` if it is a start (one of the two tips of
        /// a merge base).
        child: Option<CommitId>,
    },
    /// The store could not return a commit. The message and the cause are those of the read error.
    Read(ReadError),
}

impl HistoryError {
    /// Returns the error of the store if the store could not return a commit: the storage failed,
    /// the object is damaged, or it has another kind. `None` for a commit that is absent, for one
    /// whose body is not in canonical form, and for one whose ID does not match its content.
    pub fn store_error(&self) -> Option<&StoreError> {
        match self {
            Self::Read(source) => source.store_error(),
            Self::Missing { .. } => None,
        }
    }
}

impl fmt::Display for HistoryError {
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

impl Error for HistoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Missing { .. } => None,
            // Transparent: the message is that of the read error, so its cause is the cause here.
            Self::Read(source) => source.source(),
        }
    }
}

impl From<ReadError> for HistoryError {
    fn from(source: ReadError) -> Self {
        Self::Read(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::sample_change;
    use crate::failure::{Classify, FailureKind};
    use crate::object::Tree;
    use crate::testutil::{FaultyStore, TempDir};
    use crate::Actor;

    /// Stores a commit with the given parents; `time` is its date and makes the commit unique.
    fn commit(store: &dyn ObjectStore, parents: &[CommitId], time: i64) -> CommitId {
        commit_named(store, parents, time, &format!("commit {time}"))
    }

    /// Like [`commit`], with a message of its own, so that two commits of one date differ.
    fn commit_named(
        store: &dyn ObjectStore,
        parents: &[CommitId],
        time: i64,
        message: &str,
    ) -> CommitId {
        let tree = store.put_tree(&Tree::try_new(vec![]).unwrap()).unwrap();
        let commit = Commit::try_new(
            tree,
            parents.to_vec(),
            sample_change(),
            Actor::human("daniel").unwrap(),
            time,
            message,
        )
        .unwrap();
        store.put_commit(&commit).unwrap()
    }

    fn ids(listed: Vec<(CommitId, Commit)>) -> Vec<CommitId> {
        listed.into_iter().map(|(id, _)| id).collect()
    }

    #[test]
    fn a_linear_history_is_listed_from_the_start_to_the_root() {
        let dir = TempDir::new();
        let store = dir.store();
        let root = commit(&store, &[], 0);
        let middle = commit(&store, &[root], 1);
        let tip = commit(&store, &[middle], 2);
        let listed = ancestors(&store, tip, Parents::All, None).unwrap();
        assert_eq!(listed[0].1.message(), "commit 2");
        assert_eq!(ids(listed), vec![tip, middle, root]);
    }

    #[test]
    fn a_merge_is_listed_before_its_parents_and_both_before_the_fork() {
        let dir = TempDir::new();
        let store = dir.store();
        let fork = commit(&store, &[], 0);
        let left = commit(&store, &[fork], 1);
        let right = commit(&store, &[fork], 2);
        let merge = commit(&store, &[left, right], 3);
        let listed = ids(ancestors(&store, merge, Parents::All, None).unwrap());
        // The newer parent first; the fork waits for both.
        assert_eq!(listed, vec![merge, right, left, fork]);
    }

    #[test]
    fn a_parent_waits_for_all_of_its_children_whatever_their_dates() {
        let dir = TempDir::new();
        let store = dir.store();
        // The fork has a date after both branches, which must not bring it forward.
        let fork = commit(&store, &[], 100);
        let left = commit(&store, &[fork], 1);
        let right = commit(&store, &[fork], 2);
        let merge = commit(&store, &[left, right], 3);
        let listed = ids(ancestors(&store, merge, Parents::All, None).unwrap());
        assert_eq!(listed, vec![merge, right, left, fork]);
    }

    #[test]
    fn equal_dates_are_ordered_by_the_id_and_the_order_does_not_depend_on_the_parent_order() {
        let dir = TempDir::new();
        let store = dir.store();
        let fork = commit(&store, &[], 0);
        let one = commit_named(&store, &[fork], 5, "one");
        let two = commit_named(&store, &[fork], 5, "two");
        let (high, low) = if one > two { (one, two) } else { (two, one) };
        let a = commit(&store, &[one, two], 9);
        let b = commit(&store, &[two, one], 9);
        for merge in [a, b] {
            let listed = ids(ancestors(&store, merge, Parents::All, None).unwrap());
            assert_eq!(listed, vec![merge, high, low, fork]);
        }
    }

    #[test]
    fn the_limit_stops_the_list_after_that_many_commits() {
        let dir = TempDir::new();
        let store = dir.store();
        let root = commit(&store, &[], 0);
        let middle = commit(&store, &[root], 1);
        let tip = commit(&store, &[middle], 2);
        for parents in [Parents::All, Parents::First] {
            assert_eq!(
                ids(ancestors(&store, tip, parents, Some(2)).unwrap()),
                vec![tip, middle]
            );
            assert_eq!(
                ids(ancestors(&store, tip, parents, Some(10)).unwrap()),
                vec![tip, middle, root]
            );
        }
    }

    #[test]
    fn first_parent_follows_the_first_parent_of_a_merge_only() {
        let dir = TempDir::new();
        let store = dir.store();
        let fork = commit(&store, &[], 0);
        let left = commit(&store, &[fork], 1);
        let right = commit(&store, &[fork], 2);
        let merge = commit(&store, &[left, right], 3);
        assert_eq!(
            ids(ancestors(&store, merge, Parents::First, None).unwrap()),
            vec![merge, left, fork]
        );
    }

    #[test]
    fn a_missing_start_and_a_missing_parent_are_told_apart() {
        let dir = TempDir::new();
        let store = dir.store();
        let other = TempDir::new();
        let elsewhere = other.store();
        let lost = commit(&elsewhere, &[], 0);
        let child = commit(&elsewhere, &[lost], 1);
        // Only the child is in `store`.
        store.put_tree(&Tree::try_new(vec![]).unwrap()).unwrap();
        let kept = elsewhere.get_commit(&child).unwrap().unwrap();
        store.put_commit(&kept).unwrap();

        for parents in [Parents::All, Parents::First] {
            assert!(matches!(
                ancestors(&store, lost, parents, None),
                Err(HistoryError::Missing {
                    id,
                    child: None
                }) if id == lost
            ));
            assert!(matches!(
                ancestors(&store, child, parents, None),
                Err(HistoryError::Missing {
                    id,
                    child: Some(from)
                }) if id == lost && from == child
            ));
        }
    }

    #[test]
    fn a_skip_edge_does_not_bring_a_parent_forward() {
        let dir = TempDir::new();
        let store = dir.store();
        // `root` is a parent of `tip` and of `a`, and has the newest date of all.
        let root = commit(&store, &[], 100);
        let a = commit(&store, &[root], 1);
        let tip = commit(&store, &[a, root], 2);
        let listed = ids(ancestors(&store, tip, Parents::All, None).unwrap());
        assert_eq!(listed, vec![tip, a, root]);
    }

    #[test]
    fn a_criss_cross_lists_every_commit_once_and_after_its_children() {
        let dir = TempDir::new();
        let store = dir.store();
        let root = commit(&store, &[], 0);
        let a = commit(&store, &[root], 1);
        let b = commit(&store, &[root], 2);
        let m1 = commit_named(&store, &[a, b], 3, "m1");
        let m2 = commit_named(&store, &[b, a], 3, "m2");
        let tip = commit(&store, &[m1, m2], 9);
        let listed = ids(ancestors(&store, tip, Parents::All, None).unwrap());
        assert_eq!(listed.len(), 6);
        let position = |id: CommitId| listed.iter().position(|&listed| listed == id).unwrap();
        assert_eq!(position(tip), 0);
        assert_eq!(position(root), 5);
        for (child, parent) in [(m1, a), (m1, b), (m2, a), (m2, b), (a, root), (b, root)] {
            assert!(position(child) < position(parent));
        }
        let (high, low) = if m1 > m2 { (m1, m2) } else { (m2, m1) };
        assert_eq!(&listed[1..3], &[high, low]);
        assert_eq!(&listed[3..5], &[b, a]);
    }

    #[test]
    fn the_limit_cuts_a_merge_and_every_limit_is_the_beginning_of_the_full_list() {
        let dir = TempDir::new();
        let store = dir.store();
        let fork = commit(&store, &[], 0);
        let left = commit(&store, &[fork], 1);
        let right = commit(&store, &[fork], 2);
        let merge = commit(&store, &[left, right], 3);
        assert_eq!(
            ids(ancestors(&store, merge, Parents::All, Some(2)).unwrap()),
            vec![merge, right]
        );
        assert_eq!(
            ids(ancestors(&store, merge, Parents::First, Some(2)).unwrap()),
            vec![merge, left]
        );
        for parents in [Parents::All, Parents::First] {
            let full = ids(ancestors(&store, merge, parents, None).unwrap());
            for limit in 0..=full.len() + 1 {
                let limited = ids(ancestors(&store, merge, parents, Some(limit)).unwrap());
                assert_eq!(limited[..], full[..limit.min(full.len())]);
            }
        }
    }

    #[test]
    fn a_parent_of_the_wrong_kind_is_damage() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = store.put_tree(&Tree::try_new(vec![]).unwrap()).unwrap();
        // A commit that names a tree as its parent.
        let parent = CommitId::new(tree.object_id());
        let child = commit(&store, &[parent], 1);
        for parents in [Parents::All, Parents::First] {
            let error = ancestors(&store, child, parents, None).unwrap_err();
            assert!(matches!(error, HistoryError::Read(_)), "{error}");
            assert_eq!(error.kind(), FailureKind::Damaged, "{error}");
        }
    }

    #[test]
    fn a_failing_disk_is_an_error_of_the_store_and_not_damage() {
        let dir = TempDir::new();
        let root = commit(&dir.store(), &[], 0);
        let store = FaultyStore::new(dir.store(), true, false);
        for parents in [Parents::All, Parents::First] {
            let error = ancestors(&store, root, parents, None).unwrap_err();
            assert!(error.store_error().is_some());
            assert_eq!(error.kind(), FailureKind::Other);
        }
    }
}
