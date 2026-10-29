//! References: the branches and HEAD of a repository (ADR-008).
//!
//! A branch is a name for a commit. The names and the files follow the rule of ADR-005: one
//! value, one encoding, and a reader that rejects everything else.
//!
//! The files live below the repository's metadata directory (`root`):
//!
//! ```text
//! <root>/HEAD                    branch main\n   or   commit b3:<64 hex digits>\n
//! <root>/refs/heads/<name>       b3:<64 hex digits>\n
//! <root>/refs.lock               exists while a command writes (ADR-014)
//! <root>/refs.lock.guard         stays; its holder holds a lock of the system on it (ADR-008)
//! ```
//!
//! A [`BranchName`] is valid by construction, as an [`Actor`] is. A branch that has
//! no file does not exist yet; HEAD may name such a branch, as it does in a new repository.
//!
//! A change is a compare-and-swap: [`RefStore::update_branch`] takes the commit that the caller
//! saw and fails with [`RefError::Stale`] if the branch has moved since. It checks inside that the
//! new commit is a commit that the object store has, so that no caller can point a branch at an
//! object that is missing, or at a tree. The check, the comparison, and the write happen while one
//! lock file exists, so two processes cannot overtake each other.

use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::actor::ActorParseError;
use crate::object::{CommitId, Kind, ObjectId, ObjectIdParseError};
use crate::oplog::Pointer;
use crate::store::{
    create_dirs_synced, create_temp, flush_directory, remove_scratch, unique_temp_name,
    ObjectStore, StoreError,
};
use crate::verify::{RefVerifyError, VerifyError};
use crate::{Actor, Allowed};

/// The longest branch name in bytes. A file name is limited by the file system, which is 255 bytes
/// for the usual ones, and this stays below that. The path of a branch has the directory of the
/// repository in front of it, so a long name in a deep directory can still be too long on a
/// platform with a short limit; such a path is an error of the file system when it is used.
pub const MAX_BRANCH_NAME_LEN: usize = 200;

/// The rule that a rejected branch name violates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BranchNameProblem {
    /// The name is empty.
    Empty,
    /// The name is longer than [`MAX_BRANCH_NAME_LEN`] bytes.
    TooLong,
    /// The name starts or ends with `/` or contains `//`.
    EmptyComponent,
    /// A character is not one of `A-Z a-z 0-9 . _ -` and `/`. Carries the first such character.
    InvalidCharacter(char),
    /// A component starts with `.` or `-`. This includes `.` and `..`.
    LeadingDotOrDash,
    /// A component ends with `.`, which Windows drops from a file name.
    TrailingDot,
    /// A component is the name of a device on Windows, such as `con` or `com1`, with or without an
    /// extension and in any case. A file with such a name cannot be created there.
    ReservedName,
}

impl fmt::Display for BranchNameProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the name is empty"),
            Self::TooLong => write!(f, "the name is longer than {MAX_BRANCH_NAME_LEN} bytes"),
            Self::EmptyComponent => f.write_str("the name has an empty component"),
            Self::InvalidCharacter(c) => write!(
                f,
                "the character `{}` is not allowed; use letters, digits, `.`, `_`, `-`, and `/`",
                c.escape_debug()
            ),
            Self::LeadingDotOrDash => f.write_str("a component starts with `.` or `-`"),
            Self::TrailingDot => f.write_str("a component ends with `.`"),
            Self::ReservedName => f.write_str("a component is the name of a device on Windows"),
        }
    }
}

/// Why a branch name is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BranchNameError {
    /// The rejected name.
    pub name: String,
    /// The rule it violates.
    pub problem: BranchNameProblem,
}

impl fmt::Display for BranchNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid branch name `{}`: {}",
            self.name.escape_debug(),
            self.problem
        )
    }
}

impl Error for BranchNameError {}

/// Whether the name is a device name on Windows: `CON`, `PRN`, `AUX`, `NUL`, `COM1` to `COM9`,
/// and `LPT1` to `LPT9`, in any case, with or without an extension.
fn is_windows_device(component: &str) -> bool {
    let base = component.split('.').next().unwrap_or(component);
    let upper = base.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            upper
                .strip_prefix(prefix)
                .is_some_and(|digit| matches!(digit.as_bytes(), [b'1'..=b'9']))
        })
}

/// Returns the first rule that the name violates, if any.
fn branch_name_problem(name: &str) -> Option<BranchNameProblem> {
    if name.is_empty() {
        return Some(BranchNameProblem::Empty);
    }
    if name.len() > MAX_BRANCH_NAME_LEN {
        return Some(BranchNameProblem::TooLong);
    }
    for component in name.split('/') {
        if component.is_empty() {
            return Some(BranchNameProblem::EmptyComponent);
        }
        if let Some(c) = component
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        {
            return Some(BranchNameProblem::InvalidCharacter(c));
        }
        if component.starts_with(['.', '-']) {
            return Some(BranchNameProblem::LeadingDotOrDash);
        }
        if component.ends_with('.') {
            return Some(BranchNameProblem::TrailingDot);
        }
        if is_windows_device(component) {
            return Some(BranchNameProblem::ReservedName);
        }
    }
    None
}

/// The name of a branch, such as `main` or `feat/TF-792-login`. A name is valid by construction:
/// the components are separated by `/`, each is made of letters, digits, `.`, `_`, and `-`, none
/// starts with `.` or `-` or ends with `.`, none is the name of a device on Windows, and the whole
/// name has at most [`MAX_BRANCH_NAME_LEN`] bytes. The rule is that of a name that is a file path
/// on every platform; loosening it changes no ID (ADR-008).
///
/// The order is that of the bytes of the name, so `Z` comes before `a`, and `a-b` and `a.b` come
/// before `a/b`. `Eq` is exact too: the store, not this type, treats names that differ only in
/// case as one name (see [`RefError::Conflict`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BranchName(String);

impl BranchName {
    /// Checks the name.
    ///
    /// # Errors
    ///
    /// Returns [`BranchNameError`] with the first rule that the name violates.
    pub fn try_new(name: impl Into<String>) -> Result<Self, BranchNameError> {
        let name = name.into();
        match branch_name_problem(&name) {
            Some(problem) => Err(BranchNameError { name, problem }),
            None => Ok(Self(name)),
        }
    }

    /// Returns the name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for BranchName {
    type Err = BranchNameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::try_new(name)
    }
}

/// What is wrong with the shape of a file of references. The `Display` form is the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RefMalformed {
    /// The file does not end with a newline, or it is empty: it is cut off.
    CutOff,
    /// The file has more than one line.
    ExtraLines,
    /// The line of HEAD is not `<kind> <value>`.
    HeadShape,
    /// The kind in HEAD is neither `branch` nor `commit`.
    UnknownHeadKind,
    /// The second line of the file `working-copy` is not `owner <actor>`.
    OwnerShape,
}

impl fmt::Display for RefMalformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CutOff => "the file is empty or has no terminating newline",
            Self::ExtraLines => "the file has more than one line",
            Self::HeadShape => "expected `branch <name>` or `commit <id>`",
            Self::UnknownHeadKind => "the kind is neither `branch` nor `commit`",
            Self::OwnerShape => "expected `owner <actor>` on the second line",
        })
    }
}

/// Why the content of a file of references is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefParseError {
    /// The content is not valid UTF-8.
    NotUtf8 {
        /// Length of the longest valid prefix, in bytes.
        valid_up_to: usize,
    },
    /// The lines do not have the shape of the file.
    Malformed(RefMalformed),
    /// The object ID is not in canonical form.
    InvalidObjectId(ObjectIdParseError),
    /// The name of a branch is rejected: the one in HEAD, or the name of a file in the directory
    /// of the branches.
    InvalidBranchName(BranchNameError),
    /// The owner in the file `working-copy` is not a valid actor.
    InvalidOwner(ActorParseError),
}

impl fmt::Display for RefParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8 { valid_up_to } => {
                write!(f, "not valid UTF-8 (valid up to byte {valid_up_to})")
            }
            Self::Malformed(reason) => write!(f, "{reason}"),
            Self::InvalidObjectId(source) => write!(f, "invalid object ID: {source}"),
            Self::InvalidBranchName(source) => write!(f, "{source}"),
            Self::InvalidOwner(source) => write!(f, "invalid owner: {source}"),
        }
    }
}

impl Error for RefParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message has the message of the cause; see the crate documentation.
            Self::InvalidObjectId(source) => source.source(),
            Self::InvalidBranchName(source) => source.source(),
            Self::InvalidOwner(source) => source.source(),
            Self::NotUtf8 { .. } | Self::Malformed(_) => None,
        }
    }
}

/// Returns the one line of a file, without its newline.
fn single_line(bytes: &[u8]) -> Result<&str, RefParseError> {
    let text = std::str::from_utf8(bytes).map_err(|error| RefParseError::NotUtf8 {
        valid_up_to: error.valid_up_to(),
    })?;
    let line = text
        .strip_suffix('\n')
        .ok_or(RefParseError::Malformed(RefMalformed::CutOff))?;
    if line.contains('\n') {
        return Err(RefParseError::Malformed(RefMalformed::ExtraLines));
    }
    Ok(line)
}

/// The content of the file of a branch: the ID of the commit and a newline.
pub(crate) fn encode_branch(commit: &CommitId) -> Vec<u8> {
    format!("{commit}\n").into_bytes()
}

/// Reads the file of a branch. It accepts what [`encode_branch`] writes and nothing else.
pub(crate) fn parse_branch(bytes: &[u8]) -> Result<CommitId, RefParseError> {
    let id = ObjectId::from_str(single_line(bytes)?).map_err(RefParseError::InvalidObjectId)?;
    Ok(CommitId::new(id))
}

/// The content of the file `working-copy`: the ID of the working-copy commit and a newline, as
/// in the file of a branch, and from format 5 on the line `owner <actor>` with the exact actor
/// string of ADR-002 (ADR-010). A file without an owner is that of a repository from before
/// format 5.
pub(crate) fn encode_working_copy(commit: &CommitId, owner: Option<&Actor>) -> Vec<u8> {
    match owner {
        Some(owner) => format!("{commit}\nowner {owner}\n"),
        None => format!("{commit}\n"),
    }
    .into_bytes()
}

/// Reads the file `working-copy`. It accepts what [`encode_working_copy`] writes and nothing else:
/// the actor reads back to exactly the line it was read from.
pub(crate) fn parse_working_copy(bytes: &[u8]) -> Result<(CommitId, Option<Actor>), RefParseError> {
    let Some(first) = bytes.iter().position(|&byte| byte == b'\n') else {
        return parse_branch(bytes).map(|commit| (commit, None));
    };
    let (commit, rest) = bytes.split_at(first + 1);
    let commit = parse_branch(commit)?;
    if rest.is_empty() {
        return Ok((commit, None));
    }
    let line = single_line(rest)?;
    let owner = line
        .strip_prefix("owner ")
        .ok_or(RefParseError::Malformed(RefMalformed::OwnerShape))?;
    let owner = Actor::from_str(owner).map_err(RefParseError::InvalidOwner)?;
    Ok((commit, Some(owner)))
}

/// What HEAD points to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// A branch, which may not exist yet.
    Branch(BranchName),
    /// A commit, without a branch ("detached").
    Detached(CommitId),
}

impl Head {
    /// The content of the file HEAD: `branch <name>` or `commit <id>` and a newline.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Branch(name) => format!("branch {name}\n"),
            Self::Detached(commit) => format!("commit {commit}\n"),
        }
        .into_bytes()
    }

    /// Reads the file HEAD. It accepts what [`encode`](Self::encode) writes and nothing else, so
    /// `parse(bytes).encode() == bytes` for every accepted input.
    ///
    /// # Errors
    ///
    /// Returns [`RefParseError`] for input that is not UTF-8, has no newline at its end or more
    /// than one line, is not `<kind> <value>`, has a kind other than `branch` and `commit`, a
    /// branch name that [`BranchName::try_new`] rejects, or an object ID that is not canonical.
    pub fn parse(bytes: &[u8]) -> Result<Self, RefParseError> {
        let line = single_line(bytes)?;
        let (kind, value) = line
            .split_once(' ')
            .ok_or(RefParseError::Malformed(RefMalformed::HeadShape))?;
        match kind {
            "branch" => BranchName::try_new(value)
                .map(Self::Branch)
                .map_err(RefParseError::InvalidBranchName),
            "commit" => ObjectId::from_str(value)
                .map(|id| Self::Detached(CommitId::new(id)))
                .map_err(RefParseError::InvalidObjectId),
            _ => Err(RefParseError::Malformed(RefMalformed::UnknownHeadKind)),
        }
    }
}

/// Why an operation on references fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum RefError {
    /// The file system failed.
    Io {
        /// What was being done, for example "read the reference".
        operation: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error of the file system. Its message ends the message of this error.
        source: io::Error,
    },
    /// A file of references is not in canonical form.
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        source: RefParseError,
    },
    /// The branch is not where the caller saw it: another change came first.
    Stale {
        /// The branch.
        name: BranchName,
        /// The commit the caller expected. `None` means that the branch has to be new.
        expected: Option<CommitId>,
        /// The commit the branch has, or `None` if it does not exist.
        found: Option<CommitId>,
    },
    /// The name cannot be created because of another entry: a branch is a file, so a name cannot
    /// be a branch and the start of other names, and on a file system that ignores case two names
    /// that differ only in case are one file.
    Conflict {
        /// The name that was to be created.
        name: BranchName,
        /// The entry that is in the way, as a name below the branches. It is not always a branch;
        /// `kind` says what it is.
        with: String,
        /// What `with` is.
        kind: ConflictKind,
    },
    /// The object store has no commit with this ID.
    MissingCommit(CommitId),
    /// The object with this ID is not a commit.
    NotACommit {
        /// The ID that was given.
        id: ObjectId,
        /// The kind the object has.
        found: Kind,
    },
    /// The object store failed in a way that is not a failure of the file system, for example a
    /// damaged object.
    Store(Box<StoreError>),
    /// The change was made, but the directory could not be flushed to disk afterwards, so it may
    /// not survive a crash. The caller must not repeat the change with the old state, which is
    /// gone: read the reference again.
    NotFlushed {
        /// The directory that could not be flushed.
        path: PathBuf,
        /// The error of the file system. Its message ends the message of this error.
        source: io::Error,
    },
    /// A lock file exists that this command may not take over: another process changes the
    /// references, or a process that ended left it in a way that cannot be told from a running one
    /// (see [`LockHolder`]).
    Locked {
        /// The lock file.
        path: PathBuf,
        /// What is known about the one that holds the lock.
        holder: LockHolder,
    },
    /// There is no HEAD.
    NoHead {
        /// The file that does not exist.
        path: PathBuf,
    },
    /// There is no working-copy commit. `init` records one, so the file was removed or the
    /// repository was made before the working-copy commit existed (ADR-010). A snapshot cannot
    /// start without it; there is no command yet that makes the file again.
    NoWorkingCopy {
        /// The file that does not exist.
        path: PathBuf,
    },
    /// The working-copy commit is not where the caller saw it: another change came first.
    StaleWorkingCopy {
        /// The commit the caller expected.
        expected: CommitId,
        /// The commit the working copy has.
        found: CommitId,
    },
    /// A purge has not finished: its journal exists (ADR-014). No other change may write until
    /// `stemma purge --resume` has finished it or `stemma purge --abort` has undone it.
    PurgePending {
        /// The journal.
        path: PathBuf,
    },
    /// A pointer that a purge moves is at neither the value before the purge nor the one after
    /// it: another change came first, after the lock was removed by hand (ADR-014).
    PurgeStale {
        /// The pointer: `HEAD`, `working-copy`, or the name of a branch.
        pointer: String,
    },
}

/// What is known about the holder of a lock that is in the way (ADR-008).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LockHolder {
    /// A process that is running holds the lock: it holds the guard of the lock, or it made the
    /// lock file in the moment after this command removed a stale one.
    Running {
        /// The process ID that the lock file names, if it names one. The file is read after the
        /// guard was found held, so it names the holder unless that holder is just replacing it.
        process: Option<u32>,
    },
    /// This process holds the lock already, through another value of the same store. The lock is
    /// not re-entrant, so this is a mistake of the caller and not a state of the repository.
    ThisProcess,
    /// A command on another host made the lock. A lock of the operating system may not reach
    /// from one host to another on a file system that two hosts share, so the guard tells
    /// nothing, and the lock is not taken over: the command may run there, or have crashed.
    Elsewhere {
        /// The host that the lock file names.
        host: String,
        /// The process ID that the lock file names.
        process: u32,
    },
    /// No running process is known to hold the lock: the lock file has another form than the one
    /// that a holder of the guard writes. It was made by an older version of stemma, by a command
    /// that could not take the guard, or by hand, and it may be left from a crash. The empty file
    /// that a crash leaves between making the lock and writing it has this form as well.
    Unknown,
    /// This command could not take the guard of the lock, so it cannot tell whether the process
    /// that made the lock file still runs, and it does not take the file over.
    Unverified {
        /// Why the guard could not be taken: the guard file cannot be opened or locked, as on a
        /// file system without locks of files, or the name of this host is not known.
        reason: String,
    },
}

/// What stands in the way of a new branch name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConflictKind {
    /// A branch with a name that is the start of the new one: `a` is a branch, so `a/b` cannot be.
    Branch,
    /// A directory of other branches has the name: `x/y` is a branch, so `x` cannot be.
    Directory,
    /// A name that differs only in case, which is one file on a file system that ignores case.
    CaseVariant,
}

impl fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Branch => "is a branch",
            Self::Directory => "is a directory of branches",
            Self::CaseVariant => "differs only in case",
        })
    }
}

impl fmt::Display for RefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
            Self::Corrupt { path, source } => {
                write!(f, "`{}` is not a valid reference: {source}", path.display())
            }
            Self::Stale {
                name,
                expected,
                found,
            } => write!(
                f,
                "branch `{name}` has moved: {}, but {}",
                expected.as_ref().map_or_else(
                    || "it was expected to be new".to_owned(),
                    |id| format!("it was expected to be at {id}")
                ),
                found.as_ref().map_or_else(
                    || "it does not exist".to_owned(),
                    |id| format!("it is at {id}")
                )
            ),
            Self::Conflict { name, with, kind } => write!(
                f,
                "branch `{name}` conflicts with `{}`, which {kind}",
                with.escape_debug()
            ),
            Self::MissingCommit(id) => write!(f, "the store has no commit {id}"),
            Self::NotACommit { id, found } => {
                write!(f, "object {id} is a {}, not a commit", found.name())
            }
            Self::NotFlushed { path, source } => write!(
                f,
                "the change was made, but the directory `{}` could not be flushed to disk: {source}",
                path.display()
            ),
            Self::Store(source) => write!(f, "{source}"),
            Self::Locked {
                path,
                holder: LockHolder::Running { process },
            } => {
                write!(f, "the references are locked by `{}`: ", path.display())?;
                match process {
                    Some(process) => write!(f, "another stemma command (process {process})")?,
                    None => f.write_str("another stemma command")?,
                }
                f.write_str(" is running and holds the lock; try again when it has ended")
            }
            Self::Locked {
                path,
                holder: LockHolder::Elsewhere { host, process },
            } => write!(
                f,
                "the references are locked by `{}`: a stemma command (process {process}) on the \
                 host `{}` holds the lock, or left it from a crash; if none runs there, the file \
                 can be removed",
                path.display(),
                host.escape_debug()
            ),
            Self::Locked {
                path,
                holder: LockHolder::ThisProcess,
            } => write!(
                f,
                "the references are locked by `{}`: this process holds the lock already, and the \
                 lock is not re-entrant",
                path.display()
            ),
            Self::Locked {
                path,
                holder: LockHolder::Unknown,
            } => write!(
                f,
                "the references are locked by `{}`: another stemma command is running, and holds \
                 the lock while it saves the working directory; if none runs, the file is left \
                 from a crash, of an older version or of a command that could not take the guard \
                 of the lock, and can be removed",
                path.display()
            ),
            Self::Locked {
                path,
                holder: LockHolder::Unverified { reason },
            } => write!(
                f,
                "the references are locked by `{}`, and this command cannot tell whether the \
                 stemma command that made the lock still runs: the guard of the lock cannot be \
                 taken ({reason}); if none runs, the file is left from a crash and can be removed",
                path.display()
            ),
            Self::NoHead { path } => write!(f, "there is no HEAD at `{}`", path.display()),
            Self::NoWorkingCopy { path } => write!(
                f,
                "there is no working-copy commit at `{}`; `init` records one, so the file was removed \
                 or the repository was made before the working-copy commit existed",
                path.display()
            ),
            Self::StaleWorkingCopy { expected, found } => write!(
                f,
                "the working-copy commit has moved: it was expected to be at {expected}, but it \
                 is at {found}"
            ),
            Self::PurgePending { path } => write!(
                f,
                "a purge has not finished (its journal is `{}`); run `stemma purge --resume` to \
                 finish it or `stemma purge --abort` to undo it first",
                path.display()
            ),
            Self::PurgeStale { pointer } => write!(
                f,
                "`{pointer}` is at neither the value before the purge nor the one after it; \
                 another change came first"
            ),
        }
    }
}

impl RefError {
    /// Returns the error of the file system, if that is what failed. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } | Self::NotFlushed { source, .. } => Some(source),
            Self::Store(source) => source.io_error(),
            _ => None,
        }
    }

    /// Returns the error of the object store, if that is what failed.
    pub fn store_error(&self) -> Option<&StoreError> {
        match self {
            Self::Store(source) => Some(source),
            _ => None,
        }
    }
}

impl Error for RefError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message has the message of the cause, so the source is the cause of the cause.
            Self::Io { source, .. } | Self::NotFlushed { source, .. } => source.source(),
            Self::Corrupt { source, .. } => source.source(),
            Self::Store(source) => source.source(),
            Self::Stale { .. }
            | Self::Conflict { .. }
            | Self::MissingCommit(_)
            | Self::NotACommit { .. }
            | Self::Locked { .. }
            | Self::NoHead { .. }
            | Self::NoWorkingCopy { .. }
            | Self::StaleWorkingCopy { .. }
            | Self::PurgePending { .. }
            | Self::PurgeStale { .. } => None,
        }
    }
}

/// Turns an error of the helpers that the object store shares into an error of references.
fn from_store(error: StoreError) -> RefError {
    match error {
        StoreError::Io {
            operation,
            path,
            source,
        } => RefError::Io {
            operation,
            path,
            source,
        },
        other => RefError::Store(Box::new(other)),
    }
}

fn io_error<'a>(
    operation: &'static str,
    path: &'a Path,
) -> impl FnOnce(io::Error) -> RefError + 'a {
    move |source| RefError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

/// Storage for the branches and HEAD of a repository.
///
/// The trait is sealed: only this crate implements it.
pub trait RefStore: crate::store::sealed::Sealed {
    /// Returns what HEAD points to.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::NoHead`] if there is no HEAD, [`RefError::Corrupt`] if the file is not
    /// in canonical form, and [`RefError::Io`] if the storage fails.
    fn head(&self) -> Result<Head, RefError>;

    /// Points HEAD to a branch, which need not exist yet, or to a commit. A commit has to be in
    /// `objects`; for a branch, `objects` is not used. The change replaces HEAD whatever it was:
    /// of two changes that race, the later one wins, and there is no `expected` as for a branch.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::MissingCommit`] if the commit is not in the store,
    /// [`RefError::NotACommit`] if the object is not a commit, [`RefError::Store`] and
    /// [`RefError::Io`] if the store or the storage fails, [`RefError::Locked`] if another change
    /// is in progress, and [`RefError::NotFlushed`] if the change was made but could not be
    /// flushed to disk. Nothing is changed in any other case.
    fn set_head(
        &self,
        head: &Head,
        objects: &dyn ObjectStore,
        allowed: &Allowed,
    ) -> Result<(), RefError>;

    /// Returns the commit of a branch, or `None` if there is no branch with exactly this name.
    /// That includes a name that is held by another entry, such as the branch `Main` for `main`;
    /// creating the branch fails with [`RefError::Conflict`]. A directory without any branch in
    /// it, which a change that stopped leaves behind, holds no name: the next change clears it.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Corrupt`] if the file is not in canonical form and [`RefError::Io`] if
    /// the storage fails.
    fn branch(&self, name: &BranchName) -> Result<Option<CommitId>, RefError>;

    /// Returns all branches with their commits, sorted by name (by the bytes of the name, see
    /// [`BranchName`]). Entries whose name starts with `.` are skipped, and for a directory all
    /// that is below it as well: no component of a branch name can start with a dot, and file
    /// managers and editors leave such files, for example `.DS_Store`. The listing reads without the lock, so
    /// a branch that another change deletes while it runs may be missing from it; an entry that
    /// is gone when it is read is no error.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Corrupt`] for a file that is not a valid reference, also one whose name
    /// is not a valid branch name such as `main~`, and for a directory with such a name; and
    /// [`RefError::Io`] if the storage fails.
    fn branches(&self) -> Result<Vec<(BranchName, CommitId)>, RefError>;

    /// Moves a branch to `new`, or creates it, if it is still at `expected` (`None` for a branch
    /// that does not exist). The commit has to be in `objects`.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::MissingCommit`] if `new` is not in the store,
    /// [`RefError::NotACommit`] if it is another object, [`RefError::Store`] and
    /// [`RefError::Io`] if the store or the storage fails, [`RefError::Locked`] if another change
    /// is in progress, [`RefError::Conflict`] if the name is held by another entry (checked before
    /// `expected`, because such a name has no branch to be at any commit; a directory without any
    /// branch in it is cleared and holds nothing), [`RefError::Stale`] if
    /// the branch is not at `expected`, and [`RefError::Corrupt`] as for
    /// [`branch`](Self::branch). Nothing is changed in any of these cases, except for
    /// [`RefError::NotFlushed`]: the change was made, and the directory could not be flushed.
    fn update_branch(
        &self,
        name: &BranchName,
        expected: Option<CommitId>,
        new: CommitId,
        objects: &dyn ObjectStore,
        allowed: &Allowed,
    ) -> Result<(), RefError>;

    /// Deletes a branch, and the directories above it that this empties, if it is still at
    /// `expected`. A branch that does not exist is [`RefError::Stale`] with `found` `None`. The
    /// rule that the branch of HEAD is not deleted belongs to the layer above.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Locked`] if another change is in progress,
    /// [`RefError::PurgePending`] if the journal of a purge exists (ADR-014),
    /// [`RefError::Conflict`] if the name is held by another entry (checked before `expected`, as
    /// for [`update_branch`](Self::update_branch)), [`RefError::Stale`] if the branch is not at
    /// `expected`, [`RefError::Io`] if the storage fails, and [`RefError::Corrupt`] as for
    /// [`branch`](Self::branch). Nothing is changed in any of these cases, except for
    /// [`RefError::NotFlushed`]: the branch is gone, but may come back after a crash. A caller
    /// must not repeat the delete then, because the repeat is `Stale` with `found` `None`; it
    /// reads the branch instead.
    ///
    /// This changes the pointer without an entry in the operation log. A command that deletes a
    /// branch for a person logs the move through the operation log (ADR-011) and does not call this directly.
    fn delete_branch(
        &self,
        name: &BranchName,
        expected: CommitId,
        allowed: &Allowed,
    ) -> Result<(), RefError>;

    /// Returns the commit that HEAD leads to: the commit of its branch, `None` if that branch does
    /// not exist yet, or the commit that it names. This is two reads without a lock, so HEAD can
    /// change between them; a caller that changes a branch uses [`head`](Self::head) and
    /// [`branch`](Self::branch) and passes what it saw as `expected`.
    ///
    /// # Errors
    ///
    /// See [`head`](Self::head) and [`branch`](Self::branch).
    fn resolve_head(&self) -> Result<Option<CommitId>, RefError> {
        match self.head()? {
            Head::Branch(name) => self.branch(&name),
            Head::Detached(commit) => Ok(Some(commit)),
        }
    }
}

/// References as files below the repository's metadata directory.
#[derive(Debug, Clone)]
pub struct FsRefStore {
    root: PathBuf,
}

/// The lock file while it exists. The file holds a token that only this lock knows (`token` is the
/// whole content of the file, which includes the line `token`), and the file is removed when the
/// value is dropped, but only if it still holds the token: a person may have removed a lock that
/// looked stale, and another process may hold a new one by now.
///
/// Every lock of the process is listed in [`HELD`] while it exists, so that
/// [`release_locks_for_exit`] can remove it when the process is interrupted.
///
/// The guard of the lock (see [`FsRefStore::acquire`]) is held as long as the value exists, and
/// it is released after the file is removed: the operating system releases it with the handle.
struct Lock {
    path: PathBuf,
    token: String,
    guard: Option<fs::File>,
}

/// Which lock file that is in the way [`FsRefStore::acquire`] replaces once it holds the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Takeover {
    /// Only one that a holder of the guard wrote, which is left by a process that ended, and not
    /// while the journal of a purge exists.
    Stale,
    /// Also one of another form and while the journal exists, for `--resume` and `--abort` of a
    /// purge, which take the lock that the purge left. A file that a running process guards,
    /// or that names another host, stays as well.
    Any,
}

/// The first line of a lock file that a holder of the guard writes (ADR-008).
const LOCK_HEADER: &str = "stemma-lock 1";

/// A lock file in the form that a holder of the guard writes: the header, the host, the process
/// ID, and the token, one line each.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LockFile {
    host: String,
    process: u32,
}

impl LockFile {
    /// Parses the form that a holder of the guard writes, and nothing else.
    fn parse(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let rest = text.strip_prefix(LOCK_HEADER)?.strip_prefix("\nhost ")?;
        let (host, rest) = rest.split_once('\n')?;
        let (process, rest) = rest.strip_prefix("pid ")?.split_once('\n')?;
        let token = rest.strip_prefix("token ")?.strip_suffix('\n')?;
        let canonical = valid_host(host)
            && !process.is_empty()
            && process.bytes().all(|byte| byte.is_ascii_digit())
            && (process == "0" || !process.starts_with('0'))
            && !token.is_empty()
            && !token.contains('\n');
        canonical.then_some(Self {
            host: host.to_owned(),
            process: process.parse().ok()?,
        })
    }

    /// Reads the lock file at `path`: `None` if it is gone or has another form.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Io`] if the file cannot be read.
    fn read(path: &Path) -> Result<Option<Self>, RefError> {
        match fs::read(path) {
            Ok(bytes) => Ok(Self::parse(&bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error("read the lock file", path)(error)),
        }
    }
}

/// Whether a host name can stand on a line of a lock file: printable ASCII without spaces.
fn valid_host(host: &str) -> bool {
    !host.is_empty() && host.len() <= 255 && host.bytes().all(|byte| byte.is_ascii_graphic())
}

/// Names that many hosts carry, so that they do not tell two hosts apart: the defaults of a host
/// whose name was never set.
const PLACEHOLDER_HOSTS: [&str; 3] = ["localhost", "localhost.localdomain", "(none)"];

/// The name of this host, once per process, or `None` if it cannot be found, cannot stand in a
/// lock file, or is a placeholder that many hosts carry. Without it a lock is made without a guard
/// (see [`FsRefStore::acquire`]).
///
/// The rule of the takeover assumes that no two hosts that share the repository carry the same
/// name (ADR-008). On macOS the name is the local host name of the system configuration, which
/// stays when the network changes; the name that `hostname` reports there follows the network.
fn host_name() -> Option<&'static str> {
    static HOST: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        let output_of = |program: &str, args: &[&str]| {
            std::process::Command::new(program)
                .args(args)
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
        };
        let name = if cfg!(windows) {
            std::env::var("COMPUTERNAME").ok()
        } else if cfg!(target_os = "macos") {
            output_of("scutil", &["--get", "LocalHostName"]).or_else(|| output_of("hostname", &[]))
        } else {
            fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .or_else(|| output_of("hostname", &[]))
        }?;
        let name = name.trim().to_owned();
        usable_host(&name).then_some(name)
    })
    .as_deref()
}

/// Whether a host name can stand in a lock file and tells hosts apart.
fn usable_host(name: &str) -> bool {
    valid_host(name)
        && !PLACEHOLDER_HOSTS
            .iter()
            .any(|placeholder| name.eq_ignore_ascii_case(placeholder))
}

/// Runs `open` on a file or directory that another change may be deleting or replacing. On
/// Windows, an entry whose deletion or replacement has begun but not ended cannot be opened, and
/// the open fails with "access is denied" for a short moment; it is then repeated until the entry
/// is gone or can be opened, and only a denial that lasts is an error. Elsewhere a denial is final.
fn settled<T>(mut open: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    const ATTEMPTS: u32 = if cfg!(windows) { 50 } else { 1 };
    let mut attempt = 1;
    loop {
        match open() {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied && attempt < ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            result => return result,
        }
    }
}

/// Whether anything is at the path of the lock file, without following a link.
fn exists(path: &Path) -> Result<bool, RefError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect the lock file", path)(error)),
    }
}

/// Makes the lock file with "create new": `None` if something is at the path already.
fn create_lock_file(path: &Path) -> Result<Option<fs::File>, RefError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(error) => Err(io_error("create the lock file", path)(error)),
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let mut held = held_locks();
        // Best effort: a lock that stays behind is reported by the next change.
        remove_if_own(&self.path, &self.token);
        held.retain(|(path, token)| path != &self.path || token != &self.token);
    }
}

/// The locks that this process holds now, each with its path and its token. The list is locked
/// while a lock file is made or removed, so an interruption does not miss a lock that is being
/// made.
static HELD: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

/// The list of held locks. A thread that panicked while it held the list left it consistent, as
/// every change to it is one call.
fn held_locks() -> MutexGuard<'static, Vec<(PathBuf, String)>> {
    HELD.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Removes the lock file if it still holds the token.
fn remove_if_own(path: &Path, token: &str) {
    if fs::read(path).is_ok_and(|content| content == token.as_bytes()) {
        let _ = fs::remove_file(path);
    }
}

/// Removes every lock file that this process holds, for a process that is about to end because
/// it was interrupted (Ctrl-C), so that the next command does not find a lock left behind.
///
/// A command that writes holds the lock from before its first object to after its last move
/// (ADR-014), at least as long as its snapshot takes. The caller ends the process right after the
/// call: the list stays locked, so no lock can be made or released and no pointer moved any more,
/// and a thread that tries waits until the end. A lock file that holds another token (a person
/// removed the lock, and another process holds a new one) stays.
///
/// An interrupted writer is left as a crash leaves it: objects that no commit names are harmless,
/// and an entry without its move is one that did not take effect (ADR-011). A process that ends
/// without a chance to call this (`kill -9`, a crash of the system) leaves its lock behind.
pub fn release_locks_for_exit() {
    let mut held = held_locks();
    release(&mut held);
    // The process ends now; the list stays locked so that no lock is taken in between.
    std::mem::forget(held);
}

/// Removes the lock files of the list that still hold their tokens, and empties the list.
fn release(held: &mut Vec<(PathBuf, String)>) {
    for (path, token) in held.drain(..) {
        remove_if_own(&path, &token);
    }
}

/// The lock of the references while a command that writes holds it, from before its first object
/// to after its last move (ADR-014). A purge cannot start while it is held, and a writer that
/// finds the journal of a purge does not get it, so a purge cannot delete an object that a writer
/// has saved and is about to name. The lock is not re-entrant, so the steps of the writer take
/// this value as the proof that the lock is held.
#[must_use]
pub(crate) struct WriteLock {
    _lock: Lock,
}

/// The lock of the references while a purge holds it, from its plan to its end (ADR-014). The
/// lock is not re-entrant, so the purge moves the pointers through
/// [`FsRefStore::purge_moves`], which takes this value as the proof that the lock is held.
#[must_use]
pub(crate) struct PurgeLock {
    _lock: Lock,
}

impl PurgeLock {
    /// Leaves the lock file in place and releases its guard, as a crash does. A test stops a
    /// purge with it. If the lock has a guard, the file then names process 0, which is no process
    /// of a user, so that it reads as left by a process that ended; a lock without a guard keeps
    /// its form.
    pub(crate) fn leak(mut self) {
        // A process that crashed holds no lock any more, so the lock leaves the list.
        held_locks().retain(|(path, token)| path != &self._lock.path || token != &self._lock.token);
        if let (Some(host), Some(_)) = (host_name(), self._lock.guard.take()) {
            let left = format!(
                "{LOCK_HEADER}\nhost {host}\npid 0\ntoken {}\n",
                unique_temp_name()
            );
            let _ = fs::write(&self._lock.path, left);
        }
        std::mem::forget(self);
    }
}

/// The name of the journal of a purge in the metadata directory (ADR-014).
pub(crate) const PURGE_JOURNAL: &str = "purge-pending";

/// The pointers that a purge moves, each from the value before the purge to the one after it.
/// A purge moves HEAD only when it names a commit, and it neither creates nor deletes a branch.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PurgeMoves {
    pub(crate) head: Option<(CommitId, CommitId)>,
    pub(crate) working_copy: Option<(CommitId, CommitId)>,
    pub(crate) branches: Vec<(BranchName, CommitId, CommitId)>,
}

impl FsRefStore {
    /// Creates a store that keeps its files below `root`, the repository's metadata directory.
    /// The directory is not checked here; every operation checks it.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn head_path(&self) -> PathBuf {
        self.root.join("HEAD")
    }

    fn heads_dir(&self) -> PathBuf {
        self.root.join("refs").join("heads")
    }

    fn branch_path(&self, name: &BranchName) -> PathBuf {
        name.as_str()
            .split('/')
            .fold(self.heads_dir(), |path, component| path.join(component))
    }

    fn working_copy_path(&self) -> PathBuf {
        self.root.join("working-copy")
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join("refs.lock")
    }

    fn guard_path(&self) -> PathBuf {
        self.root.join("refs.lock.guard")
    }

    /// Fails unless `root` is an existing directory.
    fn require_root(&self) -> Result<(), RefError> {
        let operation = "open the metadata directory";
        let metadata = fs::metadata(&self.root).map_err(io_error(operation, &self.root))?;
        if metadata.is_dir() {
            Ok(())
        } else {
            Err(RefError::Io {
                operation,
                path: self.root.clone(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "not a directory"),
            })
        }
    }

    /// Reads a file that is a regular file, or returns `None` if there is none. Anything else at
    /// the path is an error: a directory, a link (which is not followed), or a path that goes
    /// through a file.
    fn read_reference(&self, path: &Path) -> Result<Option<Vec<u8>>, RefError> {
        // The file may be in the middle of its deletion by another change, which on Windows
        // denies even the inspection for a moment.
        match settled(|| fs::symlink_metadata(path)) {
            // A reader without the lock may see another change delete the file between the two
            // calls; it is then gone, as if the first call had found nothing.
            Ok(metadata) if metadata.is_file() => match settled(|| fs::read(path)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(io_error("read the reference", path)(error)),
            },
            Ok(_) => Err(RefError::Io {
                operation: "read the reference",
                path: path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error("inspect the reference", path)(error)),
        }
    }

    /// The commit of a branch file that a lookup or a listing found, or `None` if the file is gone
    /// since: a reader without the lock may see another change delete the branch in between, and
    /// the branch then does not exist.
    fn listed_branch(&self, path: PathBuf) -> Result<Option<CommitId>, RefError> {
        let Some(bytes) = self.read_reference(&path)? else {
            return Ok(None);
        };
        parse_branch(&bytes)
            .map(Some)
            .map_err(|source| RefError::Corrupt { path, source })
    }

    /// Whether a writer holds the lock of the references now, so it may be between its entry in the
    /// log and the move of a pointer (ADR-011). A lock that a crashed writer left counts as held
    /// until the next writer of this host takes it over or a person removes it (see
    /// [`acquire`](Self::acquire)): the check does not
    /// take the guard, since a reader that held it, if only for a moment, would make a writer fail
    /// with [`RefError::Locked`].
    pub(crate) fn is_locked(&self) -> Result<bool, RefError> {
        exists(&self.lock_path())
    }

    fn journal_path(&self) -> PathBuf {
        self.root.join(PURGE_JOURNAL)
    }

    /// Fails with [`RefError::PurgePending`] if the journal of a purge exists (ADR-014), without
    /// the lock. [`lock`](Self::lock) checks it under the lock, which every command that writes
    /// takes before its first object; this check names a pending purge before a command reads
    /// anything else (a purge before its snapshot, `op restore` before it judges the entry).
    ///
    /// # Errors
    ///
    /// Returns [`RefError::PurgePending`], and [`RefError::Io`] if the journal cannot be inspected.
    pub(crate) fn refuse_pending_purge(&self) -> Result<(), RefError> {
        if self.purge_pending()? {
            return Err(RefError::PurgePending {
                path: self.journal_path(),
            });
        }
        Ok(())
    }

    /// Whether the journal of a purge exists (ADR-014). Anything at its path counts.
    fn purge_pending(&self) -> Result<bool, RefError> {
        let path = self.journal_path();
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error("inspect the journal of the purge", &path)(error)),
        }
    }

    /// Takes the lock for a change of the references. The journal of a purge is checked under
    /// the lock, so a change that was waiting for it does not slip in while a purge runs, and a
    /// lock that a purge holds or left behind is reported as the purge (ADR-014). A lock that this
    /// process holds already stays [`LockHolder::ThisProcess`]: that is a mistake of the caller,
    /// whatever else is pending.
    /// If no journal exists, the scratch in `tmp/` that no writer owns any more is then removed
    /// ([`remove_scratch`], ADR-011); a change that a journal refuses leaves it to the resume or
    /// the abort.
    fn lock(&self) -> Result<Lock, RefError> {
        let lock = match self.acquire(Takeover::Stale) {
            Err(locked @ RefError::Locked { .. }) => {
                let this_process = matches!(
                    &locked,
                    RefError::Locked {
                        holder: LockHolder::ThisProcess,
                        ..
                    }
                );
                return Err(if !this_process && self.purge_pending()? {
                    RefError::PurgePending {
                        path: self.journal_path(),
                    }
                } else {
                    locked
                });
            }
            other => other?,
        };
        self.refuse_pending_purge()?;
        remove_scratch(&self.root);
        Ok(lock)
    }

    /// Takes the lock for a command that writes, before its first object (ADR-014): as
    /// [`lock`](Self::lock), so a journal of a purge refuses it. The steps of the command take the
    /// value as the proof that the lock is held.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Locked`] or [`RefError::PurgePending`], and [`RefError::Io`] if the
    /// lock file cannot be made.
    pub(crate) fn lock_writer(&self, _allowed: &Allowed) -> Result<WriteLock, RefError> {
        self.require_root()?;
        self.lock().map(|lock| WriteLock { _lock: lock })
    }

    /// Takes the lock for a purge that starts: as [`lock`](Self::lock), and a journal that exists
    /// is [`RefError::PurgePending`].
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Locked`] or [`RefError::PurgePending`], and [`RefError::Io`] if the
    /// lock file cannot be made.
    pub(crate) fn lock_purge(&self, _allowed: &Allowed) -> Result<PurgeLock, RefError> {
        self.require_root()?;
        self.lock().map(|lock| PurgeLock { _lock: lock })
    }

    /// Takes the lock for `--resume` or `--abort` of a purge whose journal exists. A lock that is
    /// there is taken for the one that the purge left behind when it stopped, and is replaced
    /// (ADR-014), unless a running process holds its guard, so that a purge that still runs keeps
    /// it, or the file names another host. A command that cannot take the guard itself replaces
    /// only a file of another form, as a purge without a guard leaves it, and not one that a
    /// holder of the guard wrote. The caller checks the journal before and after, and asks the
    /// authorizer again if the journal under the lock names another blob
    /// (`Repository::take_over`). With the lock held, the scratch in `tmp/` is removed, as by
    /// [`lock`](Self::lock).
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Io`] if the lock file cannot be removed or made, and
    /// [`RefError::Locked`] if a running process holds the guard, if the file names another host,
    /// if this command cannot take the guard and the file has the form of a guarded lock, or if
    /// another process took the lock in between.
    pub(crate) fn take_over_lock(&self, _allowed: &Allowed) -> Result<PurgeLock, RefError> {
        self.require_root()?;
        let lock = self.acquire(Takeover::Any)?;
        remove_scratch(&self.root);
        Ok(PurgeLock { _lock: lock })
    }

    /// Makes the lock file, without looking at the journal of a purge, and holds its guard
    /// (ADR-008).
    ///
    /// The guard is the file `refs.lock.guard`, which stays, and the process holds a lock of the
    /// operating system on it while the lock file is its own. The operating system releases that
    /// lock when the process ends in any way, also by `kill -9` or a crash. A lock file whose
    /// guard is free was therefore left by a process that ended, if a holder of the guard on this
    /// host wrote it: only such a file is replaced, and with [`Takeover::Stale`] only while no
    /// journal of a purge exists. That holds also for a file that names the ID of this process,
    /// which an earlier process had: a lock of this process is found in [`HELD`] first, and the
    /// guard is not opened a second time, since closing a second handle releases the lock of the
    /// first where the system keeps its locks per process (`fcntl` on NFS). A file that names
    /// another host stays, since a lock of the operating system may not reach across a file
    /// system that two hosts share. A file of another form may be held by an older version of
    /// stemma, and it stays as well, unless [`Takeover::Any`] takes it for the purge.
    ///
    /// If the guard cannot be locked, or the name of the host is not known, the lock is made
    /// without a guard, in the form of an older version, so that no other command takes it over.
    /// Such a command takes no lock over either, except that [`Takeover::Any`] replaces a file of
    /// another form, as `--resume` and `--abort` did before the guard existed.
    fn acquire(&self, takeover: Takeover) -> Result<Lock, RefError> {
        let path = self.lock_path();
        let host = host_name();
        // The list is held from the file to its entry, so an interruption sees either both or none.
        let mut held = held_locks();
        let locked = |holder| RefError::Locked {
            path: path.clone(),
            holder,
        };
        if self.held_here(&held) {
            return Err(locked(LockHolder::ThisProcess));
        }
        let guard = match host {
            Some(_) => self.guard()?,
            None => Err("the name of this host is not known".to_owned()),
        };
        let token = match (&guard, host) {
            (Ok(_), Some(host)) => format!(
                "{LOCK_HEADER}\nhost {host}\npid {}\ntoken {}\n",
                std::process::id(),
                unique_temp_name()
            ),
            _ => unique_temp_name(),
        };
        let mut file = match create_lock_file(&path)? {
            Some(file) => file,
            None => {
                let found = LockFile::read(&path)?;
                let gone = found.is_none() && !exists(&path)?;
                let replace = match (&guard, found) {
                    // The holder removed it in between; the file is made anew.
                    _ if gone => false,
                    // Without the guard nothing tells whether the holder runs. A purge without a
                    // guard leaves a file of another form, which `--resume` and `--abort` replace.
                    (Err(_), None) if takeover == Takeover::Any => true,
                    (Err(reason), _) => {
                        return Err(locked(LockHolder::Unverified {
                            reason: reason.clone(),
                        }))
                    }
                    (Ok(_), Some(found)) if Some(found.host.as_str()) != host => {
                        return Err(locked(LockHolder::Elsewhere {
                            host: found.host,
                            process: found.process,
                        }))
                    }
                    // A purge that stopped keeps its lock for `--resume` or `--abort`;
                    // [`lock`](Self::lock) reports the purge.
                    (Ok(_), Some(_)) if takeover == Takeover::Stale && self.purge_pending()? => {
                        return Err(locked(LockHolder::Unknown))
                    }
                    // Left by a process of this host that ended, also one that had the ID of this
                    // process: this process holds no lock here, as [`HELD`] shows.
                    (Ok(_), Some(_)) => true,
                    (Ok(_), None) if takeover == Takeover::Any => true,
                    (Ok(_), None) => return Err(locked(LockHolder::Unknown)),
                };
                if replace {
                    match fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(io_error("remove the lock file", &path)(error)),
                    }
                }
                // A command without the guard may make the file in between; it holds it then.
                create_lock_file(&path)?
                    .ok_or_else(|| locked(LockHolder::Running { process: None }))?
            }
        };
        // The lock exists now. If the token cannot be written, the lock is removed again, and the
        // error is reported.
        if let Err(error) = file.write_all(token.as_bytes()) {
            let _ = fs::remove_file(&path);
            return Err(io_error("write the lock file", &path)(error));
        }
        held.push((path.clone(), token.clone()));
        Ok(Lock {
            path,
            token,
            guard: guard.ok(),
        })
    }

    /// Whether this process holds the lock of these references already, under any spelling of
    /// the path of the metadata directory.
    fn held_here(&self, held: &[(PathBuf, String)]) -> bool {
        let path = self.lock_path();
        let canonical = |lock: &Path| {
            lock.parent()
                .and_then(|dir| fs::canonicalize(dir).ok())
                .map(|dir| dir.join("refs.lock"))
        };
        held.iter().any(|(other, _)| {
            *other == path || canonical(other).is_some_and(|other| Some(other) == canonical(&path))
        })
    }

    /// Opens the guard of the lock and takes the lock of the operating system on it, or returns
    /// why it cannot: the guard cannot be opened, or cannot be locked for another reason than a
    /// holder, as on a file system without locks of files. The lock is then made in the form of
    /// an older version, which no command takes over, so that a guard that fails costs the
    /// takeover and never the exclusion; the reason is reported if another command finds the
    /// lock while this one cannot take the guard.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Locked`] with [`LockHolder::Running`] if a running process holds the
    /// guard. A guard that cannot be opened, such as one that another user made and this one may
    /// only read, is no error: the lock is made without it.
    fn guard(&self) -> Result<Result<fs::File, String>, RefError> {
        let path = self.guard_path();
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            // A lock of the operating system needs no right to write.
            .or_else(|_| fs::File::open(&path));
        let file = match opened {
            Ok(file) => file,
            Err(error) => {
                return Ok(Err(format!(
                    "`{}` cannot be opened: {error}",
                    path.display()
                )))
            }
        };
        match file.try_lock() {
            Ok(()) => Ok(Ok(file)),
            Err(fs::TryLockError::WouldBlock) => Err(RefError::Locked {
                path: self.lock_path(),
                holder: LockHolder::Running {
                    // A detail of the message: a file that cannot be read names no process.
                    process: LockFile::read(&self.lock_path())
                        .ok()
                        .flatten()
                        .map(|lock| lock.process),
                },
            }),
            Err(fs::TryLockError::Error(error)) => Ok(Err(format!(
                "`{}` cannot be locked: {error}",
                path.display()
            ))),
        }
    }

    /// Replaces the file with the content, atomically: the content goes to a temporary file, is
    /// flushed, and is renamed over the file. The file is not made read-only, because Windows
    /// refuses to replace one.
    ///
    /// The temporary file is made first, and the directories of the file after it, so that a
    /// failure of the first leaves no directory behind. If a later step fails, the directories
    /// that are empty again are removed up to `top`, which stays. Once the file is in place the
    /// change has happened: a failure to flush the directory is [`RefError::NotFlushed`].
    fn write_replacing(&self, path: &Path, content: &[u8], top: &Path) -> Result<(), RefError> {
        let temp_dir = self.root.join("tmp");
        let parent = path.parent().ok_or_else(|| RefError::Io {
            operation: "find the directory of",
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "the path has no parent"),
        })?;
        create_dirs_synced(&self.root, &temp_dir).map_err(from_store)?;
        let (mut file, mut temp) =
            create_temp(&temp_dir, &mut unique_temp_name).map_err(from_store)?;
        file.write_all(content)
            .map_err(io_error("write the temporary file", &temp.path))?;
        file.sync_all()
            .map_err(io_error("flush the temporary file", &temp.path))?;
        drop(file);
        {
            // The list of held locks is held around the move. An interruption keeps the list
            // once it has removed the locks, so a move waits for the end of the process and
            // does not happen after another process took the lock (ADR-014).
            let _held = held_locks();
            self.place(&temp.path, path, parent, top, |from, to| {
                fs::rename(from, to)
            })?;
        }
        temp.disarm();
        flush_directory(parent).map_err(|source| RefError::NotFlushed {
            path: parent.to_path_buf(),
            source,
        })
    }

    /// Makes the directories of `to` and moves `from` to it with `rename`. If a step fails, the
    /// directories that are empty again are removed up to `top`, which stays. The move is a
    /// parameter so that a test can make it fail.
    fn place(
        &self,
        from: &Path,
        to: &Path,
        parent: &Path,
        top: &Path,
        rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<(), RefError> {
        let placed = create_dirs_synced(&self.root, parent)
            .map_err(from_store)
            .and_then(|()| {
                rename(from, to).map_err(io_error("move the reference into place at", to))
            });
        if placed.is_err() {
            remove_empty_directories(parent, top);
        }
        placed
    }

    /// Whether the directory of the branches exists. `refs` and `refs/heads` are each checked for
    /// what they are, because the error for a path that goes through a file is not the same on
    /// every platform (Windows says "not found"), and a file in the place of a directory is
    /// damage and not "no branches".
    fn heads_exist(&self) -> Result<bool, RefError> {
        for dir in [self.root.join("refs"), self.heads_dir()] {
            match fs::symlink_metadata(&dir) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(RefError::Io {
                        operation: "list the branches in",
                        path: dir,
                        source: io::Error::new(io::ErrorKind::InvalidInput, "not a directory"),
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(io_error("inspect the directory", &dir)(error)),
            }
        }
        Ok(true)
    }

    /// Finds out what the name is in the file system, comparing every component **exactly**. On a
    /// file system that ignores case, `main` would open the file of `Main`; the listing of each
    /// directory shows the case that is stored, so such a name is found to be in conflict and not
    /// to be the other branch. A variant that is a directory without any file below it is a
    /// leftover, as for an exact name, and is reported as [`Lookup::EmptyDirectory`].
    fn lookup(&self, name: &BranchName) -> Result<Lookup, RefError> {
        if !self.heads_exist()? {
            return Ok(Lookup::Absent);
        }
        let components: Vec<&str> = name.as_str().split('/').collect();
        let mut dir = self.heads_dir();
        let mut relative = String::new();
        for (index, component) in components.iter().enumerate() {
            // Every directory but the first was seen to be a directory, and the first has to be
            // one if it exists: `refs` or `heads` as a file is damage and not "no branches".
            let entries = match settled(|| fs::read_dir(&dir)) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Lookup::Absent),
                Err(error) => return Err(io_error("list the branches in", &dir)(error)),
            };
            let (mut exact, mut variant) = (false, None);
            for entry in entries {
                let entry = entry.map_err(io_error("list the branches in", &dir))?;
                let found = entry.file_name().to_string_lossy().into_owned();
                if found == *component {
                    exact = true;
                } else if found.eq_ignore_ascii_case(component) && variant.is_none() {
                    variant = Some(found);
                }
            }
            if !exact {
                let Some(found) = variant else {
                    return Ok(Lookup::Absent);
                };
                // A directory that differs in case and holds no branch is a leftover like any
                // other, and a change clears it: on a file system that ignores case it would
                // hold the place of the name.
                let there = dir.join(&found);
                return match fs::symlink_metadata(&there) {
                    Ok(metadata) if metadata.is_dir() && !has_files_below(&there)? => {
                        Ok(Lookup::EmptyDirectory(there))
                    }
                    Ok(_) => Ok(Lookup::Conflict(
                        join_name(&relative, &found),
                        ConflictKind::CaseVariant,
                    )),
                    // Gone since the listing: a change removed it, and nothing holds the name.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Lookup::Absent),
                    Err(error) => Err(io_error("inspect the branch", &there)(error)),
                };
            }
            let here = dir.join(component);
            let current = join_name(&relative, component);
            let last = index + 1 == components.len();
            match fs::symlink_metadata(&here) {
                Ok(metadata) if metadata.is_dir() => {
                    if last {
                        // A directory holds the name, unless no branch is below it: that is what
                        // a change leaves behind that stopped, and it is cleared by the next one.
                        return if has_files_below(&here)? {
                            Ok(Lookup::Conflict(current, ConflictKind::Directory))
                        } else {
                            Ok(Lookup::EmptyDirectory(here))
                        };
                    }
                }
                Ok(_) if last => return Ok(Lookup::Branch(here)),
                // A file holds the place of a directory in the name.
                Ok(_) => return Ok(Lookup::Conflict(current, ConflictKind::Branch)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Lookup::Absent),
                Err(error) => return Err(io_error("inspect the branch", &here)(error)),
            }
            dir = here;
            relative = current;
        }
        Ok(Lookup::Absent)
    }

    /// The commit of the branch, and what stands in the way of creating it if there is none.
    fn current(&self, name: &BranchName) -> Result<Current, RefError> {
        match self.lookup(name)? {
            Lookup::Absent => Ok(Current::default()),
            Lookup::EmptyDirectory(path) => Ok(Current {
                empty_directory: Some(path),
                ..Current::default()
            }),
            Lookup::Conflict(with, kind) => Ok(Current {
                in_the_way: Some((with, kind)),
                ..Current::default()
            }),
            // A reader without the lock may find the file gone, deleted by a change since the
            // lookup: the branch does not exist then.
            Lookup::Branch(path) => Ok(Current {
                commit: self.listed_branch(path)?,
                ..Current::default()
            }),
        }
    }

    /// Fails unless the object store has the commit.
    pub(crate) fn require_commit(
        commit: &CommitId,
        objects: &dyn ObjectStore,
    ) -> Result<(), RefError> {
        match objects.resolve_commit(&commit.object_id()) {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(RefError::MissingCommit(*commit)),
            Err(StoreError::KindMismatch { id, found, .. }) => {
                Err(RefError::NotACommit { id, found })
            }
            Err(error) => Err(from_store(error)),
        }
    }
}

/// What a branch name is in the file system.
enum Lookup {
    /// Nothing is there, and nothing is in the way.
    Absent,
    /// The file of the branch.
    Branch(PathBuf),
    /// A directory without any file below it holds the name. It is no branch and in nobody's way:
    /// it is what a change leaves behind that stopped. A change removes it.
    EmptyDirectory(PathBuf),
    /// The name cannot be a branch: another branch, or a directory of branches, is in the way, or
    /// a branch, or a directory that holds one, whose name differs only in case. Carries that name
    /// and what it is.
    Conflict(String, ConflictKind),
}

/// What is known about a name: its commit, what is in the way of creating it, and an empty
/// directory to clear before it is created.
#[derive(Default)]
struct Current {
    commit: Option<CommitId>,
    in_the_way: Option<(String, ConflictKind)>,
    empty_directory: Option<PathBuf>,
}

/// Whether a file, or anything but a directory, is below the directory at any depth. A caller
/// that finds `false` may remove the directory with all that is in it, so every doubt is `true`.
/// An entry whose name starts with a dot is not counted, as in the listing of the branches: no
/// branch name starts with a dot, and file managers leave such files, like `.DS_Store`. A caller
/// that removes the directory removes these too, and a directory with a dot name with all that is
/// in it.
/// The directory itself may be gone: a lookup without the lock may race with a change that deletes
/// the last branch below it, and nothing is below it then. A directory below it that was listed
/// and cannot be opened, or an entry whose kind cannot be inspected because it is gone, counts as
/// a file, since it was there a moment ago and may still hold one.
fn has_files_below(dir: &Path) -> Result<bool, RefError> {
    let Some(entries) = list_directory(dir)? else {
        return Ok(false);
    };
    let mut pending = vec![(dir.to_path_buf(), entries)];
    while let Some((dir, entries)) = pending.pop() {
        for entry in entries {
            let entry = entry.map_err(io_error("list the branches in", &dir))?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
                Err(error) => return Err(io_error("inspect the branch", &path)(error)),
            };
            if !kind.is_dir() {
                return Ok(true);
            }
            match list_directory(&path)? {
                Some(below) => pending.push((path, below)),
                None => return Ok(true),
            }
        }
    }
    Ok(false)
}

/// Removes `dir` and its parents while they are empty, up to but not including `top`. It stops
/// at the first directory that is not empty or cannot be removed.
fn remove_empty_directories(dir: &Path, top: &Path) {
    let mut dir = dir;
    while dir != top && dir.starts_with(top) {
        if fs::remove_dir(dir).is_err() {
            return;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return,
        }
    }
}

/// The entries of a directory of branches, or `None` if the directory is gone: a listing runs
/// without the lock, and a change that deletes the last branch below a directory removes the
/// directory (see [`FsRefStore::delete_branch_file`]), so a directory that was listed a moment ago
/// may be gone.
fn list_directory(dir: &Path) -> Result<Option<fs::ReadDir>, RefError> {
    match settled(|| fs::read_dir(dir)) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("list the branches in", dir)(error)),
    }
}

fn join_name(prefix: &str, component: &str) -> String {
    if prefix.is_empty() {
        component.to_owned()
    } else {
        format!("{prefix}/{component}")
    }
}

/// The move of a branch in [`FsRefStore::update_branch_and_working_copy_after`].
pub(crate) struct BranchMove<'a> {
    /// The branch, which need not exist yet.
    pub(crate) name: &'a BranchName,
    /// The commit the branch has to be at, `None` for a branch that does not exist.
    pub(crate) expected: Option<CommitId>,
    /// The commit the branch takes. It differs from `expected`.
    pub(crate) new: CommitId,
}

/// The move of the working-copy commit in [`FsRefStore::update_branch_and_working_copy_after`].
pub(crate) struct WorkingCopyMove {
    /// The commit the working copy has to be at.
    pub(crate) expected: CommitId,
    /// The commit the working copy takes. It differs from `expected`.
    pub(crate) new: CommitId,
}

/// The values that a restore gives the pointers (ADR-013). A `head` or a `working_copy` that is
/// `None` is not touched. A branch is touched when it is listed; its `None` means that it does
/// not exist afterwards.
pub(crate) struct RestoreTargets<'a> {
    /// The value of HEAD.
    pub(crate) head: Option<&'a Head>,
    /// The working-copy commit.
    pub(crate) working_copy: Option<CommitId>,
    /// The branches, each with the commit it takes, or `None` if it does not exist afterwards.
    pub(crate) branches: &'a [(BranchName, Option<CommitId>)],
}

/// The pointers that a restore moves, each as `(value now, value after)`; for a branch, `None`
/// means that it does not exist. A pointer that has its target value already is left out.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RestoreMoves {
    pub(crate) head: Option<(Head, Head)>,
    pub(crate) working_copy: Option<(CommitId, CommitId)>,
    pub(crate) branches: Vec<(BranchName, Option<CommitId>, Option<CommitId>)>,
}

impl RestoreMoves {
    pub(crate) fn is_empty(&self) -> bool {
        self.head.is_none() && self.working_copy.is_none() && self.branches.is_empty()
    }
}

impl crate::store::sealed::Sealed for FsRefStore {}

/// The outcome of a change of a pointer that a step precedes, such as the entry of the operation
/// log (ADR-011). `T` is what the step returned, `E` is how it can fail. Every variant is the only
/// record of what happened, so a caller must look at it.
#[derive(Debug)]
#[must_use]
pub(crate) enum Logged<T, E> {
    /// The pointer has the value already. The step did not run and nothing changed.
    Unchanged,
    /// The step ran and the pointer moved.
    Moved(T),
    /// The step failed. The pointer has not moved. The step can have left an effect of its own,
    /// which `E` says (a log entry that was written and is not known to be safe).
    StepFailed(E),
    /// The step ran, but the pointer did not move: the file system failed. The step stays done.
    MoveFailed(T, RefError),
    /// The step ran and the pointer moved, but the directory could not be flushed to disk
    /// ([`RefError::NotFlushed`]).
    NotFlushed(T, RefError),
    /// The step ran and at least one pointer moved, but a later one did not: the file system
    /// failed. The step stays done, and the entry names a move that was half made.
    /// [`FsRefStore::update_branch_and_working_copy_after`] (two pointers) and
    /// [`FsRefStore::restore_after`] (any number) return it.
    PartlyMoved(T, RefError),
}

impl FsRefStore {
    /// Moves a branch to `new`, or creates it, if it is still at `expected`, and runs `step` after
    /// the checks and before the move, all under the lock of the references that the caller holds
    /// (`_lock`; a command that writes takes it before its first object, ADR-014).
    ///
    /// The checks are the ones of [`RefStore::update_branch`]: a name that is in the way is a
    /// [`RefError::Conflict`], a branch that is not at `expected` is [`RefError::Stale`], and the
    /// commit has to be in `objects`. They run under the lock, so no other change can come between
    /// them and the move, and `step` runs only for a move that is going to be made. The commit is
    /// looked up under the lock as well. If `expected` is `Some(new)`, so that the branch is at
    /// `new` already, `step` does not run; any other branch that is not at `expected` is stale.
    ///
    /// # Errors
    ///
    /// Returns the errors of the checks. Once `step` has run, nothing is an error of this call: the
    /// outcome says what happened, so the caller can tell a step that ran from one that did not.
    /// An empty directory that a stopped change left is removed before the step; if that fails the
    /// error is returned and no step has run.
    pub(crate) fn update_branch_after<T, E>(
        &self,
        _lock: &WriteLock,
        name: &BranchName,
        expected: Option<CommitId>,
        new: CommitId,
        objects: &dyn ObjectStore,
        step: impl FnOnce() -> Result<T, E>,
    ) -> Result<Logged<T, E>, RefError> {
        self.require_root()?;
        Self::require_commit(&new, objects)?;
        let current = self.current(name)?;
        if let Some((with, kind)) = current.in_the_way {
            return Err(RefError::Conflict {
                name: name.clone(),
                with,
                kind,
            });
        }
        if current.commit != expected {
            return Err(RefError::Stale {
                name: name.clone(),
                expected,
                found: current.commit,
            });
        }
        if expected == Some(new) {
            return Ok(Logged::Unchanged);
        }
        if let Some(empty) = &current.empty_directory {
            // Nothing is below it, and the lock is held, so no branch is lost. It goes before the
            // step, so that a failure here leaves no entry that names a move that cannot be made.
            fs::remove_dir_all(empty).map_err(io_error("remove the empty directory", empty))?;
        }
        let done = match step() {
            Ok(done) => done,
            Err(error) => return Ok(Logged::StepFailed(error)),
        };
        let moved = self.write_replacing(
            &self.branch_path(name),
            &encode_branch(&new),
            &self.heads_dir(),
        );
        Ok(Self::after_move(done, moved))
    }

    /// Points HEAD to `head`, and runs `step` with the value that HEAD had, after the checks and
    /// before the move, under the lock of the references. The old value is read under the lock, so
    /// it is the one that the move replaces. If HEAD has the value already, `step` does not run.
    ///
    /// Unlike [`RefStore::set_head`], which replaces HEAD whatever it was, this reads HEAD first,
    /// so a HEAD that is missing or damaged is an error and is not replaced.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::MissingCommit`] or [`RefError::NotACommit`] for a commit that `objects`
    /// does not have, and the errors of [`RefStore::head`] for an unreadable HEAD. Once `step` has
    /// run, nothing is an error of this call; see
    /// [`update_branch_after`](Self::update_branch_after).
    pub(crate) fn set_head_after<T, E>(
        &self,
        _lock: &WriteLock,
        head: &Head,
        objects: &dyn ObjectStore,
        step: impl FnOnce(&Head) -> Result<T, E>,
    ) -> Result<Logged<T, E>, RefError> {
        self.require_root()?;
        if let Head::Detached(commit) = head {
            Self::require_commit(commit, objects)?;
        }
        let old = self.head()?;
        if &old == head {
            return Ok(Logged::Unchanged);
        }
        let done = match step(&old) {
            Ok(done) => done,
            Err(error) => return Ok(Logged::StepFailed(error)),
        };
        let moved = self.write_replacing(&self.head_path(), &head.encode(), &self.root);
        Ok(Self::after_move(done, moved))
    }

    /// The working-copy commit (ADR-010), recorded in the file `working-copy` in the form of a
    /// branch file. `init` writes it, so a missing file is [`RefError::NoWorkingCopy`] and not
    /// "no commit yet".
    ///
    /// # Errors
    ///
    /// Returns [`RefError::NoWorkingCopy`] if the file is missing, [`RefError::Corrupt`] if it is
    /// not in canonical form, and [`RefError::Io`] if the storage fails.
    pub fn working_copy(&self) -> Result<CommitId, RefError> {
        self.working_copy_and_owner().map(|(commit, _)| commit)
    }

    /// The owner of the working copy (ADR-010): the actor that the file `working-copy` names on
    /// its second line, or `None` for a repository from before format 5, whose file names none.
    ///
    /// # Errors
    ///
    /// See [`working_copy`](Self::working_copy).
    pub fn working_copy_owner(&self) -> Result<Option<Actor>, RefError> {
        self.working_copy_and_owner().map(|(_, owner)| owner)
    }

    /// Reads the file `working-copy` once: the commit and the owner. See
    /// [`working_copy`](Self::working_copy) and [`working_copy_owner`](Self::working_copy_owner).
    ///
    /// # Errors
    ///
    /// See [`working_copy`](Self::working_copy).
    pub(crate) fn working_copy_and_owner(&self) -> Result<(CommitId, Option<Actor>), RefError> {
        self.require_root()?;
        let path = self.working_copy_path();
        let bytes = self
            .read_reference(&path)?
            .ok_or_else(|| RefError::NoWorkingCopy { path: path.clone() })?;
        parse_working_copy(&bytes).map_err(|source| RefError::Corrupt { path, source })
    }

    /// Moves the working-copy commit to `new` and keeps the owner that the file names now, which
    /// it reads again: a step before the move can have recorded one ([`Self::adopt_owner`]).
    fn write_working_copy(&self, new: &CommitId) -> Result<(), RefError> {
        let (_, owner) = self.working_copy_and_owner()?;
        self.write_replacing(
            &self.working_copy_path(),
            &encode_working_copy(new, owner.as_ref()),
            &self.root,
        )
    }

    /// Makes `actor` the owner of the working copy if the file `working-copy` names none, as in a
    /// repository from before format 5 (ADR-010). The writer of an entry calls it under the lock
    /// of the references, after the entry, so the actor of the first entry in format 5 becomes
    /// the owner. A repository without the file has no working copy to own, and is left alone.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Corrupt`] for a file that is not in canonical form, and
    /// [`RefError::Io`] or [`RefError::NotFlushed`] if the storage fails.
    pub(crate) fn adopt_owner(&self, _lock: &WriteLock, actor: &Actor) -> Result<(), RefError> {
        let (commit, owner) = match self.working_copy_and_owner() {
            Ok(read) => read,
            Err(RefError::NoWorkingCopy { .. }) => return Ok(()),
            Err(error) => return Err(error),
        };
        if owner.is_some() {
            return Ok(());
        }
        self.write_replacing(
            &self.working_copy_path(),
            &encode_working_copy(&commit, Some(actor)),
            &self.root,
        )
    }

    /// Makes `owner` the owner of the working copy, and runs `step` before, under the lock of the
    /// references, as [`update_working_copy_after`](Self::update_working_copy_after) does for the
    /// commit: the step writes the entry of the operation `take-over` (ADR-010, ADR-011). If
    /// `owner` owns the working copy already, `step` does not run.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::NoWorkingCopy`] and [`RefError::Corrupt`] for a file that cannot be
    /// read. Once `step` has run, nothing is an error of this call; see
    /// [`update_branch_after`](Self::update_branch_after).
    pub(crate) fn set_owner_after<T, E>(
        &self,
        _lock: &WriteLock,
        owner: &Actor,
        step: impl FnOnce() -> Result<T, E>,
    ) -> Result<Logged<T, E>, RefError> {
        let (_, found) = self.working_copy_and_owner()?;
        if found.as_ref() == Some(owner) {
            return Ok(Logged::Unchanged);
        }
        let done = match step() {
            Ok(done) => done,
            Err(error) => return Ok(Logged::StepFailed(error)),
        };
        // The step may have recorded an owner and so rewritten the file: read the commit again.
        let moved = self.working_copy_and_owner().and_then(|(commit, _)| {
            self.write_replacing(
                &self.working_copy_path(),
                &encode_working_copy(&commit, Some(owner)),
                &self.root,
            )
        });
        Ok(Self::after_move(done, moved))
    }

    /// Checks that every reference names a commit that `objects` has: a detached HEAD, the
    /// working-copy commit, and every branch. A write checks the commit already (ADR-008), so a
    /// problem here comes from outside the module: an object that was removed or damaged, or a
    /// reference file that was edited by hand. HEAD that names a branch is not checked itself; the
    /// branch is, if it exists, and a branch that does not exist yet is no problem, as in a new
    /// repository.
    ///
    /// The result has one error for each reference that fails, in the order HEAD, the
    /// working-copy commit, and the branches in the byte order of their names; it is empty if all
    /// hold. A commit that is missing is [`VerifyError::Missing`]; one that is another kind of
    /// object ([`StoreError::KindMismatch`]), is damaged ([`StoreError::Corrupt`]), or cannot be
    /// read ([`StoreError::Io`]) is [`VerifyError::Store`]. Each commit is read and hashed
    /// completely, but it is not parsed, and its tree and parents are not followed.
    ///
    /// The references are read without the lock, so a change can move a pointer while the check
    /// runs. A pointer that fails is therefore read again after its commit was looked up: one that
    /// has moved is checked at its new commit, and one that is gone is no failure. A change that
    /// moves a pointer and removes its old commit meanwhile, as a purge does (ADR-014), thus
    /// reports nothing, and each failure holds at the time of the second read. If the second read
    /// itself fails, its error is returned, as for the first read.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::PurgePending`] if the journal of a purge exists, before the check and
    /// after it, since a pointer may be between its old and its new commit then (ADR-014).
    /// Returns the errors of reading the references, as [`head`](RefStore::head),
    /// [`working_copy`](Self::working_copy), and [`branches`](RefStore::branches) do: a missing
    /// HEAD or working-copy file, or a reference file that is not in canonical form, is a problem
    /// of the references themselves. All references are read before the first commit is looked
    /// up, so such an error comes without any failure of a commit.
    pub fn verify(&self, objects: &dyn ObjectStore) -> Result<Vec<RefVerifyError>, RefError> {
        self.require_root()?;
        self.refuse_pending_purge()?;
        let mut pointers = Vec::new();
        if let Head::Detached(commit) = self.head()? {
            pointers.push((Pointer::Head, commit));
        }
        pointers.push((Pointer::WorkingCopy, self.working_copy()?));
        for (name, commit) in self.branches()? {
            pointers.push((Pointer::Branch(name), commit));
        }
        let mut errors = Vec::new();
        for (pointer, commit) in pointers {
            if Self::check_commit(&pointer, commit, objects).is_ok() {
                continue;
            }
            // An error of reading wins over the failures found so far, as it does for the first
            // read: a report that mixes a read that failed with commits that are missing would
            // name a cause that may no longer hold.
            let Some(now) = self.commit_of(&pointer)? else {
                continue;
            };
            if let Err(error) = Self::check_commit(&pointer, now, objects) {
                errors.push(error);
            }
        }
        self.refuse_pending_purge()?;
        Ok(errors)
    }

    /// Checks that `objects` has `commit`, the commit of `pointer`.
    fn check_commit(
        pointer: &Pointer,
        commit: CommitId,
        objects: &dyn ObjectStore,
    ) -> Result<(), RefVerifyError> {
        let id = commit.object_id();
        let found = objects.resolve_commit(&id).map(|found| found.is_some());
        VerifyError::check(|| pointer.clone(), id, found)
    }

    /// The commit that a pointer names now: `None` for HEAD that names a branch and for a branch
    /// that does not exist.
    fn commit_of(&self, pointer: &Pointer) -> Result<Option<CommitId>, RefError> {
        match pointer {
            Pointer::Head => Ok(match self.head()? {
                Head::Detached(commit) => Some(commit),
                Head::Branch(_) => None,
            }),
            Pointer::WorkingCopy => self.working_copy().map(Some),
            Pointer::Branch(name) => self.branch(name),
        }
    }

    /// Moves the working-copy commit from `expected` to `new`, and runs `step` after the checks and
    /// before the move, all under the lock of the references, as
    /// [`update_branch_after`](Self::update_branch_after) does for a branch. The commit has to be
    /// in `objects`, and it is looked up under the lock. If `expected` is `new`, so that the working
    /// copy is at `new` already, `step` does not run.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::MissingCommit`] or [`RefError::NotACommit`] for a commit that `objects`
    /// does not have, [`RefError::NoWorkingCopy`] and [`RefError::Corrupt`] for a file that cannot
    /// be read, and [`RefError::StaleWorkingCopy`] if the working-copy commit is not `expected`.
    /// Once `step` has run, nothing is an error of this call; see
    /// [`update_branch_after`](Self::update_branch_after).
    pub(crate) fn update_working_copy_after<T, E>(
        &self,
        _lock: &WriteLock,
        expected: CommitId,
        new: CommitId,
        objects: &dyn ObjectStore,
        step: impl FnOnce() -> Result<T, E>,
    ) -> Result<Logged<T, E>, RefError> {
        self.require_root()?;
        Self::require_commit(&new, objects)?;
        let found = self.working_copy()?;
        if found != expected {
            return Err(RefError::StaleWorkingCopy { expected, found });
        }
        if expected == new {
            return Ok(Logged::Unchanged);
        }
        let done = match step() {
            Ok(done) => done,
            Err(error) => return Ok(Logged::StepFailed(error)),
        };
        let moved = self.write_working_copy(&new);
        Ok(Self::after_move(done, moved))
    }

    /// Moves a branch and the working-copy commit in one operation, and runs `step` after the
    /// checks and before the first move, all under the lock of the references. This is what a
    /// `commit` does (ADR-010): the branch takes the named commit and the working-copy commit takes
    /// the new one, and the log has one entry for both (ADR-011).
    ///
    /// The checks are the ones of [`update_branch_after`](Self::update_branch_after) and
    /// [`update_working_copy_after`](Self::update_working_copy_after), and both commits have to be
    /// in `objects`. The branch moves first: if the process stops between the two moves, the
    /// commit that was named is reachable from the branch and is not lost. Both pointers change, so
    /// there is no unchanged case.
    ///
    /// # Errors
    ///
    /// Returns the errors of the two calls named above. Once `step` has run, nothing is an error of
    /// this call; see [`update_branch_after`](Self::update_branch_after).
    pub(crate) fn update_branch_and_working_copy_after<T, E>(
        &self,
        _lock: &WriteLock,
        branch: &BranchMove<'_>,
        working_copy: &WorkingCopyMove,
        objects: &dyn ObjectStore,
        step: impl FnOnce() -> Result<T, E>,
    ) -> Result<Logged<T, E>, RefError> {
        debug_assert!(
            branch.expected != Some(branch.new) && working_copy.expected != working_copy.new,
            "both pointers must change"
        );
        self.require_root()?;
        Self::require_commit(&branch.new, objects)?;
        Self::require_commit(&working_copy.new, objects)?;
        let current = self.current(branch.name)?;
        if let Some((with, kind)) = current.in_the_way {
            return Err(RefError::Conflict {
                name: branch.name.clone(),
                with,
                kind,
            });
        }
        if current.commit != branch.expected {
            return Err(RefError::Stale {
                name: branch.name.clone(),
                expected: branch.expected,
                found: current.commit,
            });
        }
        let found = self.working_copy()?;
        if found != working_copy.expected {
            return Err(RefError::StaleWorkingCopy {
                expected: working_copy.expected,
                found,
            });
        }
        if let Some(empty) = &current.empty_directory {
            fs::remove_dir_all(empty).map_err(io_error("remove the empty directory", empty))?;
        }
        let done = match step() {
            Ok(done) => done,
            Err(error) => return Ok(Logged::StepFailed(error)),
        };
        let first = self.write_replacing(
            &self.branch_path(branch.name),
            &encode_branch(&branch.new),
            &self.heads_dir(),
        );
        Ok(Self::after_two_moves(done, first, || {
            self.write_working_copy(&working_copy.new)
        }))
    }

    /// Reads the pointers and returns the moves that a restore would make, without a lock and
    /// without a write, and the empty directories that stand where a branch that moves is created
    /// or deleted (`restore_after` removes them). The commits that the targets name have to be in
    /// `objects`.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::MissingCommit`] or [`RefError::NotACommit`] for a commit that `objects`
    /// does not have, [`RefError::Conflict`] for a name that is in the way of a branch that is to
    /// exist afterwards (a branch that is to be deleted is not blocked, and neither is a name that
    /// only branches deleted by the same restore hold: they are deleted first, see
    /// `apply_moves`), and the errors of reading the pointers.
    pub(crate) fn plan_restore(
        &self,
        targets: &RestoreTargets<'_>,
        objects: &dyn ObjectStore,
    ) -> Result<(RestoreMoves, Vec<PathBuf>), RefError> {
        self.require_root()?;
        if let Some(Head::Detached(commit)) = targets.head {
            Self::require_commit(commit, objects)?;
        }
        if let Some(commit) = &targets.working_copy {
            Self::require_commit(commit, objects)?;
        }
        for (_, commit) in targets.branches {
            if let Some(commit) = commit {
                Self::require_commit(commit, objects)?;
            }
        }
        let mut moves = RestoreMoves::default();
        let mut empty_directories = Vec::new();
        if let Some(target) = targets.head {
            let current = self.head()?;
            if &current != target {
                moves.head = Some((current, target.clone()));
            }
        }
        if let Some(target) = targets.working_copy {
            let current = self.working_copy()?;
            if current != target {
                moves.working_copy = Some((current, target));
            }
        }
        let mut currents = Vec::with_capacity(targets.branches.len());
        for (name, _) in targets.branches {
            currents.push(self.current(name)?);
        }
        // The branches that this restore deletes: they are no obstacle for the ones it creates
        // (`a` is deleted and `a/b` created, or the other way round), because the deletions come
        // first (see `apply_moves`).
        let deleted: Vec<&BranchName> = targets
            .branches
            .iter()
            .zip(&currents)
            .filter(|((_, target), current)| target.is_none() && current.commit.is_some())
            .map(|((name, _), _)| name)
            .collect();
        for ((name, target), current) in targets.branches.iter().zip(currents) {
            if let Some((with, kind)) = &current.in_the_way {
                // A branch that is to be deleted is not in the way of itself.
                if target.is_some() && !self.cleared_by(with, *kind, &deleted)? {
                    return Err(RefError::Conflict {
                        name: name.clone(),
                        with: with.clone(),
                        kind: *kind,
                    });
                }
            }
            if current.commit != *target {
                moves.branches.push((name.clone(), current.commit, *target));
                empty_directories.extend(current.empty_directory);
            }
        }
        Ok((moves, empty_directories))
    }

    /// Whether what is in the way of a branch is gone once the branches `deleted` are deleted: a
    /// branch that holds the place of a directory in the name, or every branch below a directory
    /// that holds the name. A name that differs only in case is never cleared, also when the
    /// variant is deleted by the restore: that is a refusal on purpose, and the safe side.
    fn cleared_by(
        &self,
        with: &str,
        kind: ConflictKind,
        deleted: &[&BranchName],
    ) -> Result<bool, RefError> {
        Ok(match kind {
            ConflictKind::Branch => deleted.iter().any(|name| name.as_str() == with),
            ConflictKind::Directory => {
                let prefix = format!("{with}/");
                self.branches()?
                    .iter()
                    .filter(|(name, _)| name.as_str().starts_with(&prefix))
                    .all(|(name, _)| deleted.contains(&name))
            }
            ConflictKind::CaseVariant => false,
        })
    }

    /// Moves the pointers back as a restore does (ADR-013), and runs `step` with the moves after
    /// the checks and before the first move, all under the lock of the references. The step writes
    /// the entry of the log and brings the working directory to the new tree. If the moves are
    /// empty, so that every pointer has its target value, the step does not run.
    ///
    /// The values are read under the lock, so the step sees the ones that the moves replace. Empty
    /// directories that stand where a branch is created or deleted are removed before the step; if
    /// one removal fails, the ones before it stay removed. The branches move first, the deletions
    /// before the creations (so that `a` can give way to `a/b` and the other way round), then the
    /// working-copy commit, then HEAD (ADR-013); a branch whose target is `None` is deleted. A
    /// failure at the first move is [`Logged::MoveFailed`], a failure after a pointer moved is
    /// [`Logged::PartlyMoved`], and a failure to flush a directory
    /// ([`RefError::NotFlushed`]) does not stop the later moves.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`plan_restore`](Self::plan_restore). Once `step` has run, nothing is
    /// an error of this call; see
    /// [`update_branch_after`](Self::update_branch_after).
    pub(crate) fn restore_after<T, E>(
        &self,
        _lock: &WriteLock,
        targets: &RestoreTargets<'_>,
        objects: &dyn ObjectStore,
        step: impl FnOnce(&RestoreMoves) -> Result<T, E>,
    ) -> Result<Logged<T, E>, RefError> {
        self.require_root()?;
        let (moves, empty_directories) = self.plan_restore(targets, objects)?;
        if moves.is_empty() {
            return Ok(Logged::Unchanged);
        }
        for empty in &empty_directories {
            match fs::remove_dir_all(empty) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error("remove the empty directory", empty)(error)),
            }
        }
        let done = match step(&moves) {
            Ok(done) => done,
            Err(error) => return Ok(Logged::StepFailed(error)),
        };
        Ok(self.apply_moves(done, &moves))
    }

    /// Moves the pointers that a purge moves (ADR-014), with the lock that the purge holds, and
    /// runs `step` after the checks and before the first move. The step writes the `purge` entry;
    /// it runs also when no pointer moves, as the entry is written in any case.
    ///
    /// Every pointer is checked first: one that has its value after the purge already is skipped
    /// (a purge that is resumed), one at its value before is moved, and one at any other value is
    /// [`RefError::PurgeStale`] before anything is written. The moves are made in the order of
    /// [`restore_after`](Self::restore_after): branches, the working-copy commit, then HEAD.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::PurgeStale`], [`RefError::MissingCommit`] for a new commit that is not
    /// in `objects`, and the errors of reading the pointers. Once `step` has run, nothing is an
    /// error of this call; see [`update_branch_after`](Self::update_branch_after).
    pub(crate) fn purge_moves<T, E>(
        &self,
        _lock: &PurgeLock,
        planned: &PurgeMoves,
        objects: &dyn ObjectStore,
        step: impl FnOnce() -> Result<T, E>,
    ) -> Result<Logged<T, E>, RefError> {
        self.require_root()?;
        let stale = |pointer: &str| RefError::PurgeStale {
            pointer: pointer.to_owned(),
        };
        let mut moves = RestoreMoves::default();
        if let Some((old, new)) = planned.head {
            Self::require_commit(&new, objects)?;
            let (old, new) = (Head::Detached(old), Head::Detached(new));
            let current = self.head()?;
            if current == old {
                moves.head = Some((old, new));
            } else if current != new {
                return Err(stale("HEAD"));
            }
        }
        if let Some((old, new)) = planned.working_copy {
            Self::require_commit(&new, objects)?;
            let current = self.working_copy()?;
            if current == old {
                moves.working_copy = Some((old, new));
            } else if current != new {
                return Err(stale("working-copy"));
            }
        }
        for (name, old, new) in &planned.branches {
            Self::require_commit(new, objects)?;
            let current = self.current(name)?.commit;
            if current == Some(*old) {
                moves.branches.push((name.clone(), Some(*old), Some(*new)));
            } else if current != Some(*new) {
                return Err(stale(name.as_str()));
            }
        }
        let done = match step() {
            Ok(done) => done,
            Err(error) => return Ok(Logged::StepFailed(error)),
        };
        Ok(self.apply_moves(done, &moves))
    }

    /// Makes the moves after the step ran: the branches, the deletions before the creations, then
    /// the working-copy commit, then HEAD, one after the other. After a failure, the branches that
    /// did not move are therefore not the end of the list of the moves.
    fn apply_moves<T, E>(&self, done: T, moves: &RestoreMoves) -> Logged<T, E> {
        // The steps run one after the other and stop at the first failure, so that the pointers
        // after it are not moved.
        type Step<'s> = Box<dyn Fn() -> Result<bool, RefError> + 's>;
        let mut steps: Vec<Step<'_>> = Vec::new();
        // The deletions come before the creations, so that `a` can go before `a/b` comes, and
        // `a/b` before `a`.
        let deletions_first = moves
            .branches
            .iter()
            .filter(|(_, _, target)| target.is_none())
            .chain(
                moves
                    .branches
                    .iter()
                    .filter(|(_, _, target)| target.is_some()),
            );
        for (name, _, target) in deletions_first {
            steps.push(match target {
                Some(commit) => Box::new(move || {
                    // The deletions before this step may have left a directory without any
                    // branch where the name goes (`a/` after `a/b`, with a dot file or an empty
                    // directory in it), which a plain removal of the parents does not clear.
                    if let Some(empty) = self.current(name)?.empty_directory {
                        fs::remove_dir_all(&empty)
                            .map_err(io_error("remove the empty directory", &empty))?;
                    }
                    self.write_replacing(
                        &self.branch_path(name),
                        &encode_branch(commit),
                        &self.heads_dir(),
                    )
                    .map(|()| true)
                }),
                None => Box::new(move || self.delete_branch_file(name)),
            });
        }
        if let Some((_, new)) = &moves.working_copy {
            steps.push(Box::new(move || {
                self.write_working_copy(new).map(|()| true)
            }));
        }
        // HEAD last: a stop before it leaves the file of HEAD as it was. A HEAD that names a
        // branch follows that branch, so it already leads to the commit that the branch has.
        if let Some((_, new)) = &moves.head {
            steps.push(Box::new(move || {
                self.write_replacing(&self.head_path(), &new.encode(), &self.root)
                    .map(|()| true)
            }));
        }
        let mut moved_any = false;
        let mut unflushed: Option<RefError> = None;
        for step in steps {
            match step() {
                // A branch whose file was gone already did not move.
                Ok(changed) => moved_any |= changed,
                Err(error @ RefError::NotFlushed { .. }) => {
                    moved_any = true;
                    unflushed.get_or_insert(error);
                }
                Err(error) if moved_any => return Logged::PartlyMoved(done, error),
                Err(error) => return Logged::MoveFailed(done, error),
            }
        }
        match unflushed {
            Some(error) => Logged::NotFlushed(done, error),
            None => Logged::Moved(done),
        }
    }

    /// Deletes the file of a branch, and the directories above it that this empties, up to the
    /// directory of the branches, which stays. Returns `false` if the file was gone already, so
    /// that nothing moved and nothing needs a flush.
    ///
    /// Once the file is gone the branch is gone: a directory that cannot be removed is left, and
    /// is no failure of the move (it is an empty directory, which a later lookup clears). The
    /// deepest directory that changed is flushed; if that fails, the result is
    /// [`RefError::NotFlushed`]: the branch is gone, but may come back after a crash.
    fn delete_branch_file(&self, name: &BranchName) -> Result<bool, RefError> {
        let path = self.branch_path(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_error("remove the reference", &path)(error)),
        }
        let heads = self.heads_dir();
        let mut changed = path
            .parent()
            .map_or_else(|| heads.clone(), Path::to_path_buf);
        while changed != heads && fs::remove_dir(&changed).is_ok() {
            match changed.parent() {
                Some(parent) => changed = parent.to_path_buf(),
                None => break,
            }
        }
        flush_directory(&changed)
            .map(|()| true)
            .map_err(|source| RefError::NotFlushed {
                path: changed.clone(),
                source,
            })
    }

    /// The outcome of two moves, the second of which runs only if the first moved its pointer
    /// (`NotFlushed` counts as moved, see [`after_move`](Self::after_move)).
    ///
    /// If both moves are not flushed, the error of the first is kept: in both cases the caller
    /// learns that the change may not survive a crash and must not repeat it. If the second move
    /// fails after the first was not flushed, the result is
    /// [`PartlyMoved`](Logged::PartlyMoved) and the flush error of the first is dropped, since a
    /// caller that may not repeat the change reads the pointers either way.
    fn after_two_moves<T, E>(
        done: T,
        first: Result<(), RefError>,
        second: impl FnOnce() -> Result<(), RefError>,
    ) -> Logged<T, E> {
        let unflushed = match first {
            Ok(()) => None,
            Err(error @ RefError::NotFlushed { .. }) => Some(error),
            Err(error) => return Logged::MoveFailed(done, error),
        };
        match (second(), unflushed) {
            (Ok(()), None) => Logged::Moved(done),
            (Ok(()) | Err(RefError::NotFlushed { .. }), Some(first)) => {
                Logged::NotFlushed(done, first)
            }
            (Err(error @ RefError::NotFlushed { .. }), None) => Logged::NotFlushed(done, error),
            (Err(error), _) => Logged::PartlyMoved(done, error),
        }
    }

    fn after_move<T, E>(done: T, moved: Result<(), RefError>) -> Logged<T, E> {
        match moved {
            Ok(()) => Logged::Moved(done),
            Err(error @ RefError::NotFlushed { .. }) => Logged::NotFlushed(done, error),
            Err(error) => Logged::MoveFailed(done, error),
        }
    }
}

impl RefStore for FsRefStore {
    fn head(&self) -> Result<Head, RefError> {
        self.require_root()?;
        let path = self.head_path();
        let bytes = self
            .read_reference(&path)?
            .ok_or_else(|| RefError::NoHead { path: path.clone() })?;
        Head::parse(&bytes).map_err(|source| RefError::Corrupt { path, source })
    }

    fn set_head(
        &self,
        head: &Head,
        objects: &dyn ObjectStore,
        allowed: &Allowed,
    ) -> Result<(), RefError> {
        self.require_root()?;
        let _lock = self.lock_writer(allowed)?;
        // Under the lock, as in `set_head_after`: a purge that ran before it may have removed the
        // commit since a look without the lock.
        if let Head::Detached(commit) = head {
            Self::require_commit(commit, objects)?;
        }
        self.write_replacing(&self.head_path(), &head.encode(), &self.root)
    }

    fn branch(&self, name: &BranchName) -> Result<Option<CommitId>, RefError> {
        self.require_root()?;
        Ok(self.current(name)?.commit)
    }

    fn branches(&self) -> Result<Vec<(BranchName, CommitId)>, RefError> {
        self.require_root()?;
        if !self.heads_exist()? {
            return Ok(Vec::new());
        }
        let mut found = Vec::new();
        let mut pending = vec![(self.heads_dir(), String::new())];
        while let Some((dir, prefix)) = pending.pop() {
            let Some(entries) = list_directory(&dir)? else {
                continue;
            };
            for entry in entries {
                let entry = entry.map_err(io_error("list the branches in", &dir))?;
                let path = entry.path();
                let file_name = entry.file_name().to_string_lossy().into_owned();
                // No branch name starts with a dot, and file managers and editors leave files
                // that do, such as `.DS_Store`.
                if file_name.starts_with('.') {
                    continue;
                }
                let kind = match entry.file_type() {
                    Ok(kind) => kind,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(io_error("inspect the branch", &path)(error)),
                };
                // A directory holds the start of branch names, so its name follows the same rule:
                // one that does not is damage, and it is not skipped with all that is below it.
                let name =
                    BranchName::try_new(join_name(&prefix, &file_name)).map_err(|error| {
                        RefError::Corrupt {
                            path: path.clone(),
                            source: RefParseError::InvalidBranchName(error),
                        }
                    })?;
                if kind.is_dir() {
                    pending.push((path, name.as_str().to_owned()));
                    continue;
                }
                if let Some(commit) = self.listed_branch(path)? {
                    found.push((name, commit));
                }
            }
        }
        found.sort();
        Ok(found)
    }

    fn update_branch(
        &self,
        name: &BranchName,
        expected: Option<CommitId>,
        new: CommitId,
        objects: &dyn ObjectStore,
        allowed: &Allowed,
    ) -> Result<(), RefError> {
        self.require_root()?;
        let _lock = self.lock_writer(allowed)?;
        // Under the lock, as in `update_branch_after`: a purge that ran before it may have removed
        // the commit since a look without the lock.
        Self::require_commit(&new, objects)?;
        let current = self.current(name)?;
        if let Some((with, kind)) = current.in_the_way {
            return Err(RefError::Conflict {
                name: name.clone(),
                with,
                kind,
            });
        }
        if current.commit != expected {
            return Err(RefError::Stale {
                name: name.clone(),
                expected,
                found: current.commit,
            });
        }
        if let Some(empty) = current.empty_directory {
            // Nothing is below it, and the lock is held, so no branch is lost.
            fs::remove_dir_all(&empty).map_err(io_error("remove the empty directory", &empty))?;
        }
        self.write_replacing(
            &self.branch_path(name),
            &encode_branch(&new),
            &self.heads_dir(),
        )
    }

    fn delete_branch(
        &self,
        name: &BranchName,
        expected: CommitId,
        allowed: &Allowed,
    ) -> Result<(), RefError> {
        self.require_root()?;
        let _lock = self.lock_writer(allowed)?;
        let current = self.current(name)?;
        if let Some((with, kind)) = current.in_the_way {
            return Err(RefError::Conflict {
                name: name.clone(),
                with,
                kind,
            });
        }
        if current.commit != Some(expected) {
            return Err(RefError::Stale {
                name: name.clone(),
                expected: Some(expected),
                found: current.commit,
            });
        }
        // The file was there under the lock, so a file that is gone now was removed by a process
        // that ignores the lock: this call did not delete the branch.
        if self.delete_branch_file(name)? {
            Ok(())
        } else {
            Err(RefError::Stale {
                name: name.clone(),
                expected: Some(expected),
                found: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::sample_change;
    use crate::object::{Blob, Commit, Tree};
    use crate::store::FsObjectStore;
    use crate::testutil::{FaultyStore, TempDir, DISK_MESSAGE};
    use crate::Actor;

    const ID_A: &str = "b3:4a0b6603381c7c38c557e9ceb3cba8876f0a7863146e7309801352e80b073b65";

    fn name(text: &str) -> BranchName {
        BranchName::try_new(text).unwrap()
    }

    /// A commit in the store; the message makes it different from the others.
    fn commit(objects: &FsObjectStore, message: &str) -> CommitId {
        let tree = objects.put_tree(&Tree::try_new(vec![]).unwrap()).unwrap();
        let commit = Commit::try_new(
            tree,
            vec![],
            sample_change(),
            Actor::human("daniel").unwrap(),
            0,
            message,
        );
        objects.put_commit(&commit.unwrap()).unwrap()
    }

    fn setup() -> (TempDir, FsObjectStore, FsRefStore) {
        let dir = TempDir::new();
        let objects = dir.store();
        let refs = FsRefStore::new(dir.path());
        (dir, objects, refs)
    }

    fn files_in(dir: &Path) -> usize {
        fs::read_dir(dir).map_or(0, |entries| entries.count())
    }

    // ---- BranchName

    #[test]
    fn names_that_follow_the_rule_are_accepted() {
        for text in [
            "main",
            "feat/TF-792-login",
            "a.b",
            "a_b",
            "x/y/z",
            "v1.2.3",
            "console",
            "com10",
            "lpt0",
            "a/b/c/d/e",
            "UPPER",
        ] {
            assert_eq!(BranchName::try_new(text).unwrap().as_str(), text, "{text}");
            assert_eq!(text.parse::<BranchName>().unwrap().to_string(), text);
        }
        let longest = "a".repeat(MAX_BRANCH_NAME_LEN);
        assert!(BranchName::try_new(longest).is_ok());
    }

    #[test]
    fn every_rule_has_its_own_problem_and_the_first_one_is_reported() {
        let too_long = "a".repeat(MAX_BRANCH_NAME_LEN + 1);
        let cases: Vec<(String, BranchNameProblem)> = vec![
            (String::new(), BranchNameProblem::Empty),
            (too_long, BranchNameProblem::TooLong),
            ("/a".into(), BranchNameProblem::EmptyComponent),
            ("a/".into(), BranchNameProblem::EmptyComponent),
            ("a//b".into(), BranchNameProblem::EmptyComponent),
            ("a b".into(), BranchNameProblem::InvalidCharacter(' ')),
            (
                "\u{e9}".into(),
                BranchNameProblem::InvalidCharacter('\u{e9}'),
            ),
            ("a\\b".into(), BranchNameProblem::InvalidCharacter('\\')),
            ("a:b".into(), BranchNameProblem::InvalidCharacter(':')),
            ("a\nb".into(), BranchNameProblem::InvalidCharacter('\n')),
            ("a\0b".into(), BranchNameProblem::InvalidCharacter('\0')),
            ("a~b".into(), BranchNameProblem::InvalidCharacter('~')),
            (".a".into(), BranchNameProblem::LeadingDotOrDash),
            ("..".into(), BranchNameProblem::LeadingDotOrDash),
            (".".into(), BranchNameProblem::LeadingDotOrDash),
            ("-a".into(), BranchNameProblem::LeadingDotOrDash),
            ("a/-b".into(), BranchNameProblem::LeadingDotOrDash),
            ("a/.stemma".into(), BranchNameProblem::LeadingDotOrDash),
            ("a.".into(), BranchNameProblem::TrailingDot),
            ("a/b.".into(), BranchNameProblem::TrailingDot),
            ("con".into(), BranchNameProblem::ReservedName),
            ("CON".into(), BranchNameProblem::ReservedName),
            ("Nul.txt".into(), BranchNameProblem::ReservedName),
            ("a/com1".into(), BranchNameProblem::ReservedName),
            ("lpt9.x.y".into(), BranchNameProblem::ReservedName),
            ("aux".into(), BranchNameProblem::ReservedName),
            ("prn".into(), BranchNameProblem::ReservedName),
            // Several problems: the first in the order of the rules, and of the components.
            ("a b/.c".into(), BranchNameProblem::InvalidCharacter(' ')),
            ("/ ".into(), BranchNameProblem::EmptyComponent),
        ];
        for (text, problem) in cases {
            let error = BranchName::try_new(text.clone()).unwrap_err();
            assert_eq!(error.problem, problem, "{text:?}");
            assert_eq!(error.name, text);
        }
    }

    #[test]
    fn the_messages_of_the_problems_are_pinned() {
        for (problem, message) in [
            (BranchNameProblem::Empty, "the name is empty"),
            (
                BranchNameProblem::TooLong,
                "the name is longer than 200 bytes",
            ),
            (
                BranchNameProblem::EmptyComponent,
                "the name has an empty component",
            ),
            (
                BranchNameProblem::InvalidCharacter('\n'),
                "the character `\\n` is not allowed; use letters, digits, `.`, `_`, `-`, and `/`",
            ),
            (
                BranchNameProblem::LeadingDotOrDash,
                "a component starts with `.` or `-`",
            ),
            (BranchNameProblem::TrailingDot, "a component ends with `.`"),
            (
                BranchNameProblem::ReservedName,
                "a component is the name of a device on Windows",
            ),
        ] {
            assert_eq!(problem.to_string(), message);
        }
        let error = BranchName::try_new("a\nb c").unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid branch name `a\\nb c`: the character `\\n` is not allowed; use letters, \
             digits, `.`, `_`, `-`, and `/`"
        );
        assert!(error.source().is_none());
    }

    // ---- the files

    #[test]
    fn the_files_are_pinned_by_literals() {
        let id = CommitId::new(ID_A.parse().unwrap());
        assert_eq!(encode_branch(&id), format!("{ID_A}\n").into_bytes());
        assert_eq!(
            Head::Branch(name("main")).encode(),
            b"branch main\n".to_vec()
        );
        assert_eq!(
            Head::Detached(id).encode(),
            format!("commit {ID_A}\n").into_bytes()
        );
        assert_eq!(parse_branch(format!("{ID_A}\n").as_bytes()), Ok(id));
        assert_eq!(
            Head::parse(b"branch feat/x\n"),
            Ok(Head::Branch(name("feat/x")))
        );
        assert_eq!(
            Head::parse(format!("commit {ID_A}\n").as_bytes()),
            Ok(Head::Detached(id))
        );
    }

    #[test]
    fn the_file_working_copy_names_the_owner_on_a_second_line() {
        let id = CommitId::new(ID_A.parse().unwrap());
        let owner: Actor = "agent bot model=m session=s sponsor=daniel"
            .parse()
            .unwrap();
        let with_owner =
            format!("{ID_A}\nowner agent bot model=m session=s sponsor=daniel\n").into_bytes();
        assert_eq!(encode_working_copy(&id, Some(&owner)), with_owner);
        assert_eq!(
            encode_working_copy(&id, None),
            format!("{ID_A}\n").into_bytes()
        );
        assert_eq!(parse_working_copy(&with_owner), Ok((id, Some(owner))));
        // A file from before format 5 has no owner.
        assert_eq!(
            parse_working_copy(format!("{ID_A}\n").as_bytes()),
            Ok((id, None))
        );
    }

    #[test]
    fn the_owner_is_read_only_in_the_form_that_the_writer_writes() {
        use RefMalformed::*;
        let cases: Vec<(String, RefParseError)> = vec![
            (
                format!("{ID_A}\nowner human a"),
                RefParseError::Malformed(CutOff),
            ),
            (format!("{ID_A}\n\n"), RefParseError::Malformed(OwnerShape)),
            (
                format!("{ID_A}\nowner human a\n\n"),
                RefParseError::Malformed(ExtraLines),
            ),
            (
                format!("{ID_A}\nowner human a\nowner human b\n"),
                RefParseError::Malformed(ExtraLines),
            ),
            (
                format!("{ID_A}\nowner: human a\n"),
                RefParseError::Malformed(OwnerShape),
            ),
            (
                format!("{ID_A}\nOwner human a\n"),
                RefParseError::Malformed(OwnerShape),
            ),
            (
                format!("{ID_A}\nowner  human a\n"),
                RefParseError::InvalidOwner(Actor::from_str(" human a").unwrap_err()),
            ),
            (
                format!("{ID_A}\nowner human a \n"),
                RefParseError::InvalidOwner(Actor::from_str("human a ").unwrap_err()),
            ),
            (
                format!("{ID_A}\nowner robot a\n"),
                RefParseError::InvalidOwner(Actor::from_str("robot a").unwrap_err()),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(
                parse_working_copy(text.as_bytes()),
                Err(expected),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_reader_rejects_everything_that_is_not_what_the_writer_writes() {
        use RefMalformed::*;
        let bad_id = |text: &str| ObjectId::from_str(text).unwrap_err();
        let branch_cases: Vec<(Vec<u8>, RefParseError)> = vec![
            (b"".to_vec(), RefParseError::Malformed(CutOff)),
            (b"\n".to_vec(), RefParseError::InvalidObjectId(bad_id(""))),
            (ID_A.as_bytes().to_vec(), RefParseError::Malformed(CutOff)),
            (
                format!("{ID_A}\n\n").into_bytes(),
                RefParseError::Malformed(ExtraLines),
            ),
            (
                format!("{ID_A}\n{ID_A}\n").into_bytes(),
                RefParseError::Malformed(ExtraLines),
            ),
            (
                format!("{ID_A}\r\n").into_bytes(),
                RefParseError::InvalidObjectId(bad_id(&format!("{ID_A}\r"))),
            ),
            (
                format!(" {ID_A}\n").into_bytes(),
                RefParseError::InvalidObjectId(bad_id(&format!(" {ID_A}"))),
            ),
            (
                ID_A.to_uppercase().replace("B3:", "b3:").into_bytes(),
                RefParseError::Malformed(CutOff),
            ),
            (
                b"\xff\n".to_vec(),
                RefParseError::NotUtf8 { valid_up_to: 0 },
            ),
            (
                b"b3:00\n".to_vec(),
                RefParseError::InvalidObjectId(bad_id("b3:00")),
            ),
        ];
        for (bytes, expected) in branch_cases {
            assert_eq!(parse_branch(&bytes), Err(expected), "{bytes:?}");
        }
        let head_cases: Vec<(Vec<u8>, RefParseError)> = vec![
            (b"".to_vec(), RefParseError::Malformed(CutOff)),
            (b"branch main".to_vec(), RefParseError::Malformed(CutOff)),
            (b"main\n".to_vec(), RefParseError::Malformed(HeadShape)),
            (b"\n".to_vec(), RefParseError::Malformed(HeadShape)),
            (
                b"branch main\nbranch main\n".to_vec(),
                RefParseError::Malformed(ExtraLines),
            ),
            (
                b"tag main\n".to_vec(),
                RefParseError::Malformed(UnknownHeadKind),
            ),
            (
                b"Branch main\n".to_vec(),
                RefParseError::Malformed(UnknownHeadKind),
            ),
            (
                b"branch  main\n".to_vec(),
                RefParseError::InvalidBranchName(BranchNameError {
                    name: " main".to_owned(),
                    problem: BranchNameProblem::InvalidCharacter(' '),
                }),
            ),
            (
                b"branch \n".to_vec(),
                RefParseError::InvalidBranchName(BranchNameError {
                    name: String::new(),
                    problem: BranchNameProblem::Empty,
                }),
            ),
            (
                b"branch .x\n".to_vec(),
                RefParseError::InvalidBranchName(BranchNameError {
                    name: ".x".to_owned(),
                    problem: BranchNameProblem::LeadingDotOrDash,
                }),
            ),
            (
                b"commit b3:00\n".to_vec(),
                RefParseError::InvalidObjectId(bad_id("b3:00")),
            ),
            (
                b"\xff \n".to_vec(),
                RefParseError::NotUtf8 { valid_up_to: 0 },
            ),
        ];
        for (bytes, expected) in head_cases {
            assert_eq!(Head::parse(&bytes), Err(expected), "{bytes:?}");
        }
    }

    #[test]
    fn the_messages_of_a_rejected_file_are_pinned_and_print_the_cause_once() {
        for (reason, message) in [
            (
                RefMalformed::CutOff,
                "the file is empty or has no terminating newline",
            ),
            (RefMalformed::ExtraLines, "the file has more than one line"),
            (
                RefMalformed::HeadShape,
                "expected `branch <name>` or `commit <id>`",
            ),
            (
                RefMalformed::UnknownHeadKind,
                "the kind is neither `branch` nor `commit`",
            ),
        ] {
            assert_eq!(reason.to_string(), message);
            let error = RefParseError::Malformed(reason);
            assert_eq!(error.to_string(), message);
            assert!(error.source().is_none());
        }
        let error = parse_branch(b"b3:00\n").unwrap_err();
        assert!(
            error.to_string().starts_with("invalid object ID: "),
            "{error}"
        );
        assert!(error.source().is_none());
        assert_eq!(
            RefParseError::NotUtf8 { valid_up_to: 3 }.to_string(),
            "not valid UTF-8 (valid up to byte 3)"
        );
    }

    // ---- branches

    #[test]
    fn a_branch_that_does_not_exist_is_none_and_a_created_one_is_read_back() {
        let (dir, objects, refs) = setup();
        let main = name("main");
        assert_eq!(refs.branch(&main).unwrap(), None);
        assert!(refs.branches().unwrap().is_empty());
        let first = commit(&objects, "first");
        refs.update_branch(&main, None, first, &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.branch(&main).unwrap(), Some(first));
        // The file is exactly the ID and a newline, and no temporary file stays behind.
        let file = dir.path().join("refs/heads/main");
        assert_eq!(fs::read(&file).unwrap(), format!("{first}\n").into_bytes());
        assert_eq!(files_in(&dir.path().join("tmp")), 0);
        assert!(!dir.path().join("refs.lock").exists());
        let second = commit(&objects, "second");
        refs.update_branch(&main, Some(first), second, &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.branch(&main).unwrap(), Some(second));
        assert_eq!(files_in(&dir.path().join("tmp")), 0);
    }

    #[test]
    fn a_change_from_a_state_that_is_not_current_is_stale_and_changes_nothing() {
        let (dir, objects, refs) = setup();
        let main = name("main");
        let (first, second, third) = (
            commit(&objects, "1"),
            commit(&objects, "2"),
            commit(&objects, "3"),
        );
        refs.update_branch(&main, None, first, &objects, &Allowed::granted())
            .unwrap();
        for expected in [None, Some(second)] {
            match refs
                .update_branch(&main, expected, third, &objects, &Allowed::granted())
                .unwrap_err()
            {
                RefError::Stale {
                    name: stale,
                    expected: seen,
                    found,
                } => {
                    assert_eq!(
                        (stale.as_str(), seen, found),
                        ("main", expected, Some(first))
                    );
                }
                other => panic!("unexpected error: {other}"),
            }
        }
        // A branch that does not exist cannot be moved.
        let error = refs
            .update_branch(
                &name("other"),
                Some(first),
                second,
                &objects,
                &Allowed::granted(),
            )
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Stale { found: None, .. }),
            "{error}"
        );
        assert_eq!(refs.branch(&main).unwrap(), Some(first));
        assert!(!dir.path().join("refs/heads/other").exists());
        // The lock is released after a failure too.
        assert!(!dir.path().join("refs.lock").exists());
    }

    #[test]
    fn a_branch_is_deleted_if_it_is_at_the_expected_commit() {
        let (dir, objects, refs) = setup();
        let (first, second) = (commit(&objects, "1"), commit(&objects, "2"));
        let (main, nested) = (name("main"), name("a/b"));
        for (branch, tip) in [(&main, first), (&nested, second)] {
            refs.update_branch(branch, None, tip, &objects, &Allowed::granted())
                .unwrap();
        }
        refs.delete_branch(&main, first, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.branch(&main).unwrap(), None);
        // The directory that the deletion empties goes with it; `refs/heads` stays.
        refs.delete_branch(&nested, second, &Allowed::granted())
            .unwrap();
        assert!(!dir.path().join("refs/heads/a").exists());
        assert!(dir.path().join("refs/heads").is_dir());
        assert!(refs.branches().unwrap().is_empty());
        assert!(!dir.path().join("refs.lock").exists());
    }

    #[test]
    fn a_delete_from_a_state_that_is_not_current_is_stale_and_changes_nothing() {
        let (dir, objects, refs) = setup();
        let main = name("main");
        let (first, second) = (commit(&objects, "1"), commit(&objects, "2"));
        refs.update_branch(&main, None, first, &objects, &Allowed::granted())
            .unwrap();
        match refs
            .delete_branch(&main, second, &Allowed::granted())
            .unwrap_err()
        {
            RefError::Stale {
                expected, found, ..
            } => assert_eq!((expected, found), (Some(second), Some(first))),
            other => panic!("unexpected error: {other}"),
        }
        // A branch that does not exist cannot be deleted.
        let error = refs
            .delete_branch(&name("other"), first, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Stale { found: None, .. }),
            "{error}"
        );
        assert_eq!(refs.branch(&main).unwrap(), Some(first));
        assert!(!dir.path().join("refs.lock").exists());
    }

    #[test]
    fn deleting_a_name_that_another_entry_holds_is_a_conflict() {
        let (_dir, objects, refs) = setup();
        let first = commit(&objects, "1");
        refs.update_branch(&name("a/b"), None, first, &objects, &Allowed::granted())
            .unwrap();
        // `a` is a directory with a branch below it, not a branch.
        let error = refs
            .delete_branch(&name("a"), first, &Allowed::granted())
            .unwrap_err();
        assert!(matches!(error, RefError::Conflict { .. }), "{error}");
        assert_eq!(refs.branch(&name("a/b")).unwrap(), Some(first));
    }

    #[test]
    fn a_delete_is_refused_while_a_lock_or_a_pending_purge_exists() {
        let (dir, objects, refs) = setup();
        let first = commit(&objects, "1");
        let main = name("main");
        refs.update_branch(&main, None, first, &objects, &Allowed::granted())
            .unwrap();
        let lock = dir.path().join("refs.lock");
        fs::write(&lock, b"").unwrap();
        // The lock is checked before `expected`: a wrong tip is `Locked`, not `Stale`.
        let error = refs
            .delete_branch(&main, commit(&objects, "2"), &Allowed::granted())
            .unwrap_err();
        assert!(matches!(error, RefError::Locked { .. }), "{error}");
        assert!(
            lock.exists(),
            "a lock of another process is not ours to remove"
        );
        fs::remove_file(&lock).unwrap();
        let journal = dir.path().join(PURGE_JOURNAL);
        fs::write(&journal, b"journal").unwrap();
        let error = refs
            .delete_branch(&main, first, &Allowed::granted())
            .unwrap_err();
        assert!(matches!(error, RefError::PurgePending { .. }), "{error}");
        assert_eq!(refs.branch(&main).unwrap(), Some(first));
    }

    #[test]
    fn deleting_a_case_variant_of_a_branch_is_a_conflict_and_not_stale() {
        let (_dir, objects, refs) = setup();
        let first = commit(&objects, "1");
        refs.update_branch(&name("Main"), None, first, &objects, &Allowed::granted())
            .unwrap();
        let error = refs
            .delete_branch(&name("main"), first, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(
                error,
                RefError::Conflict {
                    kind: ConflictKind::CaseVariant,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(refs.branch(&name("Main")).unwrap(), Some(first));
    }

    #[test]
    fn a_delete_keeps_the_directory_that_other_branches_or_files_still_use() {
        let (dir, objects, refs) = setup();
        let first = commit(&objects, "1");
        for branch in ["a/b", "a/c", "d/e"] {
            refs.update_branch(&name(branch), None, first, &objects, &Allowed::granted())
                .unwrap();
        }
        refs.delete_branch(&name("a/b"), first, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.branch(&name("a/c")).unwrap(), Some(first));
        // A file that is no branch, such as one of a file manager, keeps its directory as well.
        let dot = dir.path().join("refs/heads/d/.DS_Store");
        fs::write(&dot, b"").unwrap();
        refs.delete_branch(&name("d/e"), first, &Allowed::granted())
            .unwrap();
        assert!(dot.exists());
        assert_eq!(refs.branch(&name("d/e")).unwrap(), None);
    }

    #[test]
    fn a_commit_that_the_store_does_not_have_or_that_is_not_a_commit_is_refused() {
        let (dir, objects, refs) = setup();
        let main = name("main");
        let absent = Commit::try_new(
            objects.put_tree(&Tree::try_new(vec![]).unwrap()).unwrap(),
            vec![],
            sample_change(),
            Actor::human("daniel").unwrap(),
            0,
            "never stored",
        )
        .unwrap()
        .id();
        let error = refs
            .update_branch(&main, None, absent, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(&error, RefError::MissingCommit(id) if *id == absent),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!("the store has no commit {absent}")
        );
        let blob = objects.put_blob(&Blob::new("x")).unwrap();
        let blob_as_commit = CommitId::new(ObjectId::from(blob));
        let error = refs
            .update_branch(&main, None, blob_as_commit, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(
                &error,
                RefError::NotACommit {
                    found: Kind::Blob,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!("object {blob_as_commit} is a blob, not a commit")
        );
        assert!(error.store_error().is_none());
        assert_eq!(refs.branch(&main).unwrap(), None);
        assert!(!dir.path().join("refs/heads").exists());
        assert!(!dir.path().join("refs.lock").exists());
    }

    #[test]
    fn branches_are_listed_sorted_by_name_with_their_commits() {
        let (_dir, objects, refs) = setup();
        let names = [
            "main", "feat/b", "feat/a", "feat/a-x", "z/y/x", "Alpha", "a.b",
        ];
        let mut expected = Vec::new();
        for (n, text) in names.iter().enumerate() {
            let id = commit(&objects, &format!("c{n}"));
            refs.update_branch(&name(text), None, id, &objects, &Allowed::granted())
                .unwrap();
            expected.push((name(text), id));
        }
        expected.sort();
        assert_eq!(refs.branches().unwrap(), expected);
        let listed: Vec<&str> = expected.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            listed,
            ["Alpha", "a.b", "feat/a", "feat/a-x", "feat/b", "main", "z/y/x"]
        );
    }

    #[test]
    fn a_name_cannot_be_a_branch_and_the_start_of_other_names() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("a"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let error = refs
            .update_branch(&name("a/b"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Conflict { with, .. } if with == "a"),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            "branch `a/b` conflicts with `a`, which is a branch"
        );
        refs.update_branch(&name("x/y"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let error = refs
            .update_branch(&name("x"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Conflict { with, kind: ConflictKind::Directory, .. } if with == "x"),
            "{error}"
        );
        let error = refs
            .update_branch(&name("x/y/z"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Conflict { with, kind: ConflictKind::Branch, .. } if with == "x/y"),
            "{error}"
        );
        // Nothing was written for the refused names, and the lock is gone.
        assert_eq!(refs.branches().unwrap().len(), 2);
        assert!(!dir.path().join("refs.lock").exists());
        // A directory of branches is no branch.
        assert_eq!(refs.branch(&name("x")).unwrap(), None);
    }

    #[test]
    fn two_names_that_differ_only_in_case_are_one_name() {
        let (_dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("Main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let error = refs
            .update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Conflict { with, .. } if with == "Main"),
            "{error}"
        );
        refs.update_branch(&name("Feat/Login"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let error = refs
            .update_branch(&name("feat/login"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Conflict { with, .. } if with == "Feat"),
            "{error}"
        );
        let error = refs
            .update_branch(&name("Feat/login"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Conflict { with, .. } if with == "Feat/Login"),
            "{error}"
        );
        // Moving an existing branch is not a creation and needs no such check.
        let next = commit(&objects, "next");
        refs.update_branch(&name("Main"), Some(id), next, &objects, &Allowed::granted())
            .unwrap();
    }

    #[test]
    fn a_change_removes_the_scratch_that_no_writer_owns() {
        // A killed writer leaves its temporary file in `tmp/`, read-only if it was an object. The
        // next change holds the lock, so the file is nobody's, and it is removed (ADR-011).
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let tmp = dir.path().join("tmp");
        fs::write(tmp.join("1-0-stale"), b"secret").unwrap();
        let read_only = tmp.join("1-1-stale");
        fs::write(&read_only, b"object").unwrap();
        let mut permissions = fs::metadata(&read_only).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&read_only, permissions).unwrap();
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(files_in(&tmp), 0);
    }

    #[test]
    fn the_scratch_of_a_writer_that_holds_the_lock_stays() {
        // A writer holds the lock while it writes to `tmp/`. A second change fails with `Locked`
        // and leaves the file of the first alone.
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let writer = refs.lock_writer(&Allowed::granted()).unwrap();
        let live = dir.path().join("tmp").join("1-0-live");
        fs::write(&live, b"in progress").unwrap();
        let other = FsRefStore::new(dir.path());
        let error = other
            .update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(matches!(error, RefError::Locked { .. }), "{error}");
        assert_eq!(fs::read(&live).unwrap(), b"in progress");
        drop(writer);
        // Once the writer is gone, the next change removes what it left.
        other
            .update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert!(!live.exists());
    }

    #[test]
    fn the_take_over_of_a_stopped_purge_removes_the_scratch() {
        // `--resume` and `--abort` take the lock that a stopped purge left behind, and then own
        // `tmp/` as any writer does.
        let (dir, _objects, refs) = setup();
        fs::write(dir.path().join(PURGE_JOURNAL), b"journal").unwrap();
        fs::write(dir.path().join("refs.lock"), b"left by the purge").unwrap();
        let stale = dir.path().join("tmp").join("1-0-stale");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, b"secret").unwrap();
        let lock = refs.take_over_lock(&Allowed::granted()).unwrap();
        assert!(!stale.exists());
        drop(lock);
    }

    #[test]
    fn a_change_that_a_pending_purge_refuses_leaves_the_scratch_to_the_purge() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        fs::write(dir.path().join(PURGE_JOURNAL), b"journal").unwrap();
        let stale = dir.path().join("tmp").join("1-0-stale");
        fs::write(&stale, b"secret").unwrap();
        let error = refs
            .update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(matches!(error, RefError::PurgePending { .. }), "{error}");
        assert!(stale.exists());
    }

    #[test]
    fn a_root_not_named_like_the_metadata_directory_leaves_its_neighbours_alone() {
        // Only a metadata directory named `.stemma` has the directory of `init` next to it; the
        // directory around any other root is not looked at.
        let dir = TempDir::new();
        let root = dir.path().join("meta");
        fs::create_dir(&root).unwrap();
        let objects = FsObjectStore::new(&root);
        let refs = FsRefStore::new(&root);
        let id = commit(&objects, "c");
        let neighbour = dir.path().join(".stemma-init-1-0-0123456789abcdef");
        fs::create_dir(&neighbour).unwrap();
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert!(neighbour.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn the_cleanup_of_scratch_follows_no_link_and_keeps_a_directory() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let tmp = dir.path().join("tmp");
        let target = dir.path().join("kept");
        fs::write(&target, b"not scratch").unwrap();
        std::os::unix::fs::symlink(&target, tmp.join("link")).unwrap();
        fs::create_dir(tmp.join("directory")).unwrap();
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert!(
            fs::symlink_metadata(tmp.join("link")).is_err(),
            "the link is removed"
        );
        assert_eq!(fs::read(&target).unwrap(), b"not scratch");
        assert!(tmp.join("directory").is_dir());
    }

    #[test]
    fn a_lock_that_exists_stops_every_change_and_is_not_removed() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let lock = dir.path().join("refs.lock");
        fs::write(&lock, b"").unwrap();
        let error = refs
            .update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Locked { path, .. } if *path == lock),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .starts_with("the references are locked by `"),
            "{error}"
        );
        let error = refs
            .set_head(&Head::Branch(name("main")), &objects, &Allowed::granted())
            .unwrap_err();
        assert!(matches!(error, RefError::Locked { .. }));
        assert!(
            lock.exists(),
            "a lock of another process is not ours to remove"
        );
        assert_eq!(refs.branch(&name("main")).unwrap(), None);
        fs::remove_file(&lock).unwrap();
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
    }

    #[test]
    fn a_file_that_is_not_canonical_is_corrupt_with_its_path() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let file = dir.path().join("refs/heads/main");
        fs::write(&file, format!("{id}")).unwrap();
        let error = refs.branch(&name("main")).unwrap_err();
        assert!(
            matches!(
                &error,
                RefError::Corrupt { path, source: RefParseError::Malformed(RefMalformed::CutOff) }
                    if *path == file
            ),
            "{error}"
        );
        assert!(
            error.to_string().contains("is not a valid reference"),
            "{error}"
        );
        // The same file stops the listing, and a change of that branch.
        assert!(matches!(refs.branches(), Err(RefError::Corrupt { .. })));
        assert!(matches!(
            refs.update_branch(&name("main"), Some(id), id, &objects, &Allowed::granted()),
            Err(RefError::Corrupt { .. })
        ));
        // A file in `refs/heads` whose name is not a branch name is corrupt too.
        fs::write(&file, format!("{id}\n")).unwrap();
        fs::write(dir.path().join("refs/heads/not valid"), format!("{id}\n")).unwrap();
        assert!(matches!(
            refs.branches(),
            Err(RefError::Corrupt {
                source: RefParseError::InvalidBranchName(_),
                ..
            })
        ));
    }

    // ---- HEAD

    #[test]
    fn head_is_missing_until_it_is_set_and_may_name_a_branch_that_does_not_exist() {
        let (dir, objects, refs) = setup();
        let error = refs.head().unwrap_err();
        assert!(matches!(&error, RefError::NoHead { path } if *path == dir.path().join("HEAD")));
        refs.set_head(&Head::Branch(name("main")), &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.head().unwrap(), Head::Branch(name("main")));
        assert_eq!(fs::read(dir.path().join("HEAD")).unwrap(), b"branch main\n");
        assert_eq!(refs.resolve_head().unwrap(), None);
        let id = commit(&objects, "c");
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.resolve_head().unwrap(), Some(id));
        refs.set_head(&Head::Branch(name("other")), &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.resolve_head().unwrap(), None);
        assert_eq!(files_in(&dir.path().join("tmp")), 0);
        assert!(!dir.path().join("refs.lock").exists());
    }

    #[test]
    fn a_detached_head_needs_a_commit_in_the_store() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.set_head(&Head::Detached(id), &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.head().unwrap(), Head::Detached(id));
        assert_eq!(
            fs::read(dir.path().join("HEAD")).unwrap(),
            format!("commit {id}\n").into_bytes()
        );
        assert_eq!(refs.resolve_head().unwrap(), Some(id));
        let absent = CommitId::new(ID_A.parse().unwrap());
        let error = refs
            .set_head(&Head::Detached(absent), &objects, &Allowed::granted())
            .unwrap_err();
        assert!(matches!(error, RefError::MissingCommit(_)), "{error}");
        assert_eq!(
            refs.head().unwrap(),
            Head::Detached(id),
            "HEAD is unchanged"
        );
    }

    #[test]
    fn a_head_that_is_not_canonical_is_corrupt() {
        let (dir, _objects, refs) = setup();
        fs::write(dir.path().join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        let error = refs.head().unwrap_err();
        assert!(
            matches!(
                &error,
                RefError::Corrupt {
                    source: RefParseError::Malformed(RefMalformed::UnknownHeadKind),
                    ..
                }
            ),
            "{error}"
        );
    }

    // ---- the file system

    #[test]
    fn a_missing_root_is_an_error_and_is_not_created() {
        let dir = TempDir::new();
        let objects = dir.store();
        let id = commit(&objects, "c");
        let root = dir.path().join("absent");
        let refs = FsRefStore::new(&root);
        for error in [
            refs.head().unwrap_err(),
            refs.branch(&name("main")).unwrap_err(),
            refs.branches().unwrap_err(),
            refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
                .unwrap_err(),
            refs.set_head(&Head::Branch(name("main")), &objects, &Allowed::granted())
                .unwrap_err(),
        ] {
            assert!(
                matches!(&error, RefError::Io { path, .. } if *path == root),
                "{error}"
            );
            assert!(error.io_error().is_some());
        }
        assert!(!root.exists());
    }

    #[test]
    fn a_directory_in_the_place_of_a_branch_file_is_no_branch() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("a/b"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.branch(&name("a")).unwrap(), None);
        assert_eq!(refs.branches().unwrap(), vec![(name("a/b"), id)]);
        let _ = dir;
    }

    #[test]
    fn writers_that_race_for_one_new_branch_leave_exactly_one_winner() {
        use std::sync::{Arc, Barrier};
        let (dir, objects, refs) = setup();
        let commits: Vec<CommitId> = (0..8).map(|n| commit(&objects, &format!("c{n}"))).collect();
        let barrier = Arc::new(Barrier::new(commits.len()));
        let results: Vec<(CommitId, Result<(), RefError>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = commits
                .iter()
                .map(|&id| {
                    let (barrier, refs, objects) = (barrier.clone(), refs.clone(), dir.store());
                    scope.spawn(move || {
                        barrier.wait();
                        (
                            id,
                            refs.update_branch(
                                &name("main"),
                                None,
                                id,
                                &objects,
                                &Allowed::granted(),
                            ),
                        )
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let winners: Vec<CommitId> = results
            .iter()
            .filter(|(_, result)| result.is_ok())
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(winners.len(), 1, "{results:?}");
        for (_, result) in &results {
            if let Err(error) = result {
                assert!(
                    matches!(error, RefError::Stale { .. } | RefError::Locked { .. }),
                    "{error}"
                );
            }
        }
        assert_eq!(refs.branch(&name("main")).unwrap(), Some(winners[0]));
        assert!(!dir.path().join("refs.lock").exists());
        assert_eq!(files_in(&dir.path().join("tmp")), 0);
    }

    // ---- the chain of causes

    #[test]
    fn the_errors_print_the_cause_once_and_the_source_goes_on_below() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let faulty = FaultyStore::new(dir.store(), true, false);
        let error = refs
            .update_branch(&name("main"), None, id, &faulty, &Allowed::granted())
            .unwrap_err();
        // A failure of the file system is an `Io`, whether it comes from the object store or from
        // the reference files.
        assert!(matches!(error, RefError::Io { .. }), "{error}");
        assert!(error.to_string().ends_with(DISK_MESSAGE), "{error}");
        assert_eq!(error.to_string().matches(DISK_MESSAGE).count(), 1);
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("the deepest cause")
        );
        assert!(error.store_error().is_none());
        assert_eq!(
            error.io_error().map(io::Error::kind),
            Some(io::ErrorKind::Other)
        );
        let io = RefError::Io {
            operation: "read the reference",
            path: PathBuf::from("a/b"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        };
        assert_eq!(io.to_string(), "could not read the reference `a/b`: denied");
        assert!(io.source().is_none());
        assert_eq!(
            io.io_error().map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied)
        );
        assert!(io.store_error().is_none());
    }

    #[test]
    fn the_other_errors_have_pinned_messages() {
        let a = CommitId::new(ID_A.parse().unwrap());
        let stale = RefError::Stale {
            name: name("main"),
            expected: Some(a),
            found: None,
        };
        assert_eq!(
            stale.to_string(),
            format!("branch `main` has moved: it was expected to be at {a}, but it does not exist")
        );
        let created = RefError::Stale {
            name: name("main"),
            expected: None,
            found: Some(a),
        };
        assert_eq!(
            created.to_string(),
            format!("branch `main` has moved: it was expected to be new, but it is at {a}")
        );
        assert_eq!(
            RefError::NoHead {
                path: PathBuf::from("x/HEAD")
            }
            .to_string(),
            "there is no HEAD at `x/HEAD`"
        );
        assert!(stale.source().is_none() && created.source().is_none());
    }

    // ---- review fixes

    /// Whether permissions have an effect: a user that may write into a directory without the
    /// write permission (root) cannot show a failure of the file system by a permission.
    #[cfg(unix)]
    fn permissions_hold(dir: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        let probe = dir.join("probe");
        fs::create_dir(&probe).unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o555)).unwrap();
        let held = fs::write(probe.join("file"), b"").is_err();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&probe).unwrap();
        held
    }

    #[test]
    fn what_is_in_the_way_of_a_name_is_named_by_its_kind() {
        let (_dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        for text in ["a", "x/y", "Main"] {
            refs.update_branch(&name(text), None, id, &objects, &Allowed::granted())
                .unwrap();
        }
        for (new, with, kind) in [
            ("a/b", "a", ConflictKind::Branch),
            ("x", "x", ConflictKind::Directory),
            ("x/y/z", "x/y", ConflictKind::Branch),
            ("main", "Main", ConflictKind::CaseVariant),
            ("X/y", "x", ConflictKind::CaseVariant),
        ] {
            match refs
                .update_branch(&name(new), None, id, &objects, &Allowed::granted())
                .unwrap_err()
            {
                RefError::Conflict {
                    with: found,
                    kind: found_kind,
                    ..
                } => {
                    assert_eq!((found.as_str(), found_kind), (with, kind), "{new}");
                }
                other => panic!("{new}: unexpected error: {other}"),
            }
        }
        assert_eq!(
            ConflictKind::Directory.to_string(),
            "is a directory of branches"
        );
        assert_eq!(
            ConflictKind::CaseVariant.to_string(),
            "differs only in case"
        );
    }

    #[test]
    fn a_name_that_is_in_the_way_is_a_conflict_even_if_the_caller_expects_a_commit() {
        let (_dir, objects, refs) = setup();
        let (first, second) = (commit(&objects, "1"), commit(&objects, "2"));
        refs.update_branch(&name("Main"), None, first, &objects, &Allowed::granted())
            .unwrap();
        // Not `Stale { found: None }`: nothing has moved, the name is held by another entry.
        for expected in [None, Some(first)] {
            let error = refs
                .update_branch(
                    &name("main"),
                    expected,
                    second,
                    &objects,
                    &Allowed::granted(),
                )
                .unwrap_err();
            assert!(
                matches!(
                    &error,
                    RefError::Conflict {
                        kind: ConflictKind::CaseVariant,
                        ..
                    }
                ),
                "{error}"
            );
        }
        // The read path says that there is no branch of exactly this name.
        assert_eq!(refs.branch(&name("main")).unwrap(), None);
    }

    #[test]
    fn an_empty_directory_that_a_stopped_change_left_does_not_block_the_name() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let leftover = dir.path().join("refs/heads/feat/deeper");
        fs::create_dir_all(&leftover).unwrap();
        assert_eq!(refs.branch(&name("feat")).unwrap(), None);
        assert!(refs.branches().unwrap().is_empty());
        refs.update_branch(&name("feat"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.branch(&name("feat")).unwrap(), Some(id));
        assert!(dir.path().join("refs/heads/feat").is_file());
        // A directory with a branch below it is still a directory of branches.
        fs::create_dir_all(dir.path().join("refs/heads/other/deeper")).unwrap();
        refs.update_branch(
            &name("other/deeper/x"),
            None,
            id,
            &objects,
            &Allowed::granted(),
        )
        .unwrap();
        let error = refs
            .update_branch(&name("other"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(
                &error,
                RefError::Conflict {
                    kind: ConflictKind::Directory,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_case_variant_below_a_directory_is_cleared_when_empty_and_a_conflict_when_not() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        // The variant is one level down: the directory that is cleared is the one found there.
        fs::create_dir_all(dir.path().join("refs/heads/x/Feat/deeper")).unwrap();
        refs.update_branch(&name("x/feat"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.branch(&name("x/feat")).unwrap(), Some(id));
        // A variant that is a directory with a branch in it stays a conflict, and the branch stays.
        refs.update_branch(&name("Main/b"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let error = refs
            .update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(
                &error,
                RefError::Conflict {
                    kind: ConflictKind::CaseVariant,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(refs.branch(&name("Main/b")).unwrap(), Some(id));
    }

    #[test]
    fn a_leftover_that_only_a_file_manager_touched_does_not_block_the_name() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let leftover = dir.path().join("refs/heads/feat/deeper");
        fs::create_dir_all(&leftover).unwrap();
        fs::write(leftover.join(".DS_Store"), b"x").unwrap();
        assert_eq!(refs.branch(&name("feat")).unwrap(), None);
        refs.update_branch(&name("feat"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.branch(&name("feat")).unwrap(), Some(id));
    }

    #[test]
    fn an_empty_directory_that_differs_in_case_is_cleared_and_one_with_a_branch_is_a_conflict() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        fs::create_dir_all(dir.path().join("refs/heads/Main/deeper")).unwrap();
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.branch(&name("main")).unwrap(), Some(id));
        // A variant that holds a branch stays a conflict.
        refs.update_branch(&name("Other"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let error = refs
            .update_branch(&name("other"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(
                &error,
                RefError::Conflict {
                    kind: ConflictKind::CaseVariant,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn empty_directories_are_removed_up_to_the_top_and_not_beyond() {
        let dir = TempDir::new();
        let top = dir.path().join("heads");
        fs::create_dir_all(top.join("a/b/c")).unwrap();
        fs::create_dir_all(top.join("a/keep")).unwrap();
        remove_empty_directories(&top.join("a/b/c"), &top);
        assert!(!top.join("a/b").exists(), "a/b/c and a/b are empty and go");
        assert!(
            top.join("a/keep").exists(),
            "a sibling stays, and so does a"
        );
        // A directory with a file stops the removal, and the top is never removed.
        fs::create_dir_all(top.join("f/g")).unwrap();
        fs::write(top.join("f/file"), b"x").unwrap();
        remove_empty_directories(&top.join("f/g"), &top);
        assert!(top.join("f/file").exists() && !top.join("f/g").exists());
        remove_empty_directories(&top, &top);
        assert!(top.exists());
        // A directory outside the top is left alone.
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        remove_empty_directories(&outside, &top);
        assert!(outside.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_change_that_cannot_make_its_temporary_file_leaves_no_directory_behind() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, objects, refs) = setup();
        if !permissions_hold(dir.path()) {
            return;
        }
        let id = commit(&objects, "c");
        let tmp = dir.path().join("tmp");
        fs::create_dir_all(&tmp).unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o555)).unwrap();
        let error = refs
            .update_branch(&name("feat/x"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(&error, RefError::Io { .. }), "{error}");
        assert!(!dir.path().join("refs/heads/feat").exists());
        assert!(!dir.path().join("refs.lock").exists());
        // The name is free afterwards.
        refs.update_branch(&name("feat"), None, id, &objects, &Allowed::granted())
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_change_that_cannot_flush_its_directory_says_that_it_was_made() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, objects, refs) = setup();
        if !permissions_hold(dir.path()) {
            return;
        }
        let id = commit(&objects, "c");
        refs.set_head(&Head::Branch(name("main")), &objects, &Allowed::granted())
            .unwrap();
        // The directory may be written and entered, but not read: the rename works and the flush
        // of the directory cannot open it.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o300)).unwrap();
        let result = refs.set_head(&Head::Detached(id), &objects, &Allowed::granted());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let error = result.unwrap_err();
        assert!(
            matches!(&error, RefError::NotFlushed { path, .. } if path == dir.path()),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .starts_with("the change was made, but the directory `"),
            "{error}"
        );
        assert!(error.io_error().is_some());
        // The change happened, and the lock is gone.
        assert_eq!(refs.head().unwrap(), Head::Detached(id));
        assert!(!dir.path().join("refs.lock").exists());
    }

    // ---- the guard of the lock (STEMMA-51)

    /// A lock file in the form that a holder of the guard on `host` writes.
    fn lock_of(host: &str, process: u32) -> String {
        format!("{LOCK_HEADER}\nhost {host}\npid {process}\ntoken 1-0-left\n")
    }

    /// A lock file that a process of this host left when it ended.
    fn stale_lock(process: u32) -> String {
        lock_of(host_name().expect("the host has a name"), process)
    }

    #[test]
    fn a_lock_whose_guard_is_free_is_taken_over() {
        // A process that ended by `kill -9` or a crash leaves its lock file, and the operating
        // system releases its guard: the next change replaces the file.
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let lock = dir.path().join("refs.lock");
        fs::write(&lock, stale_lock(4_000_000)).unwrap();
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert_eq!(refs.branch(&name("main")).unwrap(), Some(id));
        assert!(!lock.exists());
        // The guard stays, as a file without content that nothing reads.
        assert!(dir.path().join("refs.lock.guard").is_file());
    }

    /// Holds the guard through a handle of its own, as another process that holds the lock does:
    /// the system keeps these locks per handle (`flock`, `LockFileEx`), so `acquire` finds it
    /// held, and nothing in [`HELD`] stands in for it.
    fn hold_guard(dir: &Path) -> fs::File {
        let guard = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("refs.lock.guard"))
            .unwrap();
        guard.try_lock().unwrap();
        guard
    }

    #[test]
    fn a_lock_whose_guard_is_held_stops_the_change_and_names_the_process() {
        // The file names a process that is not this one, and a free guard would let the change
        // take it over: only the held guard keeps the running holder's lock in place.
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let lock = dir.path().join("refs.lock");
        let holder = stale_lock(4_000_000);
        fs::write(&lock, &holder).unwrap();
        let guard = hold_guard(dir.path());
        let error = refs
            .update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(
                error,
                RefError::Locked {
                    holder: LockHolder::Running {
                        process: Some(4_000_000)
                    },
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the references are locked by `{}`: another stemma command (process 4000000) is \
                 running and holds the lock; try again when it has ended",
                lock.display()
            )
        );
        // `--resume` and `--abort` of a purge do not take a lock whose guard is held either.
        assert!(matches!(
            refs.take_over_lock(&Allowed::granted()),
            Err(RefError::Locked {
                holder: LockHolder::Running { .. },
                ..
            })
        ));
        assert_eq!(fs::read_to_string(&lock).unwrap(), holder);
        // A holder whose file cannot be parsed is named without its process.
        fs::write(&lock, b"").unwrap();
        let error = refs
            .update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "the references are locked by `{}`: another stemma command is running and holds \
                 the lock; try again when it has ended",
                lock.display()
            )
        );
        // The holder ends: the operating system releases the guard, and the lock is taken over.
        fs::write(&lock, &holder).unwrap();
        drop(guard);
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert!(!lock.exists());
    }

    #[test]
    fn a_lock_that_this_process_holds_is_not_taken_again() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let held = refs.lock().unwrap();
        let lock = dir.path().join("refs.lock");
        assert_eq!(
            LockFile::read(&lock).unwrap(),
            Some(LockFile {
                host: host_name().unwrap().to_owned(),
                process: std::process::id()
            })
        );
        let error = refs
            .update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(
                error,
                RefError::Locked {
                    holder: LockHolder::ThisProcess,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the references are locked by `{}`: this process holds the lock already, and the \
                 lock is not re-entrant",
                lock.display()
            )
        );
        // Another spelling of the same directory finds the lock as well, and the guard is not
        // opened a second time.
        let other = FsRefStore::new(dir.path().join("."));
        let result = other.take_over_lock(&Allowed::granted()).map(drop);
        assert!(
            matches!(
                result,
                Err(RefError::Locked {
                    holder: LockHolder::ThisProcess,
                    ..
                })
            ),
            "{result:?}"
        );
        drop(held);
        assert!(!lock.exists());
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
    }

    #[test]
    fn a_lock_that_this_process_holds_is_not_hidden_by_a_pending_purge() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let held = refs.lock().unwrap();
        fs::write(dir.path().join(PURGE_JOURNAL), b"journal").unwrap();
        let error = refs
            .update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(
                error,
                RefError::Locked {
                    holder: LockHolder::ThisProcess,
                    ..
                }
            ),
            "{error}"
        );
        drop(held);
    }

    #[test]
    fn a_lock_of_another_form_is_not_taken_over() {
        // An older version, a file system without locks of files, or a person made it: the file
        // stays, and the message says that it may be left from a crash.
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let lock = dir.path().join("refs.lock");
        let pid = std::process::id();
        let host = host_name().unwrap();
        for content in [
            String::from("4711-0-0123456789abcdef"),
            String::new(),
            format!("{LOCK_HEADER}\npid 7\ntoken x\n"),
            format!("{LOCK_HEADER}\nhost {host}\npid 7\ntoken x"),
            format!("{LOCK_HEADER}\nhost {host}\npid 07\ntoken x\n"),
            format!("{LOCK_HEADER}\nhost {host}\npid \ntoken x\n"),
            format!("{LOCK_HEADER}\nhost {host}\npid 7\ntoken \n"),
            format!("{LOCK_HEADER}\nhost {host}\npid 7\ntoken x\n\n"),
            format!("{LOCK_HEADER}\nhost \npid 7\ntoken x\n"),
            format!("{LOCK_HEADER}\nhost a b\npid 7\ntoken x\n"),
            format!("stemma-lock 2\nhost {host}\npid 7\ntoken x\n"),
        ] {
            fs::write(&lock, &content).unwrap();
            let error = refs
                .update_branch(&name("main"), None, id, &objects, &Allowed::granted())
                .unwrap_err();
            assert!(
                matches!(
                    &error,
                    RefError::Locked {
                        holder: LockHolder::Unknown,
                        ..
                    }
                ),
                "{content:?}: {error}"
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "the references are locked by `{}`: another stemma command is running, and \
                     holds the lock while it saves the working directory; if none runs, the file \
                     is left from a crash, of an older version or of a command that could not \
                     take the guard of the lock, and can be removed",
                    lock.display()
                )
            );
            assert_eq!(fs::read_to_string(&lock).unwrap(), content);
        }
        // `--resume` and `--abort` take any lock whose guard is free: the one the purge left.
        let taken = refs.take_over_lock(&Allowed::granted()).unwrap();
        assert_eq!(
            LockFile::read(&lock).unwrap().map(|lock| lock.process),
            Some(pid)
        );
        drop(taken);
        assert!(!lock.exists());
    }

    #[test]
    fn a_lock_of_another_host_is_not_taken_over() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let lock = dir.path().join("refs.lock");
        // Another host: a lock of the operating system may not reach across a shared file system,
        // so a free guard tells nothing about a command that runs there.
        let elsewhere = lock_of("other-host", 4711);
        fs::write(&lock, &elsewhere).unwrap();
        let error = refs
            .update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(
                &error,
                RefError::Locked {
                    holder: LockHolder::Elsewhere { host, process: 4711 },
                    ..
                } if host == "other-host"
            ),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the references are locked by `{}`: a stemma command (process 4711) on the host \
                 `other-host` holds the lock, or left it from a crash; if none runs there, the \
                 file can be removed",
                lock.display()
            )
        );
        assert!(matches!(
            refs.take_over_lock(&Allowed::granted()),
            Err(RefError::Locked {
                holder: LockHolder::Elsewhere { .. },
                ..
            })
        ));
        assert_eq!(fs::read_to_string(&lock).unwrap(), elsewhere);
    }

    #[test]
    fn a_lock_that_names_this_process_is_taken_over_when_this_process_holds_none() {
        // An earlier process had the ID that this one has now, as in a container where the
        // command always runs as the same process, and left its lock when it crashed.
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let lock = dir.path().join("refs.lock");
        fs::write(&lock, stale_lock(std::process::id())).unwrap();
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert!(!lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_guard_that_cannot_be_opened_costs_the_takeover_and_not_the_exclusion() {
        // Another user made the guard, and this one may not open it: the lock is made in the
        // form of an older version, which still excludes, and which no command takes over.
        use std::os::unix::fs::PermissionsExt;
        let (dir, objects, refs) = setup();
        assert_permissions_bind(dir.path());
        let id = commit(&objects, "c");
        let guard = dir.path().join("refs.lock.guard");
        let lock = dir.path().join("refs.lock");
        fs::write(&guard, b"").unwrap();
        fs::set_permissions(&guard, fs::Permissions::from_mode(0o000)).unwrap();
        let made = refs.lock().map(|held| {
            let content = fs::read_to_string(&lock);
            drop(held);
            content
        });
        // A file left by a holder of the guard, and one of the older form.
        let guarded = stale_lock(4_000_000);
        fs::write(&lock, &guarded).unwrap();
        let change = refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted());
        let resume = refs.take_over_lock(&Allowed::granted()).map(drop);
        let guarded_after = fs::read_to_string(&lock);
        fs::write(&lock, b"4711-0-0123456789abcdef").unwrap();
        let older = refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted());
        let older_after = fs::read_to_string(&lock);
        let older_resume = refs.take_over_lock(&Allowed::granted()).map(drop);
        let older_resumed = lock.exists();
        fs::set_permissions(&guard, fs::Permissions::from_mode(0o644)).unwrap();

        let content = made.unwrap().unwrap();
        assert!(!content.starts_with(LOCK_HEADER), "{content:?}");
        // Neither file is taken over by a change, and the message says why.
        for error in [change.unwrap_err(), older.unwrap_err()] {
            assert!(
                matches!(
                    &error,
                    RefError::Locked {
                        holder: LockHolder::Unverified { reason },
                        ..
                    } if reason.starts_with(&format!("`{}` cannot be opened: ", guard.display()))
                ),
                "{error}"
            );
            assert!(
                error
                    .to_string()
                    .contains("the guard of the lock cannot be taken"),
                "{error}"
            );
        }
        assert_eq!(older_after.unwrap(), "4711-0-0123456789abcdef");
        // `--resume` and `--abort` leave the guarded file, whose holder may run, and replace the
        // one of the older form, which a purge without a guard leaves, as before the guard.
        assert!(matches!(
            resume,
            Err(RefError::Locked {
                holder: LockHolder::Unverified { .. },
                ..
            })
        ));
        assert_eq!(guarded_after.unwrap(), guarded);
        older_resume.unwrap();
        assert!(!older_resumed);
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
    }

    #[test]
    fn a_placeholder_does_not_name_a_host() {
        assert!(usable_host("build-01.example.org"));
        for name in [
            "localhost",
            "LOCALHOST",
            "localhost.localdomain",
            "(none)",
            "",
            "a b",
        ] {
            assert!(!usable_host(name), "{name:?}");
        }
    }

    #[test]
    fn a_stale_lock_stays_while_the_journal_of_a_purge_exists() {
        // The lock that a purge left belongs to `--resume` and `--abort`, which report it as the
        // purge; another change reports the purge and does not touch the lock (ADR-014).
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let lock = dir.path().join("refs.lock");
        let left = stale_lock(4_000_000);
        fs::write(&lock, &left).unwrap();
        fs::write(dir.path().join(PURGE_JOURNAL), b"").unwrap();
        assert!(matches!(
            refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted()),
            Err(RefError::PurgePending { .. })
        ));
        assert_eq!(fs::read_to_string(&lock).unwrap(), left);
    }

    #[test]
    fn a_purge_that_stopped_leaves_a_lock_that_the_next_change_takes_over() {
        // `leak` is a crash: the file stays, and the guard is released. Without a journal, as
        // after a crash before the purge wrote it, the next change takes the lock over.
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.lock_purge(&Allowed::granted()).unwrap().leak();
        let lock = dir.path().join("refs.lock");
        assert_eq!(
            LockFile::read(&lock).unwrap().map(|lock| lock.process),
            Some(0)
        );
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert!(!lock.exists());
    }

    #[test]
    fn a_lock_is_removed_only_by_the_one_that_made_it() {
        let dir = TempDir::new();
        let path = dir.path().join("refs.lock");
        // Taken over by another: the token in the file is not ours, and the file stays.
        fs::write(&path, b"other").unwrap();
        drop(Lock {
            path: path.clone(),
            token: "mine".to_owned(),
            guard: None,
        });
        assert_eq!(fs::read(&path).unwrap(), b"other");
        // Our own lock goes.
        fs::write(&path, b"mine").unwrap();
        drop(Lock {
            path: path.clone(),
            token: "mine".to_owned(),
            guard: None,
        });
        assert!(!path.exists());
        // A lock that is already gone is no error.
        drop(Lock {
            path: path.clone(),
            token: "mine".to_owned(),
            guard: None,
        });
        // A real lock holds a token of its own, which differs between locks.
        let (_dir, _objects, refs) = setup();
        let first = refs.lock().unwrap();
        assert_eq!(fs::read(&first.path).unwrap(), first.token.as_bytes());
        assert!(matches!(refs.lock(), Err(RefError::Locked { .. })));
        let token = first.token.clone();
        drop(first);
        let second = refs.lock().unwrap();
        assert_ne!(second.token, token);
    }

    #[test]
    fn entries_with_a_leading_dot_are_not_branches() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let heads = dir.path().join("refs/heads");
        fs::write(heads.join(".DS_Store"), [0u8, 1, 2]).unwrap();
        fs::write(heads.join("._main"), [0u8]).unwrap();
        fs::create_dir(heads.join(".hidden")).unwrap();
        fs::write(heads.join(".hidden/file"), b"x").unwrap();
        assert_eq!(refs.branches().unwrap(), vec![(name("main"), id)]);
        // Other names that are not branch names are still damage.
        fs::write(heads.join("not~valid"), format!("{id}\n")).unwrap();
        assert!(matches!(
            refs.branches(),
            Err(RefError::Corrupt {
                path,
                source: RefParseError::InvalidBranchName(_),
            }) if path == heads.join("not~valid")
        ));
    }

    #[test]
    fn the_rules_for_names_hold_below_a_directory_and_for_a_directory() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("feat/x"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let heads = dir.path().join("refs/heads");
        fs::write(heads.join("feat/.DS_Store"), [0u8]).unwrap();
        assert_eq!(refs.branches().unwrap(), vec![(name("feat/x"), id)]);
        // A directory whose name is not the start of a branch name is damage, empty or not: it is
        // not skipped with what is below it.
        for stray in ["foo~", "feat/x-~"] {
            fs::create_dir(heads.join(stray)).unwrap();
            assert!(
                matches!(
                    refs.branches(),
                    Err(RefError::Corrupt {
                        path,
                        source: RefParseError::InvalidBranchName(_),
                    }) if path == heads.join(stray)
                ),
                "{stray}"
            );
            fs::remove_dir(heads.join(stray)).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_below_the_branches_is_an_error_and_not_a_branch() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let heads = dir.path().join("refs/heads");
        std::os::unix::fs::symlink(heads.join("main"), heads.join("alias")).unwrap();
        let error = refs.branches().unwrap_err();
        assert!(
            matches!(&error, RefError::Io { path, .. } if *path == heads.join("alias")),
            "{error}"
        );
    }

    /// Fails unless a file without permissions cannot be read, as for a user who is not root.
    #[cfg(unix)]
    fn assert_permissions_bind(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let probe = dir.join("probe");
        fs::create_dir(&probe).unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o000)).unwrap();
        let binds = fs::read_dir(&probe).is_err();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir(&probe).unwrap();
        assert!(
            binds,
            "permissions do not bind this user; do not run the tests as root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_branch_or_a_directory_that_cannot_be_read_is_an_error_and_not_an_absence() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, objects, refs) = setup();
        assert_permissions_bind(dir.path());
        let id = commit(&objects, "c");
        refs.update_branch(&name("a/b"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let mode = |path: &Path, mode| {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        };
        let file = dir.path().join("refs/heads/a/b");
        mode(&file, 0o000);
        let listed = refs.branches();
        let looked_up = refs.branch(&name("a/b"));
        mode(&file, 0o644);
        assert!(
            matches!(&listed, Err(RefError::Io { path, .. }) if *path == file),
            "{listed:?}"
        );
        assert!(
            matches!(looked_up, Err(RefError::Io { .. })),
            "{looked_up:?}"
        );
        // A directory: the listing, and the lookup of a name that the directory holds.
        let sub = dir.path().join("refs/heads/a");
        mode(&sub, 0o000);
        let listed = refs.branches();
        let looked_up = refs.branch(&name("a"));
        mode(&sub, 0o755);
        assert!(matches!(listed, Err(RefError::Io { .. })), "{listed:?}");
        assert!(
            matches!(looked_up, Err(RefError::Io { .. })),
            "{looked_up:?}"
        );
    }

    #[test]
    fn a_listing_while_branches_come_and_go_does_not_fail() {
        // A writer creates `a/b` and deletes it with its directory, again and again; a reader
        // without the lock lists the branches meanwhile and sees each one or not, but no error.
        use std::sync::atomic::{AtomicBool, Ordering};
        let (_dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..300 {
                    refs.update_branch(&name("a/b"), None, id, &objects, &Allowed::granted())
                        .unwrap();
                    refs.delete_branch_file(&name("a/b")).unwrap();
                }
                done.store(true, Ordering::SeqCst);
            });
            while !done.load(Ordering::SeqCst) {
                let listed = refs.branches().unwrap();
                assert!(listed.contains(&(name("main"), id)), "{listed:?}");
                assert!(
                    listed
                        .iter()
                        .all(|(branch, _)| ["main", "a/b"].contains(&branch.as_str())),
                    "{listed:?}"
                );
            }
        });
    }

    #[test]
    fn an_entry_that_is_gone_after_it_was_listed_is_no_branch() {
        // A listing runs without the lock, so a change may delete a branch, or the last branch
        // below a directory and with it the directory, between the listing and the read.
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let heads = dir.path().join("refs/heads");
        assert!(list_directory(&heads.join("gone")).unwrap().is_none());
        assert_eq!(refs.listed_branch(heads.join("gone")).unwrap(), None);
        assert!(!has_files_below(&heads.join("gone")).unwrap());
        // What is there is read as before, and damage is still damage.
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        assert!(list_directory(&heads).unwrap().is_some());
        assert_eq!(refs.listed_branch(heads.join("main")).unwrap(), Some(id));
        fs::write(heads.join("main"), id.to_string()).unwrap();
        assert!(matches!(
            refs.listed_branch(heads.join("main")),
            Err(RefError::Corrupt { .. })
        ));
        // A file where a directory of branches should be is no absence. Windows may report such a
        // path as not found, but a listing reaches it only through a race, as it inspects the
        // kind of each entry first.
        #[cfg(unix)]
        assert!(matches!(
            list_directory(&heads.join("main")),
            Err(RefError::Io { .. })
        ));
        fs::create_dir(heads.join("x")).unwrap();
        assert!(matches!(
            refs.listed_branch(heads.join("x")),
            Err(RefError::Io { .. })
        ));
    }

    #[test]
    fn a_directory_or_a_file_in_the_wrong_place_is_an_error_and_not_an_absence() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        // HEAD is a directory: not "no HEAD".
        fs::create_dir(dir.path().join("HEAD")).unwrap();
        let error = refs.head().unwrap_err();
        assert!(
            matches!(
                &error,
                RefError::Io {
                    operation: "read the reference",
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.to_string().ends_with("not a regular file"), "{error}");
        // `refs` is a file: reading and writing agree that it is damage.
        fs::write(dir.path().join("refs"), b"x").unwrap();
        for error in [
            refs.branch(&name("main")).unwrap_err(),
            refs.branches().unwrap_err(),
            refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
                .unwrap_err(),
        ] {
            assert!(matches!(&error, RefError::Io { .. }), "{error}");
        }
        assert!(!dir.path().join("refs.lock").exists());
    }

    #[test]
    fn a_failed_check_of_the_object_store_is_an_io_error_when_the_file_system_failed() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let faulty = FaultyStore::new(dir.store(), true, false);
        let error = refs
            .set_head(&Head::Detached(id), &faulty, &Allowed::granted())
            .unwrap_err();
        assert!(matches!(&error, RefError::Io { .. }), "{error}");
        assert_eq!(
            error.io_error().map(io::Error::kind),
            Some(io::ErrorKind::Other)
        );
        // A damaged object is not a failure of the file system.
        let damaged = commit(&objects, "damaged");
        crate::testutil::overwrite(
            &dir.store().object_path(&damaged.object_id()),
            b"commit 1\0x",
        );
        let error = refs
            .update_branch(&name("main"), None, damaged, &objects, &Allowed::granted())
            .unwrap_err();
        assert!(
            matches!(error.store_error(), Some(StoreError::Corrupt { .. })),
            "{error}"
        );
    }

    #[test]
    fn a_move_that_fails_removes_the_directories_that_it_made() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("keep/this"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let heads = dir.path().join("refs/heads");
        let target = heads.join("p/q/r");
        let parent = target.parent().unwrap();
        let from = dir.path().join("from");
        fs::write(&from, b"x").unwrap();
        let failing = |_: &Path, _: &Path| Err(io::Error::other("the move failed"));
        let error = refs
            .place(&from, &target, parent, &heads, failing)
            .unwrap_err();
        assert!(matches!(&error, RefError::Io { .. }), "{error}");
        assert!(error.to_string().ends_with("the move failed"), "{error}");
        assert!(
            !heads.join("p").exists(),
            "p, p/q, and p/q/r were made by this call"
        );
        assert!(heads.join("keep/this").is_file(), "other branches stay");
        assert!(heads.is_dir(), "the top stays");
        // A move that works leaves the directories.
        refs.place(&from, &target, parent, &heads, |from, to| {
            fs::rename(from, to)
        })
        .unwrap();
        assert!(target.is_file());
    }

    // ---- update_branch_after and set_head_after (the lock over a step, ADR-011)

    #[test]
    fn a_lock_is_listed_while_it_exists_and_released_only_with_its_own_token() {
        let (dir, _objects, refs) = setup();
        let path = dir.path().join("refs.lock");
        let listed = |path: &Path| held_locks().iter().any(|(held, _)| held == path);
        let lock = refs.lock_writer(&Allowed::granted()).unwrap();
        assert!(listed(&path));
        drop(lock);
        assert!(!listed(&path));
        assert!(!path.exists());
        // An interruption removes the file of a lock that the process holds.
        let lock = refs.lock_writer(&Allowed::granted()).unwrap();
        let mut own: Vec<_> = held_locks()
            .iter()
            .filter(|(held, _)| held == &path)
            .cloned()
            .collect();
        release(&mut own);
        assert!(!path.exists());
        drop(lock);
        // A file that holds another token (another process took the lock since) stays.
        fs::write(&path, b"other").unwrap();
        release(&mut vec![(path.clone(), "mine".to_owned())]);
        assert_eq!(fs::read(&path).unwrap(), b"other");
    }

    #[test]
    fn a_move_waits_while_the_list_of_held_locks_is_held() {
        let (dir, _objects, refs) = setup();
        let path = dir.path().join("moved");
        // An interruption holds the list from the removal of the locks to the end.
        let held = held_locks();
        std::thread::scope(|scope| {
            let mover = scope.spawn(|| refs.write_replacing(&path, b"new", dir.path()));
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(!path.exists(), "the move waits");
            drop(held);
            mover.join().unwrap().unwrap();
        });
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    /// The lock of a writer, for a step that needs it.
    fn writer(refs: &FsRefStore) -> WriteLock {
        refs.lock_writer(&Allowed::granted()).unwrap()
    }

    /// A step that records that it ran and returns its number.
    fn counted(ran: &std::cell::Cell<u32>) -> impl FnOnce() -> Result<u32, &'static str> + '_ {
        move || {
            ran.set(ran.get() + 1);
            Ok(ran.get())
        }
    }

    #[test]
    fn a_step_runs_only_for_a_move_that_is_made_and_the_pointer_moves_after_it() {
        let (_dir, objects, refs) = setup();
        let (a, b) = (commit(&objects, "a"), commit(&objects, "b"));
        let ran = std::cell::Cell::new(0);
        let outcome = refs
            .update_branch_after(&writer(&refs), &name("main"), None, a, &objects, || {
                // The step runs before the pointer moves.
                assert_eq!(refs.branch(&name("main")).unwrap(), None);
                counted(&ran)()
            })
            .unwrap();
        assert!(matches!(outcome, Logged::Moved(1)));
        assert_eq!(refs.branch(&name("main")).unwrap(), Some(a));
        // At the value already: the step does not run.
        let outcome = refs
            .update_branch_after(
                &writer(&refs),
                &name("main"),
                Some(a),
                a,
                &objects,
                counted(&ran),
            )
            .unwrap();
        assert!(matches!(outcome, Logged::Unchanged));
        assert_eq!(ran.get(), 1);
        // Not at the value that the caller expected, even if that is the new value: stale. The
        // check for "already there" comes after the check for "where the caller saw it".
        for expected in [None, Some(b)] {
            let error = refs
                .update_branch_after(
                    &writer(&refs),
                    &name("main"),
                    expected,
                    a,
                    &objects,
                    counted(&ran),
                )
                .map(|_| ())
                .unwrap_err();
            assert!(
                matches!(&error, RefError::Stale { found: Some(f), .. } if *f == a),
                "{expected:?}: {error}"
            );
        }
        assert_eq!(ran.get(), 1);
    }

    #[test]
    fn the_lock_is_held_while_the_step_runs_and_released_after_every_outcome() {
        let (dir, objects, refs) = setup();
        let (a, b) = (commit(&objects, "a"), commit(&objects, "b"));
        refs.set_head(&Head::Branch(name("main")), &objects, &Allowed::granted())
            .unwrap();
        let lock_file = dir.path().join("refs.lock");
        // While the step runs, the lock file exists, and a second change meets the lock.
        let outcome = refs
            .update_branch_after(&writer(&refs), &name("main"), None, a, &objects, || {
                assert!(lock_file.exists());
                let inner =
                    refs.update_branch(&name("other"), None, b, &objects, &Allowed::granted());
                assert!(matches!(inner, Err(RefError::Locked { .. })), "{inner:?}");
                let inner = refs.set_head(&Head::Branch(name("x")), &objects, &Allowed::granted());
                assert!(matches!(inner, Err(RefError::Locked { .. })), "{inner:?}");
                Ok::<(), ()>(())
            })
            .unwrap();
        assert!(matches!(outcome, Logged::Moved(())));
        assert!(!lock_file.exists());
        // The same for the step of HEAD.
        let outcome = refs
            .set_head_after(
                &writer(&refs),
                &Head::Branch(name("dev")),
                &objects,
                |old| {
                    assert_eq!(old, &Head::Branch(name("main")));
                    assert!(lock_file.exists());
                    let inner =
                        refs.update_branch(&name("other"), None, b, &objects, &Allowed::granted());
                    assert!(matches!(inner, Err(RefError::Locked { .. })), "{inner:?}");
                    Ok::<(), ()>(())
                },
            )
            .unwrap();
        assert!(matches!(outcome, Logged::Moved(())));
        assert!(!lock_file.exists());
        // Released after a step that failed, after "at the value already", and after a refusal.
        let failed = refs
            .update_branch_after(&writer(&refs), &name("main"), Some(a), b, &objects, || {
                Err::<(), _>("no")
            })
            .unwrap();
        assert!(matches!(failed, Logged::StepFailed("no")));
        assert!(!lock_file.exists());
        assert_eq!(refs.branch(&name("main")).unwrap(), Some(a));
        let same = refs
            .update_branch_after(&writer(&refs), &name("main"), Some(a), a, &objects, || {
                Ok::<(), ()>(())
            })
            .unwrap();
        assert!(matches!(same, Logged::Unchanged));
        assert!(!lock_file.exists());
        let stale =
            refs.update_branch_after(&writer(&refs), &name("main"), None, b, &objects, || {
                Ok::<(), ()>(())
            });
        assert!(matches!(stale, Err(RefError::Stale { .. })));
        assert!(!lock_file.exists());
        // A lock that another writer holds is not taken, and its file is not removed.
        fs::write(&lock_file, b"other").unwrap();
        let held = refs.lock_writer(&Allowed::granted()).map(|_| ());
        assert!(matches!(held, Err(RefError::Locked { .. })));
        assert_eq!(fs::read(&lock_file).unwrap(), b"other");
    }

    #[test]
    fn the_commit_and_the_name_are_checked_under_the_lock_before_the_step() {
        let (_dir, objects, refs) = setup();
        let a = commit(&objects, "a");
        let ran = std::cell::Cell::new(0);
        let absent = CommitId::new(ID_A.parse().unwrap());
        let error = refs
            .update_branch_after(
                &writer(&refs),
                &name("main"),
                None,
                absent,
                &objects,
                counted(&ran),
            )
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(error, RefError::MissingCommit(_)), "{error}");
        refs.update_branch(&name("main"), None, a, &objects, &Allowed::granted())
            .unwrap();
        let error = refs
            .update_branch_after(
                &writer(&refs),
                &name("main/x"),
                None,
                a,
                &objects,
                counted(&ran),
            )
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(error, RefError::Conflict { .. }), "{error}");
        let error = refs
            .set_head_after(&writer(&refs), &Head::Detached(absent), &objects, |_| {
                counted(&ran)()
            })
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(error, RefError::MissingCommit(_)), "{error}");
        assert_eq!(ran.get(), 0, "no step ran for a change that was refused");
    }

    #[test]
    fn an_empty_directory_that_a_stopped_change_left_is_removed_before_the_step() {
        let (dir, objects, refs) = setup();
        let a = commit(&objects, "a");
        let leftover = dir.path().join("refs/heads/feat/deeper");
        fs::create_dir_all(&leftover).unwrap();
        let outcome = refs
            .update_branch_after(&writer(&refs), &name("feat"), None, a, &objects, || {
                // The directory is gone when the step runs, so a failure to remove it could never
                // leave a step that names a move that cannot be made.
                assert!(!leftover.exists());
                Ok::<(), ()>(())
            })
            .unwrap();
        assert!(matches!(outcome, Logged::Moved(())));
        assert_eq!(refs.branch(&name("feat")).unwrap(), Some(a));
        assert!(dir.path().join("refs/heads/feat").is_file());
    }

    #[test]
    fn a_head_that_is_missing_or_damaged_is_an_error_and_no_step_runs() {
        let (dir, objects, refs) = setup();
        let ran = std::cell::Cell::new(0);
        let head = dir.path().join("HEAD");
        let error = refs
            .set_head_after(&writer(&refs), &Head::Branch(name("x")), &objects, |_| {
                counted(&ran)()
            })
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(error, RefError::NoHead { .. }), "{error}");
        assert!(!head.exists());
        fs::write(&head, b"damaged\n").unwrap();
        let error = refs
            .set_head_after(&writer(&refs), &Head::Branch(name("x")), &objects, |_| {
                counted(&ran)()
            })
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(error, RefError::Corrupt { .. }), "{error}");
        assert_eq!(fs::read(&head).unwrap(), b"damaged\n");
        assert_eq!(ran.get(), 0);
        assert!(!dir.path().join("refs.lock").exists());
    }

    #[test]
    fn the_outcome_of_two_moves_says_which_pointer_moved() {
        let disk = || io::Error::other("disk");
        let unflushed = |path: &str| RefError::NotFlushed {
            path: PathBuf::from(path),
            source: disk(),
        };
        let hard = || RefError::Io {
            operation: "write the reference",
            path: PathBuf::from("/x/working-copy"),
            source: disk(),
        };
        type Outcome = Logged<u8, ()>;
        let two = |first: Result<(), RefError>, second: Result<(), RefError>| {
            let ran = std::cell::Cell::new(false);
            let outcome = FsRefStore::after_two_moves::<u8, ()>(5, first, || {
                ran.set(true);
                second
            });
            (outcome, ran.get())
        };
        assert!(matches!(two(Ok(()), Ok(())), (Outcome::Moved(5), true)));
        // The first move failed hard: the second is not tried.
        assert!(matches!(
            two(Err(hard()), Ok(())),
            (Outcome::MoveFailed(5, RefError::Io { .. }), false)
        ));
        // Not flushed counts as moved, so the second is tried, and the flush error is kept.
        assert!(matches!(
            two(Err(unflushed("/heads")), Ok(())),
            (Outcome::NotFlushed(5, RefError::NotFlushed { path, .. }), true) if path == Path::new("/heads")
        ));
        assert!(matches!(
            two(Ok(()), Err(unflushed("/root"))),
            (Outcome::NotFlushed(5, RefError::NotFlushed { path, .. }), true) if path == Path::new("/root")
        ));
        assert!(matches!(
            two(Err(unflushed("/heads")), Err(unflushed("/root"))),
            (Outcome::NotFlushed(5, RefError::NotFlushed { path, .. }), true) if path == Path::new("/heads")
        ));
        // The second failed hard: one pointer moved.
        assert!(matches!(
            two(Ok(()), Err(hard())),
            (Outcome::PartlyMoved(5, RefError::Io { .. }), true)
        ));
        assert!(matches!(
            two(Err(unflushed("/heads")), Err(hard())),
            (Outcome::PartlyMoved(5, RefError::Io { .. }), true)
        ));
    }

    #[test]
    fn the_outcome_of_a_move_says_whether_the_pointer_moved() {
        let disk = || io::Error::other("disk");
        let flushed = || RefError::NotFlushed {
            path: PathBuf::from("/x"),
            source: disk(),
        };
        let io_failure = || RefError::Io {
            operation: "write the reference",
            path: PathBuf::from("/x/main"),
            source: disk(),
        };
        type Outcome = Logged<u8, ()>;
        // The pointer moved: the flush of its directory is the only thing that failed.
        assert!(matches!(
            FsRefStore::after_move::<u8, ()>(5, Ok(())),
            Outcome::Moved(5)
        ));
        assert!(matches!(
            FsRefStore::after_move::<u8, ()>(5, Err(flushed())),
            Outcome::NotFlushed(5, RefError::NotFlushed { .. })
        ));
        // The pointer did not move: any other error of the move.
        assert!(matches!(
            FsRefStore::after_move::<u8, ()>(5, Err(io_failure())),
            Outcome::MoveFailed(5, RefError::Io { .. })
        ));
        assert!(matches!(
            FsRefStore::after_move::<u8, ()>(
                5,
                Err(RefError::Locked {
                    path: PathBuf::from("l"),
                    holder: LockHolder::Unknown
                })
            ),
            Outcome::MoveFailed(5, RefError::Locked { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_move_that_fails_after_the_step_leaves_the_step_done_and_the_pointer_where_it_was() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, objects, refs) = setup();
        let a = commit(&objects, "a");
        let heads = dir.path().join("refs/heads");
        fs::create_dir_all(&heads).unwrap();
        fs::set_permissions(&heads, fs::Permissions::from_mode(0o555)).unwrap();
        let writable = fs::File::create(heads.join("probe")).is_ok();
        let ran = std::cell::Cell::new(0);
        let outcome = refs.update_branch_after(
            &writer(&refs),
            &name("main"),
            None,
            a,
            &objects,
            counted(&ran),
        );
        fs::set_permissions(&heads, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_file(heads.join("probe"));
        if writable {
            eprintln!("skipped: the permission of a directory is not enforced here (root?)");
            return;
        }
        let outcome = outcome.unwrap();
        assert!(
            matches!(outcome, Logged::MoveFailed(1, RefError::Io { .. })),
            "the step ran once and the move failed"
        );
        assert_eq!(ran.get(), 1);
        assert_eq!(refs.branch(&name("main")).unwrap(), None);
        assert!(!dir.path().join("refs.lock").exists());
    }

    // ---- verify

    /// Writes a reference past the checks of the module, as a person or a damaged disk would.
    fn write_reference(dir: &TempDir, relative: &str, content: &str) {
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn references_that_name_commits_in_the_store_verify() {
        let (dir, objects, refs) = setup();
        let first = commit(&objects, "first");
        let second = commit(&objects, "second");
        write_reference(&dir, "HEAD", "branch main\n");
        write_reference(&dir, "working-copy", &format!("{second}\n"));
        refs.update_branch(&name("main"), None, first, &objects, &Allowed::granted())
            .unwrap();
        refs.update_branch(&name("feat/x"), None, second, &objects, &Allowed::granted())
            .unwrap();
        assert!(refs.verify(&objects).unwrap().is_empty());
        // HEAD may name a branch that does not exist yet, as it does in a new repository.
        write_reference(&dir, "HEAD", "branch next\n");
        assert!(refs.verify(&objects).unwrap().is_empty());
        write_reference(&dir, "HEAD", &format!("commit {first}\n"));
        assert!(refs.verify(&objects).unwrap().is_empty());
    }

    #[test]
    fn every_reference_that_fails_is_reported_in_order() {
        let (dir, objects, refs) = setup();
        let kept = commit(&objects, "kept");
        let removed = commit(&objects, "removed");
        let tree = objects.put_tree(&Tree::try_new(vec![]).unwrap()).unwrap();
        fs::remove_file(objects.object_path(&removed.object_id())).unwrap();
        write_reference(&dir, "HEAD", &format!("commit {removed}\n"));
        write_reference(&dir, "working-copy", &format!("{tree}\n"));
        write_reference(&dir, "refs/heads/a", &format!("{kept}\n"));
        write_reference(&dir, "refs/heads/z", &format!("{removed}\n"));
        write_reference(&dir, "refs/heads/b/c", &format!("{removed}\n"));
        write_reference(&dir, "refs/heads/M/y", &format!("{removed}\n"));

        let errors = refs.verify(&objects).unwrap();
        let places: Vec<&Pointer> = errors.iter().map(VerifyError::at).collect();
        // The branches by the bytes of their names, and the branch that holds is left out.
        assert_eq!(
            places,
            [
                &Pointer::Head,
                &Pointer::WorkingCopy,
                &Pointer::Branch(name("M/y")),
                &Pointer::Branch(name("b/c")),
                &Pointer::Branch(name("z"))
            ]
        );
        assert!(
            matches!(&errors[0], VerifyError::Missing { id, .. } if *id == removed.object_id())
        );
        assert!(matches!(
            errors[1].store_error(),
            Some(StoreError::KindMismatch {
                found: Kind::Tree,
                ..
            })
        ));
        assert_eq!(
            errors[3].to_string(),
            format!("the branch `b/c`: the store has no object {removed}")
        );
    }

    #[test]
    fn head_on_a_branch_is_checked_through_the_branch_and_each_pointer_counts() {
        let (dir, objects, refs) = setup();
        let kept = commit(&objects, "kept");
        let removed = commit(&objects, "removed");
        fs::remove_file(objects.object_path(&removed.object_id())).unwrap();
        write_reference(&dir, "HEAD", "branch main\n");
        write_reference(&dir, "working-copy", &format!("{kept}\n"));
        write_reference(&dir, "refs/heads/main", &format!("{removed}\n"));
        let places: Vec<Pointer> = refs
            .verify(&objects)
            .unwrap()
            .iter()
            .map(|error| error.at().clone())
            .collect();
        assert_eq!(places, [Pointer::Branch(name("main"))]);

        // Three pointers at one missing commit are three failures.
        write_reference(&dir, "HEAD", &format!("commit {removed}\n"));
        write_reference(&dir, "working-copy", &format!("{removed}\n"));
        let places: Vec<Pointer> = refs
            .verify(&objects)
            .unwrap()
            .iter()
            .map(|error| error.at().clone())
            .collect();
        assert_eq!(
            places,
            [
                Pointer::Head,
                Pointer::WorkingCopy,
                Pointer::Branch(name("main"))
            ]
        );
    }

    #[test]
    fn a_damaged_commit_is_a_store_error_and_not_missing() {
        let (dir, objects, refs) = setup();
        let damaged = commit(&objects, "damaged");
        crate::testutil::overwrite(&objects.object_path(&damaged.object_id()), b"commit 1\0x");
        write_reference(&dir, "HEAD", "branch main\n");
        write_reference(&dir, "working-copy", &format!("{damaged}\n"));
        let errors = refs.verify(&objects).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].at(), &Pointer::WorkingCopy);
        assert!(
            matches!(errors[0].store_error(), Some(StoreError::Corrupt { .. })),
            "{}",
            errors[0]
        );
    }

    #[test]
    fn the_check_does_not_follow_the_tree_or_the_parents() {
        let (dir, objects, refs) = setup();
        let parent = commit(&objects, "parent");
        let tree = objects.put_tree(&Tree::try_new(vec![]).unwrap()).unwrap();
        let child = Commit::try_new(
            tree,
            vec![parent],
            sample_change(),
            Actor::human("daniel").unwrap(),
            0,
            "child",
        );
        let child = objects.put_commit(&child.unwrap()).unwrap();
        fs::remove_file(objects.object_path(&parent.object_id())).unwrap();
        fs::remove_file(objects.object_path(&tree.object_id())).unwrap();
        write_reference(&dir, "HEAD", "branch main\n");
        write_reference(&dir, "working-copy", &format!("{child}\n"));
        assert!(refs.verify(&objects).unwrap().is_empty());
    }

    #[test]
    fn a_failure_of_the_storage_is_a_store_error_of_each_reference() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        write_reference(&dir, "HEAD", "branch main\n");
        write_reference(&dir, "working-copy", &format!("{id}\n"));
        refs.update_branch(&name("main"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let faulty = FaultyStore::new(dir.store(), true, false);
        let errors = refs.verify(&faulty).unwrap();
        let places: Vec<&Pointer> = errors.iter().map(VerifyError::at).collect();
        assert_eq!(
            places,
            [&Pointer::WorkingCopy, &Pointer::Branch(name("main"))]
        );
        for error in &errors {
            assert!(
                matches!(error.store_error(), Some(StoreError::Io { .. })),
                "{error}"
            );
            assert_eq!(error.to_string().matches(DISK_MESSAGE).count(), 1);
        }
    }

    #[test]
    fn the_references_are_read_before_the_store_is_asked() {
        let (dir, objects, refs) = setup();
        let kept = commit(&objects, "kept");
        write_reference(&dir, "HEAD", &format!("commit {kept}\n"));
        write_reference(&dir, "working-copy", &format!("{kept}\n"));
        write_reference(&dir, "refs/heads/main", &kept.to_string());
        let faulty = FaultyStore::new(dir.store(), true, false);
        assert!(matches!(
            refs.verify(&faulty),
            Err(RefError::Corrupt { .. })
        ));
    }

    #[test]
    fn the_check_does_not_take_the_lock() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        write_reference(&dir, "HEAD", "branch main\n");
        write_reference(&dir, "working-copy", &format!("{id}\n"));
        fs::write(dir.path().join("refs.lock"), b"held by a writer").unwrap();
        assert!(refs.verify(&objects).unwrap().is_empty());
    }

    #[test]
    fn a_commit_is_hashed_and_not_parsed() {
        let (dir, objects, refs) = setup();
        let raw = CommitId::new(objects.put_raw(Kind::Commit, b"tree x\n").unwrap());
        write_reference(&dir, "HEAD", "branch main\n");
        write_reference(&dir, "working-copy", &format!("{raw}\n"));
        assert!(refs.verify(&objects).unwrap().is_empty());
    }

    /// A store that runs a hook before its first read, so that a test can change the references
    /// while a check runs.
    struct OnFirstRead {
        inner: FsObjectStore,
        hook: std::cell::RefCell<Option<Box<dyn FnOnce()>>>,
    }

    impl OnFirstRead {
        fn new(inner: FsObjectStore, hook: impl FnOnce() + 'static) -> Self {
            Self {
                inner,
                hook: std::cell::RefCell::new(Some(Box::new(hook))),
            }
        }
    }

    impl crate::store::sealed::Sealed for OnFirstRead {}

    impl ObjectStore for OnFirstRead {
        fn put_blob(&self, blob: &Blob) -> Result<crate::object::BlobId, StoreError> {
            self.inner.put_blob(blob)
        }

        fn put_tree(&self, tree: &Tree) -> Result<crate::object::TreeId, StoreError> {
            self.inner.put_tree(tree)
        }

        fn put_commit(&self, commit: &Commit) -> Result<CommitId, StoreError> {
            self.inner.put_commit(commit)
        }

        fn get(&self, id: &ObjectId) -> Result<Option<crate::store::RawObject>, StoreError> {
            let hook = self.hook.borrow_mut().take();
            if let Some(hook) = hook {
                hook();
            }
            self.inner.get(id)
        }

        fn contains(&self, id: &ObjectId) -> Result<bool, StoreError> {
            self.inner.contains(id)
        }
    }

    /// A repository in which the branch `main` names a commit that is gone, and the working copy
    /// one that is kept. Returns the kept commit and a second commit that is gone.
    fn with_a_failing_branch(dir: &TempDir, objects: &FsObjectStore) -> (CommitId, CommitId) {
        let kept = commit(objects, "kept");
        let removed = commit(objects, "removed");
        let gone = commit(objects, "gone");
        for id in [removed, gone] {
            fs::remove_file(objects.object_path(&id.object_id())).unwrap();
        }
        write_reference(dir, "HEAD", "branch main\n");
        write_reference(dir, "working-copy", &format!("{kept}\n"));
        write_reference(dir, "refs/heads/main", &format!("{removed}\n"));
        (kept, gone)
    }

    #[test]
    fn a_pointer_that_moves_during_the_check_is_checked_at_its_new_commit() {
        // As a purge does: the branch moves to a new commit, and its old commit is removed.
        let (dir, objects, refs) = setup();
        let (kept, _) = with_a_failing_branch(&dir, &objects);
        let moved = dir.path().join("refs/heads/main");
        let store = OnFirstRead::new(dir.store(), move || {
            fs::write(moved, format!("{kept}\n")).unwrap();
        });
        assert!(refs.verify(&store).unwrap().is_empty());

        // A move to a commit that is gone as well is a failure at the new commit.
        let (dir, objects, refs) = setup();
        let (_, gone) = with_a_failing_branch(&dir, &objects);
        let moved = dir.path().join("refs/heads/main");
        let store = OnFirstRead::new(dir.store(), move || {
            fs::write(moved, format!("{gone}\n")).unwrap();
        });
        let errors = refs.verify(&store).unwrap();
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            VerifyError::Missing { at: Pointer::Branch(branch), id }
                if *branch == name("main") && *id == gone.object_id()
        ));
    }

    #[test]
    fn a_pointer_that_is_gone_after_the_check_is_no_failure() {
        let (dir, objects, refs) = setup();
        with_a_failing_branch(&dir, &objects);
        let path = dir.path().join("refs/heads/main");
        let store = OnFirstRead::new(dir.store(), move || fs::remove_file(path).unwrap());
        assert!(refs.verify(&store).unwrap().is_empty());
    }

    #[test]
    fn the_commit_of_a_change_is_looked_up_under_the_lock() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let lock = dir.path().join("refs.lock");
        let seen = std::rc::Rc::new(std::cell::Cell::new(false));
        let hook_seen = std::rc::Rc::clone(&seen);
        let hook_lock = lock.clone();
        let store = OnFirstRead::new(dir.store(), move || hook_seen.set(hook_lock.exists()));
        refs.update_branch(&name("main"), None, id, &store, &Allowed::granted())
            .unwrap();
        assert!(
            seen.get(),
            "update_branch looked up the commit without the lock"
        );

        let seen = std::rc::Rc::new(std::cell::Cell::new(false));
        let hook_seen = std::rc::Rc::clone(&seen);
        let store = OnFirstRead::new(dir.store(), move || hook_seen.set(lock.exists()));
        refs.set_head(&Head::Detached(id), &store, &Allowed::granted())
            .unwrap();
        assert!(seen.get(), "set_head looked up the commit without the lock");
    }

    /// Restores `targets` and asserts that the moves were made.
    fn restore(refs: &FsRefStore, objects: &FsObjectStore, targets: &[(&str, Option<CommitId>)]) {
        let branches: Vec<(BranchName, Option<CommitId>)> = targets
            .iter()
            .map(|(branch, commit)| (name(branch), *commit))
            .collect();
        let targets = RestoreTargets {
            head: None,
            working_copy: None,
            branches: &branches,
        };
        let outcome = refs
            .restore_after(&writer(refs), &targets, objects, |_| Ok::<_, &str>(()))
            .unwrap();
        assert!(matches!(outcome, Logged::Moved(())), "{outcome:?}");
    }

    #[test]
    fn a_restore_deletes_a_branch_that_is_in_the_way_of_one_it_creates_and_the_other_way_round() {
        let (_dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        // `a` goes, `a/b` comes: the file of `a` holds the place of the directory.
        refs.update_branch(&name("a"), None, id, &objects, &Allowed::granted())
            .unwrap();
        restore(&refs, &objects, &[("a", None), ("a/b", Some(id))]);
        assert_eq!(refs.branch(&name("a")).unwrap(), None);
        assert_eq!(refs.branch(&name("a/b")).unwrap(), Some(id));
        // `a/b` goes, `a` comes, and the list names `a` first.
        restore(&refs, &objects, &[("a", Some(id)), ("a/b", None)]);
        assert_eq!(refs.branch(&name("a")).unwrap(), Some(id));
        assert_eq!(refs.branch(&name("a/b")).unwrap(), None);
    }

    #[test]
    fn a_restore_clears_what_the_deleted_branches_leave_in_the_directory() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("a/b"), None, id, &objects, &Allowed::granted())
            .unwrap();
        // The listing of the branches skips a dot file and an empty directory, so the restore
        // may create `a`; a plain removal of the parents of `a/b` would leave both behind.
        fs::write(dir.path().join("refs/heads/a/.DS_Store"), b"x").unwrap();
        fs::create_dir_all(dir.path().join("refs/heads/a/x")).unwrap();
        restore(&refs, &objects, &[("a", Some(id)), ("a/b", None)]);
        assert!(dir.path().join("refs/heads/a").is_file());
        assert_eq!(refs.branch(&name("a")).unwrap(), Some(id));
    }

    #[test]
    fn a_restore_does_not_take_a_sibling_for_a_branch_below_the_name() {
        let (_dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("a/b"), None, id, &objects, &Allowed::granted())
            .unwrap();
        refs.update_branch(&name("ab"), None, id, &objects, &Allowed::granted())
            .unwrap();
        // `ab` starts with `a` but is not below it.
        restore(&refs, &objects, &[("a", Some(id)), ("a/b", None)]);
        assert_eq!(refs.branch(&name("a")).unwrap(), Some(id));
        assert_eq!(refs.branch(&name("ab")).unwrap(), Some(id));
    }

    #[test]
    fn a_restore_refuses_a_name_that_differs_in_case_from_a_branch_it_deletes() {
        let (_dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("Foo"), None, id, &objects, &Allowed::granted())
            .unwrap();
        let branches = [(name("foo"), Some(id)), (name("Foo"), None)];
        let targets = RestoreTargets {
            head: None,
            working_copy: None,
            branches: &branches,
        };
        let error = refs
            .restore_after(&writer(&refs), &targets, &objects, |_| Ok::<_, &str>(()))
            .unwrap_err();
        assert!(
            matches!(
                &error,
                RefError::Conflict {
                    kind: ConflictKind::CaseVariant,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(refs.branch(&name("Foo")).unwrap(), Some(id));
    }

    #[test]
    fn a_restore_still_refuses_a_name_that_a_branch_it_keeps_holds() {
        let (_dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("a/b"), None, id, &objects, &Allowed::granted())
            .unwrap();
        refs.update_branch(&name("a/c"), None, id, &objects, &Allowed::granted())
            .unwrap();
        // `a/b` goes, but `a/c` stays below `a`.
        let branches = [(name("a"), Some(id)), (name("a/b"), None)];
        let targets = RestoreTargets {
            head: None,
            working_copy: None,
            branches: &branches,
        };
        let error = refs
            .restore_after(&writer(&refs), &targets, &objects, |_| Ok::<_, &str>(()))
            .unwrap_err();
        assert!(
            matches!(
                &error,
                RefError::Conflict {
                    kind: ConflictKind::Directory,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(refs.branch(&name("a/b")).unwrap(), Some(id));
    }

    #[test]
    fn a_purge_that_starts_during_the_check_refuses_it() {
        let (dir, objects, refs) = setup();
        with_a_failing_branch(&dir, &objects);
        let journal = dir.path().join(PURGE_JOURNAL);
        let store = OnFirstRead::new(dir.store(), move || fs::write(journal, b"").unwrap());
        assert!(matches!(
            refs.verify(&store),
            Err(RefError::PurgePending { .. })
        ));
    }

    #[test]
    fn a_pending_purge_refuses_the_check() {
        // Before any reference is read: there is no HEAD, and the journal is what is reported.
        let (dir, objects, refs) = setup();
        fs::write(dir.path().join(PURGE_JOURNAL), b"").unwrap();
        assert!(matches!(
            refs.verify(&objects),
            Err(RefError::PurgePending { .. })
        ));
    }

    #[test]
    fn a_reference_that_cannot_be_read_stops_the_check() {
        let (dir, objects, refs) = setup();
        let kept = commit(&objects, "kept");
        assert!(matches!(
            refs.verify(&objects),
            Err(RefError::NoHead { .. })
        ));
        write_reference(&dir, "HEAD", "branch main\n");
        assert!(matches!(
            refs.verify(&objects),
            Err(RefError::NoWorkingCopy { .. })
        ));
        write_reference(&dir, "working-copy", &format!("{kept}\n"));
        write_reference(&dir, "refs/heads/main", &kept.to_string());
        assert!(matches!(
            refs.verify(&objects),
            Err(RefError::Corrupt { .. })
        ));
        // The error of the read wins over a commit that is missing.
        let removed = commit(&objects, "removed");
        fs::remove_file(objects.object_path(&removed.object_id())).unwrap();
        write_reference(&dir, "HEAD", &format!("commit {removed}\n"));
        assert!(matches!(
            refs.verify(&objects),
            Err(RefError::Corrupt { .. })
        ));
    }
}
