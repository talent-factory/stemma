//! The stream: the lines of all files of a [`Diff`] as one list of rows, laid out for a width.
//!
//! The view scrolls through this list. It is computed once for a set of settings, and not again
//! for each frame; [`Stream::fit`] computes it again only when the width, whether the layout is
//! split, or the wrapping changes.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use stemma_core::diff::Edit;
use stemma_core::moved::{moved_lines, MovedLines};
use stemma_core::repository::{Content, Diff, FileDiff, TextDiff};

use std::ops::Range;

use crate::highlight::{char_colors, Category, FileHighlights, Theme};

/// The text of the marker that `diff` writes after a last line without a line end.
const NO_LINE_END: &str = "\\ No newline at end of file";

/// The narrowest pane, in columns inside its border, in which the layout `auto` splits the rows.
/// Each half then has 59 or 60 columns, of which a few go to the number and the mark. With the
/// list of files at its widest (40 columns) and the borders, this is a terminal of 162 columns.
pub(crate) const SPLIT_MIN_WIDTH: u16 = 120;

/// How the rows of a file are laid out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayoutMode {
    /// Split from 120 columns of the pane inside its border, unified below.
    #[default]
    Auto,
    /// One column of lines: removals above the additions that replace them.
    Unified,
    /// Two columns: the old text left and the new text right, side by side.
    Split,
}

impl LayoutMode {
    /// Tells whether a pane of `width` columns shows the split layout in this mode.
    pub fn is_split(self, width: u16) -> bool {
        match self {
            Self::Auto => width >= SPLIT_MIN_WIDTH,
            Self::Unified => false,
            Self::Split => true,
        }
    }
}

/// The rows of the diff pane for all files of a diff, and where each file and hunk starts.
///
/// The positions are row indices into [`Stream::lines`]. A file or a hunk maps to a row through
/// [`Stream::file_start`] and [`Stream::hunk_start`], and a row to its file through
/// [`Stream::file_at`].
#[derive(Debug, Clone)]
pub struct Stream {
    moved: MovedLines,
    /// The width, the split, and the wrapping that the rows are laid out for; `None` before the
    /// first fit.
    fitted: Option<(u16, bool, bool)>,
    /// The number of files of the diff that the stream was made for.
    files: usize,
    rows: Vec<Line<'static>>,
    file_starts: Vec<usize>,
    hunk_starts: Vec<Vec<usize>>,
    /// Where the last frame drew the names of the files: the area inside the border of the list and
    /// the index of its first name. `None` before the first frame.
    file_list: Option<(Rect, usize)>,
    /// The colours of the rows, and whether they carry syntax highlighting.
    theme: Theme,
    /// The highlighted ranges of each file, found on the first fit with a theme that highlights.
    /// `None` for a file whose language is not known.
    highlights: Option<Vec<Option<FileHighlights>>>,
}

impl Stream {
    /// Prepares the stream of `diff`: finds its moved lines, once. The rows are built by
    /// [`Stream::fit`].
    pub fn new(diff: &Diff) -> Self {
        Self {
            moved: moved_lines(diff),
            fitted: None,
            files: diff.files().len(),
            rows: Vec::new(),
            file_starts: Vec::new(),
            hunk_starts: Vec::new(),
            file_list: None,
            theme: Theme::default(),
            highlights: None,
        }
    }

    /// Sets the theme of the rows. The rows are laid out again by the next [`Stream::fit`] if the
    /// theme is another one.
    pub fn set_theme(&mut self, theme: Theme) {
        if self.theme != theme {
            self.theme = theme;
            self.fitted = None;
        }
    }

    /// Records where the last frame drew the names of the files: `inner` is the area inside the
    /// border of the list, and `offset` the index of the file in its first row. [`Stream::file_at_cell`]
    /// maps a click through it.
    pub(crate) fn set_file_list(&mut self, inner: Rect, offset: usize) {
        self.file_list = Some((inner, offset));
    }

    /// The file whose name the last frame drew in the cell (`column`, `row`), or `None` for a cell
    /// outside the names, and for every cell before the first frame.
    pub(crate) fn file_at_cell(&self, column: u16, row: u16) -> Option<usize> {
        let (inner, offset) = self.file_list?;
        let inside = column >= inner.x
            && column - inner.x < inner.width
            && row >= inner.y
            && row - inner.y < inner.height;
        let file = offset + usize::from(row.checked_sub(inner.y)?);
        (inside && file < self.files).then_some(file)
    }

    /// Lays the rows out for a pane that is `width` columns wide inside its border, in `layout`,
    /// unless the stream is laid out for these very settings already. With `wrap`, a line that is
    /// longer than its column goes on in the rows below; without it, the end of the line is cut.
    /// `diff` must be the diff that the stream was made for; in a debug build a diff with another
    /// number of files is caught.
    pub fn fit(&mut self, diff: &Diff, width: u16, layout: LayoutMode, wrap: bool) {
        debug_assert_eq!(
            self.files,
            diff.files().len(),
            "the stream was made for another diff"
        );
        let split = layout.is_split(width);
        if self.fitted == Some((width, split, wrap)) {
            return;
        }
        self.fitted = Some((width, split, wrap));
        self.rows.clear();
        self.file_starts.clear();
        self.hunk_starts.clear();
        let grid = Grid {
            width: usize::from(width),
            number_width: number_width(diff),
            split,
            wrap,
        };
        // Taken out for the loop, which borrows the stream mutably, and put back after it.
        let highlights = self.take_highlights(diff);
        for (index, file) in diff.files().iter().enumerate() {
            if index > 0 {
                self.rows.push(Line::raw(""));
            }
            self.file_starts.push(self.rows.len());
            self.rows.push(header(file, grid.width, self.theme));
            let mut starts = Vec::new();
            match file.content() {
                Content::Text(text) => {
                    let marks = highlights.as_ref().and_then(|all| all[index].as_ref());
                    self.text_rows(index, text, marks, &grid, &mut starts);
                }
                Content::Binary => self.rows.push(Line::raw("Binary file differs")),
                Content::TooLarge => self.rows.push(Line::raw("File too large to compare")),
                // A kind of content that this version of the interface does not know how to show.
                _ => self
                    .rows
                    .push(Line::raw("The content of this file cannot be shown")),
            }
            self.hunk_starts.push(starts);
        }
        self.highlights = highlights;
    }

    /// The highlighted ranges of all files, found now if they are not yet, or `None` for a theme
    /// without highlighting. The ranges do not depend on the theme, so they are kept when the
    /// theme changes.
    fn take_highlights(&mut self, diff: &Diff) -> Option<Vec<Option<FileHighlights>>> {
        if !self.theme.highlights() {
            return self.highlights.take();
        }
        Some(self.highlights.take().unwrap_or_else(|| {
            diff.files()
                .iter()
                .map(|file| match file.content() {
                    Content::Text(text) => {
                        FileHighlights::of(file.path(), text.old_lines(), text.new_lines())
                    }
                    _ => None,
                })
                .collect()
        }))
    }

    fn text_rows(
        &mut self,
        file: usize,
        text: &TextDiff,
        marks: Option<&FileHighlights>,
        grid: &Grid,
        starts: &mut Vec<usize>,
    ) {
        let theme = self.theme;
        // A theme without highlighting keeps the ranges for later but does not use them.
        let marks = marks.filter(|_| theme.highlights());
        let (old_lines, new_lines) = (text.old_lines(), text.new_lines());
        for hunk in text.hunks() {
            starts.push(self.rows.len());
            self.rows.push(Line::styled(
                format!(
                    "@@ -{} +{} @@",
                    range(hunk.old_start, hunk.old_len),
                    range(hunk.new_start, hunk.new_len)
                ),
                Style::new().fg(Color::Cyan),
            ));
            let pieces: Vec<Piece> = hunk
                .edits
                .iter()
                .map(|edit| match *edit {
                    Edit::Equal { old, new } => Piece {
                        old: Some(old + 1),
                        new: Some(new + 1),
                        mark: ' ',
                        line: old_lines.get(old),
                        style: Style::new(),
                        ranges: marks.and_then(|m| m.new.get(new)),
                        theme,
                    },
                    Edit::Delete { old } => {
                        let moved = self.moved.is_moved_from(file, old);
                        Piece {
                            old: Some(old + 1),
                            new: None,
                            mark: '-',
                            line: old_lines.get(old),
                            style: if moved {
                                Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)
                            } else {
                                Style::new().fg(theme.text()).bg(theme.removed_background())
                            },
                            // A moved line keeps the colour that says so.
                            ranges: marks.filter(|_| !moved).and_then(|m| m.old.get(old)),
                            theme,
                        }
                    }
                    Edit::Insert { new } => {
                        let moved = self.moved.is_moved_to(file, new);
                        Piece {
                            old: None,
                            new: Some(new + 1),
                            mark: '+',
                            line: new_lines.get(new),
                            style: if moved {
                                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                            } else {
                                Style::new().fg(theme.text()).bg(theme.added_background())
                            },
                            ranges: marks.filter(|_| !moved).and_then(|m| m.new.get(new)),
                            theme,
                        }
                    }
                })
                .collect();
            if grid.split {
                split_rows(&pieces, grid, &mut self.rows);
            } else {
                for piece in &pieces {
                    unified_rows(piece, grid, &mut self.rows);
                }
            }
        }
    }

    /// All rows of the pane, the empty rows between the files included.
    pub fn lines(&self) -> &[Line<'static>] {
        &self.rows
    }

    /// The number of rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Tells whether there is no row, which is the case for a diff without files.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The number of files.
    pub fn file_count(&self) -> usize {
        self.file_starts.len()
    }

    /// The row of the header of a file, or `None` if there is no such file.
    pub fn file_start(&self, file: usize) -> Option<usize> {
        self.file_starts.get(file).copied()
    }

    /// The number of hunks of a file. A binary file and a file without changes in its text have
    /// none.
    pub fn hunk_count(&self, file: usize) -> usize {
        self.hunk_starts.get(file).map_or(0, Vec::len)
    }

    /// The row of the `@@` line of a hunk, or `None` if there is no such hunk.
    pub fn hunk_start(&self, file: usize, hunk: usize) -> Option<usize> {
        self.hunk_starts.get(file)?.get(hunk).copied()
    }

    /// The file that holds `row`; the empty row between two files belongs to the file above it.
    /// `None` if there is no file.
    pub fn file_at(&self, row: usize) -> Option<usize> {
        self.file_starts
            .partition_point(|&start| start <= row)
            .checked_sub(1)
    }
}

/// One line of a hunk, with the numbers it has on both sides, counted from one.
struct Piece<'a> {
    old: Option<usize>,
    new: Option<usize>,
    mark: char,
    line: Option<&'a String>,
    style: Style,
    /// The highlighted ranges of the line, in bytes of the line; `None` for a line that is not
    /// highlighted.
    ranges: Option<&'a Vec<(Range<usize>, Category)>>,
    theme: Theme,
}

impl Piece<'_> {
    /// The line without its line end, the colour of each of its characters, and whether it had a
    /// line end. The core builds the hunks from the very lines it holds, so a line is always
    /// there; a missing one is shown, not a reason to panic in the middle of a frame.
    fn content(&self) -> (Text, bool) {
        let line = self.line.map_or("(line missing)", String::as_str);
        let (content, line_end) = match line.strip_suffix('\n') {
            // The `\r` of a `\r\n` line end is part of the end, not of the text.
            Some(content) => (content.strip_suffix('\r').unwrap_or(content), true),
            None => (line, false),
        };
        let colors = match self.ranges {
            Some(ranges) if self.line.is_some() => char_colors(content, ranges, self.theme),
            _ => Vec::new(),
        };
        (printable(content, &colors), line_end)
    }

    /// The style of the text: that of the line, or a style of its own for the notice that stands
    /// in place of a missing line, so that it is not taken for an addition or a removal.
    fn text_style(&self) -> Style {
        if self.line.is_some() {
            self.style
        } else {
            Style::new()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC)
        }
    }

    /// The style of the numbers and the mark: grey for context, the style of the line otherwise.
    fn gutter_style(&self) -> Style {
        if self.style == Style::new() {
            Style::new().fg(Color::DarkGray)
        } else {
            self.style
        }
    }
}

/// The measures that the rows of a file are laid out with.
struct Grid {
    /// The columns of the pane inside its border.
    width: usize,
    /// The columns of one line number.
    number_width: usize,
    /// Whether the old and the new text stand side by side.
    split: bool,
    /// Whether a long line goes on in the rows below.
    wrap: bool,
}

/// A number right-aligned in `width` columns, or blanks.
fn number(number: Option<usize>, width: usize) -> String {
    match number {
        Some(number) => format!("{number:>width$}"),
        None => " ".repeat(width),
    }
}

/// Appends the rows of one line of a hunk in the unified layout, and after a last line without a
/// line end the marker that `diff` writes there.
fn unified_rows(piece: &Piece, grid: &Grid, rows: &mut Vec<Line<'static>>) {
    let (text, line_end) = piece.content();
    let gutter = format!(
        "{} {} {}",
        number(piece.old, grid.number_width),
        number(piece.new, grid.number_width),
        piece.mark
    );
    let room = grid.width.saturating_sub(gutter.chars().count());
    let blank = " ".repeat(gutter.chars().count());
    for (k, chunk) in fit_cells(&text.text, room, grid.wrap)
        .into_iter()
        .enumerate()
    {
        let gutter = if k == 0 {
            gutter.clone()
        } else {
            blank.clone()
        };
        let mut spans = vec![Span::styled(gutter, piece.gutter_style())];
        spans.extend(colored_spans(&chunk, &text.colors, piece.text_style()));
        rows.push(Line::from(spans));
    }
    if !line_end {
        for chunk in fit_rows(NO_LINE_END, room, grid.wrap) {
            rows.push(Line::from(vec![
                Span::raw(blank.clone()),
                Span::styled(chunk, Style::new().fg(Color::DarkGray)),
            ]));
        }
    }
}

/// The rows of one side of a split row, each `width` columns wide: the number, the mark, and the
/// text. A line that wraps has more than one row. A line without a line end is followed by the
/// rows of the marker, which wrap or are cut like the text.
fn side_rows(piece: &Piece, old_side: bool, width: usize, grid: &Grid) -> Vec<Vec<Span<'static>>> {
    let (text, line_end) = piece.content();
    let gutter = format!(
        "{} {}",
        number(
            if old_side { piece.old } else { piece.new },
            grid.number_width
        ),
        piece.mark
    );
    let room = width.saturating_sub(gutter.chars().count());
    let blank = " ".repeat(gutter.chars().count());
    let mut rows: Vec<Vec<Span<'static>>> = fit_cells(&text.text, room, grid.wrap)
        .into_iter()
        .enumerate()
        .map(|(k, chunk)| {
            let gutter = if k == 0 {
                gutter.clone()
            } else {
                blank.clone()
            };
            let mut spans = vec![Span::styled(gutter, piece.gutter_style())];
            spans.extend(colored_spans(&chunk, &text.colors, piece.text_style()));
            spans
        })
        .collect();
    if !line_end {
        rows.extend(
            fit_rows(NO_LINE_END, room, grid.wrap)
                .into_iter()
                .map(|chunk| {
                    vec![
                        Span::raw(blank.clone()),
                        Span::styled(chunk, Style::new().fg(Color::DarkGray)),
                    ]
                }),
        );
    }
    rows
}

/// Appends the rows of a hunk in the split layout. A context line stands on both sides. A run of
/// removals and additions is paired line by line: the first removal with the first addition, and
/// so on, and the side with fewer lines stays empty below its last line.
fn split_rows(pieces: &[Piece], grid: &Grid, rows: &mut Vec<Line<'static>>) {
    // The separator takes one column; the right half gets the column that is left over.
    let left_width = grid.width.saturating_sub(1) / 2;
    let right_width = grid.width.saturating_sub(1 + left_width);
    let mut push_pair = |left: Option<&Piece>, right: Option<&Piece>| {
        let side = |piece: Option<&Piece>, old_side: bool, width: usize| {
            piece.map_or_else(Vec::new, |p| side_rows(p, old_side, width, grid))
        };
        let left_rows = side(left, true, left_width);
        let right_rows = side(right, false, right_width);
        for k in 0..left_rows.len().max(right_rows.len()) {
            let mut spans = left_rows
                .get(k)
                .cloned()
                .unwrap_or_else(|| vec![Span::raw(" ".repeat(left_width))]);
            spans.push(Span::styled("│", Style::new().fg(Color::DarkGray)));
            spans.extend(
                right_rows
                    .get(k)
                    .cloned()
                    .unwrap_or_else(|| vec![Span::raw(" ".repeat(right_width))]),
            );
            rows.push(Line::from(spans));
        }
    };
    let mut i = 0;
    while i < pieces.len() {
        if pieces[i].mark == ' ' {
            push_pair(Some(&pieces[i]), Some(&pieces[i]));
            i += 1;
            continue;
        }
        let end = pieces[i..]
            .iter()
            .position(|piece| piece.mark == ' ')
            .map_or(pieces.len(), |at| i + at);
        let removed: Vec<&Piece> = pieces[i..end].iter().filter(|p| p.mark == '-').collect();
        let added: Vec<&Piece> = pieces[i..end].iter().filter(|p| p.mark == '+').collect();
        for k in 0..removed.len().max(added.len()) {
            push_pair(removed.get(k).copied(), added.get(k).copied());
        }
        i = end;
    }
}

/// The rows that `text` takes in a column of `width` columns, each followed by as many spaces as
/// it takes to fill the column, so that the background of the row reaches the next one. With
/// `wrap`, the text goes on in the next row whenever a character would cross the edge. Without it,
/// the rest of the text is cut, and the last column of the row shows `…` to say so. A character
/// that is wider than the whole column is shown as `…`. No row is longer than `width`, and an
/// empty text still has one row.
fn fit_rows(text: &str, width: usize, wrap: bool) -> Vec<String> {
    fit_cells(text, width, wrap)
        .into_iter()
        .map(|row| row.into_iter().map(|(c, _)| c).collect())
        .collect()
}

/// A character of a row, and the index of the character of the text that it shows; `None` for the
/// spaces that fill the row and for the `…` of a cut.
type Cell = (char, Option<usize>);

/// Like [`fit_rows`], but every character of a row says which character of `text` it shows, so
/// that the colour of that character can follow it into the row.
fn fit_cells(text: &str, width: usize, wrap: bool) -> Vec<Vec<Cell>> {
    let mut rows = Vec::new();
    let mut row: Vec<Cell> = Vec::new();
    let mut used = 0;
    for (index, c) in text.chars().enumerate() {
        let (c, w, source) = match char_width(c) {
            w if w > width => ('…', 1, None),
            w => (c, w, Some(index)),
        };
        if w > width {
            // A column of no width holds nothing, not even the sign of a cut.
            break;
        }
        if used + w > width {
            if !wrap {
                cut(&mut row, &mut used, width);
                break;
            }
            row.extend(std::iter::repeat_n((' ', None), width - used));
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push((c, source));
        used += w;
    }
    row.extend(std::iter::repeat_n((' ', None), width - used));
    rows.push(row);
    rows
}

/// The spans of one row of cells: neighbours of the same style are one span. A character with a
/// colour in `colors` (indexed like the text of the row) takes it as its foreground.
fn colored_spans(row: &[Cell], colors: &[Option<Color>], base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style = base;
    for &(c, source) in row {
        let style = match source.and_then(|at| colors.get(at).copied().flatten()) {
            Some(color) => base.fg(color),
            None => base,
        };
        if style != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    spans
}

/// Ends `row` with `…` in the last of its `width` columns, replacing what stood there.
fn cut(row: &mut Vec<Cell>, used: &mut usize, width: usize) {
    while *used + 1 > width {
        match row.pop() {
            Some((c, _)) => *used -= char_width(c),
            None => return,
        }
    }
    row.push(('…', None));
    *used += 1;
}

/// The number of columns that a character takes.
fn char_width(c: char) -> usize {
    if c.is_ascii() {
        1
    } else {
        Span::raw(c.to_string()).width()
    }
}

/// The text as the terminal can take it. Ratatui drops every control character, so a tab would
/// vanish together with the indentation, and a control character that it passed on would act on
/// the terminal. A tab is shown as four spaces, not aligned to tab stops. Any other control
/// character is shown in caret notation (`^[` for escape, `^M` for a `\r`, `^?` for delete), and
/// one of the C1 range as `^@`.
fn printable(content: &str, colors: &[Option<Color>]) -> Text {
    let mut out = Text::default();
    for (at, c) in content.chars().enumerate() {
        let color = colors.get(at).copied().flatten();
        let mut push = |text: &str| {
            for c in text.chars() {
                out.text.push(c);
                out.colors.push(color);
            }
        };
        match c {
            '\t' => push("    "),
            '\u{7f}' => push("^?"),
            c if c.is_control() => match u8::try_from(u32::from(c)) {
                Ok(byte) if byte < 0x20 => push(&format!("^{}", char::from(byte + 0x40))),
                _ => push("^@"),
            },
            c => push(c.encode_utf8(&mut [0; 4])),
        }
    }
    out
}

/// The text of a line as the terminal can take it, and the colour of each of its characters, or
/// `None` for the colour of the line.
#[derive(Debug, Default)]
struct Text {
    text: String,
    colors: Vec<Option<Color>>,
}

/// The row with the path of a file, its counts, and what happened to it, on one background.
fn header(file: &FileDiff, width: usize, theme: Theme) -> Line<'static> {
    let style = Style::new()
        .fg(theme.text())
        .bg(theme.header_background())
        .add_modifier(Modifier::BOLD);
    let state = match (file.old_exists(), file.new_exists()) {
        (false, _) => " (new file)",
        (_, false) => " (deleted)",
        _ => "",
    };
    let left = format!(" {}{state}", file.path());
    let (added, removed) = match file.content() {
        Content::Text(text) => text.hunks().iter().flat_map(|hunk| &hunk.edits).fold(
            (0, 0),
            |(added, removed), edit| match edit {
                Edit::Insert { .. } => (added + 1, removed),
                Edit::Delete { .. } => (added, removed + 1),
                _ => (added, removed),
            },
        ),
        _ => (0, 0),
    };
    let counts = match file.content() {
        Content::Text(_) => (format!("+{added}"), format!(" -{removed} ")),
        _ => (String::new(), String::new()),
    };
    // The path is cut, with `…`, before the counts are: they say the most and are short.
    let room = width.saturating_sub(counts.0.chars().count() + counts.1.chars().count());
    let left = fit_rows(&left, room, false).concat();
    Line::from(vec![
        Span::styled(left, style),
        Span::styled(counts.0, style.fg(Color::Green)),
        Span::styled(counts.1, style.fg(Color::Red)),
    ])
}

/// The width of a column of line numbers: the digits of the longest file.
fn number_width(diff: &Diff) -> usize {
    let longest = diff
        .files()
        .iter()
        .filter_map(|file| match file.content() {
            Content::Text(text) => Some(text.old_lines().len().max(text.new_lines().len())),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    longest.max(1).to_string().len()
}

/// A range of lines as the `@@` line writes it, as `diff` does. `start` is the zero-based index of
/// the first line. A range that has lines writes `start + 1` and, unless it is one line, its
/// length; an empty range writes `start`, the line before it, and the length 0.
fn range(start: usize, len: usize) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{len}", start + 1),
    }
}
