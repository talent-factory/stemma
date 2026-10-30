//! A repository: the metadata directory `.stemma/` with its object store and references
//! (ADR-009).
//!
//! The layout of `.stemma/` is:
//!
//! ```text
//! .stemma/
//! ├── format          "stemma-repository 5" and a newline; 2 to 4 before the owner (ADR-010)
//! ├── HEAD            "branch <name>" and a newline (the branch may not exist yet)
//! ├── working-copy    the ID of the working-copy commit and a newline (ADR-010)
//! ├── objects/        loose objects (ADR-007)
//! ├── refs/heads/     branches (ADR-008)
//! ├── refs/tags/      tags, made by the first tag (ADR-008)
//! ├── oplog/          the operation log: one numbered file per entry (ADR-011)
//! ├── oplog-tip       the number and the hash of the last entry of the log (ADR-011)
//! ├── purge-pending   the journal of a purge, only while it runs or after it stopped (ADR-014)
//! └── tmp/            temporary files of the object store, the references, and the log
//! ```

use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use std::time::{SystemTime, UNIX_EPOCH};

mod checkout;
mod commit;
mod difference;
mod purge;
#[cfg(any(fuzzing, test))]
pub(crate) use purge::fuzz_journal;
mod restore;
mod snapshot;
mod status;
mod track;

pub use checkout::{remove_temp_files_for_exit, CheckoutError, CheckoutPlan, PlannedWrite};
pub use commit::{CommitError, Committed};
pub use difference::{Comparison, Content, Diff, DiffError, FileDiff, Side, TextDiff};
pub use purge::{
    Aborted, ApplyPurgeError, JournalError, PurgeError, PurgeFailure, PurgePlan, PurgeStep, Purged,
};
pub use restore::{RestoreError, RestorePlan, Restored};
pub use snapshot::{SnapshotError, Unsupported};
pub use status::{FileChange, FileChangeKind, Saved, Status, StatusError};
pub use track::{normalize_path, normalize_path_in, TrackError, Tracking};

use crate::change::{ChangeId, GenerateError};
use crate::object::{Commit, CommitId, ObjectId, Tree, METADATA_DIR};
use crate::oplog::{Change, Entry, FsOpLog, Moved, OpLogError, OperationName, Purge, TAKE_OVER};
use crate::refs::{
    encode_working_copy, BranchName, FsRefStore, Head, Logged, PurgeLock, RefError, RefStore,
    WriteLock,
};
use crate::store::{
    create_dirs_synced as create_store_dirs, create_temp, flush_directory,
    sync_directory as flush_store_directory, unique_temp_name, FsObjectStore, ObjectStore,
    StoreError, INIT_STAGING_PREFIX,
};
use crate::verify::RefVerifyError;
use crate::{Actor, ActorRef, Allowed, Authorizer};

/// The version of the repository format that `init` writes, and that a purge and every new entry
/// of the log move a repository to: version 5, the owner of the working copy (ADR-010).
const FORMAT_VERSION: u32 = 5;

/// The versions of the repository format that this code reads. Version 3 adds the stub and the
/// `purge` entry to the operation log and the journal `purge-pending` (ADR-014). A repository
/// moves to it with its first purge, before the journal is written, so that a program that knows
/// only version 2 refuses a log that it cannot read and does not write past a journal it does not
/// know. Version 4 adds the hash chain: entries of version 3 and the tip file of version 2. A
/// repository moves to it before its first entry in the chain, for the same reason: a program
/// that knows only version 3 would write entries outside the chain after it. Version 5 adds the
/// owner to the file `working-copy`: a repository moves to it with its next entry, and the actor
/// of the command that causes the rise becomes the owner, so that a program that knows only
/// version 4 refuses a file that it cannot read.
const READ_VERSIONS: [u32; 4] = [2, 3, 4, 5];

// A purge and every new entry of the log move a repository to `FORMAT_VERSION` (ADR-011,
// ADR-014). It is the highest that this code reads, so neither lowers a version.
const _: () = assert!(FORMAT_VERSION == READ_VERSIONS[READ_VERSIONS.len() - 1]);

/// The name that the file `format` starts with, followed by the version.
const FORMAT_PREFIX: &str = "stemma-repository ";

/// The longest `format` file that is read. A longer file is not in canonical form anyway.
const FORMAT_MAX_LEN: u64 = 64;

/// How many names are tried for the temporary directory of `init` before it gives up.
const STAGING_ATTEMPTS: usize = 16;

/// The content of the file `format` that `init` writes: the prefix, the version, and a newline.
fn format_content() -> String {
    format_of(FORMAT_VERSION)
}

/// The content of the file `format` for a version.
fn format_of(version: u32) -> String {
    format!("{FORMAT_PREFIX}{version}\n")
}

/// Moves the repository whose metadata directory is `dir` to `version` of the format, unless the
/// file `format` names it already. The new content goes to a temporary file in `tmp/`, is flushed,
/// and is renamed over the file, and the directory is flushed, so a reader sees the old or the new
/// version and never a mix. A purge and the writer of an entry call it (ADR-011, ADR-014). The
/// version is never lowered: a file that names `version` or a higher one is left alone.
///
/// If the directory cannot be flushed after the rename, the old content is put back the same way
/// and the error of the flush is returned. A later call then finds the old version and raises it
/// again, flush included; without that, it would find the new version, skip the flush, and let an
/// entry of the chain rest on a format file that a crash may undo. The callers write nothing
/// after an error, so the old version may stay on disk. If the old content cannot be put back
/// either, that error is returned instead, as it names the file that holds the new version.
fn raise_format(dir: &Path, version: u32) -> Result<(), StoreError> {
    raise_format_with(dir, version, &flush_store_directory)
}

/// [`raise_format`] with the flush of the directory as a parameter, so that a test can fail it.
fn raise_format_with(
    dir: &Path,
    version: u32,
    flush: &dyn Fn(&Path) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let path = dir.join("format");
    let current = fs::read(&path).map_err(|source| StoreError::Io {
        operation: "read the format file",
        path: path.clone(),
        source,
    })?;
    let found = parse_format(&current).and_then(|digits| digits.parse::<u32>().ok());
    if found.is_some_and(|found| found >= version) {
        return Ok(());
    }
    let temp_dir = dir.join("tmp");
    create_store_dirs(dir, &temp_dir)?;
    replace_format(
        &temp_dir,
        &path,
        format_of(version).as_bytes(),
        "replace the format file",
    )?;
    flush(dir).or_else(|error| {
        replace_format(&temp_dir, &path, &current, "restore the format file")?;
        Err(error)
    })
}

/// Writes `content` to a new temporary file in `temp_dir`, flushes it, and renames it over the
/// format file at `path`. The directory is not flushed. `operation` names the rename in an error.
fn replace_format(
    temp_dir: &Path,
    path: &Path,
    content: &[u8],
    operation: &'static str,
) -> Result<(), StoreError> {
    let (mut file, mut temp) = create_temp(temp_dir, &mut unique_temp_name)?;
    let io = |operation: &'static str, path: &Path| {
        let path = path.to_path_buf();
        move |source| StoreError::Io {
            operation,
            path,
            source,
        }
    };
    file.write_all(content)
        .map_err(io("write the temporary file", &temp.path))?;
    file.sync_all()
        .map_err(io("flush the temporary file", &temp.path))?;
    drop(file);
    fs::rename(&temp.path, path).map_err(io(operation, path))?;
    temp.disarm();
    Ok(())
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
        Self::new(BranchName::try_new("main").expect("`main` is a valid branch name"))
    }
}

/// An open repository: the directory that contains `.stemma/`, with its object store, its
/// references, and its operation log.
///
/// The value holds paths only and no handle, so a clone is as good as the original. That is a
/// promise of this type: a field that makes a clone wrong, such as an open lock, has to remove
/// the `Clone` first. The one other field, the take-over of [`taking_over`](Self::taking_over),
/// is shared by the clones on purpose.
#[derive(Debug, Clone)]
pub struct Repository {
    path: PathBuf,
    objects: FsObjectStore,
    refs: FsRefStore,
    oplog: FsOpLog,
    /// Set by [`taking_over`](Self::taking_over): the snapshots take the working copy over, and
    /// the entry of the take-over goes here. Clones share it, which is what a clone of a value
    /// that takes over should do.
    take_over: Option<Arc<Mutex<Option<Entry>>>>,
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
    /// killed in the middle leaves it behind. The next command that writes in the repository
    /// that then exists in `path` removes it under the lock of the references (ADR-009,
    /// STEMMA-31); otherwise it can be removed by hand. HEAD points to the initial branch of
    /// `options`, which does not exist yet.
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
        let Some(allowed) = Allowed::ask(
            authorizer,
            actor,
            &OperationName::try_new("init").expect("`init` is a valid operation name"),
            &path.display().to_string(),
        ) else {
            return Err(InitError::Denied {
                path: path.to_path_buf(),
            });
        };
        create_dirs_synced(path)?;
        if exists(&target).map_err(InitError::io("inspect", &target))? {
            return Err(InitError::AlreadyExists { path: target });
        }
        let staging = create_staging(path, &mut || {
            format!("{INIT_STAGING_PREFIX}{}", unique_temp_name())
        })?;
        install(&staging, &target, options, actor, &allowed)?;
        flush_directory(path).map_err(|source| InitError::NotDurable {
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
            Some(version)
                if READ_VERSIONS
                    .iter()
                    .any(|known| version == known.to_string()) =>
            {
                Ok(Self::at(path))
            }
            Some(found) => Err(OpenError::UnsupportedFormat {
                path: format_path,
                found: found.to_owned(),
            }),
            None => Err(OpenError::MalformedFormat { path: format_path }),
        }
    }

    /// Opens the repository that `start` lies in: the one at `start`, as [`open`](Self::open)
    /// does, or else the nearest directory above it that contains `.stemma` (ADR-009). The
    /// result names the directory of `start` below the root, so that a path that a person gives
    /// relative to `start` can be turned into one relative to the root
    /// ([`normalize_path_in`]).
    ///
    /// Only a missing `.stemma` at `start` leads the search upwards. Anything else at `start`
    /// that is in the way, such as a `.stemma` that is a file or has a bad format, is the error
    /// of [`open`](Self::open), and so is anything wrong with the repository that the search
    /// finds. The search goes along the directories of the path of `start` with its links
    /// resolved, so a repository above it is found under its real path, and
    /// [`Discovered::through_link`] says whether that path left the path as it was given.
    /// [`Discovered::nested_git`] names, as a path with its links resolved, the nearest Git
    /// checkout between the start and the root, if there is one.
    ///
    /// A `.stemma` that the search finds must be one that the user who runs the command can
    /// trust. Otherwise a user could put a `.stemma` into a directory that others share, such as
    /// `/tmp`, and a command that runs in it or below it would work in that repository: a
    /// snapshot would save the files that its tree names into the store of the other user. On
    /// Unix the effective user of the process must own it ([`OpenError::OtherOwner`]), at `start`
    /// as well as above it, since `-C` defaults to the current directory and `cd /tmp` is as
    /// common as a directory below it. On Windows, where `std` gives no owner, a `.stemma` above
    /// `start` must not be at the root of a volume, where every user may create directories
    /// ([`OpenError::AtVolumeRoot`]); a `.stemma` at `start` is opened there without a check, as
    /// the root of a volume is only reached by naming it (ADR-009).
    ///
    /// # Errors
    ///
    /// Returns [`OpenError::Missing`] if `start` does not exist,
    /// [`OpenError::NotADirectoryToStartIn`] if it is not a directory,
    /// [`OpenError::NoRepositoryAbove`] if neither `start` nor a directory above it contains
    /// `.stemma`, [`OpenError::OtherOwner`] or [`OpenError::AtVolumeRoot`] as above,
    /// [`OpenError::NotUtf8`] if the path from the root to `start` has a part that is not UTF-8,
    /// [`OpenError::Io`] if the path cannot be resolved or a directory cannot be inspected, and
    /// the errors of [`open`](Self::open).
    pub fn discover(start: impl AsRef<Path>) -> Result<Discovered, OpenError> {
        Self::discover_with(start.as_ref(), &trusted)
    }

    /// [`discover`](Self::discover) with the check that a `.stemma` that it finds may be used,
    /// so that a test can stand in for another owner.
    fn discover_with(
        start: &Path,
        trusted: &dyn Fn(&Path) -> Result<(), OpenError>,
    ) -> Result<Discovered, OpenError> {
        let at_start = start.join(METADATA_DIR);
        // Before the open, so that nothing of another user's `.stemma` is read, such as a
        // `format` that is a FIFO and would block. Anything else in the way is the error of `open`.
        if CHECK_AT_START && fs::symlink_metadata(&at_start).is_ok() {
            trusted(&at_start)?;
        }
        match Self::open(start) {
            Err(OpenError::NotARepository { .. }) => {}
            opened => {
                // And after an open that succeeded, so that a `.stemma` that appeared after the
                // first look, or one that the first look could not inspect, is not used unchecked.
                if CHECK_AT_START && opened.is_ok() {
                    trusted(&at_start)?;
                }
                return opened.map(|repository| Discovered {
                    repository,
                    subdirectory: String::new(),
                    nested_git: None,
                    // `-C ..` from a link opens the directory above its target.
                    through_link: fs::canonicalize(start)
                        .is_ok_and(|real| !lies_below(start, &real)),
                });
            }
        }
        let real = match fs::canonicalize(start) {
            Ok(real) => real,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(OpenError::Missing {
                    path: start.to_path_buf(),
                })
            }
            Err(error) => return Err(OpenError::io("resolve the path of", start)(error)),
        };
        let metadata = fs::metadata(&real).map_err(OpenError::io("inspect", &real))?;
        if !metadata.is_dir() {
            return Err(OpenError::NotADirectoryToStartIn {
                path: start.to_path_buf(),
            });
        }
        // The nearest directory on the way up, from the start to just below the root, that holds
        // a `.git` (a file in a Git worktree, a directory in a nested clone).
        let mut nested_git = holds_git_entry(&real).then(|| real.clone());
        for root in real.ancestors().skip(1) {
            let metadata_dir = root.join(METADATA_DIR);
            match fs::symlink_metadata(&metadata_dir) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if nested_git.is_none() && holds_git_entry(root) {
                        nested_git = Some(root.to_path_buf());
                    }
                    continue;
                }
                Err(error) => return Err(OpenError::io("inspect", &metadata_dir)(error)),
            }
            trusted(&metadata_dir)?;
            let below = real
                .strip_prefix(root)
                .expect("an ancestor of a path is a prefix of it");
            let parts = below
                .components()
                .map(|component| component.as_os_str().to_str())
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| OpenError::NotUtf8 { path: real.clone() })?;
            let repository = Self::open(root)?;
            return Ok(Discovered {
                repository,
                subdirectory: parts.join("/"),
                nested_git,
                through_link: !lies_below(start, root),
            });
        }
        Err(OpenError::NoRepositoryAbove {
            path: start.to_path_buf(),
        })
    }

    fn at(path: &Path) -> Self {
        let metadata_dir = path.join(METADATA_DIR);
        Self {
            path: path.to_path_buf(),
            objects: FsObjectStore::new(&metadata_dir),
            refs: FsRefStore::new(&metadata_dir),
            oplog: FsOpLog::new(metadata_dir),
            take_over: None,
        }
    }

    /// The directory that contains `.stemma/`, as it was given to [`init`](Self::init) or
    /// [`open`](Self::open), or to [`discover`](Self::discover) when the repository is at the
    /// start. A relative path stays relative, so it follows a change of the working directory of
    /// the process. A repository that [`discover`](Self::discover) found above its start has
    /// the absolute path with its links resolved.
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

    /// Checks that a detached HEAD, the working-copy commit, every branch, and every tag name a
    /// commit that the object store of this repository has. See [`FsRefStore::verify`].
    ///
    /// # Errors
    ///
    /// See [`FsRefStore::verify`].
    pub fn verify_refs(&self) -> Result<Vec<RefVerifyError>, RefError> {
        self.refs.verify(&self.objects)
    }

    /// The store of the references, for the code of this crate that moves several pointers at once.
    pub(crate) fn refs_store(&self) -> &FsRefStore {
        &self.refs
    }

    /// The operation log of this repository. It can be read; the operations that change the
    /// repository add the entries, through `append` and `append_purge`, which take the lock of the
    /// references, and `init` writes the first one.
    pub fn oplog(&self) -> &FsOpLog {
        &self.oplog
    }

    /// Writes the entry of a change, as [`FsOpLog::append`] does, after it moved the repository
    /// to the current format; the caller holds the lock of the references. A repository of
    /// version 2 or 3 moves with its first new entry, which starts the chain (ADR-011). A
    /// repository whose file `working-copy` names no owner, as one from before version 5, gets
    /// the actor of the change as its owner after the entry is written, so the entry names the
    /// actor that it made the owner (ADR-010). The owner is recorded only once the entry stands,
    /// so a failed entry leaves no owner behind that no entry names. If the owner cannot be
    /// recorded, the entry stays valid and the working copy stays without an owner, as in
    /// version 4: the next entry records it. That costs one read of `working-copy` per entry.
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::Io`] if the format file cannot be raised and flushed, and what
    /// [`FsOpLog::append`] returns. No entry is written then. The format file holds the old
    /// version if the raise failed, unless that could not be put back either (see
    /// [`raise_format`]); once it is raised, it stays raised, even if the entry fails after it.
    pub(crate) fn append(&self, lock: &WriteLock, change: &Change) -> Result<Entry, OpLogError> {
        raise_format(&self.metadata_dir(), FORMAT_VERSION).map_err(OpLogError::from_store)?;
        let entry = self.oplog.append(change)?;
        // Not an error of the change: the entry and its move stand without the owner.
        let _ = self.refs.adopt_owner(lock, change.actor());
        Ok(entry)
    }

    /// Writes the entries of a purge, as [`FsOpLog::append_purge`] does. The purge holds the lock
    /// of the references from its plan to its end, which this value proves.
    ///
    /// # Errors
    ///
    /// See [`FsOpLog::append_purge`].
    pub(crate) fn append_purge(
        &self,
        _lock: &PurgeLock,
        purge: Purge,
    ) -> Result<Vec<Entry>, OpLogError> {
        self.oplog.append_purge(purge)
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
        let allowed = self.authorize(actor, authorizer, operation, name.as_str())?;
        let lock = self.refs.lock_writer(&allowed)?;
        let outcome =
            self.refs
                .update_branch_after(&lock, name, expected, new, &self.objects, || {
                    let change = Change::try_new(
                        now(),
                        actor.clone(),
                        operation.clone(),
                        None,
                        None,
                        vec![(name.clone(), Moved::from_to(expected, Some(new)))],
                    )
                    .map_err(OpLogError::Entry)?;
                    self.append(&lock, &change)
                })?;
        outcome.into_result()
    }

    /// Creates the tag `name` at `commit` and records it in the log, in the order of
    /// [`update_branch`](Self::update_branch) and under the same lock: the tag is checked (a name
    /// that is in the way, a tag that exists, a commit that is not in the object store), the entry
    /// is written and flushed, and only then is the tag made (ADR-008, ADR-011). The old value of
    /// the tag in the entry is `-`, because a tag never moves. The entry says who set the tag
    /// and when.
    ///
    /// Returns the entry.
    ///
    /// # Errors
    ///
    /// Returns [`UpdateError::Denied`] if the authorizer refuses, [`UpdateError::Refs`] for what
    /// the checks find ([`RefError::TagExists`] for a tag that exists, at this commit or at
    /// another; [`RefError::Locked`]), and [`UpdateError::Log`], [`UpdateError::MoveFailed`], and
    /// [`UpdateError::NotDurable`], as for [`update_branch`](Self::update_branch). A refusal
    /// writes no entry.
    pub fn create_tag(
        &self,
        operation: &OperationName,
        name: &BranchName,
        commit: ObjectId,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Entry, UpdateError> {
        // A claim only: the store confirms the kind under the lock, and an ID that names no commit
        // is refused there (`RefError::MissingCommit`, `RefError::NotACommit`).
        let commit = CommitId::new(commit);
        let allowed = self.authorize(actor, authorizer, operation, name.as_str())?;
        let lock = self.refs.lock_writer(&allowed)?;
        let outcome = self
            .refs
            .create_tag_after(&lock, name, commit, &self.objects, || {
                let change = Change::try_new_with_tags(
                    now(),
                    actor.clone(),
                    operation.clone(),
                    None,
                    None,
                    vec![],
                    vec![(name.clone(), Moved::from_to(None, Some(commit)))],
                )
                .map_err(OpLogError::Entry)?;
                self.append(&lock, &change)
            })?;
        // `create_tag_after` refuses an existing tag and never reports "unchanged".
        Ok(outcome
            .into_result()?
            .expect("create_tag_after never reports an unchanged tag"))
    }

    /// Points HEAD to a branch, which need not exist yet, or to a commit, if HEAD still is
    /// `expected`, and records the change in the log, in the order of
    /// [`update_branch`](Self::update_branch) and under the same lock. The old value in the entry
    /// is the one that the change replaces: it is read under the lock, and it is `expected`.
    ///
    /// Unlike [`RefStore::set_head`], which replaces HEAD whatever it was, this reads HEAD first,
    /// so a HEAD that is missing or damaged is an error and is not replaced.
    ///
    /// Returns the entry, or `None` if HEAD has the value already.
    ///
    /// # Errors
    ///
    /// Returns [`UpdateError::Denied`], [`UpdateError::Refs`] (a commit that is not in the store, an
    /// unreadable HEAD, [`RefError::StaleHead`], [`RefError::Locked`]), [`UpdateError::Log`],
    /// [`UpdateError::MoveFailed`], and [`UpdateError::NotDurable`], as for
    /// [`update_branch`](Self::update_branch).
    pub fn set_head(
        &self,
        operation: &OperationName,
        expected: &Head,
        head: &Head,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Option<Entry>, UpdateError> {
        let allowed = self.authorize(actor, authorizer, operation, "HEAD")?;
        let lock = self.refs.lock_writer(&allowed)?;
        let outcome = self
            .refs
            .set_head_after(&lock, expected, head, &self.objects, |old| {
                let change = Change::try_new(
                    now(),
                    actor.clone(),
                    operation.clone(),
                    Some(Moved::from_to(Some(old.clone()), Some(head.clone()))),
                    None,
                    vec![],
                )
                .map_err(OpLogError::Entry)?;
                self.append(&lock, &change)
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
        let allowed = self.authorize(actor, authorizer, operation, "working-copy")?;
        let lock = self.refs.lock_writer(&allowed)?;
        self.move_working_copy(&lock, operation, expected, new, actor)
    }

    /// The move of [`update_working_copy`](Self::update_working_copy) without the question to the
    /// authorizer, for a caller that has asked already and holds the lock.
    fn move_working_copy(
        &self,
        lock: &WriteLock,
        operation: &OperationName,
        expected: CommitId,
        new: CommitId,
        actor: &Actor,
    ) -> Result<Option<Entry>, UpdateError> {
        let outcome =
            self.refs
                .update_working_copy_after(lock, expected, new, &self.objects, || {
                    let change = Change::try_new(
                        now(),
                        actor.clone(),
                        operation.clone(),
                        None,
                        Some(Moved::from_to(Some(expected), Some(new))),
                        vec![],
                    )
                    .map_err(OpLogError::Entry)?;
                    self.append(lock, &change)
                })?;
        outcome.into_result()
    }

    /// The same repository, whose snapshots take the working copy over (ADR-010): a command of
    /// this value that takes a snapshot makes its actor the owner, with an entry of the operation
    /// `take-over`, instead of being refused as another actor. The take-over is decided before the
    /// snapshot reads a file (an agent is refused, and the authorizer is asked for `take-over`
    /// with the target `working-copy`) and made after the snapshot has read the files, just
    /// before its move, so a command whose own checks fail first does not take over. An actor that
    /// owns the working copy already takes nothing over. Taking over is level 3 of ADR-004: it
    /// decides whose changes a snapshot may save, so only a human may do it.
    ///
    /// The value shares with its clones the place where [`taken_over`](Self::taken_over) finds the
    /// entry.
    #[must_use]
    pub fn taking_over(&self) -> Self {
        Self {
            take_over: Some(Arc::new(Mutex::new(None))),
            ..self.clone()
        }
    }

    /// The entry of the take-over that a snapshot of this value made, if one did (see
    /// [`taking_over`](Self::taking_over)). It is there even if the command failed after it.
    pub fn taken_over(&self) -> Option<Entry> {
        self.take_over.as_ref().and_then(|cell| {
            cell.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
    }

    /// Checks that `actor` may save the working copy whose owner is `owner` (ADR-010), and returns
    /// whether the snapshot has to take it over first. A working copy without an owner, from
    /// before format 5, may be saved by any actor.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::NotOwner`] if another actor owns the working copy and this value
    /// does not take it over, and [`SnapshotError::AgentTakeOver`] if it would and `actor` is an
    /// agent.
    pub(crate) fn check_owner(
        &self,
        actor: &Actor,
        owner: Option<&Actor>,
    ) -> Result<bool, SnapshotError> {
        if owner == Some(actor) {
            return Ok(false);
        }
        if self.take_over.is_none() {
            return match owner {
                Some(owner) => Err(SnapshotError::NotOwner {
                    owner: Box::new(owner.clone()),
                    actor: Box::new(actor.clone()),
                }),
                None => Ok(false),
            };
        }
        if matches!(actor.view(), ActorRef::Agent { .. }) {
            return Err(SnapshotError::AgentTakeOver);
        }
        Ok(true)
    }

    /// Asks the authorizer for the take-over of the working copy, and returns its operation.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::TakeOver`] with [`UpdateError::Denied`] if it refuses.
    pub(crate) fn ask_take_over(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<OperationName, SnapshotError> {
        let operation =
            OperationName::try_new(TAKE_OVER).expect("`take-over` is a valid operation name");
        // The command holds the lock already, as `snapshot_locked` does, so only the question
        // is asked.
        let _ = self
            .authorize(actor, authorizer, &operation, "working-copy")
            .map_err(|error| SnapshotError::TakeOver(Box::new(error)))?;
        Ok(operation)
    }

    /// Makes `actor` the owner of the working copy under the lock, in the order of
    /// [`update_working_copy`](Self::update_working_copy): the entry of `take-over`, which names
    /// no pointer and has `actor` as its actor, is written and flushed, and only then does the
    /// file `working-copy` name the new owner (ADR-011). The entry goes to
    /// [`taken_over`](Self::taken_over) as soon as it is written.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::TakeOver`] with the errors of
    /// [`update_working_copy`](Self::update_working_copy).
    pub(crate) fn take_over_locked(
        &self,
        lock: &WriteLock,
        operation: &OperationName,
        actor: &Actor,
    ) -> Result<(), SnapshotError> {
        let outcome = self
            .refs
            .set_owner_after(lock, actor, || {
                let change =
                    Change::try_new(now(), actor.clone(), operation.clone(), None, None, vec![])
                        .map_err(OpLogError::Entry)?;
                let entry = self.append(lock, &change)?;
                if let Some(cell) = &self.take_over {
                    *cell
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(entry.clone());
                }
                Ok::<_, OpLogError>(entry)
            })
            .map_err(|error| SnapshotError::TakeOver(Box::new(UpdateError::Refs(error))))?;
        outcome
            .into_result()
            .map_err(|error| SnapshotError::TakeOver(Box::new(error)))?;
        Ok(())
    }

    /// Asks the authorizer, and returns the proof that it allowed some operation, which the locks of
    /// the references require.
    fn authorize(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
        operation: &OperationName,
        target: &str,
    ) -> Result<Allowed, UpdateError> {
        Allowed::ask(authorizer, actor, operation, target).ok_or_else(|| UpdateError::Denied {
            operation: operation.to_string(),
            target: target.to_owned(),
        })
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

    /// The commit of a tag, or `None` if there is no such tag. See [`RefStore::tag`].
    ///
    /// # Errors
    ///
    /// See [`RefStore::tag`].
    pub fn tag(&self, name: &BranchName) -> Result<Option<CommitId>, RefError> {
        self.refs.tag(name)
    }

    /// All tags with their commits, sorted by name. See [`RefStore::tags`].
    ///
    /// # Errors
    ///
    /// See [`RefStore::tags`].
    pub fn tags(&self) -> Result<Vec<(BranchName, CommitId)>, RefError> {
        self.refs.tags()
    }

    /// Whether a writer holds the lock of the references, so that an operation may be between its
    /// entry in the log and the move of a pointer. A lock that a crashed writer left counts as held
    /// until the next writer of this host takes it over or a person removes it (ADR-008).
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

    /// The owner of the working copy (ADR-010). See [`FsRefStore::working_copy_owner`].
    ///
    /// # Errors
    ///
    /// See [`FsRefStore::working_copy_owner`].
    pub fn working_copy_owner(&self) -> Result<Option<Actor>, RefError> {
        self.refs.working_copy_owner()
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
        flush_directory(parent).map_err(InitError::io("flush the directory", parent))?;
    }
    for created in &missing {
        flush_directory(created).map_err(InitError::io("flush the directory", created))?;
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
    _allowed: &Allowed,
) -> Result<(), InitError> {
    let result = fill(staging, options, actor).and_then(|()| move_into_place(staging, target));
    match result {
        Ok(()) => Ok(()),
        Err(error) => match fs::remove_dir_all(staging) {
            Ok(()) => Err(error),
            // The directory is gone already: a writer of a repository that another `init` made
            // here in the meantime removed it as stale (ADR-009). Nothing is left behind.
            Err(source) if source.kind() == io::ErrorKind::NotFound => Err(error),
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
        .put_tree(&Tree::try_new(Vec::new()).expect("an empty tree is valid"))
        .map_err(|error| InitError::Store(Box::new(error)))?;
    let change = ChangeId::generate().map_err(InitError::ChangeId)?;
    let commit = Commit::try_new(tree, Vec::new(), change, actor.clone(), now(), "")
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
    let init = OperationName::try_new("init").expect("`init` is a valid operation name");
    let change = Change::try_new(
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
    write_new(
        &dir.join("working-copy"),
        &encode_working_copy(&working_copy, Some(actor)),
    )?;
    write_new(&dir.join("HEAD"), &head.encode())?;
    for flushed in [&refs, &heads, &oplog, dir] {
        flush_directory(flushed).map_err(InitError::io("flush the directory", flushed))?;
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
    /// named here and can be removed by hand; the next command that writes in a repository in the
    /// same directory also removes it (ADR-009, STEMMA-31).
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

/// A repository that [`Repository::discover`] found, and where the search started in it.
#[derive(Debug)]
#[non_exhaustive]
pub struct Discovered {
    /// The repository.
    pub repository: Repository,
    /// The directory where the search started, relative to the root and with `/` between its
    /// parts, as a path of [`Repository::track`] has it: empty if the search started at the
    /// root.
    pub subdirectory: String,
    /// The nearest directory between the start and the root, the start included and the root
    /// not, that holds a `.git`: the start lies in a Git worktree or a nested clone inside the
    /// repository (STEMMA-132). The command then works in the repository of the root, and the
    /// files of that checkout appear there as paths of the root. `None` if there is none, and
    /// always if the repository is at the start.
    pub nested_git: Option<PathBuf>,
    /// Whether the search reached the repository through a link: no directory of the path of
    /// the start, as the shell reads it, is the root. A command that runs in a link to a
    /// directory of another repository then works in that one, which a person may not expect.
    /// Without the `PWD` of a shell, a link that led to the directory of the process is not seen,
    /// and a link to the root itself does not count (see the search in
    /// [`Repository::discover`]).
    pub through_link: bool,
}

/// Whether `dir` may hold a `.git`: a file (Git worktree), a directory (clone), or a link, which
/// is not followed. Only a clear `NotFound` says no; any other error is read as "present", so
/// that an entry that cannot be inspected still raises the advisory warning.
fn holds_git_entry(dir: &Path) -> bool {
    !matches!(
        fs::symlink_metadata(dir.join(".git")),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    )
}

/// Whether a directory of `start`, as the shell reads its path, is `root` once resolved: the
/// path is made absolute, and `.` and `..` are applied to its names before any link is resolved,
/// as `cd` does by default. Then a link anywhere on the way counts, also one that a `..` follows:
/// from a link `~/link` to `/b/repo/sub`, `~/link/..` is `~` for the shell and not `/b/repo`.
///
/// A relative `start` is taken from the directory of the process as the shell names it, in `PWD`,
/// if that names the same directory. Without such a `PWD`, as when a program starts the command in
/// a directory of its own, only the directory with its links resolved is known, and a link that
/// led there cannot be seen. A link to the root of a repository itself is no detour either: the
/// root is a directory of the path then.
fn lies_below(start: &Path, root: &Path) -> bool {
    let absolute = if start.is_absolute() {
        start.to_path_buf()
    } else {
        let Ok(current) = std::env::current_dir() else {
            return false;
        };
        let logical = std::env::var_os("PWD")
            .map(PathBuf::from)
            .filter(|pwd| pwd.is_absolute())
            .filter(|pwd| fs::canonicalize(pwd).ok() == fs::canonicalize(&current).ok());
        logical.unwrap_or(current).join(start)
    };
    let mut logical = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                logical.pop();
            }
            other => logical.push(other),
        }
    }
    logical
        .ancestors()
        .any(|ancestor| fs::canonicalize(ancestor).is_ok_and(|real| real == root))
}

/// Whether [`trusted`] is asked for a `.stemma` at the start of [`Repository::discover`] as well
/// as for one above it: on Unix, where it checks the owner, but not on Windows, where it only
/// refuses the root of a volume.
const CHECK_AT_START: bool = cfg!(unix);

/// Whether the `.stemma` that the search found may be used: on Unix the effective user of the
/// process owns it, as Git checks for `safe.directory`. The owner of the entry itself counts, so
/// a link that another user made is not followed.
#[cfg(unix)]
fn trusted(metadata_dir: &Path) -> Result<(), OpenError> {
    use std::os::unix::fs::MetadataExt;
    let owner = fs::symlink_metadata(metadata_dir)
        .map_err(OpenError::io("inspect", metadata_dir))?
        .uid();
    if owner == rustix::process::geteuid().as_raw() {
        Ok(())
    } else {
        Err(OpenError::OtherOwner {
            found: metadata_dir.to_path_buf(),
        })
    }
}

/// Whether the `.stemma` that the search found above the start may be used. `std` gives no
/// owner on Windows, so only the root of a volume is refused, where every user may create
/// directories by default. Other places that all users may write stay open (STEMMA-73).
#[cfg(not(unix))]
fn trusted(metadata_dir: &Path) -> Result<(), OpenError> {
    let at_volume_root = metadata_dir
        .parent()
        .is_some_and(|root| root.parent().is_none());
    if at_volume_root {
        Err(OpenError::AtVolumeRoot {
            found: metadata_dir.to_path_buf(),
        })
    } else {
        Ok(())
    }
}

/// Reasons why [`Repository::open`] and [`Repository::discover`] fail.
#[derive(Debug)]
#[non_exhaustive]
pub enum OpenError {
    /// The directory has no `.stemma`.
    NotARepository {
        /// The directory that was to be opened.
        path: PathBuf,
    },
    /// Neither the directory nor one above it has `.stemma` ([`Repository::discover`]).
    NoRepositoryAbove {
        /// The directory where the search started.
        path: PathBuf,
    },
    /// The directory where the search was to start does not exist ([`Repository::discover`]).
    Missing {
        /// The directory.
        path: PathBuf,
    },
    /// The search was to start at something that is not a directory, such as a file
    /// ([`Repository::discover`]).
    NotADirectoryToStartIn {
        /// The path.
        path: PathBuf,
    },
    /// The path from the root of the repository to the directory where the search started has a
    /// part that is not UTF-8, so no path of the tree starts there ([`Repository::discover`]).
    NotUtf8 {
        /// The directory where the search started, with its links resolved.
        path: PathBuf,
    },
    /// The search found a `.stemma` in the directory where it started or above it, but the user
    /// who runs the command does not own it, so it is not opened (Unix,
    /// [`Repository::discover`]).
    OtherOwner {
        /// The `.stemma` that was found.
        found: PathBuf,
    },
    /// The search found a `.stemma` at the root of a volume, where every user may create
    /// directories, so it is not opened (Windows, [`Repository::discover`]).
    AtVolumeRoot {
        /// The `.stemma` that was found.
        found: PathBuf,
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
            | Self::NoRepositoryAbove { .. }
            | Self::Missing { .. }
            | Self::NotADirectoryToStartIn { .. }
            | Self::NotUtf8 { .. }
            | Self::OtherOwner { .. }
            | Self::AtVolumeRoot { .. }
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
            Self::NoRepositoryAbove { path } => write!(
                f,
                "`{}` is not in a repository: there is no `{METADATA_DIR}` in it or in a directory \
                 above it",
                path.display()
            ),
            Self::Missing { path } => write!(f, "`{}` does not exist", path.display()),
            Self::NotADirectoryToStartIn { path } => write!(
                f,
                "`{}` is not a directory; name a directory of a repository",
                path.display()
            ),
            Self::NotUtf8 { path } => write!(
                f,
                "`{}` has a part that is not UTF-8 below the root of its repository, so no path of \
                 the repository starts there; run the command from a directory above it",
                path.display()
            ),
            Self::OtherOwner { found } => write!(
                f,
                "`{}` belongs to another user than the one who runs the command, so it is not \
                 used; run the command as the user who owns it",
                found.display()
            ),
            Self::AtVolumeRoot { found } => write!(
                f,
                "`{}` is at the root of a volume, where every user may create directories, so it \
                 is not used; name its directory with `-C {}` to open it anyway",
                found.display(),
                found.parent().unwrap_or(found).display()
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
                "the format file `{}` names version {found}, but this code supports versions \
                 {} to {}",
                path.display(),
                READ_VERSIONS[0],
                READ_VERSIONS[READ_VERSIONS.len() - 1]
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
            | Self::NoRepositoryAbove { .. }
            | Self::Missing { .. }
            | Self::NotADirectoryToStartIn { .. }
            | Self::NotUtf8 { .. }
            | Self::OtherOwner { .. }
            | Self::AtVolumeRoot { .. }
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
    use crate::Decision;

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &OperationName, _target: &str) -> Decision {
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
            b"stemma-repository 5\n"
        );
        assert_eq!(fs::read(meta.join("HEAD")).unwrap(), b"branch main\n");
        let first = FsOpLog::new(&meta).read(1).unwrap();
        assert_eq!(
            fs::read_to_string(meta.join("oplog-tip")).unwrap(),
            format!(
                "stemma-oplog-tip 2\nnumber 1\nhash {}\n",
                first.hash().unwrap()
            )
        );
        for directory in ["objects", "tmp", "oplog", "refs", "refs/heads"] {
            assert!(meta.join(directory).is_dir(), "{directory}");
        }
        assert_eq!(names(&meta.join("refs")), ["heads"]);
        assert_eq!(repository.path(), dir.path());
        assert_eq!(repository.metadata_dir(), meta);
    }

    #[test]
    fn the_references_of_a_new_repository_verify_and_a_removed_commit_is_found() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        assert!(repository.verify_refs().unwrap().is_empty());
        let id = repository.refs().working_copy().unwrap();
        fs::remove_file(repository.objects().object_path(&id.object_id())).unwrap();
        let errors = repository.verify_refs().unwrap();
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            crate::verify::VerifyError::Missing { at: crate::oplog::Pointer::WorkingCopy, id: missing }
                if *missing == id.object_id()
        ));
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
            format!("{id}\nowner human daniel\n").into_bytes()
        );
        let raw = a
            .objects()
            .get_expecting(&id.object_id(), crate::object::Kind::Commit)
            .unwrap()
            .unwrap();
        let commit = Commit::parse(raw.body()).unwrap();
        assert_eq!(commit.tree(), Tree::try_new(Vec::new()).unwrap().id());
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
        let commit = Commit::try_new(
            Tree::try_new(Vec::new()).unwrap().id(),
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
        OperationName::try_new(name).unwrap()
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
        let moved = entry.change().unwrap().working_copy().unwrap();
        assert_eq!((moved.before(), moved.after()), (Some(&old), Some(&new)));
        assert!(
            entry.change().unwrap().head().is_none()
                && entry.change().unwrap().branches().is_empty()
        );
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
        fs::write(dir.path().join(".stemma/refs.lock"), "other").unwrap();
        let error = repository
            .update_working_copy(&operation("snapshot"), old, new, &actor(), &DenyAll)
            .unwrap_err();
        assert!(matches!(error, UpdateError::Denied { .. }), "{error}");
        assert_eq!(
            fs::read(dir.path().join(".stemma/refs.lock")).unwrap(),
            b"other",
            "a refusal leaves a foreign lock alone"
        );
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
        let main = BranchName::try_new("main").unwrap();
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
        let options = InitOptions::new(BranchName::try_new("trunk/main").unwrap());
        let repository = Repository::init(dir.path(), &options, &actor(), &AllowAll).unwrap();
        assert_eq!(
            fs::read(dir.path().join(".stemma").join("HEAD")).unwrap(),
            b"branch trunk/main\n"
        );
        assert_eq!(
            repository.refs().head().unwrap(),
            Head::Branch(BranchName::try_new("trunk/main").unwrap())
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
            fn authorize(
                &self,
                actor: &Actor,
                operation: &OperationName,
                target: &str,
            ) -> Decision {
                self.0.borrow_mut().push((
                    format!("{actor:?}"),
                    operation.to_string(),
                    target.into(),
                ));
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
        let error = install(
            &staging,
            &target,
            &InitOptions::default(),
            &actor(),
            &Allowed::granted(),
        )
        .unwrap_err();
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
        let error = install(
            &staging,
            &target,
            &InitOptions::default(),
            &actor(),
            &Allowed::granted(),
        )
        .unwrap_err();
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
        let error = install(
            &staging,
            &target,
            &InitOptions::default(),
            &actor(),
            &Allowed::granted(),
        )
        .unwrap_err();
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
        let error = install(
            &staging,
            &target,
            &InitOptions::default(),
            &actor(),
            &Allowed::granted(),
        )
        .unwrap_err();
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
            &Allowed::granted(),
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
    fn a_change_removes_the_directory_of_an_interrupted_init() {
        // A killed `init` leaves `.stemma-init-…` next to `.stemma` (ADR-009). A change of a
        // repository there holds the lock, so no `init` can still build it, and it is removed.
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let stale = dir.path().join(".stemma-init-1-0-0123456789abcdef");
        fs::create_dir_all(stale.join("objects")).unwrap();
        fs::write(stale.join("format"), b"stemma-repository 2\n").unwrap();
        // A file with the prefix is not what `init` makes, and it stays.
        let file = dir.path().join(".stemma-init-file");
        fs::write(&file, b"x").unwrap();
        // Nor is a directory of the working copy whose name only starts like it.
        let content = dir.path().join(".stemma-init-notes");
        fs::create_dir(&content).unwrap();
        fs::write(content.join("plan.md"), b"mine").unwrap();
        let near = dir.path().join(".stemma-init-1-1-0123456789ABCDEF");
        fs::create_dir(&near).unwrap();
        // A file whose name is exactly one of `init` is no directory of `init`.
        let exact_file = dir.path().join(".stemma-init-1-2-0123456789abcdef");
        fs::write(&exact_file, b"x").unwrap();
        let old = repository.refs().working_copy().unwrap();
        let new = next_commit(&repository, "x");
        repository
            .update_working_copy(&operation("snapshot"), old, new, &actor(), &AllowAll)
            .unwrap();
        assert!(!stale.exists());
        assert!(file.exists());
        assert_eq!(fs::read(content.join("plan.md")).unwrap(), b"mine");
        assert!(near.is_dir());
        assert!(exact_file.is_file());
        assert_eq!(
            names(&dir.path().join(".stemma").join("tmp")),
            Vec::<String>::new()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_cleanup_that_cannot_remove_a_directory_does_not_stop_the_change() {
        // The cleanup is best effort (ADR-011): what cannot be removed stays, and the move is made.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let stale = dir.path().join(".stemma-init-1-0-0123456789abcdef");
        let sealed = stale.join("objects");
        fs::create_dir_all(&sealed).unwrap();
        fs::write(sealed.join("object"), b"x").unwrap();
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::remove_file(sealed.join("object")).is_ok() {
            fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipped: the permission of a directory is not enforced here (root?)");
            return;
        }
        let old = repository.refs().working_copy().unwrap();
        let new = next_commit(&repository, "x");
        let result =
            repository.update_working_copy(&operation("snapshot"), old, new, &actor(), &AllowAll);
        let left = sealed.join("object").exists();
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
        result.unwrap();
        assert_eq!(repository.refs().working_copy().unwrap(), new);
        assert!(left, "what cannot be removed stays");
    }

    #[cfg(unix)]
    #[test]
    fn the_cleanup_does_not_follow_a_link_named_like_the_directory_of_init() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let target = dir.path().join("kept");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("file"), b"x").unwrap();
        let link = dir.path().join(".stemma-init-1-0-0123456789abcdef");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let old = repository.refs().working_copy().unwrap();
        let new = next_commit(&repository, "x");
        repository
            .update_working_copy(&operation("snapshot"), old, new, &actor(), &AllowAll)
            .unwrap();
        assert!(fs::symlink_metadata(&link).is_ok(), "the link stays");
        assert!(target.join("file").exists());
    }

    #[test]
    fn an_init_whose_directory_was_removed_under_it_reports_the_failure_and_no_leftover() {
        // A writer of a repository that another `init` made here may remove the directory of a
        // running `init` as stale (ADR-009). The `init` fails, and nothing is left behind.
        let dir = TempDir::new();
        let staging = dir.path().join(".stemma-init-1-0-0123456789abcdef");
        let error = install(
            &staging,
            &dir.path().join(".stemma"),
            &InitOptions::default(),
            &actor(),
            &Allowed::granted(),
        )
        .unwrap_err();
        assert!(!matches!(error, InitError::Leftover { .. }), "{error}");
        assert!(!staging.exists());
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
    fn discover_finds_the_repository_at_the_start_or_above_it() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let found = Repository::discover(dir.path()).unwrap();
        assert_eq!(
            found.repository.path(),
            dir.path(),
            "at the start, the path stays as given"
        );
        assert_eq!(found.subdirectory, "");
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        let found = Repository::discover(dir.path().join("a/b")).unwrap();
        assert_eq!(
            found.repository.path(),
            fs::canonicalize(dir.path()).unwrap()
        );
        assert_eq!(found.subdirectory, "a/b");
        // The nearest one wins: a repository inside a repository.
        init(&dir.path().join("a")).unwrap();
        let found = Repository::discover(dir.path().join("a/b")).unwrap();
        assert_eq!(
            found.repository.path(),
            fs::canonicalize(dir.path().join("a")).unwrap()
        );
        assert_eq!(found.subdirectory, "b");
    }

    #[test]
    fn discover_does_not_search_past_a_metadata_directory_that_is_in_the_way() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/.stemma"), b"x").unwrap();
        let error = Repository::discover(dir.path().join("sub")).unwrap_err();
        assert!(matches!(error, OpenError::NotADirectory { .. }), "{error}");
        // What the search finds above is opened strictly, too.
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join(".stemma")).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let error = Repository::discover(dir.path().join("sub")).unwrap_err();
        assert!(matches!(error, OpenError::MissingFormat { .. }), "{error}");
    }

    #[test]
    fn discover_without_a_repository_says_that_it_searched_above() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let start = dir.path().join("sub");
        let error = Repository::discover(&start).unwrap_err();
        assert!(
            matches!(&error, OpenError::NoRepositoryAbove { path } if *path == start),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "`{}` is not in a repository: there is no `.stemma` in it or in a directory above \
                 it",
                start.display()
            )
        );
    }

    #[test]
    fn discover_names_a_start_that_is_missing_or_not_a_directory() {
        // Inside a repository, so that a search would find it: the message must not say that
        // there is none.
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let missing = dir.path().join("sub/missing");
        let error = Repository::discover(&missing).unwrap_err();
        assert!(
            matches!(&error, OpenError::Missing { path } if *path == missing),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!("`{}` does not exist", missing.display())
        );
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        let error = Repository::discover(&file).unwrap_err();
        assert!(
            matches!(&error, OpenError::NotADirectoryToStartIn { path } if *path == file),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "`{}` is not a directory; name a directory of a repository",
                file.display()
            )
        );
    }

    #[test]
    fn discover_checks_a_repository_above_and_on_unix_one_at_the_start() {
        // The check stands for another owner: it refuses every `.stemma` it is shown.
        let refuse = |found: &Path| {
            Err(OpenError::OtherOwner {
                found: found.to_path_buf(),
            })
        };
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let error = Repository::discover_with(&dir.path().join("sub"), &refuse).unwrap_err();
        let found = fs::canonicalize(dir.path()).unwrap().join(METADATA_DIR);
        assert!(
            matches!(&error, OpenError::OtherOwner { found: at } if *at == found),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "`{}` belongs to another user than the one who runs the command, so it is not \
                 used; run the command as the user who owns it",
                found.display()
            )
        );
        // At the start, as after `cd /tmp`, the check names the `.stemma` as it was given.
        let at_start = Repository::discover_with(dir.path(), &refuse);
        if cfg!(unix) {
            let found = dir.path().join(METADATA_DIR);
            assert!(
                matches!(&at_start, Err(OpenError::OtherOwner { found: at }) if *at == found),
                "{:?}",
                at_start.err()
            );
        } else {
            at_start.unwrap();
        }
    }

    #[test]
    fn discover_does_not_check_a_start_without_a_repository() {
        // Without `.stemma` at the start, the search goes on upwards and checks only what it
        // finds, so a refusing check that is never asked changes nothing.
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let refuse =
            |found: &Path| -> Result<(), OpenError> { panic!("asked for `{}`", found.display()) };
        let error = Repository::discover_with(&dir.path().join("sub"), &refuse).unwrap_err();
        assert!(
            matches!(error, OpenError::NoRepositoryAbove { .. }),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_repository_that_the_user_who_runs_the_command_owns_is_trusted() {
        use std::os::unix::fs::MetadataExt;
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let metadata_dir = dir.path().join(METADATA_DIR);
        trusted(&metadata_dir).unwrap();
        // `/` belongs to root, so it stands for another owner unless the tests run as root.
        let root = Path::new("/");
        if fs::metadata(root).unwrap().uid() != rustix::process::geteuid().as_raw() {
            assert!(matches!(trusted(root), Err(OpenError::OtherOwner { .. })));
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "needs root: run with `sudo -E cargo test -- --ignored`"]
    fn discover_trusts_by_the_user_who_runs_the_command_and_not_by_the_start() {
        // Changing the owner needs root, as on a machine where another user made the repository.
        use std::os::unix::fs::lchown;
        assert_eq!(
            rustix::process::geteuid().as_raw(),
            0,
            "the test needs root"
        );
        let other = Some(65534);
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        // A directory of another user in one's own repository, such as `build` after `sudo make`.
        lchown(&sub, other, other).unwrap();
        Repository::discover(&sub).unwrap();
        // A repository of another user, also when the start belongs to that user.
        lchown(dir.path().join(METADATA_DIR), other, other).unwrap();
        let error = Repository::discover(&sub).unwrap_err();
        assert!(matches!(error, OpenError::OtherOwner { .. }), "{error}");
        lchown(&sub, Some(0), Some(0)).unwrap();
        let error = Repository::discover(&sub).unwrap_err();
        assert!(matches!(error, OpenError::OtherOwner { .. }), "{error}");
        // At the start, as after `cd /tmp`, it is refused as well.
        let error = Repository::discover(dir.path()).unwrap_err();
        assert!(matches!(error, OpenError::OtherOwner { .. }), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn the_owner_of_a_found_link_counts_and_not_that_of_its_target() {
        // A link that this user made to `/`, which belongs to root: the link is trusted, as its
        // owner is this user, and the open that follows decides what it leads to.
        let dir = TempDir::new();
        let link = dir.path().join(METADATA_DIR);
        std::os::unix::fs::symlink("/", &link).unwrap();
        trusted(&link).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_repository_at_the_root_of_a_volume_is_not_trusted() {
        for root in [r"C:\.stemma", r"\\?\C:\.stemma", r"\\server\share\.stemma"] {
            assert!(
                matches!(
                    trusted(Path::new(root)),
                    Err(OpenError::AtVolumeRoot { .. })
                ),
                "{root}"
            );
        }
        trusted(Path::new(r"C:\x\.stemma")).unwrap();
        trusted(Path::new(r"\\?\C:\x\.stemma")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_at_the_metadata_directory_above_stops_the_search() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("sub")).unwrap();
        std::os::unix::fs::symlink("nowhere", dir.path().join(METADATA_DIR)).unwrap();
        let error = Repository::discover(dir.path().join("sub")).unwrap_err();
        assert!(matches!(error, OpenError::NotADirectory { .. }), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn discover_through_a_link_searches_along_the_real_path_and_says_so() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let repo = dir.path().join("repo");
        init(&repo).unwrap();
        fs::create_dir(repo.join("sub")).unwrap();
        // Without a link, the search says nothing.
        let found = Repository::discover(repo.join("sub")).unwrap();
        assert!(!found.through_link);
        // A link from outside into the repository: the repository is found, through the link.
        symlink(repo.join("sub"), dir.path().join("link")).unwrap();
        let found = Repository::discover(dir.path().join("link")).unwrap();
        assert_eq!(found.repository.path(), fs::canonicalize(&repo).unwrap());
        assert_eq!(found.subdirectory, "sub");
        assert!(found.through_link);
        // A link in the repository that leads out of it is outside it.
        fs::create_dir(dir.path().join("outside")).unwrap();
        symlink(dir.path().join("outside"), repo.join("out")).unwrap();
        let error = Repository::discover(repo.join("out")).unwrap_err();
        assert!(
            matches!(error, OpenError::NoRepositoryAbove { .. }),
            "{error}"
        );
        // A `..` after a link leads up from the link for the shell, but the system goes up from
        // its target: `link/../other` is `repo/other` for the system and a directory beside the
        // link for the shell, so the repository is reached through the link.
        fs::create_dir(repo.join("other")).unwrap();
        let found = Repository::discover(dir.path().join("link/../other")).unwrap();
        assert_eq!(found.subdirectory, "other");
        assert!(found.through_link);
        let deep = dir.path().join("deep");
        fs::create_dir(&deep).unwrap();
        symlink(repo.join("sub"), deep.join("link")).unwrap();
        let found = Repository::discover(deep.join("link/..")).unwrap();
        assert_eq!(found.repository.path(), deep.join("link/.."));
        assert!(
            found.through_link,
            "the shell reads `deep/link/..` as `deep`"
        );
    }

    #[test]
    fn discover_names_the_nearest_git_checkout_below_the_root() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        // The `.git` of the root itself is the normal case and says nothing.
        fs::create_dir(dir.path().join(".git")).unwrap();
        let task = dir.path().join("task");
        fs::create_dir_all(task.join("src")).unwrap();
        let found = Repository::discover(&task).unwrap();
        assert_eq!(found.nested_git, None);
        fs::write(task.join(".git"), "gitdir: x\n").unwrap();
        let real = fs::canonicalize(&task).unwrap();
        let found = Repository::discover(&task).unwrap();
        assert_eq!(found.nested_git, Some(real.clone()));
        let found = Repository::discover(task.join("src")).unwrap();
        assert_eq!(found.nested_git, Some(real));
        assert_eq!(found.subdirectory, "task/src");
        // At the root, there is no search and no checkout.
        assert_eq!(Repository::discover(dir.path()).unwrap().nested_git, None);
    }

    #[test]
    fn discover_names_the_nearest_of_two_nested_git_checkouts() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let outer = dir.path().join("vendor");
        let inner = outer.join("lib");
        fs::create_dir_all(inner.join("src")).unwrap();
        fs::write(outer.join(".git"), "gitdir: x\n").unwrap();
        fs::create_dir(inner.join(".git")).unwrap();
        let real_inner = fs::canonicalize(&inner).unwrap();
        // The nearest wins, from the checkout itself and from below it.
        for start in [inner.clone(), inner.join("src")] {
            let found = Repository::discover(start).unwrap();
            assert_eq!(found.nested_git, Some(real_inner.clone()));
        }
        // Without the inner one, the outer is the nearest.
        fs::remove_dir(inner.join(".git")).unwrap();
        let found = Repository::discover(&inner).unwrap();
        assert_eq!(found.nested_git, Some(fs::canonicalize(&outer).unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn discover_names_a_git_checkout_by_its_real_path_when_started_through_a_link() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let task = dir.path().join("task");
        fs::create_dir(&task).unwrap();
        fs::write(task.join(".git"), "gitdir: x\n").unwrap();
        let elsewhere = TempDir::new();
        std::os::unix::fs::symlink(&task, elsewhere.path().join("link")).unwrap();
        let found = Repository::discover(elsewhere.path().join("link")).unwrap();
        assert_eq!(found.nested_git, Some(fs::canonicalize(&task).unwrap()));
        assert!(found.through_link);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_above_the_repository_or_to_its_root_is_no_detour() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let repo = dir.path().join("data/repo");
        init(&repo).unwrap();
        fs::create_dir(repo.join("sub")).unwrap();
        // A link above the repository, as `~/proj` to `/data/proj`: the root is on the way.
        symlink(dir.path().join("data"), dir.path().join("alias")).unwrap();
        let found = Repository::discover(dir.path().join("alias/repo/sub")).unwrap();
        assert!(!found.through_link);
        assert_eq!(found.subdirectory, "sub");
        // A link to the root itself: the root is on the way as well.
        symlink(&repo, dir.path().join("root")).unwrap();
        let found = Repository::discover(dir.path().join("root/sub")).unwrap();
        assert!(!found.through_link);
        let found = Repository::discover(dir.path().join("root")).unwrap();
        assert!(!found.through_link);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn discover_refuses_a_start_whose_path_in_the_repository_is_not_utf8() {
        // A part that is not UTF-8 must not become another name, such as one with U+FFFD that a
        // directory beside it may really have.
        use std::os::unix::ffi::OsStrExt;
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let odd = dir.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
        fs::create_dir(&odd).unwrap();
        fs::create_dir(dir.path().join("caf\u{fffd}")).unwrap();
        let error = Repository::discover(&odd).unwrap_err();
        assert!(matches!(error, OpenError::NotUtf8 { .. }), "{error}");
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
            "6",
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
    fn a_repository_of_an_older_format_moves_to_the_chain_with_its_next_entry() {
        use crate::oplog::{Chain, Record};
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        // A repository of format 2: its first entry is written before the chain, and its tip
        // names no hash.
        let meta = dir.path().join(METADATA_DIR);
        let first = repository.oplog().read(1).unwrap();
        let Record::Change(change) = first.record().clone() else {
            panic!("a change");
        };
        let old = Entry::of(1, Record::Change(change)).unwrap();
        fs::write(meta.join("oplog/000000000001"), old.encode()).unwrap();
        fs::write(meta.join("oplog-tip"), "stemma-oplog-tip 1\nnumber 1\n").unwrap();
        set_format(dir.path(), "stemma-repository 2\n");
        let repository = Repository::open(dir.path()).unwrap();
        assert_eq!(repository.oplog().entries().unwrap(), [old]);

        let entry = repository
            .set_head(
                &OperationName::try_new("checkout").unwrap(),
                &repository.refs().head().unwrap(),
                &Head::Branch(BranchName::try_new("other").unwrap()),
                &actor(),
                &AllowAll,
            )
            .unwrap()
            .unwrap();
        assert_eq!(entry.chain(), Chain::Start);
        assert_eq!(
            fs::read_to_string(meta.join("format")).unwrap(),
            "stemma-repository 5\n"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), 2);
    }

    // The owner of the working copy (ADR-010)

    fn alice() -> Actor {
        Actor::human("alice").unwrap()
    }

    /// Turns a new repository into one of format 4: no owner in `working-copy`.
    fn make_format_4(dir: &Path, repository: &Repository) {
        let id = repository.refs().working_copy().unwrap();
        fs::write(
            dir.join(METADATA_DIR).join("working-copy"),
            format!("{id}\n"),
        )
        .unwrap();
        set_format(dir, "stemma-repository 4\n");
    }

    #[test]
    fn init_makes_its_actor_the_owner() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        assert_eq!(
            repository.refs().working_copy_owner().unwrap(),
            Some(actor())
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(".stemma/format")).unwrap(),
            "stemma-repository 5\n"
        );
    }

    #[test]
    fn a_repository_of_version_4_rises_with_its_next_entry_and_its_actor_becomes_the_owner() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        make_format_4(dir.path(), &repository);
        let repository = Repository::open(dir.path()).unwrap();
        assert_eq!(repository.refs().working_copy_owner().unwrap(), None);
        // Without an owner, any actor may save the working copy; nothing changed, so no entry
        // is written and the repository stays at 4.
        assert_eq!(repository.snapshot(&alice(), &AllowAll).unwrap(), None);
        assert_eq!(
            fs::read_to_string(dir.path().join(".stemma/format")).unwrap(),
            "stemma-repository 4\n"
        );

        fs::write(dir.path().join("a.txt"), "a").unwrap();
        let entry = repository
            .track(&["a.txt".to_owned()], &alice(), &AllowAll)
            .unwrap()
            .entry
            .unwrap();

        assert_eq!(entry.change().unwrap().actor(), &alice());
        assert_eq!(
            repository.refs().working_copy_owner().unwrap(),
            Some(alice())
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(".stemma/format")).unwrap(),
            "stemma-repository 5\n"
        );
        // The move of the working-copy commit kept the owner.
        let id = repository.refs().working_copy().unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".stemma/working-copy")).unwrap(),
            format!("{id}\nowner human alice\n")
        );
    }

    fn agent() -> Actor {
        Actor::agent("claude", "opus-5-5", "s1", "daniel").unwrap()
    }

    /// Allows everything but the take-over.
    struct DenyTakeOver;

    impl Authorizer for DenyTakeOver {
        fn authorize(&self, _actor: &Actor, operation: &OperationName, _target: &str) -> Decision {
            if operation.as_str() == "take-over" {
                Decision::Deny
            } else {
                Decision::Allow
            }
        }
    }

    #[test]
    fn a_snapshot_of_another_actor_is_refused_and_writes_nothing() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        let before = repository.oplog().entries().unwrap().len();

        let error = repository
            .track(&["a.txt".to_owned()], &alice(), &AllowAll)
            .unwrap_err();

        let TrackError::Snapshot(SnapshotError::NotOwner {
            owner,
            actor: refused,
        }) = &error
        else {
            panic!("refused as not the owner: {error:?}");
        };
        assert_eq!((&**owner, &**refused), (&actor(), &alice()));
        let message = error.to_string();
        assert!(
            message.starts_with("the working copy belongs to `human daniel`"),
            "{message}"
        );
        assert!(message.contains("--take-over"), "{message}");
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        assert_eq!(
            repository.refs().working_copy_owner().unwrap(),
            Some(actor())
        );
    }

    #[test]
    fn taking_over_writes_an_entry_without_a_pointer_and_makes_the_actor_the_owner() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let commit = repository.refs().working_copy().unwrap();
        let taking = repository.taking_over();

        // Nothing changed, so the snapshot writes no entry of its own; the take-over does.
        assert_eq!(taking.snapshot(&alice(), &AllowAll).unwrap(), None);

        let entry = taking.taken_over().unwrap();
        let change = entry.change().unwrap();
        assert_eq!(change.operation().as_str(), "take-over");
        assert_eq!(change.actor(), &alice());
        assert_eq!(change.head(), None);
        assert_eq!(change.working_copy(), None);
        assert!(change.branches().is_empty());
        assert_eq!(repository.oplog().read(entry.number()).unwrap(), entry);
        assert!(repository.oplog().verify().is_ok());
        assert_eq!(
            repository.refs().working_copy_owner().unwrap(),
            Some(alice())
        );
        assert_eq!(repository.refs().working_copy().unwrap(), commit);
        assert_eq!(repository.taken_over(), None);
        // The new owner may save the working copy, and the old one may not any more.
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        let tracked = repository
            .track(&["a.txt".to_owned()], &alice(), &AllowAll)
            .unwrap();
        assert_eq!(tracked.entry.unwrap().change().unwrap().actor(), &alice());
        assert!(matches!(
            repository.snapshot(&actor(), &AllowAll),
            Err(SnapshotError::NotOwner { .. })
        ));
        // Taking over what one owns already changes nothing.
        let entries = repository.oplog().entries().unwrap().len();
        let again = repository.taking_over();
        assert_eq!(again.snapshot(&alice(), &AllowAll).unwrap(), None);
        assert_eq!(again.taken_over(), None);
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[test]
    fn taking_over_asks_the_authorizer_and_a_refusal_changes_nothing() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        struct Recorder(RefCell<Vec<(Actor, String, String)>>);
        impl Authorizer for Recorder {
            fn authorize(
                &self,
                actor: &Actor,
                operation: &OperationName,
                target: &str,
            ) -> Decision {
                self.0
                    .borrow_mut()
                    .push((actor.clone(), operation.to_string(), target.into()));
                Decision::Allow
            }
        }
        let entries = repository.oplog().entries().unwrap().len();

        let error = repository
            .taking_over()
            .snapshot(&alice(), &DenyTakeOver)
            .unwrap_err();
        assert!(
            matches!(&error, SnapshotError::TakeOver(update)
                if matches!(&**update, UpdateError::Denied { operation, target }
                    if operation == "take-over" && target == "working-copy")),
            "{error:?}"
        );
        assert!(error
            .to_string()
            .starts_with("could not take the working copy over"));
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert_eq!(
            repository.refs().working_copy_owner().unwrap(),
            Some(actor())
        );

        let recorder = Recorder(RefCell::new(Vec::new()));
        repository
            .taking_over()
            .snapshot(&alice(), &recorder)
            .unwrap();
        assert_eq!(
            recorder.0.into_inner(),
            [
                (alice(), "snapshot".to_owned(), "working-copy".to_owned()),
                (alice(), "take-over".to_owned(), "working-copy".to_owned())
            ]
        );
    }

    #[test]
    fn an_agent_may_not_take_the_working_copy_over() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let entries = repository.oplog().entries().unwrap().len();

        let error = repository
            .taking_over()
            .snapshot(&agent(), &AllowAll)
            .unwrap_err();

        assert!(matches!(error, SnapshotError::AgentTakeOver), "{error:?}");
        assert!(
            error.to_string().contains("a human has to do it"),
            "{error}"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert_eq!(
            repository.refs().working_copy_owner().unwrap(),
            Some(actor())
        );
        // Without the take-over, the agent is refused as any other actor.
        assert!(matches!(
            repository.snapshot(&agent(), &AllowAll),
            Err(SnapshotError::NotOwner { .. })
        ));
    }

    #[test]
    fn a_command_whose_own_check_fails_does_not_take_over() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let entries = repository.oplog().entries().unwrap().len();
        let taking = repository.taking_over();

        let error = taking
            .track(&["missing.txt".to_owned()], &alice(), &AllowAll)
            .unwrap_err();

        assert!(
            !matches!(error, TrackError::Snapshot(SnapshotError::TakeOver(_))),
            "{error}"
        );
        assert_eq!(taking.taken_over(), None);
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert_eq!(
            repository.refs().working_copy_owner().unwrap(),
            Some(actor())
        );
    }

    #[test]
    fn a_commit_after_a_take_over_names_its_caller_and_not_the_actor_of_init() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        let taking = repository.taking_over();

        let committed = taking.commit("Add a", &alice(), &AllowAll).unwrap_err();
        // Nothing is tracked yet, so there is nothing to commit; the take-over stands.
        assert!(taking.taken_over().is_some(), "{committed}");
        repository
            .track(&["a.txt".to_owned()], &alice(), &AllowAll)
            .unwrap();
        let committed = repository.commit("Add a", &alice(), &AllowAll).unwrap();

        let named = snapshot::read_commit(repository.objects(), committed.commit).unwrap();
        assert_eq!(named.author(), &alice());
        assert_eq!(committed.entry.change().unwrap().actor(), &alice());
    }

    #[test]
    fn taking_over_a_working_copy_of_format_4_raises_the_format() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        make_format_4(dir.path(), &repository);
        let repository = Repository::open(dir.path()).unwrap().taking_over();

        repository.snapshot(&alice(), &AllowAll).unwrap();

        let entry = repository.taken_over().unwrap();
        assert_eq!(entry.change().unwrap().actor(), &alice());
        assert_eq!(
            repository.refs().working_copy_owner().unwrap(),
            Some(alice())
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(".stemma/format")).unwrap(),
            "stemma-repository 5\n"
        );
    }

    #[test]
    fn an_entry_that_fails_leaves_no_owner_that_no_entry_names() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        make_format_4(dir.path(), &repository);
        let repository = Repository::open(dir.path()).unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        // An actor too long for an entry: the snapshot gets to its entry, which is refused.
        let long = Actor::human("a".repeat(70_000)).unwrap();

        let error = repository
            .track(&["a.txt".to_owned()], &long, &AllowAll)
            .unwrap_err();

        assert!(error.to_string().contains("entry"), "{error}");
        assert_eq!(repository.refs().working_copy_owner().unwrap(), None);
        // The next entry that stands makes its actor the owner.
        repository
            .track(&["a.txt".to_owned()], &alice(), &AllowAll)
            .unwrap();
        assert_eq!(
            repository.refs().working_copy_owner().unwrap(),
            Some(alice())
        );
    }

    #[test]
    fn the_newest_take_over_is_judged_by_the_owner_and_an_older_one_is_unknown() {
        use crate::oplog::{outcomes, Current, Outcome};
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        repository
            .taking_over()
            .snapshot(&alice(), &AllowAll)
            .unwrap();
        let bob = Actor::human("bob").unwrap();
        repository.taking_over().snapshot(&bob, &AllowAll).unwrap();
        let entries = repository.oplog().entries().unwrap();
        let head = repository.refs().head().unwrap();
        let commit = repository.refs().working_copy().unwrap();
        let judge = |owner: Option<Option<Actor>>, settled: bool| {
            let current = Current::new(head.clone(), [], settled).with_working_copy(Some(commit));
            let current = match owner {
                Some(owner) => current.with_owner(owner),
                None => current,
            };
            let all = outcomes(&entries, Some(&current));
            (all[1], all[2])
        };

        assert_eq!(
            judge(Some(Some(bob.clone())), true),
            (Outcome::Unknown, Outcome::Took)
        );
        // The file names another owner: the move of the owner did not happen.
        assert_eq!(
            judge(Some(Some(alice())), true),
            (Outcome::Unknown, Outcome::NotTaken)
        );
        assert_eq!(
            judge(Some(Some(alice())), false),
            (Outcome::Unknown, Outcome::Unknown)
        );
        assert_eq!(judge(None, true), (Outcome::Unknown, Outcome::Unknown));
    }

    #[test]
    fn a_format_is_raised_and_never_lowered() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let meta = dir.path().join(METADATA_DIR);
        let read = || fs::read_to_string(meta.join("format")).unwrap();
        raise_format(&meta, 3).unwrap();
        assert_eq!(read(), "stemma-repository 5\n");
        set_format(dir.path(), "stemma-repository 2\n");
        raise_format(&meta, 3).unwrap();
        assert_eq!(read(), "stemma-repository 3\n");
        raise_format(&meta, 4).unwrap();
        assert_eq!(read(), "stemma-repository 4\n");
        raise_format(&meta, 5).unwrap();
        assert_eq!(read(), "stemma-repository 5\n");
    }

    #[test]
    fn a_format_whose_directory_cannot_be_flushed_is_put_back_and_raised_again() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let meta = dir.path().join(METADATA_DIR);
        let read = || fs::read_to_string(meta.join("format")).unwrap();
        set_format(dir.path(), "stemma-repository 2\n");
        let failing = |dir: &Path| {
            Err(StoreError::Io {
                operation: "flush the directory",
                path: dir.to_path_buf(),
                source: io::Error::other("injected"),
            })
        };
        match raise_format_with(&meta, 3, &failing) {
            Err(StoreError::Io {
                operation, path, ..
            }) => {
                assert_eq!(operation, "flush the directory");
                assert_eq!(path, meta);
            }
            other => panic!("expected the error of the flush, got {other:?}"),
        }
        // The old version is back, and no temporary file stays.
        assert_eq!(read(), "stemma-repository 2\n");
        assert_eq!(fs::read_dir(meta.join("tmp")).unwrap().count(), 0);
        // The next call does not take the format for raised: it raises it and flushes.
        let flushed = std::cell::Cell::new(0);
        let counting = |dir: &Path| {
            flushed.set(flushed.get() + 1);
            flush_store_directory(dir)
        };
        raise_format_with(&meta, 3, &counting).unwrap();
        assert_eq!(read(), "stemma-repository 3\n");
        assert_eq!(flushed.get(), 1);
    }

    #[test]
    fn open_reads_versions_2_to_4_and_init_writes_version_5() {
        let dir = TempDir::new();
        init(dir.path()).unwrap();
        let format = fs::read_to_string(dir.path().join(".stemma/format")).unwrap();
        assert_eq!(format, "stemma-repository 5\n");
        for version in ["2", "3", "4"] {
            set_format(dir.path(), format!("stemma-repository {version}\n"));
            Repository::open(dir.path()).unwrap();
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
                "stemma-repository 5",
            ),
            (
                OpenError::UnsupportedFormat {
                    path: path.clone(),
                    found: "7".to_string(),
                },
                "version 7, but this code supports versions 2 to 5",
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
        OperationName::try_new(name).unwrap()
    }

    fn branch(name: &str) -> BranchName {
        BranchName::try_new(name).unwrap()
    }

    /// Stores a commit with no parents and returns its ID.
    fn store_commit(repository: &Repository, message: &str) -> CommitId {
        let tree = repository
            .objects()
            .put_tree(&Tree::try_new(vec![]).unwrap())
            .unwrap();
        let commit = Commit::try_new(tree, vec![], sample_change(), actor(), 1, message).unwrap();
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
        let change = entry.change().unwrap();
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
        assert_eq!(entry.change().unwrap().actor(), &agent);
        assert_eq!(
            entry.change().unwrap().head().unwrap().after(),
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
            let change = entry.change().unwrap();
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
    fn a_tag_is_made_after_its_entry_and_the_entry_says_who_set_it() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let a = store_commit(&repository, "a");
        let before = numbers(&repository);
        let entry = repository
            .create_tag(
                &op("tag"),
                &branch("v1"),
                a.object_id(),
                &actor(),
                &AllowAll,
            )
            .unwrap();
        assert_eq!(repository.refs().tag(&branch("v1")).unwrap(), Some(a));
        assert_eq!(numbers(&repository).len(), before.len() + 1);
        let change = entry.change().unwrap();
        assert_eq!(change.operation().as_str(), "tag");
        assert_eq!(change.actor(), &actor());
        assert!(change.head().is_none() && change.working_copy().is_none());
        assert!(change.branches().is_empty());
        let [(name, moved)] = change.tags() else {
            panic!("one tag expected");
        };
        assert_eq!(name, &branch("v1"));
        assert_eq!((moved.before(), moved.after()), (None, Some(&a)));
        assert_eq!(repository.oplog().entries().unwrap().last(), Some(&entry));
    }

    #[test]
    fn a_tag_that_exists_is_refused_without_an_entry_whatever_the_commit() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let (a, b) = (
            store_commit(&repository, "a"),
            store_commit(&repository, "b"),
        );
        let tag = |commit: CommitId| {
            repository.create_tag(
                &op("tag"),
                &branch("v1"),
                commit.object_id(),
                &actor(),
                &AllowAll,
            )
        };
        tag(a).unwrap();
        let before = numbers(&repository);
        for commit in [a, b] {
            let error = tag(commit).unwrap_err();
            assert!(
                matches!(&error, UpdateError::Refs(RefError::TagExists { found, .. }) if *found == a),
                "{error}"
            );
        }
        assert_eq!(numbers(&repository), before);
        assert_eq!(repository.refs().tag(&branch("v1")).unwrap(), Some(a));
    }

    #[test]
    fn a_tag_at_a_missing_commit_or_a_refused_tag_writes_no_entry() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let elsewhere = TempDir::new();
        let foreign = store_commit(&init(elsewhere.path()).unwrap(), "elsewhere");
        let a = store_commit(&repository, "a");
        let before = numbers(&repository);
        let error = repository
            .create_tag(
                &op("tag"),
                &branch("v1"),
                foreign.object_id(),
                &actor(),
                &AllowAll,
            )
            .unwrap_err();
        assert!(
            matches!(error, UpdateError::Refs(RefError::MissingCommit(_))),
            "{error}"
        );
        let error = repository
            .create_tag(&op("tag"), &branch("v1"), a.object_id(), &actor(), &DenyAll)
            .unwrap_err();
        assert!(matches!(error, UpdateError::Denied { .. }), "{error}");
        assert_eq!(numbers(&repository), before);
        assert_eq!(repository.refs().tag(&branch("v1")).unwrap(), None);
    }

    #[test]
    fn a_refused_operation_writes_no_entry_and_does_not_move() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let a = store_commit(&repository, "a");
        let before = numbers(&repository);
        fs::write(dir.path().join(".stemma/refs.lock"), "other").unwrap();
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
                &repository.refs().head().unwrap(),
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
        assert_eq!(
            fs::read(dir.path().join(".stemma/refs.lock")).unwrap(),
            b"other",
            "a refusal leaves a foreign lock alone"
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
            &repository.refs().head().unwrap(),
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
                &repository.refs().head().unwrap(),
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
        let [(name, pair)] = read.change().unwrap().branches() else {
            panic!("one branch")
        };
        assert_eq!(name, &branch("main"));
        assert_eq!((pair.before(), pair.after()), (None, Some(&a)));
        assert_eq!(repository.refs().branch(&branch("main")).unwrap(), None);
    }

    #[test]
    fn a_tag_that_fails_after_the_entry_leaves_the_entry_and_a_retry_makes_the_tag() {
        use crate::oplog::{outcomes, Current, Outcome};
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let a = store_commit(&repository, "a");
        let tags = dir.path().join(".stemma").join("refs").join("tags");
        fs::create_dir_all(&tags).unwrap();
        fs::set_permissions(&tags, fs::Permissions::from_mode(0o555)).unwrap();
        let writable = fs::File::create(tags.join("probe")).is_ok();
        let tag = || {
            repository.create_tag(
                &op("tag"),
                &branch("v1"),
                a.object_id(),
                &actor(),
                &AllowAll,
            )
        };
        let result = tag();
        fs::set_permissions(&tags, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_file(tags.join("probe"));
        if writable {
            eprintln!("skipped: the permission of a directory is not enforced here (root?)");
            return;
        }
        // The entry is written first; the file is not, and the error carries the entry.
        let error = result.unwrap_err();
        assert!(matches!(&error, UpdateError::MoveFailed { .. }), "{error}");
        assert_eq!(error.entry().map(Entry::number), Some(2));
        assert_eq!(numbers(&repository), [1, 2]);
        assert_eq!(repository.refs().tag(&branch("v1")).unwrap(), None);

        // The log says that the tag was not made, and a retry makes it with a second entry.
        let judge = |entries: &[Entry]| {
            let current = Current::new(Head::Branch(branch("main")), [], true)
                .with_tags(repository.refs().tags().unwrap());
            outcomes(entries, Some(&current))
        };
        let entries = repository.oplog().entries().unwrap();
        assert_eq!(judge(&entries[1..]), [Outcome::NotTaken]);
        let retry = tag().unwrap();
        assert_eq!(retry.number(), 3);
        assert_eq!(repository.refs().tag(&branch("v1")).unwrap(), Some(a));
        let entries = repository.oplog().entries().unwrap();
        assert_eq!(judge(&entries[1..]), [Outcome::NotTaken, Outcome::Took]);
    }

    #[test]
    fn a_tag_name_in_the_way_is_refused_without_an_entry() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let a = store_commit(&repository, "a");
        let tag = |name: &str| {
            repository.create_tag(
                &op("tag"),
                &branch(name),
                a.object_id(),
                &actor(),
                &AllowAll,
            )
        };
        tag("release/1").unwrap();
        let before = numbers(&repository);
        let error = tag("release").unwrap_err();
        assert!(
            matches!(&error, UpdateError::Refs(RefError::Conflict { .. })),
            "{error}"
        );
        assert_eq!(numbers(&repository), before);
        assert_eq!(repository.refs().tag(&branch("release")).unwrap(), None);
    }

    #[test]
    fn the_outcomes_of_a_step_that_ran_become_errors_that_carry_the_entry() {
        let entry = || {
            Entry::try_new(
                9,
                Change::try_new(
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
                                &repository.refs().head().unwrap(),
                                &Head::Branch(name.clone()),
                                &actor(),
                                &AllowAll,
                            ) {
                                Ok(entry) => return entry.expect("every value is new"),
                                // Another writer holds the lock, or finished a change between the
                                // read of HEAD and the lock: read again and try again.
                                Err(UpdateError::Refs(
                                    RefError::Locked { .. } | RefError::StaleHead { .. },
                                )) => {
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
            let head = entry.change().unwrap().head().unwrap();
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
                &Head::Branch(branch("main")),
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
                &repository.refs().head().unwrap(),
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
                &repository.refs().head().unwrap(),
                &Head::Branch(branch("dev")),
                &actor(),
                &AllowAll,
            )
            .unwrap()
            .unwrap();
        let detached = repository
            .set_head(
                &op("checkout"),
                &repository.refs().head().unwrap(),
                &Head::Detached(a),
                &actor(),
                &AllowAll,
            )
            .unwrap()
            .unwrap();
        assert_eq!(repository.refs().head().unwrap(), Head::Detached(a));
        let first = to_branch.change().unwrap().head().unwrap();
        assert_eq!(first.before(), Some(&Head::Branch(branch("main"))));
        assert_eq!(first.after(), Some(&Head::Branch(branch("dev"))));
        let second = detached.change().unwrap().head().unwrap();
        assert_eq!(second.before(), Some(&Head::Branch(branch("dev"))));
        assert_eq!(second.after(), Some(&Head::Detached(a)));
        // The commit that HEAD named is a root of the log.
        assert!(detached.change().unwrap().commits().contains(&a));
    }

    #[test]
    fn a_head_that_is_unchanged_or_names_a_missing_commit_writes_no_entry() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let before = numbers(&repository);
        let same = repository
            .set_head(
                &op("checkout"),
                &repository.refs().head().unwrap(),
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
                &repository.refs().head().unwrap(),
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
    fn set_head_from_a_value_that_is_not_the_current_one_changes_nothing_and_logs_nothing() {
        let dir = TempDir::new();
        let repository = init(dir.path()).unwrap();
        let a = store_commit(&repository, "a");
        let seen = repository.refs().head().unwrap();
        repository
            .set_head(
                &op("checkout"),
                &seen,
                &Head::Detached(a),
                &actor(),
                &AllowAll,
            )
            .unwrap()
            .unwrap();
        let before = numbers(&repository);
        // A second caller still holds `seen`: its change must not replace the first one.
        let error = repository
            .set_head(
                &op("checkout"),
                &seen,
                &Head::Branch(branch("x")),
                &actor(),
                &AllowAll,
            )
            .unwrap_err();
        assert!(
            matches!(
                &error,
                UpdateError::Refs(RefError::StaleHead { expected, found })
                    if expected == &seen && found == &Head::Detached(a)
            ),
            "{error}"
        );
        assert_eq!(repository.refs().head().unwrap(), Head::Detached(a));
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
        assert_eq!(
            entry.change().unwrap().branches()[0].1.after(),
            Some(&moved)
        );
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
            &Allowed::granted(),
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
        assert_eq!(OperationName::try_new("init").unwrap().as_str(), "init");
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
