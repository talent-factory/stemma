//! The view of `stemma diff --tui` on a real diff, drawn into the `TestBackend` of ratatui, so that
//! no terminal is needed (ADR-016).

use std::fs;
use std::path::{Path, PathBuf};

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use stemma_core::repository::{Comparison, Diff, InitOptions, Repository};
use stemma_core::{Actor, AllowAll};
use stemma_tui::{render, Input, State};

/// A directory below the system temp directory that is removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("stemma-tui-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
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
fn diff() -> (TempDir, Diff) {
    let dir = TempDir::new(&format!("{:?}", std::thread::current().id()).replace(['(', ')'], ""));
    let actor: Actor = "human alice".parse().unwrap();
    let repository = Repository::init(dir.path(), &InitOptions::default(), &actor, &AllowAll)
        .expect("init the repository");
    fs::write(dir.path().join("a.txt"), numbered(&[])).unwrap();
    fs::write(dir.path().join("b.txt"), "one\n").unwrap();
    repository
        .track(&["a.txt".into(), "b.txt".into()], &actor, &AllowAll)
        .expect("track the files");
    let _ = repository
        .commit("first", &actor, &AllowAll)
        .expect("commit");
    fs::write(dir.path().join("a.txt"), numbered(&[3, 36])).unwrap();
    fs::write(dir.path().join("b.txt"), "two\n").unwrap();
    let diff = repository
        .diff(&actor, &AllowAll, Comparison::WithParent)
        .expect("diff");
    (dir, diff)
}

fn screen(state: &State, diff: &Diff) -> String {
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    terminal.draw(|frame| render(frame, state, diff)).unwrap();
    let buffer = terminal.backend().buffer();
    buffer
        .content()
        .chunks(usize::from(buffer.area.width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn keys_map_to_inputs() {
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    assert_eq!(Input::from_key(key(KeyCode::Char('q'))), Some(Input::Quit));
    assert_eq!(Input::from_key(key(KeyCode::Esc)), Some(Input::Quit));
    assert_eq!(
        Input::from_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(Input::Quit)
    );
    assert_eq!(
        Input::from_key(key(KeyCode::Char('n'))),
        Some(Input::NextHunk)
    );
    assert_eq!(
        Input::from_key(key(KeyCode::Down)),
        Some(Input::ScrollDown(1))
    );
    assert_eq!(Input::from_key(key(KeyCode::Char('x'))), None);
}

#[test]
fn the_view_lists_the_files_and_shows_the_first() {
    let (_dir, diff) = diff();
    let screen = screen(&State::default(), &diff);
    assert!(screen.contains("a.txt"), "{screen}");
    assert!(screen.contains("b.txt"), "{screen}");
    assert!(screen.contains("@@ -1,6 +1,6 @@"), "{screen}");
    assert!(screen.contains("-line 3"), "{screen}");
    assert!(screen.contains("+changed 3"), "{screen}");
}

#[test]
fn hunks_and_files_are_walked_in_order() {
    let (_dir, diff) = diff();
    let mut state = State::default();

    assert!(state.apply(Input::NextHunk, &diff));
    assert_eq!((state.file(), state.hunk()), (0, 1));
    assert!(state.scroll() > 0, "the second hunk is below the first");
    let screen = screen(&state, &diff);
    assert!(screen.contains("+changed 36"), "{screen}");

    // After the last hunk of `a.txt`, the next hunk is the first of `b.txt`.
    state.apply(Input::NextHunk, &diff);
    assert_eq!((state.file(), state.hunk(), state.scroll()), (1, 0, 0));

    // At the end nothing moves on; going back lands on the last hunk of the previous file.
    state.apply(Input::NextHunk, &diff);
    assert_eq!((state.file(), state.hunk()), (1, 0));
    state.apply(Input::PreviousHunk, &diff);
    assert_eq!((state.file(), state.hunk()), (0, 1));
}

#[test]
fn the_files_are_selected_one_by_one_and_stop_at_the_ends() {
    let (_dir, diff) = diff();
    let mut state = State::default();
    state.apply(Input::PreviousFile, &diff);
    assert_eq!(state.file(), 0);
    state.apply(Input::NextFile, &diff);
    state.apply(Input::NextFile, &diff);
    assert_eq!(state.file(), 1);
    assert!(screen(&state, &diff).contains("+two"));
}

#[test]
fn scrolling_does_not_go_below_the_top() {
    let (_dir, diff) = diff();
    let mut state = State::default();
    state.apply(Input::ScrollUp(5), &diff);
    assert_eq!(state.scroll(), 0);
    state.apply(Input::ScrollDown(3), &diff);
    assert_eq!(state.scroll(), 3);
}

#[test]
fn quit_ends_the_interface() {
    let (_dir, diff) = diff();
    assert!(!State::default().apply(Input::Quit, &diff));
}
