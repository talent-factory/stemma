//! The terminal interface of `stemma diff --tui` (ADR-016): a read-only view that browses the files
//! and hunks of a [`Diff`].
//!
//! The state of the view is a plain value, [`State`], that changes by [`Input`]; [`render`] draws
//! it into any ratatui frame, so tests use the `TestBackend` and need no terminal. [`run`] and
//! [`restore`] are the only public functions that touch the real terminal.

use std::io;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListState, Paragraph};
use ratatui::Frame;
use stemma_core::diff::Edit;
use stemma_core::repository::{Content, Diff, FileDiff};

/// What a key asks the view to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// Leave the interface.
    Quit,
    /// Select the next file.
    NextFile,
    /// Select the previous file.
    PreviousFile,
    /// Select the next hunk of the file, or the first hunk of the next file after the last one.
    NextHunk,
    /// Select the previous hunk of the file, or the last hunk of the previous file before the
    /// first one.
    PreviousHunk,
    /// Scroll the file down by this many lines, but not past its last line.
    ScrollDown(u16),
    /// Scroll the file up by this many lines, but not above its first line.
    ScrollUp(u16),
}

impl Input {
    /// The input that a key stands for, or `None` for a key that the view does not use.
    pub fn from_key(key: KeyEvent) -> Option<Self> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        Some(match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Self::Quit,
            KeyCode::Char('c') if control => Self::Quit,
            KeyCode::Char('J') | KeyCode::Tab => Self::NextFile,
            KeyCode::Char('K') | KeyCode::BackTab => Self::PreviousFile,
            KeyCode::Char('n') => Self::NextHunk,
            KeyCode::Char('p') => Self::PreviousHunk,
            KeyCode::Char('j') | KeyCode::Down => Self::ScrollDown(1),
            KeyCode::Char('k') | KeyCode::Up => Self::ScrollUp(1),
            KeyCode::PageDown | KeyCode::Char(' ') => Self::ScrollDown(PAGE),
            KeyCode::PageUp | KeyCode::Char('b') => Self::ScrollUp(PAGE),
            _ => return None,
        })
    }
}

/// The number of lines that a page key scrolls.
const PAGE: u16 = 20;

/// Which file and hunk are selected, and how far the file is scrolled.
///
/// The indices refer to the [`Diff`] that is passed to [`State::apply`] and to [`render`]; use the
/// same diff throughout. Another diff cannot make a call fail: an index that is out of range for
/// it shows no file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct State {
    file: usize,
    hunk: usize,
    scroll: usize,
}

impl State {
    /// The index of the selected file.
    pub fn file(&self) -> usize {
        self.file
    }

    /// The index of the selected hunk in the selected file.
    pub fn hunk(&self) -> usize {
        self.hunk
    }

    /// The number of lines of the selected file that are scrolled off the top.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Applies `input`. Returns `false` once the user asked to quit, and `true` otherwise.
    pub fn apply(&mut self, input: Input, diff: &Diff) -> bool {
        let files = diff.files().len();
        match input {
            Input::Quit => return false,
            Input::NextFile => {
                if self.file + 1 < files {
                    self.select_file(self.file + 1);
                }
            }
            Input::PreviousFile => {
                if self.file > 0 {
                    self.select_file(self.file - 1);
                }
            }
            Input::NextHunk => {
                if self.hunk + 1 < hunk_count(diff.files().get(self.file)) {
                    self.select_hunk(self.hunk + 1, diff);
                } else if self.file + 1 < files {
                    self.select_file(self.file + 1);
                }
            }
            Input::PreviousHunk => {
                if self.hunk > 0 {
                    self.select_hunk(self.hunk - 1, diff);
                } else if self.file > 0 {
                    self.select_file(self.file - 1);
                    let last = hunk_count(diff.files().get(self.file)).saturating_sub(1);
                    self.select_hunk(last, diff);
                }
            }
            Input::ScrollDown(lines) => {
                let last = diff
                    .files()
                    .get(self.file)
                    .map_or(0, |file| file_lines(file).lines.len().saturating_sub(1));
                self.scroll = self.scroll.saturating_add(usize::from(lines)).min(last);
            }
            Input::ScrollUp(lines) => self.scroll = self.scroll.saturating_sub(usize::from(lines)),
        }
        true
    }

    fn select_file(&mut self, file: usize) {
        *self = Self {
            file,
            hunk: 0,
            scroll: 0,
        };
    }

    /// Selects a hunk and scrolls the file to the line of its `@@` header.
    fn select_hunk(&mut self, hunk: usize, diff: &Diff) {
        self.hunk = hunk;
        self.scroll = diff
            .files()
            .get(self.file)
            .and_then(|file| file_lines(file).hunk_starts.get(hunk).copied())
            .unwrap_or(0);
    }
}

fn hunk_count(file: Option<&FileDiff>) -> usize {
    match file.map(FileDiff::content) {
        Some(Content::Text(text)) => text.hunks().len(),
        _ => 0,
    }
}

/// The lines of one file as the right pane shows them, and where each hunk starts in them.
struct FileLines {
    lines: Vec<Line<'static>>,
    hunk_starts: Vec<usize>,
}

fn file_lines(file: &FileDiff) -> FileLines {
    let mut lines = Vec::new();
    let mut hunk_starts = Vec::new();
    let text = match file.content() {
        Content::Text(text) => text,
        Content::Binary => {
            lines.push(Line::raw("Binary file differs"));
            return FileLines { lines, hunk_starts };
        }
        // A kind of content that this version of the interface does not know how to show.
        _ => {
            lines.push(Line::raw("The content of this file cannot be shown"));
            return FileLines { lines, hunk_starts };
        }
    };
    let (old_lines, new_lines) = (text.old_lines(), text.new_lines());
    for hunk in text.hunks() {
        hunk_starts.push(lines.len());
        lines.push(Line::styled(
            format!(
                "@@ -{} +{} @@",
                range(hunk.old_start, hunk.old_len),
                range(hunk.new_start, hunk.new_len)
            ),
            Style::new().fg(Color::Cyan),
        ));
        for edit in &hunk.edits {
            lines.push(match *edit {
                Edit::Equal { old, .. } => edit_line(' ', old_lines.get(old), Style::new()),
                Edit::Delete { old } => {
                    edit_line('-', old_lines.get(old), Style::new().fg(Color::Red))
                }
                Edit::Insert { new } => {
                    edit_line('+', new_lines.get(new), Style::new().fg(Color::Green))
                }
            });
        }
    }
    FileLines { lines, hunk_starts }
}

/// A range of lines as the `@@` line writes it, as `diff` does: the first line counted from one,
/// and the length unless it is one. An empty range names the line before it, so `start` is
/// counted from zero.
fn range(start: usize, len: usize) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{len}", start + 1),
    }
}

/// One line of a hunk with its mark. The core builds the hunks from the very lines it holds, so a
/// line is always there; a missing one is shown, not a reason to panic in the middle of a frame.
fn edit_line(mark: char, line: Option<&String>, style: Style) -> Line<'static> {
    let line = line.map_or("(line missing)", String::as_str);
    let content = line.strip_suffix('\n').unwrap_or(line);
    // Ratatui drops control characters, so a tab would vanish together with the indentation.
    Line::styled(format!("{mark}{}", content.replace('\t', "    ")), style)
}

/// Draws the view: the files on the left with a marker for the selected one, and the lines of the
/// selected file on the right, scrolled by the state.
pub fn render(frame: &mut Frame, state: &State, diff: &Diff) {
    let [main, help] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [files_area, text_area] =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).areas(main);

    let paths: Vec<&str> = diff.files().iter().map(FileDiff::path).collect();
    let mut list_state = ListState::default().with_selected(Some(state.file));
    let list = List::new(paths)
        .block(Block::new().borders(Borders::ALL).title("Files"))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, files_area, &mut list_state);

    let (title, mut body) = match diff.files().get(state.file) {
        Some(file) => (file.path().to_owned(), file_lines(file).lines),
        None => ("No differences".to_owned(), Vec::new()),
    };
    // The lines above the pane are dropped here, not scrolled by the widget, whose offset is a
    // `u16`: a file of more than 65535 lines can be read to its end.
    body.drain(..state.scroll.min(body.len()));
    let text = Paragraph::new(body).block(Block::new().borders(Borders::ALL).title(title));
    frame.render_widget(text, text_area);

    frame.render_widget(
        Paragraph::new("q quit · J/K file · n/p hunk · j/k scroll · space/b page"),
        help,
    );
}

/// Runs the interface on the terminal until the user quits. The terminal is restored on return,
/// and on a panic, through the hook that ratatui installs. A signal that ends the process does
/// not return here; the caller restores the terminal with [`restore`] first.
///
/// # Errors
///
/// Returns the error of the terminal if it cannot be set up, drawn on, read from, or restored.
/// The first error wins.
pub fn run(diff: &Diff) -> io::Result<()> {
    // Setting up may have turned on raw mode before it failed.
    let mut terminal = ratatui::try_init().inspect_err(|_| restore())?;
    let result = event_loop(&mut terminal, diff);
    let restored = ratatui::try_restore();
    result.and(restored)
}

/// Gives the terminal back: leaves the alternate screen and the raw mode. It is for the code that
/// ends the process while [`run`] is active, as a handler of a signal does; it does nothing
/// harmful if the interface is not active.
pub fn restore() {
    ratatui::restore();
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, diff: &Diff) -> io::Result<()> {
    let mut state = State::default();
    loop {
        terminal.draw(|frame| render(frame, &state, diff))?;
        // A resize event only redraws, which the next turn of the loop does.
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(input) = Input::from_key(key) {
                if !state.apply(input, diff) {
                    return Ok(());
                }
            }
        }
    }
}
