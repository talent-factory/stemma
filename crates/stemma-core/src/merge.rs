//! The merge of trees (ADR-004, level 2): the sum of the trees of two commits and their merge
//! bases, merged path by path into a sum of trees that is a plain tree if nothing conflicts.
//!
//! [`tree_sum`] builds the sum that a merge starts from, [`merge_trees`] merges any such sum, and
//! [`merge_commits`] does both. The result is a [`Merge<TreeId>`](Merge) in the form that a
//! `commit2` holds (ADR-005, [`crate::conflict`]): one tree if the merge is clean, and otherwise
//! three or more trees in which every path that could not be decided holds the competing values.
//!
//! # The sum
//!
//! One merge base `B` of the commits `O` (ours) and `T` (theirs) gives `O + (T - B)`. Several
//! best common ancestors, as after a criss-cross merge, are not merged into a virtual commit, and
//! no recursive merge runs: they are combined as a flat sum (Jujutsu does the same). The sum of a
//! set of bases is the first base, plus the change to each further base from its best common
//! ancestors with any one of the earlier bases; those ancestors are summed by the same rule.
//! Terms that two summands have in common cancel. The sum may therefore have more than three sides. The bases come sorted by ID from
//! [`merge_bases`], which fixes the sum: it depends on the histories alone. With three or more
//! bases the choice of the first one is arbitrary, but it is the same every time. A merge of
//! histories that share nothing has the empty tree as its base.
//!
//! # The merge of a name
//!
//! Every name in the trees of the sum has one value per term: absent, a file with a mode, a
//! symbolic link, or a directory. The values are merged in this order, and the first rule that
//! applies decides:
//!
//! 1. **Cancelling.** Equal added and removed terms cancel ([`Merge::simplify`]). A path that one
//!    side changed and the other did not is the changed value; for a directory it is decided by
//!    the ID of the tree, without reading it.
//! 2. **The same change on both sides.** If all sides are equal, that value is the result, for
//!    content and for mode alike. There is no setting for this in phase 3.
//! 3. **Directories.** If every term is a directory or absent, the sum of the trees (an absent
//!    term counts as the empty tree) is merged by the same rules, one level down. A directory
//!    that one side deleted and the other side changed is therefore not one conflict, but a
//!    conflict at each path that the changed side touched. A directory that merges to nothing is
//!    absent.
//! 4. **Files.** If every term is a file, the mode and the content are merged independently. The
//!    mode is decided like any value: by cancelling, or by agreement. The content is merged by
//!    line ([`merge_lines`]). A path where the mode or the content is not decided holds, in every
//!    term, the value that was decided, and the competing values for the rest.
//! 5. **Symbolic links.** The target is one value and is never merged by line.
//! 6. **Everything else is a conflict of the path.** That covers a deleted file against a changed
//!    one (renames are not detected, so a rename is a delete and an add), two sides that add the
//!    same path with different content (the base is absent, so there is nothing to merge by line),
//!    and different types, such as a file against a directory or a symbolic link.
//!
//! # The merge of lines
//!
//! [`merge_lines`] compares every term with the first removed term of the simplified sum, the base,
//! by the same function as `diff` ([`diff_byte_lines`], with the indent heuristic), so the merge
//! uses the edit script that `diff` shows for the base and each side. The lines of the base that
//! all comparisons match are the fixed lines and divide the file; the text between two of them is
//! a piece with one part per term, and the parts are a sum of their own that is decided by
//! cancelling or by agreement. Changes of two sides that touch each other, even if they share no
//! line, fall into one piece and conflict, except when both sides made the same change. A binary
//! file and a file that is not UTF-8 are never merged by line. A file is merged only if all of its
//! pieces are decided. Otherwise the conflict of the path holds the whole
//! blobs: markers exist only as the text of [`marker_text`](crate::conflict::marker_text), on
//! disk.
//!
//! # Cost
//!
//! For a sum of `k` terms and `n` entries in all directories that differ, the merge reads `k`
//! trees per such directory, looks every name up in each by binary search, and writes `2s - 1`
//! trees for `s` sides, so the cost is `O(n·k)` up to log factors; a tree that cancels is not read.
//! The merge of lines costs one diff of `O((N+M)·D)` per term against the base, plus a linear pass
//! and the `simplify` of each piece, which is quadratic in `k`. Finding the bases is the cost of
//! [`merge_bases`], a walk of the whole ancestry. With several bases that walk runs once more for
//! every further base, and again in every level of the recursion over their common ancestors, so
//! both the depth and the number of calls grow with the history in the worst case; criss-cross
//! lattices that are deeper than a few levels are not expected in practice.
//!
//! A tree or blob that is not read is not verified: an object that cancels, or that only one side
//! changed, enters the result by its ID, so a missing or damaged one is reported only by the
//! reader of the result.
//!
//! An operand may be a `commit` or a `commit2`: a `CommitId` names either kind (ADR-005,
//! STEMMA-179), and the sum of a `commit2` enters the sum of the merge as its list of trees.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use crate::conflict::Merge;
use crate::diff::{diff_byte_lines, is_binary, Edit};
use crate::history::{read_commit, HistoryError};
use crate::merge_base::merge_bases;
use crate::object::{
    Blob, BlobId, CommitId, EntryMode, FileMode, Kind, ObjectId, Tree, TreeEntry, TreeError, TreeId,
};
use crate::store::{ObjectStore, ReadError, StoreError};

/// Why a merge failed. A conflict is no failure: it is part of the result.
#[derive(Debug)]
#[non_exhaustive]
pub enum MergeError {
    /// A commit of the history could not be read, or the store does not have it.
    History(HistoryError),
    /// A tree could not be read: the storage failed, the object is damaged, or its body is not in
    /// canonical form.
    Tree(ReadError),
    /// A blob could not be read, or a tree or a blob could not be written.
    Store(StoreError),
    /// A tree or a blob that a commit or a tree names is not in the store.
    Missing {
        /// The ID of the object.
        id: ObjectId,
        /// The kind that the tree names it as.
        kind: Kind,
    },
    /// The entries of a merged tree are not valid. The names come from trees that were valid, so
    /// this is a defect, and it is reported and not hidden.
    Entries(TreeError),
}

impl MergeError {
    /// Returns the error of the store if the store could not return or take an object: the
    /// storage failed, the object is damaged, or it has another kind. `None` for the others.
    pub fn store_error(&self) -> Option<&StoreError> {
        match self {
            Self::History(source) => source.store_error(),
            Self::Tree(source) => source.store_error(),
            Self::Store(source) => Some(source),
            Self::Missing { .. } | Self::Entries(_) => None,
        }
    }
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::History(source) => write!(f, "{source}"),
            Self::Tree(source) => write!(f, "{source}"),
            Self::Store(source) => write!(f, "{source}"),
            Self::Missing { id, kind } => {
                write!(f, "the {} {id} is not in the object store", kind.name())
            }
            Self::Entries(source) => write!(f, "the merged tree is not valid: {source}"),
        }
    }
}

impl Error for MergeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message includes that of the cause, so the source is the cause of the cause (see the
        // crate documentation).
        match self {
            Self::History(source) => source.source(),
            Self::Tree(source) => source.source(),
            Self::Store(source) => source.source(),
            Self::Missing { .. } => None,
            Self::Entries(source) => source.source(),
        }
    }
}

impl From<HistoryError> for MergeError {
    fn from(source: HistoryError) -> Self {
        Self::History(source)
    }
}

impl From<ReadError> for MergeError {
    fn from(source: ReadError) -> Self {
        Self::Tree(source)
    }
}

impl From<StoreError> for MergeError {
    fn from(source: StoreError) -> Self {
        Self::Store(source)
    }
}

/// Merges the trees of two commits over their merge bases and returns the sum of trees: one tree
/// if nothing conflicts, and otherwise the trees of a `commit2`. See [`tree_sum`] and
/// [`merge_trees`].
///
/// # Errors
///
/// Returns the errors of [`tree_sum`] and [`merge_trees`].
pub fn merge_commits(
    store: &dyn ObjectStore,
    ours: CommitId,
    theirs: CommitId,
) -> Result<Merge<TreeId>, MergeError> {
    merge_trees(store, &tree_sum(store, ours, theirs)?)
}

/// Returns the sum of trees that the merge of `ours` and `theirs` starts from: `ours + theirs`
/// minus the sum of their merge bases (see the module documentation). The sum is not simplified
/// and not merged. If one commit is an ancestor of the other, it simplifies to the tree of the
/// descendant.
///
/// # Errors
///
/// Returns [`MergeError::History`] if the store lacks a commit that the walk reaches or cannot
/// return one intact, and [`MergeError::Store`] or [`MergeError::Entries`] if the empty tree cannot
/// be written for histories without a common ancestor.
pub fn tree_sum(
    store: &dyn ObjectStore,
    ours: CommitId,
    theirs: CommitId,
) -> Result<Merge<TreeId>, MergeError> {
    let base = base_sum(store, &merge_bases(store, ours, theirs)?)?;
    let ours_sum = read_commit(store, ours, None)?.tree_sum();
    let theirs_sum = read_commit(store, theirs, None)?.tree_sum();
    // `ours + theirs - base`: the removed terms of the base are added, its added terms removed.
    let mut adds: Vec<TreeId> = ours_sum.adds().copied().collect();
    adds.extend(theirs_sum.adds().copied());
    adds.extend(base.removes().copied());
    let mut base_adds: Vec<TreeId> = ours_sum.removes().copied().collect();
    base_adds.extend(theirs_sum.removes().copied());
    base_adds.extend(base.adds().copied());
    Ok(Merge::from_parts(adds, base_adds))
}

/// The sum that stands for the common ancestors: the empty tree if there are none, the tree of
/// the only one, and for several the first plus the change to each further one from its best
/// common ancestors with any one of the earlier ones.
fn base_sum(store: &dyn ObjectStore, bases: &[CommitId]) -> Result<Merge<TreeId>, MergeError> {
    let Some((&first, rest)) = bases.split_first() else {
        return Ok(Merge::resolved(store.put_tree(&Tree::try_new(vec![])?)?));
    };
    let mut sum = read_commit(store, first, None)?.tree_sum();
    let mut done = vec![first];
    for &other in rest {
        let common = base_sum(store, &common_ancestors(store, &done, other)?)?;
        let other_sum = read_commit(store, other, None)?.tree_sum();
        // `sum + other - common`.
        let mut adds: Vec<TreeId> = sum.adds().copied().collect();
        adds.extend(other_sum.adds().copied());
        adds.extend(common.removes().copied());
        let mut removes: Vec<TreeId> = sum.removes().copied().collect();
        removes.extend(other_sum.removes().copied());
        removes.extend(common.adds().copied());
        sum = Merge::from_parts(adds, removes);
        done.push(other);
    }
    Ok(sum)
}

/// The best common ancestors of a set of commits and one more commit: the commits that are an
/// ancestor of the commit and of at least one of the set, and of no other such commit. Sorted by
/// ID. For a set of one commit these are its merge bases with the commit.
fn common_ancestors(
    store: &dyn ObjectStore,
    set: &[CommitId],
    commit: CommitId,
) -> Result<Vec<CommitId>, MergeError> {
    let mut candidates: Vec<CommitId> = Vec::new();
    for &member in set {
        candidates.extend(merge_bases(store, member, commit)?);
    }
    candidates.sort();
    candidates.dedup();
    let mut best = Vec::new();
    for &candidate in &candidates {
        // A candidate that is an ancestor of another one is not a best ancestor.
        let mut shadowed = false;
        for &other in &candidates {
            if other != candidate && merge_bases(store, candidate, other)? == [candidate] {
                shadowed = true;
                break;
            }
        }
        if !shadowed {
            best.push(candidate);
        }
    }
    Ok(best)
}

impl From<TreeError> for MergeError {
    fn from(source: TreeError) -> Self {
        Self::Entries(source)
    }
}

/// Merges a sum of trees and returns the sum of trees of the result, simplified. The new blobs
/// and trees are written to the store. The result is resolved (one tree) if no path conflicts.
///
/// # Errors
///
/// Returns [`MergeError::Tree`], [`MergeError::Store`], or [`MergeError::Missing`] if an object
/// cannot be read or written, and [`MergeError::Entries`] for an entry that is not valid.
pub fn merge_trees(
    store: &dyn ObjectStore,
    sum: &Merge<TreeId>,
) -> Result<Merge<TreeId>, MergeError> {
    Merger { store }.tree(sum)
}

/// One value of a name in one term of the sum. A name that a term does not have is `None` where
/// this is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) mode: EntryMode,
    pub(crate) id: ObjectId,
}

struct Merger<'a> {
    store: &'a dyn ObjectStore,
}

impl Merger<'_> {
    fn empty_tree(&self) -> Result<TreeId, MergeError> {
        Ok(self.store.put_tree(&Tree::try_new(vec![])?)?)
    }

    /// Merges a sum of trees into a sum of trees.
    fn tree(&self, sum: &Merge<TreeId>) -> Result<Merge<TreeId>, MergeError> {
        let sum = sum.simplify();
        if sum.is_resolved() {
            return Ok(sum);
        }
        if let Some(agreed) = agreed(&sum) {
            return Ok(Merge::resolved(agreed));
        }
        let trees: Vec<Tree> = sum
            .terms()
            .iter()
            .map(|id| {
                self.store.get_tree(id)?.ok_or(MergeError::Missing {
                    id: id.object_id(),
                    kind: Kind::Tree,
                })
            })
            .collect::<Result<_, _>>()?;
        let names: BTreeSet<&str> = trees
            .iter()
            .flat_map(|tree| tree.entries().iter().map(TreeEntry::name))
            .collect();
        let empty = self.empty_tree()?;

        let mut merged: Vec<(&str, Merge<Option<Slot>>)> = Vec::new();
        for name in names {
            let slots = Merge::try_from_terms(
                trees
                    .iter()
                    .map(|tree| {
                        tree.entries()
                            .binary_search_by(|entry| entry.name().cmp(name))
                            .ok()
                            .map(|at| {
                                let entry = &tree.entries()[at];
                                Slot {
                                    mode: entry.mode(),
                                    id: entry.id(),
                                }
                            })
                    })
                    .collect(),
            )
            .expect("the trees are the terms of a sum");
            merged.push((name, self.entry(&slots, empty)?));
        }

        // Every path has the number of sides of the widest one; a narrower one is padded.
        let sides = merged
            .iter()
            .map(|(_, slots)| slots.sides())
            .max()
            .unwrap_or(1);
        let padded: Vec<(&str, Merge<Option<Slot>>)> = merged
            .into_iter()
            .map(|(name, slots)| (name, slots.padded(sides)))
            .collect();
        let mut terms = Vec::with_capacity(2 * sides - 1);
        for term in 0..2 * sides - 1 {
            let mut entries = Vec::new();
            for (name, slots) in &padded {
                if let Some(slot) = slots.terms()[term] {
                    if slot.mode == EntryMode::Directory && TreeId::new(slot.id) == empty {
                        continue;
                    }
                    entries.push(entry_of(name, slot)?);
                }
            }
            terms.push(self.store.put_tree(&Tree::try_new(entries)?)?);
        }
        Ok(Merge::try_from_terms(terms)
            .expect("an odd number of terms")
            .simplify())
    }

    /// Merges the values of one name.
    fn entry(
        &self,
        slots: &Merge<Option<Slot>>,
        empty: TreeId,
    ) -> Result<Merge<Option<Slot>>, MergeError> {
        let slots = slots.simplify();
        if slots.is_resolved() {
            return Ok(slots);
        }
        if let Some(agreed) = agreed(&slots) {
            return Ok(Merge::resolved(agreed));
        }
        let all = |wanted: &dyn Fn(&Slot) -> bool| {
            slots
                .terms()
                .iter()
                .all(|term| term.as_ref().is_none_or(wanted))
        };
        let present = |wanted: &dyn Fn(&Slot) -> bool| {
            slots
                .terms()
                .iter()
                .all(|term| term.as_ref().is_some_and(wanted))
        };

        if all(&|slot| slot.mode == EntryMode::Directory) {
            let trees = slots.map(|term| term.map_or(empty, |slot| TreeId::new(slot.id)));
            let merged = self.tree(&trees)?;
            return Ok(merged.map(|&tree| {
                (tree != empty).then_some(Slot {
                    mode: EntryMode::Directory,
                    id: tree.object_id(),
                })
            }));
        }
        if present(&|slot| matches!(slot.mode, EntryMode::File | EntryMode::Executable)) {
            return self.file(&slots);
        }
        // A symbolic link is one value, and a mix of types or of absent and present is a conflict
        // of the path. Both stay as they are.
        Ok(slots)
    }

    /// Merges the mode and the content of a file independently.
    fn file(&self, slots: &Merge<Option<Slot>>) -> Result<Merge<Option<Slot>>, MergeError> {
        let present = slots.map(|term| term.expect("every term is a file"));
        let modes = present.map(|slot| slot.mode);
        let contents = present.map(|slot| slot.id);
        let mode = decided(&modes);
        let content = self.content(&contents)?;
        let terms: Vec<Option<Slot>> = present
            .terms()
            .iter()
            .map(|slot| {
                Some(Slot {
                    mode: mode.unwrap_or(slot.mode),
                    id: content.unwrap_or(slot.id),
                })
            })
            .collect();
        // Terms that differed in one part only can now be equal and cancel.
        Ok(Merge::try_from_terms(terms)
            .expect("the terms of a sum")
            .simplify())
    }

    /// The content that the sum of blobs decides, or `None` if it is a conflict.
    fn content(&self, contents: &Merge<ObjectId>) -> Result<Option<ObjectId>, MergeError> {
        let contents = contents.simplify();
        if let Some(value) = decided(&contents) {
            return Ok(Some(value));
        }
        let blobs: Vec<Vec<u8>> = contents
            .terms()
            .iter()
            .map(|id| self.blob(id))
            .collect::<Result<_, _>>()?;
        let sum = Merge::try_from_terms(blobs.iter().map(Vec::as_slice).collect())
            .expect("the terms of a sum");
        let Some(merged) = merge_lines(&sum) else {
            return Ok(None);
        };
        Ok(Some(self.store.put_blob(&Blob::new(merged))?.object_id()))
    }

    fn blob(&self, id: &ObjectId) -> Result<Vec<u8>, MergeError> {
        self.store
            .get_expecting(id, Kind::Blob)?
            .map(crate::store::RawObject::into_body)
            .ok_or(MergeError::Missing {
                id: *id,
                kind: Kind::Blob,
            })
    }
}

/// The tree entry for a value of a name.
fn entry_of(name: &str, slot: Slot) -> Result<TreeEntry, TreeError> {
    match FileMode::from_entry_mode(slot.mode) {
        Some(mode) => TreeEntry::file(name, mode, BlobId::new(slot.id)),
        None => TreeEntry::directory(name, TreeId::new(slot.id)),
    }
}

/// The value that all sides of the sum have, if they are the same.
fn agreed<T: Clone + Eq>(sum: &Merge<T>) -> Option<T> {
    let mut adds = sum.adds();
    let first = adds.next()?;
    adds.all(|add| add == first).then(|| first.clone())
}

/// The value that a sum decides: what is left after cancelling, or what the sides agree on.
fn decided<T: Clone + Eq>(sum: &Merge<T>) -> Option<T> {
    sum.resolution().or_else(|| agreed(sum))
}

/// Merges the lines of a file over a sum of contents and returns the text, or `None` if the file
/// conflicts (see the module documentation): a binary file, a file that is not UTF-8, or a place
/// where the terms do not cancel and the sides do not agree.
///
/// A sum of one term is that text, if it is text. Otherwise the first removed term, the base, is
/// the reference of whatever sum is passed. The merge of a file simplifies the sum first, so that the
/// reference is the first base that did not cancel. Each other term is compared with it by the same function as `diff`
/// ([`diff_byte_lines`]). The lines of the base that every comparison matches are the fixed lines,
/// and the text between two of them is a piece with one part per term. The parts of a piece are a
/// sum of their own and are decided like any sum: cancelling, or all sides equal. Changes of two
/// sides that touch or overlap fall into one piece, where they are a conflict unless they are the
/// same change.
pub fn merge_lines(sum: &Merge<&[u8]>) -> Option<Vec<u8>> {
    if sum
        .terms()
        .iter()
        .any(|text| is_binary(text) || std::str::from_utf8(text).is_err())
    {
        return None;
    }
    if let [only] = sum.terms() {
        return Some(only.to_vec());
    }
    let texts: Vec<Vec<&[u8]>> = sum.terms().iter().map(|text| lines(text)).collect();
    const REFERENCE: usize = 1;
    let reference = &texts[REFERENCE];
    // `matched[term][line of the reference]` is the line of the term that equals it, if the
    // comparison pairs them. The reference matches itself.
    let matched: Vec<Vec<Option<usize>>> = texts
        .iter()
        .enumerate()
        .map(|(term, text)| {
            if term == REFERENCE {
                return (0..reference.len()).map(Some).collect();
            }
            let mut matched = vec![None; reference.len()];
            for edit in diff_byte_lines(reference, text) {
                if let Edit::Equal { old, new } = edit {
                    matched[old] = Some(new);
                }
            }
            matched
        })
        .collect();

    let mut merged: Vec<u8> = Vec::new();
    // The first line of each term that no hunk has taken yet.
    let mut taken = vec![0; texts.len()];
    let fixed_lines = (0..reference.len())
        .filter(|&line| matched.iter().all(|term| term[line].is_some()))
        .map(Some)
        .chain(std::iter::once(None));
    for fixed_line in fixed_lines {
        // The piece ends where the fixed line is in each term, or at the end of the term.
        let ends: Vec<usize> = texts
            .iter()
            .zip(&matched)
            .map(|(text, term)| {
                fixed_line.map_or(text.len(), |line| term[line].expect("a fixed line"))
            })
            .collect();
        let pieces: Vec<Vec<&[u8]>> = texts
            .iter()
            .zip(taken.iter().zip(&ends))
            .map(|(text, (&from, &to))| text[from..to].to_vec())
            .collect();
        let piece = Merge::try_from_terms(pieces).expect("the terms of a sum");
        decided(&piece)?
            .iter()
            .for_each(|line| merged.extend_from_slice(line));
        if let Some(line) = fixed_line {
            merged.extend_from_slice(reference[line]);
            for (taken, end) in taken.iter_mut().zip(&ends) {
                *taken = end + 1;
            }
        }
    }
    Some(merged)
}

/// The lines of a text with their line ends. A last line without one is a line of its own, which
/// differs from the same line with one.
fn lines(text: &[u8]) -> Vec<&[u8]> {
    text.split_inclusive(|byte| *byte == b'\n').collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::change::sample_change;
    use crate::object::{Commit, Tree};
    use crate::properties::{cases, seed, Rng};
    use crate::testutil::{FaultyStore, TempDir};
    use crate::Actor;

    /// Builds the tree for paths with contents. A content that starts with `x:` is an executable
    /// file, one that starts with `l:` is a symbolic link with that target, and any other is a
    /// regular file. A path with a `/` is in a directory.
    fn build(store: &dyn ObjectStore, files: &[(&str, &str)]) -> TreeId {
        let mut here: Vec<TreeEntry> = Vec::new();
        let mut below: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
        for &(path, content) in files {
            match path.split_once('/') {
                Some((directory, rest)) => {
                    below.entry(directory).or_default().push((rest, content))
                }
                None => {
                    let (mode, text) = match content {
                        c if c.starts_with("x:") => (FileMode::Executable, &c[2..]),
                        c if c.starts_with("l:") => (FileMode::Symlink, &c[2..]),
                        c => (FileMode::File, c),
                    };
                    let blob = store.put_blob(&Blob::new(text)).unwrap();
                    here.push(TreeEntry::file(path, mode, blob).unwrap());
                }
            }
        }
        for (directory, files) in below {
            here.push(TreeEntry::directory(directory, build(store, &files)).unwrap());
        }
        store.put_tree(&Tree::try_new(here).unwrap()).unwrap()
    }

    /// The files of a tree as `path -> mode:content`, directories flattened into paths.
    fn flat(store: &dyn ObjectStore, tree: TreeId) -> BTreeMap<String, String> {
        let mut files = BTreeMap::new();
        let tree = store.get_tree(&tree).unwrap().unwrap();
        for entry in tree.entries() {
            if entry.mode() == EntryMode::Directory {
                for (path, value) in flat(store, TreeId::new(entry.id())) {
                    files.insert(format!("{}/{path}", entry.name()), value);
                }
            } else {
                let body = store.get(&entry.id()).unwrap().unwrap().into_body();
                let prefix = match entry.mode() {
                    EntryMode::Executable => "x:",
                    EntryMode::Symlink => "l:",
                    _ => "",
                };
                files.insert(
                    entry.name().to_string(),
                    format!("{prefix}{}", String::from_utf8(body).unwrap()),
                );
            }
        }
        files
    }

    fn expected(files: &[(&str, &str)]) -> BTreeMap<String, String> {
        files
            .iter()
            .map(|&(path, content)| (path.to_string(), content.to_string()))
            .collect()
    }

    /// Merges `ours` and `theirs` over `base` and returns the files of every tree of the result.
    fn merge3(
        base: &[(&str, &str)],
        ours: &[(&str, &str)],
        theirs: &[(&str, &str)],
    ) -> Vec<BTreeMap<String, String>> {
        let dir = TempDir::new();
        let store = dir.store();
        let sum = Merge::try_from_terms(vec![
            build(&store, ours),
            build(&store, base),
            build(&store, theirs),
        ])
        .unwrap();
        let merged = merge_trees(&store, &sum).unwrap();
        merged
            .terms()
            .iter()
            .map(|&tree| flat(&store, tree))
            .collect()
    }

    /// The files of a merge that must be clean.
    fn clean(
        base: &[(&str, &str)],
        ours: &[(&str, &str)],
        theirs: &[(&str, &str)],
    ) -> BTreeMap<String, String> {
        let mut trees = merge3(base, ours, theirs);
        assert_eq!(trees.len(), 1, "a conflict: {trees:?}");
        trees.remove(0)
    }

    fn text(lines: &[&str]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    fn lines_of(base: &str, ours: &str, theirs: &str) -> Option<String> {
        let sum = Merge::try_from_terms(vec![ours.as_bytes(), base.as_bytes(), theirs.as_bytes()])
            .unwrap();
        merge_lines(&sum).map(|bytes| String::from_utf8(bytes).unwrap())
    }

    #[test]
    fn changes_in_different_places_of_a_file_are_both_taken() {
        let base = text(&["a", "b", "c", "d", "e"]);
        let ours = text(&["A", "b", "c", "d", "e"]);
        let theirs = text(&["a", "b", "c", "d", "E"]);
        assert_eq!(
            lines_of(&base, &ours, &theirs),
            Some(text(&["A", "b", "c", "d", "E"]))
        );
        assert_eq!(
            lines_of(&base, &theirs, &ours),
            Some(text(&["A", "b", "c", "d", "E"]))
        );
    }

    #[test]
    fn changes_that_touch_each_other_are_a_conflict_even_without_a_shared_line() {
        let base = text(&["a", "b", "c", "d"]);
        let ours = text(&["a", "B", "c", "d"]);
        let theirs = text(&["a", "b", "C", "d"]);
        assert_eq!(lines_of(&base, &ours, &theirs), None);
        // One line between the changes is enough to keep them apart.
        let theirs = text(&["a", "b", "c", "D"]);
        assert_eq!(
            lines_of(&base, &ours, &theirs),
            Some(text(&["a", "B", "c", "D"]))
        );
    }

    #[test]
    fn the_same_change_on_both_sides_is_taken_once() {
        let base = text(&["a", "b", "c", "d", "e"]);
        let ours = text(&["a", "X", "c", "d", "E"]);
        let theirs = text(&["a", "X", "c", "d", "e"]);
        assert_eq!(lines_of(&base, &ours, &theirs), Some(ours.clone()));
        assert_eq!(lines_of(&base, &ours, &ours), Some(ours));
    }

    #[test]
    fn different_changes_of_the_same_line_are_a_conflict() {
        let base = text(&["a", "b", "c"]);
        let ours = text(&["a", "one", "c"]);
        let theirs = text(&["a", "two", "c"]);
        assert_eq!(lines_of(&base, &ours, &theirs), None);
    }

    #[test]
    fn insertions_at_the_same_place_are_a_conflict_unless_they_are_equal() {
        let base = text(&["a", "b"]);
        assert_eq!(
            lines_of(&base, &text(&["a", "x", "b"]), &text(&["a", "y", "b"])),
            None
        );
        assert_eq!(
            lines_of(&base, &text(&["a", "x", "b"]), &text(&["a", "x", "b"])),
            Some(text(&["a", "x", "b"]))
        );
    }

    #[test]
    fn a_deletion_and_an_insertion_elsewhere_both_apply() {
        let base = text(&["a", "b", "c", "d", "e"]);
        let ours = text(&["a", "c", "d", "e"]);
        let theirs = text(&["a", "b", "c", "d", "new", "e"]);
        assert_eq!(
            lines_of(&base, &ours, &theirs),
            Some(text(&["a", "c", "d", "new", "e"]))
        );
    }

    #[test]
    fn a_last_line_without_a_line_end_is_its_own_line() {
        // Both sides change the last line, one of them only by its line end.
        assert_eq!(lines_of("a\nb", "a\nb\n", "a\nb"), Some("a\nb\n".into()));
        assert_eq!(lines_of("a\nb", "a\nb\n", "a\nB"), None);
        assert_eq!(lines_of("", "a\n", ""), Some("a\n".into()));
        assert_eq!(lines_of("", "a\n", "b\n"), None);
    }

    #[test]
    fn a_binary_file_and_a_file_that_is_not_utf8_are_never_merged_by_line() {
        let binary = |base: &[u8], ours: &[u8], theirs: &[u8]| {
            merge_lines(&Merge::try_from_terms(vec![ours, base, theirs]).unwrap())
        };
        // The changes are far apart, so that text would merge.
        let (base, ours, theirs) = (
            b"a\nb\nc\nd\ne\n".as_slice(),
            b"A\nb\nc\nd\ne\n".as_slice(),
            b"a\nb\nc\nd\nE\n".as_slice(),
        );
        assert!(binary(base, ours, theirs).is_some());
        assert_eq!(binary(base, b"A\0\nb\nc\nd\ne\n", theirs), None);
        assert_eq!(binary(base, b"A\xff\nb\nc\nd\ne\n", theirs), None);
        assert_eq!(binary(b"a\xfe\nb\nc\nd\ne\n", ours, theirs), None);
    }

    #[test]
    fn a_path_that_one_side_changed_is_the_changed_value() {
        let base = [("a", "1\n"), ("b", "2\n")];
        let ours = [("a", "1\n"), ("b", "3\n")];
        assert_eq!(clean(&base, &ours, &base), expected(&ours));
        assert_eq!(clean(&base, &base, &ours), expected(&ours));
    }

    #[test]
    fn a_file_is_merged_by_line_and_the_other_files_stay() {
        let base = [("f", "a\nb\nc\nd\ne\n"), ("g", "g\n")];
        let ours = [("f", "A\nb\nc\nd\ne\n"), ("g", "g\n")];
        let theirs = [("f", "a\nb\nc\nd\nE\n"), ("g", "g\n"), ("h", "new\n")];
        assert_eq!(
            clean(&base, &ours, &theirs),
            expected(&[("f", "A\nb\nc\nd\nE\n"), ("g", "g\n"), ("h", "new\n")])
        );
    }

    #[test]
    fn both_sides_adding_the_same_file_is_taken_and_different_files_are_a_conflict() {
        let same = [("n", "same\n")];
        assert_eq!(clean(&[], &same, &same), expected(&same));

        let trees = merge3(&[], &[("n", "one\n")], &[("n", "two\n")]);
        assert_eq!(trees.len(), 3);
        assert_eq!(trees[0], expected(&[("n", "one\n")]));
        assert!(trees[1].is_empty());
        assert_eq!(trees[2], expected(&[("n", "two\n")]));
    }

    #[test]
    fn a_file_that_one_side_deleted_and_the_other_changed_is_a_conflict() {
        let base = [("f", "1\n"), ("k", "keep\n")];
        let trees = merge3(&base, &[("k", "keep\n")], &[("f", "2\n"), ("k", "keep\n")]);
        assert_eq!(trees.len(), 3);
        assert_eq!(trees[0], expected(&[("k", "keep\n")]));
        assert_eq!(trees[1], expected(&base));
        assert_eq!(trees[2], expected(&[("f", "2\n"), ("k", "keep\n")]));
    }

    #[test]
    fn a_file_that_one_side_deleted_and_the_other_left_alone_is_deleted() {
        let base = [("f", "1\n"), ("k", "keep\n")];
        let ours = [("k", "keep\n")];
        assert_eq!(clean(&base, &ours, &base), expected(&ours));
        assert_eq!(clean(&base, &base, &ours), expected(&ours));
        assert_eq!(clean(&base, &ours, &ours), expected(&ours));
    }

    #[test]
    fn a_rename_is_a_delete_and_an_add_so_an_edit_on_the_other_side_is_a_conflict() {
        let base = [("old", "text\n")];
        let renamed = [("new", "text\n")];
        let edited = [("old", "text changed\n")];
        assert_eq!(merge3(&base, &renamed, &edited).len(), 3);
    }

    #[test]
    fn the_mode_and_the_content_are_merged_independently() {
        let base = [("run", "a\nb\nc\nd\ne\n")];
        let ours = [("run", "x:a\nb\nc\nd\ne\n")];
        let theirs = [("run", "a\nb\nc\nd\nE\n")];
        assert_eq!(
            clean(&base, &ours, &theirs),
            expected(&[("run", "x:a\nb\nc\nd\nE\n")])
        );
    }

    #[test]
    fn a_conflict_of_the_content_keeps_the_mode_that_was_decided() {
        let base = [("run", "a\n")];
        let trees = merge3(&base, &[("run", "x:one\n")], &[("run", "two\n")]);
        assert_eq!(trees.len(), 3);
        assert_eq!(trees[0], expected(&[("run", "x:one\n")]));
        assert_eq!(trees[1], expected(&[("run", "x:a\n")]));
        assert_eq!(trees[2], expected(&[("run", "x:two\n")]));
    }

    #[test]
    fn the_same_change_of_the_mode_on_both_sides_is_taken() {
        let base = [("run", "a\n")];
        let both = [("run", "x:a\n")];
        assert_eq!(clean(&base, &both, &both), expected(&both));
    }

    #[test]
    fn a_symbolic_link_is_one_value_that_is_never_merged_by_line() {
        let base = [("l", "l:a\nb\nc\nd\ne")];
        assert_eq!(
            clean(&base, &[("l", "l:other")], &base),
            expected(&[("l", "l:other")])
        );
        let trees = merge3(
            &base,
            &[("l", "l:a\nb\nc\nd\nE")],
            &[("l", "l:A\nb\nc\nd\ne")],
        );
        assert_eq!(trees.len(), 3);
    }

    #[test]
    fn a_change_of_the_type_on_one_side_is_taken() {
        let base = [("p", "file\n")];
        let link = [("p", "l:target")];
        assert_eq!(clean(&base, &link, &base), expected(&link));
        let directory = [("p/q", "inside\n")];
        assert_eq!(clean(&base, &base, &directory), expected(&directory));
    }

    #[test]
    fn different_changes_of_the_type_are_a_conflict_of_the_path() {
        let base = [("p", "file\n")];
        let trees = merge3(&base, &[("p/q", "inside\n")], &[("p", "l:target")]);
        assert_eq!(trees.len(), 3);
        assert_eq!(trees[0], expected(&[("p/q", "inside\n")]));
        assert_eq!(trees[1], expected(&base));
        assert_eq!(trees[2], expected(&[("p", "l:target")]));
    }

    #[test]
    fn a_file_against_a_directory_is_a_conflict_of_the_path() {
        let trees = merge3(&[], &[("p", "file\n")], &[("p/q", "inside\n")]);
        assert_eq!(trees.len(), 3);
    }

    #[test]
    fn directories_are_merged_below_the_path() {
        let base = [("d/a", "1\n"), ("d/e/b", "2\n"), ("z", "z\n")];
        let ours = [("d/a", "changed\n"), ("d/e/b", "2\n"), ("z", "z\n")];
        let theirs = [
            ("d/a", "1\n"),
            ("d/e/b", "2\n"),
            ("d/e/c", "new\n"),
            ("z", "z\n"),
        ];
        assert_eq!(
            clean(&base, &ours, &theirs),
            expected(&[
                ("d/a", "changed\n"),
                ("d/e/b", "2\n"),
                ("d/e/c", "new\n"),
                ("z", "z\n")
            ])
        );
    }

    #[test]
    fn a_directory_that_one_side_deleted_and_the_other_left_alone_is_gone() {
        let base = [("d/a", "1\n"), ("d/b", "2\n"), ("z", "z\n")];
        let ours = [("z", "z\n")];
        assert_eq!(clean(&base, &ours, &base), expected(&ours));
    }

    #[test]
    fn a_directory_that_merges_to_nothing_is_absent() {
        let base = [("d/a", "1\n"), ("z", "z\n")];
        let ours = [("z", "z\n")];
        let theirs = [("d/a", "1\n"), ("z", "z\n"), ("w", "w\n")];
        let dir = TempDir::new();
        let store = dir.store();
        let (b, o, t) = (
            build(&store, &base),
            build(&store, &ours),
            build(&store, &theirs),
        );
        let merged = merge_trees(&store, &Merge::try_from_terms(vec![o, b, t]).unwrap()).unwrap();
        let want = build(&store, &[("z", "z\n"), ("w", "w\n")]);
        // The ID of the result is that of a tree that never had the directory.
        assert_eq!(merged.resolution(), Some(want));
    }

    #[test]
    fn a_directory_deleted_on_one_side_and_changed_on_the_other_conflicts_only_where_it_changed() {
        let base = [("d/a", "1\n"), ("d/b", "2\n")];
        let theirs = [("d/a", "changed\n"), ("d/b", "2\n")];
        let trees = merge3(&base, &[], &theirs);
        assert_eq!(trees.len(), 3);
        // The untouched file is deleted in every term and does not appear; the changed one conflicts.
        assert!(trees[0].is_empty());
        assert_eq!(trees[1], expected(&[("d/a", "1\n")]));
        assert_eq!(trees[2], expected(&[("d/a", "changed\n")]));
    }

    #[test]
    fn a_conflict_below_a_directory_does_not_hide_a_clean_merge_beside_it() {
        let base = [("d/a", "1\n"), ("e", "a\nb\nc\nd\ne\n")];
        let ours = [("d/a", "one\n"), ("e", "A\nb\nc\nd\ne\n")];
        let theirs = [("d/a", "two\n"), ("e", "a\nb\nc\nd\nE\n")];
        let trees = merge3(&base, &ours, &theirs);
        assert_eq!(trees.len(), 3);
        for tree in &trees {
            assert_eq!(tree["e"], "A\nb\nc\nd\nE\n");
        }
        assert_eq!(trees[0]["d/a"], "one\n");
        assert_eq!(trees[1]["d/a"], "1\n");
        assert_eq!(trees[2]["d/a"], "two\n");
    }

    #[test]
    fn a_sum_that_cancels_reads_no_tree() {
        let dir = TempDir::new();
        let store = dir.store();
        let base = build(&store, &[("d/a", "1\n")]);
        let ours = build(&store, &[("d/a", "2\n")]);
        // The tree of `theirs` is the base, so the sum cancels without a read or a write.
        let failing = FaultyStore::new(dir.store(), true, true);
        let merged = merge_trees(
            &failing,
            &Merge::try_from_terms(vec![ours, base, base]).unwrap(),
        );
        assert_eq!(merged.unwrap().resolution(), Some(ours));
    }

    #[test]
    fn a_failing_store_is_reported() {
        let dir = TempDir::new();
        let store = dir.store();
        let (b, o, t) = (
            build(&store, &[("a", "1\n")]),
            build(&store, &[("a", "2\n")]),
            build(&store, &[("a", "3\n")]),
        );
        let failing = FaultyStore::new(dir.store(), true, false);
        let error =
            merge_trees(&failing, &Merge::try_from_terms(vec![o, b, t]).unwrap()).unwrap_err();
        assert!(matches!(error, MergeError::Tree(_)), "{error:?}");
        assert!(error.store_error().is_some());
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn a_tree_that_is_missing_is_reported() {
        let dir = TempDir::new();
        let store = dir.store();
        let (b, o) = (
            build(&store, &[("a", "1\n")]),
            build(&store, &[("a", "2\n")]),
        );
        let absent = TreeId::new(ObjectId::from(Blob::new("absent").id()));
        let error =
            merge_trees(&store, &Merge::try_from_terms(vec![o, b, absent]).unwrap()).unwrap_err();
        assert!(
            matches!(
                error,
                MergeError::Missing {
                    kind: Kind::Tree,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    fn commit(store: &dyn ObjectStore, tree: TreeId, parents: &[CommitId], n: i64) -> CommitId {
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

    #[test]
    fn a_fork_merges_over_the_commit_where_it_forks() {
        let dir = TempDir::new();
        let store = dir.store();
        let file = |lines: &[&str]| build(&store, &[("f", &text(lines))]);
        let root = commit(&store, file(&["a", "b", "c", "d", "e"]), &[], 1);
        let ours = commit(&store, file(&["A", "b", "c", "d", "e"]), &[root], 2);
        let theirs = commit(&store, file(&["a", "b", "c", "d", "E"]), &[root], 3);
        let sum = tree_sum(&store, ours, theirs).unwrap();
        assert_eq!(sum.sides(), 2);
        let merged = merge_commits(&store, ours, theirs).unwrap();
        let tree = merged.resolution().expect("a clean merge");
        assert_eq!(flat(&store, tree)["f"], text(&["A", "b", "c", "d", "E"]));
    }

    #[test]
    fn a_commit_merged_with_its_descendant_gives_the_descendant() {
        let dir = TempDir::new();
        let store = dir.store();
        let first = build(&store, &[("f", "1\n")]);
        let second = build(&store, &[("f", "2\n")]);
        let ancestor = commit(&store, first, &[], 1);
        let descendant = commit(&store, second, &[ancestor], 2);
        for (ours, theirs) in [(ancestor, descendant), (descendant, ancestor)] {
            let merged = merge_commits(&store, ours, theirs).unwrap();
            assert_eq!(merged.resolution(), Some(second));
        }
    }

    #[test]
    fn histories_without_a_common_ancestor_merge_over_the_empty_tree() {
        let dir = TempDir::new();
        let store = dir.store();
        let ours = commit(
            &store,
            build(&store, &[("a", "1\n"), ("n", "one\n")]),
            &[],
            1,
        );
        let theirs = commit(
            &store,
            build(&store, &[("b", "2\n"), ("n", "two\n")]),
            &[],
            2,
        );
        let merged = merge_commits(&store, ours, theirs).unwrap();
        let trees: Vec<_> = merged.terms().iter().map(|&t| flat(&store, t)).collect();
        assert_eq!(trees.len(), 3);
        // The files that only one side has are in every term; the one both added conflicts.
        for tree in &trees {
            assert_eq!(tree["a"], "1\n");
            assert_eq!(tree["b"], "2\n");
        }
        assert_eq!(trees[0]["n"], "one\n");
        assert!(!trees[1].contains_key("n"));
        assert_eq!(trees[2]["n"], "two\n");
    }

    /// The history of a criss-cross merge: a root, two branches, and two merges that each take in
    /// the other branch. `ours` also changes a line that neither branch touched.
    fn criss_cross(store: &dyn ObjectStore) -> (CommitId, CommitId, [CommitId; 2]) {
        let file = |lines: &[&str]| build(store, &[("f", &text(lines))]);
        let nine = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];
        let with = |changes: &[(usize, &str)]| {
            let mut lines = nine.to_vec();
            for &(at, line) in changes {
                lines[at] = line;
            }
            file(&lines)
        };
        let root = commit(store, with(&[]), &[], 1);
        let left = commit(store, with(&[(1, "left")]), &[root], 2);
        let right = commit(store, with(&[(7, "right")]), &[root], 3);
        let both = [(1, "left"), (7, "right")];
        let ours = commit(
            store,
            with(&[both[0], both[1], (4, "ours")]),
            &[left, right],
            4,
        );
        let theirs = commit(store, with(&both), &[right, left], 5);
        (ours, theirs, [left, right])
    }

    #[test]
    fn a_criss_cross_is_merged_as_a_flat_sum_of_all_bases() {
        let dir = TempDir::new();
        let store = dir.store();
        let (ours, theirs, bases) = criss_cross(&store);
        let mut found = merge_bases(&store, ours, theirs).unwrap();
        found.sort();
        let mut expected_bases = bases.to_vec();
        expected_bases.sort();
        assert_eq!(found, expected_bases);

        // `ours + theirs - left - right + root`: three sides and two bases.
        let sum = tree_sum(&store, ours, theirs).unwrap();
        assert_eq!(sum.sides(), 3);

        let merged = merge_commits(&store, ours, theirs).unwrap();
        let tree = merged.resolution().expect("a clean merge");
        assert_eq!(
            flat(&store, tree)["f"],
            text(&["1", "left", "3", "4", "ours", "6", "7", "right", "9"])
        );
    }

    #[test]
    fn the_merge_does_not_depend_on_which_side_is_ours() {
        let dir = TempDir::new();
        let store = dir.store();
        let (ours, theirs, _) = criss_cross(&store);
        let one = merge_commits(&store, ours, theirs).unwrap();
        let other = merge_commits(&store, theirs, ours).unwrap();
        assert!(one.is_equivalent(&other));
    }

    #[test]
    fn a_conflict_in_a_criss_cross_has_the_sides_of_the_sum() {
        let dir = TempDir::new();
        let store = dir.store();
        let file = |text: &str| build(&store, &[("f", text)]);
        let lines = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];
        let with = |at: usize, line: &str| {
            let mut changed = lines.to_vec();
            changed[at] = line;
            text(&changed)
        };
        let root = commit(&store, file(&text(&lines)), &[], 1);
        let left = commit(&store, file(&with(1, "left")), &[root], 2);
        let right = commit(&store, file(&with(7, "right")), &[root], 3);
        // The two merges change the same line of the middle differently.
        let merged_text = |middle: &str| {
            let mut changed = lines.to_vec();
            changed[1] = "left";
            changed[7] = "right";
            changed[4] = middle;
            text(&changed)
        };
        let ours = commit(&store, file(&merged_text("ours")), &[left, right], 4);
        let theirs = commit(&store, file(&merged_text("theirs")), &[right, left], 5);
        let merged = merge_commits(&store, ours, theirs).unwrap();
        // `ours + theirs - left - right + root`: the conflict has three sides and two bases, and
        // every term holds the whole file.
        assert_eq!(merged.sides(), 3);
        let trees: Vec<_> = merged.terms().iter().map(|&t| flat(&store, t)).collect();
        assert_eq!(trees.len(), 5);
        assert_eq!(trees[0]["f"], merged_text("ours"));
        assert_eq!(trees[1]["f"], with(1, "left"));
        assert_eq!(trees[2]["f"], merged_text("theirs"));
        assert_eq!(trees[3]["f"], with(7, "right"));
        assert_eq!(trees[4]["f"], text(&lines));
    }

    /// A text of numbered lines, and the edits that two sides make to it.
    ///
    /// `None` leaves a line alone, `Some(true)` replaces it, and `Some(false)` deletes it. Only
    /// `ours` deletes: a line that both sides delete is the same change, which is not touching.
    fn generated(rng: &mut Rng, lines: usize, deletes: bool) -> Vec<Option<bool>> {
        (0..lines)
            .map(|_| rng.one_in(4).then(|| !(deletes && rng.one_in(3))))
            .collect()
    }

    fn apply(edits: &[Option<bool>], prefix: &str) -> String {
        edits
            .iter()
            .enumerate()
            .filter_map(|(at, edit)| match edit {
                None => Some(format!("line {at}\n")),
                Some(true) => Some(format!("{prefix} {at}\n")),
                Some(false) => None,
            })
            .collect()
    }

    #[test]
    fn generated_edits_merge_exactly_when_no_two_of_them_touch() {
        let (seed, mut rng) = (seed(), Rng::new(seed(), 0x3E26));
        let base: String = (0..14).map(|at| format!("line {at}\n")).collect();
        let (mut clean_cases, mut conflict_cases) = (0, 0);
        for case in 0..cases() {
            let ours = generated(&mut rng, 14, true);
            let theirs = generated(&mut rng, 14, false);
            let touching = ours.iter().enumerate().any(|(i, o)| {
                o.is_some() && (i.saturating_sub(1)..=(i + 1).min(13)).any(|j| theirs[j].is_some())
            });
            let (our_text, their_text) = (apply(&ours, "ours"), apply(&theirs, "theirs"));
            let merged = lines_of(&base, &our_text, &their_text);
            let swapped = lines_of(&base, &their_text, &our_text);
            let context = format!("case {case}, seed {seed}: {ours:?} against {theirs:?}");
            assert_eq!(merged, swapped, "{context}");
            if touching {
                conflict_cases += 1;
                assert_eq!(merged, None, "{context}");
            } else {
                clean_cases += 1;
                // Neither side touches the other: apply both sets of edits to one text.
                let combined: Vec<Option<bool>> =
                    ours.iter().zip(&theirs).map(|(o, t)| o.or(*t)).collect();
                let want: String = combined
                    .iter()
                    .enumerate()
                    .filter_map(|(at, edit)| match edit {
                        None => Some(format!("line {at}\n")),
                        Some(true) if ours[at].is_some() => Some(format!("ours {at}\n")),
                        Some(true) => Some(format!("theirs {at}\n")),
                        Some(false) => None,
                    })
                    .collect();
                assert_eq!(merged, Some(want), "{context}");
            }
        }
        assert!(
            clean_cases > 0 && conflict_cases > 0,
            "the generator is one-sided"
        );
    }

    #[test]
    fn the_order_of_the_bases_does_not_change_the_result() {
        let dir = TempDir::new();
        let store = dir.store();
        let (ours, theirs, [left, right]) = criss_cross(&store);
        let merge = |bases: &[CommitId]| {
            let base = base_sum(&store, bases).unwrap();
            let our_tree = read_commit(&store, ours, None).unwrap().tree_sum().terms()[0];
            let their_tree = read_commit(&store, theirs, None)
                .unwrap()
                .tree_sum()
                .terms()[0];
            let (adds, removes) = (
                base.removes().copied().collect::<Vec<_>>(),
                base.adds().copied().collect::<Vec<_>>(),
            );
            let mut all = vec![our_tree, their_tree];
            all.extend(adds);
            merge_trees(&store, &Merge::from_parts(all, removes)).unwrap()
        };
        assert!(merge(&[left, right]).is_equivalent(&merge(&[right, left])));
    }

    /// The files of the prototype of the conflict model (STEMMA-153): the base and three branches.
    const PROTOTYPE_BASE: [(&str, &str); 3] = [
        ("app.conf", "timeout=30"),
        ("notes.txt", "v1"),
        ("HOWTO.md", "v1"),
    ];
    const PROTOTYPE_MAIN: [(&str, &str); 3] = [
        ("app.conf", "timeout=45"),
        ("notes.txt", "v1"),
        ("HOWTO.md", "v1"),
    ];
    const PROTOTYPE_FEATURE: [(&str, &str); 3] = [
        ("app.conf", "timeout=60"),
        ("notes.txt", "v2"),
        ("HOWTO.md", "v1"),
    ];
    const PROTOTYPE_HOTFIX: [(&str, &str); 3] = [
        ("app.conf", "timeout=10"),
        ("notes.txt", "v1"),
        ("HOWTO.md", "v1"),
    ];

    #[test]
    fn golden_the_prototype_merges_feature_into_main_with_one_conflict() {
        let trees = merge3(&PROTOTYPE_BASE, &PROTOTYPE_MAIN, &PROTOTYPE_FEATURE);
        assert_eq!(trees.len(), 3);
        // `notes.txt` and `HOWTO.md` are decided in every term; only `app.conf` is open.
        for tree in &trees {
            assert_eq!(tree["notes.txt"], "v2");
            assert_eq!(tree["HOWTO.md"], "v1");
        }
        assert_eq!(trees[0]["app.conf"], "timeout=45");
        assert_eq!(trees[1]["app.conf"], "timeout=30");
        assert_eq!(trees[2]["app.conf"], "timeout=60");
    }

    #[test]
    fn golden_a_second_branch_that_lands_on_the_open_conflict_extends_the_sum() {
        let dir = TempDir::new();
        let store = dir.store();
        let [base, main, feature, hotfix] = [
            &PROTOTYPE_BASE,
            &PROTOTYPE_MAIN,
            &PROTOTYPE_FEATURE,
            &PROTOTYPE_HOTFIX,
        ]
        .map(|files| build(&store, files));
        let first = merge_trees(
            &store,
            &Merge::try_from_terms(vec![main, base, feature]).unwrap(),
        )
        .unwrap();
        // `Merge::land` adds the change from the base to the hotfix to the open conflict.
        let landed = first.land(base, hotfix);
        let merged = merge_trees(&store, &landed).unwrap();
        let trees: Vec<_> = merged.terms().iter().map(|&t| flat(&store, t)).collect();
        // Three sides and two bases, the same base twice, and nothing nested.
        assert_eq!(merged.sides(), 3);
        assert_eq!(trees.len(), 5);
        let app: Vec<&str> = trees.iter().map(|tree| tree["app.conf"].as_str()).collect();
        assert_eq!(
            app,
            [
                "timeout=45",
                "timeout=30",
                "timeout=60",
                "timeout=30",
                "timeout=10"
            ]
        );
        for tree in &trees {
            assert_eq!(tree["notes.txt"], "v2");
        }
    }

    #[test]
    fn golden_a_text_that_only_talks_about_markers_is_no_conflict() {
        let marker = "<<<<<<< this is the sample\nline\n>>>>>>>\n";
        let base = [("HOWTO.md", "intro\n")];
        let ours = [("HOWTO.md", marker)];
        assert_eq!(clean(&base, &ours, &base), expected(&ours));
    }

    #[test]
    fn a_tree_merged_with_a_rewrite_of_itself_resolves() {
        let dir = TempDir::new();
        let store = dir.store();
        let tree = build(&store, &[("f", "a\nb\nc\n")]);
        let parent = commit(&store, build(&store, &[("f", "a\nb\n")]), &[], 1);
        let original = commit(&store, tree, &[parent], 2);
        // The rewrite has the same tree and parent and another date and message.
        let rewrite = commit(&store, tree, &[parent], 3);
        assert_ne!(original, rewrite);
        let merged = merge_commits(&store, original, rewrite).unwrap();
        assert_eq!(merged.resolution(), Some(tree));
    }

    #[test]
    fn generated_histories_merge_with_themselves_and_their_ancestors_as_the_identity() {
        let dir = TempDir::new();
        let store = dir.store();
        let (seed, mut rng) = (seed(), Rng::new(seed(), 0x3E27));
        let names = ["a", "b", "d/c"];
        for round in 0..(cases() / 1000).max(1) {
            let mut commits: Vec<(CommitId, TreeId)> = Vec::new();
            let mut files: Vec<Vec<String>> = Vec::new();
            for n in 0..9 {
                let mut parents: Vec<usize> = Vec::new();
                if n > 0 {
                    parents.push(rng.below(n));
                    if n > 1 && rng.one_in(3) {
                        let other = rng.below(n);
                        if !parents.contains(&other) {
                            parents.push(other);
                        }
                    }
                }
                // The files of the first parent, with one line of one file changed.
                let mut own = parents.first().map_or_else(
                    || {
                        names
                            .iter()
                            .map(|_| text(&["1", "2", "3", "4", "5", "6"]))
                            .collect()
                    },
                    |&p| files[p].clone(),
                );
                let which = rng.below(names.len());
                let line = rng.below(6);
                let mut changed: Vec<String> = own[which].lines().map(str::to_string).collect();
                changed[line] = format!("c{round}-{n}");
                own[which] = text(&changed.iter().map(String::as_str).collect::<Vec<_>>());
                let tree = build(
                    &store,
                    &names
                        .iter()
                        .zip(&own)
                        .map(|(n, c)| (*n, c.as_str()))
                        .collect::<Vec<_>>(),
                );
                let ids: Vec<CommitId> = parents.iter().map(|&p| commits[p].0).collect();
                let id = commit(&store, tree, &ids, i64::try_from(n).unwrap());
                commits.push((id, tree));
                files.push(own);
            }
            for _ in 0..8 {
                let (a, b) = (rng.below(commits.len()), rng.below(commits.len()));
                let context = format!("round {round}, seed {seed}, commits {a} and {b}");
                let ab = merge_commits(&store, commits[a].0, commits[b].0).unwrap();
                let ba = merge_commits(&store, commits[b].0, commits[a].0).unwrap();
                assert!(ab.is_equivalent(&ba), "{context}");
                if a == b {
                    assert_eq!(ab.resolution(), Some(commits[a].1), "{context}");
                }
                let bases = merge_bases(&store, commits[a].0, commits[b].0).unwrap();
                if bases == [commits[a].0] {
                    assert_eq!(ab.resolution(), Some(commits[b].1), "{context}");
                } else if bases == [commits[b].0] {
                    assert_eq!(ab.resolution(), Some(commits[a].1), "{context}");
                }
            }
        }
    }

    #[test]
    fn a_commit_that_is_missing_is_reported() {
        let (dir, other) = (TempDir::new(), TempDir::new());
        let (store, elsewhere) = (dir.store(), other.store());
        let tree = build(&store, &[]);
        let known = commit(&store, tree, &[], 1);
        // The tree is in both stores, the commit only in the other one.
        let absent = commit(&elsewhere, build(&elsewhere, &[]), &[], 2);
        let error = merge_commits(&store, known, absent).unwrap_err();
        assert!(matches!(error, MergeError::History(_)), "{error:?}");
    }

    #[test]
    fn the_checks_of_the_fuzz_target_hold_for_generated_texts() {
        let (seed, mut rng) = (seed(), Rng::new(seed(), 0x3E28));
        // A small alphabet, so that the three texts share many lines, and a few bytes that make a
        // text binary or not UTF-8.
        let alphabet: &[u8] = b"ab\n\n\n  \x1e\x1e\0\xff";
        for case in 0..cases() {
            let length = rng.below(40);
            let input: Vec<u8> = (0..length).map(|_| *rng.pick(alphabet)).collect();
            let result = std::panic::catch_unwind(|| crate::fuzz::merge_lines(&input));
            assert!(result.is_ok(), "case {case}, seed {seed}: {input:?}");
        }
    }

    #[test]
    fn the_checks_of_the_fuzz_target_reach_the_text_branch_for_generated_texts() {
        let mut rng = Rng::new(seed(), 0x3E29);
        // Text only, so that the equal-sides, one-side-changed, and known-lines checks run.
        let alphabet: &[u8] = b"ab\n\n\n  \x1e\x1e";
        for _ in 0..cases() {
            let length = rng.below(40);
            let input: Vec<u8> = (0..length).map(|_| *rng.pick(alphabet)).collect();
            crate::fuzz::merge_lines(&input);
        }
    }

    #[test]
    fn a_line_deleted_on_one_side_and_changed_on_the_other_is_a_conflict() {
        let base = text(&["a", "b", "c", "d", "e"]);
        let deleted = text(&["a", "c", "d", "e"]);
        let changed = text(&["a", "B", "c", "d", "e"]);
        assert_eq!(lines_of(&base, &deleted, &changed), None);
        assert_eq!(lines_of(&base, &changed, &deleted), None);
    }

    #[test]
    fn the_same_deletion_on_both_sides_is_made_once() {
        let base = text(&["a", "b", "c", "d", "e", "f", "g"]);
        let ours = text(&["a", "c", "d", "e", "f", "G"]);
        let theirs = text(&["a", "c", "d", "E", "f", "g"]);
        let merged = Some(text(&["a", "c", "d", "E", "f", "G"]));
        assert_eq!(lines_of(&base, &ours, &theirs), merged);
        assert_eq!(lines_of(&base, &theirs, &ours), merged);
    }

    #[test]
    fn repeated_lines_are_merged_symmetrically() {
        let base = text(&["x", "x", "x"]);
        let ours = text(&["y", "x", "x", "x"]);
        let theirs = text(&["x", "x", "x", "z"]);
        let merged = Some(text(&["y", "x", "x", "x", "z"]));
        assert_eq!(lines_of(&base, &ours, &theirs), merged);
        assert_eq!(lines_of(&base, &theirs, &ours), merged);
    }

    #[test]
    fn a_directory_replaced_by_a_file_is_a_conflict_if_the_other_side_edits_inside() {
        let trees = merge3(&[("d/a", "1\n")], &[("d", "file\n")], &[("d/a", "2\n")]);
        assert_eq!(trees.len(), 3, "{trees:?}");
        assert_eq!(trees[0], expected(&[("d", "file\n")]));
        assert_eq!(trees[1], expected(&[("d/a", "1\n")]));
        assert_eq!(trees[2], expected(&[("d/a", "2\n")]));
    }

    #[test]
    fn a_directory_replaced_by_a_file_on_one_side_alone_is_the_file() {
        let merged = clean(&[("d/a", "1\n")], &[("d", "file\n")], &[("d/a", "1\n")]);
        assert_eq!(merged, expected(&[("d", "file\n")]));
    }

    /// Merges `ours` and `theirs` over the given bases, in the given order.
    fn merge_over(
        store: &dyn ObjectStore,
        ours: CommitId,
        theirs: CommitId,
        bases: &[CommitId],
    ) -> Merge<TreeId> {
        let base = base_sum(store, bases).unwrap();
        let mut adds = vec![
            read_commit(store, ours, None).unwrap().tree_sum().terms()[0],
            read_commit(store, theirs, None).unwrap().tree_sum().terms()[0],
        ];
        adds.extend(base.removes().copied());
        let removes = base.adds().copied().collect();
        merge_trees(store, &Merge::from_parts(adds, removes)).unwrap()
    }

    #[test]
    fn a_change_that_two_of_three_bases_share_is_counted_once_in_every_order() {
        let dir = TempDir::new();
        let store = dir.store();
        let files = |x: &str, a: &str, b: &str, c: &str, o: &str| {
            build(&store, &[("x", x), ("a", a), ("b", b), ("c", c), ("o", o)])
        };
        let w = commit(&store, files("0", "0", "0", "0", "0"), &[], 1);
        let x = commit(&store, files("1", "0", "0", "0", "0"), &[w], 2);
        let b1 = commit(&store, files("0", "1", "0", "0", "0"), &[w], 3);
        let b2 = commit(&store, files("1", "0", "1", "0", "0"), &[x], 4);
        let b3 = commit(&store, files("1", "0", "0", "1", "0"), &[x], 5);
        let m12 = commit(&store, files("1", "1", "1", "0", "0"), &[b1, b2], 6);
        let ours = commit(&store, files("1", "1", "1", "1", "1"), &[m12, b3], 7);
        let m23 = commit(&store, files("1", "0", "1", "1", "0"), &[b2, b3], 8);
        let theirs = commit(&store, files("1", "1", "1", "1", "0"), &[m23, b1], 9);

        let mut found = merge_bases(&store, ours, theirs).unwrap();
        found.sort();
        let mut want = vec![b1, b2, b3];
        want.sort();
        assert_eq!(found, want);

        let result = files("1", "1", "1", "1", "1");
        for order in [
            [b1, b2, b3],
            [b1, b3, b2],
            [b2, b1, b3],
            [b2, b3, b1],
            [b3, b1, b2],
            [b3, b2, b1],
        ] {
            let merged = merge_over(&store, ours, theirs, &order);
            assert_eq!(merged.resolution(), Some(result), "order {order:?}");
        }
    }

    #[test]
    fn a_link_against_a_file_is_a_conflict_and_a_link_changed_the_same_way_is_taken() {
        let base = [("p", "file\n")];
        let link = [("p", "l:target")];
        assert_eq!(
            merge3(&base, &link, &base.map(|(n, _)| (n, "other\n"))).len(),
            3
        );
        assert_eq!(clean(&base, &link, &link), expected(&link));
    }

    #[test]
    fn a_missing_blob_is_reported_with_its_kind() {
        let dir = TempDir::new();
        let store = dir.store();
        let absent = Blob::new("absent").id();
        let tree = |blob: BlobId| {
            let entry = TreeEntry::file("f", FileMode::File, blob).unwrap();
            store
                .put_tree(&Tree::try_new(vec![entry]).unwrap())
                .unwrap()
        };
        let present = store.put_blob(&Blob::new("a\nb\nc\nd\ne\n")).unwrap();
        let other = store.put_blob(&Blob::new("A\nb\nc\nd\ne\n")).unwrap();
        let sum = Merge::try_from_terms(vec![tree(other), tree(present), tree(absent)]).unwrap();
        let error = merge_trees(&store, &sum).unwrap_err();
        assert!(
            matches!(
                error,
                MergeError::Missing {
                    kind: Kind::Blob,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("blob"), "{error}");
        assert!(error.store_error().is_none());
    }

    #[test]
    fn a_store_that_cannot_write_fails_the_merge_with_a_store_error() {
        let dir = TempDir::new();
        let store = dir.store();
        let (b, o, t) = (
            build(&store, &[("a", "1\n")]),
            build(&store, &[("a", "2\n")]),
            build(&store, &[("b", "3\n")]),
        );
        let failing = FaultyStore::new(dir.store(), false, true);
        let error =
            merge_trees(&failing, &Merge::try_from_terms(vec![o, b, t]).unwrap()).unwrap_err();
        assert!(matches!(error, MergeError::Store(_)), "{error:?}");
        assert!(error.store_error().is_some());
    }

    #[test]
    fn the_errors_print_their_cause_once_and_expose_the_cause_of_the_cause() {
        let io = || StoreError::Io {
            operation: "read the object file",
            path: std::path::PathBuf::from("objects/b3/00/x"),
            source: std::io::Error::other("the disk failed"),
        };
        let store = MergeError::Store(io());
        assert!(store.to_string().contains("the disk failed"));
        // The message holds the cause already, and the I/O error has no cause of its own.
        assert!(store.source().is_none());
        let tree = MergeError::Tree(ReadError::Store(Box::new(io())));
        assert!(tree.store_error().is_some());
        assert!(tree.to_string().contains("the disk failed"));
        let missing = MergeError::Missing {
            id: Blob::new("x").id().object_id(),
            kind: Kind::Tree,
        };
        assert!(missing.to_string().starts_with("the tree "));
        assert!(missing.source().is_none() && missing.store_error().is_none());
        let entries = MergeError::Entries(TreeError::DuplicateName("n".into()));
        assert!(entries.to_string().contains("duplicate tree entry name"));
        assert!(entries.store_error().is_none());
        let history = MergeError::History(HistoryError::Missing {
            id: CommitId::new(Blob::new("c").id().object_id()),
            child: None,
        });
        assert!(history.store_error().is_none());
    }

    #[test]
    fn a_sum_of_five_texts_is_merged_by_line_over_the_first_base_that_did_not_cancel() {
        // `ours + theirs - left - right + root`, with the changes in different places.
        let root = text(&["1", "2", "3", "4", "5", "6", "7", "8", "9"]);
        let left = text(&["1", "L", "3", "4", "5", "6", "7", "8", "9"]);
        let right = text(&["1", "2", "3", "4", "5", "6", "7", "R", "9"]);
        let ours = text(&["1", "L", "3", "4", "O", "6", "7", "R", "9"]);
        let theirs = text(&["1", "L", "3", "4", "5", "6", "7", "R", "9"]);
        let sum = Merge::try_from_terms(vec![
            ours.as_bytes(),
            left.as_bytes(),
            theirs.as_bytes(),
            right.as_bytes(),
            root.as_bytes(),
        ])
        .unwrap();
        assert_eq!(merge_lines(&sum), Some(ours.clone().into_bytes()));
    }
}
