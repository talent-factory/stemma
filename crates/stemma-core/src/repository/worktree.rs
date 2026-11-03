//! Linked worktrees: `worktree add` and `worktree list` (ADR-009, ADR-010).
//!
//! The shared store (`objects/`, `refs/`, `oplog/`, the lock) stays in the `.stemma` of the main
//! worktree. A linked worktree has a state directory `.stemma/worktrees/<name>/` with its own
//! `HEAD` and `working-copy` and a file `location` that holds the absolute path of the worktree.
//! The directory of the worktree holds a file `.stemma` (the pointer file) with the absolute path of
//! the state directory. Opening the worktree reads the pointer file and checks that the file
//! `location` of the state directory names the directory that holds the pointer file, so a
//! worktree that was moved or copied is refused with a clear message.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::checkout::{CheckoutError, CheckoutPlan};
use super::snapshot::{read_commit, SnapshotError};
use super::{exists, now, OpenError, Repository, UpdateError, METADATA_DIR, WORKTREES_DIR};
use crate::change::{ChangeId, GenerateError};
use crate::fault;
use crate::object::{Commit, CommitId};
use crate::oplog::{Change, Entry, Moved, OpLogError, OperationName};
use crate::refs::{
    BranchName, Head, Logged, NewWorktree, NewWorktreeBranch, RefError, LOCATION_FILE,
    MAIN_WORKTREE,
};
use crate::store::{ObjectStore, StoreError};
use crate::{Actor, Authorizer};

/// The operation that the log and the authorizer know for `worktree add`. The name of an operation
/// holds no space, so the subcommand is joined with a hyphen.
const OPERATION: &str = "worktree-add";

/// What the pointer file `.stemma` of a linked worktree starts with.
pub(super) const POINTER_PREFIX: &str = "stemma-worktree ";

/// The longest pointer file that is read. A longer file is not in canonical form anyway.
const POINTER_MAX_LEN: u64 = 4096;

/// The branch that `worktree add` checks out in the new worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeBranch {
    /// An existing branch that no worktree has checked out.
    Existing(BranchName),
    /// A new branch (`-b`), made at the commit that HEAD of the calling worktree leads to.
    Create(BranchName),
}

impl WorktreeBranch {
    fn name(&self) -> &BranchName {
        match self {
            Self::Existing(name) | Self::Create(name) => name,
        }
    }
}

/// What `worktree add` made.
#[derive(Debug)]
#[must_use]
pub struct AddedWorktree {
    /// The entry of the log.
    pub entry: Box<Entry>,
    /// The name of the worktree: the name of its state directory.
    pub name: String,
    /// The directory of the worktree, absolute and with its links resolved.
    pub path: PathBuf,
    /// The branch that HEAD of the worktree names.
    pub branch: BranchName,
    /// Whether the call made the branch.
    pub created: bool,
    /// The commit at the tip of the branch, the parent of the working-copy commit.
    pub commit: CommitId,
    /// The working-copy commit of the new worktree: empty, on the tip.
    pub working_copy: CommitId,
    /// The owner of the working copy.
    pub owner: Actor,
}

/// One row of `worktree list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// The name: `main` for the main worktree, else the name of the state directory.
    pub name: String,
    /// The directory of the worktree. It need not exist: a worktree whose directory was removed
    /// is still listed, with the path that it had.
    pub path: PathBuf,
    /// What HEAD of the worktree names.
    pub head: Head,
    /// The owner of the working copy, or `None` for one from before format 5.
    pub owner: Option<Actor>,
}

impl Repository {
    /// Adds a linked worktree in `path` that has the branch checked out (ADR-009).
    ///
    /// The steps are: the authorizer is asked for `worktree-add` with the path as the target; the
    /// path is made absolute and checked (it must not be, lie in, or hold a worktree, and if it
    /// exists it is an empty directory); the lock of the references is taken and held to the last
    /// pointer; the branch is read again, and refused if another worktree has it checked out or, for
    /// a new branch, if it exists; the state directory gets a name from the last part of the path;
    /// the working-copy commit is made, empty and on the tip, with a change ID of its own and
    /// `owner` as its author; the plan of the files is made; the entry is written and flushed; the
    /// files are written; the pointers are placed: the new branch, the state directory with
    /// `location`, `working-copy` and `HEAD`, and the pointer file `.stemma` last. The calling
    /// worktree is not read and not saved.
    ///
    /// `owner` is the actor of the caller unless another is given.
    ///
    /// # Errors
    ///
    /// Before the entry: [`WorktreeError::Update`] for a refusal of the question, [`WorktreeError::PathNotUtf8`],
    /// [`WorktreeError::PathInUse`], [`WorktreeError::Nested`], [`WorktreeError::NoSuchBranch`],
    /// [`WorktreeError::NoStart`], [`WorktreeError::Refs`] (among them [`RefError::CheckedOut`]
    /// and [`RefError::BranchExists`]), [`WorktreeError::Checkout`], and the errors of the
    /// storage. The directory that the call made is removed again. After the entry:
    /// [`WorktreeError::CheckoutFailed`], [`WorktreeError::MoveFailed`],
    /// [`WorktreeError::PartlyMoved`], and [`WorktreeError::NotDurable`].
    pub fn add_worktree(
        &self,
        path: &Path,
        branch: &WorktreeBranch,
        owner: Option<&Actor>,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<AddedWorktree, WorktreeError> {
        let operation =
            OperationName::try_new(OPERATION).expect("`worktree-add` is a valid operation name");
        self.refs_store()
            .refuse_pending_purge()
            .map_err(WorktreeError::Refs)?;
        let allowed = self
            .authorize(actor, authorizer, &operation, &path.display().to_string())
            .map_err(WorktreeError::Update)?;
        let owner = owner.unwrap_or(actor);
        let location = resolve(path)?;
        let text = location
            .to_str()
            .ok_or_else(|| WorktreeError::PathNotUtf8(location.clone()))?
            .to_owned();
        self.refuse_overlap(&location)?;
        let made = prepare_directory(&location)?;
        let result = self.add_in(&location, &text, branch, owner, actor, &operation, &allowed);
        if let (Err(error), Some(made)) = (&result, &made) {
            if error.entry().is_none() {
                // Nothing of the add was written: the directory that this call made goes again. A
                // directory that cannot be removed stays, and the message names the add only.
                let _ = fault::remove_dir_all(made);
            }
        }
        result
    }

    /// The part of [`add_worktree`](Self::add_worktree) that runs under the lock.
    #[allow(clippy::too_many_arguments)]
    fn add_in(
        &self,
        location: &Path,
        text: &str,
        branch: &WorktreeBranch,
        owner: &Actor,
        actor: &Actor,
        operation: &OperationName,
        allowed: &crate::Allowed,
    ) -> Result<AddedWorktree, WorktreeError> {
        let lock = self
            .refs_store()
            .lock_writer(allowed)
            .map_err(WorktreeError::Refs)?;
        let name = self.free_name(location)?;
        // The pointer file holds this path, so it is absolute and has no link in it.
        let shared = resolve(&self.metadata_dir())?;
        let state = shared.join(WORKTREES_DIR).join(&name);
        let (target, tip) = self.start_of(branch)?;
        let tree = read_commit(self.objects(), tip)
            .map_err(WorktreeError::Read)?
            .tree();
        let working_copy = Commit::try_new(
            tree,
            vec![tip],
            ChangeId::generate().map_err(WorktreeError::ChangeId)?,
            owner.clone(),
            now(),
            "",
        )
        .expect("a single parent is not repeated");
        let working_copy = self
            .objects()
            .put_commit(&working_copy)
            .map_err(|error| WorktreeError::Store(Box::new(error)))?;
        // The view of the new worktree, for its files: nothing is read from its pointer file.
        let view = Repository::linked(location, &shared, &name);
        let plan: CheckoutPlan = view
            .plan_checkout(None, tree)
            .map_err(WorktreeError::Checkout)?;
        let new = NewWorktree {
            name: &name,
            location: text,
            branch: target,
            working_copy,
            owner,
        };
        let created = matches!(branch, WorktreeBranch::Create(_));
        let logged = self.refs_store().add_worktree_after(
            &lock,
            &new,
            self.objects(),
            || {
                let change = Change::try_new(
                    now(),
                    actor.clone(),
                    operation.clone(),
                    Some(Moved::from_to(
                        None,
                        Some(Head::Branch(branch.name().clone())),
                    )),
                    Some(Moved::from_to(None, Some(working_copy))),
                    if created {
                        vec![(branch.name().clone(), Moved::from_to(None, Some(tip)))]
                    } else {
                        Vec::new()
                    },
                )
                .map_err(|error| {
                    WorktreeError::Update(UpdateError::Log(OpLogError::Entry(error)))
                })?;
                let entry = self
                    .append(&lock, &change)
                    .map_err(|error| WorktreeError::Update(UpdateError::Log(error)))?;
                if let Err(source) = view.apply_checkout(&plan) {
                    return Err(WorktreeError::CheckoutFailed {
                        entry: Box::new(entry),
                        source,
                    });
                }
                Ok(entry)
            },
            || write_pointer(location, &state),
        );
        let added = |entry: Entry| AddedWorktree {
            entry: Box::new(entry),
            name: name.clone(),
            path: location.to_path_buf(),
            branch: branch.name().clone(),
            created,
            commit: tip,
            working_copy,
            owner: owner.clone(),
        };
        match logged.map_err(WorktreeError::Refs)? {
            Logged::Unchanged => unreachable!("a new worktree always changes the pointers"),
            Logged::Moved(entry) => Ok(added(entry)),
            Logged::StepFailed(error) => Err(error),
            Logged::MoveFailed(entry, source) => Err(WorktreeError::MoveFailed {
                entry: Box::new(entry),
                source,
            }),
            Logged::PartlyMoved(entry, source) => Err(WorktreeError::PartlyMoved {
                entry: Box::new(entry),
                source,
            }),
            Logged::NotFlushed(entry, source) => Err(WorktreeError::NotDurable {
                entry: Box::new(entry),
                source,
            }),
        }
    }

    /// The branch for the refs layer and the commit it starts at. An existing branch starts at its
    /// tip; a new one at the commit that HEAD of this worktree leads to.
    fn start_of(
        &self,
        branch: &WorktreeBranch,
    ) -> Result<(NewWorktreeBranch, CommitId), WorktreeError> {
        let refs = self.refs();
        let tip = match branch {
            WorktreeBranch::Existing(name) => refs
                .branch(name)
                .map_err(WorktreeError::Refs)?
                .ok_or_else(|| WorktreeError::NoSuchBranch(name.clone()))?,
            WorktreeBranch::Create(_) => match refs.head().map_err(WorktreeError::Refs)? {
                Head::Detached(commit) => commit,
                Head::Branch(current) => refs
                    .branch(&current)
                    .map_err(WorktreeError::Refs)?
                    .ok_or(WorktreeError::NoStart(current))?,
            },
        };
        let target = match branch {
            WorktreeBranch::Existing(name) => NewWorktreeBranch::Existing(name.clone(), tip),
            WorktreeBranch::Create(name) => NewWorktreeBranch::Create(name.clone(), tip),
        };
        Ok((target, tip))
    }

    /// A name for the state directory of the worktree at `location`: the last part of the path,
    /// reduced to letters, digits, `-`, `_`, and `.` (not first), with a number added if the name is
    /// taken or is `main`.
    fn free_name(&self, location: &Path) -> Result<String, WorktreeError> {
        let base: String = location
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let base = base.trim_start_matches('.');
        let base = if base.is_empty() { "worktree" } else { base };
        let dir = self.metadata_dir().join(WORKTREES_DIR);
        let mut candidate = base.to_owned();
        let mut number = 1;
        while candidate == MAIN_WORKTREE
            || exists(&dir.join(&candidate)).map_err(WorktreeError::io("inspect", &dir))?
        {
            number += 1;
            candidate = format!("{base}-{number}");
        }
        Ok(candidate)
    }

    /// Refuses a path that is a worktree, lies in one, or holds one. The main worktree counts.
    fn refuse_overlap(&self, location: &Path) -> Result<(), WorktreeError> {
        for other in self.worktrees()? {
            let known = resolve(&other.path).unwrap_or(other.path);
            if location == known {
                return Err(WorktreeError::PathInUse {
                    path: location.to_path_buf(),
                    worktree: other.name,
                });
            }
            if location.starts_with(&known) {
                return Err(WorktreeError::Nested {
                    path: location.to_path_buf(),
                    worktree: other.name,
                    inside: true,
                });
            }
            if known.starts_with(location) {
                return Err(WorktreeError::Nested {
                    path: location.to_path_buf(),
                    worktree: other.name,
                    inside: false,
                });
            }
        }
        Ok(())
    }

    /// Lists the worktrees: the main one first, then the linked ones by name (ADR-009). A state
    /// directory without `HEAD` is an add that stopped and is left out.
    ///
    /// # Errors
    ///
    /// Returns [`WorktreeError::Refs`] if a `HEAD` or a `working-copy` cannot be read, and
    /// [`WorktreeError::Io`] if the directory of the worktrees cannot be listed.
    pub fn worktrees(&self) -> Result<Vec<WorktreeInfo>, WorktreeError> {
        let shared = self.metadata_dir();
        let main_path = shared
            .parent()
            .map_or_else(|| shared.clone(), Path::to_path_buf);
        let main_path = resolve(&main_path)?;
        let mut rows = vec![read_info(&shared, &shared, MAIN_WORKTREE, main_path)?];
        let dir = shared.join(WORKTREES_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(rows),
            Err(error) => return Err(WorktreeError::io("list the worktrees in", &dir)(error)),
        };
        let mut states = Vec::new();
        for entry in entries {
            let entry = entry.map_err(WorktreeError::io("list the worktrees in", &dir))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| WorktreeError::Io {
                    operation: "list the worktrees in",
                    path: entry.path(),
                    source: io::Error::new(io::ErrorKind::InvalidData, "the name is not UTF-8"),
                })?;
            states.push((name, entry.path()));
        }
        states.sort();
        for (name, state) in states {
            if !state.join("HEAD").exists() {
                continue;
            }
            let located = state.join(LOCATION_FILE);
            let text = fs::read_to_string(&located).map_err(WorktreeError::io(
                "read the location of the worktree",
                &located,
            ))?;
            let path = PathBuf::from(text.strip_suffix('\n').unwrap_or(&text));
            rows.push(read_info(&shared, &state, &name, path)?);
        }
        Ok(rows)
    }
}

/// Reads HEAD and the owner of the worktree whose state directory is `state`.
fn read_info(
    shared: &Path,
    state: &Path,
    name: &str,
    path: PathBuf,
) -> Result<WorktreeInfo, WorktreeError> {
    let refs = crate::refs::FsRefStore::for_worktree(shared, state);
    let head = crate::refs::RefStore::head(&refs).map_err(WorktreeError::Refs)?;
    let (_, owner) = refs.working_copy_and_owner().map_err(WorktreeError::Refs)?;
    Ok(WorktreeInfo {
        name: name.to_owned(),
        path,
        head,
        owner,
    })
}

/// Makes `path` absolute and resolves the links of the part that exists; the part that does not
/// exist yet is appended as it is.
fn resolve(path: &Path) -> Result<PathBuf, WorktreeError> {
    let absolute =
        std::path::absolute(path).map_err(WorktreeError::io("make the path absolute", path))?;
    let mut missing = Vec::new();
    let mut existing = absolute.as_path();
    loop {
        match fs::canonicalize(existing) {
            Ok(real) => {
                let mut resolved = real;
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
                    return Ok(absolute);
                };
                missing.push(name);
                existing = parent;
            }
            Err(error) => return Err(WorktreeError::io("resolve the path of", existing)(error)),
        }
    }
}

/// Checks that `location` is free, and makes it. Returns the topmost directory that the call made,
/// or `None` if the directory existed, empty.
fn prepare_directory(location: &Path) -> Result<Option<PathBuf>, WorktreeError> {
    match fs::symlink_metadata(location) {
        Ok(metadata) if metadata.is_dir() => {
            let mut entries =
                fs::read_dir(location).map_err(WorktreeError::io("list", location))?;
            if entries.next().is_some() {
                return Err(WorktreeError::NotEmpty(location.to_path_buf()));
            }
            Ok(None)
        }
        Ok(_) => Err(WorktreeError::NotEmpty(location.to_path_buf())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut top = location;
            while let Some(parent) = top.parent() {
                if parent.exists() {
                    break;
                }
                top = parent;
            }
            fs::create_dir_all(location)
                .map_err(WorktreeError::io("create the directory", location))?;
            Ok(Some(top.to_path_buf()))
        }
        Err(error) => Err(WorktreeError::io("inspect", location)(error)),
    }
}

/// Writes the pointer file `.stemma` in the directory of the worktree, the last file of the add.
fn write_pointer(location: &Path, state: &Path) -> Result<(), RefError> {
    let target = location.join(METADATA_DIR);
    let text = state
        .to_str()
        .expect("the state directory is below a UTF-8 path");
    let content = format!("{POINTER_PREFIX}{text}\n");
    let write = || -> io::Result<()> {
        let mut file = fault::create_new(&target)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()
    };
    write().map_err(|source| RefError::Io {
        operation: "write the pointer file",
        path: target.clone(),
        source,
    })
}

/// Opens the linked worktree whose pointer file is `path/.stemma` (ADR-009). The pointer file names
/// the state directory; the file `location` in it must name `path`. Then the format of the shared
/// metadata directory is checked as for the main worktree.
pub(super) fn open_linked(path: &Path, owned: bool) -> Result<Repository, OpenError> {
    let pointer = path.join(METADATA_DIR);
    let malformed = || OpenError::MalformedPointer {
        path: pointer.clone(),
    };
    let file = fs::File::open(&pointer).map_err(OpenError::io("read", &pointer))?;
    let mut bytes = Vec::new();
    file.take(POINTER_MAX_LEN + 1)
        .read_to_end(&mut bytes)
        .map_err(OpenError::io("read", &pointer))?;
    // A file that does not start like a pointer file is a file in the way of `.stemma`, as before
    // worktrees; only a file that claims to be a pointer file can be a damaged one.
    if !bytes.starts_with(POINTER_PREFIX.as_bytes()) {
        return Err(OpenError::NotADirectory { path: pointer });
    }
    if bytes.len() as u64 > POINTER_MAX_LEN {
        return Err(malformed());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| malformed())?;
    let state = PathBuf::from(
        text.strip_prefix(POINTER_PREFIX)
            .and_then(|rest| rest.strip_suffix('\n'))
            .ok_or_else(malformed)?,
    );
    let name = state
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(malformed)?
        .to_owned();
    let worktrees = state.parent().ok_or_else(malformed)?;
    let shared = worktrees.parent().ok_or_else(malformed)?;
    let canonical = state.is_absolute()
        && worktrees
            .file_name()
            .is_some_and(|dir| dir == WORKTREES_DIR)
        && shared.file_name().is_some_and(|dir| dir == METADATA_DIR);
    if !canonical {
        return Err(malformed());
    }
    match fs::metadata(&state) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(malformed()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(OpenError::MissingWorktreeState {
                path: pointer,
                state,
            })
        }
        Err(error) => return Err(OpenError::io("inspect", &state)(error)),
    }
    let here = fs::canonicalize(path).map_err(OpenError::io("resolve the path of", path))?;
    let located = state.join(LOCATION_FILE);
    let recorded = match fs::read(&located) {
        Ok(bytes) => String::from_utf8(bytes)
            .ok()
            .and_then(|text| text.strip_suffix('\n').map(str::to_owned)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(OpenError::io("read", &located)(error)),
    };
    if recorded.as_deref().map(Path::new) != Some(here.as_path()) {
        return Err(OpenError::WorktreeMoved {
            path: here,
            state,
            recorded,
        });
    }
    super::check_format(shared, owned)?;
    Ok(Repository::linked(path, shared, &name))
}

/// Why [`Repository::add_worktree`] or [`Repository::worktrees`] fails. The variants before
/// [`CheckoutFailed`](Self::CheckoutFailed) wrote nothing of the add; the others wrote its entry.
#[derive(Debug)]
#[non_exhaustive]
pub enum WorktreeError {
    /// The authorizer refused `worktree-add`, or the entry could not be written.
    Update(UpdateError),
    /// The references could not be read, a lock is held, or the branch is in the way, exists, or
    /// is checked out in another worktree.
    Refs(RefError),
    /// A commit or a tree could not be read.
    Read(SnapshotError),
    /// A commit could not be stored.
    Store(Box<StoreError>),
    /// The files cannot be planned. Nothing was written.
    Checkout(CheckoutError),
    /// The operating system gave no random bytes for the change ID.
    ChangeId(GenerateError),
    /// The storage failed.
    Io {
        /// What was being done.
        operation: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error of the file system.
        source: io::Error,
    },
    /// The path has a part that is not UTF-8, which the pointer file cannot hold.
    PathNotUtf8(PathBuf),
    /// The path is not an empty directory, or does not exist and cannot be made.
    NotEmpty(PathBuf),
    /// The path is the directory of a worktree already.
    PathInUse {
        /// The path.
        path: PathBuf,
        /// The worktree.
        worktree: String,
    },
    /// The path lies in the directory of a worktree, or holds one.
    Nested {
        /// The path.
        path: PathBuf,
        /// The worktree.
        worktree: String,
        /// Whether the path lies in the worktree, and not the worktree in the path.
        inside: bool,
    },
    /// There is no branch with the name (`-b` makes one).
    NoSuchBranch(BranchName),
    /// `-b` has no start: HEAD names a branch that has no commit yet.
    NoStart(BranchName),
    /// The files could not be written after the entry. No pointer moved; the directory of the
    /// worktree stays as far as it came.
    CheckoutFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: CheckoutError,
    },
    /// The entry was written and the files are in place, but no pointer moved.
    MoveFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: RefError,
    },
    /// The entry was written and the files are in place, but only some of the pointers moved.
    PartlyMoved {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why the next pointer did not move.
        source: RefError,
    },
    /// The worktree was added, but a directory could not be flushed to disk.
    NotDurable {
        /// The entry that was written.
        entry: Box<Entry>,
        /// The error of the flush.
        source: RefError,
    },
}

impl WorktreeError {
    fn io<'a>(
        operation: &'static str,
        path: &'a Path,
    ) -> impl FnOnce(io::Error) -> WorktreeError + 'a {
        move |source| WorktreeError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    /// Returns the error of the file system, if there is one inside.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Update(error) => error.io_error(),
            Self::Refs(error)
            | Self::MoveFailed { source: error, .. }
            | Self::PartlyMoved { source: error, .. }
            | Self::NotDurable { source: error, .. } => error.io_error(),
            Self::Read(error) => error.io_error(),
            Self::Store(error) => error.io_error(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.io_error(),
            Self::Io { source, .. } => Some(source),
            Self::ChangeId(_)
            | Self::PathNotUtf8(_)
            | Self::NotEmpty(_)
            | Self::PathInUse { .. }
            | Self::Nested { .. }
            | Self::NoSuchBranch(_)
            | Self::NoStart(_) => None,
        }
    }

    /// The entry of the add, if the error leaves one in the log.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Update(error) => error.entry(),
            Self::CheckoutFailed { entry, .. }
            | Self::MoveFailed { entry, .. }
            | Self::PartlyMoved { entry, .. }
            | Self::NotDurable { entry, .. } => Some(entry),
            _ => None,
        }
    }

    /// Whether the worktree was added and only the flush failed.
    pub fn made(&self) -> bool {
        matches!(self, Self::NotDurable { .. })
    }

    /// Whether a pointer moved although the call failed.
    pub fn pointer_moved(&self) -> bool {
        matches!(self, Self::PartlyMoved { .. } | Self::NotDurable { .. })
    }
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Update(error) => write!(f, "could not add the worktree: {error}"),
            Self::Refs(error) => {
                write!(
                    f,
                    "could not add the worktree, nothing of it was written: {error}"
                )
            }
            Self::Read(error) => write!(f, "could not read a commit or a tree: {error}"),
            Self::Store(error) => write!(f, "could not store a commit: {error}"),
            Self::Checkout(error) => {
                write!(
                    f,
                    "could not add the worktree, nothing of it was written: {error}"
                )
            }
            Self::ChangeId(error) => write!(f, "could not make a change ID: {error}"),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "could not {operation} `{}`: {source}", path.display()),
            Self::PathNotUtf8(path) => write!(
                f,
                "`{}` has a part that is not UTF-8, which the pointer file of a worktree cannot \
                 hold; choose another path",
                path.display()
            ),
            Self::NotEmpty(path) => write!(
                f,
                "`{}` exists and is not an empty directory; choose a path that does not exist \
                 or an empty directory",
                path.display()
            ),
            Self::PathInUse { path, worktree } => write!(
                f,
                "`{}` is the directory of the worktree `{worktree}` already",
                path.display()
            ),
            Self::Nested {
                path,
                worktree,
                inside: true,
            } => write!(
                f,
                "`{}` lies in the worktree `{worktree}`; a worktree may not be inside another",
                path.display()
            ),
            Self::Nested {
                path,
                worktree,
                inside: false,
            } => write!(
                f,
                "`{}` holds the worktree `{worktree}`; a worktree may not contain another",
                path.display()
            ),
            Self::NoSuchBranch(name) => write!(
                f,
                "there is no branch `{name}`; `stemma worktree add -b {name} <path>` makes one"
            ),
            Self::NoStart(name) => write!(
                f,
                "the branch `{name}` has no commit yet, so a new branch has no start; commit first"
            ),
            Self::CheckoutFailed { entry, source } => write!(
                f,
                "could not write the files; entry {} names a worktree that was not added and no \
                 pointer moved: {source}; remove the directory of the worktree and run the \
                 command again",
                entry.number()
            ),
            Self::MoveFailed { entry, source } => write!(
                f,
                "entry {} was written and the files are in place, but no pointer moved: \
                 {source}; remove the directory of the worktree and run the command again",
                entry.number()
            ),
            Self::PartlyMoved { entry, source } => write!(
                f,
                "entry {} was written and the files are in place, but only some of the pointers \
                 moved: {source}; remove the directory of the worktree and its state directory \
                 below `.stemma/worktrees` and run the command again",
                entry.number()
            ),
            Self::NotDurable { entry, source } => write!(
                f,
                "the worktree was added as entry {}, but a directory could not be flushed, so \
                 it may not survive a crash: {source}",
                entry.number()
            ),
        }
    }
}

impl Error for WorktreeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Update(error) => error.source(),
            Self::Refs(error)
            | Self::MoveFailed { source: error, .. }
            | Self::PartlyMoved { source: error, .. }
            | Self::NotDurable { source: error, .. } => error.source(),
            Self::Read(error) => error.source(),
            Self::Store(error) => error.source(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.source(),
            Self::ChangeId(error) => error.source(),
            Self::Io { source, .. } => source.source(),
            Self::PathNotUtf8(_)
            | Self::NotEmpty(_)
            | Self::PathInUse { .. }
            | Self::Nested { .. }
            | Self::NoSuchBranch(_)
            | Self::NoStart(_) => None,
        }
    }
}
