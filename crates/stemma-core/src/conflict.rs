//! Conflicts as data (ADR-005, ADR-010): a sum of values that cancels, the commit that holds a sum
//! of trees, and the text with markers that shows a conflicted file.
//!
//! A conflict is not a mark in a file. It is a sum `A + (C - B) + (E - D) + ...` of whole values,
//! read as: take `A`, and apply the change from `B` to `C`, and the change from `D` to `E`. The
//! terms alternate between a side that is added and a base that is taken away, so a sum always has
//! an odd number of terms, and one more side than it has bases. A sum of one term is not a
//! conflict, it is the value itself. [`Merge`] is the sum over any value; a commit holds it over
//! trees ([`AnyCommit`]), and the merge of a single path holds it over file contents.
//!
//! Three rules make the sum behave:
//!
//! 1. **Equal terms cancel.** An added value that equals a removed value is dropped together with
//!    it ([`Merge::simplify`]). The test is the equality of the value, which for a tree or a blob
//!    is the equality of its ID, so no content is read. Because of this a conflict never nests:
//!    a second branch that lands on an open conflict extends the sum ([`Merge::land`]) and the
//!    terms that the two have in common cancel.
//! 2. **The number of sides is global.** All paths of a commit share the number of sides, so a
//!    conflict with fewer sides is padded with pairs of equal terms ([`Merge::padded`]). They
//!    change the length and not the meaning, and simplifying removes them again.
//! 3. **One tree is a commit.** A sum that cancels down to one tree is written as a plain
//!    [`Commit`], with the encoding and the ID of a commit that never had a conflict. Only a sum of
//!    three or more trees is a [`Commit2`] ([`AnyCommit::from_sum`]).
//!
//! The algebra does not apply the same-change rule of some systems, which resolves two sides that
//! made the same change as if one of them had not changed anything. That rule loses information
//! when the result is merged again, and whether to use it is a decision of the merge, not of the
//! sum.
//!
//! The terms of a sum are compared one by one, so the cost of `simplify` is quadratic in the
//! number of terms. The number of sides is the number of branches that were merged into one
//! conflict, which is small. [`Merge::is_equivalent`] simplifies both sums and then compares the
//! terms as multisets, which is quadratic again; it is meant for tests and checks, not for a loop
//! over every path.

use std::error::Error;
use std::fmt;

use crate::change::ChangeId;
use crate::object::{Commit, Commit2, Commit2Error, CommitError, CommitId, TreeId};
use crate::Actor;

/// A sum of values: `terms[0] - terms[1] + terms[2] - terms[3] + ... + terms[n - 1]`.
///
/// The terms at even positions are added, the terms at odd positions are removed, and the length
/// is odd, so every sum has at least one side. A sum of one term is resolved: it is that value.
///
/// Equality (`==`) compares the terms in order. Two sums can mean the same although they differ
/// as lists, for example in the order of their sides or in a pair of cancelling terms. Use
/// [`is_equivalent`](Self::is_equivalent) to ask whether they mean the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge<T> {
    terms: Vec<T>,
}

/// Why [`Merge::try_from_terms`] rejects a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MergeError {
    /// The list does not have an odd length. A sum has one more side than it has bases.
    TermCount(usize),
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TermCount(count) => {
                write!(f, "a sum lists an odd number of terms, not {count}")
            }
        }
    }
}

impl Error for MergeError {}

impl<T: Clone + Eq> Merge<T> {
    /// A resolved sum: the value itself.
    pub fn resolved(value: T) -> Self {
        Self { terms: vec![value] }
    }

    /// Builds a sum from its terms in the order side, base, side, base, ..., side.
    ///
    /// # Errors
    ///
    /// Returns [`MergeError::TermCount`] if the list is empty or has an even length.
    pub fn try_from_terms(terms: Vec<T>) -> Result<Self, MergeError> {
        if terms.len().is_multiple_of(2) {
            return Err(MergeError::TermCount(terms.len()));
        }
        Ok(Self { terms })
    }

    /// Returns the terms in the order side, base, side, base, ..., side.
    pub fn terms(&self) -> &[T] {
        &self.terms
    }

    /// Returns the terms, for a caller that builds another list from them.
    pub fn into_terms(self) -> Vec<T> {
        self.terms
    }

    /// Returns the number of sides: one more than the number of bases.
    pub fn sides(&self) -> usize {
        self.terms.len() / 2 + 1
    }

    /// Returns true for a sum of one term. A sum that has terms which cancel is not resolved until
    /// it is [simplified](Self::simplify).
    pub fn is_resolved(&self) -> bool {
        self.terms.len() == 1
    }

    /// Returns the added terms, the sides, in order.
    pub fn adds(&self) -> impl Iterator<Item = &T> {
        self.terms.iter().step_by(2)
    }

    /// Returns the removed terms, the bases, in order.
    pub fn removes(&self) -> impl Iterator<Item = &T> {
        self.terms.iter().skip(1).step_by(2)
    }

    /// Cancels every removed term against an equal added term and returns what is left.
    ///
    /// The terms that stay keep their order: the sides in the order they had, the bases in the
    /// order they had. A removed term cancels against the first equal added term that has not
    /// cancelled yet. Since equal terms are interchangeable, the choice does not change the
    /// meaning, and the meaning does not depend on the order of the terms. The choice does decide
    /// the order of what is left, and with it the text of a marker and the ID of a commit. The rule
    /// is made to fit [`padded`](Self::padded): the pairs of padding stand in front, and each
    /// cancels against itself, so a padded sum simplifies to the sum it was padded from. The
    /// result is never empty: every cancelled pair takes one side and one base, and a sum has one
    /// side more.
    pub fn simplify(&self) -> Self {
        let adds: Vec<&T> = self.adds().collect();
        let removes: Vec<&T> = self.removes().collect();
        let mut add_stays = vec![true; adds.len()];
        let mut remove_stays = vec![true; removes.len()];
        for (r, remove) in removes.iter().enumerate() {
            if let Some(a) = (0..adds.len()).find(|&a| add_stays[a] && adds[a] == *remove) {
                add_stays[a] = false;
                remove_stays[r] = false;
            }
        }
        let kept_adds = adds.iter().zip(&add_stays).filter(|(_, stays)| **stays);
        let mut kept_removes = removes
            .iter()
            .zip(&remove_stays)
            .filter(|(_, stays)| **stays);
        let mut terms = Vec::with_capacity(self.terms.len());
        for (add, _) in kept_adds {
            terms.push((*add).clone());
            if let Some((remove, _)) = kept_removes.next() {
                terms.push((*remove).clone());
            }
        }
        Self { terms }
    }

    /// Returns the value if the sum cancels down to one term.
    pub fn resolution(&self) -> Option<T> {
        match self.simplify().terms.as_slice() {
            [value] => Some(value.clone()),
            _ => None,
        }
    }

    /// Takes in the change from `base` to `side`: the sum plus `side` minus `base`, simplified.
    /// This is what a branch does that lands on a conflict that is already open. Terms that the
    /// open conflict and the new change have in common cancel, so the result has no nesting.
    pub fn land(&self, base: T, side: T) -> Self {
        let mut terms = self.terms.clone();
        terms.push(base);
        terms.push(side);
        Self { terms }.simplify()
    }

    /// Pads the sum to `sides` sides with pairs of equal terms, which do not change its meaning.
    /// A sum that already has that many sides or more is returned as it is. Commits share the
    /// number of sides across all paths, and a path with fewer sides is padded to it.
    pub fn padded(&self, sides: usize) -> Self {
        let mut terms = Vec::with_capacity(self.terms.len().max(2 * sides));
        // The pairs stand in front and repeat the first term: the added one is the first side, so
        // the removed one cancels against it and no term of the original is touched.
        while terms.len() + self.terms.len() + 1 < 2 * sides {
            terms.push(self.terms[0].clone());
            terms.push(self.terms[0].clone());
        }
        terms.extend(self.terms.iter().cloned());
        Self { terms }
    }

    /// Returns true if the two sums mean the same: after simplifying, they have the same sides
    /// and the same bases, in any order.
    pub fn is_equivalent(&self, other: &Self) -> bool {
        let (left, right) = (self.simplify(), other.simplify());
        let (left_adds, right_adds): (Vec<&T>, Vec<&T>) =
            (left.adds().collect(), right.adds().collect());
        let (left_removes, right_removes): (Vec<&T>, Vec<&T>) =
            (left.removes().collect(), right.removes().collect());
        same_items(&left_adds, &right_adds) && same_items(&left_removes, &right_removes)
    }
}

/// Returns true if the two lists hold the same items the same number of times, in any order.
fn same_items<T: Eq>(a: &[&T], b: &[&T]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut taken = vec![false; b.len()];
    a.iter().all(
        |item| match (0..b.len()).find(|&i| !taken[i] && b[i] == *item) {
            Some(i) => {
                taken[i] = true;
                true
            }
            None => false,
        },
    )
}

impl From<&Commit> for Merge<TreeId> {
    fn from(commit: &Commit) -> Self {
        Self::resolved(commit.tree())
    }
}

impl From<&Commit2> for Merge<TreeId> {
    fn from(commit: &Commit2) -> Self {
        // A `Commit2` holds an odd number of at least three trees, which is the invariant of a sum.
        Self {
            terms: commit.trees().to_vec(),
        }
    }
}

/// A commit in either form: a [`Commit`] for a state without a conflict, a [`Commit2`] for a state
/// with an open conflict.
///
/// The form follows from the sum of trees and from nothing else, so one state has one encoding and
/// one ID (ADR-005): [`from_sum`](Self::from_sum) simplifies the sum, and writes a `commit` if one
/// tree is left. A resolution of a conflict is therefore a `commit` again, with the same change
/// ID that the caller passes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnyCommit {
    /// A state without a conflict.
    Commit(Commit),
    /// A state with an open conflict.
    Commit2(Commit2),
}

/// Why [`AnyCommit::from_sum`] rejects its input: the error of the form it chose.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FromSumError {
    /// The sum cancelled down to one tree and [`Commit::try_new`] rejected the commit.
    Commit(CommitError),
    /// The sum has sides left and [`Commit2::try_new`] rejected the commit.
    Commit2(Commit2Error),
}

impl fmt::Display for FromSumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Commit(source) => write!(f, "{source}"),
            Self::Commit2(source) => write!(f, "{source}"),
        }
    }
}

impl Error for FromSumError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message includes that of the cause, so the source is the cause of the cause (see the
        // crate documentation).
        match self {
            Self::Commit(source) => source.source(),
            Self::Commit2(source) => source.source(),
        }
    }
}

impl AnyCommit {
    /// Creates the commit for a sum of trees. The other arguments are those of
    /// [`Commit::try_new`].
    ///
    /// # Errors
    ///
    /// Returns the error of the commit that the sum simplifies to: a duplicate parent is the only
    /// reason today. The error of the other form cannot be a wrong number of trees, since a
    /// simplified sum of more than one term has an odd length of at least three, but the type
    /// keeps the case and does not assume it.
    pub fn from_sum(
        trees: &Merge<TreeId>,
        parents: Vec<CommitId>,
        change: ChangeId,
        author: Actor,
        timestamp: i64,
        message: impl Into<String>,
    ) -> Result<Self, FromSumError> {
        let simplified = trees.simplify();
        match simplified.terms() {
            [tree] => Commit::try_new(*tree, parents, change, author, timestamp, message)
                .map(Self::Commit)
                .map_err(FromSumError::Commit),
            _ => Commit2::try_new(
                simplified.into_terms(),
                parents,
                change,
                author,
                timestamp,
                message,
            )
            .map(Self::Commit2)
            .map_err(FromSumError::Commit2),
        }
    }

    /// Returns the sum of trees of the commit: one tree for a `commit`, the list for a `commit2`.
    pub fn tree_sum(&self) -> Merge<TreeId> {
        match self {
            Self::Commit(commit) => commit.into(),
            Self::Commit2(commit) => commit.into(),
        }
    }

    /// Returns true if the commit records an open conflict.
    pub fn is_conflicted(&self) -> bool {
        matches!(self, Self::Commit2(_))
    }

    /// Returns the canonical encoding of the commit in its form.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Commit(commit) => commit.encode(),
            Self::Commit2(commit) => commit.encode(),
        }
    }

    /// Returns the object ID of the commit in its form.
    pub fn id(&self) -> CommitId {
        match self {
            Self::Commit(commit) => commit.id(),
            Self::Commit2(commit) => commit.id(),
        }
    }
}

/// The shortest run of a marker character that starts a marker line.
const MIN_MARKER_LEN: usize = 7;

/// Returns the text that shows a conflicted file, or `None` if the sum cancels down to one term and
/// there is nothing to show.
///
/// A term is the content of the file in one version, or `None` where the file does not exist in
/// that version. The sum is simplified first, so a pair of cancelling terms never shows, and a sum
/// and its padded form give the same text. The text is a function of the list of terms and of
/// nothing else, so it depends on their order: two sums that are
/// [equivalent](Merge::is_equivalent) but list their sides in another order show them in that
/// order. The snapshot compares the file with the text of the sum that the commit stores, so the
/// order of the stored sum is the one that counts.
/// Disk, `diff`, and the snapshot all call it, and the snapshot decides whether a file is still
/// the conflict by comparing the bytes of the file with this text; Stemma does not parse markers.
///
/// The form, for a sum of three terms:
///
/// ```text
/// <<<<<<< conflict
/// +++++++ side 1
/// content of the first side
/// ------- base 1
/// content of the base
/// +++++++ side 2
/// content of the second side
/// >>>>>>>
/// ```
///
/// - The terms follow in the order of the sum: side, base, side, base, ..., side. The sides and
///   the bases are numbered from 1 in that order.
/// - A marker line starts with a run of `<`, `+`, `-`, or `>`. The run has seven characters. If a
///   line of some content starts with a run of such a character, the run of all marker lines is
///   one longer than the longest of them, so that no line of content is a marker line.
/// - A term that is absent has `(absent)` after its label and no lines. A term whose last line has
///   no line feed gets one, and `(no newline at end)` after its label tells that it was not
///   there. The text uses `\n` for every line it writes, whatever the content uses.
/// - The text ends with the closing marker and a line feed.
pub fn marker_text(sum: &Merge<Option<&[u8]>>) -> Option<Vec<u8>> {
    let sum = sum.simplify();
    if sum.is_resolved() {
        return None;
    }
    let run = marker_run(&sum);
    let mut out = Vec::new();
    push_marker(&mut out, b'<', run, " conflict");
    for (position, term) in sum.terms().iter().enumerate() {
        let (character, label) = if position % 2 == 0 {
            (b'+', "side")
        } else {
            (b'-', "base")
        };
        let number = position / 2 + 1;
        let note = match term {
            None => " (absent)",
            Some(content) if !content.is_empty() && !content.ends_with(b"\n") => {
                " (no newline at end)"
            }
            Some(_) => "",
        };
        push_marker(
            &mut out,
            character,
            run,
            &format!(" {label} {number}{note}"),
        );
        if let Some(content) = term {
            out.extend_from_slice(content);
            if !content.is_empty() && !content.ends_with(b"\n") {
                out.push(b'\n');
            }
        }
    }
    push_marker(&mut out, b'>', run, "");
    Some(out)
}

/// The length of the runs of the marker lines of a sum: seven, or one more than the longest run of
/// a marker character at the start of a line of content.
fn marker_run(sum: &Merge<Option<&[u8]>>) -> usize {
    let longest = sum
        .terms()
        .iter()
        .flatten()
        .flat_map(|content| content.split(|&byte| byte == b'\n'))
        .map(|line| match line.first() {
            Some(first) if b"<+->".contains(first) => {
                line.iter().take_while(|&&byte| byte == *first).count()
            }
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    MIN_MARKER_LEN.max(longest + 1)
}

fn push_marker(out: &mut Vec<u8>, character: u8, run: usize, rest: &str) {
    out.extend(std::iter::repeat_n(character, run));
    out.extend_from_slice(rest.as_bytes());
    out.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::sample_change;
    use crate::object::{Kind, ObjectId};
    use crate::properties::{cases, seed, Rng, DEFAULT_CASES, DEFAULT_SEED};

    fn sum(terms: &[u8]) -> Merge<u8> {
        Merge::try_from_terms(terms.to_vec()).expect("an odd number of terms")
    }

    fn human() -> Actor {
        Actor::human("daniel").expect("valid actor")
    }

    /// A tree ID for a number, without a tree behind it: the sum never reads a tree.
    fn tree(number: u8) -> TreeId {
        TreeId::new(ObjectId::from_digest(blake3::hash(&[number])))
    }

    fn trees(numbers: &[u8]) -> Merge<TreeId> {
        Merge::try_from_terms(numbers.iter().map(|&n| tree(n)).collect()).expect("odd")
    }

    fn from_sum(trees: &Merge<TreeId>) -> AnyCommit {
        AnyCommit::from_sum(trees, vec![], sample_change(), human(), 1_700_000_000, "m")
            .expect("valid commit")
    }

    /// How often each value is added minus how often it is removed: the meaning of the sum, found
    /// without the code under test.
    fn counts(sum: &Merge<u8>) -> [i32; 4] {
        let mut counts = [0; 4];
        for (position, &term) in sum.terms().iter().enumerate() {
            counts[usize::from(term)] += if position % 2 == 0 { 1 } else { -1 };
        }
        counts
    }

    fn some(content: &[u8]) -> Option<&[u8]> {
        Some(content)
    }

    fn text(terms: &[Option<&[u8]>]) -> String {
        let sum = Merge::try_from_terms(terms.to_vec()).expect("odd");
        String::from_utf8(marker_text(&sum).expect("a conflict")).expect("UTF-8")
    }

    #[test]
    fn a_list_of_even_length_is_not_a_sum() {
        assert_eq!(
            Merge::<u8>::try_from_terms(vec![]),
            Err(MergeError::TermCount(0))
        );
        assert_eq!(
            Merge::try_from_terms(vec![1, 2]),
            Err(MergeError::TermCount(2))
        );
        assert_eq!(
            MergeError::TermCount(4).to_string(),
            "a sum lists an odd number of terms, not 4"
        );
    }

    #[test]
    fn sides_and_bases_are_the_even_and_the_odd_terms() {
        let merge = sum(&[1, 2, 3, 2, 1]);
        assert_eq!(merge.sides(), 3);
        assert!(!merge.is_resolved());
        assert_eq!(merge.adds().copied().collect::<Vec<_>>(), [1, 3, 1]);
        assert_eq!(merge.removes().copied().collect::<Vec<_>>(), [2, 2]);
        let resolved = Merge::resolved(7_u8);
        assert!(resolved.is_resolved());
        assert_eq!(resolved.sides(), 1);
    }

    #[test]
    fn equal_terms_cancel() {
        // 1 - 2 + 3 - 3 + 4 loses the pair 3 - 3, and keeps the order of what is left.
        assert_eq!(sum(&[1, 2, 3, 3, 4]).simplify(), sum(&[1, 2, 4]));
        // 1 - 1 + 2 is the value 2.
        assert_eq!(sum(&[1, 1, 2]).simplify(), sum(&[2]));
        assert_eq!(sum(&[1, 1, 2]).resolution(), Some(2));
        assert_eq!(sum(&[1, 2, 3]).resolution(), None);
        // A removed term cancels against the nearest equal side before it: 5 - 5 + 5 is 5.
        assert_eq!(sum(&[5, 5, 5]).simplify(), sum(&[5]));
    }

    #[test]
    fn two_sides_that_agree_do_not_resolve() {
        // The same-change rule is not part of the algebra: 2 - 1 + 2 stays a conflict.
        assert_eq!(sum(&[2, 1, 2]).simplify(), sum(&[2, 1, 2]));
    }

    #[test]
    fn a_second_branch_extends_the_conflict_and_does_not_nest_it() {
        // The prototype of the conflict model: timeout 45 and 60 over the base 30 conflict, and a
        // hotfix with 10 over the same base lands on it.
        let open = sum(&[45, 30, 60]);
        let landed = open.land(30, 10);
        assert_eq!(landed, sum(&[45, 30, 60, 30, 10]));
        assert_eq!(landed.sides(), 3);
    }

    #[test]
    fn a_change_that_undoes_a_side_cancels_it() {
        // Landing the change from 60 to 45 on 45 - 30 + 60 takes both sides to 45.
        let open = sum(&[45, 30, 60]);
        assert_eq!(open.land(60, 45), sum(&[45, 30, 45]));
        // Landing the change from 30 to 45 on 45 - 30 + 60: the base 30 cancels against the added
        // 30 of a change that leaves the conflict, and 60 is the only value left.
        assert_eq!(sum(&[45, 30, 60, 45, 30]).simplify(), sum(&[60]));
    }

    #[test]
    fn padding_adds_pairs_and_changes_no_meaning() {
        let merge = sum(&[1, 2, 3]);
        let padded = merge.padded(4);
        assert_eq!(padded, sum(&[1, 1, 1, 1, 1, 2, 3]));
        assert_eq!(padded.sides(), 4);
        assert!(padded.is_equivalent(&merge));
        assert_eq!(padded.simplify(), merge);
        // A resolved value gets three terms, and a smaller or equal target changes nothing.
        assert_eq!(Merge::resolved(9_u8).padded(2), sum(&[9, 9, 9]));
        assert_eq!(Merge::resolved(9_u8).padded(1), sum(&[9]));
        assert_eq!(merge.padded(2), merge);
        assert_eq!(merge.padded(0), merge);
    }

    #[test]
    fn equivalence_ignores_order_and_cancelling_pairs() {
        assert!(sum(&[1, 2, 3]).is_equivalent(&sum(&[3, 2, 1])));
        assert!(sum(&[1, 2, 3, 4, 4]).is_equivalent(&sum(&[1, 2, 3])));
        assert!(!sum(&[1, 2, 3]).is_equivalent(&sum(&[1, 3, 2])));
        assert!(!sum(&[1, 2, 3]).is_equivalent(&sum(&[1, 2, 3, 2, 3])));
    }

    #[test]
    fn a_sum_of_trees_that_cancels_to_one_tree_is_a_plain_commit() {
        let plain = Commit::try_new(
            tree(2),
            vec![],
            sample_change(),
            human(),
            1_700_000_000,
            "m",
        )
        .unwrap();
        let resolved = from_sum(&trees(&[1, 1, 2]));
        assert_eq!(resolved, AnyCommit::Commit(plain.clone()));
        assert!(!resolved.is_conflicted());
        assert_eq!(resolved.id(), plain.id());
        assert_eq!(resolved.encode(), plain.encode());
        assert_eq!(from_sum(&trees(&[2])), AnyCommit::Commit(plain));
    }

    #[test]
    fn a_sum_with_sides_left_is_a_commit2_in_simplified_form() {
        let commit = from_sum(&trees(&[1, 2, 3, 4, 4]));
        assert!(commit.is_conflicted());
        let AnyCommit::Commit2(inner) = &commit else {
            panic!("a conflict is a commit2");
        };
        assert_eq!(inner.trees(), [tree(1), tree(2), tree(3)]);
        assert_eq!(commit.id(), inner.id());
        assert_eq!(commit.tree_sum(), trees(&[1, 2, 3]));
        assert_eq!(Commit2::parse(&commit.encode()).unwrap(), *inner);
        // The padded form is the same commit.
        assert_eq!(from_sum(&trees(&[1, 2, 3]).padded(4)), commit);
    }

    #[test]
    fn a_duplicate_parent_is_reported_in_both_forms() {
        let parent = from_sum(&trees(&[9])).id();
        for numbers in [&[1_u8][..], &[1, 2, 3]] {
            assert_eq!(
                AnyCommit::from_sum(
                    &trees(numbers),
                    vec![parent, parent],
                    sample_change(),
                    human(),
                    0,
                    "m"
                ),
                Err(if numbers.len() == 1 {
                    FromSumError::Commit(CommitError::DuplicateParent(parent))
                } else {
                    FromSumError::Commit2(Commit2Error::DuplicateParent(parent))
                })
            );
        }
    }

    #[test]
    fn the_error_of_from_sum_prints_the_message_of_its_cause() {
        let parent = from_sum(&trees(&[9])).id();
        let error = FromSumError::Commit2(Commit2Error::DuplicateParent(parent));
        assert_eq!(error.to_string(), format!("duplicate parent {parent}"));
        assert!(error.source().is_none());
        let error = FromSumError::Commit(CommitError::DuplicateParent(parent));
        assert_eq!(error.to_string(), format!("duplicate parent {parent}"));
    }

    #[test]
    fn tree_sum_of_a_commit_is_its_tree() {
        let commit = from_sum(&trees(&[6]));
        assert_eq!(commit.tree_sum(), trees(&[6]));
        assert!(commit.tree_sum().is_resolved());
    }

    #[test]
    fn marker_text_of_two_sides() {
        assert_eq!(
            text(&[
                some(b"timeout=45\n"),
                some(b"timeout=30\n"),
                some(b"timeout=60\n")
            ]),
            "<<<<<<< conflict\n\
             +++++++ side 1\n\
             timeout=45\n\
             ------- base 1\n\
             timeout=30\n\
             +++++++ side 2\n\
             timeout=60\n\
             >>>>>>>\n"
        );
    }

    #[test]
    fn marker_text_of_three_sides() {
        assert_eq!(
            text(&[
                some(b"45\n"),
                some(b"30\n"),
                some(b"60\n"),
                some(b"30\n"),
                some(b"10\n")
            ]),
            "<<<<<<< conflict\n\
             +++++++ side 1\n\
             45\n\
             ------- base 1\n\
             30\n\
             +++++++ side 2\n\
             60\n\
             ------- base 2\n\
             30\n\
             +++++++ side 3\n\
             10\n\
             >>>>>>>\n"
        );
    }

    #[test]
    fn marker_text_of_five_sides() {
        let contents: [&[u8]; 9] = [
            b"a\n", b"b\n", b"c\n", b"d\n", b"e\n", b"f\n", b"g\n", b"h\n", b"i\n",
        ];
        let terms: Vec<Option<&[u8]>> = contents.iter().map(|&c| Some(c)).collect();
        assert_eq!(
            text(&terms),
            "<<<<<<< conflict\n\
             +++++++ side 1\na\n\
             ------- base 1\nb\n\
             +++++++ side 2\nc\n\
             ------- base 2\nd\n\
             +++++++ side 3\ne\n\
             ------- base 3\nf\n\
             +++++++ side 4\ng\n\
             ------- base 4\nh\n\
             +++++++ side 5\ni\n\
             >>>>>>>\n"
        );
    }

    #[test]
    fn marker_text_shows_no_cancelled_pair_and_no_padding() {
        let terms = [some(b"x\n"), some(b"y\n"), some(b"z\n")];
        let plain = text(&terms);
        let padded = Merge::try_from_terms(terms.to_vec()).unwrap().padded(5);
        assert_eq!(marker_text(&padded).unwrap(), plain.as_bytes());
        // 1 - 2 + 3 - 3 + 4 shows two sides: the cancelled pair 3 - 3 is left out.
        let with_pair = [
            some(b"x\n"),
            some(b"y\n"),
            some(b"z\n"),
            some(b"z\n"),
            some(b"w\n"),
        ];
        assert_eq!(
            text(&with_pair),
            "<<<<<<< conflict\n+++++++ side 1\nx\n------- base 1\ny\n+++++++ side 2\nw\n>>>>>>>\n"
        );
    }

    #[test]
    fn marker_text_of_a_sum_that_resolves_is_none() {
        let resolved = Merge::resolved(some(b"one\n"));
        assert_eq!(marker_text(&resolved), None);
        let cancelled = Merge::try_from_terms(vec![some(b"a"), some(b"a"), some(b"b")]).unwrap();
        assert_eq!(marker_text(&cancelled), None);
    }

    #[test]
    fn marker_text_marks_an_absent_term_and_a_missing_line_feed() {
        assert_eq!(
            text(&[None, some(b"base\n"), some(b"no feed")]),
            "<<<<<<< conflict\n\
             +++++++ side 1 (absent)\n\
             ------- base 1\n\
             base\n\
             +++++++ side 2 (no newline at end)\n\
             no feed\n\
             >>>>>>>\n"
        );
    }

    #[test]
    fn marker_text_tells_an_empty_file_from_an_absent_one() {
        let empty = text(&[some(b""), some(b"x\n"), some(b"y\n")]);
        let absent = text(&[None, some(b"x\n"), some(b"y\n")]);
        assert!(empty.starts_with("<<<<<<< conflict\n+++++++ side 1\n------- base 1\n"));
        assert!(absent.starts_with("<<<<<<< conflict\n+++++++ side 1 (absent)\n"));
        assert_ne!(empty, absent);
    }

    #[test]
    fn marker_text_tells_an_empty_file_from_one_empty_line() {
        let empty = text(&[some(b""), some(b"x\n"), some(b"y\n")]);
        let blank = text(&[some(b"\n"), some(b"x\n"), some(b"y\n")]);
        assert!(blank.starts_with("<<<<<<< conflict\n+++++++ side 1\n\n------- base 1\n"));
        assert_ne!(empty, blank);
    }

    #[test]
    fn marker_text_follows_the_order_of_the_terms() {
        // Equivalent sums with the sides in another order show them in that order.
        let one = [some(b"a\n"), some(b"x\n"), some(b"b\n")];
        let other = [some(b"b\n"), some(b"x\n"), some(b"a\n")];
        assert!(Merge::try_from_terms(one.to_vec())
            .unwrap()
            .is_equivalent(&Merge::try_from_terms(other.to_vec()).unwrap()));
        assert_ne!(text(&one), text(&other));
    }

    #[test]
    fn padding_a_sum_that_cancels_in_itself_gives_the_same_result() {
        // The first removed term equals the first side, as the pad pair does.
        let merge = sum(&[1, 1, 2]);
        let padded = merge.padded(4);
        assert_eq!(padded, sum(&[1, 1, 1, 1, 1, 1, 2]));
        assert_eq!(padded.simplify(), merge.simplify());
        let merge = sum(&[1, 1, 1, 2, 1]);
        assert_eq!(merge.padded(6).simplify(), merge.simplify());
    }

    #[test]
    fn marker_text_makes_the_markers_longer_than_any_line_of_content() {
        // A file that documents the markers: its lines start with runs of seven characters.
        let documented: &[u8] = b"<<<<<<< conflict\n>>>>>>>\nfine\n";
        let text = text(&[some(documented), some(b"base\n"), some(b"other\n")]);
        assert!(text.starts_with("<<<<<<<< conflict\n++++++++ side 1\n<<<<<<< conflict\n"));
        assert!(text.ends_with("++++++++ side 2\nother\n>>>>>>>>\n"));
        // A run of six is shorter than the markers and changes nothing.
        let short: &[u8] = b"------\n";
        assert!(self::text(&[some(short), some(b"b\n"), some(b"c\n")])
            .starts_with("<<<<<<< conflict\n+++++++ side 1\n------\n"));
        // Only the start of a line counts.
        let inside: &[u8] = b"a >>>>>>>>>> b\n";
        assert!(self::text(&[some(inside), some(b"b\n"), some(b"c\n")]).ends_with("c\n>>>>>>>\n"));
    }

    #[test]
    fn marker_text_keeps_binary_content_byte_for_byte() {
        let binary: &[u8] = &[0x00, 0xff, b'\r', b'\n', 0x80];
        let shown =
            marker_text(&Merge::try_from_terms(vec![some(binary), None, some(b"x")]).unwrap())
                .unwrap();
        let start = b"<<<<<<< conflict\n+++++++ side 1 (no newline at end)\n".len();
        assert_eq!(&shown[start..start + binary.len()], binary);
    }

    /// A random sum of `sides` values from a small alphabet, so that terms cancel often.
    fn random_sum(rng: &mut Rng) -> Merge<u8> {
        let length = 2 * (1 + rng.below(5)) - 1;
        let terms = (0..length).map(|_| rng.below(4) as u8).collect();
        Merge::try_from_terms(terms).expect("odd")
    }

    fn shuffled<T>(items: &mut [T], rng: &mut Rng) {
        for i in (1..items.len()).rev() {
            items.swap(i, rng.below(i + 1));
        }
    }

    fn rebuilt(adds: &[u8], removes: &[u8]) -> Merge<u8> {
        let mut terms = Vec::new();
        for (i, &add) in adds.iter().enumerate() {
            terms.push(add);
            if let Some(&remove) = removes.get(i) {
                terms.push(remove);
            }
        }
        Merge::try_from_terms(terms).expect("odd")
    }

    #[test]
    fn simplify_keeps_the_meaning_and_the_length_odd() {
        let (seed, mut rng) = (seed(), Rng::new(seed(), 0xC0F1));
        for case in 0..cases() {
            let merge = random_sum(&mut rng);
            let simplified = merge.simplify();
            assert_eq!(
                counts(&simplified),
                counts(&merge),
                "seed {seed}, case {case}"
            );
            assert_eq!(simplified.terms().len() % 2, 1, "seed {seed}, case {case}");
            // Nothing is left to cancel, and a second pass changes nothing.
            assert!(
                simplified
                    .adds()
                    .all(|a| simplified.removes().all(|r| a != r)),
                "seed {seed}, case {case}: {simplified:?}"
            );
            assert_eq!(
                simplified.simplify(),
                simplified,
                "seed {seed}, case {case}"
            );
        }
    }

    #[test]
    fn cancelling_does_not_depend_on_the_order_of_the_terms() {
        let (seed, mut rng) = (seed(), Rng::new(seed(), 0xC0F2));
        for case in 0..cases() {
            let merge = random_sum(&mut rng);
            let mut adds: Vec<u8> = merge.adds().copied().collect();
            let mut removes: Vec<u8> = merge.removes().copied().collect();
            shuffled(&mut adds, &mut rng);
            shuffled(&mut removes, &mut rng);
            let permuted = rebuilt(&adds, &removes);
            assert_eq!(
                counts(&permuted),
                counts(&merge),
                "seed {seed}, case {case}"
            );
            assert!(
                permuted.is_equivalent(&merge),
                "seed {seed}, case {case}: {merge:?} and {permuted:?}"
            );
            // What is left is the same multiset, whichever term cancelled first.
            let (mut left, mut right) = (
                permuted.simplify().into_terms(),
                merge.simplify().into_terms(),
            );
            let (mut left_adds, mut right_adds): (Vec<_>, Vec<_>) = (
                left.iter().copied().step_by(2).collect(),
                right.iter().copied().step_by(2).collect(),
            );
            left_adds.sort_unstable();
            right_adds.sort_unstable();
            assert_eq!(left_adds, right_adds, "seed {seed}, case {case}");
            left.clear();
            right.clear();
        }
    }

    #[test]
    fn padding_keeps_the_meaning_the_marker_text_and_the_parity() {
        let (seed, mut rng) = (seed(), Rng::new(seed(), 0xC0F3));
        for case in 0..cases() {
            let merge = random_sum(&mut rng);
            let target = 1 + rng.below(8);
            let padded = merge.padded(target);
            assert_eq!(padded.terms().len() % 2, 1, "seed {seed}, case {case}");
            assert_eq!(
                padded.sides(),
                merge.sides().max(target),
                "seed {seed}, case {case}"
            );
            assert_eq!(counts(&padded), counts(&merge), "seed {seed}, case {case}");
            assert!(padded.is_equivalent(&merge), "seed {seed}, case {case}");
            // The same sum of file contents shows the same text, padded or not.
            let contents: [&[u8]; 4] = [b"a\n", b"b\n", b"c\n", b"d\n"];
            let shown = |sum: &Merge<u8>| {
                marker_text(
                    &Merge::try_from_terms(
                        sum.terms()
                            .iter()
                            .map(|&t| Some(contents[usize::from(t)]))
                            .collect(),
                    )
                    .unwrap(),
                )
            };
            assert_eq!(shown(&padded), shown(&merge), "seed {seed}, case {case}");
        }
    }

    #[test]
    fn landing_adds_the_change_and_keeps_the_length_odd() {
        let (seed, mut rng) = (seed(), Rng::new(seed(), 0xC0F4));
        for case in 0..cases() {
            let merge = random_sum(&mut rng);
            let (base, side) = (rng.below(4) as u8, rng.below(4) as u8);
            let landed = merge.land(base, side);
            let mut expected = counts(&merge);
            expected[usize::from(side)] += 1;
            expected[usize::from(base)] -= 1;
            assert_eq!(counts(&landed), expected, "seed {seed}, case {case}");
            assert_eq!(landed.terms().len() % 2, 1, "seed {seed}, case {case}");
            // Landing never nests: the result is already simplified.
            assert_eq!(landed.simplify(), landed, "seed {seed}, case {case}");
        }
    }

    #[test]
    fn the_form_of_a_commit_follows_the_simplified_sum() {
        let (seed, mut rng) = (seed(), Rng::new(seed(), 0xC0F5));
        let (mut plain, mut conflicted) = (0, 0);
        for case in 0..cases() {
            let merge = random_sum(&mut rng);
            let numbers: Vec<u8> = merge.terms().to_vec();
            let sum_of_trees = trees(&numbers);
            let commit = from_sum(&sum_of_trees);
            let simplified = sum_of_trees.simplify();
            if simplified.is_resolved() {
                plain += 1;
                let AnyCommit::Commit(inner) = &commit else {
                    panic!("seed {seed}, case {case}: a resolved sum is a commit");
                };
                assert_eq!(
                    inner.tree(),
                    simplified.terms()[0],
                    "seed {seed}, case {case}"
                );
                assert!(!commit.is_conflicted());
            } else {
                conflicted += 1;
                let AnyCommit::Commit2(inner) = &commit else {
                    panic!("seed {seed}, case {case}: an open conflict is a commit2");
                };
                assert_eq!(
                    inner.trees(),
                    simplified.terms(),
                    "seed {seed}, case {case}"
                );
                assert!(inner.trees().len() >= 3 && inner.trees().len() % 2 == 1);
            }
            // The tree sum that a reader finds means what the sum meant, and the ID does not
            // depend on how many cancelling pairs the caller added.
            assert!(
                commit.tree_sum().is_equivalent(&sum_of_trees),
                "seed {seed}, case {case}"
            );
            assert_eq!(
                from_sum(&sum_of_trees.padded(1 + rng.below(8))).id(),
                commit.id(),
                "seed {seed}, case {case}"
            );
        }
        // The generator reaches both forms, so the test cannot pass on one of them only. Checked
        // for the default seed and at least the default number of cases.
        if seed == DEFAULT_SEED && cases() >= DEFAULT_CASES {
            assert!(
                plain > 0 && conflicted > 0,
                "plain {plain}, conflicted {conflicted}"
            );
        }
    }

    #[test]
    fn the_kind_of_the_two_forms_differs_for_the_same_trees() {
        // Two commits with the same body but not the same kind are different objects.
        let conflicted = from_sum(&trees(&[1, 2, 3]));
        assert_eq!(
            conflicted.id(),
            Commit2::parse(&conflicted.encode()).unwrap().id()
        );
        assert_ne!(Kind::Commit, Kind::Commit2);
    }
}
