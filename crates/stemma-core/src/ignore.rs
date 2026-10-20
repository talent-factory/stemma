//! The exclusion file `.stemmaignore` (ADR-012).
//!
//! The file lists paths that are never tracked, not even by `stemma track`. (`stemma status`,
//! STEMMA-4, will not report them as untracked.) [`IgnoreRules::parse`] reads the bytes of the file
//! and accepts only the canonical form, as the readers of the index, trees, and the log do;
//! [`IgnoreRules::matching`] says whether a path is excluded. [`IgnoreRules::read`] reads the file
//! of a working directory.
//!
//! A line is a comment (it starts with `#`) or a pattern: parts separated by `/`, each part a name
//! with the wildcards `*` (any run of characters inside one name) and `?` (one character), or the
//! whole part `**` (any number of directories, also none, so `foo/**` excludes `foo` too). A
//! leading `/` or a `/` between parts anchors the pattern at the root; otherwise it matches a name
//! at any depth. A trailing `/` matches only a directory. There is no negation, escape, or
//! character class: the characters `\`, `[`, `]` and a leading `!` are rejected, so that they stay
//! free for a later version.
//!
//! Names are compared byte by byte and case-sensitively, also on a file system that folds case or
//! normalizes Unicode: a pattern `.ENV` does not exclude `.env`, and a pattern in NFC does not
//! exclude a name that the file system keeps in NFD. `track` makes a path spell its names as on
//! disk, but the patterns are the person's text (open item of ADR-012).

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use crate::object::{name_problem, NameProblem};

/// The name of the exclusion file, in the root of the working directory.
pub const FILE_NAME: &str = ".stemmaignore";

/// The most bytes that a file may have.
pub const MAX_FILE_BYTES: usize = 64 * 1024;

/// The most bytes that a line may have, without the newline.
pub const MAX_LINE_BYTES: usize = 1024;

/// The patterns of an exclusion file. Two values are equal if they have the same patterns on the
/// same lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IgnoreRules {
    patterns: Vec<Pattern>,
}

/// One pattern with the line it was written on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    line: NonZeroUsize,
    /// Matches only a directory.
    directory_only: bool,
    target: Target,
}

/// What a pattern is matched against. The two shapes are the two cases of anchoring, so a pattern
/// cannot be anchored and have the shape of one that is not.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// One name, matched against the last name of the path, at any depth.
    AnyDepth(Vec<Glob>),
    /// Parts matched against the whole path from the root, part by part.
    FromRoot(Vec<Part>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    /// `**`: any number of parts of the path, also none (and so also the last one).
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
    /// The rules of a working directory without an exclusion file: nothing is excluded. The same
    /// value as [`Default::default`].
    pub fn empty() -> Self {
        Self::default()
    }

    /// Reads and checks the bytes of an exclusion file.
    ///
    /// The lines are checked in order, so the error names the first line that the reader refuses;
    /// a last line without a newline is reported after the lines before it. A problem of the whole
    /// file (its size, a byte order mark, bytes that are not UTF-8) has no line and comes first.
    ///
    /// # Errors
    ///
    /// Returns an [`IgnoreParseError`] for the first problem.
    pub fn parse(bytes: &[u8]) -> Result<Self, IgnoreParseError> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(IgnoreParseError::File(FileProblem::TooLarge));
        }
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            return Err(IgnoreParseError::File(FileProblem::ByteOrderMark));
        }
        let text =
            std::str::from_utf8(bytes).map_err(|_| IgnoreParseError::File(FileProblem::NotUtf8))?;
        let mut seen: HashMap<&str, NonZeroUsize> = HashMap::new();
        let mut patterns = Vec::new();
        let mut rest = text;
        let mut number = 0usize;
        while let Some(end) = rest.find('\n') {
            number += 1;
            let at = NonZeroUsize::new(number).expect("a counted line is numbered from 1");
            let line = &rest[..end];
            rest = &rest[end + 1..];
            let problem = |problem| IgnoreParseError::Line { line: at, problem };
            if line.is_empty() {
                return Err(problem(LineProblem::Empty));
            }
            if line.len() > MAX_LINE_BYTES {
                return Err(problem(LineProblem::TooLong));
            }
            if line.contains('\r') {
                return Err(problem(LineProblem::CarriageReturn));
            }
            if line.chars().any(char::is_control) {
                return Err(problem(LineProblem::ControlCharacter));
            }
            if line.starts_with(' ') || line.ends_with(' ') {
                return Err(problem(LineProblem::Space));
            }
            if let Some(first) = seen.insert(line, at) {
                return Err(problem(LineProblem::Repeated { first }));
            }
            if line.starts_with('#') {
                continue;
            }
            patterns.push(parse_pattern(line, at).map_err(problem)?);
        }
        if !rest.is_empty() {
            let at = NonZeroUsize::new(number + 1).expect("a line number is at least 1");
            return Err(IgnoreParseError::Line {
                line: at,
                problem: LineProblem::MissingNewline,
            });
        }
        Ok(Self { patterns })
    }

    /// Reads the exclusion file of the working directory `root`. A missing file means no
    /// patterns. The file is read from the directory and not from a tree, so an edit takes effect
    /// at once.
    ///
    /// The path is inspected without following a link, then opened, and the open file is checked
    /// to be the one that was inspected (same device and inode on Unix) and a regular file, so
    /// that a file replaced in between is an error and not a read through a link. The bytes that
    /// were read have to be as many as the file has, so a file that is written while it is read is
    /// an error and not a prefix. One window is left: a file replaced by a pipe between the
    /// inspection and the open blocks the open (open item of ADR-012).
    ///
    /// # Errors
    ///
    /// Returns [`IgnoreError::NotAFile`] if the name is a link, a directory, or anything but a
    /// regular file, [`IgnoreError::Changed`] if it was replaced, removed, or written while it was
    /// read, [`IgnoreError::Io`] if it cannot be inspected, opened, or read, and
    /// [`IgnoreError::Invalid`] if the reader refuses it.
    pub fn read(root: &Path) -> Result<Self, IgnoreError> {
        let path = root.join(FILE_NAME);
        let before = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::empty()),
            Err(source) => return Err(IgnoreError::io("inspect", &path, source)),
        };
        if !before.is_file() {
            return Err(IgnoreError::NotAFile {
                found: describe(&before),
                path,
            });
        }
        if before.len() > MAX_FILE_BYTES as u64 {
            return Err(IgnoreError::Invalid(IgnoreParseError::File(
                FileProblem::TooLarge,
            )));
        }
        let file = match File::open(&path) {
            Ok(file) => file,
            // Removed since the inspection: a rule that is gone is not "no rules".
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(IgnoreError::Changed { path });
            }
            Err(source) => return Err(IgnoreError::io("open", &path, source)),
        };
        let during = file
            .metadata()
            .map_err(|source| IgnoreError::io("inspect", &path, source))?;
        if !during.is_file() || !same_file(&before, &during) {
            return Err(IgnoreError::Changed { path });
        }
        let bytes = read_checked(file, during.len(), &path)?;
        Self::parse(&bytes).map_err(IgnoreError::Invalid)
    }

    /// Whether a pattern excludes the path, and if so, the line of the pattern that does. If a
    /// directory above the path is excluded, the line is the one of the outermost directory that
    /// is, and otherwise the one of the path itself; of several patterns for the same place, the
    /// first one. The path is relative to the working directory, written with `/`, and has no
    /// empty part. A path is excluded if a pattern matches it or one of its parent directories;
    /// `is_directory` says whether the path itself is a directory (the parents are). The exclusion
    /// file at the root is never excluded.
    ///
    /// The time is polynomial and never exponential: for each prefix of the path and each pattern,
    /// at most the product of the number of parts of the two, and for each pair at most the
    /// product of the lengths of the two names.
    pub fn matching(&self, path: &str, is_directory: bool) -> Option<NonZeroUsize> {
        if self.patterns.is_empty() || path.is_empty() || path == FILE_NAME {
            return None;
        }
        debug_assert!(
            !path.split('/').any(str::is_empty),
            "a path has no empty part: {path:?}"
        );
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

/// Reads the bytes of an open file whose length is `expected`. At most one byte more than the
/// limit is read, so a file that grew past it is refused without being read in full, and a file
/// that has another length than it had is an error and not a prefix.
fn read_checked(reader: impl Read, expected: u64, path: &Path) -> Result<Vec<u8>, IgnoreError> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| IgnoreError::io("read", path, source))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(IgnoreError::Invalid(IgnoreParseError::File(
            FileProblem::TooLarge,
        )));
    }
    if bytes.len() as u64 != expected {
        return Err(IgnoreError::Changed {
            path: path.to_path_buf(),
        });
    }
    Ok(bytes)
}

/// What a name that is not a regular file is, for the message.
fn describe(metadata: &Metadata) -> &'static str {
    let kind = metadata.file_type();
    if kind.is_symlink() {
        "a link"
    } else if kind.is_dir() {
        "a directory"
    } else {
        "neither a file, a link, nor a directory (a pipe, a socket, or a device)"
    }
}

/// Whether two descriptions are of the same file. Where the platform gives no identity, they are.
fn same_file(first: &Metadata, second: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        first.dev() == second.dev() && first.ino() == second.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = (first, second);
        true
    }
}

impl Pattern {
    /// Whether the pattern matches the path whose parts are given; `directory` says whether the
    /// last part is a directory.
    fn matches(&self, path: &[&str], directory: bool) -> bool {
        if self.directory_only && !directory {
            return false;
        }
        match &self.target {
            Target::AnyDepth(glob) => path.last().is_some_and(|name| glob_matches(glob, name)),
            Target::FromRoot(parts) => parts_match(parts, path),
        }
    }
}

/// Matches the parts of an anchored pattern against the parts of a path. `**` takes any number
/// of parts, also none. The table has one cell for each pair, so the time is the product of the
/// two lengths (times the cost of matching one name) and no input makes it grow faster.
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

fn parse_pattern(line: &str, number: NonZeroUsize) -> Result<Pattern, LineProblem> {
    if line.starts_with('!') {
        return Err(LineProblem::Negation);
    }
    if let Some(reserved) = line.chars().find(|c| matches!(c, '\\' | '[' | ']')) {
        return Err(LineProblem::ReservedCharacter(reserved));
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
            return Err(LineProblem::EmptyPart);
        }
        if text == "**" {
            parts.push(Part::Anywhere);
            continue;
        }
        if text == FILE_NAME {
            return Err(LineProblem::ExclusionFile);
        }
        // The wildcards stand for letters here: the name is checked as a tree checks a name.
        let plain: String = text
            .chars()
            .map(|c| if matches!(c, '*' | '?') { 'x' } else { c })
            .collect();
        if let Some(problem) = name_problem(&plain) {
            return Err(LineProblem::BadName {
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
        return Err(LineProblem::Everything);
    }
    let target = if rooted || parts.len() > 1 {
        Target::FromRoot(parts)
    } else {
        // One part, and not `**` (refused above): a name.
        match parts.pop() {
            Some(Part::Name(glob)) => Target::AnyDepth(glob),
            _ => return Err(LineProblem::Everything),
        }
    };
    Ok(Pattern {
        line: number,
        directory_only,
        target,
    })
}

/// Why the reader refuses a file: a problem of the whole file, or the first line that it refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IgnoreParseError {
    /// The file as a whole is wrong; there is no line.
    File(FileProblem),
    /// A line is wrong.
    Line {
        /// The number of the line, counted from 1.
        line: NonZeroUsize,
        /// What is wrong with it.
        problem: LineProblem,
    },
}

impl IgnoreParseError {
    /// The line of the problem, counted from 1, or `None` for a problem of the whole file.
    pub fn line(&self) -> Option<NonZeroUsize> {
        match self {
            Self::File(_) => None,
            Self::Line { line, .. } => Some(*line),
        }
    }
}

/// What is wrong with a file as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileProblem {
    /// The file has more than [`MAX_FILE_BYTES`] bytes.
    TooLarge,
    /// The file starts with a byte order mark.
    ByteOrderMark,
    /// The file is not UTF-8.
    NotUtf8,
}

/// What is wrong with a line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LineProblem {
    /// The last line does not end with a newline.
    MissingNewline,
    /// The line is empty.
    Empty,
    /// The line has more than [`MAX_LINE_BYTES`] bytes.
    TooLong,
    /// The line has a carriage return: only `\n` ends a line.
    CarriageReturn,
    /// The line has a control character (NUL, a tab, and the like).
    ControlCharacter,
    /// The line starts or ends with a space.
    Space,
    /// The line is the same as an earlier one.
    Repeated {
        /// The number of the first line of that text.
        first: NonZeroUsize,
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

impl fmt::Display for FileProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "has more than {MAX_FILE_BYTES} bytes"),
            Self::ByteOrderMark => f.write_str("starts with a byte order mark"),
            Self::NotUtf8 => f.write_str("is not UTF-8"),
        }
    }
}

impl fmt::Display for LineProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingNewline => f.write_str("does not end with a newline"),
            Self::Empty => f.write_str("empty line"),
            Self::TooLong => write!(f, "more than {MAX_LINE_BYTES} bytes"),
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
        match self {
            Self::File(problem) => write!(f, "the file {problem}"),
            Self::Line { line, problem } => write!(f, "line {line}: {problem}"),
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
        /// What it is, for the message.
        found: &'static str,
    },
    /// The file was replaced, removed, or written while it was read. Run the command again.
    Changed {
        /// The path of the exclusion file.
        path: PathBuf,
    },
    /// The file cannot be inspected, opened, or read.
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
            Self::NotAFile { path, found } => write!(
                f,
                "`{}` is {found}, not a regular file; an exclusion file has to be a regular file",
                path.display()
            ),
            Self::Changed { path } => write!(
                f,
                "`{}` was replaced, removed, or written while it was read; run the command again",
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
    use std::io::Cursor;

    fn rules(text: &str) -> IgnoreRules {
        IgnoreRules::parse(text.as_bytes()).unwrap()
    }

    fn refused(bytes: &[u8]) -> IgnoreParseError {
        IgnoreRules::parse(bytes).unwrap_err()
    }

    fn at(line: usize) -> Option<NonZeroUsize> {
        NonZeroUsize::new(line)
    }

    fn line_problem(error: &IgnoreParseError) -> &LineProblem {
        match error {
            IgnoreParseError::Line { problem, .. } => problem,
            other => panic!("a problem of the file: {other}"),
        }
    }

    // ---- the golden vectors of ADR-012: what the accepted files exclude

    #[test]
    fn the_accepted_files_of_the_adr_exclude_what_the_table_says() {
        let target = rules("target/\n");
        assert!(target.is_ignored("target", true) && target.is_ignored("a/b/target/x", false));
        assert!(!target.is_ignored("target", false));
        let build = rules("/build\n");
        assert!(build.is_ignored("build", false) && build.is_ignored("build/x", false));
        assert!(!build.is_ignored("src/build", true));
        let log = rules("*.log\n");
        assert!(log.is_ignored("a.log", false) && log.is_ignored("d/e/b.log", false));
        assert!(!log.is_ignored("a.logx", false));
        let draft = rules("docs/**/draft-?.md\n");
        assert!(draft.is_ignored("docs/draft-1.md", false));
        assert!(draft.is_ignored("docs/a/b/draft-x.md", false));
        let secrets = rules("# secrets\n.env\n");
        assert!(secrets.is_ignored(".env", false) && secrets.is_ignored("a/.env", false));
        assert!(!secrets.is_ignored("# secrets", false));
        assert_eq!(secrets.matching(".env", false), at(2));
        let nothing = rules("");
        assert!(nothing.is_empty() && !nothing.is_ignored("a", false));
    }

    #[test]
    fn the_rejected_files_of_the_adr_name_the_line_and_the_reason() {
        let cases: &[(&[u8], usize, &str)] = &[
            (b"a\n\nb\n", 2, "empty line"),
            (b"a\na\n", 2, "repeats line 1"),
            (b"a \n", 1, "space at the start or end"),
            (b" a\n", 1, "space at the start or end"),
            (b" \n", 1, "space at the start or end"),
            (b"a", 1, "does not end with a newline"),
            (b"a\nb", 2, "does not end with a newline"),
            (b"a\n# c", 2, "does not end with a newline"),
            (b"a\n\nb", 2, "empty line"),
            (b"!keep\n", 1, "negation"),
            (b"a\\b\n", 1, "`\\` is reserved"),
            (b"[ab]\n", 1, "`[` is reserved"),
            (b"a]\n", 1, "`]` is reserved"),
            (b"a//b\n", 1, "empty part"),
            (b"/\n", 1, "empty part"),
            (b"//\n", 1, "empty part"),
            (b"a//\n", 1, "empty part"),
            (b"./a\n", 1, "`.` is not a valid name"),
            (b"../a\n", 1, "`..` is not a valid name"),
            (b"**\n", 1, "exclude everything"),
            (b"**/**\n", 1, "exclude everything"),
            (b"/**\n", 1, "exclude everything"),
            (b"**/\n", 1, "exclude everything"),
            (b".stemma\n", 1, "not a valid name"),
            (b"a/.STEMMA\n", 1, "not a valid name"),
            (b".stemmaignore\n", 1, "never excluded"),
            (b"/.stemmaignore\n", 1, "never excluded"),
            (b"a/.stemmaignore\n", 1, "never excluded"),
            (b"a\r\n", 1, "carriage return"),
            (b"a\tb\n", 1, "control character"),
            (b"\ta\n", 1, "control character"),
            (b"a\0b\n", 1, "control character"),
            (b"a\x7Fb\n", 1, "control character"),
            ("a\u{85}b\n".as_bytes(), 1, "control character"),
            ("a\u{200B}b\n".as_bytes(), 1, "not a valid name"),
            (b"a\nb\na\n", 3, "repeats line 1"),
        ];
        for (bytes, line, text) in cases {
            let error = refused(bytes);
            assert_eq!(error.line(), at(*line), "{bytes:?}: {error}");
            assert!(error.to_string().contains(text), "{bytes:?}: {error}");
        }
    }

    #[test]
    fn a_problem_of_the_whole_file_has_no_line_and_is_written_without_one() {
        let cases: &[(&[u8], FileProblem, &str)] = &[
            (
                b"# one\nok\n\xff\n",
                FileProblem::NotUtf8,
                "the file is not UTF-8",
            ),
            (
                b"\xEF\xBB\xBFa\n",
                FileProblem::ByteOrderMark,
                "the file starts with a byte order mark",
            ),
        ];
        for (bytes, problem, text) in cases {
            let error = refused(bytes);
            assert_eq!(error, IgnoreParseError::File(*problem));
            assert_eq!(error.line(), None);
            assert_eq!(error.to_string(), *text);
        }
        assert_eq!(refused(b"a\n\n").to_string(), "line 2: empty line");
    }

    #[test]
    fn the_first_problem_is_the_one_reported_and_a_missing_newline_comes_after_the_lines() {
        assert_eq!(refused(b"!a\n\n").line(), at(1));
        assert_eq!(refused(b"a\n\n!b\n").line(), at(2));
        // The unterminated last line does not hide an earlier line that is wrong.
        assert_eq!(refused(b"a\n\nb").line(), at(2));
        assert_eq!(refused(b"a \nb").line(), at(1));
    }

    #[test]
    fn the_limits_of_a_line_are_in_bytes_and_exact() {
        let line = "a".repeat(MAX_LINE_BYTES);
        assert!(IgnoreRules::parse(format!("{line}\n").as_bytes()).is_ok());
        let long = "a".repeat(MAX_LINE_BYTES + 1);
        assert_eq!(
            line_problem(&refused(format!("{long}\n").as_bytes())),
            &LineProblem::TooLong
        );
        // 600 characters of two bytes are 1,200 bytes.
        let wide = "é".repeat(600);
        assert_eq!(
            line_problem(&refused(format!("{wide}\n").as_bytes())),
            &LineProblem::TooLong
        );
        // 512 characters of two bytes are 1,024 bytes.
        assert!(IgnoreRules::parse(format!("{}\n", "é".repeat(512)).as_bytes()).is_ok());
    }

    #[test]
    fn the_limit_of_the_file_is_exact() {
        // 64 distinct lines of 1,023 bytes and a newline are exactly 64 KiB.
        let file: String = (0..64).map(|n| format!("{n:0>1023}\n")).collect();
        assert_eq!(file.len(), MAX_FILE_BYTES);
        assert!(IgnoreRules::parse(file.as_bytes()).is_ok());
        let more = format!("{file}x");
        assert_eq!(
            refused(more.as_bytes()),
            IgnoreParseError::File(FileProblem::TooLarge)
        );
        // Exactly the limit, without the newline at the end, is a line problem, not a size one.
        let cut = &file[..file.len() - 1];
        assert_eq!(
            line_problem(&refused(cut.as_bytes())),
            &LineProblem::MissingNewline
        );
    }

    #[test]
    fn a_repeated_comment_is_a_repeated_line_and_names_the_first() {
        let error = refused(b"# a\nb\n# a\n");
        assert_eq!(error.line(), at(3));
        assert_eq!(
            line_problem(&error),
            &LineProblem::Repeated {
                first: NonZeroUsize::new(1).unwrap()
            }
        );
    }

    #[test]
    fn a_part_that_a_tree_refuses_passes_the_reason_through() {
        let error = refused("a/b\u{200B}c\n".as_bytes());
        assert!(matches!(
            line_problem(&error),
            LineProblem::BadName { part, problem }
                if part == "b\u{200B}c" && *problem == NameProblem::InvisibleCharacter
        ));
    }

    // ---- what is excluded

    #[test]
    fn a_name_without_a_slash_matches_at_any_depth_and_excludes_what_is_below() {
        let rules = rules("target/\n*.log\n.env\n");
        assert_eq!(rules.matching("target", true), at(1));
        assert_eq!(rules.matching("a/b/target", true), at(1));
        assert_eq!(rules.matching("target/debug/x.o", false), at(1));
        assert_eq!(rules.matching("a/b.log", false), at(2));
        assert_eq!(rules.matching("x/.env", false), at(3));
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
    fn a_trailing_slash_with_an_anchor_matches_a_directory_and_what_is_below_it() {
        let rules = rules("/build/\na/b/\n");
        assert!(rules.is_ignored("build", true));
        assert!(!rules.is_ignored("build", false));
        assert!(
            rules.is_ignored("build/x", false),
            "the parent is a directory"
        );
        assert!(rules.is_ignored("a/b", true) && rules.is_ignored("a/b/c", false));
        assert!(!rules.is_ignored("a/b", false));
        assert!(
            !rules.is_ignored("x/a/b", true),
            "a slash in the middle anchors"
        );
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
        // `**` takes the last part too, so `logs/**` excludes `logs` itself.
        assert!(rules.is_ignored("logs", false));
    }

    #[test]
    fn adjacent_and_middle_double_stars_and_a_single_star() {
        let rules = rules("a/**/**/b\n/*\n");
        assert!(
            rules.is_ignored("a/b", false),
            "a middle `**` matches nothing"
        );
        assert!(rules.is_ignored("a/x/y/z/b", false));
        assert!(!rules.is_ignored("c/a/b", false) || rules.is_ignored("c", true));
        // A single `*` excludes every name at the root, and the exclusion file is spared.
        assert!(rules.is_ignored("anything", false));
        assert!(
            rules.is_ignored("sub/file", false),
            "the parent `sub` is excluded"
        );
        assert!(!rules.is_ignored(FILE_NAME, false));
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
    }

    #[test]
    fn a_question_mark_is_one_unicode_scalar_not_one_byte_or_one_glyph() {
        let rules = rules("/c?d\n");
        assert!(rules.is_ignored("cäd", false), "two bytes");
        assert!(rules.is_ignored("c😀d", false), "four bytes");
        // `a` and a combining diaeresis are two scalars.
        assert!(!rules.is_ignored("ca\u{308}d", false));
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
        assert_eq!(one.matching("a.o", false), at(1));
        assert_eq!(other.matching("a.o", false), at(2));
        // A parent that matches comes before the file that matches.
        assert_eq!(one.matching("build/a.o", false), at(2));
    }

    #[test]
    fn the_exclusion_file_at_the_root_is_never_excluded_but_a_file_of_that_name_below_it_is() {
        let rules = rules(".stemma*\n");
        assert!(!rules.is_ignored(FILE_NAME, false));
        assert!(rules.is_ignored("sub/.stemmaignore", false));
    }

    #[test]
    fn a_path_without_rules_or_without_a_name_is_not_excluded() {
        assert!(!IgnoreRules::empty().is_ignored("a", false));
        assert!(!rules("a\n").is_ignored("", true));
        assert_eq!(IgnoreRules::empty(), IgnoreRules::default());
    }

    #[test]
    fn a_pattern_with_many_stars_and_one_with_many_double_stars_end_with_the_right_answer() {
        // Backtracking that grows exponentially would not come back from these.
        let stars = rules(&format!("/{}b\n", "a*".repeat(100)));
        assert!(!stars.is_ignored(&"a".repeat(3000), false));
        assert!(stars.is_ignored(&format!("{}b", "a".repeat(3000)), false));
        let deep = format!("{}x", "d/".repeat(300));
        let doubles = rules("**/**/**/**/y\n");
        assert!(!doubles.is_ignored(&deep, false));
        assert!(doubles.is_ignored(&format!("{}y", "d/".repeat(300)), false));
    }

    // ---- the file

    #[test]
    fn a_missing_file_and_an_empty_file_mean_no_patterns() {
        let dir = TempDir::new();
        assert!(IgnoreRules::read(dir.path()).unwrap().is_empty());
        std::fs::write(dir.path().join(FILE_NAME), "").unwrap();
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
            matches!(&error, IgnoreError::Invalid(e) if e.line() == at(2)),
            "{error}"
        );
        assert!(
            error.to_string().starts_with(".stemmaignore: line 2: "),
            "{error}"
        );
        std::fs::write(dir.path().join(FILE_NAME), b"\xff").unwrap();
        let error = IgnoreRules::read(dir.path()).unwrap_err();
        assert_eq!(error.to_string(), ".stemmaignore: the file is not UTF-8");
    }

    #[test]
    fn a_directory_is_not_an_exclusion_file_and_the_message_says_what_it_is() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.path().join(FILE_NAME)).unwrap();
        let error = IgnoreRules::read(dir.path()).unwrap_err();
        assert!(
            matches!(
                &error,
                IgnoreError::NotAFile {
                    found: "a directory",
                    ..
                }
            ),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("is a directory, not a regular file"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_not_an_exclusion_file_whatever_it_leads_to() {
        let dir = TempDir::new();
        let link = dir.path().join(FILE_NAME);
        // To a file, to a directory, and to nothing: a dangling link is no missing file.
        std::fs::write(dir.path().join("rules"), "a\n").unwrap();
        std::fs::create_dir(dir.path().join("dir")).unwrap();
        for target in ["rules", "dir", "gone"] {
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(target, &link).unwrap();
            let error = IgnoreRules::read(dir.path()).unwrap_err();
            assert!(
                matches!(
                    &error,
                    IgnoreError::NotAFile {
                        found: "a link",
                        ..
                    }
                ),
                "{target}: {error}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_is_not_an_exclusion_file_and_is_not_opened() {
        let dir = TempDir::new();
        let made = std::process::Command::new("mkfifo")
            .arg(dir.path().join(FILE_NAME))
            .status();
        if !made.is_ok_and(|status| status.success()) {
            return;
        }
        let error = IgnoreRules::read(dir.path()).unwrap_err();
        assert!(matches!(&error, IgnoreError::NotAFile { .. }), "{error}");
        assert!(
            error.to_string().contains("a pipe, a socket, or a device"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_opened_is_an_io_error_from_the_real_call() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "a\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = File::open(&path).is_ok();
        let result = IgnoreRules::read(dir.path());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        if readable {
            return;
        }
        let error = result.unwrap_err();
        assert!(
            matches!(
                &error,
                IgnoreError::Io {
                    operation: "open",
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.io_error().is_some());
        assert!(error.to_string().contains("could not open"), "{error}");
    }

    #[test]
    fn a_file_above_the_limit_is_refused() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join(FILE_NAME), vec![b'a'; MAX_FILE_BYTES + 1]).unwrap();
        let error = IgnoreRules::read(dir.path()).unwrap_err();
        assert!(
            matches!(
                &error,
                IgnoreError::Invalid(IgnoreParseError::File(FileProblem::TooLarge))
            ),
            "{error}"
        );
    }

    #[test]
    fn the_bytes_that_were_read_have_to_be_as_many_as_the_file_had_and_below_the_limit() {
        let path = Path::new(".stemmaignore");
        // As many as expected: the bytes.
        assert_eq!(read_checked(Cursor::new(b"a\n"), 2, path).unwrap(), b"a\n");
        // Fewer than expected (the file was cut) and more (it grew): changed, not a prefix.
        for expected in [5, 1] {
            let error = read_checked(Cursor::new(b"a\n"), expected, path).unwrap_err();
            assert!(matches!(error, IgnoreError::Changed { .. }), "{error}");
        }
        // Past the limit: refused after one byte more than the limit, not read in full.
        let long = vec![b'a'; MAX_FILE_BYTES * 3];
        let mut reader = Cursor::new(long);
        let error = read_checked(&mut reader, 10, path).unwrap_err();
        assert!(
            matches!(
                error,
                IgnoreError::Invalid(IgnoreParseError::File(FileProblem::TooLarge))
            ),
            "{error}"
        );
        assert_eq!(reader.position(), MAX_FILE_BYTES as u64 + 1);
        // Exactly the limit is fine.
        let exact = vec![b'a'; MAX_FILE_BYTES];
        assert_eq!(
            read_checked(Cursor::new(exact), MAX_FILE_BYTES as u64, path)
                .unwrap()
                .len(),
            MAX_FILE_BYTES
        );
    }

    #[test]
    fn io_error_is_only_for_the_file_system() {
        let dir = TempDir::new();
        let io = IgnoreError::io("read", dir.path(), io::Error::other("disk"));
        assert!(io.io_error().is_some());
        assert!(io.to_string().contains("could not read"), "{io}");
        for other in [
            IgnoreError::NotAFile {
                path: dir.path().to_path_buf(),
                found: "a link",
            },
            IgnoreError::Changed {
                path: dir.path().to_path_buf(),
            },
            IgnoreError::Invalid(IgnoreParseError::File(FileProblem::NotUtf8)),
        ] {
            assert!(other.io_error().is_none(), "{other}");
        }
        let changed = IgnoreError::Changed {
            path: dir.path().to_path_buf(),
        };
        assert!(
            changed.to_string().contains("run the command again"),
            "{changed}"
        );
    }
}
