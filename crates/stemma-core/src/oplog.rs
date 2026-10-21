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
//! move can still fail, because the file system fails between the entry and the move. Nothing is
//! written after the move, so nothing can be half written; [`outcomes`] reads from the chain of
//! the entries of a pointer whether a move happened (ADR-011).
//!
//! # The encoding
//!
//! Version 1 of the entry encoding is strict text. A reader accepts what [`Entry::encode`] writes
//! and nothing else, so one entry has exactly one encoding (ADR-005):
//!
//! ```text
//! stemma-oplog 1
//! number 42
//! time 1700000000
//! actor human daniel
//! operation snapshot
//! head branch:main commit:b3:0eec…6eb6
//! working-copy b3:1d71…eed9 b3:4feb…b32c
//! branch main b3:8cb4…b3e8 b3:b5f2…e662
//! ```
//!
//! The `…` stands for digits that are left out here; a real ID has all 64. The first five lines come
//! in this order, each once. Then come the lines of the pointers that
//! changed, at least one: `head`, then `working-copy`, then one `branch <name>` for each branch,
//! sorted by name in UTF-8 byte order and each name once. Each has the old and the new value, which
//! differ. `-` means that the pointer did not exist, or does not exist any more. A branch and the
//! working-copy commit have a commit ID or `-`; HEAD has `branch:<name>`, `commit:<id>`, or `-`.
//! The file ends with a newline and has nothing after the last pointer.
//!
//! # Stubs and purges (version 2)
//!
//! A purge (ADR-014) replaces every entry that names a commit it rewrote by a *stub* with the same
//! number, and writes an entry of its own. Both have version 2 of the encoding, and nothing else
//! has, so every entry still has one encoding:
//!
//! ```text
//! stemma-oplog 2              stemma-oplog 2
//! number 5                    number 9
//! time 1700000000             time 1700000500
//! actor human daniel          actor human daniel
//! stub                        operation purge
//!                             commits 3
//!                             stubs 4-5 7
//! ```
//!
//! A stub keeps the number, the time, and the actor, and drops the operation and every ID. The
//! `purge` entry names no pointer and no ID: the count of rewritten commits, and the numbers of the
//! stubs as ranges in ascending order that neither overlap nor touch, all below its own number, or
//! `-` for none. A list that does not fit into one entry is split over several, and the first has
//! the count. A stub that no `purge` entry lists is an error of the log ([`check_stubs`]); a
//! listed entry that is not a stub yet is not, as the purge writes its entry first. A change
//! cannot have the operation `purge`. The stub keeps no hash, as the log has no chain yet
//! (STEMMA-41).
//!
//! # Numbers and writers
//!
//! A writer takes the next number that is free and publishes the file with a hard link, which
//! fails if the number is taken, so the entry is whole from the first moment it can be seen. If
//! the number is taken, another writer was faster; the writer then looks for the end of the log
//! again and tries the next free number. [`Repository`](crate::repository::Repository) writes
//! under the lock of the references, so its writers do not meet here; the retry is for any other
//! writer.
//!
//! The numbers have no gap: a gap means that an entry is lost, and the reader reports it. A
//! directory listing is not a snapshot while a writer adds files, so the reader lists the directory
//! again before it reports a gap, and a writer that only needs the end of the log looks at the
//! largest number and at the tip file (below). Names that start with a dot are skipped, as in the
//! references, because file managers and editors leave such files; any other name is reported.
//!
//! Only the repository writes. `FsOpLog::append` and `Entry::new` are visible inside this crate
//! only, so no caller can add an entry past the authorizer or invent one that was never in the log.
//!
//! A gap does not stop a writer: the reader reports it, and a refusal would block the move that
//! repairs the log. A lost *last* entry leaves no gap, so the file `oplog-tip` next to `oplog/`
//! holds the number of the last entry. The writer replaces it after the entry is published. A tip
//! beyond the largest number in the directory means that the last entries are lost: the writer
//! refuses ([`OpLogError::TipAhead`]) and the reader reports it, as the number would go to another
//! move otherwise. A tip one behind or a missing file is no fault. The tip guards against
//! accidents and crashes, not against an attack: who removes the tip and the entry is not noticed
//! (STEMMA-41, ADR-011).
//!
//! `tmp/` holds scratch files. A temporary name is removed after the entry is published, on a best
//! effort basis: a name that stays is harmless and costs only space, and no operation of this slice
//! empties `tmp/` (ADR-011).
//!
//! An entry is at most 64 KiB. The writer refuses a larger one before it writes anything, so the
//! reader never meets a file that the writer made and cannot read; the actor and the names in it
//! have no limit of their own.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::object::{parse_seconds, CommitId, ObjectId, ObjectIdParseError, TimeError};
use crate::refs::{BranchName, BranchNameError, Head};
use crate::store::{create_temp, sync_directory, unique_temp_name, StoreError};
use crate::{Actor, ActorParseError};

/// The first line of an entry of a change, with the version of the encoding.
const HEADER: &str = "stemma-oplog 1";

/// The first line of a stub and of a `purge` entry (ADR-014). Version 2 is written only for these
/// two, so that every entry has exactly one encoding.
const HEADER_2: &str = "stemma-oplog 2";

/// The name of the operation of a purge. A change cannot have it: a purge entry names no pointer
/// (ADR-014).
pub const PURGE: &str = "purge";

/// The start of the first line of every version of the encoding.
const HEADER_PREFIX: &str = "stemma-oplog ";

/// The number of digits of a file name.
const NUMBER_WIDTH: usize = 12;

/// The largest number of an entry: the largest value of twelve decimal digits.
pub const MAX_NUMBER: u64 = 999_999_999_999;

/// The largest size of an entry that is read, in bytes. An entry is a few hundred bytes; a file much
/// larger is not one.
const MAX_ENTRY_LEN: u64 = 64 * 1024;

/// How many numbers a writer tries before it gives up. A writer tries the next number only because
/// another writer took the one before, which means that the other one made progress, so the limit is
/// reached only by a writer that is overtaken again and again. Any other failure returns at once.
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
    /// The name is longer than 32 bytes, which is 32 characters for a name that is valid. Carries
    /// its length in bytes. The length is checked before the characters, so a long name with a
    /// character that is not allowed is reported as too long.
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
                "the name of an operation has at most {MAX_OPERATION_LEN} bytes, which is {MAX_OPERATION_LEN} \
                 characters of a valid name, not {length} bytes"
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
    /// A change has the operation `purge`, which only the entry of a purge has.
    ReservedOperation,
    /// A `purge` entry lists a stub whose number is not below its own: the purge entry comes after
    /// the entries that it replaces (ADR-014). Carries the largest number it lists.
    StubNotBefore(u64),
    /// The entry of a purge cannot become a stub. Carries its number.
    PurgeNotReplaceable(u64),
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
            Self::ReservedOperation => write!(
                f,
                "the operation `{PURGE}` has an entry of its own kind and moves no pointer in it"
            ),
            Self::StubNotBefore(number) => write!(
                f,
                "the purge lists entry {number} as a stub, which does not come before it"
            ),
            Self::PurgeNotReplaceable(number) => {
                write!(
                    f,
                    "entry {number} is the entry of a purge and cannot become a stub"
                )
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
    /// Returns [`EntryError::ReservedOperation`] for the operation `purge`, which has an entry of
    /// its own kind, [`EntryError::NoChange`] if no pointer is given, [`EntryError::Unchanged`]
    /// for a pointer whose old and new value are equal, and [`EntryError::DuplicateBranch`] for a
    /// branch that is listed twice.
    pub fn new(
        time: i64,
        actor: Actor,
        operation: OperationName,
        head: Option<Moved<Head>>,
        working_copy: Option<Moved<CommitId>>,
        mut branches: Vec<(BranchName, Moved<CommitId>)>,
    ) -> Result<Self, EntryError> {
        if operation.as_str() == PURGE {
            return Err(EntryError::ReservedOperation);
        }
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
    /// needs from this entry (ADR-007, ADR-011). A commit that is named twice is listed twice.
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

/// What remains of an entry that a purge replaced (ADR-014): its number, its time, and its actor.
/// The operation and every ID are gone, so the entry cannot be restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stub {
    time: i64,
    actor: Actor,
}

impl Stub {
    /// The time of the entry that the stub replaced.
    pub fn time(&self) -> i64 {
        self.time
    }

    /// The actor of the entry that the stub replaced.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }
}

/// The entry of a purge (ADR-014): who purged, when, how many commits were rewritten, and the
/// numbers of the entries that became stubs. It names no pointer and no ID, as the audit must not
/// keep what the purge removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Purge {
    time: i64,
    actor: Actor,
    commits: u64,
    stubs: Vec<(u64, u64)>,
}

impl Purge {
    /// Builds the record of a purge. `stubs` are the numbers of the entries that become stubs, in
    /// any order; they are kept as ranges.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the purge of STEMMA-48 calls it")
    )]
    pub(crate) fn new(time: i64, actor: Actor, commits: u64, stubs: &[u64]) -> Self {
        let mut numbers = stubs.to_vec();
        numbers.sort_unstable();
        numbers.dedup();
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for number in numbers {
            match ranges.last_mut() {
                Some((_, end)) if *end + 1 == number => *end = number,
                _ => ranges.push((number, number)),
            }
        }
        Self {
            time,
            actor,
            commits,
            stubs: ranges,
        }
    }

    /// The time that the writer claims.
    pub fn time(&self) -> i64 {
        self.time
    }

    /// Who purged.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// How many commits the purge rewrote. A purge whose list of stubs did not fit into one entry
    /// writes several; the first has the count and the others 0.
    pub fn commits(&self) -> u64 {
        self.commits
    }

    /// The numbers of the entries that became stubs, as inclusive ranges in ascending order. Two
    /// ranges neither overlap nor touch.
    pub fn stubs(&self) -> &[(u64, u64)] {
        &self.stubs
    }

    /// Whether the purge lists the entry as a stub.
    pub fn lists(&self, number: u64) -> bool {
        self.stubs
            .binary_search_by(|(start, end)| {
                if *end < number {
                    std::cmp::Ordering::Less
                } else if *start > number {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    /// The largest number that it lists, or 0 if it lists none.
    fn last_stub(&self) -> u64 {
        self.stubs.last().map_or(0, |(_, end)| *end)
    }

    /// Splits the record into records whose entries fit [`MAX_ENTRY_LEN`], with the count of
    /// commits in the first. The size is computed for the largest number an entry can have.
    fn split(self) -> Vec<Self> {
        let mut parts = Vec::new();
        let mut current = Self {
            stubs: Vec::new(),
            ..self.clone()
        };
        for range in self.stubs {
            current.stubs.push(range);
            let size = Entry {
                number: MAX_NUMBER,
                record: Record::Purge(current.clone()),
            }
            .encode()
            .len() as u64;
            if size > MAX_ENTRY_LEN && current.stubs.len() > 1 {
                let range = current.stubs.pop().expect("one was pushed above");
                let next = Self {
                    commits: 0,
                    stubs: vec![range],
                    ..current.clone()
                };
                parts.push(std::mem::replace(&mut current, next));
            }
        }
        parts.push(current);
        parts
    }
}

/// What an entry holds.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Record {
    /// An operation that moved pointers.
    Change(Change),
    /// An entry that a purge replaced.
    Stub(Stub),
    /// A purge.
    Purge(Purge),
}

/// An entry of the log: a number and a record, which is a change, a stub, or a purge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    number: u64,
    record: Record,
}

impl Entry {
    /// Builds an entry of a change. Only the log gives a number, so only this crate builds one.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::NumberOutOfRange`] if the number is 0 or larger than [`MAX_NUMBER`].
    pub(crate) fn new(number: u64, change: Change) -> Result<Self, EntryError> {
        Self::of(number, Record::Change(change))
    }

    /// Builds an entry of any record.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::NumberOutOfRange`] if the number is 0 or larger than [`MAX_NUMBER`],
    /// and [`EntryError::StubNotBefore`] if a purge lists its own number or a later one.
    pub(crate) fn of(number: u64, record: Record) -> Result<Self, EntryError> {
        if !(1..=MAX_NUMBER).contains(&number) {
            return Err(EntryError::NumberOutOfRange(number));
        }
        if let Record::Purge(purge) = &record {
            if purge.last_stub() >= number {
                return Err(EntryError::StubNotBefore(purge.last_stub()));
            }
        }
        Ok(Self { number, record })
    }

    /// The number of the entry, which is also the name of its file.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// What the entry holds.
    pub fn record(&self) -> &Record {
        &self.record
    }

    /// What the operation did, or `None` for a stub and a purge.
    pub fn change(&self) -> Option<&Change> {
        match &self.record {
            Record::Change(change) => Some(change),
            Record::Stub(_) | Record::Purge(_) => None,
        }
    }

    /// The time that the writer claims.
    pub fn time(&self) -> i64 {
        match &self.record {
            Record::Change(change) => change.time,
            Record::Stub(stub) => stub.time,
            Record::Purge(purge) => purge.time,
        }
    }

    /// Who wrote the entry.
    pub fn actor(&self) -> &Actor {
        match &self.record {
            Record::Change(change) => &change.actor,
            Record::Stub(stub) => &stub.actor,
            Record::Purge(purge) => &purge.actor,
        }
    }

    /// Every commit that the entry names (see [`Change::commits`]); none for a stub and a purge.
    pub fn commits(&self) -> Vec<CommitId> {
        self.change().map(Change::commits).unwrap_or_default()
    }

    /// The stub that replaces this entry: the same number, time, and actor.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::PurgeNotReplaceable`] for the entry of a purge, which a later purge
    /// keeps as the record of the first.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the purge of STEMMA-48 calls it")
    )]
    pub(crate) fn to_stub(&self) -> Result<Self, EntryError> {
        if matches!(self.record, Record::Purge(_)) {
            return Err(EntryError::PurgeNotReplaceable(self.number));
        }
        Ok(Self {
            number: self.number,
            record: Record::Stub(Stub {
                time: self.time(),
                actor: self.actor().clone(),
            }),
        })
    }

    /// Returns the canonical encoding (see the module documentation).
    pub fn encode(&self) -> Vec<u8> {
        let change = match &self.record {
            Record::Change(change) => change,
            Record::Stub(stub) => {
                return format!(
                    "{HEADER_2}\nnumber {}\ntime {}\nactor {}\nstub\n",
                    self.number, stub.time, stub.actor
                )
                .into_bytes();
            }
            Record::Purge(purge) => {
                let stubs = if purge.stubs.is_empty() {
                    "-".to_owned()
                } else {
                    ranges_text(&purge.stubs)
                };
                return format!(
                    "{HEADER_2}\nnumber {}\ntime {}\nactor {}\noperation {PURGE}\ncommits {}\n\
                     stubs {stubs}\n",
                    self.number, purge.time, purge.actor, purge.commits
                )
                .into_bytes();
            }
        };
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
        let version_2 = match header.strip_prefix(HEADER_PREFIX) {
            Some("1") => false,
            Some("2") => true,
            Some(version) if !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit()) => {
                return Err(EntryParseError::UnsupportedVersion(version.to_owned()));
            }
            _ => return Err(malformed(1, EntryMalformed::ExpectedHeader)),
        };

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
        if version_2 {
            return parse_version_2(&lines, number, time, actor);
        }
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

/// Reads the lines after the actor of a version-2 entry: `stub` alone, or `operation purge`,
/// `commits <n>`, and `stubs <ranges>`.
fn parse_version_2(
    lines: &[&str],
    number: u64,
    time: i64,
    actor: Actor,
) -> Result<Entry, EntryParseError> {
    let malformed = |line, reason| EntryParseError::Malformed { line, reason };
    let record = match lines.get(4).copied() {
        Some("stub") => {
            if lines.len() > 5 {
                return Err(malformed(6, EntryMalformed::TrailingLine));
            }
            Record::Stub(Stub { time, actor })
        }
        Some(line) if line == format!("operation {PURGE}") => {
            let commits = lines
                .get(5)
                .and_then(|line| line.strip_prefix("commits "))
                .ok_or_else(|| malformed(6, EntryMalformed::ExpectedCommits))?;
            let commits = parse_count(commits)
                .ok_or_else(|| malformed(6, EntryMalformed::ExpectedCommits))?;
            let stubs = lines
                .get(6)
                .and_then(|line| line.strip_prefix("stubs "))
                .ok_or_else(|| malformed(7, EntryMalformed::ExpectedStubs))?;
            let stubs =
                parse_ranges(stubs).ok_or_else(|| malformed(7, EntryMalformed::BadStubs))?;
            if lines.len() > 7 {
                return Err(malformed(8, EntryMalformed::TrailingLine));
            }
            Record::Purge(Purge {
                time,
                actor,
                commits,
                stubs,
            })
        }
        _ => return Err(malformed(5, EntryMalformed::ExpectedStubOrPurge)),
    };
    Entry::of(number, record).map_err(EntryParseError::Entry)
}

/// Writes ranges as the log and the command line do: `4-5 7`.
pub(crate) fn ranges_text(ranges: &[(u64, u64)]) -> String {
    ranges
        .iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reads a count: decimal digits without a leading zero, or `0`.
fn parse_count(text: &str) -> Option<u64> {
    if text == "0" {
        return Some(0);
    }
    parse_number(text).ok()
}

/// Reads the ranges of a `purge` entry: `-` for none, or ranges in ascending order that neither
/// overlap nor touch, each a number or `<start>-<end>` with the start below the end.
fn parse_ranges(text: &str) -> Option<Vec<(u64, u64)>> {
    if text == "-" {
        return Some(Vec::new());
    }
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for token in text.split(' ') {
        let (start, end) = match token.split_once('-') {
            Some((start, end)) => {
                let (start, end) = (parse_number(start).ok()?, parse_number(end).ok()?);
                if start >= end {
                    return None;
                }
                (start, end)
            }
            None => {
                let number = parse_number(token).ok()?;
                (number, number)
            }
        };
        if let Some((_, previous)) = ranges.last() {
            if start <= previous + 1 {
                return None;
            }
        }
        ranges.push((start, end));
    }
    Some(ranges)
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
    /// The file is larger than an entry can be.
    TooLong,
    /// The fifth line of a version-2 entry is neither `stub` nor `operation purge`.
    ExpectedStubOrPurge,
    /// The sixth line of a `purge` entry is not `commits <n>`.
    ExpectedCommits,
    /// The seventh line of a `purge` entry is not `stubs <ranges>`.
    ExpectedStubs,
    /// The ranges of the stubs are not `-` or ascending ranges that neither overlap nor touch.
    BadStubs,
    /// A stub or a `purge` entry has a line after its last one.
    TrailingLine,
}

impl fmt::Display for EntryMalformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CutOff => "the last line has no terminating newline",
            Self::ExpectedHeader => "expected the header `stemma-oplog 1` or `2` first",
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
            Self::TooLong => "the file is too large to be an entry",
            Self::ExpectedStubOrPurge => "expected `stub` or `operation purge`",
            Self::ExpectedCommits => "expected `commits <n>`",
            Self::ExpectedStubs => "expected `stubs <ranges>`",
            Self::BadStubs => {
                "expected `-` or ranges such as `4-5 7`, ascending, that neither overlap nor touch"
            }
            Self::TrailingLine => "the entry has a line after its last one",
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
    /// The header names another version than 1 or 2, in decimal digits.
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
    /// The lines are well formed, but the entry is rejected as a whole: a change of the operation
    /// `purge`, or a `purge` entry that lists its own number or a later one. The other rules of
    /// [`Change::new`] and `Entry::new` are checked while the lines are read.
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
                "the entry has version {version}, but this code reads versions 1 and 2"
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
    /// The tip file names an entry that the directory does not hold: the last entries are lost.
    /// The writer refuses to write, as the next number would be given a second time to another
    /// move.
    TipAhead {
        /// The tip file.
        path: PathBuf,
        /// The number of the last entry that the tip file names.
        tip: u64,
        /// The largest number in the directory, 0 if it holds no entry.
        last: u64,
    },
    /// The tip file is not in canonical form.
    BadTip {
        /// The file.
        path: PathBuf,
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
    /// The log is full: the next number is larger than [`MAX_NUMBER`].
    Full,
    /// Every number that the writer tried was taken, so many times that it gave up. Another writer
    /// made progress each time, so trying again is reasonable.
    Contended {
        /// How many numbers were tried.
        attempts: usize,
    },
    /// The entry was written and is in the log, but its directory could not be flushed to disk, so
    /// it may not survive a crash. A caller that orders "entry, then move" must not move: the
    /// entry is not known to be safe. The entry stays; a second attempt writes another one.
    NotFlushed {
        /// The entry that is in the log.
        entry: Box<Entry>,
        /// The directory that could not be flushed.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// The entry would be larger than an entry can be. Nothing was written. The actor or a name in
    /// it is too long.
    EntryTooLarge {
        /// The size of the entry in bytes.
        size: usize,
    },
    /// The change is rejected.
    Entry(EntryError),
    /// An entry is a stub, but no `purge` entry lists it: an entry was replaced outside a purge, or
    /// the `purge` entry is lost (ADR-014).
    UnlistedStub {
        /// The number of the stub.
        number: u64,
    },
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

    /// The error of a helper of the object store. They fail only with a failure of the file system;
    /// any other error is kept as text in an I/O error, so that no cause is dropped.
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
            other => Self::Io {
                operation: "use the object store",
                path: PathBuf::new(),
                source: io::Error::other(other.to_string()),
            },
        }
    }

    /// Returns the error of the file system for [`OpLogError::Io`] and
    /// [`OpLogError::NotFlushed`]. The chain of [`Error::source`] does not lead to it (see the
    /// crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } | Self::NotFlushed { source, .. } => Some(source),
            _ => None,
        }
    }
}

// A path comes from the file system, and a file name in `oplog/` can be any bytes, so a path is
// written escaped like Rust debug output: an error must not write terminal escape sequences.
impl fmt::Display for OpLogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                f,
                "could not {operation} `{}`: {source}",
                path.display().to_string().escape_debug()
            ),
            Self::Corrupt { path, source } => write!(
                f,
                "the entry `{}` is not in canonical form: {source}",
                path.display().to_string().escape_debug()
            ),
            Self::WrongNumber {
                path,
                expected,
                found,
            } => write!(
                f,
                "the entry `{}` has number {found} inside, but its name says {expected}",
                path.display().to_string().escape_debug()
            ),
            Self::Gap { missing } => write!(
                f,
                "the operation log has no entry {missing}, but later ones: an entry is lost"
            ),
            Self::TipAhead { path, tip, last } => write!(
                f,
                "the operation log ends at entry {last}, but the tip file names entry {tip}: \
                 the last entries are lost; restore them, or, once the loss is understood, set \
                 the number in `{}` back to {last}",
                path.display().to_string().escape_debug()
            ),
            Self::BadTip { path } => write!(
                f,
                "the tip file `{}` of the operation log is not in canonical form; write the \
                 number of the last entry into it, or remove it",
                path.display().to_string().escape_debug()
            ),
            Self::UnexpectedFile { path } => write!(
                f,
                "`{}` is in the operation log, but is not an entry",
                path.display().to_string().escape_debug()
            ),
            Self::Missing { number } => write!(f, "the operation log has no entry {number}"),
            Self::Full => write!(
                f,
                "the operation log is full: it holds {MAX_NUMBER} entries"
            ),
            Self::Contended { attempts } => write!(
                f,
                "the operation log is busy: every one of {attempts} numbers that were tried was \
                 taken by another writer"
            ),
            Self::NotFlushed {
                entry,
                path,
                source,
            } => write!(
                f,
                "entry {} was written, but flushing `{}` failed: {source}; it may not survive a \
                 crash",
                entry.number,
                path.display().to_string().escape_debug()
            ),
            Self::EntryTooLarge { size } => write!(
                f,
                "the entry would be {size} bytes, but an entry holds at most {MAX_ENTRY_LEN}; the \
                 actor or a name in it is too long, and nothing was written"
            ),
            Self::Entry(error) => error.fmt(f),
            Self::UnlistedStub { number } => write!(
                f,
                "entry {number} is a stub, but no purge entry lists it: it was replaced outside a \
                 purge, or the purge entry is lost"
            ),
        }
    }
}

impl Error for OpLogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes that of the I/O error, so the source is its own cause.
            Self::Io { source, .. } | Self::NotFlushed { source, .. } => source.source(),
            Self::Corrupt { source, .. } => source.source(),
            Self::Entry(error) => error.source(),
            _ => None,
        }
    }
}

/// The name of the tip file, next to `oplog/` in the metadata directory. It holds the number of the
/// last entry, so that a lost last entry is noticed (ADR-011). A log from before the file has none
/// until the next entry writes it.
const TIP_FILE: &str = "oplog-tip";

/// The first line of the tip file.
const TIP_HEADER: &str = "stemma-oplog-tip 1\n";

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

    /// Lists the names of the directory as numbers, in no order. A name that starts with a dot is
    /// skipped; any other name that is not an entry number is reported.
    fn scan(&self) -> Result<Vec<u64>, OpLogError> {
        let dir = self.dir();
        let mut numbers = Vec::new();
        for item in fs::read_dir(&dir).map_err(OpLogError::io("read the directory", &dir))? {
            let item = item.map_err(OpLogError::io("read the directory", &dir))?;
            let name = item.file_name();
            // Names with a dot: file managers and editors leave them. The first byte is looked at,
            // so a name that is not UTF-8 is skipped as well.
            if name.as_encoded_bytes().first() == Some(&b'.') {
                continue;
            }
            let number = name
                .to_str()
                .and_then(parse_file_name)
                .ok_or_else(|| OpLogError::UnexpectedFile { path: item.path() })?;
            numbers.push(number);
        }
        Ok(numbers)
    }

    /// Returns the first number that is missing from 1, 2, 3, … in the sorted numbers, if any.
    fn first_gap(sorted: &[u64]) -> Option<u64> {
        sorted
            .iter()
            .enumerate()
            .find(|(index, number)| **number != *index as u64 + 1)
            .map(|(index, _)| index as u64 + 1)
    }

    /// Lists the numbers of the entries in ascending order. They have to be 1, 2, 3, … without a
    /// gap, and the last one has to reach the tip file: a tip beyond the last entry means that
    /// the last entries are lost. A name that starts with a dot is skipped, and any other name
    /// that is not an entry number is reported.
    ///
    /// A listing is not a snapshot while a writer adds files, so a gap is reported only if a second
    /// listing shows it too. The tip is read before the first listing (see `check_tip`).
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::UnexpectedFile`] for a name that is not an entry number,
    /// [`OpLogError::Gap`] if a number is missing, [`OpLogError::TipAhead`] if the tip names an
    /// entry that the directory lacks, [`OpLogError::BadTip`] if the tip file is not in canonical
    /// form, and [`OpLogError::Io`] if the directory or the tip file cannot be read.
    pub fn numbers(&self) -> Result<Vec<u64>, OpLogError> {
        let tip = self.read_tip()?;
        let mut numbers = self.scan()?;
        numbers.sort_unstable();
        if Self::first_gap(&numbers).is_some() {
            numbers = self.scan()?;
            numbers.sort_unstable();
            if let Some(missing) = Self::first_gap(&numbers) {
                return Err(OpLogError::Gap { missing });
            }
        }
        self.check_tip(tip, numbers.last().copied().unwrap_or(0))?;
        Ok(numbers)
    }

    fn tip_path(&self) -> PathBuf {
        self.root.join(TIP_FILE)
    }

    /// Reads the number of the last entry from the tip file. A missing file is `None`: a log
    /// without a tip yet (a repository made before the tip file, or a log that was never written).
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::BadTip`] if the file is not in canonical form, and
    /// [`OpLogError::Io`] if it cannot be read.
    fn read_tip(&self) -> Result<Option<u64>, OpLogError> {
        let path = self.tip_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(OpLogError::io("read the tip file", &path)(error)),
        };
        let number = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.strip_prefix(TIP_HEADER))
            .and_then(|rest| rest.strip_prefix("number "))
            .and_then(|rest| rest.strip_suffix('\n'))
            .filter(|digits| {
                !digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit())
                    && !(digits.len() > 1 && digits.starts_with('0'))
            })
            .and_then(|digits| digits.parse::<u64>().ok())
            .filter(|number| *number <= MAX_NUMBER);
        number.map(Some).ok_or(OpLogError::BadTip { path })
    }

    /// Replaces the tip file with the number: a temporary file that is flushed, then a rename. The
    /// directory is flushed on a best effort basis, because a tip that is not on disk after a
    /// crash is one behind or missing, which the writer accepts; the next entry writes it again.
    fn write_tip(&self, number: u64) -> Result<(), OpLogError> {
        let temp_dir = self.root.join("tmp");
        let (mut file, mut temp) =
            create_temp(&temp_dir, &mut unique_temp_name).map_err(OpLogError::from_store)?;
        let content = format!("{TIP_HEADER}number {number}\n");
        file.write_all(content.as_bytes())
            .map_err(OpLogError::io("write the temporary file", &temp.path))?;
        file.sync_all()
            .map_err(OpLogError::io("flush the temporary file", &temp.path))?;
        drop(file);
        let path = self.tip_path();
        fs::rename(&temp.path, &path).map_err(OpLogError::io("replace the tip file", &path))?;
        // The name is gone after the rename.
        temp.disarm();
        let _ = sync_directory(&self.root);
        Ok(())
    }

    /// Fails with [`OpLogError::TipAhead`] if the tip names an entry beyond the largest number in
    /// the directory. A tip one behind is no fault: a stop between the entry and the tip.
    ///
    /// The caller reads the tip *before* it lists the directory. A writer links entry *n* before
    /// it writes tip *n*, so a tip that was read first is covered by the listing that follows,
    /// unless the entry is really lost. In the other order a reader could see the listing without
    /// entry *n* and the tip with it, and report a loss that did not happen.
    fn check_tip(&self, tip: Option<u64>, last: u64) -> Result<(), OpLogError> {
        match tip {
            Some(tip) if tip > last => Err(OpLogError::TipAhead {
                path: self.tip_path(),
                tip,
                last,
            }),
            _ => Ok(()),
        }
    }

    /// The number after the largest one in the directory, or 1. It does not look at the tip or at
    /// gaps; it serves the retry of a writer that [`append`](Self::append) has checked already.
    fn next_number(&self) -> Result<u64, OpLogError> {
        Ok(self.scan()?.into_iter().max().map_or(1, |last| last + 1))
    }

    /// Reads the entry with the number.
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::Missing`] if there is none, [`OpLogError::Corrupt`] if it is not in
    /// canonical form or larger than an entry can be, [`OpLogError::WrongNumber`] if the number
    /// inside is not the one of the name, and [`OpLogError::Io`] if the storage fails.
    pub fn read(&self, number: u64) -> Result<Entry, OpLogError> {
        let path = self.path_of(number);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(OpLogError::Missing { number });
            }
            Err(error) => return Err(OpLogError::io("open the entry", &path)(error)),
        };
        // An entry is small. A file that is much larger is not one, and is not read in full.
        let mut bytes = Vec::new();
        file.take(MAX_ENTRY_LEN + 1)
            .read_to_end(&mut bytes)
            .map_err(OpLogError::io("read the entry", &path))?;
        if bytes.len() as u64 > MAX_ENTRY_LEN {
            return Err(OpLogError::Corrupt {
                path,
                source: EntryParseError::Malformed {
                    line: 1,
                    reason: EntryMalformed::TooLong,
                },
            });
        }
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
    /// See [`numbers`](Self::numbers) and [`read`](Self::read), and [`OpLogError::UnlistedStub`]
    /// for a stub that no `purge` entry lists.
    pub fn entries(&self) -> Result<Vec<Entry>, OpLogError> {
        let entries: Vec<Entry> = self
            .numbers()?
            .into_iter()
            .map(|number| self.read(number))
            .collect::<Result<_, _>>()?;
        check_stubs(&entries)?;
        Ok(entries)
    }

    /// Writes a new entry for the change under the next free number, flushes it, and returns it.
    /// The entry is whole when it can first be seen: it is written to a temporary file, flushed,
    /// and published with a hard link, which fails if the number is taken. If it is taken, another
    /// writer was faster, and the end of the log is looked up again.
    ///
    /// `Repository::update_branch` and `set_head` call this in the order of ADR-011, under the lock
    /// of the references: the entry first, then the move of the pointer. `init` calls it for the
    /// first entry, in a directory that no other writer can see yet.
    ///
    /// Before it takes a number, the writer reads the tip file and refuses if the tip is beyond the
    /// largest number in the directory ([`OpLogError::TipAhead`]), because the number of a lost
    /// last entry would go to another move. A gap in the middle does not stop it: a refusal would
    /// block the move that repairs the log. After the entry is published, the writer replaces the
    /// tip file. This is best effort: a tip that stays behind is accepted, and the next entry
    /// writes it again. After [`OpLogError::NotFlushed`] the tip is not advanced, as the entry may
    /// not survive a crash and a tip that did would then refuse every later writer.
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::TipAhead`] and [`OpLogError::BadTip`] (nothing is written),
    /// [`OpLogError::EntryTooLarge`] if the entry would be larger than an entry can be
    /// (nothing is written), [`OpLogError::Full`] if the log is full, [`OpLogError::Contended`] if
    /// every number that was tried was taken, [`OpLogError::NotFlushed`] if the entry is in place
    /// but not known to be flushed, [`OpLogError::UnexpectedFile`] for a name in the directory that
    /// is not an entry, and [`OpLogError::Io`] if the storage fails, among others if the file
    /// system has no hard links, or if the directory of the log does not exist.
    pub(crate) fn append(&self, change: &Change) -> Result<Entry, OpLogError> {
        self.append_record(&Record::Change(change.clone()))
    }

    /// Writes the entries of a purge, as [`append`](Self::append) writes an entry, and returns
    /// them. A list of stubs that does not fit into one entry is split over several, and the first
    /// has the count of commits (ADR-014). The entries go after the ones that they list.
    ///
    /// # Errors
    ///
    /// See [`append`](Self::append). If a later entry fails, the earlier ones are in the log.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the purge of STEMMA-48 calls it")
    )]
    pub(crate) fn append_purge(&self, purge: Purge) -> Result<Vec<Entry>, OpLogError> {
        purge
            .split()
            .into_iter()
            .map(|part| self.append_record(&Record::Purge(part)))
            .collect()
    }

    /// Replaces the entry by its stub (ADR-014): the stub is written to a temporary file in `tmp/`,
    /// flushed, and renamed over the entry, so a reader sees the entry or the stub and never a
    /// mix; then the directory is flushed. A stub is replaced by itself, so the call can be
    /// repeated. It is called after the `purge` entry that lists the number is written; a stub
    /// that no `purge` entry lists is an error of the log.
    ///
    /// # Errors
    ///
    /// Returns what [`read`](Self::read) returns for the entry, [`OpLogError::Entry`] with
    /// [`EntryError::PurgeNotReplaceable`] for the entry of a purge, and [`OpLogError::Io`] if the
    /// storage fails.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the purge of STEMMA-48 calls it")
    )]
    pub(crate) fn replace_with_stub(&self, number: u64) -> Result<Entry, OpLogError> {
        let stub = self.read(number)?.to_stub().map_err(OpLogError::Entry)?;
        let temp_dir = self.root.join("tmp");
        let (mut file, mut temp) =
            create_temp(&temp_dir, &mut unique_temp_name).map_err(OpLogError::from_store)?;
        file.write_all(&stub.encode())
            .map_err(OpLogError::io("write the temporary file", &temp.path))?;
        file.sync_all()
            .map_err(OpLogError::io("flush the temporary file", &temp.path))?;
        drop(file);
        let path = self.path_of(number);
        fs::rename(&temp.path, &path).map_err(OpLogError::io("replace the entry", &path))?;
        temp.disarm();
        let dir = self.dir();
        sync_directory(&dir).map_err(OpLogError::from_store)?;
        Ok(stub)
    }

    /// Writes an entry of any record; see [`append`](Self::append).
    fn append_record(&self, record: &Record) -> Result<Entry, OpLogError> {
        // The tip is read first, as in `numbers`.
        let tip = self.read_tip()?;
        let last = self.scan()?.into_iter().max().unwrap_or(0);
        // A lost last entry must not hand its number to another move.
        self.check_tip(tip, last)?;
        let result = self.publish(
            record,
            last + 1,
            &mut |_| {},
            &|from, to| fs::hard_link(from, to),
            &|dir| match sync_directory(dir) {
                Ok(()) => Ok(()),
                Err(StoreError::Io { source, .. }) => Err(source),
                Err(other) => Err(io::Error::other(other.to_string())),
            },
        );
        // Only a flushed entry advances the tip. A failure to write it is no error of the move: the
        // entry is in place, and the tip is one behind, which the next writer accepts.
        if let Ok(entry) = &result {
            let _ = self.write_tip(entry.number);
        }
        result
    }

    /// The loop of [`append`](Self::append), with the steps that a test has to be able to change:
    /// `before_link` is called with the number that is about to be tried, after the temporary file
    /// is ready and before it is linked; `link` publishes the temporary file under the name of the
    /// entry and has to fail with [`io::ErrorKind::AlreadyExists`] if the name is taken; `sync`
    /// flushes a directory.
    fn publish(
        &self,
        record: &Record,
        start: u64,
        before_link: &mut dyn FnMut(u64),
        link: &dyn Fn(&Path, &Path) -> io::Result<()>,
        sync: &dyn Fn(&Path) -> io::Result<()>,
    ) -> Result<Entry, OpLogError> {
        let dir = self.dir();
        let temp_dir = self.root.join("tmp");
        let mut number = start;
        for _ in 0..ATTEMPTS {
            let entry = match Entry::of(number, record.clone()) {
                Ok(entry) => entry,
                Err(EntryError::NumberOutOfRange(_)) => return Err(OpLogError::Full),
                Err(other) => return Err(OpLogError::Entry(other)),
            };
            let encoded = entry.encode();
            // The reader refuses a file larger than this, so the writer must not make one.
            if encoded.len() as u64 > MAX_ENTRY_LEN {
                return Err(OpLogError::EntryTooLarge {
                    size: encoded.len(),
                });
            }
            let (mut file, temp) =
                create_temp(&temp_dir, &mut unique_temp_name).map_err(OpLogError::from_store)?;
            file.write_all(&encoded)
                .map_err(OpLogError::io("write the temporary file", &temp.path))?;
            file.sync_all()
                .map_err(OpLogError::io("flush the temporary file", &temp.path))?;
            drop(file);
            before_link(number);
            let path = self.path_of(number);
            match link(&temp.path, &path) {
                Ok(()) => {
                    // The entry is in place under its own name. The temporary name is removed when
                    // `temp` is dropped, on a best effort basis: a name that stays in `tmp/` is
                    // scratch and harmless, and it must not stop a move whose entry is whole.
                    drop(temp);
                    return match sync(&dir) {
                        Ok(()) => Ok(entry),
                        Err(source) => Err(OpLogError::NotFlushed {
                            entry: Box::new(entry),
                            path: dir,
                            source,
                        }),
                    };
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    // Another writer was faster. Look again for the end of the log, which can be
                    // several entries further on, instead of stepping by one.
                    drop(temp);
                    number = self.next_number()?.max(number + 1);
                }
                Err(source) => {
                    return Err(OpLogError::Io {
                        operation: "publish the entry with a hard link",
                        path,
                        source,
                    });
                }
            }
        }
        Err(OpLogError::Contended { attempts: ATTEMPTS })
    }
}

/// Checks that a `purge` entry lists every stub (ADR-014). A listed number that is not a stub yet
/// is no error: the purge writes its entry before the stubs. `entries` must be the end of the log,
/// from some entry to the last: a purge entry comes after the stubs that it lists, so it is among
/// them.
///
/// # Errors
///
/// Returns [`OpLogError::UnlistedStub`] for the first stub that no `purge` entry lists.
pub fn check_stubs(entries: &[Entry]) -> Result<(), OpLogError> {
    let purges: Vec<&Purge> = entries
        .iter()
        .filter_map(|entry| match entry.record() {
            Record::Purge(purge) => Some(purge),
            _ => None,
        })
        .collect();
    match entries.iter().find(|entry| {
        matches!(entry.record(), Record::Stub(_))
            && !purges.iter().any(|purge| purge.lists(entry.number()))
    }) {
        Some(entry) => Err(OpLogError::UnlistedStub {
            number: entry.number(),
        }),
        None => Ok(()),
    }
}

/// Reads the name of a file of the log: exactly [`NUMBER_WIDTH`] decimal digits, from 1.
fn parse_file_name(name: &str) -> Option<u64> {
    if name.len() != NUMBER_WIDTH || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok().filter(|number| *number >= 1)
}

/// Whether the move of an entry took effect. An entry is written before the pointer moves, so
/// an entry can name a move that never happened (ADR-011).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every pointer of the entry moved to its new value, as far as the log and the references
    /// show.
    Took,
    /// At least one pointer of the entry did not move to its new value.
    NotTaken,
    /// The log and the references do not decide it.
    Unknown,
}

/// The values that the pointers have now, for [`outcomes`]. The working-copy commit is part of it
/// only if [`Current::with_working_copy`] gave it; without it the outcome of its last move is
/// unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    head: Head,
    working_copy: WorkingCopy,
    branches: BTreeMap<BranchName, CommitId>,
    settled: bool,
}

/// What [`Current`] knows about the working-copy commit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkingCopy {
    /// [`Current::with_working_copy`] was not called.
    Unknown,
    /// The working-copy commit does not exist.
    Absent,
    /// The working-copy commit is at the commit.
    At(CommitId),
}

impl Current {
    /// The value of HEAD, and the branches that exist among those that the entries name: a
    /// branch that is not given does not exist. `settled` is false if a writer may be between its
    /// entry and its move (it holds the lock of the references), so that a pointer which still has
    /// its old value is not a move that failed.
    pub fn new(
        head: Head,
        branches: impl IntoIterator<Item = (BranchName, CommitId)>,
        settled: bool,
    ) -> Self {
        Self {
            head,
            working_copy: WorkingCopy::Unknown,
            branches: branches.into_iter().collect(),
            settled,
        }
    }

    /// Adds the value of the working-copy commit, `None` if it does not exist. Without it, the
    /// last move of the working-copy commit in the log stays unknown.
    #[must_use]
    pub fn with_working_copy(mut self, working_copy: Option<CommitId>) -> Self {
        self.working_copy = working_copy.map_or(WorkingCopy::Absent, WorkingCopy::At);
        self
    }
}

/// What is known about a pointer now, for [`judge`].
#[derive(Clone, Copy)]
enum Now<'a, T> {
    /// The value cannot be read, or `current` does not hold the working-copy commit.
    Unknown,
    /// The pointer does not exist.
    Absent,
    /// The pointer has this value.
    Is(&'a T),
}

/// The outcome of one pointer. `next` is the move of the same pointer in the next entry that
/// changes it, if there is one; `now` is the value the pointer has now; `settled` is false if a
/// writer may be between its entry and its move.
///
/// "Did not take effect" is the strongest statement of the log, so it is made only on clear
/// evidence: the next move starts from the old value of this one, or the pointer has its old value
/// now and nobody is writing. Any other value is a change outside the log, and the answer is
/// unknown.
fn judge<T: PartialEq>(
    moved: &Moved<T>,
    next: Option<&Moved<T>>,
    now: Now<'_, T>,
    settled: bool,
) -> Outcome {
    if let Some(next) = next {
        return if next.old == moved.new {
            Outcome::Took
        } else if next.old == moved.old {
            Outcome::NotTaken
        } else {
            Outcome::Unknown
        };
    }
    let now = match now {
        Now::Unknown => return Outcome::Unknown,
        Now::Absent => None,
        Now::Is(value) => Some(value),
    };
    if now == moved.new.as_ref() {
        Outcome::Took
    } else if now == moved.old.as_ref() && settled {
        Outcome::NotTaken
    } else {
        Outcome::Unknown
    }
}

/// Reads from the log whether the move of each entry took effect, in the order of `entries`.
///
/// An entry is an intent (ADR-011). A pointer that an entry moves is confirmed by the next entry
/// that changes the same pointer if its old value is the new value of this one, and refuted if it
/// is the old value of this one; any other old value is a change outside the log and leaves it
/// unknown. The last entry of a pointer is compared with the value the pointer has now: equal to
/// the new value confirms; equal to the old value refutes, but only if `current` is settled;
/// anything else leaves it unknown, as does a working-copy commit that `current` does not hold
/// ([`Current::with_working_copy`]), and a `current` that is `None` because the references could
/// not be read. An entry took effect
/// if all its pointers did, it did not if one did not, and otherwise it is unknown.
///
/// `entries` must ascend and end with the newest entry of the log, because an entry that is the
/// last of its pointer is compared with `current`; `current` must be read after the entries and
/// must hold every branch that they name. A writer that completes an entry and its move between
/// the two reads can make the newest entry unknown, or wrong if it moves a pointer back to its
/// old value; a caller that lists the log again after it read the references can detect that and
/// pass a `current` that is not settled.
pub fn outcomes(entries: &[Entry], current: Option<&Current>) -> Vec<Outcome> {
    debug_assert!(
        entries
            .windows(2)
            .all(|pair| pair[0].number < pair[1].number),
        "the entries must ascend"
    );
    let settled = current.is_some_and(|current| current.settled);
    let mut next_head: Option<&Moved<Head>> = None;
    let mut next_working_copy: Option<&Moved<CommitId>> = None;
    let mut next_branch: HashMap<&BranchName, &Moved<CommitId>> = HashMap::new();
    let mut result = vec![Outcome::Unknown; entries.len()];
    for (index, entry) in entries.iter().enumerate().rev() {
        // A stub and a purge name no pointer: they are no evidence for the entries before them,
        // and their own outcome stays unknown (ADR-014).
        let Some(change) = entry.change() else {
            continue;
        };
        let mut judged = Vec::new();
        if let Some(moved) = change.head() {
            let now = current.map_or(Now::Unknown, |current| Now::Is(&current.head));
            judged.push(judge(moved, next_head, now, settled));
        }
        if let Some(moved) = change.working_copy() {
            let now = match current.map(|current| &current.working_copy) {
                Some(WorkingCopy::Absent) => Now::Absent,
                Some(WorkingCopy::At(commit)) => Now::Is(commit),
                Some(WorkingCopy::Unknown) | None => Now::Unknown,
            };
            judged.push(judge(moved, next_working_copy, now, settled));
        }
        for (name, moved) in change.branches() {
            let now = current.map_or(Now::Unknown, |current| {
                current.branches.get(name).map_or(Now::Absent, Now::Is)
            });
            judged.push(judge(moved, next_branch.get(name).copied(), now, settled));
        }
        result[index] = if judged.contains(&Outcome::NotTaken) {
            Outcome::NotTaken
        } else if judged.iter().all(|outcome| *outcome == Outcome::Took) {
            Outcome::Took
        } else {
            Outcome::Unknown
        };
        // This entry is the next one for the entries before it.
        next_head = change.head().or(next_head);
        next_working_copy = change.working_copy().or(next_working_copy);
        for (name, moved) in change.branches() {
            next_branch.insert(name, moved);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn a_path_with_control_characters_is_escaped_in_the_message() {
        let error = OpLogError::UnexpectedFile {
            path: PathBuf::from("oplog/a\x1b[2Jb\n"),
        };
        let message = error.to_string();
        assert!(
            !message.contains('\x1b') && !message.contains('\n'),
            "{message:?}"
        );
        assert!(message.contains("a\\u{1b}[2Jb\\n"), "{message}");
    }

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

    fn entry(
        number: u64,
        head: Option<Moved<Head>>,
        branches: Vec<(BranchName, Moved<CommitId>)>,
    ) -> Entry {
        let change = Change::new(TIME, human(), operation("snapshot"), head, None, branches)
            .expect("valid change");
        Entry::new(number, change).expect("valid entry")
    }

    fn moves(name: &str, old: Option<&str>, new: Option<&str>) -> (BranchName, Moved<CommitId>) {
        (branch(name), moved(old, new))
    }

    fn current(branches: &[(&str, &str)], settled: bool) -> Current {
        Current::new(
            Head::Branch(branch("main")),
            branches.iter().map(|(n, c)| (branch(n), commit(c))),
            settled,
        )
    }

    /// The references now, with no writer at work.
    fn now(branches: &[(&str, &str)]) -> Option<Current> {
        Some(current(branches, true))
    }

    fn head_move(old: Option<Head>, new: Option<Head>) -> Option<Moved<Head>> {
        Some(Moved::from_to(old, new))
    }

    fn on(name: &str) -> Head {
        Head::Branch(branch(name))
    }

    fn working_copy(
        number: u64,
        old: Option<&str>,
        new: Option<&str>,
        branches: Vec<(BranchName, Moved<CommitId>)>,
    ) -> Entry {
        let change = Change::new(
            TIME,
            human(),
            operation("snapshot"),
            None,
            Some(moved(old, new)),
            branches,
        )
        .expect("valid change");
        Entry::new(number, change).expect("valid entry")
    }

    #[test]
    fn the_last_entry_of_a_pointer_is_compared_with_its_value_now() {
        let entries = [entry(1, None, vec![moves("main", None, Some(A))])];
        let outcome = |branches: &[(&str, &str)]| outcomes(&entries, now(branches).as_ref());
        assert_eq!(outcome(&[("main", A)]), [Outcome::Took]);
        assert_eq!(outcome(&[]), [Outcome::NotTaken]);
        assert_eq!(outcome(&[("main", B)]), [Outcome::Unknown]);
    }

    #[test]
    fn a_pointer_that_still_has_its_old_value_is_unknown_while_a_writer_is_at_work() {
        let entries = [
            entry(1, None, vec![moves("main", None, Some(A))]),
            entry(2, None, vec![moves("other", None, Some(B))]),
        ];
        // Entry 2 may be between its entry and its move; entry 1 is unaffected by the lock.
        let busy = current(&[("main", A)], false);
        assert_eq!(
            outcomes(&entries, Some(&busy)),
            [Outcome::Took, Outcome::Unknown]
        );
        let done = current(&[("main", A), ("other", B)], false);
        assert_eq!(
            outcomes(&entries, Some(&done)),
            [Outcome::Took, Outcome::Took]
        );
    }

    #[test]
    fn without_the_references_only_the_chain_decides() {
        let entries = [
            entry(1, None, vec![moves("main", None, Some(A))]),
            entry(2, None, vec![moves("main", Some(A), Some(B))]),
        ];
        assert_eq!(outcomes(&entries, None), [Outcome::Took, Outcome::Unknown]);
    }

    #[test]
    fn a_later_entry_confirms_or_refutes_the_one_before() {
        let entries = [
            entry(1, None, vec![moves("main", None, Some(A))]),
            entry(2, None, vec![moves("main", Some(A), Some(B))]),
            // Entry 2 did not move: entry 3 starts from A again.
            entry(3, None, vec![moves("main", Some(A), Some(C))]),
        ];
        assert_eq!(
            outcomes(&entries, now(&[("main", C)]).as_ref()),
            [Outcome::Took, Outcome::NotTaken, Outcome::Took]
        );
    }

    #[test]
    fn a_later_entry_that_starts_from_a_third_value_leaves_the_one_before_unknown() {
        // Something outside the log moved `main` between entry 1 and entry 2.
        let entries = [
            entry(1, None, vec![moves("main", Some(A), Some(B))]),
            entry(2, None, vec![moves("main", Some(C), Some(A))]),
        ];
        assert_eq!(
            outcomes(&entries, now(&[("main", A)]).as_ref()),
            [Outcome::Unknown, Outcome::Took]
        );
    }

    #[test]
    fn an_entry_took_effect_only_if_all_pointers_did() {
        let entries = [entry(
            1,
            None,
            vec![moves("a", None, Some(A)), moves("b", None, Some(B))],
        )];
        let outcome = |branches: &[(&str, &str)]| outcomes(&entries, now(branches).as_ref());
        assert_eq!(outcome(&[("a", A), ("b", B)]), [Outcome::Took]);
        // Took and unknown is unknown.
        assert_eq!(outcome(&[("a", A), ("b", C)]), [Outcome::Unknown]);
        // Not taken and unknown is not taken; not taken and took is not taken.
        assert_eq!(outcome(&[("a", C)]), [Outcome::NotTaken]);
        assert_eq!(outcome(&[("a", A)]), [Outcome::NotTaken]);
    }

    #[test]
    fn a_deleted_branch_took_effect_if_it_is_gone() {
        let entries = [entry(1, None, vec![moves("main", Some(A), None)])];
        let outcome = |branches: &[(&str, &str)]| outcomes(&entries, now(branches).as_ref());
        assert_eq!(outcome(&[]), [Outcome::Took]);
        assert_eq!(outcome(&[("main", A)]), [Outcome::NotTaken]);
        assert_eq!(outcome(&[("main", B)]), [Outcome::Unknown]);
    }

    #[test]
    fn an_entry_of_another_pointer_does_not_hide_the_chain_of_a_pointer() {
        let entries = [
            entry(1, None, vec![moves("main", None, Some(A))]),
            entry(2, None, vec![moves("other", None, Some(B))]),
            entry(3, None, vec![moves("main", Some(A), Some(C))]),
            entry(4, head_move(Some(on("main")), Some(on("other"))), vec![]),
            entry(5, None, vec![moves("other", Some(B), None)]),
            entry(6, head_move(Some(on("other")), Some(on("x"))), vec![]),
        ];
        let current = Current::new(on("x"), [(branch("main"), commit(C))], true);
        assert_eq!(
            outcomes(&entries, Some(&current)),
            [
                Outcome::Took,
                Outcome::Took,
                Outcome::Took,
                Outcome::Took,
                Outcome::Took,
                Outcome::Took
            ]
        );
    }

    #[test]
    fn head_is_judged_by_its_own_chain_and_by_its_value_now() {
        let entries = [
            entry(1, head_move(None, Some(on("main"))), vec![]),
            entry(
                2,
                head_move(Some(on("main")), Some(Head::Detached(commit(A)))),
                vec![],
            ),
        ];
        let at = |head: Head| outcomes(&entries, Some(&Current::new(head, [], true)));
        // HEAD is `main` now: entry 2 is refuted, entry 1 is confirmed by entry 2.
        assert_eq!(at(on("main")), [Outcome::Took, Outcome::NotTaken]);
        assert_eq!(
            at(Head::Detached(commit(A))),
            [Outcome::Took, Outcome::Took]
        );
        assert_eq!(at(on("other")), [Outcome::Took, Outcome::Unknown]);
    }

    #[test]
    fn a_working_copy_is_confirmed_by_the_chain_and_unknown_at_the_end() {
        let entries = [
            working_copy(1, None, Some(A), vec![]),
            working_copy(2, Some(A), Some(B), vec![moves("main", None, Some(B))]),
        ];
        let outcome = |branches: &[(&str, &str)]| outcomes(&entries, now(branches).as_ref());
        // The branch is confirmed, the working copy has no place in the references.
        assert_eq!(outcome(&[("main", B)]), [Outcome::Took, Outcome::Unknown]);
        // A refuted branch fails the entry whatever the working copy is.
        assert_eq!(outcome(&[]), [Outcome::Took, Outcome::NotTaken]);
        assert_eq!(outcomes(&[], now(&[]).as_ref()), []);
    }

    #[test]
    #[should_panic(expected = "the entries must ascend")]
    fn entries_that_do_not_ascend_are_a_mistake_of_the_caller() {
        let entries = [
            entry(2, None, vec![moves("main", None, Some(A))]),
            entry(1, None, vec![moves("main", None, Some(B))]),
        ];
        let _ = outcomes(&entries, None);
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
        // Duplicates are found whatever the order of the input, not only next to each other.
        assert_eq!(
            build(
                None,
                None,
                vec![
                    (branch("b"), moved(None, Some(A))),
                    (branch("a"), moved(None, Some(A))),
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
        assert_eq!(read.change().unwrap().operation().as_str(), "snapshot");
        assert_eq!(read.change().unwrap().time(), TIME);
        assert_eq!(read.change().unwrap().actor(), &human());
        let head = read.change().unwrap().head().unwrap();
        assert_eq!(head.before(), Some(&Head::Branch(branch("main"))));
        assert_eq!(head.after(), Some(&Head::Detached(commit(A))));
        assert_eq!(
            read.change().unwrap().working_copy().unwrap().after(),
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
    fn a_version_other_than_1_and_2_is_unsupported_and_not_malformed() {
        let text = golden();
        for version in ["0", "3", "10", "99999999999999999999"] {
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
        let message = parse_error(with(&text, "stemma-oplog 1", "stemma-oplog 3")).to_string();
        assert!(
            message.contains("version 3") && message.contains("versions 1 and 2"),
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
        // A branch name: the error is exactly the one of `BranchName`, or the shape is wrong
        // because the name has a space. The line is the one of the pointer.
        for name in ["", "/a", "a/", ".a", "A\u{e9}", "a//b"] {
            assert_eq!(
                at(format!("branch {name} - {A}\n")),
                EntryParseError::InvalidBranchName {
                    line: 6,
                    source: BranchName::new(name).unwrap_err()
                },
                "{name:?}"
            );
        }
        assert_eq!(
            at(format!("branch a b - {A}\n")),
            malformed(6, EntryMalformed::WrongValueCount)
        );
        // A value of HEAD, in the old and in the new column.
        let branch_error = |name: &str| BranchName::new(name).unwrap_err();
        let id_error = ObjectId::from_str("b3:00").unwrap_err();
        let head_cases: [(&str, EntryParseError); 9] = [
            ("main", malformed(6, EntryMalformed::UnknownHeadValue)),
            ("branch", malformed(6, EntryMalformed::UnknownHeadValue)),
            ("commit", malformed(6, EntryMalformed::UnknownHeadValue)),
            (
                "Branch:main",
                malformed(6, EntryMalformed::UnknownHeadValue),
            ),
            ("tag:x", malformed(6, EntryMalformed::UnknownHeadValue)),
            (":", malformed(6, EntryMalformed::UnknownHeadValue)),
            (
                "branch:",
                EntryParseError::InvalidBranchName {
                    line: 6,
                    source: branch_error(""),
                },
            ),
            (
                "commit:b3:00",
                EntryParseError::InvalidObjectId {
                    line: 6,
                    source: id_error,
                },
            ),
            ("branch:a b", malformed(6, EntryMalformed::WrongValueCount)),
        ];
        for (value, expected) in head_cases {
            assert_eq!(at(format!("head {value} -\n")), expected, "old: {value}");
            assert_eq!(at(format!("head - {value}\n")), expected, "new: {value}");
        }
        // The line is the line of the pointer, also on the second and the third pointer line.
        let h = "head - branch:main\n";
        let w = format!("working-copy {A} {B}\n");
        assert_eq!(
            at(format!("{h}working-copy b3:00 -\n")),
            EntryParseError::InvalidObjectId {
                line: 7,
                source: ObjectId::from_str("b3:00").unwrap_err()
            }
        );
        assert_eq!(
            at(format!("{h}{w}branch a - b3:zz\n")),
            EntryParseError::InvalidObjectId {
                line: 8,
                source: ObjectId::from_str("b3:zz").unwrap_err()
            }
        );
        assert_eq!(
            at(format!("{h}{w}branch a {A} {A}\n")),
            EntryParseError::Unchanged { line: 8 }
        );
        assert_eq!(
            at(format!("{h}{w}branch /a - {A}\n")),
            EntryParseError::InvalidBranchName {
                line: 8,
                source: BranchName::new("/a").unwrap_err()
            }
        );
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
        let message = parse_error(format!("{text}branch zz {A}\n")).to_string();
        assert_eq!(
            message,
            "line 10: expected the old and the new value of the pointer"
        );
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

    fn four_entries(dir: &TempDir) -> FsOpLog {
        let log = log_in(dir);
        for (old, new) in [(None, A), (Some(A), B), (Some(B), A), (Some(A), B)] {
            log.append(&change("main", old, new)).unwrap();
        }
        log
    }

    fn entry_file(dir: &TempDir, number: u64) -> PathBuf {
        dir.path().join("oplog").join(format!("{number:012}"))
    }

    #[test]
    fn the_tip_file_names_the_last_entry() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        assert!(!dir.path().join("oplog-tip").exists());

        log.append(&change("main", None, A)).unwrap();
        assert_eq!(
            fs::read(dir.path().join("oplog-tip")).unwrap(),
            b"stemma-oplog-tip 1\nnumber 1\n"
        );
        log.append(&change("main", Some(A), B)).unwrap();

        assert_eq!(log.read_tip().unwrap(), Some(2));
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    #[test]
    fn a_lost_last_entry_is_noticed_and_the_writer_refuses_its_number() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        fs::remove_file(entry_file(&dir, 4)).unwrap();

        // Without the tip file the directory looks like a log of three entries.
        assert!(matches!(
            log.numbers(),
            Err(OpLogError::TipAhead {
                tip: 4,
                last: 3,
                ..
            })
        ));
        let error = log.append(&change("main", Some(B), A)).unwrap_err();

        assert!(matches!(
            error,
            OpLogError::TipAhead {
                tip: 4,
                last: 3,
                ..
            }
        ));
        assert_eq!(files(&dir.path().join("oplog")).len(), 3);
        assert_eq!(log.read_tip().unwrap(), Some(4));
    }

    #[test]
    fn all_entries_lost_are_noticed_too() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        fs::remove_file(entry_file(&dir, 1)).unwrap();

        let error = log.append(&change("main", None, B)).unwrap_err();

        assert!(matches!(
            error,
            OpLogError::TipAhead {
                tip: 1,
                last: 0,
                ..
            }
        ));
    }

    #[test]
    fn a_lost_first_entry_is_a_gap_and_a_lost_middle_one_too() {
        for lost in [1, 2] {
            let dir = TempDir::new();
            let log = four_entries(&dir);
            fs::remove_file(entry_file(&dir, lost)).unwrap();

            assert!(
                matches!(log.numbers(), Err(OpLogError::Gap { missing }) if missing == lost),
                "{lost}"
            );
            // The writer does not refuse a gap: that would stop the move that repairs the log.
            let appended = log.append(&change("main", Some(B), A)).unwrap();
            assert_eq!(appended.number(), 5, "{lost}");
            assert_eq!(log.read_tip().unwrap(), Some(5), "{lost}");
        }
    }

    #[test]
    fn a_tip_one_behind_is_a_stop_between_the_entry_and_the_tip_and_is_repaired() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        // The stop: entry 4 is in place, the tip still names 3.
        log.write_tip(3).unwrap();

        assert_eq!(log.numbers().unwrap(), [1, 2, 3, 4]);
        let appended = log.append(&change("main", Some(B), A)).unwrap();

        assert_eq!(appended.number(), 5);
        assert_eq!(log.read_tip().unwrap(), Some(5));
    }

    #[test]
    fn a_log_without_a_tip_file_gets_one_with_the_next_entry() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        fs::remove_file(dir.path().join("oplog-tip")).unwrap();

        assert_eq!(log.numbers().unwrap(), [1, 2, 3, 4]);
        log.append(&change("main", Some(B), A)).unwrap();

        assert_eq!(log.read_tip().unwrap(), Some(5));
    }

    #[test]
    fn a_tip_file_that_is_not_canonical_is_an_error_and_nothing_is_written() {
        for content in [
            &b""[..],
            b"stemma-oplog-tip 1\n",
            b"stemma-oplog-tip 1\nnumber 04\n",
            b"stemma-oplog-tip 1\nnumber 4",
            b"stemma-oplog-tip 1\nnumber -4\n",
            b"stemma-oplog-tip 2\nnumber 4\n",
            b"stemma-oplog-tip 1\nnumber 4\nmore\n",
            b"stemma-oplog-tip 1\nnumber 1000000000000\n",
            b"stemma-oplog-tip 1\nnumber 99999999999999\n",
        ] {
            let dir = TempDir::new();
            let log = four_entries(&dir);
            fs::write(dir.path().join("oplog-tip"), content).unwrap();

            let error = log.append(&change("main", Some(B), A)).unwrap_err();

            assert!(matches!(error, OpLogError::BadTip { .. }), "{content:?}");
            assert!(matches!(log.numbers(), Err(OpLogError::BadTip { .. })));
            assert_eq!(files(&dir.path().join("oplog")).len(), 4, "{content:?}");
        }
    }

    #[test]
    fn the_largest_number_is_a_valid_tip() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        log.write_tip(MAX_NUMBER).unwrap();

        assert_eq!(log.read_tip().unwrap(), Some(MAX_NUMBER));
        // It is a tip far beyond the entries, so the log reports the loss.
        assert!(matches!(log.numbers(), Err(OpLogError::TipAhead { .. })));
    }

    #[test]
    fn after_a_refusal_the_writer_works_again_once_the_tip_is_set_back() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        fs::remove_file(entry_file(&dir, 4)).unwrap();
        assert!(log.append(&change("main", Some(B), A)).is_err());

        log.write_tip(3).unwrap();
        let appended = log.append(&change("main", Some(B), A)).unwrap();

        assert_eq!(appended.number(), 4);
        assert_eq!(log.numbers().unwrap(), [1, 2, 3, 4]);
    }

    #[test]
    fn a_gap_is_reported_before_a_tip_that_is_ahead() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        fs::remove_file(entry_file(&dir, 2)).unwrap();
        fs::remove_file(entry_file(&dir, 4)).unwrap();

        assert!(matches!(log.numbers(), Err(OpLogError::Gap { missing: 2 })));
        // The writer ignores the gap, but not the tip: the last entry is lost.
        assert!(matches!(
            log.append(&change("main", Some(B), A)),
            Err(OpLogError::TipAhead { .. })
        ));
    }

    #[test]
    fn the_message_of_a_tip_that_is_ahead_names_the_file_and_what_to_do() {
        let dir = TempDir::new();
        let log = four_entries(&dir);
        fs::remove_file(entry_file(&dir, 4)).unwrap();

        let message = log.numbers().unwrap_err().to_string();

        assert!(
            message
                .starts_with("the operation log ends at entry 3, but the tip file names entry 4: "),
            "{message}"
        );
        assert!(message.contains("oplog-tip"), "{message}");
        assert!(message.contains("back to 3"), "{message}");
    }

    #[test]
    fn a_tip_file_that_cannot_be_read_blocks_the_writer_and_the_reader() {
        // A directory in place of the file is an error of the file system, not a missing tip.
        let dir = TempDir::new();
        let log = four_entries(&dir);
        fs::remove_file(dir.path().join("oplog-tip")).unwrap();
        fs::create_dir(dir.path().join("oplog-tip")).unwrap();

        assert!(matches!(
            log.append(&change("main", Some(B), A)),
            Err(OpLogError::Io { .. })
        ));
        assert!(matches!(log.numbers(), Err(OpLogError::Io { .. })));
        assert_eq!(files(&dir.path().join("oplog")).len(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_write_of_the_tip_is_no_error_of_the_entry() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let log = four_entries(&dir);
        // The tip is renamed into the root, which may be entered but not written. The entry goes
        // to `oplog/`, which stays writable, and the temporary file to `tmp/`.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let enforced = fs::File::create(dir.path().join("probe")).is_err();
        let result = log.append(&change("main", Some(B), A));
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        if !enforced {
            eprintln!("skipped: the permission of a directory is not enforced here (root?)");
            return;
        }

        assert_eq!(result.unwrap().number(), 5);
        // The tip is one behind, which the reader and the next writer accept.
        assert_eq!(log.read_tip().unwrap(), Some(4));
        assert_eq!(log.numbers().unwrap(), [1, 2, 3, 4, 5]);
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        assert_eq!(log.append(&change("main", Some(A), B)).unwrap().number(), 6);
        assert_eq!(log.read_tip().unwrap(), Some(6));
    }

    #[test]
    fn a_reader_sees_no_loss_while_a_writer_appends_as_a_smoke_test() {
        // The writer links an entry before it writes the tip, so the reader reads the tip before
        // it lists the directory (see `check_tip`). This is a smoke test only: the window of the
        // wrong order is narrow, and the test did not fail when that order was built in. The
        // argument is in `check_tip`.
        let dir = TempDir::new();
        let log = log_in(&dir);
        let reader = FsOpLog::new(dir.path());
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let mut reads = 0_u32;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    reader.numbers().expect("no loss while a writer appends");
                    reads += 1;
                }
                reads
            });
            for i in 0..150 {
                let (old, new) = if i % 2 == 0 { (None, A) } else { (Some(A), B) };
                log.append(&change("main", old, new)).unwrap();
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            assert!(handle.join().unwrap() > 0);
        });
        assert_eq!(log.numbers().unwrap().len(), 150);
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
        // A writer needs only the end of the log: it takes the number after the largest, and the
        // reader still reports the gap.
        assert_eq!(log.append(&change("main", None, A)).unwrap().number(), 4);
        assert!(matches!(log.numbers(), Err(OpLogError::Gap { missing: 2 })));
        let message = log.numbers().unwrap_err().to_string();
        assert!(
            message.contains("entry 2") && message.contains("lost"),
            "{message}"
        );
    }

    #[test]
    fn the_log_holds_nothing_but_entries_and_skips_names_with_a_dot() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        let oplog = dir.path().join("oplog");
        // File managers and editors leave names with a dot, as for the references.
        for name in [".DS_Store", ".hidden", "._000000000001"] {
            fs::write(oplog.join(name), b"x").unwrap();
        }
        assert_eq!(log.numbers().unwrap(), [1]);
        assert_eq!(log.append(&change("main", Some(A), B)).unwrap().number(), 2);
        // Any other name is reported with its path, also on the way to a write.
        for name in [
            "000000000003.tmp",
            "1",
            "00000000000a",
            "0000000000000",
            "000000000000",
            "entry",
        ] {
            let path = oplog.join(name);
            fs::write(&path, b"x").unwrap();
            for error in [
                log.numbers().unwrap_err(),
                log.append(&change("main", None, A)).unwrap_err(),
            ] {
                assert!(
                    matches!(&error, OpLogError::UnexpectedFile { path: found } if found == &path),
                    "{name}: {error}"
                );
            }
            fs::remove_file(path).unwrap();
        }
        assert_eq!(log.numbers().unwrap(), [1, 2]);
    }

    #[test]
    fn a_directory_in_the_place_of_an_entry_cannot_be_read_as_one() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        fs::create_dir(dir.path().join("oplog").join("000000000001")).unwrap();
        // The name is that of an entry, so the listing accepts it; reading it fails.
        assert_eq!(log.numbers().unwrap(), [1]);
        let error = log.read(1).unwrap_err();
        assert!(matches!(error, OpLogError::Io { .. }), "{error}");
        assert!(error.io_error().is_some());
    }

    #[test]
    fn a_file_that_is_larger_than_an_entry_is_not_read_in_full() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        fs::write(
            dir.path().join("oplog").join("000000000001"),
            vec![b'x'; 70_000],
        )
        .unwrap();
        let error = log.read(1).unwrap_err();
        assert!(
            matches!(
                &error,
                OpLogError::Corrupt {
                    source: EntryParseError::Malformed {
                        reason: EntryMalformed::TooLong,
                        ..
                    },
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.to_string().contains("too large"), "{error}");
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
            .map(|e| e.change().unwrap().branches()[0].0.to_string())
            .collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 40);
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    fn link(from: &Path, to: &Path) -> io::Result<()> {
        fs::hard_link(from, to)
    }

    fn sync(dir: &Path) -> io::Result<()> {
        match sync_directory(dir) {
            Ok(()) => Ok(()),
            Err(StoreError::Io { source, .. }) => Err(source),
            Err(other) => Err(io::Error::other(other.to_string())),
        }
    }

    #[test]
    fn a_writer_that_loses_a_number_looks_for_the_end_again_and_cleans_up() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        // The writer has chosen 2. Before it links, other writers take 2 and 3 and 4.
        let mut taken = Vec::new();
        let mut before_link = |number: u64| {
            taken.push(number);
            if number == 2 {
                for n in 0..3 {
                    log.append(&change(&format!("w{n}"), None, B)).unwrap();
                }
            }
        };
        let entry = log
            .publish(
                &Record::Change(change("late", None, C)),
                2,
                &mut before_link,
                &link,
                &sync,
            )
            .unwrap();
        // The second try is not 3: the writer looked at the end of the log, which was 4, and took 5.
        assert_eq!(taken, [2, 5]);
        assert_eq!(entry.number(), 5);
        assert_eq!(log.numbers().unwrap(), [1, 2, 3, 4, 5]);
        assert_eq!(
            log.read(5).unwrap().change().unwrap().branches()[0].0,
            branch("late")
        );
        // The three entries of the others are whole, and nothing is left in `tmp/`.
        for n in 2..=4 {
            assert_eq!(
                log.read(n).unwrap().change().unwrap().operation().as_str(),
                "commit"
            );
        }
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    #[test]
    fn a_writer_that_is_overtaken_every_time_gives_up_as_contended_and_leaves_nothing() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let always_taken = |_: &Path, _: &Path| Err(io::ErrorKind::AlreadyExists.into());
        let error = log
            .publish(
                &Record::Change(change("main", None, A)),
                1,
                &mut |_| {},
                &always_taken,
                &sync,
            )
            .unwrap_err();
        assert!(
            matches!(error, OpLogError::Contended { attempts } if attempts == ATTEMPTS),
            "{error}"
        );
        assert!(error.to_string().contains("busy"), "{error}");
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        assert_eq!(log.numbers().unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn the_largest_number_is_written_and_the_next_one_is_a_full_log() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let entry = log
            .publish(
                &Record::Change(change("a", None, A)),
                MAX_NUMBER,
                &mut |_| {},
                &link,
                &sync,
            )
            .unwrap();
        assert_eq!(entry.number(), MAX_NUMBER);
        assert_eq!(files(&dir.path().join("oplog")), ["999999999999"]);
        // The file is there, and a reader reports the gap before it, as for any other loss.
        assert!(matches!(log.numbers(), Err(OpLogError::Gap { missing: 1 })));
        assert_eq!(log.read(MAX_NUMBER).unwrap(), entry);
        // A writer that is at the end finds that there is no number after it.
        let error = log
            .append(&change("b", None, B))
            .expect_err("the next number is out of range");
        assert!(matches!(error, OpLogError::Full), "{error}");
        assert!(error.to_string().contains("999999999999"), "{error}");
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        let error = log
            .publish(
                &Record::Change(change("c", None, C)),
                MAX_NUMBER + 1,
                &mut |_| {},
                &link,
                &sync,
            )
            .unwrap_err();
        assert!(matches!(error, OpLogError::Full), "{error}");
    }

    #[test]
    fn a_failing_flush_leaves_the_entry_and_carries_it() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("main", None, A)).unwrap();
        let failing = |_: &Path| Err(io::Error::other("disk"));
        let error = log
            .publish(
                &Record::Change(change("main", Some(A), B)),
                2,
                &mut |_| {},
                &link,
                &failing,
            )
            .unwrap_err();
        let OpLogError::NotFlushed { entry, .. } = &error else {
            panic!("not a failed flush: {error}");
        };
        // The error carries the whole entry, not only its number.
        assert_eq!(entry.number(), 2);
        assert_eq!(entry.change().unwrap().branches()[0].0, branch("main"));
        let message = error.to_string();
        assert!(
            message.starts_with("entry 2 was written") && message.contains("disk"),
            "{message}"
        );
        assert!(error.io_error().is_some());
        // The entry is in the log, whole, and the temporary name is gone.
        assert_eq!(log.numbers().unwrap(), [1, 2]);
        assert_eq!(&log.read(2).unwrap(), entry.as_ref());
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    #[test]
    fn a_failing_link_is_an_error_that_names_the_entry_and_leaves_nothing() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let refused = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::PermissionDenied));
        let error = log
            .publish(
                &Record::Change(change("main", None, A)),
                1,
                &mut |_| {},
                &refused,
                &sync,
            )
            .unwrap_err();
        assert!(
            matches!(&error, OpLogError::Io { operation, path, .. }
                if operation.contains("hard link") && path.ends_with("000000000001")),
            "{error}"
        );
        assert!(error.io_error().is_some());
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        assert_eq!(files(&dir.path().join("oplog")), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn an_oplog_directory_that_cannot_be_written_fails_at_the_link_and_leaves_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let log = log_in(&dir);
        let oplog = dir.path().join("oplog");
        fs::set_permissions(&oplog, fs::Permissions::from_mode(0o555)).unwrap();
        let writable = fs::File::create(oplog.join("probe")).is_ok();
        let error = log.append(&change("main", None, A));
        fs::set_permissions(&oplog, fs::Permissions::from_mode(0o755)).unwrap();
        if writable {
            eprintln!("skipped: the permission of a directory is not enforced here (root?)");
            return;
        }
        let error = error.unwrap_err();
        assert!(
            matches!(&error, OpLogError::Io { operation, .. } if operation.contains("hard link")),
            "{error}"
        );
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
        assert_eq!(files(&oplog), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn a_temporary_name_that_cannot_be_removed_does_not_stop_a_whole_entry() {
        // `tmp/` holds scratch files. If the name cannot be removed after the link, the entry is
        // still whole and flushed, and the write succeeds: a cleanup must not stop a move.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let log = log_in(&dir);
        let tmp = dir.path().join("tmp");
        let probe = {
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o555)).unwrap();
            let writable = fs::File::create(tmp.join("probe")).is_ok();
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
            let _ = fs::remove_file(tmp.join("probe"));
            writable
        };
        if probe {
            eprintln!("skipped: the permission of a directory is not enforced here (root?)");
            return;
        }
        let mut make_tmp_read_only = |_: u64| {
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o555)).unwrap();
        };
        let result = log.publish(
            &Record::Change(change("main", None, A)),
            1,
            &mut make_tmp_read_only,
            &link,
            &sync,
        );
        let left = files(&tmp);
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        let entry = result.expect("the entry is whole, so the write succeeds");
        assert_eq!(entry.number(), 1);
        assert_eq!(log.numbers().unwrap(), [1]);
        assert_eq!(log.read(1).unwrap(), entry);
        // The name stays in `tmp/` and is scratch; the entry has its own name.
        assert_eq!(left.len(), 1);
        assert!(!left[0].starts_with('0'), "{left:?}");
    }

    #[test]
    fn an_entry_that_is_larger_than_an_entry_can_be_is_refused_and_nothing_is_written() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        let size_of = |id_len: usize| {
            let actor = Actor::human("a".repeat(id_len)).unwrap();
            let change = Change::new(
                TIME,
                actor,
                operation("commit"),
                None,
                None,
                vec![(branch("main"), moved(None, Some(A)))],
            )
            .unwrap();
            (
                change.clone(),
                Entry::new(1, change).unwrap().encode().len(),
            )
        };
        // Find the id length that gives exactly the largest entry.
        let (_, base) = size_of(1);
        let fit = MAX_ENTRY_LEN as usize - base + 1;
        let (change_fit, size) = size_of(fit);
        assert_eq!(size as u64, MAX_ENTRY_LEN);
        // The largest entry that the writer makes can be read back.
        let entry = log.append(&change_fit).unwrap();
        assert_eq!(log.read(1).unwrap(), entry);
        // One byte more is refused before anything is written.
        let (change_over, size) = size_of(fit + 1);
        assert_eq!(size as u64, MAX_ENTRY_LEN + 1);
        let error = log.append(&change_over).unwrap_err();
        assert!(
            matches!(error, OpLogError::EntryTooLarge { size } if size as u64 == MAX_ENTRY_LEN + 1),
            "{error}"
        );
        assert!(error.to_string().contains("nothing was written"), "{error}");
        assert_eq!(log.numbers().unwrap(), [1]);
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
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
            entry: Box::new(Entry::new(7, change("main", None, A)).unwrap()),
            path: PathBuf::from("/x/oplog"),
            source: io::Error::other(Outer),
        };
        let message = not_flushed.to_string();
        assert!(
            message.contains("entry 7 was written")
                && message.contains("flushing `/x/oplog` failed: outer"),
            "{message}"
        );
        assert!(message.contains("may not survive"), "{message}");
        assert!(not_flushed.io_error().is_some());
        for (error, text) in [
            (OpLogError::Missing { number: 3 }, "no entry 3"),
            (OpLogError::Full, "full"),
            (OpLogError::Contended { attempts: 64 }, "64 numbers"),
            (OpLogError::EntryTooLarge { size: 70_000 }, "70000 bytes"),
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

    // Stubs and purges (ADR-014)

    /// A log with three changes, then the entry of a purge that lists 2 and 3, with entries 2
    /// and 3 replaced by stubs.
    fn purged_log(dir: &TempDir) -> FsOpLog {
        let log = log_in(dir);
        log.append(&change("a", None, A)).unwrap();
        log.append(&change("b", None, B)).unwrap();
        log.append(&change("c", None, C)).unwrap();
        let written = log
            .append_purge(Purge::new(TIME + 9, human(), 2, &[3, 2]))
            .unwrap();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].number(), 4);
        log.replace_with_stub(2).unwrap();
        log.replace_with_stub(3).unwrap();
        log
    }

    #[test]
    fn a_stub_and_a_purge_have_one_encoding_each() {
        let stub = Entry::new(5, change("a", None, A))
            .unwrap()
            .to_stub()
            .unwrap();
        let text = "stemma-oplog 2\nnumber 5\ntime 1700000000\nactor human daniel\nstub\n";
        assert_eq!(String::from_utf8(stub.encode()).unwrap(), text);
        assert_eq!(Entry::parse(text.as_bytes()).unwrap(), stub);

        let purge = Entry::of(
            9,
            Record::Purge(Purge::new(TIME, human(), 3, &[7, 4, 5, 1])),
        )
        .unwrap();
        let text = "stemma-oplog 2\nnumber 9\ntime 1700000000\nactor human daniel\n\
                    operation purge\ncommits 3\nstubs 1 4-5 7\n";
        assert_eq!(String::from_utf8(purge.encode()).unwrap(), text);
        assert_eq!(Entry::parse(text.as_bytes()).unwrap(), purge);

        let none = Entry::of(2, Record::Purge(Purge::new(TIME, human(), 0, &[]))).unwrap();
        let text = "stemma-oplog 2\nnumber 2\ntime 1700000000\nactor human daniel\n\
                    operation purge\ncommits 0\nstubs -\n";
        assert_eq!(String::from_utf8(none.encode()).unwrap(), text);
        assert_eq!(Entry::parse(text.as_bytes()).unwrap(), none);
    }

    #[test]
    fn the_reader_rejects_every_other_form_of_a_stub_or_a_purge() {
        let start = "stemma-oplog 2\nnumber 9\ntime 1700000000\nactor human daniel\n";
        for rest in [
            "",
            "stub\nextra\n",
            "stub \n",
            "operation snapshot\n",
            "operation purge\n",
            "operation purge\ncommits 01\nstubs 1\n",
            "operation purge\ncommits 1\n",
            "operation purge\ncommits 1\nstubs \n",
            "operation purge\ncommits 1\nstubs 2 1\n",
            "operation purge\ncommits 1\nstubs 1 2\n",
            "operation purge\ncommits 1\nstubs 1-2 2\n",
            "operation purge\ncommits 1\nstubs 3-3\n",
            "operation purge\ncommits 1\nstubs 1,2\n",
            "operation purge\ncommits 1\nstubs 0\n",
            "operation purge\ncommits 1\nstubs 9\n",
            "operation purge\ncommits 1\nstubs 1\nmore\n",
        ] {
            let text = format!("{start}{rest}");
            assert!(Entry::parse(text.as_bytes()).is_err(), "{text:?}");
        }
        // A change cannot take the name of a purge, in either version.
        let text = "stemma-oplog 1\nnumber 3\ntime 1700000000\nactor human daniel\n\
                    operation purge\nbranch main - "
            .to_owned()
            + A
            + "\n";
        assert!(matches!(
            Entry::parse(text.as_bytes()),
            Err(EntryParseError::Entry(EntryError::ReservedOperation))
        ));
        assert!(matches!(
            Entry::parse(b"stemma-oplog 3\n"),
            Err(EntryParseError::UnsupportedVersion(version)) if version == "3"
        ));
    }

    #[test]
    fn a_purge_replaces_entries_by_stubs_that_keep_number_time_and_actor() {
        let dir = TempDir::new();
        let log = purged_log(&dir);
        let entries = log.entries().unwrap();
        let numbers: Vec<u64> = entries.iter().map(Entry::number).collect();
        assert_eq!(numbers, [1, 2, 3, 4], "no gap");
        assert!(entries[0].change().is_some());
        for stub in &entries[1..3] {
            assert!(matches!(stub.record(), Record::Stub(_)));
            assert_eq!(stub.time(), TIME);
            assert_eq!(stub.actor(), &human());
            assert!(stub.commits().is_empty());
        }
        match entries[3].record() {
            Record::Purge(purge) => {
                assert_eq!(purge.stubs(), [(2, 3)]);
                assert_eq!(purge.commits(), 2);
                assert!(purge.lists(2) && purge.lists(3) && !purge.lists(1));
            }
            other => panic!("expected a purge, got {other:?}"),
        }
        let text = fs::read_to_string(dir.path().join("oplog/000000000002")).unwrap();
        assert!(!text.contains("b3:"), "a stub names no ID: {text}");
        assert_eq!(files(&dir.path().join("tmp")), Vec::<String>::new());
    }

    #[test]
    fn replacing_a_stub_again_changes_nothing_and_a_purge_cannot_be_replaced() {
        let dir = TempDir::new();
        let log = purged_log(&dir);
        let before = fs::read(dir.path().join("oplog/000000000002")).unwrap();
        log.replace_with_stub(2).unwrap();
        assert_eq!(
            fs::read(dir.path().join("oplog/000000000002")).unwrap(),
            before
        );
        assert!(matches!(
            log.replace_with_stub(4),
            Err(OpLogError::Entry(EntryError::PurgeNotReplaceable(4)))
        ));
        assert!(matches!(
            log.replace_with_stub(9),
            Err(OpLogError::Missing { .. })
        ));
    }

    #[test]
    fn a_stub_that_no_purge_lists_is_an_error_of_the_log() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("a", None, A)).unwrap();
        log.append(&change("b", None, B)).unwrap();
        log.replace_with_stub(1).unwrap();
        let error = log.entries().unwrap_err();
        assert!(
            matches!(error, OpLogError::UnlistedStub { number: 1 }),
            "{error:?}"
        );
        assert!(error.to_string().contains("no purge entry lists it"));
    }

    #[test]
    fn a_listed_entry_that_is_not_a_stub_yet_is_no_error() {
        let dir = TempDir::new();
        let log = log_in(&dir);
        log.append(&change("a", None, A)).unwrap();
        log.append_purge(Purge::new(TIME, human(), 1, &[1]))
            .unwrap();
        assert_eq!(
            log.entries().unwrap().len(),
            2,
            "a purge stopped before its stubs"
        );
    }

    #[test]
    fn a_purge_must_come_after_the_entries_it_lists() {
        let purge = Record::Purge(Purge::new(TIME, human(), 1, &[2, 5]));
        assert_eq!(
            Entry::of(5, purge.clone()),
            Err(EntryError::StubNotBefore(5))
        );
        Entry::of(6, purge).unwrap();
    }

    #[test]
    fn a_long_list_of_stubs_is_split_over_entries_that_fit() {
        // Every second number, so that no two merge into a range.
        let numbers: Vec<u64> = (1..=20_000).map(|n| n * 2 + 100_000_000_000).collect();
        let parts = Purge::new(TIME, human(), 7, &numbers).split();
        assert!(parts.len() > 1, "{} parts", parts.len());
        assert_eq!(parts[0].commits(), 7);
        assert!(parts[1..].iter().all(|part| part.commits() == 0));
        let listed: usize = parts.iter().map(|part| part.stubs().len()).sum();
        assert_eq!(listed, numbers.len());
        for part in &parts {
            let size = Entry::of(MAX_NUMBER, Record::Purge(part.clone()))
                .unwrap()
                .encode()
                .len() as u64;
            assert!(size <= MAX_ENTRY_LEN, "{size}");
        }
    }

    #[test]
    fn the_outcomes_skip_stubs_and_purges() {
        let first = entry(1, None, vec![moves("main", None, Some(A))]);
        let stub = first.to_stub().unwrap();
        let stub = Entry { number: 2, ..stub };
        let purge = Entry::of(3, Record::Purge(Purge::new(TIME, human(), 1, &[2]))).unwrap();
        let result = outcomes(&[first, stub, purge], now(&[("main", A)]).as_ref());
        assert_eq!(result, [Outcome::Took, Outcome::Unknown, Outcome::Unknown]);
    }
}
