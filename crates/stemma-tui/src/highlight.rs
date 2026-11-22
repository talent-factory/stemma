//! Syntax highlighting of the lines of a diff, and the themes that colour them (ADR-021).
//!
//! The text of a whole file is parsed once per side, with a tree-sitter grammar chosen by the
//! extension of the path, so that a construct that spans several lines (a block comment, a raw
//! string) is coloured on every line. The result is a list of byte ranges with a [`Category`] per
//! line; a [`Theme`] turns a category into a colour. A file whose extension is unknown, and a text
//! that cannot be parsed, get no ranges and are drawn as before.

use std::ops::Range;
use std::sync::OnceLock;

use ratatui::style::Color;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// What a highlighted piece of text is, as far as the colours distinguish it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Category {
    Comment,
    String,
    Number,
    Keyword,
    Function,
    Type,
    Property,
    Constant,
}

/// The capture names that the grammars use, in the order of [`CATEGORIES`]. A capture such as
/// `keyword.control` matches the name `keyword`; a capture that no name matches is not coloured.
const NAMES: [&str; 8] = [
    "comment", "string", "number", "keyword", "function", "type", "property", "constant",
];

/// The category of the capture name with the same index in [`NAMES`].
const CATEGORIES: [Category; 8] = [
    Category::Comment,
    Category::String,
    Category::Number,
    Category::Keyword,
    Category::Function,
    Category::Type,
    Category::Property,
    Category::Constant,
];

/// The colours of the view. `Dark` is the look that the view had before the themes: it suits a
/// terminal with a dark background. `Plain` keeps these colours and turns the highlighting off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Theme {
    /// Light text on dark backgrounds, with syntax highlighting.
    #[default]
    Dark,
    /// Dark text on light backgrounds, with syntax highlighting.
    Light,
    /// The colours of `Dark`, without syntax highlighting.
    Plain,
}

impl Theme {
    /// The theme that follows this one when the user asks for the next: `Dark`, `Light`, `Plain`,
    /// and `Dark` again.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Plain,
            Self::Plain => Self::Dark,
        }
    }

    /// The name that the title of the pane shows.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Plain => "plain",
        }
    }

    /// Tells whether the theme colours the syntax of the lines.
    pub(crate) fn highlights(self) -> bool {
        self != Self::Plain
    }

    /// The text colour of an added or removed line.
    pub(crate) fn text(self) -> Color {
        match self {
            Self::Light => Color::Black,
            _ => Color::White,
        }
    }

    /// The background of a removed line.
    pub(crate) fn removed_background(self) -> Color {
        match self {
            Self::Light => Color::Indexed(224),
            _ => Color::Indexed(52),
        }
    }

    /// The background of an added line.
    pub(crate) fn added_background(self) -> Color {
        match self {
            Self::Light => Color::Indexed(194),
            _ => Color::Indexed(22),
        }
    }

    /// The background of the header of a file.
    pub(crate) fn header_background(self) -> Color {
        match self {
            Self::Light => Color::Indexed(252),
            _ => Color::Indexed(238),
        }
    }

    /// The colour of a category.
    pub(crate) fn color(self, category: Category) -> Color {
        let (dark, light) = match category {
            Category::Comment => (245, 243),
            Category::String => (180, 94),
            Category::Number | Category::Constant => (215, 130),
            Category::Keyword => (141, 90),
            Category::Function => (111, 25),
            Category::Type => (80, 30),
            Category::Property => (117, 24),
        };
        Color::Indexed(if self == Self::Light { light } else { dark })
    }
}

/// The grammars that the view knows.
#[derive(Debug, Clone, Copy)]
enum Language {
    Rust,
    Toml,
    Bash,
    Json,
    Markdown,
}

impl Language {
    /// The grammar for a path, by its extension, or `None`.
    fn of(path: &str) -> Option<Self> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let (_, extension) = name.rsplit_once('.')?;
        Some(match extension {
            "rs" => Self::Rust,
            "toml" => Self::Toml,
            "sh" | "bash" => Self::Bash,
            "json" => Self::Json,
            "md" | "markdown" => Self::Markdown,
            _ => return None,
        })
    }

    /// The configuration of the grammar, built on first use and kept for the whole run. `None` if
    /// the grammar and its query do not fit together, which a test of the crate would catch.
    fn configuration(self) -> Option<&'static HighlightConfiguration> {
        static RUST: OnceLock<Option<HighlightConfiguration>> = OnceLock::new();
        static TOML: OnceLock<Option<HighlightConfiguration>> = OnceLock::new();
        static BASH: OnceLock<Option<HighlightConfiguration>> = OnceLock::new();
        static JSON: OnceLock<Option<HighlightConfiguration>> = OnceLock::new();
        static MARKDOWN: OnceLock<Option<HighlightConfiguration>> = OnceLock::new();
        let (cell, language, query) = match self {
            Self::Rust => (
                &RUST,
                tree_sitter_rust::LANGUAGE,
                tree_sitter_rust::HIGHLIGHTS_QUERY,
            ),
            Self::Toml => (
                &TOML,
                tree_sitter_toml_ng::LANGUAGE,
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            ),
            Self::Bash => (
                &BASH,
                tree_sitter_bash::LANGUAGE,
                tree_sitter_bash::HIGHLIGHT_QUERY,
            ),
            Self::Json => (
                &JSON,
                tree_sitter_json::LANGUAGE,
                tree_sitter_json::HIGHLIGHTS_QUERY,
            ),
            // Only the block structure: the inline grammar needs injections.
            Self::Markdown => (
                &MARKDOWN,
                tree_sitter_md::LANGUAGE,
                tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            ),
        };
        cell.get_or_init(|| {
            let mut configuration =
                HighlightConfiguration::new(language.into(), "stemma", query, "", "").ok()?;
            configuration.configure(&NAMES);
            Some(configuration)
        })
        .as_ref()
    }
}

/// The highlighted ranges of every line of one text: byte offsets into the line, each with its
/// category, in order and without overlap.
pub(crate) type LineRanges = Vec<Vec<(Range<usize>, Category)>>;

/// The highlighted ranges of both sides of a file.
#[derive(Debug, Clone, Default)]
pub(crate) struct FileHighlights {
    pub(crate) old: LineRanges,
    pub(crate) new: LineRanges,
}

impl FileHighlights {
    /// Highlights the old and the new lines of the file at `path`, or returns `None` for a path
    /// whose language is not known.
    pub(crate) fn of(path: &str, old: &[String], new: &[String]) -> Option<Self> {
        let configuration = Language::of(path)?.configuration()?;
        Some(Self {
            old: highlight(configuration, old).unwrap_or_default(),
            new: highlight(configuration, new).unwrap_or_default(),
        })
    }
}

/// Highlights `lines`, which keep their line ends, as one text. `None` if the highlighter fails.
fn highlight(configuration: &HighlightConfiguration, lines: &[String]) -> Option<LineRanges> {
    let source = lines.concat();
    let mut starts = Vec::with_capacity(lines.len() + 1);
    let mut offset = 0;
    for line in lines {
        starts.push(offset);
        offset += line.len();
    }
    starts.push(offset);
    let mut ranges: LineRanges = vec![Vec::new(); lines.len()];
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(configuration, source.as_bytes(), None, |_| None)
        .ok()?;
    let mut stack: Vec<Category> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(highlight) => {
                stack.push(*CATEGORIES.get(highlight.0)?);
            }
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let Some(&category) = stack.last() else {
                    continue;
                };
                let mut start = start;
                while start < end {
                    let line = starts.partition_point(|&at| at <= start).checked_sub(1)?;
                    let line_end = *starts.get(line + 1)?;
                    let stop = end.min(line_end);
                    ranges
                        .get_mut(line)?
                        .push((start - starts[line]..stop - starts[line], category));
                    start = stop;
                }
            }
        }
    }
    Some(ranges)
}

/// The colour of every character of `content`, which is a line without its line end, from the
/// ranges of the line. A character that no range covers has no colour.
pub(crate) fn char_colors(
    content: &str,
    ranges: &[(Range<usize>, Category)],
    theme: Theme,
) -> Vec<Option<Color>> {
    let mut next = 0;
    content
        .char_indices()
        .map(|(at, _)| {
            while ranges.get(next).is_some_and(|(range, _)| range.end <= at) {
                next += 1;
            }
            ranges
                .get(next)
                .filter(|(range, _)| range.start <= at)
                .map(|(_, category)| theme.color(*category))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.split_inclusive('\n').map(str::to_owned).collect()
    }

    #[test]
    fn every_grammar_loads_with_its_query() {
        for path in ["a.rs", "a.toml", "a.sh", "a.json", "a.md"] {
            let language = Language::of(path).expect("a known extension");
            assert!(language.configuration().is_some(), "{path}");
        }
    }

    #[test]
    fn an_unknown_extension_has_no_language() {
        assert!(Language::of("a.txt").is_none());
        assert!(Language::of("Makefile").is_none());
        assert!(FileHighlights::of("a.txt", &lines("x\n"), &[]).is_none());
    }

    #[test]
    fn a_block_comment_is_coloured_on_every_line() {
        let text = lines("/* one\ntwo\nthree */\nlet x = 1;\n");
        let found = FileHighlights::of("a.rs", &text, &[]).unwrap();
        for line in 0..3 {
            assert!(
                found.old[line]
                    .iter()
                    .any(|(_, category)| *category == Category::Comment),
                "line {line}"
            );
        }
        assert!(found.old[3]
            .iter()
            .all(|(_, category)| *category != Category::Comment));
    }

    #[test]
    fn the_colours_of_a_line_follow_the_characters() {
        let ranges = vec![(0..3, Category::Keyword)];
        let colours = char_colors("let x", &ranges, Theme::Dark);
        assert_eq!(colours.len(), 5);
        assert!(colours[..3].iter().all(Option::is_some));
        assert!(colours[3..].iter().all(Option::is_none));
    }

    #[test]
    fn broken_text_does_not_fail() {
        let text = lines("fn ( { \"unterminated\n\u{0}\n");
        assert!(FileHighlights::of("a.rs", &text, &text).is_some());
    }
}
