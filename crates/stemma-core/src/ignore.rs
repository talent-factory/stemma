//! The exclusion file `.stemmaignore` (ADR-012).
//!
//! The file lists paths that are never tracked, not even by `stemma track`, and that are not
//! reported as untracked. [`IgnoreRules::parse`] reads the bytes of the file and accepts only the
//! canonical form, as the readers of the index, trees, and the log do; [`IgnoreRules::matching`]
//! says whether a path is excluded. [`IgnoreRules::read`] reads the file of a working directory.
//!
//! A line is a comment (it starts with `#`) or a pattern: parts separated by `/`, each part a name
//! with the wildcards `*` (any run of characters inside one name) and `?` (one character), or the
//! whole part `**` (any number of directories). A leading `/` or a `/` between parts anchors the
//! pattern at the root; otherwise it matches a name at any depth. A trailing `/` matches only a
//! directory. There is no negation, escape, or character class: the characters `\`, `[`, `]` and
//! a leading `!` are rejected, so that they stay free for a later version.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::object::{name_problem, NameProblem};

/// The name of the exclusion file, in the root of the working directory.
pub const FILE_NAME: &str = ".stemmaignore";

/// The most bytes that a file may have.
pub const MAX_FILE_BYTES: usize = 64 * 1024;

/// The most bytes that a line may have, without the newline.
pub const MAX_LINE_BYTES: usize = 1024;

/// The patterns of an exclusion file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IgnoreRules {
    patterns: Vec<Pattern>,
}

/// One pattern with the line it was written on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    line: usize,
    /// Matched against the whole path from the root, part by part, and not against a name.
    anchored: bool,
    /// Matches only a directory.
    directory_only: bool,
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    /// `**`: any number of directories, also none.
    Anywhere,
    /// A name with wildcards.
    Name(Vec<Glob>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Glob {
    Char(char),
    /// `*`: any run of characters, also none.
    Run,
    /// `?`: one character.
    One,
}

impl IgnoreRules {
    /// The rules of a working directory without an exclusion file: nothing is excluded.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Reads and checks the bytes of an exclusion file.
    ///
    /// # Errors
    ///
    /// Returns an [`IgnoreParseError`] that names the first line that the writer would not have
    /// written (see [`IgnoreProblem`]). A problem of the whole file has the line 0.
    pub fn parse(bytes: &[u8]) -> Result<Self, IgnoreParseError> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(IgnoreParseError::new(0, IgnoreProblem::FileTooLarge));
        }
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            return Err(IgnoreParseError::new(0, IgnoreProblem::ByteOrderMark));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| IgnoreParseError::new(0, IgnoreProblem::NotUtf8))?;
        if !text.is_empty() && !text.ends_with('\n') {
            let last = text.matches('\n').count() + 1;
            return Err(IgnoreParseError::new(last, IgnoreProblem::MissingNewline));
        }
        let mut seen: HashMap<&str, usize> = HashMap::new();
        let mut patterns = Vec::new();
        // The text ends in a newline, so the last item of the split is empty and is no line.
        let lines = text.split('\n');
        let count = text.matches('\n').count();
        for (index, line) in lines.take(count).enumerate() {
            let number = index + 1;
            let problem = |problem| IgnoreParseError::new(number, problem);
            if line.is_empty() {
                return Err(problem(IgnoreProblem::EmptyLine));
            }
            if line.len() > MAX_LINE_BYTES {
                return Err(problem(IgnoreProblem::LineTooLong));
            }
            if line.contains('\r') {
                return Err(problem(IgnoreProblem::CarriageReturn));
            }
            if line.chars().any(char::is_control) {
                return Err(problem(IgnoreProblem::ControlCharacter));
            }
            if line.starts_with(' ') || line.ends_with(' ') {
                return Err(problem(IgnoreProblem::Space));
            }
            if let Some(first) = seen.insert(line, number) {
                return Err(problem(IgnoreProblem::Repeated { first }));
            }
            if line.starts_with('#') {
                continue;
            }
            patterns.push(parse_pattern(line, number).map_err(problem)?);
        }
        Ok(Self { patterns })
    }

    /// Reads the exclusion file of the working directory `root`. A missing file means no
    /// patterns. The file is read from the directory and not from a tree, so an edit takes effect
    /// at once.
    ///
    /// # Errors
    ///
    /// Returns [`IgnoreError::NotAFile`] if the name is a link, a directory, or anything but a
    /// regular file, [`IgnoreError::Io`] if it cannot be inspected or read, and
    /// [`IgnoreError::Invalid`] if the reader refuses it.
    pub fn read(root: &Path) -> Result<Self, IgnoreError> {
        let path = root.join(FILE_NAME);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::empty()),
            Err(source) => return Err(IgnoreError::io("inspect", &path, source)),
        };
        if !metadata.is_file() {
            return Err(IgnoreError::NotAFile { path });
        }
        if metadata.len() > MAX_FILE_BYTES as u64 {
            return Err(IgnoreError::Invalid(IgnoreParseError::new(
                0,
                IgnoreProblem::FileTooLarge,
            )));
        }
        let mut bytes = Vec::new();
        File::open(&path)
            .and_then(|file| {
                // One byte more than the limit shows a file that grew since it was inspected.
                file.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut bytes)
            })
            .map_err(|source| IgnoreError::io("read", &path, source))?;
        Self::parse(&bytes).map_err(IgnoreError::Invalid)
    }

    /// Whether a pattern excludes the path, and if so, the line of the first pattern that does.
    /// The path is relative to the working directory, written with `/`, and has no empty part.
    /// A path is excluded if a pattern matches it or one of its parent directories; `is_directory`
    /// says whether the path itself is a directory (the parents are).
    pub fn matching(&self, path: &str, is_directory: bool) -> Option<usize> {
        // The exclusion file is never excluded, not even by a wildcard that matches its name
        // (ADR-012). Below the root a file of that name is a file like any other.
        if self.patterns.is_empty() || path.is_empty() || path == FILE_NAME {
            return None;
        }
        let parts: Vec<&str> = path.split('/').collect();
        // The shortest prefix first: a directory that is excluded excludes everything below it.
        (1..=parts.len()).find_map(|length| {
            let directory = length < parts.len() || is_directory;
            self.patterns
                .iter()
                .find(|pattern| pattern.matches(&parts[..length], directory))
                .map(|pattern| pattern.line)
        })
    }

    /// Whether the path is excluded: see [`matching`](Self::matching).
    pub fn is_ignored(&self, path: &str, is_directory: bool) -> bool {
        self.matching(path, is_directory).is_some()
    }

    /// Whether the file has no pattern.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

impl Pattern {
    /// Whether the pattern matches the path whose parts are given; `directory` says whether the
    /// last part is a directory.
    fn matches(&self, path: &[&str], directory: bool) -> bool {
        if self.directory_only && !directory {
            return false;
        }
        if !self.anchored {
            // One part, matched against the last name of the path.
            let Some(name) = path.last() else {
                return false;
            };
            return match &self.parts[..] {
                [Part::Name(glob)] => glob_matches(glob, name),
                _ => false,
            };
        }
        parts_match(&self.parts, path)
    }
}

/// Matches the parts of an anchored pattern against the parts of a path. `**` takes any number
/// of parts, also none. The table has one cell for each pair, so the time is the product of the
/// two lengths and no input makes it grow faster.
fn parts_match(pattern: &[Part], path: &[&str]) -> bool {
    let (rows, columns) = (pattern.len() + 1, path.len() + 1);
    let mut table = vec![false; rows * columns];
    table[rows * columns - 1] = true;
    for i in (0..pattern.len()).rev() {
        for j in (0..=path.len()).rev() {
            table[i * columns + j] = match &pattern[i] {
                Part::Anywhere => {
                    table[(i + 1) * columns + j] || (j < path.len() && table[i * columns + j + 1])
                }
                Part::Name(glob) => {
                    j < path.len()
                        && glob_matches(glob, path[j])
                        && table[(i + 1) * columns + j + 1]
                }
            };
        }
    }
    table[0]
}

/// Matches a name against `*` and `?` and literal characters, by characters (Unicode scalars).
/// When a `*` has to take one more character, the match goes back to the last `*` only, so the
/// time is the product of the two lengths at most.
fn glob_matches(glob: &[Glob], name: &str) -> bool {
    let name: Vec<char> = name.chars().collect();
    let (mut g, mut n) = (0, 0);
    let mut resume: Option<(usize, usize)> = None;
    while n < name.len() {
        match glob.get(g) {
            Some(Glob::Run) => {
                resume = Some((g, n));
                g += 1;
            }
            Some(Glob::One) => {
                g += 1;
                n += 1;
            }
            Some(Glob::Char(c)) if *c == name[n] => {
                g += 1;
                n += 1;
            }
            _ => match resume {
                Some((star, taken)) => {
                    resume = Some((star, taken + 1));
                    g = star + 1;
                    n = taken + 1;
                }
                None => return false,
            },
        }
    }
    glob[g..].iter().all(|item| *item == Glob::Run)
}

fn parse_pattern(line: &str, number: usize) -> Result<Pattern, IgnoreProblem> {
    if line.starts_with('!') {
        return Err(IgnoreProblem::Negation);
    }
    if let Some(reserved) = line.chars().find(|c| matches!(c, '\\' | '[' | ']')) {
        return Err(IgnoreProblem::ReservedCharacter(reserved));
    }
    let rooted = line.starts_with('/');
    let directory_only = line.len() > 1 && line.ends_with('/');
    let body = line.strip_prefix('/').unwrap_or(line);
    let body = if directory_only {
        body.strip_suffix('/').unwrap_or(body)
    } else {
        body
    };
    let mut parts = Vec::new();
    for text in body.split('/') {
        if text.is_empty() {
            return Err(IgnoreProblem::EmptyPart);
        }
        if text == "**" {
            parts.push(Part::Anywhere);
            continue;
        }
        if text == FILE_NAME {
            return Err(IgnoreProblem::ExclusionFile);
        }
        // The wildcards stand for letters here: the name is checked as a tree checks a name.
        let plain: String = text
            .chars()
            .map(|c| if matches!(c, '*' | '?') { 'x' } else { c })
            .collect();
        if let Some(problem) = name_problem(&plain) {
            return Err(IgnoreProblem::BadName {
                part: text.to_owned(),
                problem,
            });
        }
        parts.push(Part::Name(
            text.chars()
                .map(|c| match c {
                    '*' => Glob::Run,
                    '?' => Glob::One,
                    other => Glob::Char(other),
                })
                .collect(),
        ));
    }
    if parts.iter().all(|part| *part == Part::Anywhere) {
        return Err(IgnoreProblem::Everything);
    }
    let anchored = rooted || parts.len() > 1;
    // An unanchored pattern has one part, and `**` alone was refused above.
    debug_assert!(anchored || matches!(parts[..], [Part::Name(_)]));
    Ok(Pattern {
        line: number,
        anchored,
        directory_only,
        parts,
    })
}

/// Why the reader refuses a file: the first line that the writer would not have written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreParseError {
    line: usize,
    problem: IgnoreProblem,
}

impl IgnoreParseError {
    fn new(line: usize, problem: IgnoreProblem) -> Self {
        Self { line, problem }
    }

    /// The line of the problem, counted from 1, or 0 for a problem of the whole file.
    pub fn line(&self) -> usize {
        self.line
    }

    /// What is wrong.
    pub fn problem(&self) -> &IgnoreProblem {
        &self.problem
    }
}

/// What is wrong with a file or a line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IgnoreProblem {
    /// The file has more than [`MAX_FILE_BYTES`] bytes.
    FileTooLarge,
    /// The file starts with a byte order mark.
    ByteOrderMark,
    /// The file is not UTF-8.
    NotUtf8,
    /// The last line does not end with a newline.
    MissingNewline,
    /// The line is empty.
    EmptyLine,
    /// The line has more than [`MAX_LINE_BYTES`] bytes.
    LineTooLong,
    /// The line has a carriage return: only `\n` ends a line.
    CarriageReturn,
    /// The line has a control character (NUL, a tab, and the like).
    ControlCharacter,
    /// The line starts or ends with a space.
    Space,
    /// The line is the same as an earlier one.
    Repeated {
        /// The number of the first line of that text.
        first: usize,
    },
    /// The pattern starts with `!`: negation is not supported.
    Negation,
    /// The pattern has `\`, `[`, or `]`, which are reserved.
    ReservedCharacter(char),
    /// A part of the pattern is empty (`a//b`, or a lone `/`).
    EmptyPart,
    /// The pattern is the name of the exclusion file, which is never excluded.
    ExclusionFile,
    /// A part is a name that a tree refuses (`.`, `..`, `.stemma`, a control character, …).
    BadName {
        /// The part as written.
        part: String,
        /// Why a tree refuses it.
        problem: NameProblem,
    },
    /// Every part is `**`, so the pattern would exclude everything.
    Everything,
}

impl fmt::Display for IgnoreProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileTooLarge => write!(f, "has more than {MAX_FILE_BYTES} bytes"),
            Self::ByteOrderMark => f.write_str("starts with a byte order mark"),
            Self::NotUtf8 => f.write_str("is not UTF-8"),
            Self::MissingNewline => f.write_str("does not end with a newline"),
            Self::EmptyLine => f.write_str("empty line"),
            Self::LineTooLong => write!(f, "more than {MAX_LINE_BYTES} bytes"),
            Self::CarriageReturn => f.write_str("carriage return; only `\\n` ends a line"),
            Self::ControlCharacter => f.write_str("control character"),
            Self::Space => f.write_str("space at the start or end"),
            Self::Repeated { first } => write!(f, "repeats line {first}"),
            Self::Negation => f.write_str("negation (`!`) is not supported"),
            Self::ReservedCharacter(c) => {
                write!(f, "`{c}` is reserved and not supported in a pattern")
            }
            Self::EmptyPart => f.write_str("empty part between slashes"),
            Self::ExclusionFile => {
                write!(
                    f,
                    "`{FILE_NAME}` is never excluded, so no pattern can name it"
                )
            }
            Self::BadName { part, problem } => {
                write!(
                    f,
                    "`{}` is not a valid name: {problem}",
                    part.escape_debug()
                )
            }
            Self::Everything => f.write_str("the pattern would exclude everything"),
        }
    }
}

impl fmt::Display for IgnoreParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "the exclusion file {}", self.problem)
        } else {
            write!(f, "line {}: {}", self.line, self.problem)
        }
    }
}

impl Error for IgnoreParseError {}

/// Why [`IgnoreRules::read`] fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum IgnoreError {
    /// `.stemmaignore` is a link, a directory, or another thing that is not a regular file.
    NotAFile {
        /// The path of the exclusion file.
        path: PathBuf,
    },
    /// The file cannot be inspected or read.
    Io {
        /// What was done.
        operation: &'static str,
        /// The path of the exclusion file.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// The reader refuses the file.
    Invalid(IgnoreParseError),
}

impl IgnoreError {
    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    /// Returns the error of the file system, if that is what failed. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl fmt::Display for IgnoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAFile { path } => write!(
                f,
                "`{}` is not a regular file (a link or a directory cannot be an exclusion file)",
                path.display()
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
            Self::Invalid(error) => write!(f, "{FILE_NAME}: {error}"),
        }
    }
}

impl Error for IgnoreError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn rules(text: &str) -> IgnoreRules {
        IgnoreRules::parse(text.as_bytes()).unwrap()
    }

    fn refused(bytes: &[u8]) -> IgnoreParseError {
        IgnoreRules::parse(bytes).unwrap_err()
    }

    // ---- the golden vectors of ADR-012

    #[test]
    fn the_accepted_files_of_the_adr_are_accepted() {
        for text in [
            "target/\n",
            "/build\n",
            "*.log\n",
            "docs/**/draft-?.md\n",
            "# secrets\n.env\n",
            "",
        ] {
            assert!(IgnoreRules::parse(text.as_bytes()).is_ok(), "{text:?}");
        }
        assert!(rules("").is_empty());
    }

    #[test]
    fn the_rejected_files_of_the_adr_name_the_line_and_the_reason() {
        let cases: &[(&[u8], usize, &str)] = &[
            (b"a\n\nb\n", 2, "empty line"),
            (b"a\na\n", 2, "repeats line 1"),
            (b"a \n", 1, "space at the start or end"),
            (b" a\n", 1, "space at the start or end"),
            (b"a", 1, "does not end with a newline"),
            (b"!keep\n", 1, "negation"),
            (b"a\\b\n", 1, "`\\` is reserved"),
            (b"[ab]\n", 1, "`[` is reserved"),
            (b"a]\n", 1, "`]` is reserved"),
            (b"a//b\n", 1, "empty part"),
            (b"/\n", 1, "empty part"),
            (b"//\n", 1, "empty part"),
            (b"./a\n", 1, "`.` is not a valid name"),
            (b"../a\n", 1, "`..` is not a valid name"),
            (b"**\n", 1, "exclude everything"),
            (b"**/**\n", 1, "exclude everything"),
            (b"/**\n", 1, "exclude everything"),
            (b".stemma\n", 1, "not a valid name"),
            (b"a/.STEMMA\n", 1, "not a valid name"),
            (b".stemmaignore\n", 1, "never excluded"),
            (b"/.stemmaignore\n", 1, "never excluded"),
            (b"a\r\n", 1, "carriage return"),
            (b"a\tb\n", 1, "control character"),
            (b"a\0b\n", 1, "control character"),
            (b"# one\nok\n\xff\n", 0, "not UTF-8"),
            (b"\xEF\xBB\xBFa\n", 0, "byte order mark"),
        ];
        for (bytes, line, text) in cases {
            let error = refused(bytes);
            assert_eq!(error.line(), *line, "{bytes:?}: {error}");
            assert!(error.to_string().contains(text), "{bytes:?}: {error}");
        }
    }

    #[test]
    fn an_error_for_a_line_is_written_as_line_n_and_one_for_the_file_without_a_number() {
        assert_eq!(refused(b"a\n\n").to_string(), "line 2: empty line");
        assert_eq!(
            refused(b"\xFF").to_string(),
            "the exclusion file is not UTF-8"
        );
    }

    #[test]
    fn the_limits_of_a_line_and_of_the_file_are_exact() {
        let line = "a".repeat(MAX_LINE_BYTES);
        assert!(IgnoreRules::parse(format!("{line}\n").as_bytes()).is_ok());
        let long = "a".repeat(MAX_LINE_BYTES + 1);
        assert_eq!(
            refused(format!("{long}\n").as_bytes()).problem(),
            &IgnoreProblem::LineTooLong
        );
        // Lines of 1,000 bytes, each different, up to the limit of the file and one line more.
        let mut file = String::new();
        let mut n = 0;
        while file.len() + 1001 <= MAX_FILE_BYTES {
            file.push_str(&format!("{n:0>1000}\n"));
            n += 1;
        }
        assert!(file.len() <= MAX_FILE_BYTES);
        assert!(IgnoreRules::parse(file.as_bytes()).is_ok());
        file.push_str(&format!("{n:0>1000}\n"));
        assert_eq!(
            refused(file.as_bytes()).problem(),
            &IgnoreProblem::FileTooLarge
        );
    }

    #[test]
    fn a_repeated_comment_is_a_repeated_line_too() {
        assert_eq!(
            refused(b"# a\n# a\n").problem(),
            &IgnoreProblem::Repeated { first: 1 }
        );
    }

    #[test]
    fn the_first_problem_of_the_file_is_the_one_reported() {
        assert_eq!(refused(b"!a\n\n").line(), 1);
        assert_eq!(refused(b"a\n\n!b\n").line(), 2);
    }

    // ---- what is excluded

    #[test]
    fn a_name_without_a_slash_matches_at_any_depth_and_excludes_what_is_below() {
        let rules = rules("target/\n*.log\n.env\n");
        assert_eq!(rules.matching("target", true), Some(1));
        assert_eq!(rules.matching("a/b/target", true), Some(1));
        assert_eq!(rules.matching("target/debug/x.o", false), Some(1));
        assert_eq!(rules.matching("a/b.log", false), Some(2));
        assert_eq!(rules.matching("x/.env", false), Some(3));
        // A trailing slash matches directories only.
        assert_eq!(rules.matching("target", false), None);
        assert_eq!(rules.matching("src/main.rs", false), None);
        assert_eq!(rules.matching("targets", true), None);
    }

    #[test]
    fn a_rooted_pattern_and_one_with_a_slash_match_from_the_root() {
        let rules = rules("/build\ndocs/draft.md\n");
        assert!(rules.is_ignored("build", true));
        assert!(rules.is_ignored("build/x", false));
        assert!(!rules.is_ignored("src/build", true));
        assert!(rules.is_ignored("docs/draft.md", false));
        assert!(!rules.is_ignored("a/docs/draft.md", false));
        assert!(!rules.is_ignored("docs", true));
    }

    #[test]
    fn two_stars_take_any_number_of_directories_also_none() {
        let rules = rules("docs/**/draft-?.md\n**/node_modules\nlogs/**\n");
        assert!(rules.is_ignored("docs/draft-1.md", false));
        assert!(rules.is_ignored("docs/a/b/draft-x.md", false));
        assert!(!rules.is_ignored("docs/draft-10.md", false));
        assert!(!rules.is_ignored("other/draft-1.md", false));
        assert!(rules.is_ignored("node_modules", true));
        assert!(rules.is_ignored("a/b/node_modules/x", false));
        assert!(rules.is_ignored("logs/a", false));
        assert!(rules.is_ignored("logs/a/b", false));
    }

    #[test]
    fn a_star_stays_inside_one_name_and_a_question_mark_is_one_character() {
        let rules = rules("/a*b\n/c?d\n");
        assert!(rules.is_ignored("ab", false));
        assert!(rules.is_ignored("axxb", false));
        assert!(
            !rules.is_ignored("a/b", false),
            "a star never crosses a slash"
        );
        assert!(rules.is_ignored("cxd", false));
        assert!(!rules.is_ignored("cd", false));
        assert!(!rules.is_ignored("cxxd", false));
        // One character is one Unicode scalar, and not one byte.
        assert!(rules.is_ignored("cäd", false));
    }

    #[test]
    fn names_are_compared_by_bytes_and_case_matters() {
        let rules = rules("Target\n/é\n");
        assert!(rules.is_ignored("Target", true));
        assert!(!rules.is_ignored("target", true));
        assert!(rules.is_ignored("é", false));
        // `e` followed by a combining accent is another name.
        assert!(!rules.is_ignored("e\u{301}", false));
    }

    #[test]
    fn the_first_matching_line_is_named_and_the_order_does_not_change_what_is_excluded() {
        let one = rules("*.o\nbuild\n");
        let other = rules("build\n*.o\n");
        for path in ["a.o", "build/a.o", "src/x"] {
            assert_eq!(one.is_ignored(path, false), other.is_ignored(path, false));
        }
        assert_eq!(one.matching("a.o", false), Some(1));
        assert_eq!(other.matching("a.o", false), Some(2));
        // A parent that matches comes before the file that matches.
        assert_eq!(one.matching("build/a.o", false), Some(2));
    }

    #[test]
    fn the_exclusion_file_at_the_root_is_never_excluded_but_a_file_of_that_name_below_it_is() {
        let rules = rules(".stemma*\n");
        assert!(!rules.is_ignored(".stemmaignore", false));
        assert!(rules.is_ignored("sub/.stemmaignore", false));
    }

    #[test]
    fn a_path_without_rules_or_without_a_name_is_not_excluded() {
        assert!(!IgnoreRules::empty().is_ignored("a", false));
        assert!(!rules("a\n").is_ignored("", true));
    }

    #[test]
    fn a_pattern_with_many_stars_does_not_take_long() {
        let pattern = format!("/{}b\n", "a*".repeat(100));
        let rules = rules(&pattern);
        let name = "a".repeat(3000);
        let start = std::time::Instant::now();
        assert!(!rules.is_ignored(&name, false));
        let deep = format!("{}x", "d/".repeat(500));
        let rules_deep = rules_for("**/**/**/**/y\n");
        assert!(!rules_deep.is_ignored(&deep, false));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    fn rules_for(text: &str) -> IgnoreRules {
        IgnoreRules::parse(text.as_bytes()).unwrap()
    }

    // ---- the file

    #[test]
    fn a_missing_file_means_no_patterns() {
        let dir = TempDir::new();
        assert!(IgnoreRules::read(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn the_file_is_read_from_the_directory_and_checked() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join(FILE_NAME), "*.log\n").unwrap();
        let rules = IgnoreRules::read(dir.path()).unwrap();
        assert!(rules.is_ignored("a.log", false));
        std::fs::write(dir.path().join(FILE_NAME), "*.log\n\n").unwrap();
        let error = IgnoreRules::read(dir.path()).unwrap_err();
        assert!(
            matches!(&error, IgnoreError::Invalid(e) if e.line() == 2),
            "{error}"
        );
        assert!(
            error.to_string().starts_with(".stemmaignore: line 2: "),
            "{error}"
        );
    }

    #[test]
    fn a_directory_is_not_an_exclusion_file() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.path().join(FILE_NAME)).unwrap();
        let error = IgnoreRules::read(dir.path()).unwrap_err();
        assert!(matches!(error, IgnoreError::NotAFile { .. }), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_not_an_exclusion_file_not_even_one_that_leads_to_a_file() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("rules"), "a\n").unwrap();
        std::os::unix::fs::symlink("rules", dir.path().join(FILE_NAME)).unwrap();
        let error = IgnoreRules::read(dir.path()).unwrap_err();
        assert!(matches!(error, IgnoreError::NotAFile { .. }), "{error}");
    }

    #[test]
    fn a_file_above_the_limit_is_refused_without_being_read() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join(FILE_NAME), vec![b'a'; MAX_FILE_BYTES + 1]).unwrap();
        let error = IgnoreRules::read(dir.path()).unwrap_err();
        assert!(
            matches!(&error, IgnoreError::Invalid(e) if e.problem() == &IgnoreProblem::FileTooLarge),
            "{error}"
        );
    }

    #[test]
    fn io_error_is_only_for_the_file_system() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.path().join(FILE_NAME)).unwrap();
        assert!(IgnoreRules::read(dir.path())
            .unwrap_err()
            .io_error()
            .is_none());
        let io = IgnoreError::io("read", dir.path(), io::Error::other("disk"));
        assert!(io.io_error().is_some());
        assert!(io.to_string().contains("could not read"), "{io}");
    }
}
