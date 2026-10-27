//! The terminal interface of `stemma diff --tui` (ADR-016): a read-only view that browses the files
//! and hunks of a [`Diff`].
//!
//! The state of the view is a plain value, [`State`], that changes by [`Input`]; [`render`] draws
//! it into any ratatui frame, so tests use the `TestBackend` and need no terminal. [`run`] is the
//! only function that touches the real terminal.

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
    /// Scroll the file down by this many lines.
    ScrollDown(u16),
    /// Scroll the file up by this many lines.
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct State {
    file: usize,
    hunk: usize,
    scroll: u16,
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

    /// The number of lines that the file is scrolled by.
    pub fn scroll(&self) -> u16 {
        self.scroll
    }

    /// Applies `input`. Returns `false` when the interface is to end, and `true` otherwise.
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
            Input::ScrollDown(lines) => self.scroll = self.scroll.saturating_add(lines),
            Input::ScrollUp(lines) => self.scroll = self.scroll.saturating_sub(lines),
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
    hunk_starts: Vec<u16>,
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
        hunk_starts.push(u16::try_from(lines.len()).unwrap_or(u16::MAX));
        lines.push(Line::styled(
            format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start + 1,
                hunk.old_len,
                hunk.new_start + 1,
                hunk.new_len
            ),
            Style::new().fg(Color::Cyan),
        ));
        for edit in &hunk.edits {
            lines.push(match *edit {
                Edit::Equal { old, .. } => edit_line(' ', &old_lines[old], Style::new()),
                Edit::Delete { old } => {
                    edit_line('-', &old_lines[old], Style::new().fg(Color::Red))
                }
                Edit::Insert { new } => {
                    edit_line('+', &new_lines[new], Style::new().fg(Color::Green))
                }
            });
        }
    }
    FileLines { lines, hunk_starts }
}

fn edit_line(mark: char, line: &str, style: Style) -> Line<'static> {
    let content = line.strip_suffix('\n').unwrap_or(line);
    // A tab would move the cursor of the terminal behind the back of the buffer of ratatui.
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

    let (title, body) = match diff.files().get(state.file) {
        Some(file) => (file.path().to_owned(), file_lines(file).lines),
        None => ("No differences".to_owned(), Vec::new()),
    };
    let text = Paragraph::new(body)
        .block(Block::new().borders(Borders::ALL).title(title))
        .scroll((state.scroll, 0));
    frame.render_widget(text, text_area);

    frame.render_widget(
        Paragraph::new("q quit · J/K file · n/p hunk · j/k scroll · space/b page"),
        help,
    );
}

/// Runs the interface on the terminal until the user quits. The terminal is restored on return,
/// and on a panic.
///
/// # Errors
///
/// Returns the error of the terminal if it cannot be set up, drawn on, or read from.
pub fn run(diff: &Diff) -> io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(&mut terminal, diff);
    ratatui::restore();
    result
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
