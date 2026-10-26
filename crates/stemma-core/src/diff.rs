//! The Myers diff (E. W. Myers, 1986): a shortest edit script between two sequences.
//!
//! [`diff`] compares an old and a new sequence of elements, usually the lines of two files, and
//! returns the edit script that turns the old one into the new one with the fewest insertions and
//! deletions. The script is a list of [`Edit`]s in order. It is used for display only (ADR-004,
//! level 1). The merge path does not call it: diff computation in the merge path is level 2 under
//! ADR-004.
//!
//! The algorithm is the linear-space variant of the paper. Common prefixes and suffixes are cut
//! off first. Then a search from both ends finds a middle snake, a diagonal run of equal elements
//! that lies on a shortest script. The two parts on either side of it are compared the same way.
//!
//! Time is O((N+M)·D) for sequences of length N and M and a shortest script of D edits. Space is
//! O(N+M): the search keeps two arrays of furthest points, and the recursion keeps no copy of them.

/// One step of an edit script.
///
/// The indices refer to the old sequence (`old`) and the new sequence (`new`). Applied in order,
/// the steps consume every element of both sequences exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// The element at `old` is equal to the element at `new`; it is kept.
    Equal { old: usize, new: usize },
    /// The element at `old` is not matched by an element of the new sequence; the script removes it.
    Delete { old: usize },
    /// The element at `new` is not matched by an element of the old sequence; the script inserts it.
    Insert { new: usize },
}

/// Returns a shortest edit script that turns `old` into `new`.
///
/// The script has the fewest deletions and insertions that any edit script can have. Both sequences
/// may be empty.
pub fn diff<T: PartialEq>(old: &[T], new: &[T]) -> Vec<Edit> {
    let mut edits = Vec::new();
    align(old, new, 0, 0, &mut edits);
    edits
}

/// Appends the edit script for `old` and `new` to `edits`. `old_base` and `new_base` are the
/// positions of the two slices in the whole sequences, so that the steps carry whole indices.
fn align<T: PartialEq>(
    old: &[T],
    new: &[T],
    old_base: usize,
    new_base: usize,
    edits: &mut Vec<Edit>,
) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    for i in 0..prefix {
        edits.push(Edit::Equal {
            old: old_base + i,
            new: new_base + i,
        });
    }

    let a = &old[prefix..old.len() - suffix];
    let b = &new[prefix..new.len() - suffix];
    let (a_base, b_base) = (old_base + prefix, new_base + prefix);
    if a.is_empty() {
        edits.extend((0..b.len()).map(|j| Edit::Insert { new: b_base + j }));
    } else if b.is_empty() {
        edits.extend((0..a.len()).map(|i| Edit::Delete { old: a_base + i }));
    } else {
        let (x, y, u, v) = middle_snake(a, b);
        align(&a[..x], &b[..y], a_base, b_base, edits);
        edits.extend((0..u - x).map(|t| Edit::Equal {
            old: a_base + x + t,
            new: b_base + y + t,
        }));
        align(&a[u..], &b[v..], a_base + u, b_base + v, edits);
    }

    for i in 0..suffix {
        edits.push(Edit::Equal {
            old: old_base + old.len() - suffix + i,
            new: new_base + new.len() - suffix + i,
        });
    }
}

/// Finds a middle snake of a shortest edit script for `a` and `b`, both non-empty.
///
/// The search alternates: round `d` extends the forward paths of `d` edits from the start, then the
/// backward paths from the end, and each half checks for an overlap with the other side. The
/// paths meet on a snake of a shortest script. The snake is returned as `(x, y, u, v)`: it runs from `(x, y)` to
/// `(u, v)` in the edit graph of `a` and `b`.
fn middle_snake<T: PartialEq>(a: &[T], b: &[T]) -> (usize, usize, usize, usize) {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let max = n + m;
    // `vf[k + offset]` is the furthest `x` reached forward on diagonal `k = x - y`, and `vb` the
    // furthest `x` reached backward, counted from the end, on diagonal `c` of the reversed
    // sequences. The offset keeps every `k ± 1` in range.
    let offset = max + 1;
    let index = |k: isize| (k + offset) as usize;
    let mut vf = vec![0isize; (2 * max + 3) as usize];
    let mut vb = vec![0isize; (2 * max + 3) as usize];
    // The forward diagonal `k` and the backward diagonal `c` are the same line of the edit graph
    // when `k = delta - c`, because the reversed coordinates map `x` to `n - x` and `y` to `m - y`.
    // An odd `delta` therefore makes the paths overlap in a forward round, an even one in a
    // backward round.
    let delta = n - m;
    let odd = delta % 2 != 0;

    for d in 0..=(max + 1) / 2 {
        for k in (-d..=d).step_by(2) {
            let mut x = if k == -d || (k != d && vf[index(k - 1)] < vf[index(k + 1)]) {
                vf[index(k + 1)]
            } else {
                vf[index(k - 1)] + 1
            };
            let mut y = x - k;
            let (x_start, y_start) = (x, y);
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            vf[index(k)] = x;
            // The backward paths of round `d - 1` cover the diagonals with `|c| <= d - 1`.
            let c = delta - k;
            if odd && c.abs() < d && x >= n - vb[index(c)] {
                return (x_start as usize, y_start as usize, x as usize, y as usize);
            }
        }
        for c in (-d..=d).step_by(2) {
            let mut x = if c == -d || (c != d && vb[index(c - 1)] < vb[index(c + 1)]) {
                vb[index(c + 1)]
            } else {
                vb[index(c - 1)] + 1
            };
            let mut y = x - c;
            let (x_start, y_start) = (x, y);
            while x < n && y < m && a[(n - 1 - x) as usize] == b[(m - 1 - y) as usize] {
                x += 1;
                y += 1;
            }
            vb[index(c)] = x;
            // The forward paths of round `d` cover the diagonals with `|k| <= d`.
            let k = delta - c;
            if !odd && k.abs() <= d && vf[index(k)] >= n - x {
                // Counted from the start, the backward snake runs from `(n - x, m - y)` to
                // `(n - x_start, m - y_start)`.
                return (
                    (n - x) as usize,
                    (m - y) as usize,
                    (n - x_start) as usize,
                    (m - y_start) as usize,
                );
            }
        }
    }
    unreachable!("the two searches meet within (N+M+1)/2 rounds")
}

/// A group of changes together with the unchanged lines around it.
///
/// The hunk is a part of an edit script. Its steps are kept in `edits`, the unchanged lines before
/// and after the changes included. The positions give the first line and the number of lines of
/// the hunk on both sides, counted from zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The zero-based index of the first line of the hunk in the old sequence.
    pub old_start: usize,
    /// The number of lines of the hunk in the old sequence.
    pub old_len: usize,
    /// The zero-based index of the first line of the hunk in the new sequence.
    pub new_start: usize,
    /// The number of lines of the hunk in the new sequence.
    pub new_len: usize,
    /// The steps of the hunk in order, context lines included.
    pub edits: Vec<Edit>,
}

/// Groups an edit script into hunks with `context` unchanged lines on either side of each change.
///
/// Two neighbouring changes end up in the same hunk when at most `2 * context` unchanged lines lie
/// between them, because their context lines then overlap or touch. Such neighbours chain: a hunk
/// may span changes that are further apart than that, when every gap between neighbours is small
/// enough. At the start or end of the script a hunk has fewer context lines. The hunks follow the
/// order of the script. A script without any change gives no hunk.
pub fn hunks(edits: &[Edit], context: usize) -> Vec<Hunk> {
    // The windows of the steps to keep, end exclusive. A window of a change reaches `context`
    // steps to either side, and touching or overlapping windows are merged.
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        if matches!(edit, Edit::Equal { .. }) {
            continue;
        }
        let start = i.saturating_sub(context);
        let end = (i + context + 1).min(edits.len());
        match windows.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => windows.push((start, end)),
        }
    }

    // The number of old and new lines that the steps before each position consume.
    let mut old_before = Vec::with_capacity(edits.len() + 1);
    let mut new_before = Vec::with_capacity(edits.len() + 1);
    let (mut old_count, mut new_count) = (0, 0);
    for edit in edits {
        old_before.push(old_count);
        new_before.push(new_count);
        match edit {
            Edit::Equal { .. } => {
                old_count += 1;
                new_count += 1;
            }
            Edit::Delete { .. } => old_count += 1,
            Edit::Insert { .. } => new_count += 1,
        }
    }
    old_before.push(old_count);
    new_before.push(new_count);

    windows
        .into_iter()
        .map(|(start, end)| Hunk {
            old_start: old_before[start],
            old_len: old_before[end] - old_before[start],
            new_start: new_before[start],
            new_len: new_before[end] - new_before[start],
            edits: edits[start..end].to_vec(),
        })
        .collect()
}

/// The number of leading bytes that [`is_binary`] searches for a NUL byte. Git uses the same
/// length.
pub const BINARY_PROBE_LEN: usize = 8000;

/// Tells whether a file is binary: a NUL byte among its first [`BINARY_PROBE_LEN`] bytes makes it
/// binary, as in git. Later bytes do not count, so the check reads only the start of the file.
pub fn is_binary(content: &[u8]) -> bool {
    let probe = &content[..content.len().min(BINARY_PROBE_LEN)];
    probe.contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_file_is_not_binary() {
        assert!(!is_binary(b""));
        assert!(!is_binary("fn main() {}\n".as_bytes()));
    }

    #[test]
    fn a_nul_byte_at_the_start_makes_a_file_binary() {
        assert!(is_binary(b"\0abc"));
        assert!(is_binary(b"ab\0c"));
    }

    #[test]
    fn only_the_first_probe_bytes_are_searched() {
        let mut inside = vec![b'a'; BINARY_PROBE_LEN];
        inside[BINARY_PROBE_LEN - 1] = 0;
        assert!(is_binary(&inside), "the last probed byte is a NUL");

        let mut outside = vec![b'a'; BINARY_PROBE_LEN + 1];
        outside[BINARY_PROBE_LEN] = 0;
        assert!(!is_binary(&outside), "the NUL lies after the probe");
    }

    /// Checks that `edits` turns `old` into `new`: the equal steps match, every element of both
    /// sequences appears once and in order, and the number of changes is `expected`.
    fn check<T: PartialEq + std::fmt::Debug>(old: &[T], new: &[T], expected: usize) {
        let edits = diff(old, new);
        let mut o = 0;
        let mut n = 0;
        let mut changes = 0;
        for edit in &edits {
            match *edit {
                Edit::Equal { old: i, new: j } => {
                    assert_eq!((i, j), (o, n), "equal step out of order");
                    assert_eq!(old[i], new[j], "equal step between different elements");
                    o += 1;
                    n += 1;
                }
                Edit::Delete { old: i } => {
                    assert_eq!(i, o, "deletion out of order");
                    o += 1;
                    changes += 1;
                }
                Edit::Insert { new: j } => {
                    assert_eq!(j, n, "insertion out of order");
                    n += 1;
                    changes += 1;
                }
            }
        }
        assert_eq!(
            (o, n),
            (old.len(), new.len()),
            "the script does not consume both sides"
        );
        assert_eq!(
            changes, expected,
            "the script is not a shortest one: {edits:?}"
        );
    }

    #[test]
    fn empty_sides_give_an_empty_script() {
        check::<char>(&[], &[], 0);
    }

    #[test]
    fn identical_sides_are_all_equal() {
        let edits = diff(&["a", "b", "c"], &["a", "b", "c"]);
        assert_eq!(
            edits,
            vec![
                Edit::Equal { old: 0, new: 0 },
                Edit::Equal { old: 1, new: 1 },
                Edit::Equal { old: 2, new: 2 },
            ]
        );
    }

    #[test]
    fn pure_insertion_into_an_empty_old_side() {
        check::<char>(&[], &['a', 'b'], 2);
    }

    #[test]
    fn pure_deletion_to_an_empty_new_side() {
        check::<char>(&['a', 'b', 'c'], &[], 3);
    }

    #[test]
    fn insertion_in_the_middle() {
        check(&['a', 'c'], &['a', 'b', 'c'], 1);
    }

    #[test]
    fn deletion_at_the_start() {
        check(&['x', 'a', 'b'], &['a', 'b'], 1);
    }

    #[test]
    fn no_common_element_replaces_everything() {
        check(&['a', 'b', 'c'], &['x', 'y', 'z'], 6);
    }

    #[test]
    fn repeated_elements_are_matched_with_the_fewest_changes() {
        check(&['a', 'a', 'a', 'a'], &['a', 'a'], 2);
    }

    #[test]
    fn the_example_of_the_paper_has_distance_five() {
        // The example of Myers (1986): ABCABBA against CBABAC, a shortest script of D = 5.
        let old: Vec<char> = "ABCABBA".chars().collect();
        let new: Vec<char> = "CBABAC".chars().collect();
        check(&old, &new, 5);
    }

    /// Length of a longest common subsequence, by dynamic programming.
    fn lcs_len<T: PartialEq>(a: &[T], b: &[T]) -> usize {
        let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                table[i][j] = if a[i - 1] == b[j - 1] {
                    table[i - 1][j - 1] + 1
                } else {
                    table[i - 1][j].max(table[i][j - 1])
                };
            }
        }
        table[a.len()][b.len()]
    }

    #[test]
    fn every_pair_of_short_sequences_has_the_lcs_distance() {
        // All sequences over {a, b} of length up to 5. A shortest script has N + M - 2·LCS changes.
        let all: Vec<Vec<char>> = (0..=5u32)
            .flat_map(|len| {
                (0..1u32 << len).map(move |bits| {
                    (0..len)
                        .map(|i| if bits >> i & 1 == 1 { 'b' } else { 'a' })
                        .collect()
                })
            })
            .collect();
        for old in &all {
            for new in &all {
                check(old, new, old.len() + new.len() - 2 * lcs_len(old, new));
            }
        }
    }

    #[test]
    fn lines_of_a_file_are_compared_as_elements() {
        let old = ["fn main() {", "    one();", "}"];
        let new = ["fn main() {", "    two();", "    three();", "}"];
        check(&old, &new, 3);
    }

    #[test]
    fn large_disjoint_sides_keep_every_change() {
        // Two files of 2000 lines with no line in common: the script deletes all of the old
        // lines and inserts all of the new ones.
        let old: Vec<u32> = (0..2000).collect();
        let new: Vec<u32> = (2000..4000).collect();
        check(&old, &new, 4000);
    }

    #[test]
    fn a_common_prefix_is_kept_and_the_rest_is_inserted() {
        check(&['a', 'b'], &['a', 'b', 'c', 'd'], 2);
    }

    #[test]
    fn a_common_suffix_is_kept_and_the_rest_is_inserted() {
        check(&['c', 'd'], &['a', 'b', 'c', 'd'], 2);
    }

    #[test]
    fn a_short_sequence_against_a_much_longer_one() {
        // The element 7 is the only one in common: 1 + 501 - 2 changes.
        let long: Vec<u32> = (0..501).collect();
        check(&[7u32], &long, 500);
    }

    #[test]
    fn many_short_matches_at_several_levels() {
        // Every second element matches, so the LCS is 500 and the script has 1000 changes.
        let old: Vec<u32> = (0..1000).collect();
        let new: Vec<u32> = (0..1000)
            .map(|i| if i % 2 == 0 { i } else { i + 5000 })
            .collect();
        check(&old, &new, 1000);
    }

    /// The sequence `0..len` with the elements at `changed` replaced by values that are not in it.
    fn replaced(len: u32, changed: &[u32]) -> Vec<u32> {
        (0..len)
            .map(|i| if changed.contains(&i) { i + 1000 } else { i })
            .collect()
    }

    #[test]
    fn an_unchanged_script_gives_no_hunk() {
        let old = [1, 2, 3];
        assert!(hunks(&diff(&old, &old), 3).is_empty());
    }

    #[test]
    fn a_change_at_the_start_is_clipped_at_the_first_line() {
        let old: Vec<u32> = (0..10).collect();
        let new = replaced(10, &[0]);
        let found = hunks(&diff(&old, &new), 2);
        assert_eq!(found.len(), 1);
        let hunk = &found[0];
        assert_eq!((hunk.old_start, hunk.old_len), (0, 3));
        assert_eq!((hunk.new_start, hunk.new_len), (0, 3));
        assert_eq!(hunk.edits.len(), 4);
    }

    #[test]
    fn a_change_at_the_end_is_clipped_at_the_last_line() {
        let old: Vec<u32> = (0..10).collect();
        let new = replaced(10, &[9]);
        let found = hunks(&diff(&old, &new), 2);
        assert_eq!(found.len(), 1);
        let hunk = &found[0];
        assert_eq!((hunk.old_start, hunk.old_len), (7, 3));
        assert_eq!((hunk.new_start, hunk.new_len), (7, 3));
        assert_eq!(hunk.old_start + hunk.old_len, old.len());
    }

    #[test]
    fn changes_at_the_merge_boundary_are_joined_only_up_to_twice_the_context() {
        // Two changes with eight unchanged lines between them: exactly 2 · 4.
        let old: Vec<u32> = (0..30).collect();
        let edits = diff(&old, &replaced(30, &[5, 14]));
        assert_eq!(
            hunks(&edits, 4).len(),
            1,
            "eight lines, context 4: one hunk"
        );
        assert_eq!(
            hunks(&edits, 3).len(),
            2,
            "eight lines, context 3: two hunks"
        );

        // Nine unchanged lines between the changes, which is 2 · 4 + 1.
        let edits = diff(&old, &replaced(30, &[5, 15]));
        assert_eq!(
            hunks(&edits, 4).len(),
            2,
            "nine lines, context 4: two hunks"
        );
        assert_eq!(hunks(&edits, 5).len(), 1, "nine lines, context 5: one hunk");
    }

    #[test]
    fn without_context_each_change_is_its_own_hunk_unless_they_touch() {
        // Changes at old lines 4 and 6 are separated by one unchanged line.
        let old: Vec<u32> = (0..10).collect();
        let found = hunks(&diff(&old, &replaced(10, &[4, 6])), 0);
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].old_start, found[0].old_len), (4, 1));
        assert_eq!((found[0].new_start, found[0].new_len), (4, 1));
        assert_eq!((found[1].old_start, found[1].old_len), (6, 1));
        assert_eq!((found[1].new_start, found[1].new_len), (6, 1));
    }

    #[test]
    fn a_hunk_with_more_old_lines_than_new_keeps_both_counts() {
        // Old side 'a x b c', new side 'a b c': the hunk counts three old and two new lines.
        let old = ['a', 'x', 'b', 'c'];
        let new = ['a', 'b', 'c'];
        let found = hunks(&diff(&old, &new), 1);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].old_start, found[0].old_len), (0, 3));
        assert_eq!((found[0].new_start, found[0].new_len), (0, 2));
    }

    #[test]
    fn a_hunk_with_more_new_lines_than_old_keeps_both_counts() {
        // Old side 'a b c', new side 'a b x c': the insert is the only change.
        let old = ['a', 'b', 'c'];
        let new = ['a', 'b', 'x', 'c'];
        let found = hunks(&diff(&old, &new), 1);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].old_start, found[0].old_len), (1, 2));
        assert_eq!((found[0].new_start, found[0].new_len), (1, 3));
    }

    #[test]
    fn a_hunk_after_an_insert_starts_at_the_new_line_not_the_old() {
        // A script with the insert before the delete, built by hand. The second hunk starts
        // after one old and two new lines, so using the old count for the new side is caught.
        let edits = [
            Edit::Insert { new: 0 },
            Edit::Equal { old: 0, new: 1 },
            Edit::Insert { new: 2 },
            Edit::Delete { old: 1 },
            Edit::Equal { old: 2, new: 3 },
        ];
        let found = hunks(&edits, 0);
        assert_eq!(found.len(), 2);
        assert_eq!(
            (
                found[0].old_start,
                found[0].old_len,
                found[0].new_start,
                found[0].new_len
            ),
            (0, 0, 0, 1)
        );
        assert_eq!(
            (
                found[1].old_start,
                found[1].old_len,
                found[1].new_start,
                found[1].new_len
            ),
            (1, 1, 2, 1)
        );
    }

    #[test]
    fn a_hunk_counts_the_lines_of_both_sides() {
        let old: Vec<u32> = (0..10).collect();
        let new = replaced(10, &[4]);
        let found = hunks(&diff(&old, &new), 3);
        assert_eq!(found.len(), 1);
        let hunk = &found[0];
        let old_lines = hunk
            .edits
            .iter()
            .filter(|e| !matches!(e, Edit::Insert { .. }))
            .count();
        let new_lines = hunk
            .edits
            .iter()
            .filter(|e| !matches!(e, Edit::Delete { .. }))
            .count();
        assert_eq!((hunk.old_len, hunk.new_len), (old_lines, new_lines));
    }
}
