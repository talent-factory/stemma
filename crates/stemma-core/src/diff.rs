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
/// The search runs from the start (forward) and from the end (backward) at the same time. Round
/// `d` extends the furthest reaching paths of `d` edits in both directions. The paths meet on a
/// snake of a shortest script. The snake is returned as `(x, y, u, v)`: it runs from `(x, y)` to
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
    // The forward diagonal `k` and the backward diagonal `c` describe the same cell when
    // `k = delta - c`. An odd `delta` makes them meet in a forward round, an even one in a
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
