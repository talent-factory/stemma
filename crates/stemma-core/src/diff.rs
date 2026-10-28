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
    /// The element at `old` is not matched by an element of the new sequence; the script removes
    /// it.
    Delete { old: usize },
    /// The element at `new` is not matched by an element of the old sequence; the script inserts
    /// it.
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

/// Returns the edit script for two sequences of lines, with the boundaries of the change groups
/// moved by the indent heuristic (see [`slide`]).
///
/// The indentation of a line is the number of bytes of whitespace that begin it, and a line of
/// whitespace only is blank. Git counts a tab differently, so the heuristic can differ from git
/// where tabs are used for indentation.
pub fn diff_lines(old: &[&str], new: &[&str]) -> Vec<Edit> {
    slide(old, new, &diff(old, new), line_indent)
}

/// The indentation of a line, or `None` for a blank line.
fn line_indent(line: &&str) -> Option<usize> {
    if line.trim().is_empty() {
        None
    } else {
        Some(line.len() - line.trim_start().len())
    }
}

/// Returns the edit script for two sequences of lines kept as bytes, with the boundaries of the
/// change groups moved by the indent heuristic of [`diff_lines`].
///
/// Whitespace is the ASCII whitespace, so a vertical tab and a Unicode space such as the no-break
/// space do not count, unlike in [`diff_lines`]. A byte that is not UTF-8 takes part in the comparison like
/// any other byte. Within a group of changes the script lists the deletions before the insertions.
pub fn diff_byte_lines(old: &[&[u8]], new: &[&[u8]]) -> Vec<Edit> {
    slide(old, new, &diff(old, new), byte_line_indent)
}

/// The indentation of a line of bytes, or `None` for a blank line.
fn byte_line_indent(line: &&[u8]) -> Option<usize> {
    let indent = line
        .iter()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    if indent == line.len() {
        None
    } else {
        Some(indent)
    }
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
/// paths meet on a snake of a shortest script. The snake is returned as `(x, y, u, v)`: it runs
/// from `(x, y)` to `(u, v)` in the edit graph of `a` and `b`.
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

/// Moves the boundaries of the change groups, modelled on git's diff post-processing.
///
/// A group of changes slides up or down along the unchanged lines that equal its own first or last
/// line. Sliding keeps the number of changes. Among the positions a group can take, the indent
/// heuristic chooses the one that reads best. `indent` gives the indentation of a line, or `None`
/// for a blank line. [`diff_lines`] and [`diff_byte_lines`] call it; [`diff`] itself does not.
///
/// A replacement, a run of changes with both deletions and insertions, stays where the diff put
/// it. Its two sides already line up, and sliding one side would split the replacement. A pure
/// deletion or insertion never slides next to a replacement, so the two do not merge. Git aligns
/// the groups of both sides in a more detailed way; this is a simpler rule.
pub fn slide<T: PartialEq, F: Fn(&T) -> Option<usize>>(
    old: &[T],
    new: &[T],
    edits: &[Edit],
    indent: F,
) -> Vec<Edit> {
    let mut changed_old = vec![false; old.len()];
    let mut changed_new = vec![false; new.len()];
    for edit in edits {
        match *edit {
            Edit::Equal { .. } => {}
            Edit::Delete { old: i } => changed_old[i] = true,
            Edit::Insert { new: j } => changed_new[j] = true,
        }
    }
    let (frozen_old, frozen_new) = replacements(edits, old.len(), new.len());
    slide_side(old, &mut changed_old, &frozen_old, &indent);
    slide_side(new, &mut changed_new, &frozen_new, &indent);
    rebuild(&changed_old, &changed_new)
}

/// Marks the lines of the replacements in `edits`, one flag per line of each side.
fn replacements(edits: &[Edit], old_len: usize, new_len: usize) -> (Vec<bool>, Vec<bool>) {
    let mut frozen_old = vec![false; old_len];
    let mut frozen_new = vec![false; new_len];
    let mut start = 0;
    while start < edits.len() {
        // A run of changes ends at the next unchanged step, which is skipped.
        let end = edits[start..]
            .iter()
            .position(|edit| matches!(edit, Edit::Equal { .. }))
            .map_or(edits.len(), |offset| start + offset);
        let run = &edits[start..end];
        let has_delete = run.iter().any(|edit| matches!(edit, Edit::Delete { .. }));
        let has_insert = run.iter().any(|edit| matches!(edit, Edit::Insert { .. }));
        if has_delete && has_insert {
            for edit in run {
                match *edit {
                    Edit::Delete { old } => frozen_old[old] = true,
                    Edit::Insert { new } => frozen_new[new] = true,
                    Edit::Equal { .. } => {}
                }
            }
        }
        start = end + 1;
    }
    (frozen_old, frozen_new)
}

// The weights of the indent heuristic follow git's xdiff (xdiffi.c). The tests in
// `tests/diff_against_git.rs` need git on the PATH. They compare `diff_lines` with `git diff` on
// random files and agree wherever the plain diffs agree, replacements included. `MAX_BLANKS`
// limits the scan over blank lines. `MAX_SLIDING` limits the search window to that many positions
// above the lowest one, and as in git the window is also no larger than the size of the group
// plus one.
const MAX_BLANKS: usize = 20;
const MAX_SLIDING: usize = 100;
const INDENT_WEIGHT: i64 = 60;
const START_OF_FILE_PENALTY: i64 = 1;
const END_OF_FILE_PENALTY: i64 = 21;
const TOTAL_BLANK_WEIGHT: i64 = -30;
const POST_BLANK_WEIGHT: i64 = 6;
const RELATIVE_INDENT_PENALTY: i64 = -4;
const RELATIVE_INDENT_WITH_BLANK_PENALTY: i64 = 10;
const RELATIVE_OUTDENT_PENALTY: i64 = 24;
const RELATIVE_OUTDENT_WITH_BLANK_PENALTY: i64 = 17;
const RELATIVE_DEDENT_PENALTY: i64 = 23;
const RELATIVE_DEDENT_WITH_BLANK_PENALTY: i64 = 17;

/// The lines around a split point, as the indent heuristic reads them.
struct Split {
    /// Whether the split lies at the end of the file.
    end_of_file: bool,
    /// The indentation of the line at the split. `None` for a blank line and at the end of the
    /// file.
    indent: Option<usize>,
    /// The number of blank lines directly before the split.
    pre_blank: usize,
    /// The indentation of the nearest non-blank line before the split. After `MAX_BLANKS` blank
    /// lines it is `Some(0)`, and it is `None` when no non-blank line lies before the split.
    pre_indent: Option<usize>,
    /// The number of blank lines after the line at the split.
    post_blank: usize,
    /// The indentation of the nearest non-blank line after the line at the split. The same rules
    /// as for `pre_indent` apply.
    post_indent: Option<usize>,
}

/// Reads the lines around the split point `split`, which lies before the line with that index.
fn measure<T, F: Fn(&T) -> Option<usize>>(lines: &[T], indent: &F, split: usize) -> Split {
    let end_of_file = split >= lines.len();
    let at = if end_of_file {
        None
    } else {
        indent(&lines[split])
    };

    let mut pre_blank = 0;
    let mut pre_indent = None;
    for line in lines[..split].iter().rev() {
        pre_indent = indent(line);
        if pre_indent.is_some() {
            break;
        }
        pre_blank += 1;
        if pre_blank == MAX_BLANKS {
            pre_indent = Some(0);
            break;
        }
    }

    let mut post_blank = 0;
    let mut post_indent = None;
    for line in lines.iter().skip(split + 1) {
        post_indent = indent(line);
        if post_indent.is_some() {
            break;
        }
        post_blank += 1;
        if post_blank == MAX_BLANKS {
            post_indent = Some(0);
            break;
        }
    }

    Split {
        end_of_file,
        indent: at,
        pre_blank,
        pre_indent,
        post_blank,
        post_indent,
    }
}

/// The penalty and the effective indentation of a position. A lower score is better.
#[derive(Default)]
struct Score {
    penalty: i64,
    effective_indent: i64,
}

impl Score {
    /// Adds the cost of one split point.
    fn add(&mut self, m: &Split) {
        if m.pre_indent.is_none() && m.pre_blank == 0 {
            self.penalty += START_OF_FILE_PENALTY;
        }
        if m.end_of_file {
            self.penalty += END_OF_FILE_PENALTY;
        }
        // A split without a line of its own counts one blank line: at the end of the file, or
        // before a blank line. The end of the file therefore reads as a blank line, as in git's
        // model.
        let post_blank = if m.indent.is_none() {
            1 + m.post_blank
        } else {
            0
        };
        let total_blank = m.pre_blank + post_blank;
        self.penalty += TOTAL_BLANK_WEIGHT * total_blank as i64;
        self.penalty += POST_BLANK_WEIGHT * post_blank as i64;

        let indent = m.indent.or(m.post_indent);
        let any_blanks = total_blank != 0;
        self.effective_indent += indent.map_or(-1, |value| value as i64);
        match (indent, m.pre_indent) {
            (Some(i), Some(pre)) if i > pre => {
                self.penalty += if any_blanks {
                    RELATIVE_INDENT_WITH_BLANK_PENALTY
                } else {
                    RELATIVE_INDENT_PENALTY
                };
            }
            (Some(i), Some(pre)) if i < pre => {
                let outdent = m.post_indent.is_some_and(|post| post > i);
                self.penalty += match (outdent, any_blanks) {
                    (true, true) => RELATIVE_OUTDENT_WITH_BLANK_PENALTY,
                    (true, false) => RELATIVE_OUTDENT_PENALTY,
                    (false, true) => RELATIVE_DEDENT_WITH_BLANK_PENALTY,
                    (false, false) => RELATIVE_DEDENT_PENALTY,
                };
            }
            _ => {}
        }
    }
}

/// Compares two scores: a negative result means that `a` is better than `b`.
fn compare(a: &Score, b: &Score) -> i64 {
    INDENT_WEIGHT * (a.effective_indent - b.effective_indent).signum() + (a.penalty - b.penalty)
}

/// Slides the groups of changed lines of one side, given by the flags in `changed`. A group whose
/// first line is flagged in `frozen` stays in place.
fn slide_side<T: PartialEq, F: Fn(&T) -> Option<usize>>(
    lines: &[T],
    changed: &mut [bool],
    frozen: &[bool],
    indent: &F,
) {
    let n = lines.len();
    let mut start = 0;
    while start < n {
        if !changed[start] {
            start += 1;
            continue;
        }
        let s = start;
        let mut e = s;
        while e < n && changed[e] {
            e += 1;
        }
        if frozen[s] {
            start = e;
            continue;
        }
        // A step down removes the group's first line and adds the unchanged line after the group.
        // The two lines must be equal, so the group moves one line at a time. A step up works the
        // same way at the other end.
        let mut down = 0;
        while e + down < n && !changed[e + down] && lines[s + down] == lines[e + down] {
            down += 1;
        }
        let mut up = 0;
        while up < s && !changed[s - up - 1] && lines[s - up - 1] == lines[e - up - 1] {
            up += 1;
        }

        // Each position in the window is scored by the splits before and after the group. The
        // window ends at the lowest position the group can take and reaches up as far as git's.
        // The lowest score wins; on a tie the later position does. A position that puts the group
        // next to another change is skipped, so that the group does not merge with it.
        let len = e - s;
        let bottom = s + down;
        let first = (s - up)
            .max(bottom.saturating_sub(len + 1))
            .max(bottom.saturating_sub(MAX_SLIDING));
        let mut best: Option<(usize, Score)> = None;
        for p in first..=bottom {
            let touches = (p > 0 && changed[p - 1] && !(s..e).contains(&(p - 1)))
                || (p + len < n && changed[p + len] && !(s..e).contains(&(p + len)));
            if touches {
                continue;
            }
            let mut score = Score::default();
            score.add(&measure(lines, indent, p));
            score.add(&measure(lines, indent, p + len));
            let better = match &best {
                None => true,
                Some((_, previous)) => compare(&score, previous) <= 0,
            };
            if better {
                best = Some((p, score));
            }
        }
        let shift = best.map_or(0, |(p, _)| p as isize - s as isize);
        if shift != 0 {
            changed[s..e].fill(false);
            let moved = (s as isize + shift) as usize..(e as isize + shift) as usize;
            changed[moved].fill(true);
        }
        // The next group is searched after the moved group. A group that moved up ends at `e`, so
        // the search continues there; a group that moved down ends after `e + shift`.
        start = (e as isize + shift.max(0)) as usize;
    }
}

/// Builds an edit script from the flags of the changed lines of both sides. The unchanged lines
/// pair up in order, and a deletion comes before an insertion at the same place. Both sides must
/// have the same number of unchanged lines, which sliding preserves.
fn rebuild(changed_old: &[bool], changed_new: &[bool]) -> Vec<Edit> {
    debug_assert_eq!(
        changed_old.iter().filter(|changed| !**changed).count(),
        changed_new.iter().filter(|changed| !**changed).count(),
        "both sides keep the same number of unchanged lines"
    );
    let mut edits = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < changed_old.len() || j < changed_new.len() {
        if i < changed_old.len() && changed_old[i] {
            edits.push(Edit::Delete { old: i });
            i += 1;
        } else if j < changed_new.len() && changed_new[j] {
            edits.push(Edit::Insert { new: j });
            j += 1;
        } else {
            edits.push(Edit::Equal { old: i, new: j });
            i += 1;
            j += 1;
        }
    }
    edits
}

/// A group of changes together with the unchanged lines around it.
///
/// The hunk is a part of an edit script. Its steps are kept in `edits`, the unchanged lines before
/// and after the changes included. The positions give the first line and the number of lines of
/// the hunk on both sides, counted from zero.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
        check_script(old, new, &diff(old, new), expected);
    }

    /// The number of insertions and deletions in `edits`.
    fn changes(edits: &[Edit]) -> usize {
        edits
            .iter()
            .filter(|edit| !matches!(edit, Edit::Equal { .. }))
            .count()
    }

    /// Checks that `edits` turns `old` into `new` and that it has `expected` changes.
    fn check_script<T: PartialEq + std::fmt::Debug>(
        old: &[T],
        new: &[T],
        edits: &[Edit],
        expected: usize,
    ) {
        let mut o = 0;
        let mut n = 0;
        let mut changes = 0;
        for edit in edits {
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

    /// The depth of a line written as a digit. Any other character has no indentation and counts
    /// as a blank line.
    fn digit_indent(line: &char) -> Option<usize> {
        line.to_digit(10).map(|depth| depth as usize)
    }

    #[test]
    fn a_group_moves_to_the_shallower_boundary() {
        // The inserted block sits at the end of the file as '4 0'. It can slide up to the start,
        // where it reads '0 4'. The start has the lower effective indentation (0 against 3), so the
        // group moves there.
        let old = ['0', '4', '0'];
        let new = ['0', '4', '0', '4', '0'];
        let edits = diff(&old, &new);
        assert_eq!(edits[3], Edit::Insert { new: 3 });

        let slid = slide(&old, &new, &edits, digit_indent);
        assert_eq!(
            slid,
            vec![
                Edit::Insert { new: 0 },
                Edit::Insert { new: 1 },
                Edit::Equal { old: 0, new: 2 },
                Edit::Equal { old: 1, new: 3 },
                Edit::Equal { old: 2, new: 4 },
            ]
        );
    }

    #[test]
    fn a_group_stays_where_the_indentation_reads_best() {
        // The inserted '4' can sit after the leading '0' or after the first '4'. The second
        // position
        // has the lower effective indentation (4 against 8), so the group stays.
        let old = ['0', '4', '0'];
        let new = ['0', '4', '4', '0'];
        let edits = diff(&old, &new);
        assert_eq!(slide(&old, &new, &edits, digit_indent), edits);
    }

    #[test]
    fn sliding_keeps_the_number_of_changes_and_a_valid_script() {
        // Every pair of short sequences over {a, b}, once with 'a' at indentation 0 and 'b' at 4,
        // and once with 'a' as a blank line.
        let indents: [fn(&char) -> Option<usize>; 2] = [
            |line| Some(if *line == 'a' { 0 } else { 4 }),
            |line| if *line == 'a' { None } else { Some(4) },
        ];
        for indent in indents {
            for old in &short_sequences() {
                for new in &short_sequences() {
                    let edits = diff(old, new);
                    let slid = slide(old, new, &edits, indent);
                    check_script(old, new, &slid, changes(&edits));
                    assert_eq!(
                        slide(old, new, &slid, indent),
                        slid,
                        "sliding twice moves nothing: {old:?} {new:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn several_groups_on_one_side_are_judged_one_by_one() {
        // Two inserted blocks '4 0', each in a run '0 4 0 4 0' and apart by a '9'. The first slides
        // up to the shallower boundary. The second is judged on its own, after the '9', and stays
        // where git puts it too. The search for it starts after the first.
        let old = ['0', '4', '0', '9', '0', '4', '0'];
        let new = ['0', '4', '0', '4', '0', '9', '0', '4', '0', '4', '0'];
        let edits = diff(&old, &new);
        let slid = slide(&old, &new, &edits, digit_indent);
        check_script(&old, &new, &slid, changes(&edits));
        let inserted: Vec<usize> = slid
            .iter()
            .filter_map(|edit| match *edit {
                Edit::Insert { new } => Some(new),
                _ => None,
            })
            .collect();
        assert_eq!(inserted, vec![0, 1, 8, 9]);
    }

    #[test]
    fn a_deleted_group_moves_to_the_shallower_boundary() {
        // The mirror of the insertion test: the deleted block '4 0' sits at the end of the old
        // side.
        let old = ['0', '4', '0', '4', '0'];
        let new = ['0', '4', '0'];
        let edits = diff(&old, &new);
        assert_eq!(edits[3], Edit::Delete { old: 3 });

        let slid = slide(&old, &new, &edits, digit_indent);
        assert_eq!(
            slid,
            vec![
                Edit::Delete { old: 0 },
                Edit::Delete { old: 1 },
                Edit::Equal { old: 2, new: 0 },
                Edit::Equal { old: 3, new: 1 },
                Edit::Equal { old: 4, new: 2 },
            ]
        );
    }

    #[test]
    fn a_deletion_comes_before_an_insertion_at_the_same_place() {
        // This is a replacement, so it is frozen and does not slide. The order of its deletion and
        // insertion then comes from the rebuild alone.
        let old = ['0', '4'];
        let new = ['0', '5'];
        let edits = diff(&old, &new);
        assert_eq!(
            slide(&old, &new, &edits, digit_indent),
            vec![
                Edit::Equal { old: 0, new: 0 },
                Edit::Delete { old: 1 },
                Edit::Insert { new: 1 },
            ]
        );
    }

    #[test]
    fn a_replacement_is_not_split_by_sliding() {
        // The deleted 'a' could slide down past the equal 'a', away from the inserted 'b'. A
        // replacement stays together, so the script keeps both changes in front of the equal line.
        let old = ['a', 'a'];
        let new = ['b', 'a'];
        let edits = diff(&old, &new);
        assert_eq!(
            edits,
            vec![
                Edit::Delete { old: 0 },
                Edit::Insert { new: 0 },
                Edit::Equal { old: 1, new: 1 },
            ]
        );
        assert_eq!(
            slide(&old, &new, &edits, |line| Some(if *line == 'a' {
                0
            } else {
                4
            })),
            vec![
                Edit::Delete { old: 0 },
                Edit::Insert { new: 0 },
                Edit::Equal { old: 1, new: 1 },
            ]
        );
    }

    #[test]
    fn diff_lines_moves_an_insertion_by_the_indentation_of_its_lines() {
        // The same case as the insertion test above, with the indentation written as spaces.
        let old = ["x", "    x", "x"];
        let new = ["x", "    x", "x", "    x", "x"];
        assert_eq!(
            diff_lines(&old, &new),
            vec![
                Edit::Insert { new: 0 },
                Edit::Insert { new: 1 },
                Edit::Equal { old: 0, new: 2 },
                Edit::Equal { old: 1, new: 3 },
                Edit::Equal { old: 2, new: 4 },
            ]
        );
    }

    #[test]
    fn a_pure_insertion_does_not_slide_next_to_a_replacement() {
        // The inserted 'a' at new index 2 could slide up next to the replacement at index 0. That
        // would merge the two into one run, so it stays where the diff put it.
        let old = ['b', 'a', 'b'];
        let new = ['a', 'a', 'a', 'b', 'a'];
        let edits = diff(&old, &new);
        assert_eq!(
            slide(&old, &new, &edits, |line| Some(if *line == 'a' {
                0
            } else {
                4
            })),
            vec![
                Edit::Delete { old: 0 },
                Edit::Insert { new: 0 },
                Edit::Equal { old: 1, new: 1 },
                Edit::Insert { new: 2 },
                Edit::Equal { old: 2, new: 3 },
                Edit::Insert { new: 4 },
            ]
        );
    }

    #[test]
    fn a_tie_keeps_the_later_position() {
        // Every position scores the same, so the deleted 'a' stays at the later of the tied
        // positions instead of moving to the first one.
        let old = ['a', 'a', 'a', 'b'];
        let new = ['a', 'a', 'b'];
        let edits = diff(&old, &new);
        assert_eq!(
            slide(&old, &new, &edits, |_| Some(0)),
            vec![
                Edit::Equal { old: 0, new: 0 },
                Edit::Equal { old: 1, new: 1 },
                Edit::Delete { old: 2 },
                Edit::Equal { old: 3, new: 2 },
            ]
        );
    }

    #[test]
    fn the_scan_over_blank_lines_stops_at_max_blanks() {
        // One indented line, followed by more blank lines than the scan reads.
        let mut lines = vec!['4'];
        lines.extend(vec![' '; MAX_BLANKS + 5]);
        let split = measure(&lines, &digit_indent, lines.len());
        assert_eq!(split.pre_blank, MAX_BLANKS);
        assert_eq!(split.pre_indent, Some(0));
    }

    #[test]
    fn the_end_of_the_file_adds_its_penalty_and_counts_as_a_blank_line() {
        // A split at the end has no line, so it reads as a blank one: its post blank line is
        // penalised with the blank weights as well.
        let mut score = Score::default();
        score.add(&Split {
            end_of_file: true,
            indent: None,
            pre_blank: 0,
            pre_indent: Some(4),
            post_blank: 0,
            post_indent: None,
        });
        assert_eq!(
            score.penalty,
            END_OF_FILE_PENALTY + TOTAL_BLANK_WEIGHT + POST_BLANK_WEIGHT
        );
        assert_eq!(score.effective_indent, -1);
    }

    #[test]
    fn the_start_of_the_file_adds_its_penalty() {
        let mut score = Score::default();
        score.add(&Split {
            end_of_file: false,
            indent: Some(0),
            pre_blank: 0,
            pre_indent: None,
            post_blank: 0,
            post_indent: None,
        });
        assert_eq!(score.penalty, START_OF_FILE_PENALTY);
        assert_eq!(score.effective_indent, 0);
    }

    #[test]
    fn a_blank_line_before_an_indent_changes_its_penalty() {
        let mut score = Score::default();
        score.add(&Split {
            end_of_file: false,
            indent: Some(4),
            pre_blank: 1,
            pre_indent: Some(0),
            post_blank: 0,
            post_indent: Some(4),
        });
        assert_eq!(
            score.penalty,
            TOTAL_BLANK_WEIGHT + RELATIVE_INDENT_WITH_BLANK_PENALTY
        );
        assert_eq!(score.effective_indent, 4);
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

    /// All sequences over {a, b} of length up to 5.
    fn short_sequences() -> Vec<Vec<char>> {
        (0..=5u32)
            .flat_map(|len| {
                (0..1u32 << len).map(move |bits| {
                    (0..len)
                        .map(|i| if bits >> i & 1 == 1 { 'b' } else { 'a' })
                        .collect()
                })
            })
            .collect()
    }

    #[test]
    fn every_pair_of_short_sequences_has_the_lcs_distance() {
        // A shortest script has N + M - 2·LCS changes.
        let all = short_sequences();
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

    /// The script of a run of `extra` inserted lines at the top of a run of equal lines, between
    /// the lines `head` and `tail`.
    fn insertion_at_the_top_of_a_run(
        run: usize,
        extra: usize,
        head: char,
        tail: char,
    ) -> (Vec<char>, Vec<char>, Vec<Edit>) {
        let build = |len: usize| {
            let mut lines = vec![head];
            lines.extend(vec!['1'; len]);
            lines.push(tail);
            lines
        };
        let (old, new) = (build(run), build(run + extra));
        let mut edits = vec![Edit::Equal { old: 0, new: 0 }];
        edits.extend((1..=extra).map(|new| Edit::Insert { new }));
        edits.extend((1..old.len()).map(|old| Edit::Equal {
            old,
            new: old + extra,
        }));
        (old, new, edits)
    }

    fn first_insert(edits: &[Edit]) -> usize {
        edits
            .iter()
            .position(|edit| matches!(edit, Edit::Insert { .. }))
            .unwrap()
    }

    #[test]
    fn the_window_reaches_up_by_the_size_of_the_group_plus_one() {
        // After a blank line the top of the run reads best, but it lies further above the lowest
        // position than the one-line group plus one. As in git, the search does not reach it.
        let (old, new, edits) = insertion_at_the_top_of_a_run(6, 1, ' ', '5');
        let slid = slide(&old, &new, &edits, digit_indent);
        assert_eq!(first_insert(&slid), 6);
    }

    #[test]
    fn the_window_reaches_up_by_at_most_max_sliding() {
        // The group is large, so the size limit alone would reach the top of the run, which reads
        // best. Only MAX_SLIDING positions above the lowest one are searched, so it stays out of
        // reach.
        let (run, extra) = (MAX_SLIDING + 20, MAX_SLIDING + 50);
        let (old, new, edits) = insertion_at_the_top_of_a_run(run, extra, ' ', '5');
        let slid = slide(&old, &new, &edits, digit_indent);
        let lowest = 1 + run;
        assert!(first_insert(&slid) >= lowest - MAX_SLIDING);
    }

    #[test]
    fn byte_lines_slide_like_text_lines() {
        let old = ["fn a() {\n", "}\n", "\n", "fn c() {\n", "}\n"];
        let new = [
            "fn a() {\n",
            "}\n",
            "\n",
            "fn b() {\n",
            "}\n",
            "\n",
            "fn c() {\n",
            "}\n",
        ];
        let text = diff_lines(&old, &new);
        let old_bytes: Vec<&[u8]> = old.iter().map(|line| line.as_bytes()).collect();
        let new_bytes: Vec<&[u8]> = new.iter().map(|line| line.as_bytes()).collect();
        assert_eq!(diff_byte_lines(&old_bytes, &new_bytes), text);
    }

    #[test]
    fn byte_lines_list_deletions_before_insertions() {
        let old: [&[u8]; 3] = [b"a\n", b"x\n", b"b\n"];
        let new: [&[u8]; 4] = [b"a\n", b"y\n", b"z\n", b"b\n"];
        let raw = diff(&old, &new);
        assert_eq!(
            raw[1],
            Edit::Insert { new: 1 },
            "the plain script inserts first"
        );
        assert_eq!(
            diff_byte_lines(&old, &new),
            [
                Edit::Equal { old: 0, new: 0 },
                Edit::Delete { old: 1 },
                Edit::Insert { new: 1 },
                Edit::Insert { new: 2 },
                Edit::Equal { old: 2, new: 3 },
            ]
        );
    }

    #[test]
    fn byte_lines_count_only_ascii_whitespace_as_indent() {
        assert_eq!(byte_line_indent(&&b"  x\n"[..]), Some(2));
        assert_eq!(byte_line_indent(&&b" \r\n"[..]), None);
        // Vertical tab and the no-break space are whitespace for `str::trim`, not for bytes.
        assert_eq!(byte_line_indent(&&b"\x0bx\n"[..]), Some(0));
        assert_eq!(byte_line_indent(&"\u{a0}x\n".as_bytes()), Some(0));
        assert_eq!(line_indent(&"\u{a0}x\n"), Some(2));
    }

    #[test]
    fn a_line_that_is_not_utf8_takes_part() {
        let edits = diff_byte_lines(&[b"\xff\n"], &[b"\xfe\n"]);
        assert_eq!(changes(&edits), 2);
    }
}
