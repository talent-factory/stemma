//! The policy file `.stemma-policy` (ADR-003, ADR-004, STEMMA-230).
//!
//! The file turns the table of risk levels of ADR-004 into rules that the core can evaluate.
//! [`Policy::parse`] reads the bytes of the file, as strictly as the reader of the exclusion file
//! (ADR-012): the same limits, the same line ends, and no line that it does not know.
//! [`Policy::level_of`] gives the level of a change: the highest level of all paths that it
//! touches.
//!
//! The policy is read in the version of the base commit, not from the working directory, so this
//! module only reads bytes. Which commit to read, and the question to the authorizer, belong to the
//! slices that enforce the rules.
//!
//! # Format
//!
//! The first line that is neither empty nor a comment is the header `stemma-policy 1`; a file
//! without it, or with another version, is an error. Empty lines and comments (`#`) are allowed,
//! as in the exclusion file. Every other line is one of these, with one space between the fields:
//!
//! | Line | Meaning |
//! |---|---|
//! | `level N PATTERN` | The paths that the pattern matches have level `N` (0 to 3). |
//! | `operation NAME N` | The operation `NAME`, which touches no path, has level `N`. |
//! | `approvals N COUNT` | A change of level `N` needs `COUNT` agent approvals (at most 8). |
//! | `lease MINUTES` | The lease of a branch lasts `MINUTES` (5 to 1,440). |
//!
//! The pattern is the pattern language of ADR-012, with one difference: the name `.stemmaignore`
//! is allowed, since the policy gives it level 2. `lease` stands at most once, `approvals` once
//! for each level, and `operation` once for each name.
//!
//! # Rules
//!
//! - A path that no `level` line names has level 2 ([`DEFAULT_LEVEL`]). A path that several lines
//!   name has the highest of their levels, so no line can lower what another one raises.
//! - The built-in floor: the policy file and the files of the actor register always have level 3
//!   ([`REGISTER_DIR`]). The evaluation applies the floor whatever the lines say. The reader also
//!   refuses a `level` line below 3 whose pattern lowers it by its look, that is, one that matches
//!   the policy file, the register directory, or an anchored path below that directory (a
//!   pattern with a leading `**` is refused as well). A name at any depth is checked against the
//!   directory name only; whatever slips through is raised to 3 by the evaluation.
//! - A policy may raise the requirements of a level, never lower them. The agent approvals of a
//!   level and the levels of the operations in [`OPERATION_FLOORS`] are a floor of the core:
//!   a line below it makes the policy invalid, and so does an operation below [`DEFAULT_LEVEL`].
//!   In particular `purge` has level 3, and only a human may purge (ADR-014); no line changes
//!   that.
//! - A pattern matches a directory as well as a file, as in ADR-012, and a match on a directory
//!   covers everything below it: `*.md` gives level 0 also to `x.md/build.rs`. A line for
//!   directories ends with `/`, and a file name that a wildcard may also fit needs care.
//! - The time of an evaluation is bounded: at most [`MAX_LEVEL_LINES`] `level` lines, and a path of
//!   at most [`MAX_PATH_PARTS`] parts, and a backslash in a part is an error as well.
//! - Names are compared byte by byte, as in ADR-012: no Unicode normalization (open item of
//!   ADR-012), so a rule for `secrets/` does not cover `Secrets/` on a case-insensitive file
//!   system. Only the names of the floor ignore the case, the trailing dots and spaces, and a
//!   `:stream`.
//! - The lease has bounds that the code fixes ([`MIN_LEASE_MINUTES`], [`MAX_LEASE_MINUTES`]); a
//!   policy may change the lease only inside them.
//!
//! The register has no layout yet (STEMMA-221, STEMMA-222), so [`REGISTER_DIR`] is the directory
//! in which its files will reside. The slice that defines the layout adjusts it.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::time::Duration;

use crate::ignore::{parse_pattern, scan_lines, FileProblem, LineProblem, Pattern, ScanError};
use crate::object::{name_problem, NameProblem};
use crate::oplog::{OperationName, OperationNameError};

/// The name of the policy file, in the root of the repository.
pub const FILE_NAME: &str = ".stemma-policy";

/// The directory of the actor register. Its files have level 3 (ADR-003, ADR-004). Provisional
/// until the register has a layout (STEMMA-221).
pub const REGISTER_DIR: &str = ".stemma-actors";

/// The header of the first line.
const HEADER: &str = "stemma-policy";

/// The version that this reader knows.
const VERSION: &str = "1";

/// The level of a path that no line names.
pub const DEFAULT_LEVEL: RiskLevel = RiskLevel::Two;

/// The shortest lease that a policy may set, in minutes (ADR-004).
pub const MIN_LEASE_MINUTES: u32 = 5;

/// The longest lease that a policy may set, in minutes: 24 hours (ADR-004).
pub const MAX_LEASE_MINUTES: u32 = 24 * 60;

/// The lease of a branch if the policy sets none, in minutes (ADR-004).
pub const DEFAULT_LEASE_MINUTES: u32 = 60;

/// The most `level` lines that a policy may have, and the most parts that a path may have in
/// [`Policy::level_of_path`]. Together with the limits of a line they bound the time of one
/// evaluation (rules times parts of the pattern times parts of the path), so that a path chosen
/// by the author of a change cannot make the check slow, and a path that is too deep is an error,
/// not a lower level.
pub const MAX_LEVEL_LINES: usize = 1024;

/// See [`MAX_LEVEL_LINES`].
pub const MAX_PATH_PARTS: usize = 128;

/// The most agent approvals that a policy may ask for.
pub const MAX_AGENT_APPROVALS: u32 = 8;

/// The operations that have a level of their own and the lowest level that a policy may give them
/// (ADR-004): `purge` removes history and stays human only (ADR-014), `take-over` is the option
/// `--take-over` of level 3, and `checkout`, `commit`, `restore`, `snapshot`, `tag`, and `untag`
/// have level 2.
pub const OPERATION_FLOORS: [(&str, RiskLevel); 8] = [
    ("purge", RiskLevel::Three),
    (crate::oplog::TAKE_OVER, RiskLevel::Three),
    ("checkout", RiskLevel::Two),
    ("commit", RiskLevel::Two),
    ("restore", RiskLevel::Two),
    ("snapshot", RiskLevel::Two),
    ("tag", RiskLevel::Two),
    ("untag", RiskLevel::Two),
];

/// The agent approvals that each level needs at least, for the levels 0 to 3 (ADR-004). A human
/// owner or maintainer is asked for in addition at the levels 2 and 3; the policy cannot change
/// that, and level 3 needs no agent.
const AGENT_APPROVAL_FLOORS: [u32; 4] = [1, 2, 2, 0];

/// The impact of a change (ADR-004), from 0 to 3. The levels are ordered, so the level of a
/// change is the maximum of the levels of its paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RiskLevel {
    /// Docs, examples, formatting, tests only.
    Zero = 0,
    /// New CLI commands, output formats, logic without persistence.
    One = 1,
    /// Object format, hash, ref updates, lease, merge, network, migrations, new dependencies.
    Two = 2,
    /// RBAC, signatures and keys, namespace separation, CI and release, policy files, the actor
    /// register.
    Three = 3,
}

// The tables with one cell for each level are indexed by the number of the level.
const _: () = assert!(RiskLevel::Three as usize + 1 == AGENT_APPROVAL_FLOORS.len());

impl RiskLevel {
    /// The level with this number, or `None` above 3.
    pub fn from_number(number: u32) -> Option<Self> {
        match number {
            0 => Some(Self::Zero),
            1 => Some(Self::One),
            2 => Some(Self::Two),
            3 => Some(Self::Three),
            _ => None,
        }
    }

    /// The number of the level, from 0 to 3.
    pub fn number(self) -> u32 {
        self as u32
    }

    /// The place of the level in a table with one cell for each level.
    fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.number())
    }
}

/// What a path in a tree is. A link is a file here: it is never followed (ADR-012).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A file or a link.
    File,
    /// A directory.
    Directory,
}

/// A change to one path, as a diff of two trees lists it. The paths are relative to the root,
/// written with `/`.
///
/// The list of changes names every file that differs, also all files of a directory that is added,
/// removed, or moved as a whole: the level of a directory does not cover what is below it (a
/// pattern for a file below it may name a higher level), so the caller lists the files. A
/// directory appears only in [`PathChange::TypeChanged`], for the side of the path that is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathChange<'a> {
    /// The file is new.
    Added { path: &'a str },
    /// The content of the file changed.
    Modified { path: &'a str },
    /// The file is gone.
    Deleted { path: &'a str },
    /// The file moved: the change touches the old and the new path.
    Renamed { from: &'a str, to: &'a str },
    /// The path stays and its type changes (a file becomes a directory, or the other way round):
    /// the change touches both sides.
    TypeChanged {
        path: &'a str,
        before: EntryKind,
        after: EntryKind,
    },
}

impl<'a> PathChange<'a> {
    /// Every path that the change touches, with the type that it has at that place.
    fn touched(&self) -> Vec<(&'a str, EntryKind)> {
        match *self {
            Self::Added { path } | Self::Modified { path } | Self::Deleted { path } => {
                vec![(path, EntryKind::File)]
            }
            Self::Renamed { from, to } => vec![(from, EntryKind::File), (to, EntryKind::File)],
            Self::TypeChanged {
                path,
                before,
                after,
            } => vec![(path, before), (path, after)],
        }
    }
}

/// A policy: the rules of a repository for the level of a change, the approvals, the operations,
/// and the lease.
///
/// Two policies are equal if they have the same lines (the settings and the `level` lines in the
/// same order); comments, empty lines, and the line numbers do not count, so [`Policy::encode`]
/// followed by [`Policy::parse`] gives an equal policy. A line that repeats the floor of the core
/// (`lease 60`, `approvals 1 1`) evaluates like no line, but is another policy.
/// [`Policy::default`] is the policy of a repository without a policy file, and for nothing else:
/// a file that cannot be read is an error, never the default policy. It equals the policy of a
/// file that has only the header: every path has level 2, except the floor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    paths: Vec<PathRule>,
    operations: BTreeMap<String, RiskLevel>,
    approvals: [Option<u32>; 4],
    lease_minutes: Option<u32>,
}

/// A `level` line. Equal if the level and the pattern text are, whatever the line it stood on.
#[derive(Debug, Clone)]
struct PathRule {
    level: RiskLevel,
    /// The pattern as written, for [`Policy::encode`].
    text: String,
    pattern: Pattern,
}

impl PartialEq for PathRule {
    fn eq(&self, other: &Self) -> bool {
        (self.level, &self.text) == (other.level, &other.text)
    }
}

impl Eq for PathRule {}

impl Policy {
    /// Reads and checks the bytes of a policy file.
    ///
    /// The lines are checked in order, so the error names the first line that the reader refuses.
    /// A problem of the whole file (its size, a byte order mark, bytes that are not UTF-8, or no
    /// line at all but comments and empty lines) has no line.
    ///
    /// # Errors
    ///
    /// Returns a [`PolicyParseError`] for the first problem, among others: an unknown version or
    /// line, a second header or setting, a line that is malformed, a `level` line that lowers the
    /// floor, or a line that sets a value below the floor of the core or outside its bounds. See
    /// [`PolicyProblem`].
    pub fn parse(bytes: &[u8]) -> Result<Self, PolicyParseError> {
        let mut policy = Self::default();
        // The number of the line of each setting, to refuse a second one.
        let mut first_of: BTreeMap<String, NonZeroUsize> = BTreeMap::new();
        let mut header = false;
        scan_lines(bytes, |at, line| {
            if !header {
                return match line.split_once(' ') {
                    Some((HEADER, VERSION)) => {
                        header = true;
                        Ok(())
                    }
                    Some((HEADER, version)) => {
                        Err(PolicyProblem::UnknownVersion(version.to_owned()))
                    }
                    _ => Err(PolicyProblem::MissingHeader),
                };
            }
            policy.read_line(line, at, &mut first_of)
        })
        .map_err(|error| match error {
            ScanError::File(problem) => PolicyParseError::File(problem),
            ScanError::Line { line, problem } => PolicyParseError::Line { line, problem },
        })?;
        if !header {
            return Err(PolicyParseError::NoHeader);
        }
        Ok(policy)
    }

    /// Reads one line after the header and adds it to the policy.
    fn read_line(
        &mut self,
        line: &str,
        at: NonZeroUsize,
        first_of: &mut BTreeMap<String, NonZeroUsize>,
    ) -> Result<(), PolicyProblem> {
        let keyword = line.split(' ').next().unwrap_or(line);
        let mut once = |key: String| match first_of.insert(key.clone(), at) {
            Some(first) => Err(PolicyProblem::Twice { key, first }),
            None => Ok(()),
        };
        let fields: Vec<&str> = line.split(' ').collect();
        match keyword {
            "level" => {
                let (Some(level), Some(text)) = (
                    fields.get(1),
                    line.splitn(3, ' ').nth(2).filter(|t| !t.starts_with(' ')),
                ) else {
                    return Err(PolicyProblem::Usage("level N PATTERN"));
                };
                let level = read_level(level)?;
                let pattern = parse_pattern(text, at, true).map_err(|problem| match problem {
                    LineProblem::Everything => PolicyProblem::MatchesEverything,
                    other => PolicyProblem::Pattern(other),
                })?;
                if level < RiskLevel::Three {
                    if let Some(floor) = lowered_floor(&pattern) {
                        return Err(PolicyProblem::LowersFloor { level, floor });
                    }
                }
                if self.paths.len() >= MAX_LEVEL_LINES {
                    return Err(PolicyProblem::TooManyLevelLines);
                }
                self.paths.push(PathRule {
                    level,
                    text: text.to_owned(),
                    pattern,
                });
            }
            "operation" => {
                let [_, name, level] = fields[..] else {
                    return Err(PolicyProblem::Usage("operation NAME N"));
                };
                let operation = OperationName::try_new(name).map_err(PolicyProblem::Operation)?;
                let level = read_level(level)?;
                // An operation without a floor of its own is never below the default level.
                let floor = OPERATION_FLOORS
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map_or(DEFAULT_LEVEL, |(_, floor)| *floor);
                if level < floor {
                    return Err(PolicyProblem::OperationBelowFloor {
                        operation: name.to_owned(),
                        floor,
                    });
                }
                once(format!("operation {operation}"))?;
                self.operations.insert(name.to_owned(), level);
            }
            "approvals" => {
                let [_, level, count] = fields[..] else {
                    return Err(PolicyProblem::Usage("approvals N COUNT"));
                };
                let level = read_level(level)?;
                let count = read_number(count).ok_or(PolicyProblem::BadNumber(count.to_owned()))?;
                let floor = AGENT_APPROVAL_FLOORS[level.index()];
                if count < floor || count > MAX_AGENT_APPROVALS {
                    return Err(PolicyProblem::ApprovalsOutOfBounds {
                        level,
                        count,
                        min: floor,
                        max: MAX_AGENT_APPROVALS,
                    });
                }
                once(format!("approvals {level}"))?;
                self.approvals[level.index()] = Some(count);
            }
            "lease" => {
                let [_, minutes] = fields[..] else {
                    return Err(PolicyProblem::Usage("lease MINUTES"));
                };
                let minutes =
                    read_number(minutes).ok_or(PolicyProblem::BadNumber(minutes.to_owned()))?;
                if !(MIN_LEASE_MINUTES..=MAX_LEASE_MINUTES).contains(&minutes) {
                    return Err(PolicyProblem::LeaseOutOfBounds { minutes });
                }
                once("lease".to_owned())?;
                self.lease_minutes = Some(minutes);
            }
            HEADER => return Err(PolicyProblem::SecondHeader),
            other => return Err(PolicyProblem::UnknownLine(other.to_owned())),
        }
        Ok(())
    }

    /// The policy as a file in its one canonical form, with `\n` line ends: the header, the
    /// lease, the approvals by level, the operations by name, and the `level` lines in their
    /// order. Reading the result gives a policy that evaluates like this one; comments and empty
    /// lines are not kept.
    pub fn encode(&self) -> String {
        let mut text = format!("{HEADER} {VERSION}\n");
        if let Some(minutes) = self.lease_minutes {
            text.push_str(&format!("lease {minutes}\n"));
        }
        for (level, count) in self.approvals.iter().enumerate() {
            if let Some(count) = count {
                text.push_str(&format!("approvals {level} {count}\n"));
            }
        }
        for (name, level) in &self.operations {
            text.push_str(&format!("operation {name} {level}\n"));
        }
        for rule in &self.paths {
            text.push_str(&format!("level {} {}\n", rule.level, rule.text));
        }
        text
    }

    /// The level of a path: the highest level of the lines that name it (a line that names a
    /// directory above the path names it as well), or [`DEFAULT_LEVEL`] if none does; never below
    /// the floor. `kind` is what the path is; a pattern with a trailing `/` matches only a
    /// directory.
    ///
    /// # Errors
    ///
    /// Returns a [`PathError`] if the path is not a relative path of non-empty names written with
    /// `/`, is too deep, or has a name that a tree refuses.
    pub fn level_of_path(&self, path: &str, kind: EntryKind) -> Result<RiskLevel, PathError> {
        check_path(path)?;
        let is_directory = kind == EntryKind::Directory;
        let named = self
            .paths
            .iter()
            .filter(|rule| rule.pattern.matches_path(path, is_directory))
            .map(|rule| rule.level)
            .max();
        let level = named.unwrap_or(DEFAULT_LEVEL);
        Ok(if is_floor_path(path) {
            level.max(RiskLevel::Three)
        } else {
            level
        })
    }

    /// The level of a change: the highest level of all paths that it touches. A rename touches
    /// the old and the new path, a deletion the path, and a change of type both sides. A change
    /// that touches nothing has level 0.
    ///
    /// # Errors
    ///
    /// Returns a [`PathError`] for the first path that is not valid.
    pub fn level_of(&self, changes: &[PathChange<'_>]) -> Result<RiskLevel, PathError> {
        let mut level = RiskLevel::Zero;
        for change in changes {
            for (path, kind) in change.touched() {
                level = level.max(self.level_of_path(path, kind)?);
            }
        }
        Ok(level)
    }

    /// The level of an operation that touches no path: the line of the policy, or the floor of the
    /// core, or [`DEFAULT_LEVEL`].
    pub fn operation_level(&self, operation: &OperationName) -> RiskLevel {
        let floor = OPERATION_FLOORS
            .iter()
            .find(|(name, _)| *name == operation.to_string())
            .map(|(_, level)| *level);
        match (self.operations.get(&operation.to_string()), floor) {
            (Some(level), _) => *level,
            (None, Some(floor)) => floor,
            (None, None) => DEFAULT_LEVEL,
        }
    }

    /// The agent approvals that a change of this level needs: the line of the policy, or the
    /// minimum of the core (ADR-004). A human owner or maintainer is asked for in addition at the
    /// levels 2 and 3.
    pub fn agent_approvals(&self, level: RiskLevel) -> u32 {
        let index = level.index();
        self.approvals[index].unwrap_or(AGENT_APPROVAL_FLOORS[index])
    }

    /// The lease of a branch: the line of the policy, or [`DEFAULT_LEASE_MINUTES`].
    pub fn lease(&self) -> Duration {
        Duration::from_secs(u64::from(self.lease_minutes.unwrap_or(DEFAULT_LEASE_MINUTES)) * 60)
    }
}

/// The paths that have level 3 whatever a policy says: the policy file, and the actor register.
///
/// The comparison ignores the case of letters (Unicode case folding), the trailing dots and
/// spaces, and a stream (`:name`) of the name: a case-insensitive or Windows file system opens
/// `.Stemma-Policy` and `.stemma-policy.` as the policy file, so the spelling must not lower the
/// floor (as for `.stemma` in the names of a tree, and the exclusion file in ADR-012). Short
/// names (`STEMMA~1`) are refused by the names of a tree already.
fn is_floor_path(path: &str) -> bool {
    // Unicode case folding, as `is_reserved_name` of the trees does: a file system may fold
    // letters like the long s (U+017F) to ASCII.
    let is_name = |name: &str, floor: &str| {
        let name = name.split(':').next().unwrap_or(name);
        name.trim_end_matches(['.', ' ']).to_uppercase() == floor.to_uppercase()
    };
    let top = path.split('/').next().unwrap_or(path);
    is_name(top, REGISTER_DIR) || (path == top && is_name(path, FILE_NAME))
}

/// A floor path that the pattern matches, if any: a line that gives it a level below 3 lowers it.
/// The register has no layout yet, so the pattern is refused if it matches the register directory
/// or may match something below it (an anchored pattern that goes on below the directory). A name
/// at any depth is checked against the directory name only.
fn lowered_floor(pattern: &Pattern) -> Option<&'static str> {
    if pattern.matches_path(FILE_NAME, false) {
        Some(FILE_NAME)
    } else if pattern.matches_path(REGISTER_DIR, true) || pattern.matches_below(REGISTER_DIR) {
        Some(REGISTER_DIR)
    } else {
        None
    }
}

/// A number written with digits only, without a sign and without a leading zero, of at most nine
/// digits: one spelling for each value.
fn read_number(text: &str) -> Option<u32> {
    let plain = !text.is_empty()
        && text.len() <= 9
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if plain {
        text.parse().ok()
    } else {
        None
    }
}

fn read_level(text: &str) -> Result<RiskLevel, PolicyProblem> {
    read_number(text)
        .and_then(RiskLevel::from_number)
        .ok_or_else(|| PolicyProblem::BadLevel(text.to_owned()))
}

/// Checks that a path is relative, written with `/`, and has only names that a tree accepts.
fn check_path(path: &str) -> Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    let parts = path.split('/').count();
    if parts > MAX_PATH_PARTS {
        return Err(PathError::TooDeep { parts });
    }
    for part in path.split('/') {
        if part.contains('\\') {
            return Err(PathError::Backslash);
        }
        if let Some(problem) = name_problem(part) {
            return Err(PathError::BadPart {
                part: part.to_owned(),
                problem,
            });
        }
    }
    Ok(())
}

/// Why a path cannot be evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathError {
    /// The path is empty.
    Empty,
    /// The path has more than [`MAX_PATH_PARTS`] parts.
    TooDeep {
        /// The number of parts.
        parts: usize,
    },
    /// A part has a backslash, which a pattern cannot name (a Windows path is written with `/`).
    Backslash,
    /// A part of the path is empty (`a//b`, a leading or trailing `/`) or a name that a tree
    /// refuses.
    BadPart {
        /// The part as given.
        part: String,
        /// Why a tree refuses it.
        problem: NameProblem,
    },
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the path is empty"),
            Self::TooDeep { parts } => {
                write!(f, "the path has {parts} parts; at most {MAX_PATH_PARTS}")
            }
            Self::Backslash => f.write_str("a backslash in a path; write it with `/`"),
            Self::BadPart { part, problem } => write!(
                f,
                "`{}` is not a valid name in a path: {problem}",
                part.escape_debug()
            ),
        }
    }
}

impl Error for PathError {}

/// Why the reader refuses a policy file: a problem of the whole file, or the first line that it
/// refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PolicyParseError {
    /// The file as a whole is wrong; there is no line.
    File(FileProblem),
    /// The file has no header line.
    NoHeader,
    /// A line is wrong.
    Line {
        /// The number of the line, counted from 1.
        line: NonZeroUsize,
        /// What is wrong with it.
        problem: PolicyProblem,
    },
}

impl PolicyParseError {
    /// The line of the problem, counted from 1, or `None` for a problem of the whole file.
    pub fn line(&self) -> Option<NonZeroUsize> {
        match self {
            Self::File(_) | Self::NoHeader => None,
            Self::Line { line, .. } => Some(*line),
        }
    }
}

/// What is wrong with a line of a policy file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PolicyProblem {
    /// A problem of the line itself: its end, length, characters, spaces, or a repetition.
    Line(LineProblem),
    /// The first line that is neither empty nor a comment is not the header `stemma-policy 1`.
    MissingHeader,
    /// The header names a version that this reader does not know.
    UnknownVersion(String),
    /// A second header line.
    SecondHeader,
    /// The first word is not one of `level`, `operation`, `approvals`, and `lease`.
    UnknownLine(String),
    /// The line does not have the fields of its form. Carries the form.
    Usage(&'static str),
    /// A level that is not a number from 0 to 3.
    BadLevel(String),
    /// A number that is not written with digits only, without a leading zero, or has more than
    /// nine digits.
    BadNumber(String),
    /// The pattern is refused, as the reader of the exclusion file refuses it.
    Pattern(LineProblem),
    /// The pattern has only `**` parts, so it matches every path: write the level of a path that
    /// no line names with the lines for the paths that differ.
    MatchesEverything,
    /// A `level` line below 3 for a pattern that matches the policy file or the register (the
    /// directory, or a path below it that an anchored pattern may match), which have level 3
    /// always. Carries the level and the
    /// path that it matches.
    LowersFloor {
        /// The level of the line.
        level: RiskLevel,
        /// The path of the floor that the pattern matches.
        floor: &'static str,
    },
    /// The name of an operation is not valid.
    Operation(OperationNameError),
    /// A level of an operation below the floor of the core.
    OperationBelowFloor {
        /// The operation.
        operation: String,
        /// The lowest level that it may have.
        floor: RiskLevel,
    },
    /// A count of approvals below the minimum of the core, or above the largest count.
    ApprovalsOutOfBounds {
        /// The level.
        level: RiskLevel,
        /// The count as written.
        count: u32,
        /// The fewest approvals that the level needs.
        min: u32,
        /// The most that a policy may ask for.
        max: u32,
    },
    /// A lease outside of the bounds.
    LeaseOutOfBounds {
        /// The minutes as written.
        minutes: u32,
    },
    /// More than [`MAX_LEVEL_LINES`] `level` lines.
    TooManyLevelLines,
    /// A setting that stands a second time.
    Twice {
        /// The setting.
        key: String,
        /// The line of the first one.
        first: NonZeroUsize,
    },
}

impl From<LineProblem> for PolicyProblem {
    fn from(problem: LineProblem) -> Self {
        Self::Line(problem)
    }
}

impl fmt::Display for PolicyProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Line(problem) | Self::Pattern(problem) => problem.fmt(f),
            Self::MissingHeader => write!(f, "the first line has to be `{HEADER} {VERSION}`"),
            Self::UnknownVersion(version) => write!(
                f,
                "unknown version `{}`; this reader knows `{VERSION}`",
                version.escape_debug()
            ),
            Self::SecondHeader => f.write_str("the header stands once, in the first line"),
            Self::UnknownLine(word) => write!(
                f,
                "unknown line `{}`; expected `level`, `operation`, `approvals`, or `lease`",
                word.escape_debug()
            ),
            Self::Usage(form) => write!(f, "expected `{form}`"),
            Self::BadLevel(text) => write!(
                f,
                "`{}` is not a level; a level is 0, 1, 2, or 3",
                text.escape_debug()
            ),
            Self::BadNumber(text) => write!(
                f,
                "`{}` is not a number; write at most nine digits, without a leading zero",
                text.escape_debug()
            ),
            Self::MatchesEverything => {
                f.write_str("the pattern would match every path; name the paths that differ")
            }
            Self::LowersFloor { level, floor } => write!(
                f,
                "level {level} for a pattern that matches `{floor}`, which is always level 3"
            ),
            Self::Operation(problem) => problem.fmt(f),
            Self::OperationBelowFloor { operation, floor } => write!(
                f,
                "the operation `{operation}` cannot have a level below {floor}"
            ),
            Self::ApprovalsOutOfBounds {
                level,
                count,
                min,
                max,
            } => write!(
                f,
                "{count} approvals for level {level}; a policy may ask for {min} to {max}"
            ),
            Self::LeaseOutOfBounds { minutes } => write!(
                f,
                "a lease of {minutes} minutes; the lease is {MIN_LEASE_MINUTES} to \
                 {MAX_LEASE_MINUTES} minutes"
            ),
            Self::TooManyLevelLines => {
                write!(f, "more than {MAX_LEVEL_LINES} `level` lines")
            }
            Self::Twice { key, first } => write!(f, "`{key}` is set again; first in line {first}"),
        }
    }
}

impl fmt::Display for PolicyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(problem) => write!(f, "the file {problem}"),
            Self::NoHeader => write!(f, "the file has no header `{HEADER} {VERSION}`"),
            Self::Line { line, problem } => write!(f, "line {line}: {problem}"),
        }
    }
}

impl Error for PolicyParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Policy, PolicyParseError> {
        Policy::parse(text.as_bytes())
    }

    fn policy(text: &str) -> Policy {
        parse(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
    }

    fn message(text: &str) -> String {
        parse(text).expect_err(text).to_string()
    }

    fn file(path: &str) -> PathChange<'_> {
        PathChange::Modified { path }
    }

    /// The policy that ADR-004 describes: the table of paths as lines.
    const ADR_TABLE: &str = "stemma-policy 1\n\
        # level 3\n\
        level 3 .github/workflows/\n\
        level 3 justfile\n\
        level 3 deny.toml\n\
        level 3 crates/stemma-core/src/actor.rs\n\
        level 2 .stemmaignore\n\
        level 1 crates/stemma-cli/\n\
        level 1 crates/stemma-core/src/ignore.rs\n\
        level 0 docs/\n\
        level 0 adr/\n\
        level 0 *.md\n";

    fn level(policy: &Policy, path: &str) -> u32 {
        policy
            .level_of_path(path, EntryKind::File)
            .unwrap()
            .number()
    }

    #[test]
    fn a_file_with_only_the_header_is_the_default_policy() {
        assert_eq!(policy("stemma-policy 1\n"), Policy::default());
        assert_eq!(policy("stemma-policy 1\r\n"), Policy::default());
        assert_eq!(
            policy("# policy\n\nstemma-policy 1\n\n# nothing else\n"),
            Policy::default()
        );
    }

    #[test]
    fn the_lines_are_read() {
        let p = policy(
            "stemma-policy 1\nlease 30\napprovals 0 2\napprovals 3 1\noperation purge 3\n\
             operation tag 3\nlevel 1 src/\n",
        );
        assert_eq!(p.lease(), Duration::from_secs(30 * 60));
        assert_eq!(p.agent_approvals(RiskLevel::Zero), 2);
        assert_eq!(p.agent_approvals(RiskLevel::One), 2);
        assert_eq!(p.agent_approvals(RiskLevel::Three), 1);
        let op = |name: &str| OperationName::try_new(name).unwrap();
        assert_eq!(p.operation_level(&op("tag")), RiskLevel::Three);
        assert_eq!(p.operation_level(&op("restore")), RiskLevel::Two);
        assert_eq!(p.operation_level(&op("purge")), RiskLevel::Three);
        assert_eq!(p.operation_level(&op("diff")), DEFAULT_LEVEL);
        assert_eq!(level(&p, "src/main.rs"), 1);
    }

    #[test]
    fn the_defaults_of_the_core_hold_without_a_line() {
        let p = Policy::default();
        assert_eq!(p.lease(), Duration::from_secs(3600));
        let approvals: Vec<u32> = (0..4)
            .map(|n| p.agent_approvals(RiskLevel::from_number(n).unwrap()))
            .collect();
        assert_eq!(approvals, [1, 2, 2, 0]);
        let purge = OperationName::try_new("purge").unwrap();
        assert_eq!(p.operation_level(&purge), RiskLevel::Three);
    }

    #[test]
    fn a_path_that_no_line_names_has_level_2() {
        let p = policy(ADR_TABLE);
        assert_eq!(level(&p, "crates/stemma-core/src/object.rs"), 2);
        assert_eq!(level(&p, "a-new-directory/file"), 2);
        assert_eq!(level(&Policy::default(), "docs/x.tex"), 2);
    }

    #[test]
    fn the_table_of_adr_004_gives_its_levels() {
        let p = policy(ADR_TABLE);
        assert_eq!(level(&p, ".github/workflows/ci.yml"), 3);
        assert_eq!(level(&p, "justfile"), 3);
        assert_eq!(level(&p, "crates/stemma-core/src/actor.rs"), 3);
        assert_eq!(level(&p, ".stemmaignore"), 2);
        assert_eq!(level(&p, "crates/stemma-cli/src/cli.rs"), 1);
        assert_eq!(level(&p, "crates/stemma-core/src/ignore.rs"), 1);
        assert_eq!(level(&p, "docs/chapters/07.tex"), 0);
        assert_eq!(level(&p, "README.md"), 0);
        assert_eq!(level(&p, "docs/notes/a.md"), 0);
    }

    #[test]
    fn a_path_that_several_lines_name_has_the_highest_level() {
        let p = policy(
            "stemma-policy 1\nlevel 0 docs/\nlevel 2 docs/adr/\nlevel 3 docs/adr/secret.md\n\
             level 1 docs/adr/\n",
        );
        assert_eq!(level(&p, "docs/a.tex"), 0);
        assert_eq!(level(&p, "docs/adr/a.md"), 2);
        assert_eq!(level(&p, "docs/adr/secret.md"), 3);
        // The order of the lines does not matter.
        let q = policy("stemma-policy 1\nlevel 2 docs/adr/\nlevel 0 docs/\n");
        assert_eq!(level(&q, "docs/adr/a.md"), 2);
    }

    #[test]
    fn a_pattern_with_a_trailing_slash_matches_only_a_directory() {
        let p = policy("stemma-policy 1\nlevel 3 vendor/\nlevel 0 docs\n");
        let kind = |path, kind| p.level_of_path(path, kind).unwrap().number();
        // A file that is named `vendor` is not the directory.
        assert_eq!(kind("vendor", EntryKind::File), 2);
        assert_eq!(kind("vendor", EntryKind::Directory), 3);
        assert_eq!(kind("vendor/a", EntryKind::File), 3);
        assert_eq!(kind("docs", EntryKind::File), 0);
    }

    #[test]
    fn the_level_of_a_change_is_the_highest_level_of_its_paths() {
        let p = policy(ADR_TABLE);
        let level = |changes: &[PathChange<'_>]| p.level_of(changes).unwrap().number();
        assert_eq!(level(&[]), 0);
        assert_eq!(level(&[file("README.md")]), 0);
        assert_eq!(
            level(&[file("README.md"), file("crates/stemma-cli/a.rs")]),
            1
        );
        assert_eq!(
            level(&[
                file("README.md"),
                file("crates/stemma-cli/a.rs"),
                file(".github/workflows/ci.yml"),
                file("crates/stemma-core/src/object.rs"),
            ]),
            3
        );
    }

    #[test]
    fn a_rename_touches_the_old_and_the_new_path() {
        let p = policy(ADR_TABLE);
        let rename = |from, to| PathChange::Renamed { from, to };
        // Out of level 3 into docs: the old path counts.
        assert_eq!(level_of(&p, rename("justfile", "docs/justfile")), 3);
        // Into level 3 out of docs: the new path counts.
        assert_eq!(level_of(&p, rename("docs/justfile", "justfile")), 3);
        assert_eq!(level_of(&p, rename("docs/a.tex", "docs/b.tex")), 0);
    }

    fn level_of(p: &Policy, change: PathChange<'_>) -> u32 {
        p.level_of(&[change]).unwrap().number()
    }

    #[test]
    fn a_deletion_touches_the_path() {
        let p = policy(ADR_TABLE);
        let deleted = |path| PathChange::Deleted { path };
        assert_eq!(level_of(&p, deleted("justfile")), 3);
        assert_eq!(level_of(&p, deleted("docs/a.tex")), 0);
        assert_eq!(level_of(&p, deleted("unnamed/file")), 2);
    }

    #[test]
    fn a_change_of_type_touches_both_sides() {
        let p = policy("stemma-policy 1\nlevel 3 data/\nlevel 0 data\n");
        let change = |before, after| PathChange::TypeChanged {
            path: "data",
            before,
            after,
        };
        // The directory side is level 3 (`data/`), the file side level 0 (`data`).
        assert_eq!(
            level_of(&p, change(EntryKind::File, EntryKind::Directory)),
            3
        );
        assert_eq!(
            level_of(&p, change(EntryKind::Directory, EntryKind::File)),
            3
        );
        // Only the file side is named: a change of type reaches the level of the other side too.
        let q = policy("stemma-policy 1\nlevel 3 data/\n");
        assert_eq!(
            q.level_of_path("data", EntryKind::File).unwrap(),
            RiskLevel::Two
        );
        assert_eq!(
            level_of(&q, change(EntryKind::File, EntryKind::Directory)),
            3
        );
    }

    #[test]
    fn the_policy_file_and_the_register_have_level_3_whatever_the_lines_say() {
        for text in [
            "stemma-policy 1\n",
            "stemma-policy 1\nlevel 0 docs/\nlevel 1 crates/\n",
        ] {
            let p = policy(text);
            assert_eq!(level(&p, ".stemma-policy"), 3);
            assert_eq!(level(&p, ".stemma-actors/keys/a.pub"), 3);
            assert_eq!(
                p.level_of_path(".stemma-actors", EntryKind::Directory)
                    .unwrap(),
                RiskLevel::Three
            );
            // Another name, or the name below the root, is no floor path.
            assert_eq!(level(&p, ".stemma-policy.bak"), level(&p, "other"));
            assert_eq!(level(&p, "sub/.stemma-policy"), level(&p, "other"));
        }
    }

    #[test]
    fn a_line_that_the_reader_does_not_catch_still_leaves_the_floor_at_level_3() {
        // A pattern for a register file that the probes miss, or a directory pattern for the
        // policy file: only the evaluation holds the floor.
        let p = policy("stemma-policy 1\nlevel 0 .stemma-policy/\nlevel 0 docs/\nlevel 0 *.pub\n");
        for path in [
            ".stemma-policy",
            ".stemma-actors/keys/a.pub",
            ".stemma-actors/x",
        ] {
            assert_eq!(level(&p, path), 3, "{path}");
        }
        assert_eq!(
            p.level_of_path(".stemma-policy", EntryKind::Directory)
                .unwrap(),
            RiskLevel::Three
        );
        // The spelling in another case is the same file on a file system that folds case.
        for path in [".Stemma-Policy", ".STEMMA-ACTORS/x", ".Stemma-Actors"] {
            assert_eq!(level(&p, path), 3, "{path}");
        }
    }

    #[test]
    fn the_names_of_the_floor_have_no_alias_on_a_windows_file_system() {
        let p = policy("stemma-policy 1\nlevel 0 docs/\n");
        for path in [
            ".stemma-policy.",
            ".stemma-policy ",
            ".stemma-policy:stream",
            ".STEMMA-POLICY",
            ".stemma-actors./x",
            ".stemma-actors:s/x",
        ] {
            assert_eq!(level(&p, path), 3, "{path}");
        }
        assert_eq!(level(&p, "docs/.stemma-policy"), 0);
    }

    #[test]
    fn the_floor_folds_unicode_case_like_a_tree_does() {
        // The long s (U+017F) folds to `s` on a file system that folds case.
        let p = policy("stemma-policy 1\nlevel 0 *.pub\n");
        for path in [".\u{17f}temma-policy", ".\u{17f}temma-actors/k.pub"] {
            assert_eq!(level(&p, path), 3, "{path}");
        }
    }

    #[test]
    fn an_operation_without_a_floor_is_never_below_the_default_level() {
        let error = message("stemma-policy 1\noperation sync 1\n");
        assert!(error.contains("sync"), "{error}");
        assert!(parse("stemma-policy 1\noperation sync 3\n").is_ok());
        assert!(parse("stemma-policy 1\noperation sync 2\n").is_ok());
    }

    #[test]
    fn a_double_star_matches_zero_or_more_parts() {
        let p = policy("stemma-policy 1\nlevel 3 a/**/b\nlevel 3 **/secret\nlevel 0 docs/**\n");
        for path in [
            "a/b",
            "a/x/b",
            "a/x/y/b",
            "secret",
            "p/secret",
            "p/q/secret",
        ] {
            assert_eq!(level(&p, path), 3, "{path}");
        }
        for path in ["x/a/b", "a", "a/c/d"] {
            assert_eq!(level(&p, path), 2, "{path}");
        }
        assert_eq!(level(&p, "docs/x/y"), 0);
    }

    #[test]
    fn a_match_on_a_directory_covers_everything_below_it() {
        let p = policy("stemma-policy 1\nlevel 0 *.md\nlevel 3 build.rs\nlevel 0 docs/\n");
        assert_eq!(level(&p, "x.md/build.rs"), 3);
        assert_eq!(level(&p, "x.md/other.rs"), 0);
        assert_eq!(level(&p, "docs/a"), 0);
        // A pattern with a trailing `/` matches only a directory.
        assert_eq!(level(&p, "docs"), 2);
        assert_eq!(
            p.level_of_path("docs", EntryKind::Directory),
            Ok(RiskLevel::Zero)
        );
    }

    #[test]
    fn an_anchored_pattern_matches_from_the_root_and_a_bare_one_at_any_depth() {
        let p = policy(
            "stemma-policy 1\nlevel 3 vendor/\nlevel 3 docs/adr/\nlevel 3 /top\nlevel 1 draft-?.md\n",
        );
        for path in ["a/vendor/x", "docs/adr/a", "top"] {
            assert_eq!(level(&p, path), 3, "{path}");
        }
        for path in ["x/docs/adr/a", "a/top", "draft-12.md"] {
            assert_eq!(level(&p, path), 2, "{path}");
        }
        assert_eq!(level(&p, "draft-1.md"), 1);
    }

    #[test]
    fn the_same_pattern_at_two_levels_gives_the_higher_one_in_either_order() {
        for text in [
            "stemma-policy 1\nlevel 0 docs/\nlevel 3 docs/\n",
            "stemma-policy 1\nlevel 3 docs/\nlevel 0 docs/\n",
        ] {
            let p = policy(text);
            assert_eq!(level(&p, "docs/a"), 3, "{text:?}");
            assert_eq!(policy(&p.encode()), p);
        }
    }

    #[test]
    fn the_line_of_an_error_is_the_physical_line() {
        let text = "# c\r\n\r\nstemma-policy 1\r\n# d\r\n\r\nlease 4\r\n";
        assert_eq!(parse(text).unwrap_err().line(), NonZeroUsize::new(6));
        let mut many = String::from("stemma-policy 1\n");
        for n in 0..=MAX_LEVEL_LINES {
            many.push_str(&format!("level 3 a{n}\n"));
        }
        assert_eq!(
            parse(&many).unwrap_err().line(),
            NonZeroUsize::new(MAX_LEVEL_LINES + 2)
        );
        // A problem of the whole file has no line.
        for bytes in [
            &b"# only\n"[..],
            b"\xef\xbb\xbfstemma-policy 1\n",
            b"stemma-policy 1\n\xff\n",
        ] {
            assert_eq!(Policy::parse(bytes).unwrap_err().line(), None, "{bytes:?}");
        }
    }

    #[test]
    fn an_invalid_path_is_an_error_even_after_level_3() {
        let p = Policy::default();
        assert!(p.level_of(&[file(".stemma-policy"), file("a//b")]).is_err());
    }

    #[test]
    fn a_path_that_is_too_deep_or_has_a_backslash_is_an_error() {
        let p = policy("stemma-policy 1\nlevel 1 a/**/b\n");
        let deep = |n: usize| vec!["a"; n].join("/");
        assert!(p
            .level_of_path(&deep(MAX_PATH_PARTS), EntryKind::File)
            .is_ok());
        assert_eq!(
            p.level_of_path(&deep(MAX_PATH_PARTS + 1), EntryKind::File),
            Err(PathError::TooDeep {
                parts: MAX_PATH_PARTS + 1
            })
        );
        assert_eq!(
            p.level_of_path(".github\\workflows", EntryKind::File),
            Err(PathError::Backslash)
        );
    }

    #[test]
    fn the_evaluation_of_a_deep_path_against_many_rules_stays_fast() {
        let mut text = String::from("stemma-policy 1\n");
        for n in 0..MAX_LEVEL_LINES {
            text.push_str(&format!("level 1 a/**/a/**/a/**/a/**/b{n}\n"));
        }
        let p = policy(&text);
        let path = vec!["a"; MAX_PATH_PARTS].join("/");
        let started = std::time::Instant::now();
        assert_eq!(level(&p, &path), 2);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        text.push_str("level 1 more\n");
        assert!(message(&text).contains("more than 1024 `level` lines"));
    }

    #[test]
    fn changes_of_the_floor_files_have_level_3() {
        let p = policy("stemma-policy 1\nlevel 0 docs/\n");
        for change in [
            PathChange::Deleted {
                path: ".stemma-policy",
            },
            PathChange::Added {
                path: ".stemma-policy",
            },
            PathChange::Modified {
                path: ".stemma-actors/k.pub",
            },
            PathChange::Renamed {
                from: ".stemma-actors/k.pub",
                to: "docs/k.pub",
            },
            PathChange::Renamed {
                from: "docs/x",
                to: ".stemma-policy",
            },
            PathChange::TypeChanged {
                path: ".stemma-actors",
                before: EntryKind::File,
                after: EntryKind::Directory,
            },
        ] {
            assert_eq!(level_of(&p, change), 3, "{change:?}");
        }
    }

    #[test]
    fn the_files_of_a_directory_are_listed_so_that_their_levels_count() {
        // A directory that is gone has the levels of its files: the caller lists them.
        let p = policy("stemma-policy 1\nlevel 0 docs/\nlevel 3 docs/release.toml\n");
        let gone = [
            PathChange::Deleted { path: "docs/a.tex" },
            PathChange::Deleted {
                path: "docs/release.toml",
            },
        ];
        assert_eq!(p.level_of(&gone).unwrap(), RiskLevel::Three);
        assert_eq!(p.level_of(&gone[..1]).unwrap(), RiskLevel::Zero);
    }

    #[test]
    fn a_line_that_lowers_the_floor_makes_the_policy_invalid() {
        for line in [
            "level 2 .stemma-policy",
            "level 0 /.stemma-policy",
            "level 1 .stemma-*",
            "level 0 .stemma-actors/",
            "level 0 .stemma-actors",
            "level 0 *",
            "level 1 **/*",
            "level 0 .stemma-?olicy",
            "level 2 *-policy",
            "level 1 .stemma-actors/**",
            "level 0 .stemma-actors/keys/",
            "level 0 /.stemma-actors/alice.toml",
            "level 1 .stemma-actors/*.toml",
            "level 1 **/*.toml",
        ] {
            let text = format!("stemma-policy 1\n{line}\n");
            let error = parse(&text).expect_err(&text);
            assert!(
                matches!(
                    error,
                    PolicyParseError::Line {
                        problem: PolicyProblem::LowersFloor { .. },
                        ..
                    }
                ),
                "{line}: {error}"
            );
            assert_eq!(error.line().map(NonZeroUsize::get), Some(2), "{line}");
        }
        // A name at any depth that is not the directory is no lowering of the register.
        policy("stemma-policy 1\nlevel 0 register-file\nlevel 1 docs/**/draft-?.md\n");
        // The same patterns at level 3 raise and are fine.
        for line in [
            "level 3 .stemma-policy",
            "level 3 *",
            "level 3 .stemma-actors/",
        ] {
            policy(&format!("stemma-policy 1\n{line}\n"));
        }
    }

    #[test]
    fn an_operation_name_is_checked_and_cannot_dodge_a_floor_by_its_spelling() {
        for name in ["Purge", "1x", "pur ge", "purge!"] {
            let error = message(&format!("stemma-policy 1\noperation {name} 0\n"));
            assert!(error.starts_with("line 2: "), "{name}: {error}");
        }
    }

    #[test]
    fn the_operations_of_adr_004_cannot_be_lowered() {
        for (name, floor) in OPERATION_FLOORS {
            let text = format!("stemma-policy 1\noperation {name} 0\n");
            let error = message(&text);
            assert!(
                error.contains(&format!("`{name}` cannot have a level below {floor}")),
                "{error}"
            );
            policy(&format!("stemma-policy 1\noperation {name} 3\n"));
        }
        let take_over = OperationName::try_new("take-over").unwrap();
        assert_eq!(
            Policy::default().operation_level(&take_over),
            RiskLevel::Three
        );
        assert!(message("stemma-policy 1\noperation take-over 2\n").contains("below 3"));
    }

    #[test]
    fn a_policy_may_raise_but_not_lower_the_requirements() {
        assert_eq!(
            message("stemma-policy 1\noperation purge 2\n"),
            "line 2: the operation `purge` cannot have a level below 3"
        );
        assert!(message("stemma-policy 1\noperation restore 1\n").contains("below 2"));
        assert!(message("stemma-policy 1\noperation untag 0\n").contains("below 2"));
        policy("stemma-policy 1\noperation restore 3\noperation diff 2\n");
        assert_eq!(
            message("stemma-policy 1\napprovals 1 1\n"),
            "line 2: 1 approvals for level 1; a policy may ask for 2 to 8"
        );
        assert!(message("stemma-policy 1\napprovals 0 0\n").contains("for level 0"));
        assert!(message("stemma-policy 1\napprovals 2 9\n").contains("2 to 8"));
        policy("stemma-policy 1\napprovals 0 8\napprovals 3 0\n");
    }

    #[test]
    fn the_lease_has_bounds() {
        for minutes in [MIN_LEASE_MINUTES, 60, MAX_LEASE_MINUTES] {
            let p = policy(&format!("stemma-policy 1\nlease {minutes}\n"));
            assert_eq!(p.lease(), Duration::from_secs(u64::from(minutes) * 60));
        }
        for minutes in ["0", "4", "1441", "999999999"] {
            assert_eq!(
                message(&format!("stemma-policy 1\nlease {minutes}\n")),
                format!("line 2: a lease of {minutes} minutes; the lease is 5 to 1440 minutes")
            );
        }
    }

    #[test]
    fn an_unknown_version_or_line_is_an_error() {
        assert_eq!(
            message("stemma-policy 2\n"),
            "line 1: unknown version `2`; this reader knows `1`"
        );
        assert!(message("stemma-policy 1.0\n").contains("unknown version"));
        assert!(message("stemma-policy\n").contains("first line has to be"));
        assert!(message("level 1 a\nstemma-policy 1\n").contains("first line has to be"));
        assert_eq!(message(""), "the file has no header `stemma-policy 1`");
        assert_eq!(
            message("# only\n\n"),
            "the file has no header `stemma-policy 1`"
        );
        assert_eq!(
            message("stemma-policy 1\nowner alice\n"),
            "line 2: unknown line `owner`; expected `level`, `operation`, `approvals`, or `lease`"
        );
        assert!(message("stemma-policy 1\nLevel 1 a\n").contains("unknown line `Level`"));
        assert_eq!(
            message("stemma-policy 1\nstemma-policy 2\n"),
            "line 2: the header stands once, in the first line"
        );
    }

    #[test]
    fn a_line_with_the_wrong_fields_is_an_error() {
        for (line, form) in [
            ("level", "level N PATTERN"),
            ("level 1", "level N PATTERN"),
            ("level 1  a", "level N PATTERN"),
            ("operation tag", "operation NAME N"),
            ("operation tag 2 3", "operation NAME N"),
            ("approvals 1", "approvals N COUNT"),
            ("approvals 1 2 3", "approvals N COUNT"),
            ("lease", "lease MINUTES"),
            ("lease 5 6", "lease MINUTES"),
        ] {
            assert_eq!(
                message(&format!("stemma-policy 1\n{line}\n")),
                format!("line 2: expected `{form}`"),
                "{line}"
            );
        }
    }

    #[test]
    fn numbers_and_levels_have_one_spelling() {
        for level in ["4", "-1", "+1", "01", "x", "", "1.0", "٣"] {
            let text = format!("stemma-policy 1\nlevel {level} a\n");
            let error = message(&text);
            assert!(
                error.contains("is not a level") || error.contains("expected `level N"),
                "{level:?}: {error}"
            );
        }
        for number in ["05", "+5", "5.0", "x", "0x5", "1000000000"] {
            let text = format!("stemma-policy 1\nlease {number}\n");
            assert!(message(&text).contains("is not a number"), "{number}");
        }
    }

    #[test]
    fn a_setting_stands_once() {
        assert_eq!(
            message("stemma-policy 1\nlease 10\n\nlease 20\n"),
            "line 4: `lease` is set again; first in line 2"
        );
        assert_eq!(
            message("stemma-policy 1\napprovals 1 3\napprovals 1 4\n"),
            "line 3: `approvals 1` is set again; first in line 2"
        );
        assert_eq!(
            message("stemma-policy 1\noperation tag 2\noperation tag 3\n"),
            "line 3: `operation tag` is set again; first in line 2"
        );
        // Another level or operation is another setting.
        policy(
            "stemma-policy 1\napprovals 0 2\napprovals 1 2\noperation tag 2\noperation untag 2\n",
        );
    }

    #[test]
    fn the_patterns_are_those_of_the_exclusion_file() {
        for (pattern, why) in [
            ("!a", "negation"),
            ("a[b]", "reserved"),
            ("a//b", "empty part"),
            ("../a", "not a valid name"),
            ("a\\b", "reserved"),
        ] {
            let error = message(&format!("stemma-policy 1\nlevel 1 {pattern}\n"));
            assert!(
                error.starts_with("line 2: ") && error.contains(why),
                "{pattern}: {error}"
            );
        }
        for pattern in ["**", "**/**", "/**"] {
            assert_eq!(
                message(&format!("stemma-policy 1\nlevel 1 {pattern}\n")),
                "line 2: the pattern would match every path; name the paths that differ"
            );
        }
        // The one difference: the exclusion file may be named, as ADR-004 gives it level 2.
        assert_eq!(
            level(
                &policy("stemma-policy 1\nlevel 3 .stemmaignore\n"),
                ".stemmaignore"
            ),
            3
        );
        // A name with a space inside is a name.
        assert_eq!(
            level(&policy("stemma-policy 1\nlevel 0 my docs/\n"), "my docs/a"),
            0
        );
    }

    #[test]
    fn the_lines_of_the_file_are_checked_as_those_of_the_exclusion_file() {
        assert_eq!(
            message("stemma-policy 1\nlevel 1 a\nlevel 1 a\n"),
            "line 3: repeats line 2"
        );
        assert_eq!(
            message("stemma-policy 1\r\nlevel 1 a\n"),
            "line 2: line end differs from line 1; use `\\n` or `\\r\\n` in the whole file"
        );
        assert_eq!(
            message("stemma-policy 1\nlevel 1 a"),
            "line 2: does not end with a newline"
        );
        assert_eq!(
            message("stemma-policy 1 \n"),
            "line 1: space at the start or end"
        );
        assert_eq!(message("stemma-policy\t1\n"), "line 1: control character");
        assert_eq!(
            message("\u{feff}stemma-policy 1\n"),
            "the file starts with a byte order mark"
        );
        assert_eq!(
            Policy::parse(b"stemma-policy 1\n\xff")
                .unwrap_err()
                .to_string(),
            "the file is not UTF-8"
        );
        let big = format!(
            "stemma-policy 1\n# {}\n",
            "x".repeat(crate::ignore::MAX_FILE_BYTES)
        );
        assert_eq!(message(&big), "the file has more than 65536 bytes");
        assert!(
            message(&format!("stemma-policy 1\nlevel 1 {}\n", "a".repeat(1100)))
                .contains("more than 1024 bytes")
        );
    }

    #[test]
    fn the_first_error_names_the_first_line_that_is_refused() {
        // Line 2 is refused before line 3 is looked at.
        let error = parse("stemma-policy 1\nlevel 9 a\nlevel\there\n").unwrap_err();
        assert_eq!(error.line().map(NonZeroUsize::get), Some(2));
        assert!(error.to_string().contains("not a level"));
    }

    #[test]
    fn a_path_that_is_not_a_path_is_an_error() {
        let p = Policy::default();
        for path in ["", "/a", "a/", "a//b", "a/../b", "./a", "a/./b"] {
            assert!(p.level_of_path(path, EntryKind::File).is_err(), "{path:?}");
        }
        assert_eq!(
            p.level_of(&[file("a"), file("b//c")])
                .unwrap_err()
                .to_string(),
            "`` is not a valid name in a path: the name is empty"
        );
    }

    #[test]
    fn comments_and_empty_lines_do_not_make_policies_differ() {
        assert_eq!(
            policy("stemma-policy 1\nlevel 0 docs/\n"),
            policy("# c\nstemma-policy 1\n\n# d\nlevel 0 docs/\n")
        );
        assert_ne!(
            policy("stemma-policy 1\nlevel 0 docs/\n"),
            policy("stemma-policy 1\nlevel 1 docs/\n")
        );
    }

    #[test]
    fn encode_gives_the_canonical_form_and_reads_back() {
        let p = policy(&format!(
            "stemma-policy 1\r\n# c\r\n\r\n{}",
            ADR_TABLE
                .replacen("stemma-policy 1\n", "", 1)
                .replace('\n', "\r\n")
        ));
        let text = p.encode();
        assert!(text.starts_with("stemma-policy 1\nlevel 3 .github/workflows/\n"));
        assert!(!text.contains('#') && !text.contains('\r'));
        let again = policy(&text);
        assert_eq!(again, p);
        assert_eq!(again.encode(), text);
        for path in ["justfile", "docs/a.tex", "crates/stemma-cli/x", "other"] {
            assert_eq!(level(&again, path), level(&p, path));
        }
        let all = "stemma-policy 1\nlevel 1 a\nlevel 3 b/\n";
        assert_eq!(policy(all).encode(), all);
        let settings = "stemma-policy 1\nlease 7\napprovals 0 3\napprovals 3 1\n\
                        operation diff 3\noperation tag 3\n";
        assert_eq!(policy(settings).encode(), settings);
        assert_eq!(Policy::default().encode(), "stemma-policy 1\n");
    }

    #[test]
    fn the_seeds_of_the_fuzz_target_are_read() {
        for (name, seed) in crate::fuzz::tests::seeds("policy") {
            assert!(Policy::parse(&seed).is_ok(), "{name} is not accepted");
            for end in 0..=seed.len() {
                crate::fuzz::policy(&seed[..end]);
            }
        }
    }
}
