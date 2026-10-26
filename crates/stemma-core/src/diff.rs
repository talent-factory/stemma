//! The Myers diff (E. W. Myers, 1986): a shortest edit script between two sequences.
//!
//! [`diff`] compares an old and a new sequence of elements, usually the lines of two files, and
//! returns the edit script that turns the old one into the new one with the fewest insertions and
//! deletions. The script is a list of [`Edit`]s in order. It is used for display only (ADR-004,
//! level 1); the merge path does not call it in Phase 2.
//!
//! The algorithm is the greedy forward search of the paper. It walks the edit graph diagonal by
//! diagonal, and for each number of edits `d` it keeps the furthest reaching point on every
//! diagonal `k`. The edit script is rebuilt by walking back through the saved furthest points.
//!
//! Time is O((N+M)·D) for sequences of length N and M and a shortest script of D edits. Space is
//! O((N+M)·D), because every step keeps a copy of the furthest points. The linear-space variant of
//! the paper is not implemented; for the files of Phase 2 the saved copies are the limit.

/// One step of an edit script.
///
/// The indices refer to the old sequence (`old`) and the new sequence (`new`). Applied in order,
/// the steps consume every element of both sequences exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// The element at `old` is equal to the element at `new`; it is kept.
    Equal { old: usize, new: usize },
    /// The element at `old` does not occur in the new sequence at this place; it is removed.
    Delete { old: usize },
    /// The element at `new` does not occur in the old sequence at this place; it is inserted.
    Insert { new: usize },
}

/// Returns a shortest edit script that turns `old` into `new`.
///
/// The script has the fewest deletions and insertions that any edit script can have. Both sequences
/// may be empty.
pub fn diff<T: PartialEq>(old: &[T], new: &[T]) -> Vec<Edit> {
    let n = old.len() as isize;
    let m = new.len() as isize;
    let max = n + m;
    // `v[k + offset]` is the furthest `x` reached on diagonal `k = x - y`. The offset keeps the
    // index of `k + 1` in range for every `k` in `-max..=max`.
    let offset = max + 1;
    let index = |k: isize| (k + offset) as usize;

    let mut v = vec![0isize; (2 * max + 3) as usize];
    // `trace[d]` holds `v` as it was before the round `d`, which is what backtracking reads.
    let mut trace: Vec<Vec<isize>> = Vec::new();

    for d in 0..=max {
        trace.push(v.clone());
        for k in (-d..=d).step_by(2) {
            // Move down (an insertion) from diagonal `k + 1`, or right (a deletion) from `k - 1`.
            // Both moves come from the furthest point of the previous round.
            let mut x = if k == -d || (k != d && v[index(k - 1)] < v[index(k + 1)]) {
                v[index(k + 1)]
            } else {
                v[index(k - 1)] + 1
            };
            let mut y = x - k;
            // Follow the snake of equal elements as far as it goes.
            while x < n && y < m && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            v[index(k)] = x;
            if x >= n && y >= m {
                return backtrack(&trace, n, m);
            }
        }
    }
    unreachable!("the edit graph always reaches the end at d = N + M")
}

/// Rebuilds the edit script from the saved furthest points, from the end to the start.
fn backtrack(trace: &[Vec<isize>], n: isize, m: isize) -> Vec<Edit> {
    let max = n + m;
    let offset = max + 1;
    let index = |k: isize| (k + offset) as usize;

    let (mut x, mut y) = (n, m);
    let mut edits = Vec::new();
    for (d, v) in trace.iter().enumerate().rev() {
        let d = d as isize;
        let k = x - y;
        // The previous round ended on diagonal `prev_k`, from which this round moved.
        let prev_k = if k == -d || (k != d && v[index(k - 1)] < v[index(k + 1)]) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = v[index(prev_k)];
        let prev_y = prev_x - prev_k;

        // The snake of equal elements after the move, walked backwards.
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            edits.push(Edit::Equal {
                old: x as usize,
                new: y as usize,
            });
        }
        // The move itself. Round 0 has no move, only the snake from the origin.
        if d > 0 {
            if x == prev_x {
                // The move went down: `new[prev_y]` was inserted.
                edits.push(Edit::Insert {
                    new: prev_y as usize,
                });
            } else {
                // The move went right: `old[prev_x]` was deleted.
                edits.push(Edit::Delete {
                    old: prev_x as usize,
                });
            }
        }
        x = prev_x;
        y = prev_y;
    }
    edits.reverse();
    edits
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
        // Myers (1986), figure 2: ABCABBA against CBABAC, a shortest script of D = 5.
        let old: Vec<char> = "ABCABBA".chars().collect();
        let new: Vec<char> = "CBABAC".chars().collect();
        check(&old, &new, 5);
    }

    #[test]
    fn lines_of_a_file_are_compared_as_elements() {
        let old = ["fn main() {", "    one();", "}"];
        let new = ["fn main() {", "    two();", "    three();", "}"];
        check(&old, &new, 3);
    }
}
