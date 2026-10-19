//! The operation log (ADR-011): what every operation did to the pointers of a repository.
//!
//! The log is local. It is not an object and is not exchanged with the history (ADR-005). It is
//! the directory `.stemma/oplog/`, and each entry is a file of its own, named by its number: twelve
//! decimal digits, zero-padded, starting with 1. An entry holds the actor, the time, the name of
//! the operation, and for each pointer that the operation changes (HEAD, the working-copy commit,
//! branches) its old and its new value. The old values are the commits that stop being reachable,
//! so the log keeps them (ADR-010).
//!
//! An entry is written, and flushed, *before* the pointer moves. It is therefore an intent: the
//! move can still fail, because another writer was first. Nothing is written after the move, so
//! nothing can be half written; a reader sees from the chain of the entries of a pointer whether
//! a move happened (ADR-011).
//!
//! # The encoding
//!
//! Version 1 is strict text. A reader accepts what [`Entry::encode`] writes and nothing else, so
//! one entry has exactly one encoding (ADR-005):
//!
//! ```text
//! stemma-oplog 1
//! number 42
//! time 1700000000
//! actor human daniel
//! operation snapshot
//! head branch:main branch:main
//! working-copy b3:1d71…eed9 b3:4feb…b32c
//! branch main b3:8cb4…b3e8 b3:b5f2…e662
//! ```
//!
//! The first five lines come in this order, each once. Then come the lines of the pointers that
//! changed, at least one: `head`, then `working-copy`, then one `branch <name>` for each branch,
//! sorted by name in UTF-8 byte order and each name once. Each has the old and the new value, which
//! differ. `-` means that the pointer did not exist, or does not exist any more. A branch and the
//! working-copy commit have a commit ID or `-`; HEAD has `branch:<name>`, `commit:<id>`, or `-`.
//! The file ends with a newline and has nothing after the last pointer.
//!
//! # Numbers and writers
//!
//! A writer takes the next number that is free and publishes the file with a hard link, which
//! fails if the number is taken, so the entry is whole from the first moment it can be seen. If
//! the number is taken, another writer was faster, and the writer looks for the end of the log again
//! and tries the next free number. This orders writers without a lock. The numbers have no gap: a gap means that an entry is lost, and the
//! reader reports it.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::object::{parse_seconds, CommitId, ObjectId, ObjectIdParseError, TimeError};
use crate::refs::{BranchName, BranchNameError, Head};
use crate::store::{create_temp, sync_directory, unique_temp_name, StoreError};
use crate::{Actor, ActorParseError};

/// The first line of every entry, with the version of the encoding.
const HEADER: &str = "stemma-oplog 1";

/// The start of the first line of every version of the encoding.
const HEADER_PREFIX: &str = "stemma-oplog ";

/// The number of digits of a file name.
const NUMBER_WIDTH: usize = 12;

/// The largest number of an entry, the largest value of [`NUMBER_WIDTH`] digits.
pub const MAX_NUMBER: u64 = 999_999_999_999;

/// How many times a writer tries again before it gives up. A try fails only because another writer
/// took the number, which means that the other one made progress, so the limit is reached only by
/// a writer that is overtaken again and again.
const ATTEMPTS: usize = 64;

/// The longest name of an operation, in characters.
const MAX_OPERATION_LEN: usize = 32;

/// The name of an operation, such as `init`, `snapshot`, or `commit`.
///
/// It is a lowercase ASCII letter followed by lowercase letters, digits, and `-`, at most 32
/// characters. The rule keeps the name one token of a line, and one spelling of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OperationName(String);

/// Why a string is not the name of an operation. The `Display` form is the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OperationNameError {
    /// The name is empty.
    Empty,
    /// The name is longer than 32 characters. Carries its length in bytes.
    TooLong(usize),
    /// The name does not start with a lowercase letter.
    BadStart,
    /// A character is not a lowercase letter, a digit, or `-`. Carries its position in bytes.
    BadCharacter(usize),
}

impl fmt::Display for OperationNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the name of an operation is empty"),
            Self::TooLong(length) => write!(
                f,
                "the name of an operation has at most {MAX_OPERATION_LEN} characters, not \
                 {length} bytes"
            ),
            Self::BadStart => {
                f.write_str("the name of an operation starts with a lowercase letter")
            }
            Self::BadCharacter(position) => write!(
                f,
                "byte {position} of the name of an operation is not a lowercase letter, a digit, \
                 or `-`"
            ),
        }
    }
}

impl Error for OperationNameError {}

impl OperationName {
    /// Checks the name.
    ///
    /// # Errors
    ///
    /// Returns [`OperationNameError`] for a name that is empty, longer than 32 characters, does
    /// not start with a lowercase letter, or has another character than a lowercase letter, a
    /// digit, or `-`.
    pub fn new(name: impl Into<String>) -> Result<Self, OperationNameError> {
        let name = name.into();
        if name.is_empty() {
            return Err(OperationNameError::Empty);
        }
        if name.len() > MAX_OPERATION_LEN {
            return Err(OperationNameError::TooLong(name.len()));
        }
        if !name.as_bytes()[0].is_ascii_lowercase() {
            return Err(OperationNameError::BadStart);
        }
        if let Some(position) = name
            .bytes()
            .position(|b| !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
        {
            return Err(OperationNameError::BadCharacter(position));
        }
        Ok(Self(name))
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OperationName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for OperationName {
    type Err = OperationNameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::new(name)
    }
}

/// The value of one pointer before and after an operation. `None` means that the pointer did not
/// exist, or does not exist any more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved<T> {
    old: Option<T>,
    new: Option<T>,
}

impl<T> Moved<T> {
    /// A pointer that goes from `old` to `new`. [`Change::new`] rejects a pointer where the two
    /// are equal.
    pub fn from_to(old: Option<T>, new: Option<T>) -> Self {
        Self { old, new }
    }

    /// The value before the operation.
    pub fn before(&self) -> Option<&T> {
        self.old.as_ref()
    }

    /// The value after the operation.
    pub fn after(&self) -> Option<&T> {
        self.new.as_ref()
    }
}

/// The pointer of a change, for a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pointer {
    /// HEAD.
    Head,
    /// The working-copy commit.
    WorkingCopy,
    /// A branch.
    Branch(BranchName),
}

impl fmt::Display for Pointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Head => f.write_str("HEAD"),
            Self::WorkingCopy => f.write_str("the working-copy commit"),
            Self::Branch(name) => write!(f, "the branch `{name}`"),
        }
    }
}

/// Why a change or an entry is rejected. The `Display` form is the message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryError {
    /// The operation changes no pointer. Only a change writes an entry (ADR-011).
    NoChange,
    /// The old and the new value of a pointer are equal, so it did not change.
    Unchanged(Pointer),
    /// A branch is listed twice.
    DuplicateBranch(BranchName),
    /// The number is 0 or larger than [`MAX_NUMBER`].
    NumberOutOfRange(u64),
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoChange => f.write_str("the operation changes no pointer"),
            Self::Unchanged(pointer) => write!(f, "{pointer} has the same old and new value"),
            Self::DuplicateBranch(name) => write!(f, "the branch `{name}` is listed twice"),
            Self::NumberOutOfRange(number) => {
                write!(f, "entry number {number} is not between 1 and {MAX_NUMBER}")
            }
        }
    }
}

impl Error for EntryError {}

/// What an operation did to the pointers, without the number that the log gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    time: i64,
    actor: Actor,
    operation: OperationName,
    head: Option<Moved<Head>>,
    working_copy: Option<Moved<CommitId>>,
    branches: Vec<(BranchName, Moved<CommitId>)>,
}

impl Change {
    /// Builds a change. Branches are kept sorted by name, so the order of the input does not
    /// matter. The time is the one that the writer claims, in seconds since the Unix epoch (UTC),
    /// as in a commit (ADR-005).
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::NoChange`] if no pointer is given, [`EntryError::Unchanged`] for a
    /// pointer whose old and new value are equal, and [`EntryError::DuplicateBranch`] for a
    /// branch that is listed twice.
    pub fn new(
        time: i64,
        actor: Actor,
        operation: OperationName,
        head: Option<Moved<Head>>,
        working_copy: Option<Moved<CommitId>>,
        mut branches: Vec<(BranchName, Moved<CommitId>)>,
    ) -> Result<Self, EntryError> {
        if head.is_none() && working_copy.is_none() && branches.is_empty() {
            return Err(EntryError::NoChange);
        }
        if head.as_ref().is_some_and(|m| m.old == m.new) {
            return Err(EntryError::Unchanged(Pointer::Head));
        }
        if working_copy.as_ref().is_some_and(|m| m.old == m.new) {
            return Err(EntryError::Unchanged(Pointer::WorkingCopy));
        }
        branches.sort_by(|a, b| a.0.cmp(&b.0));
        for pair in branches.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(EntryError::DuplicateBranch(pair[0].0.clone()));
            }
        }
        if let Some((name, _)) = branches.iter().find(|(_, m)| m.old == m.new) {
            return Err(EntryError::Unchanged(Pointer::Branch(name.clone())));
        }
        Ok(Self {
            time,
            actor,
            operation,
            head,
            working_copy,
            branches,
        })
    }

    /// The time that the writer claims, in seconds since the Unix epoch (UTC). It proves nothing;
    /// the order of the entries is the order of their numbers.
    pub fn time(&self) -> i64 {
        self.time
    }

    /// Who ran the operation.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// The name of the operation.
    pub fn operation(&self) -> &OperationName {
        &self.operation
    }

    /// The change of HEAD, if the operation changed it.
    pub fn head(&self) -> Option<&Moved<Head>> {
        self.head.as_ref()
    }

    /// The change of the working-copy commit, if the operation changed it.
    pub fn working_copy(&self) -> Option<&Moved<CommitId>> {
        self.working_copy.as_ref()
    }

    /// The branches that the operation changed, sorted by name.
    pub fn branches(&self) -> &[(BranchName, Moved<CommitId>)] {
        &self.branches
    }

    /// Every commit that the change names, old and new: the roots that the garbage collection
    /// needs from this entry (ADR-007, ADR-011).
    pub fn commits(&self) -> Vec<CommitId> {
        let mut commits = Vec::new();
        if let Some(head) = &self.head {
            for value in [&head.old, &head.new] {
                if let Some(Head::Detached(commit)) = value {
                    commits.push(*commit);
                }
            }
        }
        let moves = self
            .working_copy
            .iter()
            .chain(self.branches.iter().map(|(_, moved)| moved));
        for moved in moves {
            commits.extend(moved.old.iter().chain(moved.new.iter()).copied());
        }
        commits
    }
}

/// An entry of the log: a number and a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    number: u64,
    change: Change,
}

impl Entry {
    /// Builds an entry.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::NumberOutOfRange`] if the number is 0 or larger than [`MAX_NUMBER`].
    pub fn new(number: u64, change: Change) -> Result<Self, EntryError> {
        if !(1..=MAX_NUMBER).contains(&number) {
            return Err(EntryError::NumberOutOfRange(number));
        }
        Ok(Self { number, change })
    }

    /// The number of the entry, which is also the name of its file.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// What the operation did.
    pub fn change(&self) -> &Change {
        &self.change
    }

    /// Returns the canonical encoding (see the module documentation).
    pub fn encode(&self) -> Vec<u8> {
        let change = &self.change;
        let mut out = format!(
            "{HEADER}\nnumber {}\ntime {}\nactor {}\noperation {}\n",
            self.number, change.time, change.actor, change.operation
        );
        if let Some(head) = &change.head {
            out.push_str(&format!(
                "head {} {}\n",
                head_value(head.old.as_ref()),
                head_value(head.new.as_ref())
            ));
        }
        if let Some(moved) = &change.working_copy {
            out.push_str(&format!(
                "working-copy {} {}\n",
                commit_value(moved.old.as_ref()),
                commit_value(moved.new.as_ref())
            ));
        }
        for (name, moved) in &change.branches {
            out.push_str(&format!(
                "branch {name} {} {}\n",
                commit_value(moved.old.as_ref()),
                commit_value(moved.new.as_ref())
            ));
        }
        out.into_bytes()
    }

    /// Reads an entry, as [`encode`](Self::encode) writes it. The reader is strict, by the rule of
    /// ADR-005: it rejects everything that `encode` would not have written, so an entry has
    /// exactly one encoding, and `parse(bytes).encode() == bytes` for every accepted input.
    ///
    /// # Errors
    ///
    /// Returns [`EntryParseError`] for input that is not UTF-8, a last line without a newline, a
    /// header that is missing, repeated, unknown, or out of order, a version other than 1, a number,
    /// time, actor, operation, commit ID, or branch name that is not in canonical form, a pointer
    /// whose old and new value are equal, branches that are not strictly ascending, and an entry
    /// without a pointer.
    pub fn parse(bytes: &[u8]) -> Result<Self, EntryParseError> {
        let text = std::str::from_utf8(bytes).map_err(|error| EntryParseError::NotUtf8 {
            valid_up_to: error.valid_up_to(),
        })?;
        let malformed = |line, reason| EntryParseError::Malformed { line, reason };
        if !text.is_empty() && !text.ends_with('\n') {
            return Err(malformed(
                text.matches('\n').count() + 1,
                EntryMalformed::CutOff,
            ));
        }
        let lines: Vec<&str> = text.split_terminator('\n').collect();
        let line_at = |index: usize, missing| {
            lines
                .get(index)
                .copied()
                .ok_or_else(|| malformed(index + 1, missing))
        };

        // The header and its version.
        let header = line_at(0, EntryMalformed::ExpectedHeader)?;
        match header.strip_prefix(HEADER_PREFIX) {
            Some("1") => {}
            Some(version) if !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit()) => {
                return Err(EntryParseError::UnsupportedVersion(version.to_owned()));
            }
            _ => return Err(malformed(1, EntryMalformed::ExpectedHeader)),
        }

        let value = |index: usize, key: &str, reason| -> Result<&str, EntryParseError> {
            line_at(index, reason)?
                .strip_prefix(key)
                .ok_or_else(|| malformed(index + 1, reason))
        };
        let number = value(1, "number ", EntryMalformed::ExpectedNumber)?;
        let number = parse_number(number)
            .map_err(|reason| EntryParseError::InvalidNumber { line: 2, reason })?;
        let time = value(2, "time ", EntryMalformed::ExpectedTime)?;
        let time = parse_seconds(time)
            .map_err(|reason| EntryParseError::InvalidTime { line: 3, reason })?;
        let actor = value(3, "actor ", EntryMalformed::ExpectedActor)?;
        let actor = Actor::from_str(actor)
            .map_err(|source| EntryParseError::InvalidActor { line: 4, source })?;
        let operation = value(4, "operation ", EntryMalformed::ExpectedOperation)?;
        let operation = OperationName::new(operation)
            .map_err(|source| EntryParseError::InvalidOperation { line: 5, source })?;

        // The pointers, in the fixed order: head, working-copy, branches.
        let mut head = None;
        let mut working_copy = None;
        let mut branches: Vec<(BranchName, Moved<CommitId>)> = Vec::new();
        for (index, line) in lines.iter().enumerate().skip(5) {
            let number = index + 1;
            let tokens: Vec<&str> = line.split(' ').collect();
            let after_branches = !branches.is_empty();
            match tokens.as_slice() {
                ["head", old, new] => {
                    if head.is_some() || working_copy.is_some() || after_branches {
                        return Err(malformed(number, EntryMalformed::PointerOutOfOrder));
                    }
                    let moved = Moved {
                        old: parse_head_value(old, number)?,
                        new: parse_head_value(new, number)?,
                    };
                    if moved.old == moved.new {
                        return Err(EntryParseError::Unchanged { line: number });
                    }
                    head = Some(moved);
                }
                ["working-copy", old, new] => {
                    if working_copy.is_some() || after_branches {
                        return Err(malformed(number, EntryMalformed::PointerOutOfOrder));
                    }
                    let moved = Moved {
                        old: parse_commit_value(old, number)?,
                        new: parse_commit_value(new, number)?,
                    };
                    if moved.old == moved.new {
                        return Err(EntryParseError::Unchanged { line: number });
                    }
                    working_copy = Some(moved);
                }
                ["branch", name, old, new] => {
                    let name = BranchName::new(*name).map_err(|source| {
                        EntryParseError::InvalidBranchName {
                            line: number,
                            source,
                        }
                    })?;
                    let moved = Moved {
                        old: parse_commit_value(old, number)?,
                        new: parse_commit_value(new, number)?,
                    };
                    if moved.old == moved.new {
                        return Err(EntryParseError::Unchanged { line: number });
                    }
                    if let Some((previous, _)) = branches.last() {
                        if previous.as_str().as_bytes() >= name.as_str().as_bytes() {
                            return Err(EntryParseError::OutOfOrder {
                                line: number,
                                name: name.to_string(),
                            });
                        }
                    }
                    branches.push((name, moved));
                }
                ["head" | "working-copy" | "branch", ..] => {
                    return Err(malformed(number, EntryMalformed::WrongValueCount));
                }
                _ => return Err(malformed(number, EntryMalformed::UnknownPointer)),
            }
        }
        if head.is_none() && working_copy.is_none() && branches.is_empty() {
            return Err(malformed(lines.len() + 1, EntryMalformed::ExpectedPointer));
        }
        // The rules of a change live in one place. The lines have been checked one by one above, to
        // report the line; this builds the value through the constructor, so that a reader cannot
        // return a change that the constructor would reject.
        Change::new(time, actor, operation, head, working_copy, branches)
            .and_then(|change| Entry::new(number, change))
            .map_err(EntryParseError::Entry)
    }
}

/// The text of a commit value: `-` or the ID.
fn commit_value(value: Option<&CommitId>) -> String {
    value.map_or_else(|| "-".to_owned(), ToString::to_string)
}

/// The text of a HEAD value: `-`, `branch:<name>`, or `commit:<id>`.
fn head_value(value: Option<&Head>) -> String {
    match value {
        None => "-".to_owned(),
        Some(Head::Branch(name)) => format!("branch:{name}"),
        Some(Head::Detached(commit)) => format!("commit:{commit}"),
    }
}

fn parse_commit_value(text: &str, line: usize) -> Result<Option<CommitId>, EntryParseError> {
    if text == "-" {
        return Ok(None);
    }
    ObjectId::from_str(text)
        .map(|id| Some(CommitId::new(id)))
        .map_err(|source| EntryParseError::InvalidObjectId { line, source })
}

fn parse_head_value(text: &str, line: usize) -> Result<Option<Head>, EntryParseError> {
    if text == "-" {
        return Ok(None);
    }
    match text.split_once(':') {
        Some(("branch", name)) => BranchName::new(name)
            .map(|name| Some(Head::Branch(name)))
            .map_err(|source| EntryParseError::InvalidBranchName { line, source }),
        Some(("commit", id)) => ObjectId::from_str(id)
            .map(|id| Some(Head::Detached(CommitId::new(id))))
            .map_err(|source| EntryParseError::InvalidObjectId { line, source }),
        _ => Err(EntryParseError::Malformed {
            line,
            reason: EntryMalformed::UnknownHeadValue,
        }),
    }
}

/// Reads the number of an entry: decimal digits without a leading zero, from 1 to
/// [`MAX_NUMBER`].
fn parse_number(text: &str) -> Result<u64, NumberError> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) || (text.starts_with('0')) {
        return Err(NumberError::NotCanonical);
    }
    match text.parse::<u64>() {
        Ok(number) if number <= MAX_NUMBER => Ok(number),
        _ => Err(NumberError::OutOfRange),
    }
}

/// Why the number of an entry is rejected. The `Display` form is the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NumberError {
    /// The text is empty, has a character that is not a digit, or starts with `0`.
    NotCanonical,
    /// The number is larger than [`MAX_NUMBER`], or does not fit a 64-bit integer.
    OutOfRange,
}

impl fmt::Display for NumberError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotCanonical => {
                f.write_str("the number is not a decimal integer from 1, without a leading zero")
            }
            Self::OutOfRange => write!(f, "the number is larger than {MAX_NUMBER}"),
        }
    }
}

impl Error for NumberError {}

/// What is wrong with the shape of the lines of an entry. The `Display` form is the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EntryMalformed {
    /// The last line has no terminating newline: the entry is cut off.
    CutOff,
    /// The first line is not `stemma-oplog <version>`.
    ExpectedHeader,
    /// The second line is not `number <n>`.
    ExpectedNumber,
    /// The third line is not `time <seconds>`.
    ExpectedTime,
    /// The fourth line is not `actor <actor line>`.
    ExpectedActor,
    /// The fifth line is not `operation <name>`.
    ExpectedOperation,
    /// The entry ends without a line for a pointer.
    ExpectedPointer,
    /// A line is not `head`, `working-copy`, or `branch`.
    UnknownPointer,
    /// A pointer line has too few or too many values.
    WrongValueCount,
    /// `head` or `working-copy` comes after a line that has to come later, or twice.
    PointerOutOfOrder,
    /// A value of `head` is not `-`, `branch:<name>`, or `commit:<id>`.
    UnknownHeadValue,
}

impl fmt::Display for EntryMalformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CutOff => "the last line has no terminating newline",
            Self::ExpectedHeader => "expected the header `stemma-oplog 1` first",
            Self::ExpectedNumber => "expected the header `number <n>`",
            Self::ExpectedTime => "expected the header `time <seconds>`",
            Self::ExpectedActor => "expected the header `actor <actor>`",
            Self::ExpectedOperation => "expected the header `operation <name>`",
            Self::ExpectedPointer => "expected the line of a pointer that changed",
            Self::UnknownPointer => "expected `head`, `working-copy`, or `branch <name>`",
            Self::WrongValueCount => "expected the old and the new value of the pointer",
            Self::PointerOutOfOrder => {
                "the pointers come in the order head, working-copy, branches, each once"
            }
            Self::UnknownHeadValue => "expected `-`, `branch:<name>`, or `commit:<id>`",
        })
    }
}

/// Why the body of an entry is rejected by [`Entry::parse`]. Line numbers count from 1.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryParseError {
    /// The body is not valid UTF-8.
    NotUtf8 {
        /// Length of the longest valid prefix, in bytes.
        valid_up_to: usize,
    },
    /// A line is missing, repeated, unknown, or out of order, or the last line is cut off.
    Malformed {
        /// Number of the line.
        line: usize,
        /// What is wrong with it.
        reason: EntryMalformed,
    },
    /// The header names another version than 1, in decimal digits.
    UnsupportedVersion(String),
    /// The number is not canonical.
    InvalidNumber {
        /// Number of the line.
        line: usize,
        /// Why the number is invalid.
        reason: NumberError,
    },
    /// The time is not a canonical decimal integer of seconds, or does not fit `i64`.
    InvalidTime {
        /// Number of the line.
        line: usize,
        /// Why the time is invalid.
        reason: TimeError,
    },
    /// The actor line is rejected.
    InvalidActor {
        /// Number of the line.
        line: usize,
        /// Why the actor is invalid.
        source: ActorParseError,
    },
    /// The name of the operation is rejected.
    InvalidOperation {
        /// Number of the line.
        line: usize,
        /// Why the name is invalid.
        source: OperationNameError,
    },
    /// A commit ID is not in canonical form.
    InvalidObjectId {
        /// Number of the line.
        line: usize,
        /// Why the ID is invalid.
        source: ObjectIdParseError,
    },
    /// A branch name is rejected.
    InvalidBranchName {
        /// Number of the line.
        line: usize,
        /// Why the name is invalid.
        source: BranchNameError,
    },
    /// The old and the new value of a pointer are equal.
    Unchanged {
        /// Number of the line.
        line: usize,
    },
    /// A branch does not sort after the one before it, or is listed twice.
    OutOfOrder {
        /// Number of the line.
        line: usize,
        /// The name of the branch.
        name: String,
    },
    /// The lines are well formed, but the entry is rejected as a whole. Every rule of
    /// [`Change::new`] and [`Entry::new`] is already checked while the lines are read, so this does
    /// not occur today; it is here for a rule that is added later.
    Entry(EntryError),
}

impl fmt::Display for EntryParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8 { valid_up_to } => write!(
                f,
                "the entry is not valid UTF-8 (valid up to byte {valid_up_to})"
            ),
            Self::Malformed { line, reason } => write!(f, "line {line}: {reason}"),
            Self::UnsupportedVersion(version) => write!(
                f,
                "the entry has version {version}, but this code reads version 1"
            ),
            Self::InvalidNumber { line, reason } => write!(f, "line {line}: {reason}"),
            Self::InvalidTime { line, reason } => write!(f, "line {line}: {reason}"),
            Self::InvalidActor { line, source } => write!(f, "line {line}: {source}"),
            Self::InvalidOperation { line, source } => write!(f, "line {line}: {source}"),
            Self::InvalidObjectId { line, source } => {
                write!(f, "line {line}: invalid commit ID: {source}")
            }
            Self::InvalidBranchName { line, source } => write!(f, "line {line}: {source}"),
            Self::Unchanged { line } => {
                write!(f, "line {line}: the old and the new value are equal")
            }
            Self::OutOfOrder { line, name } => write!(
                f,
                "line {line}: the branch `{name}` does not sort after the one before it; \
                 branches must be strictly ascending"
            ),
            Self::Entry(source) => source.fmt(f),
        }
    }
}

impl Error for EntryParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message of each variant includes the message of its cause, so the source is the
        // cause of the cause (see the crate documentation).
        match self {
            Self::InvalidNumber { reason, .. } => reason.source(),
            Self::InvalidTime { reason, .. } => reason.source(),
            Self::InvalidActor { source, .. } => source.source(),
            Self::InvalidOperation { source, .. } => source.source(),
            Self::InvalidObjectId { source, .. } => source.source(),
            Self::InvalidBranchName { source, .. } => source.source(),
            _ => None,
        }
    }
}

/// Why an operation on the log fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum OpLogError {
    /// The file system failed. The cause is the field `source`, and its message ends the message
    /// of this error. Use [`OpLogError::io_error`] to reach it.
    Io {
        /// What was being done, for example "write the entry".
        operation: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// An entry is not in canonical form.
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        source: EntryParseError,
    },
    /// The number inside an entry is not the number in the name of its file.
    WrongNumber {
        /// The file.
        path: PathBuf,
        /// The number in the name.
        expected: u64,
        /// The number inside.
        found: u64,
    },
    /// The numbers of the entries have a gap: an entry is lost.
    Gap {
        /// The first number that is missing.
        missing: u64,
    },
    /// The directory of the log holds something that is not an entry.
    UnexpectedFile {
        /// The file or directory.
        path: PathBuf,
    },
    /// There is no entry with this number.
    Missing {
        /// The number.
        number: u64,
    },
    /// The log is full: the next number is larger than [`MAX_NUMBER`], or every number that the
    /// writer tried was taken.
    Exhausted,
    /// The entry was written, but its directory could not be flushed to disk, so it may not
    /// survive a crash. A caller that orders "entry, then move" must not move: the entry is not
    /// known to be safe.
    NotFlushed {
        /// The directory that could not be flushed.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// The object store failed in a way that is not a failure of the file system.
    Store(Box<StoreError>),
    /// The change is rejected.
    Entry(EntryError),
}

impl OpLogError {
    fn io<'a>(
        operation: &'static str,
        path: &'a Path,
    ) -> impl FnOnce(io::Error) -> OpLogError + 'a {
        move |source| OpLogError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    fn from_store(error: StoreError) -> Self {
        match error {
            StoreError::Io {
                operation,
                path,
                source,
            } => Self::Io {
                operation,
                path,
                source,
            },
            other => Self::Store(Box::new(other)),
        }
    }

    /// Returns the error of the file system for [`OpLogError::Io`] and
    /// [`OpLogError::NotFlushed`], and for a [`StoreError::Io`] inside. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } | Self::NotFlushed { source, .. } => Some(source),
            Self::Store(error) => error.io_error(),
            _ => None,
        }
    }
}

impl fmt::Display for OpLogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
            Self::Corrupt { path, source } => write!(
                f,
                "the entry `{}` is not in canonical form: {source}",
                path.display()
            ),
            Self::WrongNumber {
                path,
                expected,
                found,
            } => write!(
                f,
                "the entry `{}` has number {found} inside, but its name says {expected}",
                path.display()
            ),
            Self::Gap { missing } => write!(
                f,
                "the operation log has no entry {missing}, but later ones: an entry is lost"
            ),
            Self::UnexpectedFile { path } => write!(
                f,
                "`{}` is in the operation log, but is not an entry",
                path.display()
            ),
            Self::Missing { number } => write!(f, "the operation log has no entry {number}"),
            Self::Exhausted => {
                f.write_str("the operation log is full, or every number that was tried is taken")
            }
            Self::NotFlushed { path, source } => write!(
                f,
                "the entry was written, but flushing `{}` failed: {source}; it may not survive a \
                 crash",
                path.display()
            ),
            Self::Store(error) => error.fmt(f),
            Self::Entry(error) => error.fmt(f),
        }
    }
}

impl Error for OpLogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes that of the I/O error, so the source is its own cause.
            Self::Io { source, .. } | Self::NotFlushed { source, .. } => source.source(),
            Self::Corrupt { source, .. } => source.source(),
            Self::Store(error) => error.source(),
            Self::Entry(error) => error.source(),
            _ => None,
        }
    }
}

/// The operation log of a repository, as files below its metadata directory.
#[derive(Debug, Clone)]
pub struct FsOpLog {
    root: PathBuf,
}

impl FsOpLog {
    /// Creates a log that keeps its files below `root`, the repository's metadata directory. The
    /// directory is not checked here; every operation checks it.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn dir(&self) -> PathBuf {
        self.root.join("oplog")
    }

    fn path_of(&self, number: u64) -> PathBuf {
        self.dir().join(format!("{number:0NUMBER_WIDTH$}"))
    }

    /// Lists the numbers of the entries in ascending order. They have to be 1, 2, 3, … without a
    /// gap, and the directory holds nothing else.
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::UnexpectedFile`] for a name that is not an entry number,
    /// [`OpLogError::Gap`] if a number is missing, and [`OpLogError::Io`] if the directory cannot
    /// be read.
    pub fn numbers(&self) -> Result<Vec<u64>, OpLogError> {
        let dir = self.dir();
        let mut numbers = Vec::new();
        for item in fs::read_dir(&dir).map_err(OpLogError::io("read the directory", &dir))? {
            let item = item.map_err(OpLogError::io("read the directory", &dir))?;
            let path = item.path();
            let number = item
                .file_name()
                .to_str()
                .and_then(parse_file_name)
                .ok_or_else(|| OpLogError::UnexpectedFile { path: path.clone() })?;
            numbers.push(number);
        }
        numbers.sort_unstable();
        for (index, number) in numbers.iter().enumerate() {
            let expected = index as u64 + 1;
            if *number != expected {
                return Err(OpLogError::Gap { missing: expected });
            }
        }
        Ok(numbers)
    }

    /// Reads the entry with the number.
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::Missing`] if there is none, [`OpLogError::Corrupt`] if it is not in
    /// canonical form, [`OpLogError::WrongNumber`] if the number inside is not the one of the
    /// name, and [`OpLogError::Io`] if the storage fails.
    pub fn read(&self, number: u64) -> Result<Entry, OpLogError> {
        let path = self.path_of(number);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(OpLogError::Missing { number });
            }
            Err(error) => return Err(OpLogError::io("read the entry", &path)(error)),
        };
        let entry = Entry::parse(&bytes).map_err(|source| OpLogError::Corrupt {
            path: path.clone(),
            source,
        })?;
        if entry.number != number {
            return Err(OpLogError::WrongNumber {
                path,
                expected: number,
                found: entry.number,
            });
        }
        Ok(entry)
    }

    /// Reads every entry, in the order of the numbers.
    ///
    /// # Errors
    ///
    /// See [`numbers`](Self::numbers) and [`read`](Self::read).
    pub fn entries(&self) -> Result<Vec<Entry>, OpLogError> {
        self.numbers()?
            .into_iter()
            .map(|number| self.read(number))
            .collect()
    }

    /// Writes a new entry for the change under the next free number, flushes it, and returns it.
    /// The entry is whole when it can first be seen: it is written to a temporary file, flushed,
    /// and published with a hard link, which fails if the number is taken. If it is taken, another
    /// writer was faster, and the next number is tried.
    ///
    /// A caller that moves a pointer calls this first and moves only if it succeeds (ADR-011).
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::Exhausted`] if the log is full, [`OpLogError::NotFlushed`] if the
    /// entry is in place but not known to be flushed, and [`OpLogError::Io`] if the storage fails,
    /// among others if the file system does not support hard links, or if the directory of the log
    /// does not exist. The reading rules of [`numbers`](Self::numbers) apply to the numbers that
    /// are found.
    pub fn append(&self, change: &Change) -> Result<Entry, OpLogError> {
        let dir = self.dir();
        let temp_dir = self.root.join("tmp");
        let mut number = self.numbers()?.last().map_or(1, |last| last + 1);
        for _ in 0..ATTEMPTS {
            let entry = Entry::new(number, change.clone()).map_err(|error| match error {
                EntryError::NumberOutOfRange(_) => OpLogError::Exhausted,
                other => OpLogError::Entry(other),
            })?;
            let (mut file, temp) =
                create_temp(&temp_dir, &mut unique_temp_name).map_err(OpLogError::from_store)?;
            file.write_all(&entry.encode())
                .map_err(OpLogError::io("write the temporary file", &temp.path))?;
            file.sync_all()
                .map_err(OpLogError::io("flush the temporary file", &temp.path))?;
            drop(file);
            let path = self.path_of(number);
            match fs::hard_link(&temp.path, &path) {
                Ok(()) => {
                    // `temp` removes the other name of the file when it is dropped.
                    drop(temp);
                    return sync_directory(&dir)
                        .map(|()| entry)
                        .map_err(|error| match error {
                            StoreError::Io { source, .. } => OpLogError::NotFlushed {
                                path: dir.clone(),
                                source,
                            },
                            other => OpLogError::Store(Box::new(other)),
                        });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    // Another writer was faster. Look again for the end of the log, which can be
                    // several entries further on, instead of stepping by one.
                    number = self
                        .numbers()?
                        .last()
                        .map_or(1, |last| last + 1)
                        .max(number + 1);
                }
                Err(error) => return Err(OpLogError::io("publish the entry", &path)(error)),
            }
        }
        Err(OpLogError::Exhausted)
    }
}

/// Reads the name of a file of the log: exactly [`NUMBER_WIDTH`] decimal digits, from 1.
fn parse_file_name(name: &str) -> Option<u64> {
    if name.len() != NUMBER_WIDTH || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok().filter(|number| *number >= 1)
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;
    use crate::testutil::TempDir;

    const A: &str = "b3:1d719c9e1d926eb0f33467406bf50911eca1d59f976105abd3eb5ccfaabdeed9";
    const B: &str = "b3:4feb2d73a2c706e33c2a71f6f03cbfb48f0a20b48567936fca293159e7e3b32c";
    const C: &str = "b3:8cb48a697fe25a78eeb950e011e7dc9897e8216d13e21f3210db70ce2d9db3e8";
    const TIME: i64 = 1_700_000_000;

    fn commit(text: &str) -> CommitId {
        CommitId::new(text.parse().expect("test ID must be valid"))
    }

    fn branch(name: &str) -> BranchName {
        BranchName::new(name).expect("valid branch name")
    }

    fn human() -> Actor {
        Actor::human("daniel").expect("valid actor")
    }

    fn operation(name: &str) -> OperationName {
        OperationName::new(name).expect("valid operation name")
    }

    fn moved(old: Option<&str>, new: Option<&str>) -> Moved<CommitId> {
        Moved::from_to(old.map(commit), new.map(commit))
    }

    /// The change of the golden vector below.
    fn sample_change() -> Change {
        Change::new(
            TIME,
            human(),
            operation("snapshot"),
            Some(Moved::from_to(
                Some(Head::Branch(branch("main"))),
                Some(Head::Detached(commit(A))),
            )),
            Some(moved(Some(A), Some(B))),
            vec![
                (branch("main"), moved(Some(C), Some(A))),
                (branch("feat/x"), moved(None, Some(B))),
            ],
        )
        .expect("valid change")
    }

    fn sample_entry() -> Entry {
        Entry::new(42, sample_change()).expect("valid entry")
    }

    fn golden() -> String {
        format!(
            "stemma-oplog 1\nnumber 42\ntime 1700000000\nactor human daniel\noperation snapshot\n\
             head branch:main commit:{A}\nworking-copy {A} {B}\nbranch feat/x - {B}\n\
             branch main {C} {A}\n"
        )
    }

    // The operation name

    #[test]
    fn an_operation_name_is_a_lowercase_word() {
        for good in ["init", "snapshot", "a", "op-2", "x9-y", &"a".repeat(32)] {
            assert_eq!(OperationName::new(good).unwrap().as_str(), good);
            assert_eq!(good.parse::<OperationName>().unwrap().to_string(), good);
        }
        let cases = [
            ("", OperationNameError::Empty),
            ("1a", OperationNameError::BadStart),
            ("-a", OperationNameError::BadStart),
            ("Init", OperationNameError::BadStart),
            ("\u{e9}a", OperationNameError::BadStart),
            ("aB", OperationNameError::BadCharacter(1)),
            ("a b", OperationNameError::BadCharacter(1)),
            ("a_b", OperationNameError::BadCharacter(1)),
            ("a\n", OperationNameError::BadCharacter(1)),
            ("a\u{e9}", OperationNameError::BadCharacter(1)),
        ];
        for (bad, expected) in cases {
            assert_eq!(OperationName::new(bad), Err(expected), "{bad:?}");
        }
        assert_eq!(
            OperationName::new("a".repeat(33)),
            Err(OperationNameError::TooLong(33))
        );
    }

    // Changes

    #[test]
    fn a_change_needs_a_pointer_that_differs() {
        let build = |head, working_copy, branches| {
            Change::new(TIME, human(), operation("x"), head, working_copy, branches)
        };
        assert_eq!(build(None, None, vec![]), Err(EntryError::NoChange));
        assert_eq!(
            build(Some(Moved::from_to(None, None)), None, vec![]),
            Err(EntryError::Unchanged(Pointer::Head))
        );
        assert_eq!(
            build(None, Some(moved(Some(A), Some(A))), vec![]),
            Err(EntryError::Unchanged(Pointer::WorkingCopy))
        );
        assert_eq!(
            build(None, None, vec![(branch("b"), moved(Some(A), Some(A)))]),
            Err(EntryError::Unchanged(Pointer::Branch(branch("b"))))
        );
        assert_eq!(
            build(
                None,
                None,
                vec![
                    (branch("b"), moved(None, Some(A))),
                    (branch("b"), moved(None, Some(B)))
                ]
            ),
            Err(EntryError::DuplicateBranch(branch("b")))
        );
    }

    #[test]
    fn the_branches_of_a_change_are_sorted_whatever_the_input_order() {
        let change = sample_change();
        let names: Vec<&str> = change.branches().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["feat/x", "main"]);
    }

    #[test]
    fn the_number_of_an_entry_is_from_1_to_the_maximum() {
        assert_eq!(
            Entry::new(0, sample_change()),
            Err(EntryError::NumberOutOfRange(0))
        );
        assert_eq!(
            Entry::new(MAX_NUMBER + 1, sample_change()),
            Err(EntryError::NumberOutOfRange(MAX_NUMBER + 1))
        );
        assert!(Entry::new(1, sample_change()).is_ok());
        assert!(Entry::new(MAX_NUMBER, sample_change()).is_ok());
    }

    #[test]
    fn a_change_names_every_commit_it_made_unreachable_or_reachable() {
        let commits = sample_change().commits();
        for id in [A, B, C] {
            assert!(commits.contains(&commit(id)), "{id}");
        }
        let only_names = Change::new(
            TIME,
            human(),
            operation("x"),
            Some(Moved::from_to(None, Some(Head::Branch(branch("main"))))),
            None,
            vec![],
        )
        .unwrap();
        assert!(only_names.commits().is_empty());
    }

    // The encoding

    #[test]
    fn an_entry_is_written_as_the_golden_encoding() {
        assert_eq!(sample_entry().encode(), golden().into_bytes());
    }

    #[test]
    fn an_entry_is_read_back_from_its_encoding() {
        let entry = sample_entry();
        let read = Entry::parse(&entry.encode()).unwrap();
        assert_eq!(read, entry);
        assert_eq!(read.number(), 42);
        assert_eq!(read.change().operation().as_str(), "snapshot");
        assert_eq!(read.change().time(), TIME);
        assert_eq!(read.change().actor(), &human());
        let head = read.change().head().unwrap();
        assert_eq!(head.before(), Some(&Head::Branch(branch("main"))));
        assert_eq!(head.after(), Some(&Head::Detached(commit(A))));
        assert_eq!(
            read.change().working_copy().unwrap().after(),
            Some(&commit(B))
        );
    }

    #[test]
    fn the_smallest_and_the_largest_entries_are_read_back() {
        let smallest = Entry::new(
            1,
            Change::new(
                i64::MIN,
                human(),
                operation("init"),
                Some(Moved::from_to(None, Some(Head::Branch(branch("main"))))),
                None,
                vec![],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            smallest.encode(),
            b"stemma-oplog 1\nnumber 1\ntime -9223372036854775808\nactor human daniel\n\
              operation init\nhead - branch:main\n"
        );
        assert_eq!(Entry::parse(&smallest.encode()).unwrap(), smallest);
        let agent = Actor::agent("a1", "m=1", "s", "daniel").unwrap();
        let largest = Entry::new(
            MAX_NUMBER,
            Change::new(
                i64::MAX,
                agent,
                operation("op-9"),
                None,
                Some(moved(Some(A), None)),
                vec![(branch("z"), moved(Some(B), None))],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(Entry::parse(&largest.encode()).unwrap(), largest);
    }

    fn with(entry: &str, from: &str, to: &str) -> Vec<u8> {
        assert!(entry.contains(from), "{from:?} is not in the entry");
        entry.replacen(from, to, 1).into_bytes()
    }

    fn parse_error(bytes: impl AsRef<[u8]>) -> EntryParseError {
        Entry::parse(bytes.as_ref()).unwrap_err()
    }

    fn malformed(line: usize, reason: EntryMalformed) -> EntryParseError {
        EntryParseError::Malformed { line, reason }
    }

    #[test]
    fn the_reader_reports_the_shape_of_a_bad_entry_at_its_line() {
        let text = golden();
        assert_eq!(
            parse_error(b""),
            malformed(1, EntryMalformed::ExpectedHeader)
        );
        assert_eq!(
            parse_error(with(&text, "stemma-oplog 1\n", "stemma-oplog\n")),
            malformed(1, EntryMalformed::ExpectedHeader)
        );
        assert_eq!(
            parse_error(with(&text, "stemma-oplog 1", "Stemma-oplog 1")),
            malformed(1, EntryMalformed::ExpectedHeader)
        );
        assert_eq!(
            parse_error(with(&text, "stemma-oplog 1", "stemma-oplog 1x")),
            malformed(1, EntryMalformed::ExpectedHeader)
        );
        assert_eq!(
            parse_error(with(&text, "number 42\n", "")),
            malformed(2, EntryMalformed::ExpectedNumber)
        );
        assert_eq!(
            parse_error(with(&text, "time 1700000000\n", "")),
            malformed(3, EntryMalformed::ExpectedTime)
        );
        assert_eq!(
            parse_error(with(&text, "actor human daniel\n", "")),
            malformed(4, EntryMalformed::ExpectedActor)
        );
        assert_eq!(
            parse_error(with(&text, "operation snapshot\n", "")),
            malformed(5, EntryMalformed::ExpectedOperation)
        );
        // The header lines out of order, repeated, or with another spacing.
        assert_eq!(
            parse_error(with(
                &text,
                "number 42\ntime 1700000000\n",
                "time 1700000000\nnumber 42\n"
            )),
            malformed(2, EntryMalformed::ExpectedNumber)
        );
        assert_eq!(
            parse_error(with(&text, "number 42\n", "number 42\nnumber 42\n")),
            malformed(3, EntryMalformed::ExpectedTime)
        );
        assert_eq!(
            parse_error(with(&text, "number 42", "number  42")),
            EntryParseError::InvalidNumber {
                line: 2,
                reason: NumberError::NotCanonical
            }
        );
    }

    #[test]
    fn a_cut_off_entry_is_reported_as_cut_off_at_the_line_it_ends() {
        let text = golden();
        assert_eq!(
            parse_error(text.trim_end()),
            malformed(9, EntryMalformed::CutOff)
        );
        assert_eq!(
            parse_error("stemma-oplog 1"),
            malformed(1, EntryMalformed::CutOff)
        );
        assert_eq!(
            parse_error("stemma-oplog 1\nnumber 4"),
            malformed(2, EntryMalformed::CutOff)
        );
        // An entry that ends after its header lines has no pointer, which is another error.
        assert_eq!(
            parse_error("stemma-oplog 1\nnumber 1\ntime 1\nactor human d\noperation x\n"),
            malformed(6, EntryMalformed::ExpectedPointer)
        );
        // Text after the last pointer is another pointer line, or unknown.
        assert_eq!(
            parse_error(format!("{text}\n")),
            malformed(10, EntryMalformed::UnknownPointer)
        );
        assert_eq!(
            parse_error(format!("{text}extra\n")),
            malformed(10, EntryMalformed::UnknownPointer)
        );
    }

    #[test]
    fn a_version_other_than_1_is_unsupported_and_not_malformed() {
        let text = golden();
        for version in ["0", "2", "10", "99999999999999999999"] {
            assert_eq!(
                parse_error(with(
                    &text,
                    "stemma-oplog 1",
                    &format!("stemma-oplog {version}")
                )),
                EntryParseError::UnsupportedVersion(version.to_owned()),
                "{version}"
            );
        }
        let message = parse_error(with(&text, "stemma-oplog 1", "stemma-oplog 2")).to_string();
        assert!(
            message.contains("version 2") && message.contains("version 1"),
            "{message}"
        );
    }

    #[test]
    fn the_reader_rejects_a_number_that_is_not_canonical() {
        let text = golden();
        for (number, reason) in [
            ("", NumberError::NotCanonical),
            ("0", NumberError::NotCanonical),
            ("00", NumberError::NotCanonical),
            ("042", NumberError::NotCanonical),
            ("+42", NumberError::NotCanonical),
            ("-1", NumberError::NotCanonical),
            ("4 2", NumberError::NotCanonical),
            ("\u{664}", NumberError::NotCanonical),
            ("1000000000000", NumberError::OutOfRange),
            ("18446744073709551616", NumberError::OutOfRange),
        ] {
            assert_eq!(
                parse_error(with(&text, "number 42", &format!("number {number}"))),
                EntryParseError::InvalidNumber { line: 2, reason },
                "{number:?}"
            );
        }
        for good in ["1", "999999999999"] {
            assert!(Entry::parse(&with(&text, "number 42", &format!("number {good}"))).is_ok());
        }
    }

    #[test]
    fn the_reader_rejects_a_time_an_actor_and_an_operation_that_are_not_canonical() {
        let text = golden();
        for time in ["", "+1", "-0", "01", "1.5", " 1", "9223372036854775808"] {
            assert!(
                matches!(
                    parse_error(with(&text, "time 1700000000", &format!("time {time}"))),
                    EntryParseError::InvalidTime { line: 3, .. }
                ),
                "{time:?}"
            );
        }
        for actor in [
            "",
            "human",
            "human a b",
            "robot x",
            "agent a",
            "human d\u{e4}nu",
        ] {
            assert!(
                matches!(
                    parse_error(with(&text, "actor human daniel", &format!("actor {actor}"))),
                    EntryParseError::InvalidActor { line: 4, .. }
                ),
                "{actor:?}"
            );
        }
        for operation in ["", "Snapshot", "1x", "a_b", "a b"] {
            assert!(
                matches!(
                    parse_error(with(
                        &text,
                        "operation snapshot",
                        &format!("operation {operation}")
                    )),
                    EntryParseError::InvalidOperation { line: 5, .. }
                ),
                "{operation:?}"
            );
        }
    }

    #[test]
    fn the_pointers_come_in_a_fixed_order_and_each_once() {
        let h = "head - branch:main\n".to_string();
        let w = format!("working-copy {A} {B}\n");
        let b1 = format!("branch a {A} {B}\n");
        let b2 = format!("branch b {A} {B}\n");
        let prefix = "stemma-oplog 1\nnumber 1\ntime 1\nactor human d\noperation x\n";
        let at = |body: String| parse_error(format!("{prefix}{body}"));
        assert!(Entry::parse(format!("{prefix}{h}{w}{b1}{b2}").as_bytes()).is_ok());
        assert!(Entry::parse(format!("{prefix}{w}{b1}").as_bytes()).is_ok());
        assert!(Entry::parse(format!("{prefix}{b1}").as_bytes()).is_ok());
        // Out of order, and twice.
        assert_eq!(
            at(format!("{w}{h}")),
            malformed(7, EntryMalformed::PointerOutOfOrder)
        );
        assert_eq!(
            at(format!("{b1}{h}")),
            malformed(7, EntryMalformed::PointerOutOfOrder)
        );
        assert_eq!(
            at(format!("{b1}{w}")),
            malformed(7, EntryMalformed::PointerOutOfOrder)
        );
        assert_eq!(
            at(format!("{h}{h}")),
            malformed(7, EntryMalformed::PointerOutOfOrder)
        );
        assert_eq!(
            at(format!("{w}{w}")),
            malformed(7, EntryMalformed::PointerOutOfOrder)
        );
        // Branches are strictly ascending by the bytes of the name.
        assert_eq!(
            at(format!("{b2}{b1}")),
            EntryParseError::OutOfOrder {
                line: 7,
                name: "a".into()
            }
        );
        assert_eq!(
            at(format!("{b1}{b1}")),
            EntryParseError::OutOfOrder {
                line: 7,
                name: "a".into()
            }
        );
        // The wrong number of values, and unknown lines.
        assert_eq!(
            at(format!("working-copy {A}\n")),
            malformed(6, EntryMalformed::WrongValueCount)
        );
        assert_eq!(
            at(format!("branch a {A}\n")),
            malformed(6, EntryMalformed::WrongValueCount)
        );
        assert_eq!(
            at(format!("branch a {A} {B} {C}\n")),
            malformed(6, EntryMalformed::WrongValueCount)
        );
        assert_eq!(
            at("head - -  \n".to_string()),
            malformed(6, EntryMalformed::WrongValueCount)
        );
        assert_eq!(
            at("tag a - -\n".to_owned()),
            malformed(6, EntryMalformed::UnknownPointer)
        );
        assert_eq!(
            at("\n".to_owned()),
            malformed(6, EntryMalformed::UnknownPointer)
        );
        assert_eq!(
            at(format!("Branch a {A} {B}\n")),
            malformed(6, EntryMalformed::UnknownPointer)
        );
    }

    #[test]
    fn the_reader_rejects_values_that_are_not_canonical() {
        let prefix = "stemma-oplog 1\nnumber 1\ntime 1\nactor human d\noperation x\n";
        let at = |body: String| parse_error(format!("{prefix}{body}"));
        // A commit ID, in every pointer line.
        for body in [
            "working-copy b3:00 -\n".to_owned(),
            "working-copy - B3:00\n".to_owned(),
            format!("working-copy - {}\n", A.to_uppercase()),
            format!("branch a - {}\n", &A[..A.len() - 1]),
            "branch a b3:zz -\n".to_owned(),
        ] {
            assert!(
                matches!(
                    at(body.clone()),
                    EntryParseError::InvalidObjectId { line: 6, .. }
                ),
                "{body:?}"
            );
        }
        // A branch name.
        for name in ["", "a b", "/a", "a/", ".a", "A\u{e9}"] {
            let body = format!("branch {name} - {A}\n");
            assert!(
                matches!(
                    at(body.clone()),
                    EntryParseError::InvalidBranchName { line: 6, .. }
                        | EntryParseError::Malformed { line: 6, .. }
                ),
                "{body:?}"
            );
        }
        // A value of HEAD.
        for value in [
            "main",
            "branch:",
            "branch:a b",
            "commit:b3:00",
            "Branch:main",
            "tag:x",
            ":",
            "branch",
            "commit",
        ] {
            let body = format!("head {value} -\n");
            assert!(
                matches!(
                    at(body.clone()),
                    EntryParseError::InvalidBranchName { line: 6, .. }
                        | EntryParseError::InvalidObjectId { line: 6, .. }
                        | EntryParseError::Malformed { line: 6, .. }
                ),
                "{body:?}"
            );
        }
        // Old and new are equal.
        for body in [
            format!("working-copy {A} {A}\n"),
            "working-copy - -\n".to_owned(),
            format!("branch a {B} {B}\n"),
            "head - -\n".to_owned(),
            "head branch:a branch:a\n".to_owned(),
        ] {
            assert_eq!(
                at(body.clone()),
                EntryParseError::Unchanged { line: 6 },
                "{body:?}"
            );
        }
        // Not UTF-8: reported before anything else.
        assert_eq!(
            parse_error(b"stemma-oplog 1\n\xff"),
            EntryParseError::NotUtf8 { valid_up_to: 15 }
        );
    }

    #[test]
    fn the_reader_accepts_no_second_spelling() {
        // Every single change of one byte that the reader still accepts has to encode back to the
        // changed bytes, or the entry has two encodings.
        let golden = golden().into_bytes();
        let mut accepted = 0;
        for at in 0..golden.len() {
            for &replacement in b"0aA \n-+:" {
                let mut bytes = golden.clone();
                bytes[at] = replacement;
                if let Ok(entry) = Entry::parse(&bytes) {
                    accepted += 1;
                    assert_eq!(entry.encode(), bytes, "byte {at} to {replacement:?}");
                }
            }
        }
        assert!(accepted > 0, "the check reached no valid entry");
    }

    #[test]
    fn error_messages_are_complete_and_name_the_line() {
        let text = golden();
        let message = parse_error(with(&text, "time 1700000000", "time 01")).to_string();
        assert_eq!(
            message,
            "line 3: the time is not a canonical decimal integer of seconds"
        );
        assert!(Error::source(&parse_error(with(&text, "time 1700000000", "time 01"))).is_none());
        let message = parse_error(with(&text, "number 42", "number 0")).to_string();
        assert!(message.starts_with("line 2: the number"), "{message}");
        let message =
            parse_error(format!("{}branch a {A} {B}\n", &text[..text.len() - 1])).to_string();
        assert!(message.starts_with("line "), "{message}");
        assert_eq!(
            EntryError::Unchanged(Pointer::Branch(branch("main"))).to_string(),
            "the branch `main` has the same old and new value"
        );
        assert_eq!(
            EntryError::NoChange.to_string(),
            "the operation changes no pointer"
        );
    }

    // The log on disk

    fn log_in(dir: &TempDir) -> FsOpLog {
        fs::create_dir(dir.path().join("oplog")).unwrap();
        fs::create_dir(dir.path().join("tmp")).unwrap();
        FsOpLog::new(dir.path())
    }

    fn change(branch_name: &str, old: Option<&str>, new: &str) -> Change {
        Change::new(
            TIME,
            human(),
            operation("commit"),
            None,
            None,
            vec![(branch(branch_name), moved(old, Some(new)))],
        )
        .unwrap()
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn entries_get_the_next_numbers_and_are_read_back() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        assert_eq!(log.numbers().unwrap(), Vec::<u64>::new());
        let first = log.append(&change("main", None, A)).unwrap();
        let second = log.append(&change("main", Some(A), B)).unwrap();
        assert_eq!((first.number(), second.number()), (1, 2));
        assert_eq!(log.numbers().unwrap(), [1, 2]);
        assert_eq!(log.read(2).unwrap(), second);
        assert_eq!(log.entries().unwrap(), [first, second]);
        assert_eq!(
            files(&dir.path().join("oplog")),
            ["000000000001", "000000000002"]
        );
        // The temporary files are gone, and the content is the canonical encoding.
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        assert_eq!(
            fs::read(dir.path().join("oplog").join("000000000002")).unwrap(),
            log.read(2).unwrap().encode()
        );
    }

    #[test]
    fn a_missing_entry_is_reported_by_its_number() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        assert!(matches!(
            log.read(1),
            Err(OpLogError::Missing { number: 1 })
        ));
        let message = log.read(7).unwrap_err().to_string();
        assert!(message.contains('7'), "{message}");
    }

    #[test]
    fn a_gap_in_the_numbers_is_reported_and_not_skipped() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        for n in 1..=3 {
            log.append(&change("main", None, if n % 2 == 0 { A } else { B }))
                .unwrap();
        }
        fs::remove_file(dir.path().join("oplog").join("000000000002")).unwrap();
        assert!(matches!(log.numbers(), Err(OpLogError::Gap { missing: 2 })));
        assert!(matches!(log.entries(), Err(OpLogError::Gap { missing: 2 })));
        // An append does not hide the gap either.
        assert!(matches!(
            log.append(&change("main", None, A)),
            Err(OpLogError::Gap { missing: 2 })
        ));
        let message = log.numbers().unwrap_err().to_string();
        assert!(
            message.contains("entry 2") && message.contains("lost"),
            "{message}"
        );
    }

    #[test]
    fn the_log_holds_nothing_but_entries() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        for name in [
            "000000000002.tmp",
            "1",
            "00000000000a",
            "0000000000000",
            "000000000000",
            ".hidden",
        ] {
            let path = dir.path().join("oplog").join(name);
            fs::write(&path, b"x").unwrap();
            assert!(
                matches!(log.numbers(), Err(OpLogError::UnexpectedFile { .. })),
                "{name}"
            );
            fs::remove_file(path).unwrap();
        }
        fs::create_dir(dir.path().join("oplog").join("000000000002")).unwrap();
        assert!(log.read(2).is_err());
    }

    #[test]
    fn a_damaged_entry_is_reported_with_its_file_and_does_not_touch_the_others() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        log.append(&change("main", Some(A), B)).unwrap();
        let path = dir.path().join("oplog").join("000000000002");
        let mut bytes = fs::read(&path).unwrap();
        bytes.pop();
        fs::remove_file(&path).unwrap();
        fs::write(&path, &bytes).unwrap();
        let error = log.read(2).unwrap_err();
        assert!(
            matches!(
                &error,
                OpLogError::Corrupt {
                    source: EntryParseError::Malformed {
                        reason: EntryMalformed::CutOff,
                        ..
                    },
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.to_string().contains("000000000002"), "{error}");
        assert!(log.read(1).is_ok());
    }

    #[test]
    fn a_number_inside_that_differs_from_the_name_is_reported() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        let text = fs::read_to_string(dir.path().join("oplog").join("000000000001")).unwrap();
        let moved_to = dir.path().join("oplog").join("000000000001");
        fs::remove_file(&moved_to).unwrap();
        fs::write(&moved_to, text.replace("number 1", "number 5")).unwrap();
        assert!(matches!(
            log.read(1),
            Err(OpLogError::WrongNumber {
                expected: 1,
                found: 5,
                ..
            })
        ));
    }

    #[test]
    fn an_append_without_a_directory_fails_and_leaves_nothing() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("tmp")).unwrap();
        let error = FsOpLog::new(dir.path())
            .append(&change("main", None, A))
            .unwrap_err();
        assert!(matches!(error, OpLogError::Io { .. }), "{error}");
        assert!(error.io_error().is_some());
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        // And without the directory for temporary files.
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("oplog")).unwrap();
        let error = FsOpLog::new(dir.path())
            .append(&change("main", None, A))
            .unwrap_err();
        assert!(error.io_error().is_some(), "{error}");
        assert_eq!(files(&dir.path().join("oplog")), Vec::<String>::new());
    }

    #[test]
    fn writers_at_the_same_moment_get_different_numbers_without_a_gap() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let barrier = Barrier::new(8);
        let numbers: Vec<Vec<u64>> = thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|writer| {
                    let (log, barrier) = (&log, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        (0..5)
                            .map(|round| {
                                let name = format!("w{writer}-{round}");
                                log.append(&change(&name, None, A)).unwrap().number()
                            })
                            .collect()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut all: Vec<u64> = numbers.into_iter().flatten().collect();
        all.sort_unstable();
        assert_eq!(all, (1..=40).collect::<Vec<u64>>());
        assert_eq!(log.numbers().unwrap(), (1..=40).collect::<Vec<u64>>());
        // Every entry is whole and holds the writer that made it.
        let mut names: Vec<String> = log
            .entries()
            .unwrap()
            .iter()
            .map(|e| e.change().branches()[0].0.to_string())
            .collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 40);
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    #[test]
    fn the_last_number_is_the_largest_and_the_log_is_then_full() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        // A log that holds only the last number is not gap free, so it is reported as such; the
        // limit is checked on the number that an append would take.
        assert_eq!(
            Entry::new(MAX_NUMBER + 1, change("a", None, A)).unwrap_err(),
            EntryError::NumberOutOfRange(MAX_NUMBER + 1)
        );
        assert_eq!(parse_file_name("999999999999"), Some(MAX_NUMBER));
        assert_eq!(parse_file_name("000000000001"), Some(1));
        assert_eq!(parse_file_name("000000000000"), None);
        assert_eq!(parse_file_name("1000000000000"), None);
        let _ = log;
    }

    #[test]
    fn errors_of_the_log_follow_the_convention_for_messages_and_causes() {
        #[derive(Debug)]
        struct Outer;
        impl fmt::Display for Outer {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("outer")
            }
        }
        impl Error for Outer {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&Deepest)
            }
        }
        #[derive(Debug)]
        struct Deepest;
        impl fmt::Display for Deepest {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("deepest")
            }
        }
        impl Error for Deepest {}
        let path = PathBuf::from("/x/.stemma/oplog/000000000001");
        let io_error = OpLogError::Io {
            operation: "read the entry",
            path: path.clone(),
            source: io::Error::other(Outer),
        };
        assert_eq!(
            io_error.to_string(),
            "could not read the entry `/x/.stemma/oplog/000000000001`: outer"
        );
        assert_eq!(io_error.source().unwrap().to_string(), "deepest");
        assert_eq!(io_error.io_error().unwrap().to_string(), "outer");
        let not_flushed = OpLogError::NotFlushed {
            path: PathBuf::from("/x/oplog"),
            source: io::Error::other(Outer),
        };
        let message = not_flushed.to_string();
        assert!(
            message.contains("flushing `/x/oplog` failed: outer"),
            "{message}"
        );
        assert!(message.contains("may not survive"), "{message}");
        assert!(not_flushed.io_error().is_some());
        for (error, text) in [
            (OpLogError::Missing { number: 3 }, "no entry 3"),
            (OpLogError::Exhausted, "full"),
            (OpLogError::Gap { missing: 2 }, "lost"),
            (
                OpLogError::UnexpectedFile { path: path.clone() },
                "is not an entry",
            ),
            (
                OpLogError::WrongNumber {
                    path: path.clone(),
                    expected: 1,
                    found: 2,
                },
                "number 2 inside",
            ),
            (OpLogError::Entry(EntryError::NoChange), "no pointer"),
        ] {
            let message = error.to_string();
            assert!(message.contains(text), "{message}");
            assert!(error.source().is_none());
            assert!(error.io_error().is_none());
        }
        let corrupt = OpLogError::Corrupt {
            path,
            source: EntryParseError::Unchanged { line: 6 },
        };
        assert!(corrupt.to_string().contains("line 6"), "{corrupt}");
    }
}
