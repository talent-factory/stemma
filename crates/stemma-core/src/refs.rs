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
//! <root>/refs.lock               exists while a reference is changed
//! ```
//!
//! A [`BranchName`] is valid by construction, as an [`Actor`](crate::Actor) is. A branch that has
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

use crate::object::{CommitId, ObjectId, ObjectIdParseError};
use crate::store::{
    create_dirs_synced, create_temp, sync_directory, unique_temp_name, ObjectStore, StoreError,
};

/// The longest branch name in bytes. A file name is limited by the file system, and a path by the
/// platform; this stays well below both.
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BranchName(String);

impl BranchName {
    /// Checks the name.
    ///
    /// # Errors
    ///
    /// Returns [`BranchNameError`] with the first rule that the name violates.
    pub fn new(name: impl Into<String>) -> Result<Self, BranchNameError> {
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
        Self::new(name)
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
}

impl fmt::Display for RefMalformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CutOff => "the file is empty or has no terminating newline",
            Self::ExtraLines => "the file has more than one line",
            Self::HeadShape => "expected `branch <name>` or `commit <id>`",
            Self::UnknownHeadKind => "the kind is neither `branch` nor `commit`",
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
    /// The name of the branch in HEAD is rejected.
    InvalidBranchName(BranchNameError),
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
        }
    }
}

impl Error for RefParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message has the message of the cause; see the crate documentation.
            Self::InvalidObjectId(source) => source.source(),
            Self::InvalidBranchName(source) => source.source(),
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
    /// branch name that [`BranchName::new`] rejects, or an object ID that is not canonical.
    pub fn parse(bytes: &[u8]) -> Result<Self, RefParseError> {
        let line = single_line(bytes)?;
        let (kind, value) = line
            .split_once(' ')
            .ok_or(RefParseError::Malformed(RefMalformed::HeadShape))?;
        match kind {
            "branch" => BranchName::new(value)
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
        /// The commit the caller expected, or `None` for a branch that does not exist.
        expected: Option<CommitId>,
        /// The commit the branch has, or `None` if it does not exist.
        found: Option<CommitId>,
    },
    /// The name cannot be created because of another branch: a branch is a file, so a name cannot
    /// be a branch and the start of other names, and on a file system that ignores case two names
    /// that differ only in case are one file.
    Conflict {
        /// The name that was to be created.
        name: BranchName,
        /// The existing branch, or directory of branches, that is in the way.
        with: String,
    },
    /// The object store has no commit with this ID.
    MissingCommit(CommitId),
    /// The object store failed, or the object is not a commit.
    Store(Box<StoreError>),
    /// A lock file exists: another process changes references, or a process that did ended
    /// without removing it.
    Locked {
        /// The lock file.
        path: PathBuf,
    },
    /// There is no HEAD.
    NoHead {
        /// The file that does not exist.
        path: PathBuf,
    },
}

fn describe(commit: &Option<CommitId>) -> String {
    commit
        .as_ref()
        .map_or_else(|| "does not exist".to_owned(), |id| format!("is at {id}"))
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
                "branch `{name}` has moved: it was expected to exist at {}, but it {}",
                expected
                    .as_ref()
                    .map_or_else(|| "nothing (a new branch)".to_owned(), ToString::to_string),
                describe(found)
            ),
            Self::Conflict { name, with } => write!(
                f,
                "branch `{name}` conflicts with `{}`",
                with.escape_debug()
            ),
            Self::MissingCommit(id) => write!(f, "the store has no commit {id}"),
            Self::Store(source) => write!(f, "{source}"),
            Self::Locked { path } => write!(
                f,
                "the references are locked by `{}`; if no process changes them, the file is left \
                 from a crash and can be removed",
                path.display()
            ),
            Self::NoHead { path } => write!(f, "there is no HEAD at `{}`", path.display()),
        }
    }
}

impl RefError {
    /// Returns the error of the file system, if that is what failed. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } => Some(source),
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
            Self::Io { source, .. } => source.source(),
            Self::Corrupt { source, .. } => source.source(),
            Self::Store(source) => source.source(),
            Self::Stale { .. }
            | Self::Conflict { .. }
            | Self::MissingCommit(_)
            | Self::Locked { .. }
            | Self::NoHead { .. } => None,
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
    /// `objects`.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::MissingCommit`] if the commit is not in the store, [`RefError::Store`]
    /// if the store fails or the object is not a commit, [`RefError::Locked`] if another change is
    /// in progress, and [`RefError::Io`] if the storage fails.
    fn set_head(&self, head: &Head, objects: &dyn ObjectStore) -> Result<(), RefError>;

    /// Returns the commit of a branch, or `None` if the branch does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Corrupt`] if the file is not in canonical form and [`RefError::Io`] if
    /// the storage fails.
    fn branch(&self, name: &BranchName) -> Result<Option<CommitId>, RefError>;

    /// Returns all branches with their commits, sorted by name.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Corrupt`] for a file that is not a valid reference, also one whose name
    /// is not a valid branch name, and [`RefError::Io`] if the storage fails.
    fn branches(&self) -> Result<Vec<(BranchName, CommitId)>, RefError>;

    /// Moves a branch to `new`, or creates it, if it is still at `expected` (`None` for a branch
    /// that does not exist). The commit has to be in `objects`.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::MissingCommit`] if `new` is not in the store, [`RefError::Store`] if
    /// the store fails or `new` is not a commit, [`RefError::Locked`] if another change is in
    /// progress, [`RefError::Stale`] if the branch is not at `expected`, [`RefError::Conflict`] if
    /// a new name is in the way of another, and [`RefError::Corrupt`] and [`RefError::Io`] as for
    /// [`branch`](Self::branch). Nothing is changed in any of these cases.
    fn update_branch(
        &self,
        name: &BranchName,
        expected: Option<CommitId>,
        new: CommitId,
        objects: &dyn ObjectStore,
    ) -> Result<(), RefError>;

    /// Returns the commit that HEAD leads to: the commit of its branch, `None` if that branch does
    /// not exist yet, or the commit that it names.
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

/// The lock file while it exists. It is removed when the value is dropped.
struct Lock {
    path: PathBuf,
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Best effort: a lock that stays behind is reported by the next change.
        let _ = fs::remove_file(&self.path);
    }
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

    pub(crate) fn branch_path(&self, name: &BranchName) -> PathBuf {
        name.as_str()
            .split('/')
            .fold(self.heads_dir(), |path, component| path.join(component))
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join("refs.lock")
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

    /// Reads a file that is a regular file, or returns `None` if there is none. A directory is no
    /// reference either; so is a link, which is not followed.
    fn read_reference(&self, path: &Path) -> Result<Option<Vec<u8>>, RefError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => fs::read(path)
                .map(Some)
                .map_err(io_error("read the reference", path)),
            Ok(metadata) if metadata.is_dir() => Ok(None),
            Ok(_) => Err(RefError::Io {
                operation: "read the reference",
                path: path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
            }),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(io_error("inspect the reference", path)(error)),
        }
    }

    fn lock(&self) -> Result<Lock, RefError> {
        let path = self.lock_path();
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => Ok(Lock { path }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Err(RefError::Locked { path })
            }
            Err(error) => Err(io_error("create the lock file", &path)(error)),
        }
    }

    /// Replaces the file with the content, atomically: the content goes to a temporary file, is
    /// flushed, and is renamed over the file. The file is not made read-only, because Windows
    /// refuses to replace one.
    fn write_replacing(&self, path: &Path, content: &[u8]) -> Result<(), RefError> {
        let temp_dir = self.root.join("tmp");
        let parent = path.parent().ok_or_else(|| RefError::Io {
            operation: "find the directory of",
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "the path has no parent"),
        })?;
        create_dirs_synced(&self.root, &temp_dir).map_err(from_store)?;
        create_dirs_synced(&self.root, parent).map_err(from_store)?;
        let (mut file, mut temp) =
            create_temp(&temp_dir, &mut unique_temp_name).map_err(from_store)?;
        file.write_all(content)
            .map_err(io_error("write the temporary file", &temp.path))?;
        file.sync_all()
            .map_err(io_error("flush the temporary file", &temp.path))?;
        drop(file);
        fs::rename(&temp.path, path).map_err(io_error("move the reference into place at", path))?;
        temp.armed = false;
        sync_directory(parent).map_err(from_store)
    }

    /// Finds out what the name is in the file system, comparing every component **exactly**. On a
    /// file system that ignores case, `main` would open the file of `Main`; the listing of each
    /// directory shows the case that is stored, so such a name is found to be in conflict and not
    /// to be the other branch.
    fn lookup(&self, name: &BranchName) -> Result<Lookup, RefError> {
        let components: Vec<&str> = name.as_str().split('/').collect();
        let mut dir = self.heads_dir();
        let mut relative = String::new();
        for (index, component) in components.iter().enumerate() {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(Lookup::Absent)
                }
                Err(error) => return Err(io_error("list the branches in", &dir)(error)),
            };
            let (mut exact, mut variant) = (false, None);
            for entry in entries {
                let entry = entry.map_err(io_error("list the branches in", &dir))?;
                let found = entry.file_name().to_string_lossy().into_owned();
                if found == *component {
                    exact = true;
                } else if found.eq_ignore_ascii_case(component) && variant.is_none() {
                    variant = Some(join_name(&relative, &found));
                }
            }
            if !exact {
                return Ok(variant.map_or(Lookup::Absent, Lookup::Conflict));
            }
            let here = dir.join(component);
            let current = join_name(&relative, component);
            let last = index + 1 == components.len();
            match fs::symlink_metadata(&here) {
                Ok(metadata) if metadata.is_dir() => {
                    if last {
                        // A directory of branches holds the name.
                        return Ok(Lookup::Conflict(current));
                    }
                }
                Ok(_) if last => return Ok(Lookup::Branch(here)),
                // A file holds the place of a directory in the name.
                Ok(_) => return Ok(Lookup::Conflict(current)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Lookup::Absent),
                Err(error) => return Err(io_error("inspect the branch", &here)(error)),
            }
            dir = here;
            relative = current;
        }
        Ok(Lookup::Absent)
    }

    /// The commit of the branch, and what stands in the way of creating it if there is none.
    fn current(&self, name: &BranchName) -> Result<(Option<CommitId>, Option<String>), RefError> {
        match self.lookup(name)? {
            Lookup::Absent => Ok((None, None)),
            Lookup::Conflict(with) => Ok((None, Some(with))),
            Lookup::Branch(path) => {
                let bytes = self.read_reference(&path)?.ok_or_else(|| RefError::Io {
                    operation: "read the reference",
                    path: path.clone(),
                    source: io::Error::new(io::ErrorKind::NotFound, "it is gone"),
                })?;
                parse_branch(&bytes)
                    .map(|commit| (Some(commit), None))
                    .map_err(|source| RefError::Corrupt { path, source })
            }
        }
    }

    /// Fails unless the object store has the commit.
    fn require_commit(commit: &CommitId, objects: &dyn ObjectStore) -> Result<(), RefError> {
        match objects.resolve_commit(&commit.object_id()) {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(RefError::MissingCommit(*commit)),
            Err(error) => Err(RefError::Store(Box::new(error))),
        }
    }
}

/// What a branch name is in the file system.
enum Lookup {
    /// Nothing is there, and nothing is in the way.
    Absent,
    /// The file of the branch.
    Branch(PathBuf),
    /// The name cannot be a branch: another branch, or a directory of branches, is in the way, or
    /// a name that differs only in case. Carries that name.
    Conflict(String),
}

fn join_name(prefix: &str, component: &str) -> String {
    if prefix.is_empty() {
        component.to_owned()
    } else {
        format!("{prefix}/{component}")
    }
}

impl crate::store::sealed::Sealed for FsRefStore {}

impl RefStore for FsRefStore {
    fn head(&self) -> Result<Head, RefError> {
        self.require_root()?;
        let path = self.head_path();
        let bytes = self
            .read_reference(&path)?
            .ok_or_else(|| RefError::NoHead { path: path.clone() })?;
        Head::parse(&bytes).map_err(|source| RefError::Corrupt { path, source })
    }

    fn set_head(&self, head: &Head, objects: &dyn ObjectStore) -> Result<(), RefError> {
        self.require_root()?;
        if let Head::Detached(commit) = head {
            Self::require_commit(commit, objects)?;
        }
        let _lock = self.lock()?;
        self.write_replacing(&self.head_path(), &head.encode())
    }

    fn branch(&self, name: &BranchName) -> Result<Option<CommitId>, RefError> {
        self.require_root()?;
        Ok(self.current(name)?.0)
    }

    fn branches(&self) -> Result<Vec<(BranchName, CommitId)>, RefError> {
        self.require_root()?;
        let heads = self.heads_dir();
        let mut found = Vec::new();
        let mut pending = vec![(heads.clone(), String::new())];
        while let Some((dir, prefix)) = pending.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound && dir == heads => {
                    return Ok(found)
                }
                Err(error) => return Err(io_error("list the branches in", &dir)(error)),
            };
            for entry in entries {
                let entry = entry.map_err(io_error("list the branches in", &dir))?;
                let path = entry.path();
                let name = join_name(&prefix, &entry.file_name().to_string_lossy());
                let kind = entry
                    .file_type()
                    .map_err(io_error("inspect the branch", &path))?;
                if kind.is_dir() {
                    pending.push((path, name));
                    continue;
                }
                let branch = BranchName::new(name).map_err(|error| RefError::Corrupt {
                    path: path.clone(),
                    source: RefParseError::InvalidBranchName(error),
                })?;
                let bytes = self.read_reference(&path)?.ok_or_else(|| RefError::Io {
                    operation: "read the reference",
                    path: path.clone(),
                    source: io::Error::new(io::ErrorKind::NotFound, "it is gone"),
                })?;
                let commit =
                    parse_branch(&bytes).map_err(|source| RefError::Corrupt { path, source })?;
                found.push((branch, commit));
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
    ) -> Result<(), RefError> {
        self.require_root()?;
        Self::require_commit(&new, objects)?;
        let _lock = self.lock()?;
        let (found, in_the_way) = self.current(name)?;
        if found != expected {
            return Err(RefError::Stale {
                name: name.clone(),
                expected,
                found,
            });
        }
        if let Some(with) = in_the_way {
            return Err(RefError::Conflict {
                name: name.clone(),
                with,
            });
        }
        self.write_replacing(&self.branch_path(name), &encode_branch(&new))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Blob, Commit, Tree};
    use crate::store::FsObjectStore;
    use crate::testutil::{FaultyStore, TempDir, DISK_MESSAGE};
    use crate::Actor;

    const ID_A: &str = "b3:4a0b6603381c7c38c557e9ceb3cba8876f0a7863146e7309801352e80b073b65";

    fn name(text: &str) -> BranchName {
        BranchName::new(text).unwrap()
    }

    /// A commit in the store; the message makes it different from the others.
    fn commit(objects: &FsObjectStore, message: &str) -> CommitId {
        let tree = objects.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
        let commit = Commit::new(tree, vec![], Actor::human("daniel").unwrap(), 0, message);
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
            assert_eq!(BranchName::new(text).unwrap().as_str(), text, "{text}");
            assert_eq!(text.parse::<BranchName>().unwrap().to_string(), text);
        }
        let longest = "a".repeat(MAX_BRANCH_NAME_LEN);
        assert!(BranchName::new(longest).is_ok());
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
            let error = BranchName::new(text.clone()).unwrap_err();
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
        let error = BranchName::new("a\nb c").unwrap_err();
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
        refs.update_branch(&main, None, first, &objects).unwrap();
        assert_eq!(refs.branch(&main).unwrap(), Some(first));
        // The file is exactly the ID and a newline, and no temporary file stays behind.
        let file = dir.path().join("refs/heads/main");
        assert_eq!(fs::read(&file).unwrap(), format!("{first}\n").into_bytes());
        assert_eq!(files_in(&dir.path().join("tmp")), 0);
        assert!(!dir.path().join("refs.lock").exists());
        let second = commit(&objects, "second");
        refs.update_branch(&main, Some(first), second, &objects)
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
        refs.update_branch(&main, None, first, &objects).unwrap();
        for expected in [None, Some(second)] {
            match refs
                .update_branch(&main, expected, third, &objects)
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
            .update_branch(&name("other"), Some(first), second, &objects)
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
    fn a_commit_that_the_store_does_not_have_or_that_is_not_a_commit_is_refused() {
        let (dir, objects, refs) = setup();
        let main = name("main");
        let absent = Commit::new(
            objects.put_tree(&Tree::new(vec![]).unwrap()).unwrap(),
            vec![],
            Actor::human("daniel").unwrap(),
            0,
            "never stored",
        )
        .unwrap()
        .id();
        let error = refs
            .update_branch(&main, None, absent, &objects)
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
            .update_branch(&main, None, blob_as_commit, &objects)
            .unwrap_err();
        assert!(
            matches!(error.store_error(), Some(StoreError::KindMismatch { .. })),
            "{error}"
        );
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
            refs.update_branch(&name(text), None, id, &objects).unwrap();
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
        refs.update_branch(&name("a"), None, id, &objects).unwrap();
        let error = refs
            .update_branch(&name("a/b"), None, id, &objects)
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Conflict { with, .. } if with == "a"),
            "{error}"
        );
        assert_eq!(error.to_string(), "branch `a/b` conflicts with `a`");
        refs.update_branch(&name("x/y"), None, id, &objects)
            .unwrap();
        let error = refs
            .update_branch(&name("x"), None, id, &objects)
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Conflict { with, .. } if with == "x"),
            "{error}"
        );
        let error = refs
            .update_branch(&name("x/y/z"), None, id, &objects)
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Conflict { with, .. } if with == "x/y"),
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
        refs.update_branch(&name("Main"), None, id, &objects)
            .unwrap();
        let error = refs
            .update_branch(&name("main"), None, id, &objects)
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Conflict { with, .. } if with == "Main"),
            "{error}"
        );
        refs.update_branch(&name("Feat/Login"), None, id, &objects)
            .unwrap();
        let error = refs
            .update_branch(&name("feat/login"), None, id, &objects)
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Conflict { with, .. } if with == "Feat"),
            "{error}"
        );
        let error = refs
            .update_branch(&name("Feat/login"), None, id, &objects)
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Conflict { with, .. } if with == "Feat/Login"),
            "{error}"
        );
        // Moving an existing branch is not a creation and needs no such check.
        let next = commit(&objects, "next");
        refs.update_branch(&name("Main"), Some(id), next, &objects)
            .unwrap();
    }

    #[test]
    fn a_lock_that_exists_stops_every_change_and_is_not_removed() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        let lock = dir.path().join("refs.lock");
        fs::write(&lock, b"").unwrap();
        let error = refs
            .update_branch(&name("main"), None, id, &objects)
            .unwrap_err();
        assert!(
            matches!(&error, RefError::Locked { path } if *path == lock),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .starts_with("the references are locked by `"),
            "{error}"
        );
        let error = refs
            .set_head(&Head::Branch(name("main")), &objects)
            .unwrap_err();
        assert!(matches!(error, RefError::Locked { .. }));
        assert!(
            lock.exists(),
            "a lock of another process is not ours to remove"
        );
        assert_eq!(refs.branch(&name("main")).unwrap(), None);
        fs::remove_file(&lock).unwrap();
        refs.update_branch(&name("main"), None, id, &objects)
            .unwrap();
    }

    #[test]
    fn a_file_that_is_not_canonical_is_corrupt_with_its_path() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.update_branch(&name("main"), None, id, &objects)
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
            refs.update_branch(&name("main"), Some(id), id, &objects),
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
        refs.set_head(&Head::Branch(name("main")), &objects)
            .unwrap();
        assert_eq!(refs.head().unwrap(), Head::Branch(name("main")));
        assert_eq!(fs::read(dir.path().join("HEAD")).unwrap(), b"branch main\n");
        assert_eq!(refs.resolve_head().unwrap(), None);
        let id = commit(&objects, "c");
        refs.update_branch(&name("main"), None, id, &objects)
            .unwrap();
        assert_eq!(refs.resolve_head().unwrap(), Some(id));
        refs.set_head(&Head::Branch(name("other")), &objects)
            .unwrap();
        assert_eq!(refs.resolve_head().unwrap(), None);
        assert_eq!(files_in(&dir.path().join("tmp")), 0);
        assert!(!dir.path().join("refs.lock").exists());
    }

    #[test]
    fn a_detached_head_needs_a_commit_in_the_store() {
        let (dir, objects, refs) = setup();
        let id = commit(&objects, "c");
        refs.set_head(&Head::Detached(id), &objects).unwrap();
        assert_eq!(refs.head().unwrap(), Head::Detached(id));
        assert_eq!(
            fs::read(dir.path().join("HEAD")).unwrap(),
            format!("commit {id}\n").into_bytes()
        );
        assert_eq!(refs.resolve_head().unwrap(), Some(id));
        let absent = CommitId::new(ID_A.parse().unwrap());
        let error = refs
            .set_head(&Head::Detached(absent), &objects)
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
            refs.update_branch(&name("main"), None, id, &objects)
                .unwrap_err(),
            refs.set_head(&Head::Branch(name("main")), &objects)
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
        refs.update_branch(&name("a/b"), None, id, &objects)
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
                        (id, refs.update_branch(&name("main"), None, id, &objects))
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
            .update_branch(&name("main"), None, id, &faulty)
            .unwrap_err();
        assert!(matches!(error, RefError::Store(_)), "{error}");
        assert!(error.to_string().ends_with(DISK_MESSAGE), "{error}");
        assert_eq!(error.to_string().matches(DISK_MESSAGE).count(), 1);
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("the deepest cause")
        );
        assert!(error.store_error().is_some());
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
            format!(
                "branch `main` has moved: it was expected to exist at {a}, but it does not exist"
            )
        );
        let created = RefError::Stale {
            name: name("main"),
            expected: None,
            found: Some(a),
        };
        assert_eq!(
            created.to_string(),
            format!("branch `main` has moved: it was expected to exist at nothing (a new branch), but it is at {a}")
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
}
