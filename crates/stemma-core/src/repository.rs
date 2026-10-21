//! A repository: the metadata directory `.stemma/` with its object store and references
//! (ADR-009).
//!
//! The layout of `.stemma/` is:
//!
//! ```text
//! .stemma/
//! ├── format          "stemma-repository 2" and a newline
//! ├── HEAD            "branch <name>" and a newline (the branch may not exist yet)
//! ├── working-copy    the ID of the working-copy commit and a newline (ADR-010)
//! ├── objects/        loose objects (ADR-007)
//! ├── refs/heads/     branches (ADR-008)
//! ├── oplog/          the operation log: one numbered file per entry (ADR-011)
//! ├── oplog-tip       the number of the last entry of the log (ADR-011)
//! └── tmp/            temporary files of the object store, the references, and the log
//! ```

use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use std::time::{SystemTime, UNIX_EPOCH};

mod checkout;
mod commit;
mod restore;
mod snapshot;
mod status;
mod track;

pub use checkout::{CheckoutError, CheckoutPlan, PlannedWrite};
pub use commit::{CommitError, Committed};
pub use restore::{RestoreError, RestorePlan, Restored};
pub use snapshot::{SnapshotError, Unsupported};
pub use status::{FileChange, FileChangeKind, Saved, Status, StatusError};
pub use track::{normalize_path, TrackError, Tracking};

use crate::change::{ChangeId, GenerateError};
use crate::object::{Commit, CommitId, Tree, METADATA_DIR};
use crate::oplog::{Change, Entry, FsOpLog, Moved, OpLogError, OperationName};
use crate::refs::{encode_branch, BranchName, FsRefStore, Head, Logged, RefError, RefStore};
use crate::store::{unique_temp_name, FsObjectStore, ObjectStore, StoreError};
use crate::{Actor, Authorizer, Decision};

/// The version of the repository format that this code writes and reads.
const FORMAT_VERSION: u32 = 2;

/// The name that the file `format` starts with, followed by the version.
const FORMAT_PREFIX: &str = "stemma-repository ";

/// The longest `format` file that is read. A longer file is not in canonical form anyway.
const FORMAT_MAX_LEN: u64 = 64;

/// How many names are tried for the temporary directory of `init` before it gives up.
const STAGING_ATTEMPTS: usize = 16;

/// The content of the file `format`: the prefix, the version, and a newline.
fn format_content() -> String {
    format!("{FORMAT_PREFIX}{FORMAT_VERSION}\n")
}

/// Reads the file `format` and returns the version as the decimal digits that it holds. It
/// accepts what [`format_content`] writes for any version, however large, and nothing else: the
/// prefix, decimal digits without a leading zero, and a newline. Returns `None` for any other
/// input. The version stays text so that a number too large for any integer is still reported
/// as an unsupported version and not as a damaged file.
fn parse_format(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let digits = text.strip_prefix(FORMAT_PREFIX)?.strip_suffix('\n')?;
    let canonical = !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && (digits.len() == 1 || !digits.starts_with('0'));
    canonical.then_some(digits)
}

/// What [`Repository::init`] needs besides the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOptions {
    initial_branch: BranchName,
}

impl InitOptions {
    /// Options for a repository whose HEAD points to `initial_branch`. The branch itself does not
    /// exist until the first commit.
    pub fn new(initial_branch: BranchName) -> Self {
        Self { initial_branch }
    }

    /// The branch that HEAD points to in a new repository.
    pub fn initial_branch(&self) -> &BranchName {
        &self.initial_branch
    }
}

impl Default for InitOptions {
    /// The initial branch is `main`.
    fn default() -> Self {
        Self::new(BranchName::new("main").expect("`main` is a valid branch name"))
    }
}

/// An open repository: the directory that contains `.stemma/`, with its object store, its
/// references, and its operation log.
///
/// The value holds paths only and no handle, so a clone is as good as the original. That is a
/// promise of this type: a field that makes a clone wrong, such as an open lock, has to remove
/// the `Clone` first.
#[derive(Debug, Clone)]
pub struct Repository {
    path: PathBuf,
    objects: FsObjectStore,
    refs: FsRefStore,
    oplog: FsOpLog,
}

impl Repository {
    /// Creates a repository in `path`. Missing directories on the way are created.
    ///
    /// The call asks `authorizer` first (ADR-002); the operation is `init` and the target is the
    /// directory. If `.stemma` exists in `path`, even as an empty directory or as a link, the call
    /// fails before it creates anything but missing parent directories.
    ///
    /// The metadata directory is built under a temporary name next to its final place,
    /// `.stemma-init-<pid>-<n>-<random>`, and moved into place with one rename. A failure
    /// removes the temporary directory again, so it leaves no half-made `.stemma`. If that
    /// removal fails too, the error says so and names the directory, and a process that is
    /// killed in the middle leaves it behind; it can be removed by hand. HEAD points to the
    /// initial branch of `options`, which does not exist yet.
    ///
    /// The check for an existing `.stemma` and the rename are two steps. If another process
    /// creates `.stemma` in between, the rename fails and the call returns
    /// [`InitError::AlreadyExists`]; a repository that the other process built is never changed.
    /// On Unix, an *empty* `.stemma` that appears in that window is replaced, because `rename`
    /// replaces an empty directory.
    ///
    /// The first entry of the operation log is written here: operation `init`, HEAD from nothing to
    /// the initial branch, and the actor. It is the only place where an entry is written without the
    /// lock of the references, because no other writer can see the directory yet.
    ///
    /// Everything that is created is flushed to disk, including missing parent directories.
    /// Platforms that cannot flush a directory give the atomicity but not the durability.
    ///
    /// # Errors
    ///
    /// Returns [`InitError::Denied`] if the authorizer refuses, [`InitError::AlreadyExists`] if
    /// `.stemma` exists in `path` (nothing is changed), [`InitError::Io`] if the storage fails
    /// (the temporary directory is removed), [`InitError::Log`] if the first entry could not be
    /// written (among others an actor that is too long for an entry), [`InitError::Leftover`] if a
    /// failure is followed by one to remove the temporary directory, and [`InitError::NotDurable`]
    /// if the repository is complete but the final flush failed. After `NotDurable`,
    /// [`Repository::open`] works.
    pub fn init(
        path: impl AsRef<Path>,
        options: &InitOptions,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Self, InitError> {
        let path = path.as_ref();
        let target = path.join(METADATA_DIR);
        if authorizer.authorize(actor, "init", &path.display().to_string()) == Decision::Deny {
            return Err(InitError::Denied {
                path: path.to_path_buf(),
            });
        }
        create_dirs_synced(path)?;
        if exists(&target).map_err(InitError::io("inspect", &target))? {
            return Err(InitError::AlreadyExists { path: target });
        }
        let staging = create_staging(path, &mut || {
            format!("{METADATA_DIR}-init-{}", unique_temp_name())
        })?;
        install(&staging, &target, options, actor)?;
        sync_directory(path).map_err(|source| InitError::NotDurable {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self::at(path))
    }

    /// Opens the repository in `path`, the directory that contains `.stemma/`. It checks the file
    /// `format` before it returns, which the stores do not do themselves (ADR-007). It does not
    /// check `HEAD`, `objects/`, or `refs/`; the stores report what is wrong with them when they
    /// are used.
    ///
    /// # Errors
    ///
    /// Returns [`OpenError::NotARepository`] if there is no `.stemma`,
    /// [`OpenError::NotADirectory`] if `.stemma` is a file or a link that leads nowhere,
    /// [`OpenError::MissingFormat`], [`OpenError::MalformedFormat`], or
    /// [`OpenError::UnsupportedFormat`] if the file `format` is missing, not in canonical form, or
    /// of another version, and [`OpenError::Io`] if the storage fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OpenError> {
        let path = path.as_ref();
        let metadata_dir = path.join(METADATA_DIR);
        match fs::metadata(&metadata_dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(OpenError::NotADirectory { path: metadata_dir }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A link that leads nowhere is not "no `.stemma`": something is in the way.
                return Err(match fs::symlink_metadata(&metadata_dir) {
                    Ok(_) => OpenError::NotADirectory { path: metadata_dir },
                    Err(_) => OpenError::NotARepository {
                        path: path.to_path_buf(),
                    },
                });
            }
            // `path` is not a directory, so nothing can be inside it.
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
                return Err(OpenError::NotARepository {
                    path: path.to_path_buf(),
                });
            }
            Err(error) => return Err(OpenError::io("inspect", &metadata_dir)(error)),
        }
        let format_path = metadata_dir.join("format");
        let bytes = read_format(&format_path)?;
        match parse_format(&bytes) {
            Some(version) if version == FORMAT_VERSION.to_string() => Ok(Self::at(path)),
            Some(found) => Err(OpenError::UnsupportedFormat {
                path: format_path,
                found: found.to_owned(),
            }),
            None => Err(OpenError::MalformedFormat { path: format_path }),
        }
    }

    fn at(path: &Path) -> Self {
        let metadata_dir = path.join(METADATA_DIR);
        Self {
            path: path.to_path_buf(),
            objects: FsObjectStore::new(&metadata_dir),
            refs: FsRefStore::new(&metadata_dir),
            oplog: FsOpLog::new(metadata_dir),
        }
    }

    /// The directory that contains `.stemma/`, as it was given to [`init`](Self::init) or
    /// [`open`](Self::open). A relative path stays relative, so it follows a change of the
    /// working directory of the process.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The metadata directory `.stemma/`.
    pub fn metadata_dir(&self) -> PathBuf {
        self.path.join(METADATA_DIR)
    }

    /// The object store of this repository.
    pub fn objects(&self) -> &FsObjectStore {
        &self.objects
    }

    /// The references of this repository, for reading. A pointer is changed through
    /// [`update_branch`](Self::update_branch) and [`set_head`](Self::set_head), which write the
    /// entry of the log first (ADR-011) and use the object store of this repository. Through
    /// `Repository` no caller can therefore move a pointer past the log, or pair the references of
    /// this repository with the objects of another. This is a convention of the API, not a
    /// boundary: the files are plain files, and code that builds its own store on the same
    /// directory is not stopped.
    pub fn refs(&self) -> RefsView<'_> {
        RefsView { refs: &self.refs }
    }

    /// The store of the references, for the code of this crate that moves several pointers at once.
    pub(crate) fn refs_store(&self) -> &FsRefStore {
        &self.refs
    }

    /// The operation log of this repository. It can be read; entries are added only by
    /// [`update_branch`](Self::update_branch) and [`set_head`](Self::set_head), and by `init`.
    pub fn oplog(&self) -> &FsOpLog {
        &self.oplog
    }

    /// Moves a branch to `new`, or creates it, if it is still at `expected` (`None` for a branch
    /// that does not exist), and records the move in the log.
    ///
    /// The order is the rule of ADR-011, and everything after the authorizer happens under the lock
    /// of the references: the branch is checked (a name that is in the way, a branch that is not at
    /// `expected`, a commit that is not in the object store of this repository), the entry is
    /// written and flushed, and only then does the branch move. No other change can come between
    /// the checks and the move, so a refusal, a conflict, a stale branch, or a missing commit write
    /// no entry, and an entry always describes a move that was going to be made. An entry can still
    /// name a move that did not happen: when the file system fails between the entry and the move
    /// ([`UpdateError::MoveFailed`]), and when the entry is not known to be flushed, so that the
    /// move is refused (`UpdateError::Log` with [`OpLogError::NotFlushed`]). Both carry the entry,
    /// and the pointer has not moved. The commit is looked up under the lock as well.
    ///
    /// Returns the entry, or `None` if `expected` is `Some(new)`, so that the branch is at `new`
    /// already: only a change writes an entry.
    ///
    /// # Errors
    ///
    /// Returns [`UpdateError::Denied`] if the authorizer refuses, and [`UpdateError::Refs`] for what
    /// the checks find or for a failure of the references before an entry was written, among them
    /// [`RefError::Locked`] (another change is in progress: try again). If the entry could not be
    /// written the error is [`UpdateError::Log`], and the branch has not moved; an entry that is
    /// too large (the actor or a name is too long) is such an error, and nothing is written. An
    /// entry that was written belongs to [`UpdateError::MoveFailed`], [`UpdateError::NotDurable`],
    /// and [`UpdateError::Log`] with [`OpLogError::NotFlushed`].
    pub fn update_branch(
        &self,
        operation: &OperationName,
        name: &BranchName,
        expected: Option<CommitId>,
        new: CommitId,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Option<Entry>, UpdateError> {
        self.authorize(actor, authorizer, operation, name.as_str())?;
        let outcome = self
            .refs
            .update_branch_after(name, expected, new, &self.objects, || {
                let change = Change::new(
                    now(),
                    actor.clone(),
                    operation.clone(),
                    None,
                    None,
                    vec![(name.clone(), Moved::from_to(expected, Some(new)))],
                )
                .map_err(OpLogError::Entry)?;
                self.oplog.append(&change)
            })?;
        outcome.into_result()
    }

    /// Points HEAD to a branch, which need not exist yet, or to a commit, and records the change
    /// in the log, in the order of [`update_branch`](Self::update_branch) and under the same lock.
    /// The old value in the entry is the one that the change replaces: it is read under the lock.
    ///
    /// Unlike the store of the references, which replaces HEAD whatever it was, this reads HEAD
    /// first, so a HEAD that is missing or damaged is an error and is not replaced.
    ///
    /// Returns the entry, or `None` if HEAD has the value already.
    ///
    /// # Errors
    ///
    /// Returns [`UpdateError::Denied`], [`UpdateError::Refs`] (a commit that is not in the store, an
    /// unreadable HEAD, [`RefError::Locked`]), [`UpdateError::Log`], [`UpdateError::MoveFailed`],
    /// and [`UpdateError::NotDurable`], as for [`update_branch`](Self::update_branch).
    pub fn set_head(
        &self,
        operation: &OperationName,
        head: &Head,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Option<Entry>, UpdateError> {
        self.authorize(actor, authorizer, operation, "HEAD")?;
        let outcome = self.refs.set_head_after(head, &self.objects, |old| {
            let change = Change::new(
                now(),
                actor.clone(),
                operation.clone(),
                Some(Moved::from_to(Some(old.clone()), Some(head.clone()))),
                None,
                vec![],
            )
            .map_err(OpLogError::Entry)?;
            self.oplog.append(&change)
        })?;
        outcome.into_result()
    }

    /// Moves the working-copy commit from `expected` to `new` and records the move in the log, in
    /// the order of [`update_branch`](Self::update_branch) and under the same lock: the commit has
    /// to be in the object store, the working-copy commit has to be at `expected`, the entry is
    /// written and flushed, and only then does the pointer move (ADR-010, ADR-011).
    ///
    /// Returns the entry, or `None` if `expected` is `new`: only a change writes an entry.
    ///
    /// # Errors
    ///
    /// Returns [`UpdateError::Denied`] if the authorizer refuses, [`UpdateError::Refs`] for what
    /// the checks find ([`RefError::StaleWorkingCopy`], [`RefError::NoWorkingCopy`], a commit that
    /// is not in the store, [`RefError::Locked`]), and [`UpdateError::Log`],
    /// [`UpdateError::MoveFailed`], and [`UpdateError::NotDurable`], as for
    /// [`update_branch`](Self::update_branch).
    pub fn update_working_copy(
        &self,
        operation: &OperationName,
        expected: CommitId,
        new: CommitId,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Option<Entry>, UpdateError> {
        self.authorize(actor, authorizer, operation, "working-copy")?;
        self.move_working_copy(operation, expected, new, actor)
    }

    /// The move of [`update_working_copy`](Self::update_working_copy) without the question to the
    /// authorizer, for a caller that has asked already.
    fn move_working_copy(
        &self,
        operation: &OperationName,
        expected: CommitId,
        new: CommitId,
        actor: &Actor,
    ) -> Result<Option<Entry>, UpdateError> {
        let outcome = self
            .refs
            .update_working_copy_after(expected, new, &self.objects, || {
                let change = Change::new(
                    now(),
                    actor.clone(),
                    operation.clone(),
                    None,
                    Some(Moved::from_to(Some(expected), Some(new))),
                    vec![],
                )
                .map_err(OpLogError::Entry)?;
                self.oplog.append(&change)
            })?;
        outcome.into_result()
    }

    fn authorize(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
        operation: &OperationName,
        target: &str,
    ) -> Result<(), UpdateError> {
        match authorizer.authorize(actor, operation.as_str(), target) {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(UpdateError::Denied {
                operation: operation.to_string(),
                target: target.to_owned(),
            }),
        }
    }
}

/// The references of a repository, for reading. See [`Repository::refs`].
#[derive(Debug, Clone, Copy)]
pub struct RefsView<'a> {
    refs: &'a FsRefStore,
}

impl RefsView<'_> {
    /// What HEAD points to. See [`RefStore::head`].
    ///
    /// # Errors
    ///
    /// See [`RefStore::head`].
    pub fn head(&self) -> Result<Head, RefError> {
        self.refs.head()
    }

    /// The commit of a branch, or `None` if there is no such branch. See [`RefStore::branch`].
    ///
    /// # Errors
    ///
    /// See [`RefStore::branch`].
    pub fn branch(&self, name: &BranchName) -> Result<Option<CommitId>, RefError> {
        self.refs.branch(name)
    }

    /// All branches with their commits, sorted by name. See [`RefStore::branches`].
    ///
    /// # Errors
    ///
    /// See [`RefStore::branches`].
    pub fn branches(&self) -> Result<Vec<(BranchName, CommitId)>, RefError> {
        self.refs.branches()
    }

    /// Whether a writer holds the lock of the references, so that an operation may be between its
    /// entry in the log and the move of a pointer. A lock that a crashed writer left counts as held.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Io`] if the lock file cannot be inspected.
    pub fn is_locked(&self) -> Result<bool, RefError> {
        self.refs.is_locked()
    }

    /// The working-copy commit (ADR-010). See [`FsRefStore::working_copy`].
    ///
    /// # Errors
    ///
    /// See [`FsRefStore::working_copy`].
    pub fn working_copy(&self) -> Result<CommitId, RefError> {
        self.refs.working_copy()
    }

    /// The commit that HEAD leads to. See [`RefStore::resolve_head`].
    ///
    /// # Errors
    ///
    /// See [`RefStore::resolve_head`].
    pub fn resolve_head(&self) -> Result<Option<CommitId>, RefError> {
        self.refs.resolve_head()
    }
}

/// The time now, in seconds since the Unix epoch (UTC), as an entry of the log claims it. A clock
/// before 1970 gives a negative value. A value that does not fit `i64` is clamped to its limit; the
/// time is a claim and proves nothing, and the order of the entries is the order of their numbers.
fn now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_secs()).map_or(i64::MIN, |s| -s),
    }
}

/// Reasons why [`Repository::update_branch`], [`Repository::set_head`], and the operations that
/// build on them (`snapshot`, `track`, `commit`) fail. Two questions tell them apart: is there an
/// entry in the log (`entry()`), and did a pointer move? An entry exists for
/// [`MoveFailed`](Self::MoveFailed) (no pointer moved), [`PartlyMoved`](Self::PartlyMoved) (one of
/// two moved), [`NotDurable`](Self::NotDurable) (the pointers moved), and a [`Log`](Self::Log)
/// error with [`OpLogError::NotFlushed`] (none moved). [`pointer_moved`](Self::pointer_moved)
/// answers the second question.
#[derive(Debug)]
#[non_exhaustive]
pub enum UpdateError {
    /// The authorizer refused the operation. No entry was written.
    Denied {
        /// The name of the operation.
        operation: String,
        /// The branch, or `HEAD`.
        target: String,
    },
    /// The checks refused, or the references failed before an entry was written: a name that is in
    /// the way, a branch that is not where the caller saw it, a commit that is not in the store,
    /// an unreadable HEAD, a lock that is held. No entry was written and nothing moved.
    Refs(RefError),
    /// The entry could not be written, and the pointer has not moved. If the error is
    /// [`OpLogError::NotFlushed`], the entry is in the log anyway and `entry()` returns it: it names
    /// a move that did not happen, and a second attempt writes another entry. Any other error
    /// means that no entry was written.
    Log(OpLogError),
    /// The entry was written, but the move failed. The file system failed after the checks, so the
    /// entry names a move that did not happen; the old value in it was true when it was written.
    MoveFailed {
        /// The entry that is in the log.
        entry: Box<Entry>,
        /// Why the pointer did not move.
        source: RefError,
    },
    /// The entry was written and the first of two pointers moved, but the second did not: the file
    /// system failed between the moves. `op log` shows the entry as not taken (`!`), although the
    /// first pointer did move. Do not repeat the change; read the pointers. After a `commit` that
    /// stopped here, the next `commit` finishes it.
    PartlyMoved {
        /// The entry that is in the log.
        entry: Box<Entry>,
        /// Why the second pointer did not move.
        source: RefError,
    },
    /// The entry was written and the pointer moved, but a directory could not be flushed to disk, so
    /// the change may not survive a crash. Do not repeat the change: read the pointer.
    NotDurable {
        /// The entry that is in the log.
        entry: Box<Entry>,
        /// The error of the flush.
        source: RefError,
    },
}

impl UpdateError {
    /// Returns the error of the file system, if there is one inside. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Refs(error)
            | Self::MoveFailed { source: error, .. }
            | Self::PartlyMoved { source: error, .. }
            | Self::NotDurable { source: error, .. } => error.io_error(),
            Self::Log(error) => error.io_error(),
            Self::Denied { .. } => None,
        }
    }

    /// Whether a pointer moved although the call failed. [`NotDurable`](Self::NotDurable) and
    /// [`PartlyMoved`](Self::PartlyMoved) moved one; in every other case the pointers are where they
    /// were, and the change can be repeated (with a second entry if the first one stays).
    pub fn pointer_moved(&self) -> bool {
        matches!(self, Self::NotDurable { .. } | Self::PartlyMoved { .. })
    }

    /// The entry that was written, if the error leaves one in the log.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::MoveFailed { entry, .. }
            | Self::PartlyMoved { entry, .. }
            | Self::NotDurable { entry, .. } => Some(entry),
            Self::Log(OpLogError::NotFlushed { entry, .. }) => Some(entry),
            _ => None,
        }
    }
}

impl<E: Into<UpdateError>> Logged<Entry, E> {
    /// The result of an update: the entry if the pointer moved, `None` if it had the value, and an
    /// error that says whether an entry exists otherwise.
    fn into_result(self) -> Result<Option<Entry>, UpdateError> {
        match self {
            Self::Unchanged => Ok(None),
            Self::Moved(entry) => Ok(Some(entry)),
            Self::StepFailed(error) => Err(error.into()),
            Self::MoveFailed(entry, source) => Err(UpdateError::MoveFailed {
                entry: Box::new(entry),
                source,
            }),
            Self::NotFlushed(entry, source) => Err(UpdateError::NotDurable {
                entry: Box::new(entry),
                source,
            }),
            Self::PartlyMoved(entry, source) => Err(UpdateError::PartlyMoved {
                entry: Box::new(entry),
                source,
            }),
        }
    }
}

impl From<OpLogError> for UpdateError {
    fn from(error: OpLogError) -> Self {
        Self::Log(error)
    }
}

impl From<RefError> for UpdateError {
    fn from(error: RefError) -> Self {
        Self::Refs(error)
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied { operation, target } => {
                write!(f, "not allowed to {operation} `{target}`")
            }
            Self::Refs(error) => error.fmt(f),
            Self::Log(OpLogError::NotFlushed {
                entry,
                path,
                source,
            }) => write!(
                f,
                "entry {} of the operation log was written, but flushing `{}` failed: {source}; the \
                 pointer did not move, and repeating the change writes another entry",
                entry.number(),
                path.display()
            ),
            Self::Log(error) => error.fmt(f),
            Self::MoveFailed { entry, source } => write!(
                f,
                "entry {} of the operation log was written, but the move failed: {source}; the \
                 entry names a move that did not happen",
                entry.number()
            ),
            Self::PartlyMoved { entry, source } => write!(
                f,
                "entry {} of the operation log was written and the first pointer moved, but the \
                 second did not: {source}; do not repeat the change, read the pointers",
                entry.number()
            ),
            Self::NotDurable { entry, source } => write!(
                f,
                "the change was made and written as entry {} of the operation log, but flushing \
                 failed: {source}; it may not survive a crash, and it must not be repeated",
                entry.number()
            ),
        }
    }
}

impl Error for UpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Refs(error)
            | Self::MoveFailed { source: error, .. }
            | Self::PartlyMoved { source: error, .. }
            | Self::NotDurable { source: error, .. } => error.source(),
            Self::Log(error) => error.source(),
            Self::Denied { .. } => None,
        }
    }
}

/// Reads the file `format`, at most [`FORMAT_MAX_LEN`] bytes of it.
fn read_format(path: &Path) -> Result<Vec<u8>, OpenError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(OpenError::MissingFormat {
                path: path.to_path_buf(),
            });
        }
        Err(error) => return Err(OpenError::io("open the format file", path)(error)),
    };
    let mut bytes = Vec::new();
    file.take(FORMAT_MAX_LEN + 1)
        .read_to_end(&mut bytes)
        .map_err(OpenError::io("read the format file", path))?;
    if bytes.len() as u64 > FORMAT_MAX_LEN {
        return Err(OpenError::MalformedFormat {
            path: path.to_path_buf(),
        });
    }
    Ok(bytes)
}

/// Creates `dir` and every missing directory above it, and flushes the new directory entries.
fn create_dirs_synced(dir: &Path) -> Result<(), InitError> {
    let mut missing: Vec<&Path> = Vec::new();
    let mut current = dir;
    loop {
        match fs::symlink_metadata(current) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(current);
                match current.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => current = parent,
                    _ => break,
                }
            }
            Err(error) => return Err(InitError::io("inspect", current)(error)),
        }
    }
    for created in missing.iter().rev() {
        match fs::create_dir(created) {
            Ok(()) => {}
            // Another process was faster. Whether it is a directory shows when it is used.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(InitError::io("create the directory", created)(error)),
        }
    }
    // The topmost new directory is an entry in an existing one, and every new directory holds
    // the entry of the next one.
    if let Some(topmost) = missing.last() {
        let parent = match topmost.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        sync_directory(parent).map_err(InitError::io("flush the directory", parent))?;
    }
    for created in &missing {
        sync_directory(created).map_err(InitError::io("flush the directory", created))?;
    }
    Ok(())
}

/// Creates a new directory in `dir` under a name from `next_name`. If the name is taken, it asks
/// for the next one, up to [`STAGING_ATTEMPTS`] times. A directory that exists already is never
/// touched, and the caller owns the one that is returned.
fn create_staging(dir: &Path, next_name: &mut dyn FnMut() -> String) -> Result<PathBuf, InitError> {
    let operation = "create the new directory";
    let mut last = dir.to_path_buf();
    for _ in 0..STAGING_ATTEMPTS {
        let staging = dir.join(next_name());
        match fs::create_dir(&staging) {
            Ok(()) => return Ok(staging),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last = staging,
            Err(error) => return Err(InitError::io(operation, &staging)(error)),
        }
    }
    Err(InitError::Io {
        operation,
        path: last,
        source: io::Error::new(io::ErrorKind::AlreadyExists, "every name was taken"),
    })
}

/// Fills `staging`, which this call created, and moves it to `target`. If either step fails, it
/// removes `staging`, and it reports a removal that fails as well.
fn install(
    staging: &Path,
    target: &Path,
    options: &InitOptions,
    actor: &Actor,
) -> Result<(), InitError> {
    let result = fill(staging, options, actor).and_then(|()| move_into_place(staging, target));
    match result {
        Ok(()) => Ok(()),
        Err(error) => match fs::remove_dir_all(staging) {
            Ok(()) => Err(error),
            Err(source) => Err(InitError::Leftover {
                path: staging.to_path_buf(),
                error: Box::new(error),
                source,
            }),
        },
    }
}

/// Renames `staging` to `target`. A rename that fails while `target` exists means that something
/// was created there since the check: a directory is not moved onto a non-empty one. Windows
/// replaces a file by the directory instead of failing, so a target that is not a directory is
/// refused up front.
fn move_into_place(staging: &Path, target: &Path) -> Result<(), InitError> {
    if fs::symlink_metadata(target).is_ok_and(|metadata| !metadata.is_dir()) {
        return Err(InitError::AlreadyExists {
            path: target.to_path_buf(),
        });
    }
    match fs::rename(staging, target) {
        Ok(()) => Ok(()),
        Err(error) => match exists(target) {
            Ok(true) => Err(InitError::AlreadyExists {
                path: target.to_path_buf(),
            }),
            _ => Err(InitError::io(
                "move the new directory into place at",
                target,
            )(error)),
        },
    }
}

/// Stores the first working-copy commit in the object store below `dir`: the empty tree, no
/// parents, a new change ID, and the actor of `init` as the author.
fn first_working_copy(dir: &Path, actor: &Actor) -> Result<CommitId, InitError> {
    let store = FsObjectStore::new(dir);
    let tree = store
        .put_tree(&Tree::new(Vec::new()).expect("an empty tree is valid"))
        .map_err(|error| InitError::Store(Box::new(error)))?;
    let change = ChangeId::generate().map_err(InitError::ChangeId)?;
    let commit = Commit::new(tree, Vec::new(), change, actor.clone(), now(), "")
        .expect("a commit without parents has no duplicate parent");
    store
        .put_commit(&commit)
        .map_err(|error| InitError::Store(Box::new(error)))
}

/// Builds the content of `.stemma/` in `dir`, which exists and is empty.
fn fill(dir: &Path, options: &InitOptions, actor: &Actor) -> Result<(), InitError> {
    write_new(&dir.join("format"), format_content().as_bytes())?;
    let objects = dir.join("objects");
    let tmp = dir.join("tmp");
    let oplog = dir.join("oplog");
    let refs = dir.join("refs");
    let heads = refs.join("heads");
    for created in [&objects, &tmp, &oplog, &refs, &heads] {
        fs::create_dir(created).map_err(InitError::io("create the directory", created))?;
    }
    let head = Head::Branch(options.initial_branch.clone());
    // The working copy starts as an empty commit with a change ID of its own (ADR-010): no file is
    // tracked yet, and a snapshot always replaces a commit that exists.
    let working_copy = first_working_copy(dir, actor)?;
    // The first entry of the log: HEAD and the working-copy commit come into being. It is written
    // before the pointers, as for every pointer (ADR-011); the directory is renamed as a whole, so
    // all appear together.
    let init = OperationName::new("init").expect("`init` is a valid operation name");
    let change = Change::new(
        now(),
        actor.clone(),
        init,
        Some(Moved::from_to(None, Some(head.clone()))),
        Some(Moved::from_to(None, Some(working_copy))),
        vec![],
    )
    .map_err(|error| InitError::Log(OpLogError::Entry(error)))?;
    match FsOpLog::new(dir).append(&change) {
        Ok(_) => {}
        // The directory of the log is flushed below, with the others, and the staging directory is
        // renamed as a whole; an entry that is not known to be flushed yet is no reason to stop.
        Err(OpLogError::NotFlushed { .. }) => {}
        Err(error) => return Err(InitError::Log(error)),
    }
    write_new(&dir.join("working-copy"), &encode_branch(&working_copy))?;
    write_new(&dir.join("HEAD"), &head.encode())?;
    for flushed in [&refs, &heads, &oplog, dir] {
        sync_directory(flushed).map_err(InitError::io("flush the directory", flushed))?;
    }
    Ok(())
}

/// Writes a file that must not exist yet, and flushes it to disk.
fn write_new(path: &Path, content: &[u8]) -> Result<(), InitError> {
    let write = || -> io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(content)?;
        file.sync_all()
    };
    write().map_err(InitError::io("write the file", path))
}

/// Whether something (a file, a directory, or a link) is at `path`.
fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn sync_directory(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// No-op on platforms that cannot flush a directory: the rename is atomic but not made durable.
#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Reasons why [`Repository::init`] fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum InitError {
    /// The authorizer refused the operation.
    Denied {
        /// The directory that was to become a repository.
        path: PathBuf,
    },
    /// The directory contains `.stemma` already. Nothing was changed.
    AlreadyExists {
        /// The existing `.stemma`.
        path: PathBuf,
    },
    /// The storage failed. The cause is the field `source`, and its message ends the message of
    /// this error. Use [`InitError::io_error`] to reach it. The temporary directory is removed.
    Io {
        /// What was being done, for example "write the file".
        operation: &'static str,
        /// The file or directory involved. It can be inside the temporary directory
        /// `.stemma-init-…`, which becomes `.stemma`.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// The repository is complete, but flushing its directory to disk failed, so a crash can
    /// lose it. [`Repository::open`] opens it; the call does not undo it.
    NotDurable {
        /// The directory that could not be flushed, the one that holds `.stemma`.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// The first entry of the log could not be written.
    Log(OpLogError),
    /// The first working-copy commit could not be stored.
    Store(Box<StoreError>),
    /// The change ID of the first working-copy commit could not be made.
    ChangeId(GenerateError),
    /// An operation failed, and the temporary directory could not be removed afterwards. It is
    /// named here and can be removed by hand.
    Leftover {
        /// The temporary directory that stayed behind.
        path: PathBuf,
        /// The error that made `init` fail.
        error: Box<InitError>,
        /// The error of the file system when the directory was removed.
        source: io::Error,
    },
}

impl InitError {
    fn io<'a>(operation: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> InitError + 'a {
        move |source| InitError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    /// Returns the error of the file system if this is [`InitError::Io`], [`InitError::NotDurable`],
    /// the error of a [`InitError::Log`], or the error of a [`InitError::Leftover`]. The chain of [`Error::source`] does not lead to
    /// it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } | Self::NotDurable { source, .. } => Some(source),
            Self::Log(error) => error.io_error(),
            Self::Store(error) => error.io_error(),
            Self::ChangeId(error) => Some(error.io_error()),
            Self::Leftover { error, .. } => error.io_error(),
            Self::Denied { .. } | Self::AlreadyExists { .. } => None,
        }
    }
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied { path } => {
                write!(
                    f,
                    "not allowed to create a repository in `{}`",
                    path.display()
                )
            }
            Self::AlreadyExists { path } => {
                write!(
                    f,
                    "`{}` exists already; nothing was changed",
                    path.display()
                )
            }
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
            Self::Log(error) => write!(
                f,
                "could not write the first entry of the operation log: {error}"
            ),
            Self::Store(error) => {
                write!(f, "could not store the first working-copy commit: {error}")
            }
            Self::ChangeId(error) => write!(
                f,
                "could not make the change ID of the first working-copy commit: {error}"
            ),
            Self::NotDurable { path, source } => write!(
                f,
                "the repository was created in `{}`, but flushing that directory failed: \
                 {source}; the repository is complete, but a crash may lose it",
                path.display()
            ),
            Self::Leftover {
                path,
                error,
                source,
            } => write!(
                f,
                "{error}; the unfinished directory `{}` could not be removed either: {source}",
                path.display()
            ),
        }
    }
}

impl Error for InitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes that of the I/O error, so the source is its own cause.
            Self::Io { source, .. } | Self::NotDurable { source, .. } => source.source(),
            Self::Log(error) => error.source(),
            Self::Store(error) => error.source(),
            Self::ChangeId(error) => error.source(),
            // The message includes the message of both causes. The cause of the first one is the
            // first link that it does not print yet.
            Self::Leftover { error, .. } => error.source(),
            Self::Denied { .. } | Self::AlreadyExists { .. } => None,
        }
    }
}

/// Reasons why [`Repository::open`] fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum OpenError {
    /// The directory has no `.stemma`.
    NotARepository {
        /// The directory that was to be opened.
        path: PathBuf,
    },
    /// `.stemma` exists, but it is not a directory: it is a file, or a link that leads nowhere.
    NotADirectory {
        /// The `.stemma` that is in the way.
        path: PathBuf,
    },
    /// `.stemma` has no file `format`.
    MissingFormat {
        /// The file that is missing.
        path: PathBuf,
    },
    /// The file `format` is not in canonical form: not UTF-8, longer than 64 bytes, or not the
    /// name, one version in decimal digits without a leading zero, and a newline.
    MalformedFormat {
        /// The file.
        path: PathBuf,
    },
    /// The file `format` is in canonical form, but it names another version than this code
    /// supports. The version can be larger than any integer.
    UnsupportedFormat {
        /// The file.
        path: PathBuf,
        /// The version that the file names, in decimal digits.
        found: String,
    },
    /// The storage failed. The cause is the field `source`, and its message ends the message of
    /// this error. Use [`OpenError::io_error`] to reach it.
    Io {
        /// What was being done, for example "read the format file".
        operation: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
}

impl OpenError {
    fn io<'a>(operation: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> OpenError + 'a {
        move |source| OpenError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    /// Returns the error of the file system if this is [`OpenError::Io`]. The chain of
    /// [`Error::source`] does not lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::NotARepository { .. }
            | Self::NotADirectory { .. }
            | Self::MissingFormat { .. }
            | Self::MalformedFormat { .. }
            | Self::UnsupportedFormat { .. } => None,
        }
    }
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotARepository { path } => write!(
                f,
                "`{}` is not a repository: there is no `{METADATA_DIR}`",
                path.display()
            ),
            Self::NotADirectory { path } => write!(
                f,
                "`{}` exists, but it is not a directory (a file, or a link that leads nowhere)",
                path.display()
            ),
            Self::MissingFormat { path } => {
                write!(f, "the repository has no format file `{}`", path.display())
            }
            Self::MalformedFormat { path } => write!(
                f,
                "the format file `{}` is not in canonical form (expected `{}`)",
                path.display(),
                format_content().trim_end()
            ),
            Self::UnsupportedFormat { path, found } => write!(
                f,
                "the format file `{}` names version {found}, but this code supports version \
                 {FORMAT_VERSION}",
                path.display()
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
        }
    }
}

impl Error for OpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes that of the I/O error, so the source is its own cause.
            Self::Io { source, .. } => source.source(),
            Self::NotARepository { .. }
            | Self::NotADirectory { .. }
            | Self::MissingFormat { .. }
            | Self::MalformedFormat { .. }
            | Self::UnsupportedFormat { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Barrier;
    use std::thread;

    use super::*;
    use crate::change::ChangeId;
    use crate::object::{Blob, ObjectId};
    use crate::store::ObjectStore;
    use crate::testutil::TempDir;
    use crate::AllowAll;

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &str, _target: &str) -> Decision {
            Decision::Deny
        }
    }

    fn actor() -> Actor {
        Actor::human("daniel").unwrap()
    }

    fn init(path: &Path) -> Result<Repository, InitError> {
        Repository::init(path, &InitOptions::default(), &actor(), &AllowAll)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn set_format(dir: &Path, content: impl AsRef<[u8]>) {
        fs::write(dir.join(".stemma").join("format"), content).unwrap();
    }

    /// Whether a directory without write permission keeps a file out. It does not for root.
    #[cfg(unix)]
    fn permissions_are_enforced(scratch: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        let probe = scratch.join("probe");
        fs::create_dir(&probe).unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o555)).unwrap();
        let enforced = fs::File::create(probe.join("file")).is_err();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
        enforced
    }

    #[test]
    fn init_creates_the_layout() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let meta = dir.path().join(".stemma");
        assert_eq!(
            names(&meta),
            [
                "HEAD",
                "format",
                "objects",
                "oplog",
                "oplog-tip",
                "refs",
                "tmp",
                "working-copy"
            ]
        );
        assert_eq!(
            fs::read(meta.join("format")).unwrap(),
            b"stemma-repository 2\n"
        );
        assert_eq!(fs::read(meta.join("HEAD")).unwrap(), b"branch main\n");
        assert_eq!(
            fs::read(meta.join("oplog-tip")).unwrap(),
            b"stemma-oplog-tip 1\nnumber 1\n"
        );
        for directory in ["objects", "tmp", "oplog", "refs", "refs/heads"] {
            assert!(meta.join(directory).is_dir(), "{directory}");
        }
        assert_eq!(names(&meta.join("refs")), ["heads"]);
        assert_eq!(repository.path(), dir.path());
        assert_eq!(repository.metadata_dir(), meta);
    }

    #[test]
    fn init_makes_an_empty_working_copy_commit_with_a_change_id_of_its_own() {
        let first = TempDir::new();
        let second = TempDir::new();
        let a = init(first.path()).unwrap();
        let b = init(second.path()).unwrap();
        let id = a.refs().working_copy().unwrap();
        assert_eq!(
            fs::read(a.metadata_dir().join("working-copy")).unwrap(),
            format!("{id}\n").into_bytes()
        );
        let raw = a
            .objects()
            .get_expecting(&id.object_id(), crate::object::Kind::Commit)
            .unwrap()
            .unwrap();
        let commit = Commit::parse(raw.body()).unwrap();
        assert_eq!(commit.tree(), Tree::new(Vec::new()).unwrap().id());
        assert!(commit.parents().is_empty());
        assert_eq!(commit.author(), &actor());
        assert_eq!(commit.message(), "");
        let other = b.refs().working_copy().unwrap();
        let other = Commit::parse(
            b.objects()
                .get_expecting(&other.object_id(), crate::object::Kind::Commit)
                .unwrap()
                .unwrap()
                .body(),
        )
        .unwrap();
        assert_ne!(commit.change(), other.change());
    }

    #[test]
    fn a_repository_without_the_working_copy_file_reports_it() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        fs::remove_file(repository.metadata_dir().join("working-copy")).unwrap();
        let error = repository.refs().working_copy().unwrap_err();
        assert!(matches!(error, RefError::NoWorkingCopy { .. }), "{error}");
        assert!(error.to_string().contains("working-copy"), "{error}");
    }

    /// A commit on top of the working-copy commit with the empty tree, to move the pointer to.
    fn next_commit(repository: &Repository, message: &str) -> CommitId {
        let current = repository.refs().working_copy().unwrap();
        let commit = Commit::new(
            Tree::new(Vec::new()).unwrap().id(),
            vec![current],
            ChangeId::generate().unwrap(),
            actor(),
            1,
            message,
        )
        .unwrap();
        repository.objects().put_commit(&commit).unwrap()
    }

    fn operation(name: &str) -> OperationName {
        OperationName::new(name).unwrap()
    }

    #[test]
    fn the_working_copy_commit_moves_and_the_entry_names_both_values() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let old = repository.refs().working_copy().unwrap();
        let new = next_commit(&repository, "x");
        let entry = repository
            .update_working_copy(&operation("snapshot"), old, new, &actor(), &AllowAll)
            .unwrap()
            .unwrap();
        assert_eq!(repository.refs().working_copy().unwrap(), new);
        assert_eq!(entry.number(), 2);
        let moved = entry.change().working_copy().unwrap();
        assert_eq!((moved.before(), moved.after()), (Some(&old), Some(&new)));
        assert!(entry.change().head().is_none() && entry.change().branches().is_empty());
    }

    #[test]
    fn a_move_to_the_value_it_has_writes_no_entry() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let old = repository.refs().working_copy().unwrap();
        let entry = repository
            .update_working_copy(&operation("snapshot"), old, old, &actor(), &AllowAll)
            .unwrap();
        assert!(entry.is_none());
        assert_eq!(repository.oplog().numbers().unwrap(), [1]);
    }

    #[test]
    fn a_stale_working_copy_move_writes_no_entry_and_does_not_move() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let old = repository.refs().working_copy().unwrap();
        let new = next_commit(&repository, "x");
        let elsewhere = next_commit(&repository, "y");
        let error = repository
            .update_working_copy(&operation("snapshot"), elsewhere, new, &actor(), &AllowAll)
            .unwrap_err();
        assert!(
            matches!(
                error,
                UpdateError::Refs(RefError::StaleWorkingCopy { expected, found })
                    if expected == elsewhere && found == old
            ),
            "{error}"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), old);
        assert_eq!(repository.oplog().numbers().unwrap(), [1]);
    }

    #[test]
    fn a_working_copy_move_to_a_commit_that_is_not_stored_is_refused() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let old = repository.refs().working_copy().unwrap();
        let missing = CommitId::new(
            "b3:1111111111111111111111111111111111111111111111111111111111111111"
                .parse::<ObjectId>()
                .unwrap(),
        );
        let error = repository
            .update_working_copy(&operation("snapshot"), old, missing, &actor(), &AllowAll)
            .unwrap_err();
        assert!(
            matches!(error, UpdateError::Refs(RefError::MissingCommit(id)) if id == missing),
            "{error}"
        );
        assert_eq!(repository.oplog().numbers().unwrap(), [1]);
    }

    #[test]
    fn a_denied_working_copy_move_writes_no_entry() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let old = repository.refs().working_copy().unwrap();
        let new = next_commit(&repository, "x");
        let error = repository
            .update_working_copy(&operation("snapshot"), old, new, &actor(), &DenyAll)
            .unwrap_err();
        assert!(matches!(error, UpdateError::Denied { .. }), "{error}");
        assert_eq!(repository.refs().working_copy().unwrap(), old);
        assert_eq!(repository.oplog().numbers().unwrap(), [1]);
    }

    #[test]
    fn init_can_be_denied_and_leaves_no_working_copy() {
        let dir = TempDir::new();
        let error =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &DenyAll).unwrap_err();
        assert!(matches!(error, InitError::Denied { .. }), "{error}");
        assert_eq!(names(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn init_leaves_nothing_but_the_metadata_directory() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        assert_eq!(names(dir.path()), [".stemma"]);
    }

    #[test]
    fn init_in_a_directory_with_files_leaves_them_alone() {
        let dir = TempDir::new();
        fs::write(dir.path().join("README.md"), b"keep").unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("main.rs"), b"fn main() {}").unwrap();
        init(dir.path()).unwrap();
        assert_eq!(names(dir.path()), [".stemma", "README.md", "src"]);
        assert_eq!(fs::read(dir.path().join("README.md")).unwrap(), b"keep");
        assert_eq!(names(&dir.path().join("src")), ["main.rs"]);
    }

    #[test]
    fn head_points_to_a_branch_that_does_not_exist_yet() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let main = BranchName::new("main").unwrap();
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(main.clone())
        );
        assert_eq!(repository.refs().branch(&main).unwrap(), None);
        assert_eq!(repository.refs().resolve_head().unwrap(), None);
        assert!(repository.refs().branches().unwrap().is_empty());
    }

    #[test]
    fn init_uses_the_initial_branch() {
        let dir = TempDir::new();
        let options = InitOptions::new(BranchName::new("trunk/main").unwrap());
        let repository = Repository::init(dir.path(), &options, &actor(), &AllowAll).unwrap();
        assert_eq!(
            fs::read(dir.path().join(".stemma").join("HEAD")).unwrap(),
            b"branch trunk/main\n"
        );
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(BranchName::new("trunk/main").unwrap())
        );
        assert_eq!(options.initial_branch().as_str(), "trunk/main");
    }

    #[test]
    fn init_creates_missing_directories() {
        let dir = TempDir::new();
        let path = dir.path().join("a").join("b");
        init(&path).unwrap();
        assert!(path.join(".stemma").is_dir());
    }

    #[test]
    fn a_second_init_fails_and_changes_nothing() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let marker = dir.path().join(".stemma").join("marker");
        fs::write(&marker, b"keep").unwrap();
        let error = init(dir.path()).unwrap_err();
        assert!(matches!(error, InitError::AlreadyExists { .. }), "{error}");
        assert_eq!(fs::read(&marker).unwrap(), b"keep");
        assert_eq!(names(dir.path()), [".stemma"]);
    }

    #[test]
    fn init_refuses_an_existing_metadata_directory_even_if_empty() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join(".stemma")).unwrap();
        let error = init(dir.path()).unwrap_err();
        assert!(matches!(error, InitError::AlreadyExists { .. }), "{error}");
        assert_eq!(names(&dir.path().join(".stemma")), Vec::<String>::new());
        assert_eq!(names(dir.path()), [".stemma"]);
    }

    #[cfg(unix)]
    #[test]
    fn init_refuses_a_link_at_the_metadata_directory() {
        use std::os::unix::fs::symlink;
        let dangling = TempDir::new();
        symlink("nowhere", dangling.path().join(".stemma")).unwrap();
        let error = init(dangling.path()).unwrap_err();
        assert!(matches!(error, InitError::AlreadyExists { .. }), "{error}");
        assert!(fs::symlink_metadata(dangling.path().join(".stemma"))
            .unwrap()
            .file_type()
            .is_symlink());

        let valid = TempDir::new();
        let elsewhere = TempDir::new();
        symlink(elsewhere.path(), valid.path().join(".stemma")).unwrap();
        let error = init(valid.path()).unwrap_err();
        assert!(matches!(error, InitError::AlreadyExists { .. }), "{error}");
        assert_eq!(names(elsewhere.path()), Vec::<String>::new());
    }

    #[test]
    fn init_fails_if_the_path_is_a_file() {
        let dir = TempDir::new();
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        let error = init(&file).unwrap_err();
        assert!(matches!(error, InitError::Io { .. }), "{error}");
        assert!(error.io_error().is_some());
        assert_eq!(fs::read(&file).unwrap(), b"x");
    }

    #[cfg(unix)]
    #[test]
    fn init_in_a_directory_that_cannot_be_written_reports_the_cause() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = TempDir::new();
        if !permissions_are_enforced(scratch.path()) {
            eprintln!("skipped: the permission of a directory is not enforced here (root?)");
            return;
        }
        let parent = scratch.path().join("readonly");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        let error = init(&parent).unwrap_err();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(error, InitError::Io { operation, .. } if operation == "create the new directory"),
            "{error}"
        );
        assert_eq!(names(&parent), Vec::<String>::new());
    }

    #[test]
    fn a_denied_init_creates_nothing() {
        let dir = TempDir::new();
        let path = dir.path().join("new");
        let error =
            Repository::init(&path, &InitOptions::default(), &actor(), &DenyAll).unwrap_err();
        assert!(matches!(error, InitError::Denied { .. }), "{error}");
        assert!(!path.exists());
    }

    #[test]
    fn the_authorizer_is_asked_before_the_directory_is_inspected() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let error =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &DenyAll).unwrap_err();
        assert!(matches!(error, InitError::Denied { .. }), "{error}");
    }

    #[test]
    fn init_asks_the_authorizer_with_the_actor_the_operation_and_the_directory() {
        struct Recorder(RefCell<Vec<(String, String, String)>>);
        impl Authorizer for Recorder {
            fn authorize(&self, actor: &Actor, operation: &str, target: &str) -> Decision {
                self.0
                    .borrow_mut()
                    .push((format!("{actor:?}"), operation.into(), target.into()));
                Decision::Allow
            }
        }
        let dir = TempDir::new();
        let recorder = Recorder(RefCell::new(Vec::new()));
        Repository::init(dir.path(), &InitOptions::default(), &actor(), &recorder).unwrap();
        assert_eq!(
            *recorder.0.borrow(),
            [(
                format!("{:?}", actor()),
                "init".to_string(),
                dir.path().display().to_string()
            )]
        );
    }

    #[test]
    fn concurrent_inits_give_one_repository_and_no_leftovers() {
        let dir = TempDir::new();
        let barrier = Barrier::new(8);
        let results: Vec<Result<Repository, InitError>> = thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        init(dir.path())
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        for error in results.iter().filter_map(|r| r.as_ref().err()) {
            assert!(matches!(error, InitError::AlreadyExists { .. }), "{error}");
        }
        assert_eq!(names(dir.path()), [".stemma"]);
        Repository::open(dir.path()).unwrap();
    }

    #[test]
    fn the_staging_directory_is_never_one_that_exists() {
        let dir = TempDir::new();
        let taken = dir.path().join("taken");
        fs::create_dir(&taken).unwrap();
        fs::write(taken.join("marker"), b"keep").unwrap();
        let mut names_to_try = vec!["fresh".to_string(), "taken".to_string()];
        let staging = create_staging(dir.path(), &mut || names_to_try.pop().unwrap()).unwrap();
        assert_eq!(staging, dir.path().join("fresh"));
        assert_eq!(fs::read(taken.join("marker")).unwrap(), b"keep");
    }

    #[test]
    fn creating_the_staging_directory_gives_up_if_every_name_is_taken() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("taken")).unwrap();
        let error = create_staging(dir.path(), &mut || "taken".to_string()).unwrap_err();
        assert!(matches!(error, InitError::Io { .. }), "{error}");
        assert_eq!(names(dir.path()), ["taken"]);
    }

    #[test]
    fn a_failed_fill_removes_the_staging_directory() {
        let dir = TempDir::new();
        let staging = dir.path().join("staging");
        fs::create_dir(&staging).unwrap();
        // `fill` writes a file that must not exist yet, so it fails at the first step.
        fs::write(staging.join("format"), b"x").unwrap();
        let target = dir.path().join(".stemma");
        let error = install(&staging, &target, &InitOptions::default(), &actor()).unwrap_err();
        assert!(
            matches!(error, InitError::Io { operation, .. } if operation == "write the file"),
            "{error}"
        );
        assert!(!staging.exists());
        assert!(!target.exists());
    }

    #[test]
    fn a_rename_onto_a_non_empty_directory_is_already_exists_and_removes_the_staging() {
        let dir = TempDir::new();
        let staging = dir.path().join("staging");
        fs::create_dir(&staging).unwrap();
        let target = dir.path().join(".stemma");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("marker"), b"keep").unwrap();
        let error = install(&staging, &target, &InitOptions::default(), &actor()).unwrap_err();
        assert!(matches!(error, InitError::AlreadyExists { .. }), "{error}");
        assert!(!staging.exists());
        assert_eq!(fs::read(target.join("marker")).unwrap(), b"keep");
    }

    #[test]
    fn a_rename_onto_a_file_is_already_exists() {
        let dir = TempDir::new();
        let staging = dir.path().join("staging");
        fs::create_dir(&staging).unwrap();
        let target = dir.path().join(".stemma");
        fs::write(&target, b"keep").unwrap();
        let error = install(&staging, &target, &InitOptions::default(), &actor()).unwrap_err();
        assert!(matches!(error, InitError::AlreadyExists { .. }), "{error}");
        assert!(!staging.exists());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn a_failed_rename_without_a_target_keeps_its_own_error() {
        let dir = TempDir::new();
        let staging = dir.path().join("staging");
        fs::create_dir(&staging).unwrap();
        let target = dir.path().join("missing").join(".stemma");
        let error = install(&staging, &target, &InitOptions::default(), &actor()).unwrap_err();
        assert!(
            matches!(error, InitError::Io { operation, .. } if operation.starts_with("move")),
            "{error}"
        );
        assert!(!staging.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_staging_directory_that_cannot_be_removed_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        if !permissions_are_enforced(dir.path()) {
            eprintln!("skipped: the permission of a directory is not enforced here (root?)");
            return;
        }
        let staging = dir.path().join("staging");
        let stuck = staging.join("stuck");
        fs::create_dir_all(&stuck).unwrap();
        fs::write(stuck.join("file"), b"x").unwrap();
        fs::write(staging.join("format"), b"x").unwrap();
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o555)).unwrap();
        let error = install(
            &staging,
            &dir.path().join(".stemma"),
            &InitOptions::default(),
            &actor(),
        )
        .unwrap_err();
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o755)).unwrap();
        match &error {
            InitError::Leftover { path, error, .. } => {
                assert_eq!(path, &staging);
                assert!(matches!(**error, InitError::Io { .. }), "{error}");
            }
            other => panic!("not a leftover: {other}"),
        }
        let message = error.to_string();
        assert!(message.contains("could not write the file"), "{message}");
        assert!(message.contains("could not be removed either"), "{message}");
        assert!(message.contains(staging.to_str().unwrap()), "{message}");
        assert!(error.io_error().is_some());
    }

    #[test]
    fn open_returns_what_init_made() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let repository = Repository::open(dir.path()).unwrap();
        assert_eq!(repository.path(), dir.path());
        assert_eq!(repository.refs().resolve_head().unwrap(), None);
    }

    #[test]
    fn the_object_store_of_a_repository_works_after_init_and_after_open() {
        let dir = TempDir::new();
        let id = init(dir.path())
            .unwrap()
            .objects()
            .put_blob(&Blob::new(b"hello".to_vec()))
            .unwrap();
        let repository = Repository::open(dir.path()).unwrap();
        let stored = repository.objects().get(&ObjectId::from(id)).unwrap();
        assert!(stored.is_some());
        assert_eq!(
            names(&dir.path().join(".stemma").join("tmp")),
            Vec::<String>::new()
        );
        assert_eq!(names(dir.path()), [".stemma"]);
    }

    #[test]
    fn open_without_a_metadata_directory_fails() {
        let dir = TempDir::new();
        let error = Repository::open(dir.path()).unwrap_err();
        assert!(matches!(error, OpenError::NotARepository { .. }), "{error}");
        let error = Repository::open(dir.path().join("missing")).unwrap_err();
        assert!(matches!(error, OpenError::NotARepository { .. }), "{error}");
    }

    #[test]
    fn open_of_a_file_is_not_a_repository() {
        let dir = TempDir::new();
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        let error = Repository::open(&file).unwrap_err();
        assert!(matches!(error, OpenError::NotARepository { .. }), "{error}");
    }

    #[test]
    fn open_with_a_file_at_the_metadata_directory_fails() {
        let dir = TempDir::new();
        fs::write(dir.path().join(".stemma"), b"x").unwrap();
        let error = Repository::open(dir.path()).unwrap_err();
        assert!(matches!(error, OpenError::NotADirectory { .. }), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn open_with_a_dangling_link_at_the_metadata_directory_fails() {
        let dir = TempDir::new();
        std::os::unix::fs::symlink("nowhere", dir.path().join(".stemma")).unwrap();
        let error = Repository::open(dir.path()).unwrap_err();
        assert!(matches!(error, OpenError::NotADirectory { .. }), "{error}");
    }

    #[test]
    fn open_without_a_format_file_fails() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join(".stemma")).unwrap();
        let error = Repository::open(dir.path()).unwrap_err();
        assert!(matches!(error, OpenError::MissingFormat { .. }), "{error}");
    }

    #[test]
    fn open_reports_the_error_of_the_file_system_for_a_format_that_cannot_be_read() {
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join(".stemma").join("format")).unwrap();
        let error = Repository::open(dir.path()).unwrap_err();
        assert!(matches!(error, OpenError::Io { .. }), "{error}");
        assert!(error.io_error().is_some());
        assert!(error.to_string().contains("format"), "{error}");
    }

    #[test]
    fn open_rejects_another_version() {
        for version in [
            "0",
            "1",
            "3",
            "99",
            "4294967295",
            "4294967296",
            "99999999999999999999",
        ] {
            let dir = TempDir::new();
            init(dir.path()).unwrap();
            set_format(dir.path(), format!("stemma-repository {version}\n"));
            let error = Repository::open(dir.path()).unwrap_err();
            assert!(
                matches!(&error, OpenError::UnsupportedFormat { found, .. } if found == version),
                "{version}: {error}"
            );
            assert!(error.to_string().contains(version), "{error}");
        }
    }

    #[test]
    fn open_rejects_a_format_that_is_not_canonical() {
        let long_digits = format!("stemma-repository 1{}\n", "0".repeat(100));
        let cases: Vec<&[u8]> = vec![
            b"",
            b"\n",
            b"stemma-repository 1",
            b"stemma-repository 1\n\n",
            b"stemma-repository 01\n",
            b"stemma-repository +1\n",
            b"stemma-repository  1\n",
            b"stemma-repository 1 \n",
            b"Stemma-repository 1\n",
            b"stemma-repository 1\r\n",
            "stemma-repository \u{0661}\n".as_bytes(),
            "stemma-repository \u{ff11}\n".as_bytes(),
            b"stemma-repository \xff\n",
            long_digits.as_bytes(),
        ];
        for content in cases {
            // The length limit is a rule of reading the file, not of the parser.
            if content.len() <= FORMAT_MAX_LEN as usize {
                assert_eq!(parse_format(content), None, "{content:?}");
            }
            let dir = TempDir::new();
            init(dir.path()).unwrap();
            set_format(dir.path(), content);
            let error = Repository::open(dir.path()).unwrap_err();
            assert!(
                matches!(error, OpenError::MalformedFormat { .. }),
                "{content:?}: {error}"
            );
        }
    }

    #[test]
    fn parse_format_returns_the_digits_of_what_format_content_writes() {
        assert_eq!(
            parse_format(format_content().as_bytes()),
            Some(FORMAT_VERSION.to_string().as_str())
        );
        assert_eq!(parse_format(b"stemma-repository 0\n"), Some("0"));
        assert_eq!(parse_format(b"stemma-repository 10\n"), Some("10"));
    }

    #[test]
    fn init_writes_a_format_that_open_accepts() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let written = fs::read(dir.path().join(".stemma").join("format")).unwrap();
        assert_eq!(written, format_content().as_bytes());
        Repository::open(dir.path()).unwrap();
    }

    /// An error with a cause, to see where the chain of causes starts.
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

    fn failure() -> io::Error {
        io::Error::other(Outer)
    }

    #[test]
    fn init_errors_follow_the_convention_for_messages_and_causes() {
        let path = PathBuf::from("/x/.stemma");
        let io_error = InitError::Io {
            operation: "write the file",
            path: path.clone(),
            source: failure(),
        };
        assert_eq!(
            io_error.to_string(),
            "could not write the file `/x/.stemma`: outer"
        );
        // The chain starts with the cause of the cause: the message prints `outer` already.
        assert_eq!(io_error.source().unwrap().to_string(), "deepest");
        assert_eq!(io_error.io_error().unwrap().to_string(), "outer");

        let not_durable = InitError::NotDurable {
            path: PathBuf::from("/x"),
            source: failure(),
        };
        let message = not_durable.to_string();
        assert!(
            message.contains("`/x`") && message.contains("outer"),
            "{message}"
        );
        assert!(message.contains("complete"), "{message}");
        assert_eq!(not_durable.source().unwrap().to_string(), "deepest");
        assert!(not_durable.io_error().is_some());

        let leftover = InitError::Leftover {
            path: PathBuf::from("/x/.stemma-init-1"),
            error: Box::new(io_error),
            source: failure(),
        };
        let message = leftover.to_string();
        assert!(message.starts_with("could not write the file"), "{message}");
        assert!(message.contains("`/x/.stemma-init-1`"), "{message}");
        assert!(message.ends_with("outer"), "{message}");
        assert_eq!(leftover.source().unwrap().to_string(), "deepest");

        for error in [
            InitError::Denied { path: path.clone() },
            InitError::AlreadyExists { path: path.clone() },
        ] {
            assert!(error.to_string().contains("/x"), "{error}");
            assert!(error.source().is_none());
            assert!(error.io_error().is_none());
        }
    }

    #[test]
    fn open_errors_follow_the_convention_for_messages_and_causes() {
        let path = PathBuf::from("/x/.stemma/format");
        let io_error = OpenError::Io {
            operation: "read the format file",
            path: path.clone(),
            source: failure(),
        };
        assert_eq!(
            io_error.to_string(),
            "could not read the format file `/x/.stemma/format`: outer"
        );
        assert_eq!(io_error.source().unwrap().to_string(), "deepest");
        assert_eq!(io_error.io_error().unwrap().to_string(), "outer");

        let cases = [
            (
                OpenError::NotARepository { path: path.clone() },
                "not a repository",
            ),
            (
                OpenError::NotADirectory { path: path.clone() },
                "not a directory",
            ),
            (
                OpenError::MissingFormat { path: path.clone() },
                "no format file",
            ),
            (
                OpenError::MalformedFormat { path: path.clone() },
                "stemma-repository 2",
            ),
            (
                OpenError::UnsupportedFormat {
                    path: path.clone(),
                    found: "7".to_string(),
                },
                "version 7, but this code supports version 2",
            ),
        ];
        for (error, text) in cases {
            let message = error.to_string();
            assert!(
                message.contains("/x/.stemma") && message.contains(text),
                "{message}"
            );
            assert!(error.source().is_none());
            assert!(error.io_error().is_none());
        }
    }

    // The operation log

    use crate::change::sample_change;
    use crate::object::{Commit, Tree};
    use crate::oplog::EntryError;
    use crate::refs::Logged;

    fn op(name: &str) -> OperationName {
        OperationName::new(name).unwrap()
    }

    fn branch(name: &str) -> BranchName {
        BranchName::new(name).unwrap()
    }

    /// Stores a commit with no parents and returns its ID.
    fn store_commit(repository: &Repository, message: &str) -> CommitId {
        let tree = repository
            .objects()
            .put_tree(&Tree::new(vec![]).unwrap())
            .unwrap();
        let commit = Commit::new(tree, vec![], sample_change(), actor(), 1, message).unwrap();
        repository.objects().put_commit(&commit).unwrap()
    }

    fn numbers(repository: &Repository) -> Vec<u64> {
        repository.oplog().numbers().unwrap()
    }

    fn update(
        repository: &Repository,
        name: &str,
        expected: Option<CommitId>,
        new: CommitId,
    ) -> Result<Option<Entry>, UpdateError> {
        repository.update_branch(
            &op("commit"),
            &branch(name),
            expected,
            new,
            &actor(),
            &AllowAll,
        )
    }

    #[test]
    fn init_writes_the_first_entry_of_the_log() {
        let before = now();
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let after = now();
        assert_eq!(numbers(&repository), [1]);
        let entry = repository.oplog().read(1).unwrap();
        let change = entry.change();
        assert_eq!(change.operation().as_str(), "init");
        assert_eq!(change.actor(), &actor());
        assert!(
            (before..=after).contains(&change.time()),
            "{}",
            change.time()
        );
        let head = change.head().unwrap();
        assert_eq!(head.before(), None);
        assert_eq!(head.after(), Some(&Head::Branch(branch("main"))));
        let working_copy = change.working_copy().unwrap();
        assert_eq!(working_copy.before(), None);
        assert_eq!(
            working_copy.after(),
            Some(&repository.refs().working_copy().unwrap())
        );
        assert!(change.branches().is_empty());
        // The entry is the one that a later open sees, and nothing else is in the directories.
        assert_eq!(
            Repository::open(dir.path())
                .unwrap()
                .oplog()
                .entries()
                .unwrap(),
            [entry]
        );
        assert_eq!(
            names(&dir.path().join(".stemma").join("tmp")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_first_entry_names_the_initial_branch_and_the_actor_of_an_agent() {
        let dir = TempDir::new();
        let agent = Actor::agent("a1", "opus-5-5", "s1", "daniel").unwrap();
        let options = InitOptions::new(branch("trunk/main"));
        let repository = Repository::init(dir.path(), &options, &agent, &AllowAll).unwrap();
        let entry = repository.oplog().read(1).unwrap();
        assert_eq!(entry.change().actor(), &agent);
        assert_eq!(
            entry.change().head().unwrap().after(),
            Some(&Head::Branch(branch("trunk/main")))
        );
    }

    #[test]
    fn a_refused_init_writes_no_log() {
        let dir = TempDir::new();
        let path = dir.path().join("new");
        let _ = Repository::init(&path, &InitOptions::default(), &actor(), &DenyAll).unwrap_err();
        assert!(!path.exists());
    }

    #[test]
    fn a_branch_that_is_created_and_moved_is_recorded_with_old_and_new() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let (a, b) = (
            store_commit(&repository, "a"),
            store_commit(&repository, "b"),
        );
        let created = update(&repository, "main", None, a).unwrap().unwrap();
        let moved = update(&repository, "main", Some(a), b).unwrap().unwrap();
        assert_eq!((created.number(), moved.number()), (2, 3));
        assert_eq!(repository.refs().branch(&branch("main")).unwrap(), Some(b));
        for (entry, old, new) in [(&created, None, a), (&moved, Some(a), b)] {
            let change = entry.change();
            assert_eq!(change.operation().as_str(), "commit");
            assert_eq!(change.actor(), &actor());
            assert!(change.head().is_none() && change.working_copy().is_none());
            let [(name, pair)] = change.branches() else {
                panic!("one branch")
            };
            assert_eq!(name, &branch("main"));
            assert_eq!(
                (pair.before().copied(), pair.after().copied()),
                (old, Some(new))
            );
        }
        assert_eq!(repository.oplog().entries().unwrap()[1..], [created, moved]);
    }

    #[test]
    fn a_branch_that_is_not_where_the_caller_saw_it_writes_no_entry_and_does_not_move() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let (a, b) = (
            store_commit(&repository, "a"),
            store_commit(&repository, "b"),
        );
        update(&repository, "main", None, a).unwrap();
        let before = numbers(&repository);
        for expected in [None, Some(b)] {
            let error = update(&repository, "main", expected, b).unwrap_err();
            assert!(
                matches!(&error, UpdateError::Refs(RefError::Stale { found: Some(f), .. }) if *f == a),
                "{error}"
            );
        }
        assert_eq!(numbers(&repository), before);
        assert_eq!(repository.refs().branch(&branch("main")).unwrap(), Some(a));
    }

    #[test]
    fn a_commit_that_is_not_in_the_store_writes_no_entry() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let elsewhere = TempDir::new();
        let other = init(elsewhere.path()).unwrap();
        let foreign = store_commit(&other, "in another repository");
        let before = numbers(&repository);
        let error = update(&repository, "main", None, foreign).unwrap_err();
        assert!(
            matches!(error, UpdateError::Refs(RefError::MissingCommit(_))),
            "{error}"
        );
        assert_eq!(numbers(&repository), before);
        assert_eq!(repository.refs().branch(&branch("main")).unwrap(), None);
    }

    #[test]
    fn a_move_to_the_value_that_a_branch_has_writes_no_entry() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let a = store_commit(&repository, "a");
        update(&repository, "main", None, a).unwrap();
        let before = numbers(&repository);
        assert_eq!(update(&repository, "main", Some(a), a).unwrap(), None);
        assert_eq!(numbers(&repository), before);
    }

    #[test]
    fn a_refused_operation_writes_no_entry_and_does_not_move() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let a = store_commit(&repository, "a");
        let before = numbers(&repository);
        let error = repository
            .update_branch(&op("commit"), &branch("main"), None, a, &actor(), &DenyAll)
            .unwrap_err();
        assert!(
            matches!(&error, UpdateError::Denied { operation, target } if operation == "commit" && target == "main"),
            "{error}"
        );
        assert_eq!(error.to_string(), "not allowed to commit `main`");
        let error = repository
            .set_head(
                &op("checkout"),
                &Head::Branch(branch("x")),
                &actor(),
                &DenyAll,
            )
            .unwrap_err();
        assert!(matches!(error, UpdateError::Denied { .. }), "{error}");
        assert_eq!(numbers(&repository), before);
        assert_eq!(repository.refs().branch(&branch("main")).unwrap(), None);
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(branch("main"))
        );
    }

    #[test]
    fn a_lost_last_entry_stops_every_operation_before_a_pointer_moves() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let a = store_commit(&repository, "a");
        let b = store_commit(&repository, "b");
        assert!(update(&repository, "main", None, a).unwrap().is_some());
        // The entry of the move to `a` is lost; the tip file still names it.
        let lost = dir.path().join(".stemma/oplog/000000000002");
        fs::remove_file(&lost).unwrap();
        let head = repository.refs().head().unwrap();

        let error = update(&repository, "main", Some(a), b).unwrap_err();
        assert!(
            matches!(&error, UpdateError::Log(OpLogError::TipAhead { .. })),
            "{error}"
        );
        assert!(error.entry().is_none());
        let moved = repository.set_head(
            &op("checkout"),
            &Head::Branch(branch("x")),
            &actor(),
            &AllowAll,
        );

        assert!(matches!(
            moved,
            Err(UpdateError::Log(OpLogError::TipAhead { .. }))
        ));
        // The branch is where the lost entry put it, and no pointer moved on, no entry was added.
        assert_eq!(repository.refs().branch(&branch("main")).unwrap(), Some(a));
        assert_eq!(repository.refs().head().unwrap(), head);
        assert_eq!(
            fs::read_dir(dir.path().join(".stemma/oplog"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn a_lock_that_is_held_writes_no_entry_and_moves_nothing() {
        // The lock is taken before the entry is written, so a change that meets a lock leaves no
        // entry that names a move that was never going to be made.
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let a = store_commit(&repository, "a");
        fs::write(
            dir.path().join(".stemma").join("refs.lock"),
            b"another writer",
        )
        .unwrap();
        let before = numbers(&repository);
        let error = update(&repository, "main", None, a).unwrap_err();
        assert!(
            matches!(error, UpdateError::Refs(RefError::Locked { .. })),
            "{error}"
        );
        assert!(error.entry().is_none());
        let error = repository
            .set_head(
                &op("checkout"),
                &Head::Branch(branch("x")),
                &actor(),
                &AllowAll,
            )
            .unwrap_err();
        assert!(
            matches!(error, UpdateError::Refs(RefError::Locked { .. })),
            "{error}"
        );
        assert_eq!(numbers(&repository), before);
        assert_eq!(repository.refs().branch(&branch("main")).unwrap(), None);
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(branch("main"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_move_that_fails_after_the_entry_leaves_the_entry_and_says_so() {
        // The entry comes first: with the directory of the branches read-only, the entry is
        // written and the move fails. If the order were the other one, nothing would be written.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let a = store_commit(&repository, "a");
        let heads = dir.path().join(".stemma").join("refs").join("heads");
        fs::set_permissions(&heads, fs::Permissions::from_mode(0o555)).unwrap();
        let writable = fs::File::create(heads.join("probe")).is_ok();
        let result = update(&repository, "main", None, a);
        fs::set_permissions(&heads, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_file(heads.join("probe"));
        if writable {
            eprintln!("skipped: the permission of a directory is not enforced here (root?)");
            return;
        }
        let error = result.unwrap_err();
        let UpdateError::MoveFailed { entry, source } = &error else {
            panic!("not a failed move: {error}");
        };
        assert_eq!(entry.number(), 2);
        assert!(source.io_error().is_some());
        assert_eq!(error.entry().map(Entry::number), Some(2));
        let message = error.to_string();
        assert!(
            message.contains("entry 2") && message.contains("names a move that did not happen"),
            "{message}"
        );
        assert!(error.io_error().is_some());
        // The entry is in the log and names the move; the branch did not move.
        assert_eq!(numbers(&repository), [1, 2]);
        let read = repository.oplog().read(2).unwrap();
        let [(name, pair)] = read.change().branches() else {
            panic!("one branch")
        };
        assert_eq!(name, &branch("main"));
        assert_eq!((pair.before(), pair.after()), (None, Some(&a)));
        assert_eq!(repository.refs().branch(&branch("main")).unwrap(), None);
    }

    #[test]
    fn the_outcomes_of_a_step_that_ran_become_errors_that_carry_the_entry() {
        let entry = || {
            Entry::new(
                9,
                Change::new(
                    1,
                    actor(),
                    op("commit"),
                    None,
                    None,
                    vec![(
                        branch("main"),
                        Moved::from_to(None, Some(CommitId::new("b3:1d719c9e1d926eb0f33467406bf50911eca1d59f976105abd3eb5ccfaabdeed9".parse().unwrap()))),
                    )],
                )
                .unwrap(),
            )
            .unwrap()
        };
        let disk = || io::Error::other("disk");
        let not_flushed = || RefError::NotFlushed {
            path: PathBuf::from("/x/refs"),
            source: disk(),
        };
        let io_failure = || RefError::Io {
            operation: "write the reference",
            path: PathBuf::from("/x/refs/heads/main"),
            source: disk(),
        };
        type Outcome = Logged<Entry, OpLogError>;
        assert!(matches!(Outcome::Unchanged.into_result(), Ok(None)));
        let moved = Outcome::Moved(entry()).into_result().unwrap().unwrap();
        assert_eq!(moved.number(), 9);
        let failed = Outcome::StepFailed(OpLogError::Full)
            .into_result()
            .unwrap_err();
        assert!(
            matches!(&failed, UpdateError::Log(OpLogError::Full)),
            "{failed}"
        );
        assert!(failed.entry().is_none());
        let move_failed = Outcome::MoveFailed(entry(), io_failure())
            .into_result()
            .unwrap_err();
        assert!(matches!(&move_failed, UpdateError::MoveFailed { .. }));
        assert_eq!(move_failed.entry().map(Entry::number), Some(9));
        assert!(
            move_failed.to_string().contains("did not happen"),
            "{move_failed}"
        );
        let durable = Outcome::NotFlushed(entry(), not_flushed())
            .into_result()
            .unwrap_err();
        assert!(matches!(&durable, UpdateError::NotDurable { .. }));
        assert_eq!(durable.entry().map(Entry::number), Some(9));
        let message = durable.to_string();
        assert!(
            message.contains("was made") && message.contains("must not be repeated"),
            "{message}"
        );
        assert!(durable.io_error().is_some());
        // An entry that is not known to be flushed is in the log, and the pointer did not move.
        let unflushed = UpdateError::Log(OpLogError::NotFlushed {
            entry: Box::new(entry()),
            path: PathBuf::from("/x/oplog"),
            source: disk(),
        });
        assert_eq!(unflushed.entry().map(Entry::number), Some(9));
        assert!(!unflushed.pointer_moved());
        let message = unflushed.to_string();
        assert!(
            message.contains("entry 9")
                && message.contains("pointer did not move")
                && message.contains("repeating the change writes another entry"),
            "{message}"
        );
        assert!(unflushed.io_error().is_some());
        // The answers to the two questions for every outcome.
        assert!(durable.pointer_moved() && !move_failed.pointer_moved() && !failed.pointer_moved());
        // Any other error of the log means that no entry was written.
        assert!(UpdateError::Log(OpLogError::Full).entry().is_none());
        assert!(UpdateError::Log(OpLogError::EntryTooLarge { size: 70_000 })
            .entry()
            .is_none());
        assert!(UpdateError::Denied {
            operation: "x".into(),
            target: "y".into()
        }
        .entry()
        .is_none());
    }

    #[test]
    fn a_name_that_is_in_the_way_is_a_conflict_before_any_entry_is_written() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let (a, b) = (
            store_commit(&repository, "a"),
            store_commit(&repository, "b"),
        );
        update(&repository, "main", None, a).unwrap();
        update(&repository, "x/y", None, a).unwrap();
        let before = numbers(&repository);
        // `main` is a branch, so `main/z` cannot be one; `x/y` is a branch, so `x` cannot be.
        for name in ["main/z", "x"] {
            let error = update(&repository, name, None, b).unwrap_err();
            assert!(
                matches!(&error, UpdateError::Refs(RefError::Conflict { .. })),
                "{name}: {error}"
            );
            assert!(error.entry().is_none(), "{name}");
        }
        assert_eq!(numbers(&repository), before);
        assert_eq!(repository.refs().branch(&branch("main")).unwrap(), Some(a));
        assert_eq!(repository.refs().branches().unwrap().len(), 2);
    }

    #[test]
    fn head_changes_are_chained_the_old_value_is_the_previous_new_value() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let names: Vec<BranchName> = (0..6).map(|n| branch(&format!("b{n}"))).collect();
        let barrier = std::sync::Barrier::new(names.len());
        let entries: Vec<Entry> = std::thread::scope(|scope| {
            let handles: Vec<_> = names
                .iter()
                .map(|name| {
                    let (repository, barrier) = (&repository, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        // A deadline and not a count of tries: how long a writer waits depends on
                        // the load of the machine, since each holder of the lock flushes to disk.
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(60);
                        while std::time::Instant::now() < deadline {
                            match repository.set_head(
                                &op("checkout"),
                                &Head::Branch(name.clone()),
                                &actor(),
                                &AllowAll,
                            ) {
                                Ok(entry) => return entry.expect("every value is new"),
                                // Another writer holds the lock: try again.
                                Err(UpdateError::Refs(RefError::Locked { .. })) => {
                                    std::thread::yield_now();
                                }
                                Err(error) => panic!("{error}"),
                            }
                        }
                        panic!("the lock was never free: a lock file may have leaked");
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        // Entry 1 is `init`; the others came in the order of their numbers, and each one names as its
        // old value what the one before it set. The log does not lie about its own history.
        let mut entries = entries;
        entries.sort_by_key(Entry::number);
        assert_eq!(
            entries.iter().map(Entry::number).collect::<Vec<_>>(),
            (2..=7).collect::<Vec<_>>()
        );
        let mut previous = Head::Branch(branch("main"));
        for entry in &entries {
            let head = entry.change().head().unwrap();
            assert_eq!(head.before(), Some(&previous), "entry {}", entry.number());
            previous = head.after().unwrap().clone();
        }
        assert_eq!(repository.refs().head().unwrap(), previous);
        assert_eq!(numbers(&repository), (1..=7).collect::<Vec<_>>());
    }

    #[test]
    fn a_damaged_head_is_an_error_and_is_not_replaced() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let head = dir.path().join(".stemma").join("HEAD");
        fs::write(&head, b"not a head\n").unwrap();
        let before = numbers(&repository);
        let error = repository
            .set_head(
                &op("checkout"),
                &Head::Branch(branch("x")),
                &actor(),
                &AllowAll,
            )
            .unwrap_err();
        assert!(
            matches!(&error, UpdateError::Refs(RefError::Corrupt { .. })),
            "{error}"
        );
        assert_eq!(fs::read(&head).unwrap(), b"not a head\n");
        assert_eq!(numbers(&repository), before);
    }

    #[test]
    fn a_pointer_does_not_move_if_the_entry_cannot_be_written() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let a = store_commit(&repository, "a");
        let log = dir.path().join(".stemma").join("oplog");
        fs::remove_dir_all(&log).unwrap();
        fs::write(&log, b"not a directory").unwrap();
        let error = update(&repository, "main", None, a).unwrap_err();
        assert!(
            matches!(error, UpdateError::Log(OpLogError::Io { .. })),
            "{error}"
        );
        assert!(error.io_error().is_some());
        assert!(error.entry().is_none() && !error.pointer_moved());
        assert_eq!(repository.refs().branch(&branch("main")).unwrap(), None);
        let error = repository
            .set_head(
                &op("checkout"),
                &Head::Branch(branch("x")),
                &actor(),
                &AllowAll,
            )
            .unwrap_err();
        assert!(matches!(error, UpdateError::Log(_)), "{error}");
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(branch("main"))
        );
    }

    #[test]
    fn head_is_recorded_with_the_value_it_had() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let a = store_commit(&repository, "a");
        let to_branch = repository
            .set_head(
                &op("checkout"),
                &Head::Branch(branch("dev")),
                &actor(),
                &AllowAll,
            )
            .unwrap()
            .unwrap();
        let detached = repository
            .set_head(&op("checkout"), &Head::Detached(a), &actor(), &AllowAll)
            .unwrap()
            .unwrap();
        assert_eq!(repository.refs().head().unwrap(), Head::Detached(a));
        let first = to_branch.change().head().unwrap();
        assert_eq!(first.before(), Some(&Head::Branch(branch("main"))));
        assert_eq!(first.after(), Some(&Head::Branch(branch("dev"))));
        let second = detached.change().head().unwrap();
        assert_eq!(second.before(), Some(&Head::Branch(branch("dev"))));
        assert_eq!(second.after(), Some(&Head::Detached(a)));
        // The commit that HEAD named is a root of the log.
        assert!(detached.change().commits().contains(&a));
    }

    #[test]
    fn a_head_that_is_unchanged_or_names_a_missing_commit_writes_no_entry() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let before = numbers(&repository);
        let same = repository
            .set_head(
                &op("checkout"),
                &Head::Branch(branch("main")),
                &actor(),
                &AllowAll,
            )
            .unwrap();
        assert_eq!(same, None);
        let elsewhere = TempDir::new();
        let foreign = store_commit(&init(elsewhere.path()).unwrap(), "x");
        let error = repository
            .set_head(
                &op("checkout"),
                &Head::Detached(foreign),
                &actor(),
                &AllowAll,
            )
            .unwrap_err();
        assert!(
            matches!(error, UpdateError::Refs(RefError::MissingCommit(_))),
            "{error}"
        );
        assert_eq!(numbers(&repository), before);
    }

    #[test]
    fn the_references_can_be_read_through_the_view() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let (a, b) = (
            store_commit(&repository, "a"),
            store_commit(&repository, "b"),
        );
        update(&repository, "main", None, a).unwrap();
        update(&repository, "dev", None, b).unwrap();
        let view = repository.refs();
        assert_eq!(view.head().unwrap(), Head::Branch(branch("main")));
        assert_eq!(view.branch(&branch("dev")).unwrap(), Some(b));
        assert_eq!(
            view.branches().unwrap(),
            [(branch("dev"), b), (branch("main"), a)]
        );
        assert_eq!(view.resolve_head().unwrap(), Some(a));
    }

    #[test]
    fn concurrent_updates_of_one_branch_leave_one_winner_and_one_entry() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let commits: Vec<CommitId> = (0..6)
            .map(|n| store_commit(&repository, &format!("c{n}")))
            .collect();
        let barrier = std::sync::Barrier::new(commits.len());
        let results: Vec<Result<Option<Entry>, UpdateError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = commits
                .iter()
                .map(|&commit| {
                    let (repository, barrier) = (&repository, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        update(repository, "main", None, commit)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let winners: Vec<&Entry> = results
            .iter()
            .filter_map(|r| r.as_ref().ok().and_then(Option::as_ref))
            .collect();
        assert_eq!(winners.len(), 1, "{results:?}");
        for error in results.iter().filter_map(|r| r.as_ref().err()) {
            assert!(
                matches!(
                    error,
                    UpdateError::Refs(RefError::Stale { .. } | RefError::Locked { .. })
                ),
                "{error}"
            );
            // A loser leaves no entry: the lock is held from the check to the move.
            assert!(error.entry().is_none(), "{error}");
        }
        // The log holds `init` and the one move, no entry for a try that lost.
        assert_eq!(numbers(&repository), [1, 2]);
        let moved = repository.refs().branch(&branch("main")).unwrap().unwrap();
        let entry = repository.oplog().read(2).unwrap();
        assert_eq!(&entry, winners[0]);
        assert_eq!(entry.change().branches()[0].1.after(), Some(&moved));
        assert!(commits.contains(&moved));
    }

    #[test]
    fn a_repository_of_the_format_before_the_log_is_refused() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        set_format(dir.path(), "stemma-repository 1\n");
        let error = Repository::open(dir.path()).unwrap_err();
        assert!(
            matches!(&error, OpenError::UnsupportedFormat { found, .. } if found == "1"),
            "{error}"
        );
    }

    #[test]
    fn an_error_of_the_log_during_init_is_reported_and_leaves_nothing() {
        let dir = TempDir::new();
        let staging = dir.path().join("staging");
        fs::create_dir(&staging).unwrap();
        // `oplog` is a file, so the directory cannot be made.
        fs::write(staging.join("oplog"), b"x").unwrap();
        let error = install(
            &staging,
            &dir.path().join(".stemma"),
            &InitOptions::default(),
            &actor(),
        )
        .unwrap_err();
        assert!(!staging.exists());
        assert!(!dir.path().join(".stemma").exists());
        assert!(
            error.to_string().contains("oplog") || error.io_error().is_some(),
            "{error}"
        );
        // The message of a failed entry names the log.
        let log = InitError::Log(OpLogError::Entry(EntryError::NoChange));
        assert!(
            log.to_string().contains("first entry of the operation log"),
            "{log}"
        );
        assert!(log.io_error().is_none() && log.source().is_none());
    }

    #[test]
    fn the_constants_that_init_relies_on_are_valid() {
        assert_eq!(OperationName::new("init").unwrap().as_str(), "init");
        assert_eq!(InitOptions::default().initial_branch().as_str(), "main");
    }

    #[test]
    fn an_actor_that_is_too_long_for_an_entry_is_refused_and_nothing_is_created() {
        // The reader refuses an entry above its limit, so the writer must refuse it first, or a
        // log would hold an entry that nobody can read.
        let dir = TempDir::new();
        let path = dir.path().join("new");
        let actor = Actor::human("a".repeat(70_000)).unwrap();
        let error =
            Repository::init(&path, &InitOptions::default(), &actor, &AllowAll).unwrap_err();
        assert!(
            matches!(&error, InitError::Log(OpLogError::EntryTooLarge { .. })),
            "{error}"
        );
        assert!(!path.join(".stemma").exists());
        assert_eq!(names(&path), Vec::<String>::new());
        // The same for a move of an existing repository: no entry, and the pointer stays.
        let repository = init(&dir.path().join("ok")).unwrap();
        let a = store_commit(&repository, "a");
        let error = repository
            .update_branch(&op("commit"), &branch("main"), None, a, &actor, &AllowAll)
            .unwrap_err();
        assert!(
            matches!(&error, UpdateError::Log(OpLogError::EntryTooLarge { .. })),
            "{error}"
        );
        assert!(error.entry().is_none() && !error.pointer_moved());
        assert_eq!(numbers(&repository), [1]);
        assert_eq!(repository.refs().branch(&branch("main")).unwrap(), None);
        assert!(repository.oplog().entries().is_ok());
    }
}
