//! The terminal interface of `stemma diff --tui` (ADR-016): a read-only view that browses the files
//! and hunks of a [`Diff`].
//!
//! The state of the view is a plain value, [`State`], that changes by [`Input`]; [`render`] draws
//! it into any ratatui frame, so tests use the `TestBackend` and need no terminal. [`run`] is the
//! only public function that touches the real terminal.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListState, Paragraph};
use ratatui::Frame;
use stemma_core::repository::{Diff, FileDiff};

mod stream;

pub use stream::{LayoutMode, Stream};

/// What the view is asked to do: what a key or the mouse stands for, or a change of the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
    /// Scroll the stream down by this many rows, but not past its last row, which can then be the
    /// only row at the top of the pane.
    ScrollDown(u16),
    /// Scroll the stream up by this many rows, but not above its first row.
    ScrollUp(u16),
    /// The terminal has this many columns now.
    Resize(u16),
    /// Lay the rows out in this mode.
    SetLayout(LayoutMode),
    /// Switch the wrapping of long lines on or off.
    ToggleWrap,
    /// The left mouse button went down in this cell of the terminal. A cell on the name of a file
    /// in the list selects that file, as `J` and `K` do; any other cell changes nothing.
    Click {
        /// The column of the cell, counted from the left edge of the terminal.
        column: u16,
        /// The row of the cell, counted from the top edge of the terminal.
        row: u16,
    },
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
            KeyCode::Char('1') => Self::SetLayout(LayoutMode::Unified),
            KeyCode::Char('2') => Self::SetLayout(LayoutMode::Split),
            KeyCode::Char('0') => Self::SetLayout(LayoutMode::Auto),
            KeyCode::Char('w') => Self::ToggleWrap,
            _ => return None,
        })
    }

    /// The input that a mouse event stands for, or `None` for an event that the view does not use:
    /// the wheel scrolls the stream by three rows, and the left button is a click.
    pub fn from_mouse(event: MouseEvent) -> Option<Self> {
        match event.kind {
            MouseEventKind::ScrollDown => Some(Self::ScrollDown(WHEEL)),
            MouseEventKind::ScrollUp => Some(Self::ScrollUp(WHEEL)),
            MouseEventKind::Down(MouseButton::Left) => Some(Self::Click {
                column: event.column,
                row: event.row,
            }),
            _ => None,
        }
    }
}

/// The number of rows that a page key scrolls: a fixed number, not the height of the view.
const PAGE: u16 = 20;

/// The number of rows that one notch of the mouse wheel scrolls.
const WHEEL: u16 = 3;

/// The width of the terminal that a state assumes until it is told the real one.
const DEFAULT_WIDTH: u16 = 80;

/// The most columns that the list of files takes; it takes a quarter of the terminal below that.
const MAX_LIST_WIDTH: u16 = 40;

/// How long the interface waits for a key before it looks at the request to stop.
const STOP_POLL: Duration = Duration::from_millis(100);

/// Which file is in view, which hunk is selected, and how far the stream is scrolled.
///
/// The file is the one that holds the top row of the pane. The indices refer to the [`Diff`] that
/// is passed to [`State::apply`] and to [`render_with`]; use the same diff throughout. Another diff
/// cannot make a call fail: an index that is out of range for it shows an empty pane.
///
/// The state also holds the width of the terminal, and [`render_with`] lays the stream out for
/// that width, so that the scroll position always refers to the rows that are drawn. A change of
/// the width, the layout, or the wrapping puts the row of the selection (the header of the file
/// for the first hunk, the `@@` row of the hunk after that) at the top again; a position inside a
/// file between two such rows is not kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    file: usize,
    hunk: usize,
    scroll: usize,
    width: u16,
    layout: LayoutMode,
    wrap: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            file: 0,
            hunk: 0,
            scroll: 0,
            width: DEFAULT_WIDTH,
            layout: LayoutMode::Auto,
            wrap: false,
        }
    }
}

impl State {
    /// The index of the file in view: the file that holds the top row of the pane.
    pub fn file(&self) -> usize {
        self.file
    }

    /// The index of the selected hunk in the file in view. It changes through the hunk and file
    /// keys, and when scrolling brings another file to the top, not through scrolling inside a
    /// file, so that `n` and `p` go on from the last selected hunk and not from the hunk that
    /// scrolling has brought into view. A change of the width, the layout, or the wrapping puts
    /// the selection at the top again and keeps the file in view and the hunk.
    pub fn hunk(&self) -> usize {
        self.hunk
    }

    /// The number of rows of the stream that are scrolled off the top.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// The number of columns of the terminal that the state has been told (see [`Input::Resize`]).
    pub fn width(&self) -> u16 {
        self.width
    }

    /// The layout that the user asked for: `auto` until `1` or `2` is pressed, and again after `0`.
    pub fn layout(&self) -> LayoutMode {
        self.layout
    }

    /// Tells whether long lines wrap; they are cut at the edge of the pane until `w` is pressed.
    pub fn wrap(&self) -> bool {
        self.wrap
    }

    /// Applies `input`. Returns `false` once the user asked to quit, and `true` otherwise.
    ///
    /// `apply` lays the stream out for the width, the layout, and the wrapping of the state first,
    /// so a fresh [`Stream::new`] needs no call of its own. Keep one stream for the whole run, or
    /// the rows are laid out again with each call.
    pub fn apply(&mut self, input: Input, diff: &Diff, stream: &mut Stream) -> bool {
        if input == Input::Quit {
            return false;
        }
        match input {
            Input::Resize(width) => self.width = width,
            Input::SetLayout(layout) => self.layout = layout,
            Input::ToggleWrap => self.wrap = !self.wrap,
            _ => {}
        }
        stream.fit(diff, text_width(self.width), self.layout, self.wrap);
        let files = stream.file_count();
        match input {
            Input::Quit => {}
            // The rows are different now, so the selection decides what is at the top again.
            Input::Resize(_) | Input::SetLayout(_) | Input::ToggleWrap => {
                self.scroll = self.anchor(stream);
                self.follow(stream);
            }
            Input::NextFile => {
                if self.file + 1 < files {
                    self.select_file(self.file + 1, stream);
                }
            }
            Input::PreviousFile => {
                if self.file > 0 {
                    self.select_file(self.file - 1, stream);
                }
            }
            Input::NextHunk => {
                if self.hunk + 1 < stream.hunk_count(self.file) {
                    self.select_hunk(self.hunk + 1, stream);
                } else if self.file + 1 < files {
                    self.select_file(self.file + 1, stream);
                }
            }
            Input::PreviousHunk => {
                if self.hunk > 0 {
                    self.select_hunk(self.hunk - 1, stream);
                } else if self.file > 0 {
                    self.select_file(self.file - 1, stream);
                    let last = stream.hunk_count(self.file).saturating_sub(1);
                    self.select_hunk(last, stream);
                }
            }
            Input::ScrollDown(rows) => {
                let last = stream.len().saturating_sub(1);
                self.scroll = self.scroll.saturating_add(usize::from(rows)).min(last);
                self.follow(stream);
            }
            Input::ScrollUp(rows) => {
                self.scroll = self.scroll.saturating_sub(usize::from(rows));
                self.follow(stream);
            }
            Input::Click { column, row } => {
                if let Some(file) = stream.file_at_cell(column, row) {
                    self.select_file(file, stream);
                }
            }
        }
        true
    }

    /// The row that shows the selection: the header of the file for the first hunk, the `@@` row
    /// of the hunk after that. It is clamped to the last row.
    fn anchor(&self, stream: &Stream) -> usize {
        let row = if self.hunk == 0 {
            stream.file_start(self.file)
        } else {
            stream.hunk_start(self.file, self.hunk)
        };
        row.unwrap_or(0).min(stream.len().saturating_sub(1))
    }

    /// Makes the file that holds the top row the file in view. Its first hunk is then selected.
    fn follow(&mut self, stream: &Stream) {
        if let Some(file) = stream.file_at(self.scroll) {
            if file != self.file {
                self.file = file;
                self.hunk = 0;
            }
        }
    }

    fn select_file(&mut self, file: usize, stream: &Stream) {
        self.file = file;
        self.hunk = 0;
        self.scroll = self.anchor(stream);
    }

    /// Selects a hunk and scrolls the stream to the line of its `@@` header.
    fn select_hunk(&mut self, hunk: usize, stream: &Stream) {
        self.hunk = hunk;
        self.scroll = stream
            .hunk_start(self.file, hunk)
            .or_else(|| stream.file_start(self.file))
            .unwrap_or(0);
    }
}

/// The three areas of the terminal: the list of files, the pane of the stream, and the help line.
fn areas(terminal: Rect) -> [Rect; 3] {
    let [main, help] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(terminal);
    let list = (main.width / 4).min(MAX_LIST_WIDTH);
    let [files, text] =
        Layout::horizontal([Constraint::Length(list), Constraint::Min(0)]).areas(main);
    [files, text, help]
}

/// The columns of the pane of the stream inside its border, for a terminal of this width.
fn text_width(terminal_width: u16) -> u16 {
    areas(Rect::new(0, 0, terminal_width, 3))[1]
        .width
        .saturating_sub(2)
}

/// Draws the view: the files on the left, with a marker for the file in view, and the stream of the
/// lines of all files on the right, scrolled by the state. The stream is laid out for the width of
/// the frame, not for the width that the state has been told, so the scroll position of a state
/// that was told another width refers to other rows. Use [`render_with`] and [`Input::Resize`] in
/// a loop.
pub fn render(frame: &mut Frame, state: &State, diff: &Diff) {
    let state = State {
        width: frame.area().width,
        ..*state
    };
    render_with(frame, &state, diff, &mut Stream::new(diff));
}

/// Like [`render`], with the stream of `diff` given, so that a loop that draws the same diff again
/// and again finds the moved lines once and lays the rows out again only when the width, whether
/// the layout is split, or the wrapping changes. `stream` must come from [`Stream::new`] for this
/// very `diff`. The rows are laid out for the width that the state has been told (see
/// [`Input::Resize`]), which is the width of the frame in a loop that sends it on every change.
pub fn render_with(frame: &mut Frame, state: &State, diff: &Diff, stream: &mut Stream) {
    let [files_area, text_area, help] = areas(frame.area());
    let text_width = text_width(state.width);
    stream.fit(diff, text_width, state.layout, state.wrap);

    let paths: Vec<&str> = diff.files().iter().map(FileDiff::path).collect();
    let mut list_state = ListState::default().with_selected(Some(state.file));
    let list = List::new(paths)
        .block(Block::new().borders(Borders::ALL).title("Files"))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, files_area, &mut list_state);
    stream.set_file_list(Block::bordered().inner(files_area), list_state.offset());

    let title = if diff.files().is_empty() {
        "No differences".to_owned()
    } else {
        let split = if state.layout.is_split(text_width) {
            "split"
        } else {
            "unified"
        };
        let auto = if state.layout == LayoutMode::Auto {
            " (auto)"
        } else {
            ""
        };
        let wrap = if state.wrap { ", wrap" } else { "" };
        format!("Diff · {split}{auto}{wrap}")
    };
    // Only the rows in view are copied, and the rows above the pane are skipped here, not scrolled
    // by the widget, whose offset is a `u16`: a stream of more than 65535 rows can be read to its
    // end.
    let body: Vec<Line> = stream
        .lines()
        .iter()
        .skip(state.scroll)
        .take(usize::from(text_area.height))
        .cloned()
        .collect();
    let text = Paragraph::new(body).block(Block::new().borders(Borders::ALL).title(title));
    frame.render_widget(text, text_area);

    frame.render_widget(
        Paragraph::new(
            "q quit · J/K file · n/p hunk · j/k scroll · space/b page · 1/2/0 layout · w wrap · \
         wheel, click · Shift selects",
        ),
        help,
    );
}

/// Runs the interface on the terminal until the user quits or `stop` is set, and gives the terminal
/// back on return. `stop` is for a handler of a signal on another thread: while no key arrives, the
/// interface looks at it ten times a second, so the handler never has to touch the terminal. A
/// panic restores the terminal through the hook that ratatui installs.
///
/// # Errors
///
/// Returns the error of the terminal if it cannot be set up, drawn on, read from, or restored.
/// If the terminal cannot be restored after another error, the error says so as well.
pub fn run(diff: &Diff, stop: &AtomicBool) -> io::Result<()> {
    // Setting up may have turned on raw mode before it failed.
    let mut terminal = ratatui::try_init().inspect_err(|_| ratatui::restore())?;
    // The hook of ratatui gives back the raw mode and the alternate screen, not the mouse.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        hook(info);
    }));
    let result = execute!(io::stdout(), EnableMouseCapture)
        .and_then(|()| event_loop(&mut terminal, diff, stop));
    // Off again on every way out, also when turning it on failed half way.
    let mouse = execute!(io::stdout(), DisableMouseCapture);
    let restored = ratatui::try_restore().and(mouse);
    match (result, restored) {
        (Err(error), Err(restore_error)) => Err(io::Error::new(
            error.kind(),
            format!("{error}; the terminal could not be restored either: {restore_error}"),
        )),
        (result, restored) => result.and(restored),
    }
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    diff: &Diff,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut state = State::default();
    // The diff does not change while the view is open, so the moves are found once, and the rows
    // are laid out again only when the width, whether the layout is split, or the wrapping changes.
    let mut stream = Stream::new(diff);
    loop {
        let width = terminal.size()?.width;
        if width != state.width() {
            state.apply(Input::Resize(width), diff, &mut stream);
        }
        terminal.draw(|frame| render_with(frame, &state, diff, &mut stream))?;
        // The request to stop is looked at every 100 ms, but only while no key arrives.
        while !event::poll(STOP_POLL)? {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
        }
        // A resize event only redraws, which the next turn of the loop does.
        let input = match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => Input::from_key(key),
            Event::Mouse(mouse) => Input::from_mouse(mouse),
            _ => None,
        };
        if let Some(input) = input {
            if !state.apply(input, diff, &mut stream) {
                return Ok(());
            }
        }
    }
}
