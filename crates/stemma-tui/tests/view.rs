//! The view of `stemma diff --tui` on a real diff, drawn into the `TestBackend` of ratatui, so that
//! no terminal is needed (ADR-016).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use stemma_core::repository::{Comparison, Diff, InitOptions, Repository};
use stemma_core::{Actor, AllowAll};
use stemma_tui::{render, Input, State};

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

fn screen_of_size(state: &State, diff: &Diff, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render(frame, state, diff)).unwrap();
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
fn hunks_and_files_are_walked_in_order() {
    let (_fixture, diff) = diff();
    let mut state = State::default();

    // At the very start there is no previous hunk.
    state.apply(Input::PreviousHunk, &diff);
    assert_eq!(state, State::default());

    // The second hunk starts after the header and the six lines of the first: line 8.
    assert!(state.apply(Input::NextHunk, &diff));
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 1, 8));
    let screen = screen(&state, &diff);
    assert!(screen.contains("+changed 36"), "{screen}");
    // The first hunk has scrolled out; a row ends in spaces, so `+changed 36` does not match.
    assert!(
        !screen.lines().any(|row| row.contains("+changed 3 ")),
        "{screen}"
    );

    // After the last hunk of `a.txt`, the next hunk is the first of `b.txt`.
    state.apply(Input::NextHunk, &diff);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (1, 0, 0));

    // At the end nothing moves on; going back lands on the last hunk of the previous file, scrolled
    // to its header.
    state.apply(Input::NextHunk, &diff);
    assert_eq!((state.file(), state.hunk()), (1, 0));
    state.apply(Input::PreviousHunk, &diff);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 1, 8));
}

#[test]
fn the_files_are_selected_one_by_one_and_stop_at_the_ends() {
    let (_fixture, diff) = diff();
    let mut state = State::default();
    state.apply(Input::PreviousFile, &diff);
    assert_eq!(state.file(), 0);
    state.apply(Input::ScrollDown(3), &diff);
    state.apply(Input::NextFile, &diff);
    assert_eq!((state.file(), state.scroll()), (1, 0));
    state.apply(Input::NextFile, &diff);
    assert_eq!(state.file(), 1);
    assert!(screen(&state, &diff).contains("+two"));
}

#[test]
fn scrolling_stops_at_the_top_and_at_the_last_line() {
    let (_fixture, diff) = diff();
    let mut state = State::default();
    state.apply(Input::ScrollUp(5), &diff);
    assert_eq!(state.scroll(), 0);
    state.apply(Input::ScrollDown(3), &diff);
    assert_eq!(state.scroll(), 3);

    // Far past the end, the last line stays in view instead of an empty pane. The file has two
    // hunks of 8 and 9 lines, so its last line is line 16.
    state.apply(Input::ScrollDown(u16::MAX), &diff);
    assert_eq!(state.scroll(), 16);
    state.apply(Input::ScrollDown(1), &diff);
    assert_eq!(state.scroll(), 16);
    assert!(screen(&state, &diff).contains(" line 39"));
}

#[test]
fn quit_ends_the_interface() {
    let (_fixture, diff) = diff();
    assert!(!State::default().apply(Input::Quit, &diff));
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
    state.apply(Input::NextHunk, &diff);
    assert_eq!((state.file(), state.hunk()), (1, 0));
    state.apply(Input::PreviousHunk, &diff);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (0, 0, 0));
}

#[test]
fn a_deleted_and_a_new_file_write_an_empty_range_as_git_does() {
    let (_fixture, diff) = special_diff();

    let mut state = State::default();
    state.apply(Input::NextFile, &diff);
    let deleted = screen(&state, &diff);
    assert!(deleted.contains("@@ -1 +0,0 @@"), "{deleted}");
    assert!(deleted.contains("-bye"), "{deleted}");

    state.apply(Input::NextFile, &diff);
    let added = screen(&state, &diff);
    assert!(added.contains("@@ -0,0 +1 @@"), "{added}");
    // The tab is shown as spaces; ratatui would drop it, and the indentation with it.
    assert!(added.contains("+    indented"), "{added}");
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
fn control_characters_in_a_file_do_not_reach_the_terminal() {
    let fixture = Fixture::new();
    fixture.dir.write("a.txt", "one\n");
    fixture.track(&["a.txt"]);
    fixture.commit();
    fixture.dir.write("a.txt", "two\x1b[2J\r\n");
    let diff = fixture.diff();

    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    terminal
        .draw(|frame| render(frame, &State::default(), &diff))
        .unwrap();
    let controls = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .any(|cell| cell.symbol().chars().any(char::is_control));
    assert!(!controls);
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
        state.apply(Input::NextFile, &diff);
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
        assert!(state.apply(input, &empty));
    }
    assert_eq!(state, State::default());
    assert!(screen(&state, &empty).contains("No differences"));

    let (_fixture, diff) = diff();
    for (width, height) in [(1, 1), (5, 2), (10, 3), (200, 3)] {
        screen_of_size(&State::default(), &diff, width, height);
        screen_of_size(&State::default(), &empty, width, height);
    }
}
