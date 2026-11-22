//! The view of `stemma diff --tui` on a real diff, drawn into the `TestBackend` of ratatui, so that
//! no terminal is needed (ADR-016).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use stemma_core::repository::{Comparison, Diff, InitOptions, Repository};
use stemma_core::{Actor, AllowAll};
use stemma_tui::{render, render_with, Input, LayoutMode, State, Stream, Theme};

/// A directory below the system temp directory that is removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "stemma-tui-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, content: impl AsRef<[u8]>) {
        fs::write(self.0.join(name), content).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A repository in a temp directory, and the actor that works in it.
struct Fixture {
    dir: TempDir,
    repository: Repository,
    actor: Actor,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new();
        let actor: Actor = "human alice".parse().unwrap();
        let repository = Repository::init(dir.path(), &InitOptions::default(), &actor, &AllowAll)
            .expect("init the repository");
        Self {
            dir,
            repository,
            actor,
        }
    }

    fn track(&self, paths: &[&str]) {
        let paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();
        self.repository
            .track(&paths, &self.actor, &AllowAll)
            .expect("track the files");
    }

    fn commit(&self) {
        let _ = self
            .repository
            .commit("first", &self.actor, &AllowAll)
            .expect("commit");
    }

    fn diff(&self) -> Diff {
        self.repository
            .diff(&self.actor, &AllowAll, Comparison::WithParent)
            .expect("diff")
    }
}

fn numbered(changed: &[usize]) -> String {
    (1..=40)
        .map(|n| {
            if changed.contains(&n) {
                format!("changed {n}\n")
            } else {
                format!("line {n}\n")
            }
        })
        .collect()
}

/// A diff of two files: `a.txt` has two hunks, far apart, and `b.txt` has one.
fn diff() -> (Fixture, Diff) {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", numbered(&[]));
    fixture.dir.write("b.txt", "one\n");
    fixture.track(&["a.txt", "b.txt"]);
    fixture.commit();
    fixture.dir.write("a.txt", numbered(&[3, 36]));
    fixture.dir.write("b.txt", "two\n");
    let diff = fixture.diff();
    (fixture, diff)
}

/// A diff of three files that are not plain edits: a binary file that changed, a file that was
/// deleted, and a new file with a tab.
fn special_diff() -> (Fixture, Diff) {
    let fixture = Fixture::new();
    fixture.dir.write("bin.dat", b"\0\x01");
    fixture.dir.write("gone.txt", "bye\n");
    fixture.track(&["bin.dat", "gone.txt"]);
    fixture.commit();
    fixture.dir.write("bin.dat", b"\0\x02");
    fs::remove_file(fixture.dir.path().join("gone.txt")).unwrap();
    fixture.dir.write("new.txt", "\tindented\n");
    fixture.track(&["new.txt"]);
    let diff = fixture.diff();
    (fixture, diff)
}

/// Applies `input` to `state` with a stream of its own.
fn apply(state: &mut State, input: Input, diff: &Diff) -> bool {
    state.apply(input, diff, &mut Stream::new(diff))
}

/// Draws `state` on a terminal of this size, after the state has been told the width.
fn terminal_of_size(state: &State, diff: &Diff, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut state = *state;
    if state.width() != width {
        apply(&mut state, Input::Resize(width), diff);
    }
    let mut stream = Stream::new(diff);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| render_with(frame, &state, diff, &mut stream))
        .unwrap();
    terminal
}

fn screen_of_size(state: &State, diff: &Diff, width: u16, height: u16) -> String {
    let terminal = terminal_of_size(state, diff, width, height);
    let buffer = terminal.backend().buffer();
    buffer
        .content()
        .chunks(usize::from(buffer.area.width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn screen(state: &State, diff: &Diff) -> String {
    screen_of_size(state, diff, 80, 12)
}

/// The width of the file list: a quarter of the terminal, but not more than 40 columns.
fn sidebar(width: u16) -> usize {
    usize::from((width / 4).min(40))
}

/// The columns of the row that holds `needle` between the border of the diff pane and `needle`:
/// the line numbers and the mark. The row is the first one that holds `needle` right of the file
/// list.
fn gutter_of(screen: &str, width: u16, needle: &str) -> String {
    screen
        .lines()
        .find_map(|row| {
            let pane: String = row.chars().skip(sidebar(width) + 1).collect();
            let at = pane.find(needle)?;
            Some(pane[..at].to_owned())
        })
        .unwrap_or_else(|| panic!("`{needle}` is not in the pane:\n{screen}"))
}

/// The row of the pane that holds `needle`, without the border of the pane and the file list.
fn pane_row(screen: &str, width: u16, needle: &str) -> String {
    let inner = usize::from(width) - sidebar(width) - 2;
    screen
        .lines()
        .map(|row| {
            row.chars()
                .skip(sidebar(width) + 1)
                .take(inner)
                .collect::<String>()
        })
        .find(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("`{needle}` is not in the pane:\n{screen}"))
}

/// A state that has been told to use `layout`.
fn with_layout(layout: LayoutMode, diff: &Diff) -> State {
    let mut state = State::default();
    apply(&mut state, Input::SetLayout(layout), diff);
    state
}

/// The index of the first row of the screen that holds `needle` right of the file list.
fn row_index(screen: &str, needle: &str) -> usize {
    screen
        .lines()
        .position(|row| row.chars().skip(21).collect::<String>().contains(needle))
        .unwrap_or_else(|| panic!("`{needle}` is not on the screen:\n{screen}"))
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn keys_map_to_inputs() {
    let table = [
        (KeyCode::Char('q'), Some(Input::Quit)),
        (KeyCode::Esc, Some(Input::Quit)),
        (KeyCode::Char('J'), Some(Input::NextFile)),
        (KeyCode::Tab, Some(Input::NextFile)),
        (KeyCode::Char('K'), Some(Input::PreviousFile)),
        (KeyCode::BackTab, Some(Input::PreviousFile)),
        (KeyCode::Char('n'), Some(Input::NextHunk)),
        (KeyCode::Char('p'), Some(Input::PreviousHunk)),
        (KeyCode::Char('j'), Some(Input::ScrollDown(1))),
        (KeyCode::Down, Some(Input::ScrollDown(1))),
        (KeyCode::Char('k'), Some(Input::ScrollUp(1))),
        (KeyCode::Up, Some(Input::ScrollUp(1))),
        (KeyCode::PageDown, Some(Input::ScrollDown(20))),
        (KeyCode::Char(' '), Some(Input::ScrollDown(20))),
        (KeyCode::PageUp, Some(Input::ScrollUp(20))),
        (KeyCode::Char('b'), Some(Input::ScrollUp(20))),
        (
            KeyCode::Char('1'),
            Some(Input::SetLayout(LayoutMode::Unified)),
        ),
        (
            KeyCode::Char('2'),
            Some(Input::SetLayout(LayoutMode::Split)),
        ),
        (KeyCode::Char('0'), Some(Input::SetLayout(LayoutMode::Auto))),
        (KeyCode::Char('x'), None),
        // Without the control key, `c` is just a letter.
        (KeyCode::Char('c'), None),
    ];
    for (code, expected) in table {
        assert_eq!(Input::from_key(key(code)), expected, "{code:?}");
    }
    assert_eq!(
        Input::from_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(Input::Quit)
    );
}

#[test]
fn the_view_lists_the_files_and_shows_the_first() {
    let (_fixture, diff) = diff();
    let screen = screen(&State::default(), &diff);
    assert!(screen.contains("a.txt"), "{screen}");
    assert!(screen.contains("b.txt"), "{screen}");
    assert!(screen.contains("@@ -1,6 +1,6 @@"), "{screen}");
    assert!(screen.contains("-line 3"), "{screen}");
    assert!(screen.contains("+changed 3"), "{screen}");
}

#[test]
fn a_header_names_the_file_and_counts_its_changes() {
    let (_fixture, diff) = diff();
    let screen = screen_of_size(&State::default(), &diff, 80, 30);
    // `a.txt` has two lines removed and two added; `b.txt` one each. The header of a file is the
    // row of the pane that holds its path and the counts.
    let header = |path: &str| {
        screen
            .lines()
            .map(|row| row.chars().skip(sidebar(80) + 1).collect::<String>())
            .find(|row| row.contains(path))
            .unwrap_or_else(|| panic!("no header for {path}:\n{screen}"))
    };
    assert!(header("a.txt").contains("+2 -2"), "{screen}");
    assert!(header("b.txt").contains("+1 -1"), "{screen}");
}

#[test]
fn the_lines_of_all_files_are_one_stream() {
    let (_fixture, diff) = diff();
    // A tall terminal shows the end of `a.txt` and all of `b.txt` below it without a key.
    let screen = screen_of_size(&State::default(), &diff, 80, 30);
    assert!(screen.contains("+changed 36"), "{screen}");
    assert!(screen.contains("-one"), "{screen}");
    assert!(screen.contains("+two"), "{screen}");
}

#[test]
fn line_numbers_stand_left_of_the_marks() {
    let (_fixture, diff) = diff();
    let screen = screen_of_size(&State::default(), &diff, 80, 30);
    // The number column is two wide (the longest file has 40 lines): old number, new number, mark.
    // A context line has both numbers, a removed line only the old one, an added line only the new
    // one.
    assert_eq!(gutter_of(&screen, 80, "line 1"), " 1  1  ");
    assert_eq!(gutter_of(&screen, 80, "-line 3"), " 3    ");
    assert_eq!(gutter_of(&screen, 80, "+changed 3"), "    3 ");
    assert_eq!(gutter_of(&screen, 80, "+changed 36"), "   36 ");
}

#[test]
fn hunks_and_files_are_walked_in_order() {
    let (_fixture, diff) = diff();
    let mut state = State::default();

    // At the very start there is no previous hunk.
    apply(&mut state, Input::PreviousHunk, &diff);
    assert_eq!(state, State::default());

    // The stream starts with the header of `a.txt` (row 0). The first hunk has its `@@` row and
    // seven lines (rows 1 to 8), so the second hunk starts in row 9.
    assert!(apply(&mut state, Input::NextHunk, &diff));
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 1, 9));
    let screen = screen(&state, &diff);
    assert!(screen.contains("+changed 36"), "{screen}");
    // The first hunk has scrolled out; a row ends in spaces, so `+changed 36` does not match.
    assert!(
        !screen.lines().any(|row| row.contains("+changed 3 ")),
        "{screen}"
    );

    // After the last hunk of `a.txt`, the next hunk is the first of `b.txt`, with the header of its
    // file in view. The second hunk has its `@@` row and eight lines (rows 9 to 17), one empty row
    // separates the files (row 18), and the header of `b.txt` is row 19.
    apply(&mut state, Input::NextHunk, &diff);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (1, 0, 19));

    // At the end nothing moves on; going back lands on the last hunk of the previous file, scrolled
    // to its header.
    apply(&mut state, Input::NextHunk, &diff);
    assert_eq!((state.file(), state.hunk()), (1, 0));
    apply(&mut state, Input::PreviousHunk, &diff);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 1, 9));
}

#[test]
fn the_files_are_selected_one_by_one_and_stop_at_the_ends() {
    let (_fixture, diff) = diff();
    let mut state = State::default();
    apply(&mut state, Input::PreviousFile, &diff);
    assert_eq!(state.file(), 0);
    apply(&mut state, Input::ScrollDown(3), &diff);
    apply(&mut state, Input::NextFile, &diff);
    assert_eq!((state.file(), state.scroll()), (1, 19));
    apply(&mut state, Input::NextFile, &diff);
    assert_eq!(state.file(), 1);
    assert!(screen(&state, &diff).contains("+two"));
}

#[test]
fn scrolling_stops_at_the_top_and_at_the_last_line() {
    let (_fixture, diff) = diff();
    let mut state = State::default();
    apply(&mut state, Input::ScrollUp(5), &diff);
    assert_eq!(state.scroll(), 0);
    apply(&mut state, Input::ScrollDown(3), &diff);
    assert_eq!(state.scroll(), 3);

    // Far past the end, the last line stays in view instead of an empty pane. The stream has 23
    // rows (see above, and the four of `b.txt`), so its last row is row 22.
    apply(&mut state, Input::ScrollDown(u16::MAX), &diff);
    assert_eq!(state.scroll(), 22);
    apply(&mut state, Input::ScrollDown(1), &diff);
    assert_eq!(state.scroll(), 22);
    assert!(screen(&state, &diff).contains("+two"));
}

#[test]
fn scrolling_into_another_file_moves_the_mark_of_the_list() {
    let (_fixture, diff) = diff();
    let mut state = State::default();
    apply(&mut state, Input::ScrollDown(18), &diff);
    assert_eq!((state.file(), state.hunk()), (0, 0));
    // Row 19 is the header of `b.txt`.
    apply(&mut state, Input::ScrollDown(1), &diff);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (1, 0, 19));
    apply(&mut state, Input::ScrollUp(1), &diff);
    assert_eq!((state.file(), state.scroll()), (0, 18));

    // The row of the list that is highlighted is the one of the file in view.
    apply(&mut state, Input::ScrollDown(5), &diff);
    let terminal = terminal_of_size(&state, &diff, 80, 12);
    let buffer = terminal.backend().buffer();
    let marked: Vec<String> = (0..buffer.area.height)
        .filter(|&y| {
            buffer[(1, y)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        })
        .map(|y| {
            (0..sidebar(80) as u16)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    assert_eq!(marked.len(), 1, "{marked:?}");
    assert!(marked[0].contains("b.txt"), "{marked:?}");
}

#[test]
fn quit_ends_the_interface() {
    let (_fixture, diff) = diff();
    assert!(!apply(&mut State::default(), Input::Quit, &diff));
}

#[test]
fn a_binary_file_says_so_and_has_no_hunks() {
    let (_fixture, diff) = special_diff();
    let paths: Vec<&str> = diff.files().iter().map(|file| file.path()).collect();
    assert_eq!(paths, ["bin.dat", "gone.txt", "new.txt"]);

    let screen = screen(&State::default(), &diff);
    assert!(screen.contains("Binary file differs"), "{screen}");

    // `n` passes over the binary file at once, and `p` steps back onto it.
    let mut state = State::default();
    // The header of `bin.dat` is row 0 and its message row 1; row 2 is empty, and the header of
    // `gone.txt` is row 3.
    apply(&mut state, Input::NextHunk, &diff);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (1, 0, 3));
    apply(&mut state, Input::PreviousHunk, &diff);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 0, 0));
}

#[test]
fn a_deleted_and_a_new_file_write_an_empty_range_as_git_does() {
    let (_fixture, diff) = special_diff();

    let mut state = State::default();
    apply(&mut state, Input::NextFile, &diff);
    let deleted = screen(&state, &diff);
    assert!(deleted.contains("@@ -1 +0,0 @@"), "{deleted}");
    assert!(deleted.contains("-bye"), "{deleted}");
    // A deleted file has an old number and no new one; a new file the other way round.
    assert_eq!(gutter_of(&deleted, 80, "-bye"), "1   ");

    apply(&mut state, Input::NextFile, &diff);
    let added = screen(&state, &diff);
    assert!(added.contains("@@ -0,0 +1 @@"), "{added}");
    // The tab is shown as spaces; ratatui would drop it, and the indentation with it.
    assert!(added.contains("+    indented"), "{added}");
    assert_eq!(gutter_of(&added, 80, "+    indented"), "  1 ");
}

#[test]
fn a_last_line_without_a_line_end_gets_the_marker_of_diff() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "foo");
    fixture.track(&["a.txt"]);
    fixture.commit();
    // Only the line end of the last line changes.
    fixture.dir.write("a.txt", "foo\n");
    let diff = fixture.diff();

    let screen = screen(&State::default(), &diff);
    let rows: Vec<&str> = screen.lines().collect();
    let marker = rows
        .iter()
        .position(|row| row.contains("\\ No newline at end of file"))
        .unwrap_or_else(|| panic!("{screen}"));
    // The marker follows the removed line, and the added line follows the marker.
    assert!(rows[marker - 1].contains("-foo"), "{screen}");
    assert!(rows[marker + 1].contains("+foo"), "{screen}");
}

#[test]
fn control_characters_are_shown_visibly_and_tabs_as_four_spaces() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "x\ty\nkeep\n");
    fixture.track(&["a.txt"]);
    fixture.commit();
    // A tab in the middle, an escape sequence, a `\r\n` line end, and a `\r` inside a line.
    fixture.dir.write("a.txt", "a\tb\x1b[31m\r\np\rq\n");
    let diff = fixture.diff();

    let terminal = terminal_of_size(&State::default(), &diff, 80, 12);
    let controls = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .any(|cell| cell.symbol().chars().any(char::is_control));
    assert!(!controls);

    let screen = screen(&State::default(), &diff);
    // A tab in a removed line and in an added line; the `\r` of the line end is dropped, the
    // escape is written as `^[` so that no bare `[31m` is left.
    assert_eq!(pane_row(&screen, 80, "-x").trim_end(), "1   -x    y");
    assert_eq!(pane_row(&screen, 80, "+a").trim_end(), "  1 +a    b^[[31m");
    // A `\r` inside a line is written as `^M`.
    assert_eq!(pane_row(&screen, 80, "+p").trim_end(), "  2 +p^Mq");
}

#[test]
fn a_selected_file_below_the_visible_list_stays_in_view() {
    let fixture = Fixture::new();
    let names: Vec<String> = (0..30).map(|n| format!("file{n:02}.txt")).collect();
    for name in &names {
        fixture.dir.write(name, "before\n");
    }
    let paths: Vec<&str> = names.iter().map(String::as_str).collect();
    fixture.track(&paths);
    fixture.commit();
    for name in &names {
        fixture.dir.write(name, "after\n");
    }
    let diff = fixture.diff();

    let mut state = State::default();
    for _ in 0..25 {
        apply(&mut state, Input::NextFile, &diff);
    }
    let screen = screen(&state, &diff);
    assert!(screen.contains("file25.txt"), "{screen}");
    assert!(!screen.contains("file00.txt"), "{screen}");
}

#[test]
fn an_empty_diff_and_tiny_terminals_do_not_panic() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "one\n");
    fixture.track(&["a.txt"]);
    fixture.commit();
    let empty = fixture.diff();
    assert!(empty.files().is_empty());

    let mut state = State::default();
    for input in [
        Input::NextFile,
        Input::PreviousFile,
        Input::NextHunk,
        Input::PreviousHunk,
        Input::ScrollDown(5),
        Input::ScrollUp(5),
    ] {
        assert!(apply(&mut state, input, &empty));
    }
    assert_eq!(state, State::default());
    assert!(screen(&state, &empty).contains("No differences"));

    let (_fixture, diff) = diff();
    for (width, height) in [(1, 1), (5, 2), (10, 3), (200, 3)] {
        screen_of_size(&State::default(), &diff, width, height);
        screen_of_size(&State::default(), &empty, width, height);
    }
}

#[test]
fn a_file_above_the_size_limit_says_so_and_has_no_hunks() {
    let fixture = Fixture::new();
    fixture.dir.write("big.txt", "small\n");
    fixture.track(&["big.txt"]);
    fixture.commit();
    fixture.dir.write(
        "big.txt",
        vec![b'a'; stemma_core::diff::DIFF_SIZE_LIMIT + 1],
    );
    let diff = fixture.diff();

    let screen = screen(&State::default(), &diff);

    assert!(screen.contains("File too large to compare"), "{screen}");
    // There is no hunk to step to.
    let mut state = State::default();
    apply(&mut state, Input::NextHunk, &diff);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 0, 0));
}

#[test]
fn a_moved_line_is_drawn_in_its_own_colour() {
    use ratatui::style::Color;
    let block = "fn first_function_name() {}\nfn second_function_name() {}\n";
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", format!("{block}keep\n"));
    fixture.dir.write("b.txt", "stay\n");
    fixture.track(&["a.txt", "b.txt"]);
    fixture.commit();
    fixture.dir.write("a.txt", "keep\n");
    fixture.dir.write("b.txt", format!("stay\n{block}"));
    let diff = fixture.diff();

    // The colour of the first cell of the line that holds `text`, on the screen of `state`.
    let colour_of = |state: &State, text: &str| {
        let terminal = terminal_of_size(state, &diff, 80, 12);
        let buffer = terminal.backend().buffer().clone();
        let area = buffer.area;
        (0..area.height)
            .find_map(|y| {
                let symbols: Vec<&str> = (0..area.width).map(|x| buffer[(x, y)].symbol()).collect();
                let row: String = symbols.concat();
                // The column counts cells, not bytes: the borders are wider than one byte.
                let x = row[..row.find(text)?].chars().count();
                // The mark is the cell before the text.
                let cell = &buffer[(u16::try_from(x).unwrap() - 1, y)];
                assert!(cell.modifier.contains(ratatui::style::Modifier::BOLD));
                Some(cell.fg)
            })
            .unwrap_or_else(|| panic!("`{text}` is not on the screen"))
    };

    assert_eq!(
        colour_of(&State::default(), "fn first_function_name"),
        Color::Magenta
    );
    let mut state = State::default();
    apply(&mut state, Input::NextFile, &diff);
    assert_eq!(colour_of(&state, "fn second_function_name"), Color::Cyan);
    // Only the colour of the text marks a move; there is no background, which a plain removal or
    // addition has.
    let background = |text: &str| {
        let terminal = terminal_of_size(&State::default(), &diff, 80, 12);
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .find_map(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                row.contains(text)
                    .then(|| buffer[(buffer.area.width - 2, y)].bg)
            })
            .unwrap_or_else(|| panic!("`{text}` is not on the screen"))
    };
    assert_eq!(background("fn first_function_name"), Color::Reset);
}

#[test]
fn a_removed_and_an_added_line_have_a_background_and_context_has_none() {
    use ratatui::style::Color;
    let (_fixture, diff) = diff();
    let terminal = terminal_of_size(&State::default(), &diff, 80, 30);
    let buffer = terminal.backend().buffer().clone();
    // The background of the last column of the pane, inside its border, on the row of `needle`.
    let background = |needle: &str| {
        (0..buffer.area.height)
            .find_map(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                row.contains(needle)
                    .then(|| buffer[(buffer.area.width - 2, y)].bg)
            })
            .unwrap_or_else(|| panic!("`{needle}` is not on the screen"))
    };
    let removed = background("-line 3");
    let added = background("+changed 3");
    assert_ne!(removed, Color::Reset);
    assert_ne!(added, Color::Reset);
    assert_ne!(removed, added);
    // Context lines and the rows of a file header carry no background of a change.
    assert_eq!(background("line 4"), Color::Reset);
}

#[test]
fn the_unified_layout_puts_a_removal_above_its_addition() {
    let (_fixture, diff) = diff();
    let screen = screen_of_size(&State::default(), &diff, 80, 30);
    // At 80 columns the layout `auto` is unified: the addition follows its removal.
    let removed = row_index(&screen, "-line 3");
    let added = row_index(&screen, "+changed 3 ");
    assert_eq!(added, removed + 1, "{screen}");
}

#[test]
fn in_the_unified_layout_the_removals_of_a_run_come_before_its_additions() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "keep\none\ntwo\nend\n");
    fixture.track(&["a.txt"]);
    fixture.commit();
    fixture.dir.write("a.txt", "keep\nuno\ndue\nend\n");
    let diff = fixture.diff();
    let screen = screen_of_size(&State::default(), &diff, 80, 30);
    let first = row_index(&screen, "-one");
    assert_eq!(row_index(&screen, "-two"), first + 1, "{screen}");
    assert_eq!(row_index(&screen, "+uno"), first + 2, "{screen}");
    assert_eq!(row_index(&screen, "+due"), first + 3, "{screen}");
}

#[test]
fn the_split_layout_puts_the_old_text_left_and_the_new_text_right() {
    let (_fixture, diff) = diff();
    let screen = screen_of_size(&State::default(), &diff, 200, 30);
    let row = pane_row(&screen, 200, "-line 3");
    let (left, right) = row
        .split_once('│')
        .unwrap_or_else(|| panic!("no separator: {row}"));
    // Each half has its own number: the old number on the left, the new number on the right.
    assert!(left.trim_start().starts_with("3 -line 3"), "{row}");
    assert!(right.trim_start().starts_with("3 +changed 3"), "{row}");
    // A context line stands on both sides.
    let context = pane_row(&screen, 200, "line 1");
    let (left, right) = context.split_once('│').unwrap();
    assert!(left.trim_start().starts_with("1  line 1"), "{context}");
    assert!(right.trim_start().starts_with("1  line 1"), "{context}");
}

#[test]
fn auto_switches_to_split_at_a_pane_of_120_columns() {
    let (_fixture, diff) = diff();
    // The list takes 40 columns and the border of the pane two, so a terminal of 162 columns
    // leaves 120 columns for the lines.
    let at_limit = screen_of_size(&State::default(), &diff, 162, 30);
    assert!(
        pane_row(&at_limit, 162, "-line 3").contains('│'),
        "{at_limit}"
    );
    let below = screen_of_size(&State::default(), &diff, 161, 30);
    assert!(!pane_row(&below, 161, "-line 3").contains('│'), "{below}");
}

#[test]
fn the_keys_force_a_layout_and_zero_goes_back_to_auto() {
    let (_fixture, diff) = diff();
    // Forced split on a narrow terminal, forced unified on a wide one.
    let split = screen_of_size(&with_layout(LayoutMode::Split, &diff), &diff, 80, 30);
    assert!(pane_row(&split, 80, "-line 3").contains('│'), "{split}");
    let unified = screen_of_size(&with_layout(LayoutMode::Unified, &diff), &diff, 200, 30);
    assert!(
        !pane_row(&unified, 200, "-line 3").contains('│'),
        "{unified}"
    );

    let mut state = with_layout(LayoutMode::Split, &diff);
    apply(&mut state, Input::SetLayout(LayoutMode::Auto), &diff);
    let auto = screen_of_size(&state, &diff, 80, 30);
    assert!(!pane_row(&auto, 80, "-line 3").contains('│'), "{auto}");
}

#[test]
fn the_title_of_the_pane_names_the_layout() {
    let (_fixture, diff) = diff();
    let auto = screen_of_size(&State::default(), &diff, 200, 12);
    assert!(auto.contains("Diff · split (auto)"), "{auto}");
    let unified = screen_of_size(&with_layout(LayoutMode::Unified, &diff), &diff, 200, 12);
    assert!(unified.contains("Diff · unified"), "{unified}");
    assert!(!unified.contains("(auto)"), "{unified}");
    let help = screen(&State::default(), &diff);
    assert!(help.contains("1/2/0 layout"), "{help}");
    // The whole help line fits the default width of 80 columns.
    assert!(help.contains("w wrap t theme"), "{help}");
}

#[test]
fn the_layout_changes_the_rows_and_so_the_hunk_starts() {
    let (_fixture, diff) = diff();
    // In the split layout a removal and its addition share a row, so the first hunk takes one row
    // less than in the unified layout: the second hunk starts in row 8, not 9.
    let mut state = with_layout(LayoutMode::Split, &diff);
    apply(&mut state, Input::NextHunk, &diff);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 1, 8));

    // Changing the layout keeps the selected hunk at the top.
    apply(&mut state, Input::SetLayout(LayoutMode::Unified), &diff);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 1, 9));
}

#[test]
fn unequal_runs_of_removals_and_additions_leave_the_short_side_empty() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "keep\none\ntwo\nend\n");
    fixture.track(&["a.txt"]);
    fixture.commit();
    fixture.dir.write("a.txt", "keep\nuno\nend\n");
    let diff = fixture.diff();

    let screen = screen_of_size(&with_layout(LayoutMode::Split, &diff), &diff, 80, 30);
    let first = pane_row(&screen, 80, "-one");
    assert!(first.contains("+uno"), "{screen}");
    let second = pane_row(&screen, 80, "-two");
    let (_, right) = second.split_once('│').unwrap();
    assert!(right.trim().is_empty(), "{second}");
}

#[test]
fn added_and_deleted_files_fill_only_one_side_in_the_split_layout() {
    let (_fixture, diff) = special_diff();
    let mut state = with_layout(LayoutMode::Split, &diff);
    apply(&mut state, Input::NextFile, &diff);
    let screen = screen(&state, &diff);

    let deleted = pane_row(&screen, 80, "-bye");
    let (left, right) = deleted.split_once('│').unwrap();
    assert!(left.trim_start().starts_with("1 -bye"), "{deleted}");
    assert!(right.trim().is_empty(), "{deleted}");

    // The new file is in view after the next key.
    apply(&mut state, Input::NextFile, &diff);
    let screen = self::screen(&state, &diff);
    let added = pane_row(&screen, 80, "+    indented");
    let (left, right) = added.split_once('│').unwrap();
    assert!(left.trim().is_empty(), "{added}");
    assert!(right.trim_start().starts_with("1 +    indented"), "{added}");
}

#[test]
fn the_halves_of_a_split_row_carry_their_own_backgrounds() {
    use ratatui::style::Color;
    let (_fixture, diff) = diff();
    let terminal = terminal_of_size(&with_layout(LayoutMode::Split, &diff), &diff, 200, 30);
    let buffer = terminal.backend().buffer().clone();
    let y = (0..buffer.area.height)
        .find(|&y| {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            row.contains("-line 3")
        })
        .unwrap();
    let separator = (0..buffer.area.width)
        .find(|&x| x > 40 && buffer[(x, y)].symbol() == "│")
        .unwrap();
    // Just left of the separator is the end of the removed line, and just inside the pane border
    // the end of the added line.
    let removed = buffer[(separator - 1, y)].bg;
    let added = buffer[(buffer.area.width - 2, y)].bg;
    assert_ne!(removed, Color::Reset);
    assert_ne!(added, Color::Reset);
    assert_ne!(removed, added);
}

#[test]
fn a_missing_line_end_gets_its_marker_under_the_line_in_the_split_layout() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "foo");
    fixture.track(&["a.txt"]);
    fixture.commit();
    fixture.dir.write("a.txt", "foo\n");
    let diff = fixture.diff();

    // A pane of 158 columns gives each half room for the whole marker.
    let screen = screen_of_size(&with_layout(LayoutMode::Split, &diff), &diff, 200, 30);
    let rows: Vec<&str> = screen.lines().collect();
    let line = rows.iter().position(|row| row.contains("-foo")).unwrap();
    assert!(rows[line].contains("+foo"), "{screen}");
    // The old side has no line end, so the marker stands on the left, in the row below.
    let marker = pane_row(rows[line + 1], 200, "\\ No newline at end of file");
    let (left, right) = marker.split_once('│').unwrap();
    assert!(left.contains("\\ No newline"), "{marker}");
    assert!(right.trim().is_empty(), "{marker}");
}

#[test]
fn both_layouts_survive_tiny_terminals() {
    let (_fixture, diff) = diff();
    for layout in [LayoutMode::Unified, LayoutMode::Split, LayoutMode::Auto] {
        let state = with_layout(layout, &diff);
        for (width, height) in [(1, 1), (2, 2), (5, 2), (9, 4), (30, 5), (200, 3)] {
            screen_of_size(&state, &diff, width, height);
        }
    }
}

/// A diff whose new side has a line of 100 characters, on the first line of `a.txt`.
fn long_line_diff() -> (Fixture, Diff, String) {
    let line = "0123456789".repeat(10);
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "x\n");
    fixture.track(&["a.txt"]);
    fixture.commit();
    fixture.dir.write("a.txt", format!("{line}\n"));
    let diff = fixture.diff();
    (fixture, diff, line)
}

/// A state with line wrapping switched on.
fn wrapping(diff: &Diff) -> State {
    let mut state = State::default();
    apply(&mut state, Input::ToggleWrap, diff);
    state
}

#[test]
fn the_layout_keys_change_the_settings_and_keep_the_selection() {
    let (_fixture, diff) = diff();
    let mut state = State::default();
    assert_eq!(state.layout(), LayoutMode::Auto);
    apply(&mut state, Input::NextHunk, &diff);
    apply(&mut state, Input::NextHunk, &diff);
    assert_eq!((state.file(), state.hunk()), (1, 0));
    for layout in [LayoutMode::Split, LayoutMode::Unified, LayoutMode::Auto] {
        assert!(apply(&mut state, Input::SetLayout(layout), &diff));
        assert_eq!(state.layout(), layout);
        assert_eq!((state.file(), state.hunk()), (1, 0));
    }
    // A resize tells the width and keeps the selection too.
    apply(&mut state, Input::Resize(200), &diff);
    assert_eq!(state.width(), 200);
    assert_eq!((state.file(), state.hunk()), (1, 0));
}

#[test]
fn the_key_w_switches_wrapping_and_the_help_line_says_so() {
    assert_eq!(
        Input::from_key(key(KeyCode::Char('w'))),
        Some(Input::ToggleWrap)
    );
    let (_fixture, diff, _) = long_line_diff();
    let mut state = State::default();
    assert!(!state.wrap());
    apply(&mut state, Input::ToggleWrap, &diff);
    assert!(state.wrap());
    apply(&mut state, Input::ToggleWrap, &diff);
    assert!(!state.wrap());
    assert!(screen(&state, &diff).contains("w wrap"));
    assert!(screen(&wrapping(&diff), &diff).contains("Diff · unified (auto), wrap"));
}

#[test]
fn a_long_line_is_cut_at_the_edge_of_the_pane_without_wrapping() {
    let (_fixture, diff, line) = long_line_diff();
    let screen = screen(&State::default(), &diff);
    let row = pane_row(&screen, 80, "+0123456789");
    // Five columns of numbers and mark; the text fills the rest, and the end of the line is cut.
    assert!(!screen.contains(&line), "{screen}");
    assert!(row[5..].starts_with(&line[..52]), "{row}");
}

#[test]
fn wrapping_continues_a_long_line_in_the_rows_below_without_numbers() {
    let (_fixture, diff, line) = long_line_diff();
    let screen = screen(&wrapping(&diff), &diff);
    let rows: Vec<&str> = screen.lines().collect();
    let at = rows
        .iter()
        .position(|row| row.contains("+0123456789"))
        .unwrap_or_else(|| panic!("{screen}"));
    let first = pane_row(rows[at], 80, "+0123456789");
    let second = pane_row(rows[at + 1], 80, "");
    // The first row has the numbers and the mark, the next one blanks in their place.
    assert!(second[..5].trim().is_empty(), "{screen}");
    let joined = format!("{}{}", first[5..].trim_end(), second[5..].trim_end());
    assert_eq!(joined, line, "{screen}");
}

#[test]
fn a_wrapped_line_has_its_background_on_every_row() {
    use ratatui::style::Color;
    let (_fixture, diff, _) = long_line_diff();
    let terminal = terminal_of_size(&wrapping(&diff), &diff, 80, 12);
    let buffer = terminal.backend().buffer().clone();
    let y = (0..buffer.area.height)
        .find(|&y| {
            let row: String = (0..80).map(|x| buffer[(x, y)].symbol()).collect();
            row.contains("+0123456789")
        })
        .unwrap();
    let last = buffer.area.width - 2;
    assert_ne!(buffer[(last, y)].bg, Color::Reset);
    assert_eq!(buffer[(last, y)].bg, buffer[(last, y + 1)].bg);
}

#[test]
fn wrapping_works_in_the_split_layout_too() {
    let (_fixture, diff, line) = long_line_diff();
    let mut state = wrapping(&diff);
    apply(&mut state, Input::SetLayout(LayoutMode::Split), &diff);
    let screen = screen_of_size(&state, &diff, 200, 12);
    let rows: Vec<&str> = screen.lines().collect();
    let at = rows
        .iter()
        .position(|row| row.contains("+0123456789"))
        .unwrap_or_else(|| panic!("{screen}"));
    // The old side is `x`, and has nothing in the row below; the new side goes on.
    let first = pane_row(rows[at], 200, "+0123456789");
    let second = pane_row(rows[at + 1], 200, "");
    let (_, first_right) = first.split_once('│').unwrap();
    let (second_left, second_right) = second.split_once('│').unwrap();
    assert!(second_left.trim().is_empty(), "{screen}");
    let gutter = 3;
    let joined = format!(
        "{}{}",
        first_right[gutter..].trim_end(),
        second_right[gutter..].trim_end()
    );
    assert_eq!(joined, line, "{screen}");
}

#[test]
fn toggling_wrapping_keeps_the_selected_hunk_at_the_top() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", numbered(&[]));
    fixture.track(&["a.txt"]);
    fixture.commit();
    let long = |n: usize| format!("changed {n} {}\n", "z".repeat(100));
    let text: String = (1..=40)
        .map(|n| match n {
            3 | 36 => long(n),
            _ => format!("line {n}\n"),
        })
        .collect();
    fixture.dir.write("a.txt", text);
    let diff = fixture.diff();

    let mut state = State::default();
    apply(&mut state, Input::NextHunk, &diff);
    let unwrapped = state.scroll();
    apply(&mut state, Input::ToggleWrap, &diff);
    // The first hunk has grown by the rows of its wrapped line, and the second one is still at the
    // top of the pane.
    assert!(state.scroll() > unwrapped, "{state:?}");
    assert_eq!((state.file(), state.hunk()), (0, 1));
    let screen = screen(&state, &diff);
    assert!(
        screen.lines().nth(1).unwrap().contains("@@ -33"),
        "{screen}"
    );
    apply(&mut state, Input::ToggleWrap, &diff);
    assert_eq!(state.scroll(), unwrapped);
}

#[test]
fn wide_characters_wrap_without_crossing_the_edge() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "x\n");
    fixture.track(&["a.txt"]);
    fixture.commit();
    fixture.dir.write("a.txt", format!("{}\n", "漢".repeat(60)));
    let diff = fixture.diff();

    let terminal = terminal_of_size(&wrapping(&diff), &diff, 80, 12);
    let buffer = terminal.backend().buffer().clone();
    let count = buffer
        .content()
        .iter()
        .filter(|cell| cell.symbol() == "漢")
        .count();
    assert_eq!(count, 60);
    // The right border is intact on every row.
    for y in 1..buffer.area.height - 2 {
        assert_eq!(buffer[(79, y)].symbol(), "│", "row {y}");
    }
}

#[test]
fn wrapping_survives_tiny_terminals_and_both_layouts() {
    let (_fixture, diff, _) = long_line_diff();
    for layout in [LayoutMode::Unified, LayoutMode::Split] {
        let mut state = wrapping(&diff);
        apply(&mut state, Input::SetLayout(layout), &diff);
        for (width, height) in [(1, 1), (2, 2), (5, 2), (9, 4), (30, 5), (200, 3)] {
            screen_of_size(&state, &diff, width, height);
        }
    }
}

/// A diff of one line that has no line end, and gets one.
fn no_newline_diff() -> (Fixture, Diff) {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "foo");
    fixture.track(&["a.txt"]);
    fixture.commit();
    fixture.dir.write("a.txt", "foo\n");
    let diff = fixture.diff();
    (fixture, diff)
}

#[test]
fn the_marker_wraps_in_the_split_layout_and_keeps_the_separator_in_place() {
    let (_fixture, diff) = no_newline_diff();
    let mut state = wrapping(&diff);
    apply(&mut state, Input::SetLayout(LayoutMode::Split), &diff);
    // A terminal of 40 columns: the file list takes 10, the pane has 28 inside its border, and
    // each half has 13, too narrow for the marker of 27 columns.
    let screen = screen_of_size(&state, &diff, 40, 14);
    let rows: Vec<Vec<char>> = screen.lines().map(|row| row.chars().collect()).collect();
    // Row 0 is the border, row 1 the header, row 2 the `@@` line; then the lines follow.
    let separators: Vec<Option<usize>> = rows[3..rows.len() - 2]
        .iter()
        .map(|row| row[11..39].iter().position(|&c| c == '│'))
        .take_while(Option::is_some)
        .collect();
    assert!(separators.len() >= 4, "{screen}");
    assert!(separators.iter().all(|at| *at == separators[0]), "{screen}");
    for row in &rows[1..rows.len() - 2] {
        assert_eq!(row[39], '│', "{screen}");
    }
    // The old side holds the whole marker, cut into pieces.
    let left: String = rows[3..rows.len() - 2]
        .iter()
        .map(|row| {
            row[11..11 + separators[0].unwrap()]
                .iter()
                .collect::<String>()
        })
        .collect();
    let squeezed: String = left.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(squeezed.contains("\\Nonewlineatendoffile"), "{screen}");
}

#[test]
fn the_marker_wraps_in_the_unified_layout_and_is_cut_without_wrapping() {
    let (_fixture, diff) = no_newline_diff();
    let screen = screen_of_size(&wrapping(&diff), &diff, 40, 14);
    let squeezed: String = screen
        .lines()
        .flat_map(|row| row.chars().skip(11).take(28))
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(squeezed.contains("\\Nonewlineatendoffile"), "{squeezed}");

    let screen = screen_of_size(&State::default(), &diff, 40, 14);
    let row = pane_row(&screen, 40, "\\ No newline");
    assert!(row.trim_end().ends_with('…'), "{row}");
}

#[test]
fn a_long_line_that_is_cut_ends_in_an_ellipsis() {
    let (_fixture, diff, line) = long_line_diff();
    let screen = screen(&State::default(), &diff);
    let row = pane_row(&screen, 80, "+0123456789");
    assert_eq!(row[5..].trim_end(), format!("{}…", &line[..52]), "{row}");
}

#[test]
fn a_wide_character_that_does_not_fit_is_shown_as_an_ellipsis() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "x\n");
    fixture.track(&["a.txt"]);
    fixture.commit();
    fixture.dir.write("a.txt", "漢\n");
    let diff = fixture.diff();
    // A terminal of 14 columns: each half of the split layout has one column for the text.
    let screen = screen_of_size(&with_layout(LayoutMode::Split, &diff), &diff, 14, 12);
    // The `…` stands in the row of the added line, right of the separator, not in the header.
    let rows: Vec<String> = screen
        .lines()
        .map(|row| row.chars().skip(4).take(9).collect())
        .collect();
    let row = rows
        .iter()
        .find(|row| row.contains('│') && row.contains('+'))
        .unwrap_or_else(|| panic!("{screen}"));
    let (_, right) = row.split_once('│').unwrap();
    assert!(right.contains('…'), "{screen}");
    assert!(!screen.contains('漢'), "{screen}");
}

#[test]
fn a_delete_and_a_c1_control_character_are_shown_in_caret_notation() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "x\n");
    fixture.track(&["a.txt"]);
    fixture.commit();
    fixture.dir.write("a.txt", "a\u{7f}b\u{85}c\n");
    let diff = fixture.diff();
    let screen = screen(&State::default(), &diff);
    assert_eq!(pane_row(&screen, 80, "+a").trim_end(), "  1 +a^?b^@c");
}

#[test]
fn a_path_that_is_longer_than_the_pane_is_cut_and_the_counts_stay_visible() {
    let fixture = Fixture::new();
    let name = format!("{}.txt", "x".repeat(70));
    fixture.dir.write(&name, "one\n");
    fixture.track(&[&name]);
    let diff = fixture.diff();
    let screen = screen_of_size(&State::default(), &diff, 80, 12);
    let header = pane_row(&screen, 80, "xxxx");
    assert!(header.contains('…'), "{screen}");
    assert!(header.trim_end().ends_with("+1 -0"), "{screen}");
}

#[test]
fn the_header_says_new_file_and_deleted_and_counts_the_lines() {
    let (_fixture, diff) = special_diff();
    let screen = screen_of_size(&State::default(), &diff, 80, 30);
    let deleted = pane_row(&screen, 80, "gone.txt");
    assert!(deleted.contains("gone.txt (deleted)"), "{screen}");
    assert!(deleted.contains("+0 -1"), "{screen}");
    let added = pane_row(&screen, 80, "new.txt");
    assert!(added.contains("new.txt (new file)"), "{screen}");
    assert!(added.contains("+1 -0"), "{screen}");
}

#[test]
fn context_lines_carry_their_old_and_their_new_number() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "keep\none\ntwo\nend\n");
    fixture.track(&["a.txt"]);
    fixture.commit();
    fixture.dir.write("a.txt", "keep\nuno\nend\n");
    let diff = fixture.diff();

    // `end` is line 4 of the old file and line 3 of the new one.
    let unified = screen_of_size(&State::default(), &diff, 80, 30);
    assert_eq!(gutter_of(&unified, 80, "keep"), "1 1  ");
    assert_eq!(gutter_of(&unified, 80, "-one"), "2   ");
    assert_eq!(gutter_of(&unified, 80, "-two"), "3   ");
    assert_eq!(gutter_of(&unified, 80, "+uno"), "  2 ");
    assert_eq!(gutter_of(&unified, 80, "end"), "4 3  ");

    let split = screen_of_size(&with_layout(LayoutMode::Split, &diff), &diff, 80, 30);
    let row = pane_row(&split, 80, "end");
    let (left, right) = row.split_once('│').unwrap();
    assert!(left.starts_with("4  end"), "{row}");
    assert!(right.starts_with("3  end"), "{row}");
}

#[test]
fn the_number_column_is_as_wide_as_the_longest_file() {
    // Three and five digits; one digit is covered by the tests above.
    for (lines, digits) in [(120_usize, 3_usize), (12_000, 5)] {
        let fixture = Fixture::new();
        let text = |prefix: &str| -> String {
            (1..=lines)
                .map(|n| {
                    if n == lines {
                        format!("{prefix}{n}\n")
                    } else {
                        format!("l{n}\n")
                    }
                })
                .collect()
        };
        fixture.dir.write("a.txt", text("o"));
        fixture.track(&["a.txt"]);
        fixture.commit();
        fixture.dir.write("a.txt", text("n"));
        let diff = fixture.diff();
        let screen = screen_of_size(&State::default(), &diff, 80, 30);
        // The old number, a blank for the new one, and the gap before the mark (the needle holds
        // the mark).
        let removed = format!("{lines:>digits$} {:digits$} ", "");
        assert_eq!(gutter_of(&screen, 80, &format!("-o{lines}")), removed);
        let added = format!("{:digits$} {lines:>digits$} ", "");
        assert_eq!(gutter_of(&screen, 80, &format!("+n{lines}")), added);
    }
}

#[test]
fn scrolling_does_not_change_the_selected_hunk() {
    let (_fixture, diff) = diff();
    let mut stream = Stream::new(&diff);

    // Scrolled into the second hunk by `j`, the selection is still the first: `n` selects the
    // second hunk and puts its header at the top.
    let mut state = State::default();
    state.apply(Input::ScrollDown(12), &diff, &mut stream);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 0, 12));
    state.apply(Input::NextHunk, &diff, &mut stream);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 1, 9));

    // Scrolled back into the first hunk, the selection is still the second: `p` selects the
    // first hunk.
    state.apply(Input::ScrollUp(5), &diff, &mut stream);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 1, 4));
    state.apply(Input::PreviousHunk, &diff, &mut stream);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 0, 1));
}

#[test]
fn the_last_row_of_a_stream_of_more_than_65535_rows_can_be_reached() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "old\n");
    fixture.track(&["a.txt"]);
    fixture.commit();
    // Each line of 180 characters takes four rows at 80 columns with wrapping (45 columns for the
    // text), so 17000 lines are more than 65535 rows, and the diff stays cheap for the core.
    let text: String = (1..=17_000)
        .map(|n| {
            let tail = if n == 17_000 { "ENDX" } else { "zzzz" };
            format!("{}{tail}\n", "z".repeat(176))
        })
        .collect();
    fixture.dir.write("a.txt", text);
    let diff = fixture.diff();

    let mut stream = Stream::new(&diff);
    let mut state = wrapping(&diff);
    for _ in 0..3 {
        state.apply(Input::ScrollDown(u16::MAX), &diff, &mut stream);
    }
    assert!(stream.len() > 65_535, "{}", stream.len());
    assert!(state.scroll() > 65_535, "{}", state.scroll());
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    terminal
        .draw(|frame| render_with(frame, &state, &diff, &mut stream))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let screen: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
    assert!(screen.contains("ENDX"), "{screen}");
}

#[test]
fn the_state_decides_the_width_of_the_stream_not_the_frame() {
    let (_fixture, diff) = diff();
    // A state that has been told 80 columns lays the stream out for 80, also in a frame of 200:
    // the scroll position of the state refers to exactly these rows.
    let mut stream = Stream::new(&diff);
    let mut terminal = Terminal::new(TestBackend::new(200, 12)).unwrap();
    terminal
        .draw(|frame| render_with(frame, &State::default(), &diff, &mut stream))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let screen: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
    assert!(screen.contains("Diff · unified (auto)"), "{screen}");
}

#[test]
fn render_takes_the_width_of_the_frame() {
    let (_fixture, diff) = diff();
    let mut terminal = Terminal::new(TestBackend::new(200, 12)).unwrap();
    terminal
        .draw(|frame| render(frame, &State::default(), &diff))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let screen: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
    assert!(screen.contains("Diff · split (auto)"), "{screen}");
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "another diff")]
fn a_stream_that_is_used_with_another_diff_is_caught() {
    let (_fixture, diff) = diff();
    let (_other_fixture, other) = special_diff();
    let mut stream = Stream::new(&diff);
    stream.fit(&other, 80, LayoutMode::Auto, false);
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn click(column: u16, row: u16) -> Input {
    Input::from_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row)).unwrap()
}

/// Draws one frame of 80 columns and 12 rows with `stream`, as the loop does before it reads.
fn draw_frame(state: &State, diff: &Diff, stream: &mut Stream) {
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    terminal
        .draw(|frame| render_with(frame, state, diff, stream))
        .unwrap();
}

#[test]
fn the_wheel_scrolls_the_stream_like_the_keys() {
    let (_fixture, diff) = diff();
    let down = Input::from_mouse(mouse(MouseEventKind::ScrollDown, 40, 5)).unwrap();
    let up = Input::from_mouse(mouse(MouseEventKind::ScrollUp, 40, 5)).unwrap();
    assert_eq!(down, Input::ScrollDown(3));
    assert_eq!(up, Input::ScrollUp(3));

    let mut stream = Stream::new(&diff);
    let mut state = State::default();
    state.apply(down, &diff, &mut stream);
    assert_eq!(state.scroll(), 3);
    state.apply(up, &diff, &mut stream);
    state.apply(up, &diff, &mut stream);
    assert_eq!(state.scroll(), 0);
}

#[test]
fn other_mouse_events_are_not_used() {
    for kind in [
        MouseEventKind::Down(MouseButton::Right),
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Moved,
        MouseEventKind::ScrollLeft,
    ] {
        assert_eq!(Input::from_mouse(mouse(kind, 2, 2)), None, "{kind:?}");
    }
}

#[test]
fn a_click_on_a_file_jumps_to_its_header() {
    let (_fixture, diff) = diff();
    let mut stream = Stream::new(&diff);
    let mut state = State::default();
    draw_frame(&state, &diff, &mut stream);

    // The list has its border in row 0 and column 0, so `b.txt` is in row 2.
    assert!(state.apply(click(3, 2), &diff, &mut stream));
    assert_eq!((state.file(), state.hunk()), (1, 0));
    assert_eq!(Some(state.scroll()), stream.file_start(1));

    // A click on the first file goes back to its header.
    assert!(state.apply(click(1, 1), &diff, &mut stream));
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 0, 0));
}

#[test]
fn a_click_outside_the_names_changes_nothing() {
    let (_fixture, diff) = diff();
    let mut stream = Stream::new(&diff);
    let mut state = State::default();
    state.apply(Input::ScrollDown(5), &diff, &mut stream);
    draw_frame(&state, &diff, &mut stream);
    let before = state;

    for (column, row) in [
        (3, 0),     // the top border of the list
        (0, 1),     // the left border
        (19, 1),    // the right border
        (3, 3),     // below the last name
        (3, 10),    // the empty part of the list
        (40, 1),    // the diff pane
        (3, 11),    // the help line
        (200, 200), // outside the terminal
    ] {
        assert!(state.apply(click(column, row), &diff, &mut stream));
        assert_eq!(state, before, "({column}, {row})");
    }
}

#[test]
fn a_click_before_the_first_frame_changes_nothing() {
    let (_fixture, diff) = diff();
    let mut stream = Stream::new(&diff);
    let mut state = State::default();
    assert!(state.apply(click(3, 2), &diff, &mut stream));
    assert_eq!(state, State::default());
}

#[test]
fn a_click_on_a_scrolled_list_selects_the_file_in_that_row() {
    let fixture = Fixture::new();
    let names: Vec<String> = (0..20).map(|n| format!("f{n:02}.txt")).collect();
    for name in &names {
        fixture.dir.write(name, "old\n");
    }
    let paths: Vec<&str> = names.iter().map(String::as_str).collect();
    fixture.track(&paths);
    fixture.commit();
    for name in &names {
        fixture.dir.write(name, "new\n");
    }
    let diff = fixture.diff();
    let mut stream = Stream::new(&diff);
    let mut state = State::default();
    state.apply(Input::Resize(80), &diff, &mut stream);
    // Selecting the last file scrolls the list of the 12-row terminal (9 name rows).
    for _ in 0..19 {
        state.apply(Input::NextFile, &diff, &mut stream);
    }
    draw_frame(&state, &diff, &mut stream);

    // The first name row is not file 0 any more: 20 files, 9 rows, the last one selected.
    state.apply(click(3, 1), &diff, &mut stream);
    assert_eq!(state.file(), 11);
    state.apply(click(3, 9), &diff, &mut stream);
    assert_eq!(state.file(), 19);
}

#[test]
fn a_wheel_past_the_next_header_changes_the_file_and_stops_at_the_end() {
    let (_fixture, diff) = diff();
    let mut stream = Stream::new(&diff);
    let mut state = State::default();
    let wheel = Input::from_mouse(mouse(MouseEventKind::ScrollDown, 40, 5)).unwrap();
    for _ in 0..4 {
        state.apply(wheel, &diff, &mut stream);
    }
    assert_eq!((state.file(), state.hunk()), (0, 0));
    for _ in 0..100 {
        state.apply(wheel, &diff, &mut stream);
    }
    assert_eq!((state.file(), state.scroll()), (1, stream.len() - 1));
}

/// A diff of a Rust file whose one line changes from a comment into code with a keyword and a
/// string, and a text file that has the same lines.
fn highlight_diff() -> (Fixture, Diff) {
    let fixture = Fixture::new();
    fixture.dir.write("a.rs", "// old\nlet kept = 1;\n");
    fixture.dir.write("b.txt", "pub fn x() {}\n");
    fixture.track(&["a.rs", "b.txt"]);
    fixture.commit();
    fixture.dir.write(
        "a.rs",
        "pub fn name() -> &'static str { \"hi\" }\nlet kept = 1;\n",
    );
    fixture.dir.write("b.txt", "pub fn y() {}\n");
    let diff = fixture.diff();
    (fixture, diff)
}

/// The foreground colours of the cells that show `text` on the screen of `state`.
fn colours_of(state: &State, diff: &Diff, text: &str) -> Vec<ratatui::style::Color> {
    let terminal = terminal_of_size(state, diff, 100, 20);
    let buffer = terminal.backend().buffer().clone();
    for y in 0..buffer.area.height {
        let cells: Vec<&str> = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        let row = cells.concat();
        if let Some(at) = row.find(text) {
            let x = row[..at].chars().count();
            return (x..x + text.chars().count())
                .map(|x| buffer[(u16::try_from(x).unwrap(), y)].fg)
                .collect();
        }
    }
    panic!("`{text}` is not on the screen");
}

#[test]
fn a_rust_file_is_highlighted_and_a_text_file_is_not() {
    let (_fixture, diff) = highlight_diff();
    let state = State::default();
    let keyword = colours_of(&state, &diff, "pub fn");
    let string = colours_of(&state, &diff, "\"hi\"");
    // The space between the two keywords has no colour of its own.
    assert!(keyword[..3].iter().all(|c| *c == keyword[0]), "{keyword:?}");
    assert!(keyword[4..].iter().all(|c| *c == keyword[0]), "{keyword:?}");
    assert!(string.iter().all(|c| *c == string[0]), "{string:?}");
    assert_ne!(keyword[0], string[0]);
    assert_ne!(keyword[0], ratatui::style::Color::White);
    // The context line keeps its own colours, and the file without a known language none.
    let plain = colours_of(&state, &diff, "pub fn y");
    assert!(plain.iter().all(|c| *c == ratatui::style::Color::White));
}

#[test]
fn the_theme_key_cycles_dark_light_plain_and_back() {
    use ratatui::style::Color;
    let (_fixture, diff) = highlight_diff();
    let mut state = State::default();
    assert_eq!(state.theme(), Theme::Dark);
    let dark = colours_of(&state, &diff, "pub fn")[0];
    apply(&mut state, Input::CycleTheme, &diff);
    assert_eq!(state.theme(), Theme::Light);
    let light = colours_of(&state, &diff, "pub fn")[0];
    assert_ne!(light, dark);
    assert!(screen(&state, &diff).contains("light"));
    apply(&mut state, Input::CycleTheme, &diff);
    assert_eq!(state.theme(), Theme::Plain);
    assert_eq!(colours_of(&state, &diff, "pub fn"), vec![Color::White; 6]);
    assert!(screen(&state, &diff).contains("plain"));
    apply(&mut state, Input::CycleTheme, &diff);
    assert_eq!(state.theme(), Theme::Dark);
    assert_eq!(colours_of(&state, &diff, "pub fn")[0], dark);
    assert_eq!(
        Input::from_key(key(KeyCode::Char('t'))),
        Some(Input::CycleTheme)
    );
}

#[test]
fn the_highlighting_keeps_the_text_the_widths_and_the_wrapping() {
    let (_fixture, diff) = highlight_diff();
    let mut plain = State::default();
    apply(&mut plain, Input::CycleTheme, &diff);
    apply(&mut plain, Input::CycleTheme, &diff);
    for (layout, wrap) in [
        (LayoutMode::Unified, false),
        (LayoutMode::Unified, true),
        (LayoutMode::Split, false),
        (LayoutMode::Split, true),
    ] {
        let mut dark = with_layout(layout, &diff);
        let mut off = with_layout(layout, &diff);
        apply(&mut off, Input::CycleTheme, &diff);
        apply(&mut off, Input::CycleTheme, &diff);
        if wrap {
            apply(&mut dark, Input::ToggleWrap, &diff);
            apply(&mut off, Input::ToggleWrap, &diff);
        }
        for width in [20, 40, 100] {
            // Only the colours differ, so the text of the screens is the same except the title.
            let strip = |screen: String| {
                screen
                    .lines()
                    .filter(|line| !line.contains("Diff ·"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            assert_eq!(
                strip(screen_of_size(&dark, &diff, width, 20)),
                strip(screen_of_size(&off, &diff, width, 20)),
                "{layout:?} wrap {wrap} width {width}"
            );
        }
    }
}

#[test]
fn a_moved_line_keeps_its_colour_in_a_highlighted_file() {
    let fixture = Fixture::new();
    let block = "pub fn first_function_name() {}\npub fn second_function_name() {}\n";
    fixture.dir.write("a.rs", format!("{block}keep\n"));
    fixture.dir.write("b.rs", "stay\n");
    fixture.track(&["a.rs", "b.rs"]);
    fixture.commit();
    fixture.dir.write("a.rs", "keep\n");
    fixture.dir.write("b.rs", format!("stay\n{block}"));
    let diff = fixture.diff();
    let colours = colours_of(&State::default(), &diff, "pub fn first_function_name");
    assert!(colours.iter().all(|c| *c == ratatui::style::Color::Magenta));
}

/// The foreground and the background of the cells that show `text` on the screen of `state`, drawn
/// with the persistent `stream`.
fn cells_of(
    state: &State,
    diff: &Diff,
    stream: &mut Stream,
    text: &str,
    extra: usize,
) -> Vec<(ratatui::style::Color, ratatui::style::Color)> {
    let mut state = *state;
    state.apply(Input::Resize(100), diff, stream);
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    terminal
        .draw(|frame| render_with(frame, &state, diff, stream))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    for y in 0..buffer.area.height {
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        if let Some(at) = row.find(text) {
            let x = row[..at].chars().count();
            return (x..x + text.chars().count() + extra)
                .map(|x| {
                    let cell = &buffer[(u16::try_from(x).unwrap(), y)];
                    (cell.fg, cell.bg)
                })
                .collect();
        }
    }
    panic!("`{text}` is not on the screen");
}

#[test]
fn one_stream_follows_the_themes_through_a_whole_cycle() {
    use ratatui::style::Color;
    let (_fixture, diff) = highlight_diff();
    let mut stream = Stream::new(&diff);
    let mut state = State::default();
    let dark = cells_of(&state, &diff, &mut stream, "+pub fn", 0)[1];
    assert_eq!(dark.1, Color::Indexed(22));
    for expected_background in [194, 22, 22] {
        state.apply(Input::CycleTheme, &diff, &mut stream);
        let cell = cells_of(&state, &diff, &mut stream, "+pub fn", 0)[1];
        match state.theme() {
            Theme::Light => {
                assert_eq!(cell.1, Color::Indexed(expected_background));
                assert_ne!(cell.0, dark.0);
            }
            Theme::Plain => {
                assert_eq!(cell, (Color::White, Color::Indexed(expected_background)));
            }
            _ => assert_eq!(cell, dark),
        }
    }
}

#[test]
fn removed_and_context_lines_are_coloured_from_their_own_side() {
    use ratatui::style::Color;
    let (_fixture, diff) = highlight_diff();
    let mut stream = Stream::new(&diff);
    let state = State::default();
    let comment = colours_of(&state, &diff, "// old")[0];
    let removed = cells_of(&state, &diff, &mut stream, "// old", 0);
    assert!(removed
        .iter()
        .all(|(fg, bg)| *fg == comment && *bg == Color::Indexed(52)));
    let keyword = cells_of(&state, &diff, &mut stream, "let kept", 0);
    assert_ne!(keyword[0].0, keyword[4].0);
}

#[test]
fn colours_follow_tabs_wide_characters_and_wrapping() {
    use ratatui::style::Color;
    let fixture = Fixture::new();
    fixture.dir.write("a.rs", "x\n");
    fixture.track(&["a.rs"]);
    fixture.commit();
    fixture
        .dir
        .write("a.rs", "\tlet s = \"日本語日本語日本語日本語\"; // end\n");
    let diff = fixture.diff();
    for layout in [LayoutMode::Unified, LayoutMode::Split] {
        for wrap in [false, true] {
            let mut state = with_layout(layout, &diff);
            if wrap {
                apply(&mut state, Input::ToggleWrap, &diff);
            }
            let mut stream = Stream::new(&diff);
            // The tab is four spaces, and the keyword after them is coloured.
            let keyword = cells_of(&state, &diff, &mut stream, "let", 0);
            assert!(
                keyword.iter().all(|c| c.0 != Color::White),
                "{layout:?} {wrap}"
            );
            let spaces = cells_of(&state, &diff, &mut stream, "    let", 0);
            assert_eq!(spaces[0].0, Color::White, "{layout:?} {wrap}");
            // The wide characters of the string share one colour, whatever the layout and the
            // wrapping.
            let string = cells_of(&state, &diff, &mut stream, "let s = \"", 8);
            // Ratatui leaves the second cell of a wide character without a style of its own.
            let inside: Vec<_> = string[9..].iter().step_by(2).collect();
            assert!(
                inside.iter().all(|c| c.0 == inside[0].0),
                "{layout:?} {wrap}"
            );
            assert_ne!(inside[0].0, Color::White);
        }
    }
}

#[test]
fn a_context_line_is_coloured_from_the_text_on_each_side_in_the_split_layout() {
    let fixture = Fixture::new();
    fixture.dir.write("a.rs", "fn a() {}\nlet x = 1;\n");
    fixture.track(&["a.rs"]);
    fixture.commit();
    // An opening comment above the unchanged line turns it into a comment on the new side only.
    fixture
        .dir
        .write("a.rs", "fn a() {}\n/* open\nlet x = 1;\n");
    let diff = fixture.diff();
    let state = with_layout(LayoutMode::Split, &diff);
    let terminal = terminal_of_size(&state, &diff, 120, 12);
    let buffer = terminal.backend().buffer().clone();
    let row = (0..buffer.area.height)
        .find(|&y| {
            let text: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            text.matches("let x").count() == 2
        })
        .expect("a row with both sides of the context line");
    let cells: Vec<(usize, ratatui::style::Color)> = (0..buffer.area.width)
        .filter(|&x| buffer[(x, row)].symbol() == "l")
        .map(|x| (usize::from(x), buffer[(x, row)].fg))
        .collect();
    assert_eq!(cells.len(), 2, "{cells:?}");
    assert_ne!(cells[0].1, cells[1].1, "{cells:?}");
}
