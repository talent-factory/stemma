//! Moved lines in a diff, as a display feature (like `git diff --color-moved`).
//!
//! A diff shows a line that moved as a deletion in one place and an insertion in another (chapter
//! 5, section on moved lines). [`moved_lines`] finds the deleted lines that come back as inserted
//! lines, so that a display can mark them. It reads a [`Diff`] and changes nothing in it: the
//! hunks, the exit code, and the plain text stay as they are.
//!
//! A block is a run of lines that are consecutive on their side. A deleted block and an inserted
//! block are the same move when their lines are equal, byte for byte, and in the same order.
//! The blocks may lie in different files. Because a move of a few short lines is more often a
//! coincidence (closing braces, blank lines) than a move, a block counts only if its lines hold at
//! least [`MIN_ALPHANUMERIC`] alphanumeric characters. The number is that of git; the count is over
//! Unicode alphanumerics, where git counts ASCII. Only text files take part: a binary file and a
//! file above the size limit have no lines to compare.

use std::collections::{HashMap, HashSet};

use crate::diff::Edit;
use crate::repository::{Content, Diff};

/// The least number of alphanumeric characters that a block of lines must hold to count as moved.
pub const MIN_ALPHANUMERIC: usize = 20;

/// The most places on the new side that are tried for one deleted line. It bounds the work for a
/// line that is repeated thousands of times, such as a blank line.
const MAX_CANDIDATES: usize = 256;

/// The most line comparisons that the search makes for one diff. Without it, a diff of many equal
/// lines costs the number of lines, times [`MAX_CANDIDATES`], times the length of a block. When
/// the budget is spent, the search stops and keeps the moves found so far; the same diff always
/// stops at the same place.
const MAX_COMPARISONS: usize = 20_000_000;

/// The lines of a diff that moved, found by [`moved_lines`].
///
/// A line is named by the position of its file in [`Diff::files`] and by its zero-based index in
/// the old or the new lines of that file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MovedLines {
    from: HashSet<(usize, usize)>,
    to: HashSet<(usize, usize)>,
}

impl MovedLines {
    /// Tells whether the deleted line `old` of the file at position `file` moved to another place.
    pub fn is_moved_from(&self, file: usize, old: usize) -> bool {
        self.from.contains(&(file, old))
    }

    /// Tells whether the inserted line `new` of the file at position `file` came from another
    /// place.
    pub fn is_moved_to(&self, file: usize, new: usize) -> bool {
        self.to.contains(&(file, new))
    }

    /// Tells whether no line moved. A line is recorded on both sides at once, so the deleted side
    /// is enough to look at.
    pub fn is_empty(&self) -> bool {
        self.from.is_empty()
    }
}

/// One deleted or inserted line: the position of its file, its index, and its text.
struct Entry<'a> {
    file: usize,
    index: usize,
    text: &'a str,
}

/// Finds the lines of `diff` that moved. The result is the same for the same diff.
///
/// The deleted lines are taken in the order of the diff. A deleted line is matched with the
/// longest inserted block of equal lines among the first 256 free places where its text occurs,
/// the first such block on a tie; a line belongs to at most one move. The whole search makes at most
/// 20 million line comparisons and keeps the moves found so far when they are spent.
pub fn moved_lines(diff: &Diff) -> MovedLines {
    let mut deleted = Vec::new();
    let mut inserted = Vec::new();
    for (file, entry) in diff.files().iter().enumerate() {
        let Content::Text(text) = entry.content() else {
            continue;
        };
        for edit in text.hunks().iter().flat_map(|hunk| &hunk.edits) {
            match *edit {
                Edit::Delete { old } => {
                    if let Some(line) = text.old_lines().get(old) {
                        deleted.push(Entry {
                            file,
                            index: old,
                            text: line,
                        });
                    }
                }
                Edit::Insert { new } => {
                    if let Some(line) = text.new_lines().get(new) {
                        inserted.push(Entry {
                            file,
                            index: new,
                            text: line,
                        });
                    }
                }
                Edit::Equal { .. } => {}
            }
        }
    }
    let mut places: HashMap<&str, Vec<usize>> = HashMap::new();
    for (position, entry) in inserted.iter().enumerate() {
        places.entry(entry.text).or_default().push(position);
    }
    let mut taken_deleted = vec![false; deleted.len()];
    let mut taken_inserted = vec![false; inserted.len()];
    let mut moved = MovedLines::default();
    let mut start = 0;
    let mut budget = MAX_COMPARISONS;
    while start < deleted.len() && budget > 0 {
        let candidates = places
            .get(deleted[start].text)
            .map_or(&[][..], Vec::as_slice);
        let mut best = (0, 0);
        let mut tried = 0;
        for &place in candidates {
            // Every place that is looked at costs one comparison, a taken one included, so that a
            // long list of taken places cannot be walked for free.
            budget = budget.saturating_sub(1);
            if budget == 0 || tried == MAX_CANDIDATES {
                break;
            }
            if taken_inserted[place] {
                continue;
            }
            tried += 1;
            let length = match_length(
                &deleted,
                &inserted,
                (start, place),
                (&taken_deleted, &taken_inserted),
            );
            budget = budget.saturating_sub(length);
            if length > best.0 {
                best = (length, place);
            }
        }
        let (length, place) = best;
        if length > 0 && alphanumeric(&deleted[start..start + length]) >= MIN_ALPHANUMERIC {
            for k in 0..length {
                taken_deleted[start + k] = true;
                taken_inserted[place + k] = true;
                moved
                    .from
                    .insert((deleted[start + k].file, deleted[start + k].index));
                moved
                    .to
                    .insert((inserted[place + k].file, inserted[place + k].index));
            }
            start += length;
        } else {
            start += 1;
        }
    }
    moved
}

/// How many lines from `deleted[start]` and `inserted[place]` on are equal, are consecutive on
/// their side, and are not part of another move yet.
fn match_length(
    deleted: &[Entry<'_>],
    inserted: &[Entry<'_>],
    (start, place): (usize, usize),
    (taken_deleted, taken_inserted): (&[bool], &[bool]),
) -> usize {
    let mut length = 0;
    while let (Some(d), Some(i)) = (deleted.get(start + length), inserted.get(place + length)) {
        if taken_deleted[start + length] || taken_inserted[place + length] || d.text != i.text {
            break;
        }
        if length > 0 {
            let (d_before, i_before) =
                (&deleted[start + length - 1], &inserted[place + length - 1]);
            let consecutive = |before: &Entry<'_>, now: &Entry<'_>| {
                before.file == now.file && before.index + 1 == now.index
            };
            if !consecutive(d_before, d) || !consecutive(i_before, i) {
                break;
            }
        }
        length += 1;
    }
    length
}

/// The number of alphanumeric characters in the lines.
fn alphanumeric(lines: &[Entry<'_>]) -> usize {
    lines
        .iter()
        .map(|entry| entry.text.chars().filter(|c| c.is_alphanumeric()).count())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{Comparison, InitOptions, Repository};
    use crate::testutil::TempDir;
    use crate::{Actor, AllowAll};

    const BLOCK: [&str; 3] = [
        "fn first_function_name() {}\n",
        "fn second_function_name() {}\n",
        "fn third_function_name() {}\n",
    ];

    /// Commits `before`, then writes `after`, and returns the moved lines of the difference and
    /// the paths of its files.
    fn moves(before: &[(&str, String)], after: &[(&str, String)]) -> (MovedLines, Vec<String>) {
        let dir = TempDir::new();
        let actor = Actor::human("daniel").unwrap();
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &actor, &AllowAll).unwrap();
        for (path, content) in before {
            std::fs::write(dir.path().join(path), content).unwrap();
            repository
                .track(&[(*path).to_owned()], &actor, &AllowAll)
                .unwrap();
        }
        let _ = repository.commit("before", &actor, &AllowAll).unwrap();
        for (path, content) in after {
            std::fs::write(dir.path().join(path), content).unwrap();
            repository
                .track(&[(*path).to_owned()], &actor, &AllowAll)
                .unwrap();
        }
        let diff = repository
            .diff(&actor, &AllowAll, Comparison::WithParent)
            .unwrap();
        let paths = diff.files().iter().map(|f| f.path().to_owned()).collect();
        (moved_lines(&diff), paths)
    }

    fn lines(parts: &[&str]) -> String {
        parts.concat()
    }

    #[test]
    fn a_block_that_moves_within_a_file_is_marked_on_both_sides() {
        // The separator is longer than the block, so the shortest script moves the block.
        let separator = "let separator_one = 1;\nlet separator_two = 2;\n\
                         let separator_three = 3;\nlet separator_four = 4;\n";
        let before = lines(&[&BLOCK.concat(), separator, "let tail_of_the_file = 2;\n"]);
        let after = lines(&[separator, &BLOCK.concat(), "let tail_of_the_file = 2;\n"]);
        let (moved, _) = moves(&[("a.rs", before)], &[("a.rs", after)]);
        // The block is deleted at 0..3 and inserted at 4..7; the separator is not touched.
        assert!((0..3).all(|i| moved.is_moved_from(0, i)));
        assert!((4..7).all(|i| moved.is_moved_to(0, i)));
        assert!(!moved.is_moved_to(0, 0));
    }

    #[test]
    fn a_block_that_moves_to_another_file_is_marked() {
        let (moved, paths) = moves(
            &[("a.rs", BLOCK.concat()), ("b.rs", "keep();\n".to_owned())],
            &[
                ("a.rs", String::new()),
                ("b.rs", lines(&["keep();\n", &BLOCK.concat()])),
            ],
        );
        assert_eq!(paths, ["a.rs", "b.rs"]);
        assert!((0..3).all(|i| moved.is_moved_from(0, i)));
        assert!((1..4).all(|i| moved.is_moved_to(1, i)));
    }

    #[test]
    fn a_block_with_too_few_letters_is_not_marked() {
        let braces = "}\n}\n}\n";
        let (moved, _) = moves(
            &[("a.rs", lines(&[braces, "let x = 1;\n"]))],
            &[("a.rs", lines(&["let x = 1;\n", braces]))],
        );
        assert!(moved.is_empty());
    }

    #[test]
    fn a_line_changed_while_moving_breaks_the_block() {
        let changed = BLOCK.map(|line| line.replace("name", "label")).concat();
        let (moved, _) = moves(&[("a.rs", BLOCK.concat())], &[("b.rs", changed)]);
        assert!(moved.is_empty());
    }

    #[test]
    fn a_deletion_without_an_insertion_is_not_marked() {
        let (moved, _) = moves(&[("a.rs", BLOCK.concat())], &[("a.rs", String::new())]);
        assert!(moved.is_empty());
    }

    #[test]
    fn a_block_is_matched_with_one_place_only() {
        let (moved, _) = moves(
            &[("a.rs", BLOCK.concat())],
            &[
                ("a.rs", String::new()),
                ("b.rs", BLOCK.concat()),
                ("c.rs", BLOCK.concat()),
            ],
        );
        let to: usize = (0..3)
            .map(|file| (0..3).filter(|&i| moved.is_moved_to(file, i)).count())
            .sum();
        assert_eq!(to, 3);
    }

    #[test]
    fn the_first_of_two_equal_places_wins() {
        let (moved, _) = moves(
            &[("a.rs", BLOCK.concat())],
            &[
                ("a.rs", String::new()),
                ("b.rs", BLOCK.concat()),
                ("c.rs", BLOCK.concat()),
            ],
        );
        assert!((0..3).all(|i| moved.is_moved_to(1, i)));
        assert!(!(0..3).any(|i| moved.is_moved_to(2, i)));
    }

    #[test]
    fn the_longer_of_two_places_wins() {
        let two = BLOCK[..2].concat();
        let (moved, _) = moves(
            &[("a.rs", BLOCK.concat())],
            &[
                ("a.rs", String::new()),
                ("b.rs", two),
                ("c.rs", BLOCK.concat()),
            ],
        );
        assert!((0..3).all(|i| moved.is_moved_to(2, i)));
        assert!(!(0..2).any(|i| moved.is_moved_to(1, i)));
    }

    #[test]
    fn the_indices_count_a_binary_file_before_the_text_files() {
        let (moved, paths) = moves(
            &[
                ("a.bin", "\0one\n".to_owned()),
                ("b.rs", BLOCK.concat()),
                ("c.rs", "keep\n".to_owned()),
            ],
            &[
                ("a.bin", "\0two\n".to_owned()),
                ("b.rs", String::new()),
                ("c.rs", lines(&["keep\n", &BLOCK.concat()])),
            ],
        );
        assert_eq!(paths, ["a.bin", "b.rs", "c.rs"]);
        assert!((0..3).all(|i| moved.is_moved_from(1, i)));
        assert!((1..4).all(|i| moved.is_moved_to(2, i)));
    }

    #[test]
    fn an_unchanged_line_between_two_deleted_runs_splits_the_block() {
        let (first, second) = (BLOCK[..2].concat(), BLOCK[2..].concat());
        let middle = "let the_unchanged_middle_line = 0;\n";
        let (moved, _) = moves(
            &[("a.rs", lines(&[&first, middle, &second]))],
            &[("a.rs", middle.to_owned()), ("b.rs", BLOCK.concat())],
        );
        // The first run holds 54 alphanumeric characters and counts. The second run is one line of
        // 19 and does not count on its own, although the whole block would.
        assert!(moved.is_moved_from(0, 0) && moved.is_moved_from(0, 1));
        assert!(!moved.is_moved_from(0, 2));
        assert!(!moved.is_moved_from(0, 3));
    }

    #[test]
    fn a_block_needs_twenty_alphanumeric_characters() {
        // The lines hold 9 and 10 characters, or 10 and 10; the newline does not count.
        for (text, marked) in [
            ("abcdefghi\nabcdefghij\n", false),
            ("abcdefghij\nabcdefghij\n", true),
        ] {
            let (moved, _) = moves(
                &[("a.txt", text.to_owned())],
                &[("a.txt", String::new()), ("b.txt", text.to_owned())],
            );
            assert_eq!(!moved.is_empty(), marked, "{text:?}");
        }
    }

    #[test]
    fn thousands_of_equal_lines_without_letters_end_in_bounded_time() {
        let braces = "}\n".repeat(5000);
        let (moved, _) = moves(
            &[("a.txt", braces.clone())],
            &[("a.txt", String::new()), ("b.txt", braces)],
        );
        assert!(moved.is_empty());
    }
}
