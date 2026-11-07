//! Linked worktrees: `worktree add`, `worktree list`, and `worktree remove` (ADR-009, ADR-010).
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
use super::conflicted;
use super::snapshot::{read_any_commit, read_commit, tracked_files, SnapshotError};
use super::{
    exists, now, refuse_reserved, OpenError, Repository, UpdateError, METADATA_DIR, WORKTREES_DIR,
};
use crate::change::{ChangeId, GenerateError};
use crate::fault;
use crate::object::{Commit, CommitId};
use crate::oplog::{Change, Entry, Moved, OpLogError, OperationName};
use crate::refs::{
    BranchName, Head, Logged, NewWorktree, NewWorktreeBranch, RefError, RefMalformed,
    RefParseError, LOCATION_FILE, MAIN_WORKTREE,
};
use crate::store::{ObjectStore, StoreError};
use crate::{Actor, Authorizer};

/// The operation that the log and the authorizer know for `worktree add`. The name of an operation
/// holds no space, so the subcommand is joined with a hyphen.
const OPERATION: &str = "worktree-add";

/// The operation that the log and the authorizer know for `worktree remove`.
const REMOVE_OPERATION: &str = "worktree-remove";

/// How many untracked files an error of `worktree remove` names; the others are counted.
const DIRTY_LISTED: usize = 5;

/// What the pointer file `.stemma` of a linked worktree starts with.
pub(super) const POINTER_PREFIX: &str = "stemma-worktree ";

/// The longest pointer file that is read. A longer file is not in canonical form anyway.
const POINTER_MAX_LEN: u64 = 4096;

/// The branch that `worktree add` checks out in the new worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeBranch {
    /// An existing branch that no worktree has checked out.
    Existing(BranchName),
    /// A new branch (`-b`), made at the commit that HEAD of the calling worktree leads to. The
    /// name must not start with `heads/` or `tags/` (ADR-008).
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

/// What `worktree remove` removed.
#[derive(Debug)]
#[must_use]
pub struct RemovedWorktree {
    /// The entry of the log.
    pub entry: Box<Entry>,
    /// The entry of the snapshot that saved the files of the worktree first, if some were unsaved.
    pub snapshot: Option<Box<Entry>>,
    /// The name of the worktree.
    pub name: String,
    /// The directory of the worktree. It was removed with the worktree, unless it was gone before.
    pub path: PathBuf,
    /// Whether the directory was gone already, so that only the state directory was removed.
    pub directory_missing: bool,
    /// What HEAD of the worktree named. The branch stays.
    pub head: Head,
    /// The working-copy commit of the worktree. The entry names it, so a purge and the log keep it.
    pub working_copy: CommitId,
    /// The files that the working-copy commit did not track and `--force` removed with the
    /// directory. They are in no commit, so nothing brings them back.
    pub discarded: Vec<String>,
}

impl Repository {
    /// Removes the linked worktree `worktree` (ADR-009): its directory and its state directory.
    /// `worktree` is the name of the worktree or the path of its directory. The branch of the
    /// worktree stays, and so do its commits.
    ///
    /// The steps are: an open purge refuses the call; the authorizer is asked for
    /// `worktree-remove` (the target is `worktree`); the lock of the references is taken and held
    /// to the last move; the worktree is found under the lock, and the main worktree and the one
    /// of this call are refused; the owner of the working copy is checked; if the directory
    /// exists, it
    /// must hold the pointer file of this worktree, and the files are saved in a snapshot of the
    /// worktree first (a second question to the authorizer, `snapshot`, and the owner of the
    /// working copy is checked as for every snapshot), then every file that the working-copy
    /// commit does not track is looked for: with `force` they are removed, else they stop the
    /// call; the entry is written and flushed, naming the worktree, its directory, HEAD, and the
    /// working-copy commit with the new value `-`; the directory is removed; and the state
    /// directory goes: `HEAD` first, so that the worktree is gone from `worktree list` and
    /// counts for no branch, then the rest. A worktree whose directory was removed outside
    /// Stemma has only its state directory removed.
    ///
    /// # Errors
    ///
    /// Before the entry: [`WorktreeError::Update`] for a refusal of the question,
    /// [`WorktreeError::NoSuchWorktree`], [`WorktreeError::Damaged`],
    /// [`WorktreeError::SeveralDamaged`], [`WorktreeError::Unfinished`],
    /// [`WorktreeError::RemoveMain`],
    /// [`WorktreeError::RemoveCurrent`], [`WorktreeError::NotItsPointer`],
    /// [`WorktreeError::Snapshot`] (the owner, a file that cannot be saved, or a refused
    /// `snapshot`), [`WorktreeError::Dirty`], [`WorktreeError::Refs`], and the errors of the
    /// storage. The snapshot that came first stays in the log when a later step fails
    /// ([`WorktreeError::AfterSnapshot`], which carries the failed flush of that snapshot, if
    /// there was one: then the call ends with code 3, else it can be repeated). After the entry:
    /// [`WorktreeError::RemoveFailed`],
    /// [`WorktreeError::RemoveMoveFailed`], [`WorktreeError::RemovePartlyMoved`], and
    /// [`WorktreeError::RemoveNotDurable`].
    pub fn remove_worktree(
        &self,
        worktree: &str,
        force: bool,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<RemovedWorktree, WorktreeError> {
        let operation = OperationName::try_new(REMOVE_OPERATION)
            .expect("`worktree-remove` is a valid operation name");
        self.refs_store()
            .refuse_pending_purge()
            .map_err(WorktreeError::Refs)?;
        let allowed = self
            .authorize(actor, authorizer, &operation, worktree)
            .map_err(WorktreeError::Update)?;
        let shared = resolve(&self.metadata_dir())?;
        let lock = self
            .refs_store()
            .lock_writer(&allowed)
            .map_err(WorktreeError::Refs)?;
        // Under the lock, so that a removal that ran in between is `NoSuchWorktree`.
        let info = self.find_worktree(worktree)?;
        let state = shared.join(WORKTREES_DIR).join(&info.name);
        let exists = match fs::symlink_metadata(&info.path) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(WorktreeError::io("inspect", &info.path)(error)),
        };
        if exists {
            check_pointer(&info.path, &state)?;
        }
        let view = Repository::linked(&info.path, &shared, &info.name);
        // The owner decides, also when the directory is gone and nothing is saved: only the owner
        // removes the state of a working copy (ADR-010).
        let (_, owner) = crate::refs::FsRefStore::for_worktree(&shared, &state)
            .working_copy_and_owner()
            .map_err(WorktreeError::Refs)?;
        view.check_owner(actor, owner.as_ref()).map_err(|error| {
            let error = if exists {
                error.without_take_over_option()
            } else {
                error.without_directory()
            };
            WorktreeError::Snapshot(Box::new(error))
        })?;
        let mut snapshot = None;
        let mut late = None;
        if exists {
            // The files go into the working-copy commit first, so that nothing that is tracked is
            // lost with the directory. The owner of the working copy is checked as for a snapshot.
            snapshot = match view.snapshot_locked(&lock, actor, authorizer) {
                Ok(entry) => entry.map(Box::new),
                Err(SnapshotError::Update(UpdateError::NotDurable { entry, source })) => {
                    late = Some(source);
                    Some(entry)
                }
                Err(error) => {
                    return Err(WorktreeError::Snapshot(Box::new(
                        error.without_take_over_option(),
                    )))
                }
            };
        }
        let refs = crate::refs::FsRefStore::for_worktree(&shared, &state);
        let head = crate::refs::RefStore::head(&refs).map_err(WorktreeError::Refs);
        let head = head.map_err(|error| after_snapshot(&snapshot, &mut late, error))?;
        let (working_copy, _) = refs
            .working_copy_and_owner()
            .map_err(WorktreeError::Refs)
            .map_err(|error| after_snapshot(&snapshot, &mut late, error))?;
        let text = info.path.to_string_lossy().into_owned();
        let mut discarded = Vec::new();
        let logged = self
            .refs_store()
            .remove_worktree_after(&lock, &info.name, || {
                if exists {
                    let untracked = view.untracked_files(working_copy)?;
                    discarded.clone_from(&untracked);
                    if !force && !untracked.is_empty() {
                        return Err(WorktreeError::Dirty {
                            path: info.path.clone(),
                            listed: untracked.iter().take(DIRTY_LISTED).cloned().collect(),
                            more: untracked.len().saturating_sub(DIRTY_LISTED),
                        });
                    }
                }
                let change = Change::try_new(
                    now(),
                    actor.clone(),
                    operation.clone(),
                    Some(Moved::from_to(Some(head.clone()), None)),
                    Some(Moved::from_to(Some(working_copy), None)),
                    Vec::new(),
                )
                .and_then(|change| change.in_worktree(Some(info.name.clone()), Some(text.clone())))
                .map_err(|error| {
                    WorktreeError::Update(UpdateError::Log(OpLogError::Entry(error)))
                })?;
                let entry = self
                    .append(&lock, &change)
                    .map_err(|error| WorktreeError::Update(UpdateError::Log(error)))?;
                if exists {
                    // The directory leaves its place in one step, and only then is it deleted:
                    // a stop in the middle of the delete leaves no half of the worktree at its
                    // path, where a second call would take it for the worktree and save it.
                    let doomed = match info.path.parent().zip(info.path.file_name()) {
                        Some((parent, base)) => {
                            let mut hidden = std::ffi::OsString::from(".stemma-removing-");
                            hidden.push(base);
                            hidden.push(format!("-{}", std::process::id()));
                            let hidden = parent.join(hidden);
                            match fault::rename(&info.path, &hidden) {
                                Ok(()) => hidden,
                                Err(source) => {
                                    return Err(WorktreeError::RemoveFailed {
                                        entry: Box::new(entry),
                                        path: info.path.clone(),
                                        source,
                                    });
                                }
                            }
                        }
                        None => info.path.clone(),
                    };
                    if let Err(source) = fault::remove_dir_all(&doomed) {
                        return Err(WorktreeError::RemoveFailed {
                            entry: Box::new(entry),
                            path: doomed,
                            source,
                        });
                    }
                }
                Ok(entry)
            });
        let removed = |entry: Entry| RemovedWorktree {
            entry: Box::new(entry),
            snapshot: snapshot.clone(),
            name: info.name.clone(),
            path: info.path.clone(),
            directory_missing: !exists,
            head: head.clone(),
            working_copy,
            discarded: discarded.clone(),
        };
        let result = match logged.map_err(WorktreeError::Refs) {
            Err(error) => Err(error),
            Ok(Logged::Unchanged) => unreachable!("a removal always changes the pointers"),
            Ok(Logged::Moved(entry)) => {
                return match late {
                    Some(source) => Err(WorktreeError::RemoveNotDurable {
                        entry: Box::new(entry),
                        source,
                    }),
                    None => Ok(removed(entry)),
                };
            }
            Ok(Logged::StepFailed(error)) => Err(error),
            Ok(Logged::MoveFailed(entry, source)) => Err(WorktreeError::RemoveMoveFailed {
                entry: Box::new(entry),
                source,
            }),
            Ok(Logged::PartlyMoved(entry, source)) => Err(WorktreeError::RemovePartlyMoved {
                entry: Box::new(entry),
                source,
            }),
            Ok(Logged::NotFlushed(entry, source)) => Err(WorktreeError::RemoveNotDurable {
                entry: Box::new(entry),
                source,
            }),
        };
        result.map_err(|error| after_snapshot(&snapshot, &mut late, error))
    }

    /// The worktree that `name` names: the name of a linked worktree, or the path of its
    /// directory. The main worktree and the worktree of this call are refused.
    ///
    /// # Errors
    ///
    /// [`WorktreeError::Damaged`] if the state of the worktree called `name` cannot be read,
    /// [`WorktreeError::SeveralDamaged`] if `name` is no valid name, matches no readable worktree,
    /// and some state directory is damaged, [`WorktreeError::Unfinished`] for a state directory
    /// without `HEAD`, and [`WorktreeError::NoSuchWorktree`] otherwise.
    fn find_worktree(&self, name: &str) -> Result<WorktreeInfo, WorktreeError> {
        // The other worktrees may be damaged; only the one that is asked for must be readable.
        let (rows, mut damaged) = self.scan_worktrees()?;
        if let Some(index) = damaged.iter().position(|(damaged, _)| damaged == name) {
            let (_, error) = damaged.swap_remove(index);
            return Err(WorktreeError::Damaged {
                name: name.to_owned(),
                source: Box::new(error),
            });
        }
        let path = resolve(Path::new(name)).ok();
        // A name wins over a path: the argument `feat` removes the worktree called `feat`, even if
        // `./feat` is the directory of another worktree. Write `./feat` to mean the directory.
        let found = rows
            .iter()
            .position(|row| row.name == name)
            .or_else(|| {
                let path = path.as_ref()?;
                rows.iter().position(|row| *path == resolve_or(&row.path))
            })
            .map(|index| rows[index].clone());
        let Some(found) = found else {
            let state = self.metadata_dir().join(WORKTREES_DIR).join(name);
            if crate::oplog::valid_worktree_name(name)
                && state.is_dir()
                && matches!(
                    fs::symlink_metadata(state.join("HEAD")),
                    Err(error) if error.kind() == io::ErrorKind::NotFound
                )
            {
                return Err(WorktreeError::Unfinished(name.to_owned()));
            }
            // A path cannot be mapped to a name without the `location` of a damaged worktree, so
            // it may be the one that is asked for: the message names all of them. An argument
            // that is a valid name stays `NoSuchWorktree`: no damaged state directory has it.
            if !damaged.is_empty() && !crate::oplog::valid_worktree_name(name) {
                return Err(WorktreeError::SeveralDamaged {
                    asked: Some(name.to_owned()),
                    damaged: boxed(damaged),
                });
            }
            return Err(WorktreeError::NoSuchWorktree(name.to_owned()));
        };
        if found.name == MAIN_WORKTREE {
            return Err(WorktreeError::RemoveMain);
        }
        if self.worktree_name() == Some(found.name.as_str()) {
            return Err(WorktreeError::RemoveCurrent(found.name));
        }
        Ok(found)
    }

    /// The files in the working directory that the tree of the working-copy commit `commit` does
    /// not hold, as paths with `/`, sorted. The pointer file at the top is not one of them. A
    /// directory counts only through the files in it.
    fn untracked_files(&self, commit: CommitId) -> Result<Vec<String>, WorktreeError> {
        let commit = read_any_commit(self.objects(), commit).map_err(WorktreeError::Read)?;
        let view =
            conflicted::view(self.objects(), &commit.tree_sum()).map_err(WorktreeError::Read)?;
        let tracked: std::collections::BTreeSet<String> = tracked_files(self.objects(), view.tree)
            .map_err(WorktreeError::Read)?
            .into_iter()
            .map(|file| file.path)
            .collect();
        let mut untracked = Vec::new();
        let mut pending = vec![(self.path().to_path_buf(), String::new())];
        while let Some((dir, prefix)) = pending.pop() {
            let entries = fs::read_dir(&dir).map_err(WorktreeError::io("list", &dir))?;
            for entry in entries {
                let entry = entry.map_err(WorktreeError::io("list", &dir))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                if prefix.is_empty() && name == METADATA_DIR {
                    continue;
                }
                let kind = entry
                    .file_type()
                    .map_err(WorktreeError::io("inspect", &entry.path()))?;
                if kind.is_dir() {
                    pending.push((entry.path(), path));
                } else if !tracked.contains(&path) {
                    untracked.push(path);
                }
            }
        }
        untracked.sort();
        Ok(untracked)
    }

    /// Adds a linked worktree in `path` that has the branch checked out (ADR-009).
    ///
    /// The steps are: the authorizer is asked for `worktree-add` with the path as the target; the
    /// path is made absolute and checked (it must not be, lie in, or hold a worktree, and if it
    /// exists it is an empty directory); the lock of the references is taken and held to the last
    /// pointer; the state directory gets a name from the last part of the path; the branch is read
    /// again, and refused if another worktree has it checked out or, for a new branch, if it
    /// exists; the working-copy commit is made, empty and on the tip, with a change ID of its own and
    /// `owner` as its author; the plan of the files is made; the entry is written and flushed; the
    /// files are written; the pointers are placed: the new branch, the state directory with
    /// `location`, `working-copy` and `HEAD`, and the pointer file `.stemma` last. The calling
    /// working copy of the calling worktree is neither read nor saved; with `-b` its `HEAD` is read.
    ///
    /// `owner` is the actor of the caller unless another is given.
    ///
    /// # Errors
    ///
    /// Before the entry: [`WorktreeError::Update`] for a refusal of the question or for a new
    /// branch whose name starts with `heads/` or `tags/` ([`UpdateError::ReservedName`]),
    /// [`WorktreeError::PathNotUtf8`],
    /// [`WorktreeError::PathInUse`], [`WorktreeError::Nested`], [`WorktreeError::NoSuchBranch`],
    /// [`WorktreeError::NoStart`], [`WorktreeError::Refs`] (among them [`RefError::CheckedOut`]
    /// and [`RefError::BranchExists`]), [`WorktreeError::Checkout`], and the errors of the
    /// storage. The directory that the call made is removed again, and
    /// [`WorktreeError::NotCleaned`] names one that cannot be. After the entry:
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
        if let WorktreeBranch::Create(name) = branch {
            refuse_reserved(name).map_err(WorktreeError::Update)?;
        }
        self.refs_store()
            .refuse_pending_purge()
            .map_err(WorktreeError::Refs)?;
        let owner = owner.unwrap_or(actor);
        let location = resolve(path)?;
        let text = location
            .to_str()
            .ok_or_else(|| WorktreeError::PathNotUtf8(location.clone()))?
            .to_owned();
        let allowed = self
            .authorize(
                actor,
                authorizer,
                &operation,
                &location.display().to_string(),
            )
            .map_err(WorktreeError::Update)?;
        // The pointer file holds the state directory, which lies below the shared directory.
        if resolve(&self.metadata_dir())?.to_str().is_none() {
            return Err(WorktreeError::PathNotUtf8(self.metadata_dir()));
        }
        self.refuse_overlap(&location)?;
        self.add_in(&location, &text, branch, owner, actor, &operation, &allowed)
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
        // Again, under the lock: an add of the same path that ran in between is in the list now.
        self.refuse_overlap(location)?;
        // The directory is made under the lock too: a call that the lock turns away has made
        // nothing, so it cannot remove the directory of the call that holds the lock.
        let made = prepare_directory(location)?;
        match self.add_locked(&lock, location, text, branch, owner, actor, operation) {
            Err(error) if error.entry().is_none() => match made {
                // Nothing of the add was written: the directory that this call made goes again. One
                // that cannot be removed is named, so that nobody finds it by chance.
                Some(made) => Err(match remove_made(location, &made) {
                    Some((path, source)) => WorktreeError::NotCleaned {
                        cause: Box::new(error),
                        path,
                        source,
                    },
                    None => error,
                }),
                None => Err(error),
            },
            result => result,
        }
    }

    /// The add itself, once the lock is held and the directory of the worktree is ready.
    #[allow(clippy::too_many_arguments)]
    fn add_locked(
        &self,
        lock: &crate::repository::WriteLock,
        location: &Path,
        text: &str,
        branch: &WorktreeBranch,
        owner: &Actor,
        actor: &Actor,
        operation: &OperationName,
    ) -> Result<AddedWorktree, WorktreeError> {
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
            lock,
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
                .and_then(|change| change.in_worktree(Some(name.clone()), Some(text.to_owned())))
                .map_err(|error| {
                    WorktreeError::Update(UpdateError::Log(OpLogError::Entry(error)))
                })?;
                let entry = self
                    .append(lock, &change)
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

    /// Whether `state` is a state directory that holds a worktree. A directory without `HEAD` is
    /// what an add or a removal that stopped left (the caller holds the lock of the references, so
    /// no add is running): it holds no worktree, and it is removed here so that its name is free.
    fn name_is_taken(&self, state: &Path) -> Result<bool, WorktreeError> {
        if !exists(state).map_err(WorktreeError::io("inspect", state))? {
            return Ok(false);
        }
        if exists(&state.join("HEAD")).map_err(WorktreeError::io("inspect", state))? {
            return Ok(true);
        }
        fault::remove_dir_all(state)
            .map_err(WorktreeError::io("remove the leftover state", state))?;
        Ok(false)
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
        while candidate == MAIN_WORKTREE || self.name_is_taken(&dir.join(&candidate))? {
            number += 1;
            candidate = format!("{base}-{number}");
        }
        Ok(candidate)
    }

    /// Refuses a path that is a worktree, lies in one, or holds one. The main worktree counts.
    fn refuse_overlap(&self, location: &Path) -> Result<(), WorktreeError> {
        for other in self.worktrees()? {
            let known = resolve(&other.path)?;
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
    /// directory without `HEAD` is no worktree (an add or a removal that stopped, a foreign
    /// directory) and is left out.
    ///
    /// # Errors
    ///
    /// Returns the error of the damaged worktree (usually [`WorktreeError::Refs`] when a `HEAD` or
    /// a `working-copy` cannot be read, [`WorktreeError::Io`] when a file cannot be inspected)
    /// when one worktree is damaged, and [`WorktreeError::SeveralDamaged`], which names all of them,
    /// when more than one is. [`WorktreeError::Io`] also stands for a directory of the worktrees
    /// that cannot be listed.
    pub fn worktrees(&self) -> Result<Vec<WorktreeInfo>, WorktreeError> {
        let (rows, damaged) = self.scan_worktrees()?;
        if damaged.len() > 1 {
            return Err(WorktreeError::SeveralDamaged {
                asked: None,
                damaged: boxed(damaged),
            });
        }
        match damaged.into_iter().next() {
            Some((_, error)) => Err(error),
            None => Ok(rows),
        }
    }

    /// The worktrees that can be read, and for each linked one that cannot, its name and why. The
    /// main worktree must be readable. A state directory without `HEAD` is skipped; a name that
    /// no worktree can have (see [`crate::oplog::valid_worktree_name`]) and a `HEAD` that cannot
    /// be inspected count as damaged. A damaged worktree does not hide the others, so that the
    /// command that removes one does not depend on the state of all.
    fn scan_worktrees(&self) -> Result<Scanned, WorktreeError> {
        let shared = self.metadata_dir();
        let main_path = shared
            .parent()
            .map_or_else(|| shared.clone(), Path::to_path_buf);
        let main_path = resolve(&main_path)?;
        let mut rows = vec![read_info(&shared, &shared, MAIN_WORKTREE, main_path)?];
        let mut damaged = Vec::new();
        let dir = shared.join(WORKTREES_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((rows, damaged)),
            Err(error) => return Err(WorktreeError::io("list the worktrees in", &dir)(error)),
        };
        let mut states = Vec::new();
        for entry in entries {
            let entry = entry.map_err(WorktreeError::io("list the worktrees in", &dir))?;
            let path = entry.path();
            // A file in the directory (a `.DS_Store`, say) is no worktree.
            let is_dir = entry
                .file_type()
                .map_err(WorktreeError::io("inspect", &path))?
                .is_dir();
            if !is_dir {
                continue;
            }
            // A name that is not UTF-8 is a name that no worktree can have: it is judged below,
            // after the check for `HEAD`, like any other name.
            let file_name = entry.file_name();
            let utf8 = file_name.to_str().is_some();
            states.push((file_name.to_string_lossy().into_owned(), path, utf8));
        }
        states.sort();
        for (name, state, utf8) in states {
            // The log and the journal of a purge can name only a valid name, so a state directory
            // with another one is damage that the command names, and the other worktrees stay
            // usable. The check comes before the read, which would take its pointers for good.
            // A directory without `HEAD` is no worktree (a stopped add or removal, a `@eaDir`), so
            // it is no damage either, whatever its name.
            // Only a missing `HEAD` counts as absent: one that cannot be inspected is an error
            // (code 1) for this worktree, and the others stay usable.
            match fs::symlink_metadata(state.join("HEAD")) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    damaged.push((
                        name,
                        WorktreeError::io("inspect", &state.join("HEAD"))(error),
                    ));
                    continue;
                }
            }
            if !utf8 || !crate::oplog::valid_worktree_name(&name) {
                let source = RefError::Corrupt {
                    path: state.clone(),
                    source: RefParseError::Malformed(RefMalformed::WorktreeName),
                };
                damaged.push((name, WorktreeError::Refs(source)));
                continue;
            }
            match read_linked(&shared, &state, &name) {
                Ok(Some(info)) => rows.push(info),
                Ok(None) => {}
                Err(error) => damaged.push((name, error)),
            }
        }
        Ok((rows, damaged))
    }
}

/// The damaged worktrees with their errors boxed, as [`WorktreeError::SeveralDamaged`] holds them.
fn boxed(damaged: Vec<(String, WorktreeError)>) -> Vec<(String, Box<WorktreeError>)> {
    damaged
        .into_iter()
        .map(|(name, error)| (name, Box::new(error)))
        .collect()
}

/// The readable worktrees, and the linked ones that are not, each with its name and why.
type Scanned = (Vec<WorktreeInfo>, Vec<(String, WorktreeError)>);

/// Reads the linked worktree whose state directory is `state`. A state directory without `HEAD`
/// is an add that stopped and gives `None`.
fn read_linked(
    shared: &Path,
    state: &Path,
    name: &str,
) -> Result<Option<WorktreeInfo>, WorktreeError> {
    let head = state.join("HEAD");
    match fs::symlink_metadata(&head) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(WorktreeError::io("inspect", &head)(error)),
    }
    let located = state.join(LOCATION_FILE);
    let text = fs::read_to_string(&located).map_err(WorktreeError::io(
        "read the location of the worktree",
        &located,
    ))?;
    let path = PathBuf::from(text.strip_suffix('\n').unwrap_or(&text));
    read_info(shared, state, name, path).map(Some)
}

/// [`resolve`], or the path as it is if it cannot be resolved.
fn resolve_or(path: &Path) -> PathBuf {
    resolve(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A failure before the entry of the removal names the snapshot that came first, and the flush of
/// it that did not succeed (`late`). A failure that has an entry of its own is returned as it is,
/// unless that flush failed: the message then names both, and the code is 3 either way.
fn after_snapshot(
    snapshot: &Option<Box<Entry>>,
    late: &mut Option<RefError>,
    error: WorktreeError,
) -> WorktreeError {
    match snapshot {
        Some(saved) if error.entry().is_none() || late.is_some() => WorktreeError::AfterSnapshot {
            snapshot: saved.clone(),
            late: late.take(),
            source: Box::new(error),
        },
        _ => error,
    }
}

/// Checks that the directory `path` holds the pointer file of the worktree whose state directory
/// is `state`, so that a removal never deletes the files of another directory.
fn check_pointer(path: &Path, state: &Path) -> Result<(), WorktreeError> {
    // A link to a directory is not the directory of the worktree: removing it would remove the
    // link only.
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_symlink()) {
        return Err(WorktreeError::NotItsPointer {
            path: path.to_path_buf(),
            state: state.to_path_buf(),
        });
    }
    let pointer = path.join(METADATA_DIR);
    let expected = state
        .to_str()
        .map(|state| format!("{POINTER_PREFIX}{state}\n"));
    let found = match fs::File::open(&pointer) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(POINTER_MAX_LEN + 1)
                .read_to_end(&mut bytes)
                .map_err(WorktreeError::io("read", &pointer))?;
            Some(bytes)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(WorktreeError::io("read", &pointer)(error)),
    };
    match (found, expected) {
        (Some(found), Some(expected)) if found == expected.as_bytes() => Ok(()),
        _ => Err(WorktreeError::NotItsPointer {
            path: path.to_path_buf(),
            state: state.to_path_buf(),
        }),
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
                    // A `..` below a directory that does not exist leads nowhere that can be
                    // compared, and `prepare_directory` could reach an existing worktree by it.
                    return Err(WorktreeError::Io {
                        operation: "resolve the path of",
                        path: absolute,
                        source: io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "the path has `..` below a directory that does not exist",
                        ),
                    });
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

/// Removes the directories that [`prepare_directory`] made, from `location` up to `made`, each
/// only while it is empty: another call may have put something into a directory they share, and
/// that stays. Returns the directory that could not be removed for another reason, with the error.
fn remove_made(location: &Path, made: &Path) -> Option<(PathBuf, io::Error)> {
    let mut current = location;
    loop {
        match fault::remove_dir(current) {
            Ok(()) => {}
            // Gone, or in use by another call: nothing to remove, nothing to report.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                return None;
            }
            Err(error) => return Some((current.to_path_buf(), error)),
        }
        if current == made {
            return None;
        }
        current = current.parent()?;
    }
}

/// Writes the pointer file `.stemma` in the directory of the worktree, the last file of the add,
/// and flushes the directory. A state directory that is not UTF-8 is refused before the add.
fn write_pointer(location: &Path, state: &Path) -> Result<(), RefError> {
    let target = location.join(METADATA_DIR);
    let io_error = |path: &Path, source: io::Error| RefError::Io {
        operation: "write the pointer file",
        path: path.to_path_buf(),
        source,
    };
    let Some(text) = state.to_str() else {
        return Err(io_error(
            &target,
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the state directory is not UTF-8",
            ),
        ));
    };
    let content = format!("{POINTER_PREFIX}{text}\n");
    let write = || -> io::Result<()> {
        let mut file = fault::create_new(&target)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()
    };
    write().map_err(|source| io_error(&target, source))?;
    crate::store::flush_directory(location).map_err(|source| RefError::NotFlushed {
        path: location.to_path_buf(),
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

/// Why [`Repository::add_worktree`], [`Repository::remove_worktree`], or [`Repository::worktrees`]
/// fails. The variants before [`CheckoutFailed`](Self::CheckoutFailed) wrote nothing of the add;
/// the others of the add wrote its entry. For a removal, [`Snapshot`](Self::Snapshot),
/// [`AfterSnapshot`](Self::AfterSnapshot), [`Dirty`](Self::Dirty), and the refusals before them
/// wrote no entry of the removal; the `Remove*` variants wrote it.
#[derive(Debug)]
#[non_exhaustive]
pub enum WorktreeError {
    /// The authorizer refused `worktree-add`, the name of a new branch starts with `heads/` or
    /// `tags/` ([`UpdateError::ReservedName`]), or the entry could not be written. An entry that is
    /// not known to be flushed stays in the log ([`WorktreeError::entry`]).
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
    /// The state directory is what an add or a removal that stopped left: it has no `HEAD`, so it
    /// holds no worktree and `worktree list` does not show it. Nothing was written.
    Unfinished(String),
    /// The state directory of the worktree cannot be read, so the worktree cannot be described or
    /// removed. The other worktrees are not affected. Nothing was written.
    Damaged {
        /// The name of the worktree.
        name: String,
        /// What cannot be read.
        source: Box<WorktreeError>,
    },
    /// Several worktrees are damaged, or the argument of a removal that is no valid worktree name
    /// (a path, usually) matches no readable worktree while at least one is damaged. A damaged
    /// worktree has no readable `location`, so a path cannot be mapped to it. Nothing was written.
    SeveralDamaged {
        /// What the call asked for, if it asked for one worktree.
        asked: Option<String>,
        /// The damaged worktrees by name, each with what cannot be read.
        damaged: Vec<(String, Box<WorktreeError>)>,
    },
    /// The path exists and is not an empty directory: a directory with files, a file, or a link.
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
    /// The add failed before it wrote anything, and the directory that the call made could not be
    /// removed again.
    NotCleaned {
        /// Why the add failed.
        cause: Box<WorktreeError>,
        /// The directory that stays.
        path: PathBuf,
        /// Why it could not be removed.
        source: io::Error,
    },
    /// The worktree was added, but a directory could not be flushed to disk.
    NotDurable {
        /// The entry that was written.
        entry: Box<Entry>,
        /// The error of the flush.
        source: RefError,
    },
    /// There is no linked worktree with this name or at this path.
    NoSuchWorktree(String),
    /// The main worktree cannot be removed: it holds the shared store.
    RemoveMain,
    /// The call runs in the worktree it is to remove; run it in another worktree.
    RemoveCurrent(String),
    /// The directory of the worktree holds no pointer file that names its state directory, so it
    /// is not the directory of this worktree and none of its files is removed.
    NotItsPointer {
        /// The directory.
        path: PathBuf,
        /// The state directory that its pointer file is to name.
        state: PathBuf,
    },
    /// The files of the worktree could not be saved first (the owner of the working copy is
    /// another actor, a file is refused, or the question `snapshot` was refused). Nothing was
    /// removed.
    Snapshot(Box<SnapshotError>),
    /// The files were saved first, in the entry `snapshot`, and a later step before the entry of
    /// the removal failed with `source`. Nothing was removed.
    AfterSnapshot {
        /// The entry of the snapshot.
        snapshot: Box<Entry>,
        /// The error of the flush, if a directory of the snapshot could not be flushed: the saved
        /// files may not survive a crash.
        late: Option<RefError>,
        /// The failure.
        source: Box<WorktreeError>,
    },
    /// The directory holds files that the working-copy commit does not track, and `--force` was
    /// not given. Nothing was removed.
    Dirty {
        /// The directory.
        path: PathBuf,
        /// The first of the files, with `/` in the paths.
        listed: Vec<String>,
        /// How many more files there are.
        more: usize,
    },
    /// The entry of the removal was written, but the directory could not be removed. No pointer
    /// moved; the part of the directory that is gone stays gone.
    RemoveFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// The directory.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// The entry of the removal was written and the directory is gone, but the first pointer did
    /// not move: the state directory is whole.
    RemoveMoveFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: RefError,
    },
    /// The entry of the removal was written and the directory is gone, but the state directory
    /// is removed in part only.
    RemovePartlyMoved {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: RefError,
    },
    /// The worktree was removed, but a directory could not be flushed to disk.
    RemoveNotDurable {
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
            | Self::NotDurable { source: error, .. }
            | Self::RemoveMoveFailed { source: error, .. }
            | Self::RemovePartlyMoved { source: error, .. }
            | Self::RemoveNotDurable { source: error, .. } => error.io_error(),
            Self::Snapshot(error) => error.io_error(),
            Self::AfterSnapshot { source, .. } | Self::Damaged { source, .. } => source.io_error(),
            // The first file error of the list; `kind()` may rest on another entry.
            Self::SeveralDamaged { damaged, .. } => {
                damaged.iter().find_map(|(_, error)| error.io_error())
            }
            Self::RemoveFailed { source, .. } => Some(source),
            Self::Read(error) => error.io_error(),
            Self::Store(error) => error.io_error(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.io_error(),
            Self::Io { source, .. } => Some(source),
            Self::NotCleaned { cause, .. } => cause.io_error(),
            Self::ChangeId(_)
            | Self::PathNotUtf8(_)
            | Self::NotEmpty(_)
            | Self::PathInUse { .. }
            | Self::Nested { .. }
            | Self::NoSuchBranch(_)
            | Self::NoStart(_)
            | Self::NoSuchWorktree(_)
            | Self::Unfinished(_)
            | Self::RemoveMain
            | Self::RemoveCurrent(_)
            | Self::NotItsPointer { .. }
            | Self::Dirty { .. } => None,
        }
    }

    /// The entry that the error leaves in the log: the entry of the add or the removal, or, for
    /// `AfterSnapshot` whose failure has no entry of its own and for a `Snapshot` that failed after
    /// its entry, the entry of the snapshot that came first (see
    /// [`wrote_its_entry`](Self::wrote_its_entry) for the entry of the add or removal only).
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Update(error) => error.entry(),
            Self::CheckoutFailed { entry, .. }
            | Self::MoveFailed { entry, .. }
            | Self::PartlyMoved { entry, .. }
            | Self::NotDurable { entry, .. }
            | Self::RemoveFailed { entry, .. }
            | Self::RemoveMoveFailed { entry, .. }
            | Self::RemovePartlyMoved { entry, .. }
            | Self::RemoveNotDurable { entry, .. } => Some(entry),
            // The entry of the removal is the later one, so it is the one that is named.
            Self::AfterSnapshot {
                snapshot, source, ..
            } => source.entry().or(Some(snapshot)),
            Self::Snapshot(error) => match &**error {
                SnapshotError::Update(update) => update.entry(),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether the worktree was added or removed and only the flush failed.
    pub fn made(&self) -> bool {
        matches!(
            self,
            Self::NotDurable { .. } | Self::RemoveNotDurable { .. }
        )
    }

    /// Whether a pointer moved although the call failed. The snapshot that came first moved the
    /// working-copy commit, but its entry can be undone and the call can be repeated, so it counts
    /// only if its flush did not succeed (ADR-018). A snapshot that failed itself counts if it
    /// moved a pointer and stopped (`PartlyMoved`, `NotDurable`).
    pub fn pointer_moved(&self) -> bool {
        match self {
            Self::AfterSnapshot { late, .. } => late.is_some(),
            Self::Snapshot(error) => {
                matches!(&**error, SnapshotError::Update(update) if update.pointer_moved())
            }
            Self::PartlyMoved { .. }
            | Self::NotDurable { .. }
            | Self::RemovePartlyMoved { .. }
            | Self::RemoveNotDurable { .. } => true,
            _ => false,
        }
    }

    /// Whether the call wrote the entry of the add or of the removal: the entry that the snapshot
    /// before it wrote does not count, so `Snapshot` is `false` here, and `AfterSnapshot` is `true`
    /// only if the failure it wraps wrote an entry of its own. Such a failure that is wrapped
    /// because the flush of the snapshot failed is `true` for `pointer_moved` as well.
    pub fn wrote_its_entry(&self) -> bool {
        match self {
            Self::AfterSnapshot { source, .. } => source.wrote_its_entry(),
            Self::Snapshot(_) => false,
            _ => self.entry().is_some(),
        }
    }
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Update(error) => write!(f, "could not add the worktree: {error}"),
            Self::Refs(error) => {
                write!(f, "the references refuse the worktree: {error}")
            }
            Self::Read(error) => write!(f, "could not read a commit or a tree: {error}"),
            Self::Store(error) => write!(f, "could not store a commit: {error}"),
            Self::Checkout(error) => {
                write!(f, "the files of the worktree cannot be planned: {error}")
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
                 moved: {source}; remove the directory of the worktree, its state directory \
                 below `.stemma/worktrees`, and the new branch if the call made one, and run \
                 the command again",
                entry.number()
            ),
            Self::NotCleaned {
                cause,
                path,
                source,
            } => write!(
                f,
                "{cause}; the directory `{}` that the call made could not be removed: {source}",
                path.display()
            ),
            Self::NotDurable { entry, source } => write!(
                f,
                "the worktree was added as entry {}, but a directory could not be flushed, so \
                 it may not survive a crash: {source}",
                entry.number()
            ),
            Self::NoSuchWorktree(name) => write!(
                f,
                "there is no linked worktree `{}`; `worktree list` shows the names and the paths",
                name.escape_debug()
            ),
            Self::RemoveMain => f.write_str(
                "the main worktree holds the shared store and cannot be removed; remove the \
                 linked worktrees and delete the repository by hand",
            ),
            Self::RemoveCurrent(name) => write!(
                f,
                "the command runs in the worktree `{name}`, which it is to remove; run it in \
                 another worktree"
            ),
            Self::NotItsPointer { path, state } => write!(
                f,
                "`{}` does not hold the pointer file `.stemma` that names `{}`, so it is not the \
                 directory of this worktree and nothing was removed; move or delete the \
                 directory by hand, and remove the state directory below `.stemma/worktrees` if \
                 the worktree is gone",
                path.display(),
                state.display()
            ),
            Self::Snapshot(error) => write!(
                f,
                "the files of the worktree could not be saved first, and nothing was removed: \
                 {error}"
            ),
            Self::AfterSnapshot {
                snapshot,
                late,
                source,
            } => {
                write!(
                    f,
                    "the files of the worktree were saved first as entry {}; {source}",
                    snapshot.number()
                )?;
                match late {
                    Some(late) => write!(
                        f,
                        "; the save could not be flushed to disk and may not survive a crash: \
                         {late}"
                    ),
                    None => Ok(()),
                }
            }
            Self::Dirty { path, listed, more } => {
                let names: Vec<String> = listed
                    .iter()
                    .map(|name| name.escape_debug().to_string())
                    .collect();
                write!(
                    f,
                    "`{}` holds files that the working-copy commit does not track ({}{}), and \
                     nothing was removed; track them, remove them, or run the command with \
                     `--force`",
                    path.display(),
                    names.join(", "),
                    if *more > 0 {
                        format!(", and {more} more")
                    } else {
                        String::new()
                    }
                )
            }
            Self::RemoveFailed {
                entry,
                path,
                source,
            } => write!(
                f,
                "entry {} was written, but the directory `{}` could not be removed and no pointer \
                 moved: {source}; remove the directory by hand if it is still there, and run the \
                 command again, which then removes the state directory only",
                entry.number(),
                path.display()
            ),
            Self::Unfinished(name) => write!(
                f,
                "`.stemma/worktrees/{name}` holds no worktree: it is what an add or a removal that \
                 stopped left; remove it by hand, or add a worktree in a directory called `{name}`, \
                 which takes the name over"
            ),
            Self::Damaged { name, source } => write!(
                f,
                "the state of the worktree `{name}` cannot be read: {source}; the other worktrees \
                 are not affected; remove the directory `.stemma/worktrees/{name}` and the \
                 directory of the worktree by hand"
            ),
            Self::SeveralDamaged { asked, damaged } => {
                match asked {
                    Some(asked) => write!(
                        f,
                        "there is no readable worktree `{}`, but the state of {} cannot be read",
                        asked.escape_debug(),
                        if damaged.len() == 1 { "a worktree" } else { "worktrees" }
                    )?,
                    None => f.write_str("the state of several worktrees cannot be read")?,
                }
                for (index, (name, source)) in damaged.iter().enumerate() {
                    f.write_str(if index == 0 { ": " } else { "; " })?;
                    write!(f, "`{}` ({source})", name.escape_debug())?;
                }
                if asked.is_some() {
                    f.write_str("; the worktree you named may be one of them")?;
                }
                f.write_str(
                    "; if the state is damaged (not just unreadable for now), remove the directory \
                     `.stemma/worktrees/NAME` and the directory of the worktree by hand",
                )
            }
            Self::RemoveMoveFailed { entry, source } => write!(
                f,
                "entry {} was written and the directory is gone, but the state directory is \
                 whole: {source}; run the command again, which removes the state directory only",
                entry.number()
            ),
            Self::RemovePartlyMoved { entry, source } => write!(
                f,
                "entry {} was written and the directory is gone, but the state directory is \
                 removed in part only: {source}; remove the rest of it below \
                 `.stemma/worktrees` by hand",
                entry.number()
            ),
            Self::RemoveNotDurable { entry, source } => write!(
                f,
                "the worktree was removed as entry {}, but a directory could not be flushed, so \
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
            | Self::NotDurable { source: error, .. }
            | Self::RemoveMoveFailed { source: error, .. }
            | Self::RemovePartlyMoved { source: error, .. }
            | Self::RemoveNotDurable { source: error, .. } => error.source(),
            Self::Snapshot(error) => error.source(),
            Self::AfterSnapshot { source, .. } | Self::Damaged { source, .. } => source.source(),
            Self::RemoveFailed { source, .. } => source.source(),
            Self::Read(error) => error.source(),
            Self::Store(error) => error.source(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.source(),
            Self::ChangeId(error) => error.source(),
            Self::Io { source, .. } => source.source(),
            Self::NotCleaned { cause, .. } => cause.source(),
            Self::PathNotUtf8(_)
            | Self::NotEmpty(_)
            | Self::PathInUse { .. }
            | Self::Nested { .. }
            | Self::NoSuchBranch(_)
            | Self::NoStart(_)
            | Self::NoSuchWorktree(_)
            | Self::Unfinished(_)
            // The causes are part of the message; each is one of the entries of the list.
            | Self::SeveralDamaged { .. }
            | Self::RemoveMain
            | Self::RemoveCurrent(_)
            | Self::NotItsPointer { .. }
            | Self::Dirty { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::refs::LockHolder;
    use crate::repository::InitOptions;
    use crate::testutil::TempDir;
    use crate::AllowAll;

    fn actor() -> Actor {
        Actor::human("daniel").unwrap()
    }

    fn locked() -> RefError {
        RefError::Locked {
            path: "x".into(),
            holder: LockHolder::Unknown,
        }
    }

    #[test]
    fn a_removal_that_stops_after_its_snapshot_names_it_and_the_flush_that_failed() {
        let dir = TempDir::new();
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
        let entry = Box::new(repository.oplog().read(1).unwrap());
        let dirty = || WorktreeError::Dirty {
            path: "/w".into(),
            listed: vec!["a.txt".to_owned()],
            more: 0,
        };

        let mut late = Some(locked());
        let flushed_badly = after_snapshot(&Some(entry.clone()), &mut late, dirty());
        assert!(late.is_none(), "the flush error moves into the failure");
        assert!(
            flushed_badly.pointer_moved(),
            "the saved work may not survive a crash"
        );
        assert!(!flushed_badly.wrote_its_entry());
        assert!(
            flushed_badly
                .to_string()
                .contains("could not be flushed to disk"),
            "{flushed_badly}"
        );

        let plain = after_snapshot(&Some(entry.clone()), &mut None, dirty());
        assert!(!plain.pointer_moved());
        assert!(!plain.wrote_its_entry());
        assert!(
            plain.entry().is_some(),
            "the entry of the snapshot is named"
        );
        assert!(!plain.to_string().contains("flushed"), "{plain}");

        // Without a snapshot the failure is returned as it is, and the entry of a removal that
        // failed after its own entry is not wrapped.
        assert!(matches!(
            after_snapshot(&None, &mut None, dirty()),
            WorktreeError::Dirty { .. }
        ));
        let removal = WorktreeError::RemoveFailed {
            entry: entry.clone(),
            path: "/w".into(),
            source: io::Error::other("busy"),
        };
        let kept = after_snapshot(&Some(entry.clone()), &mut None, removal);
        assert!(matches!(kept, WorktreeError::RemoveFailed { .. }));
        assert!(kept.wrote_its_entry());

        // If the flush of the snapshot failed as well, the failure is wrapped, and it still names
        // the entry of the removal and counts as its own entry.
        let removal = WorktreeError::RemoveFailed {
            entry: entry.clone(),
            path: "/w".into(),
            source: io::Error::other("busy"),
        };
        let both = after_snapshot(&Some(entry), &mut Some(locked()), removal);
        assert!(matches!(both, WorktreeError::AfterSnapshot { .. }));
        assert!(both.pointer_moved() && both.wrote_its_entry());
        assert!(both.entry().is_some());
    }

    #[test]
    fn a_snapshot_that_moved_a_pointer_before_it_failed_keeps_its_entry_and_code_3() {
        let dir = TempDir::new();
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
        let entry = Box::new(repository.oplog().read(1).unwrap());

        let partly =
            WorktreeError::Snapshot(Box::new(SnapshotError::Update(UpdateError::PartlyMoved {
                entry: entry.clone(),
                source: locked(),
            })));
        assert!(partly.pointer_moved());
        assert!(partly.entry().is_some());
        assert!(!partly.wrote_its_entry(), "the entry is the snapshot's");
        // The inner error names the entry; the wrapper must not name it a second time.
        assert_eq!(
            partly.to_string().matches("entry 1 ").count(),
            1,
            "{partly}"
        );

        // The pointer did not move, so the call can be repeated: code 1, not 3.
        let refused =
            WorktreeError::Snapshot(Box::new(SnapshotError::Update(UpdateError::MoveFailed {
                entry,
                source: locked(),
            })));
        assert!(!refused.pointer_moved());
        assert!(refused.entry().is_some());
        assert!(!refused.wrote_its_entry());
    }

    #[test]
    fn a_new_branch_with_a_reserved_prefix_is_refused_and_adds_no_worktree() {
        let dir = TempDir::new();
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
        fs::write(dir.path().join("a.txt"), "one").unwrap();
        repository
            .track(&["a.txt".to_owned()], &actor(), &AllowAll)
            .unwrap();
        let _first = repository.commit("first", &actor(), &AllowAll).unwrap();
        let before = repository.oplog().entries().unwrap().len();
        let place = TempDir::new();
        let path = place.path().join("linked");

        let error = repository
            .add_worktree(
                &path,
                &WorktreeBranch::Create(BranchName::try_new("tags/x").unwrap()),
                None,
                &actor(),
                &AllowAll,
            )
            .unwrap_err();

        assert!(
            matches!(&error, WorktreeError::Update(UpdateError::ReservedName(_))),
            "{error}"
        );
        assert!(!path.exists());
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
    }

    #[test]
    fn a_dirty_directory_after_a_snapshot_is_reported_with_the_entry_of_the_snapshot() {
        let dir = TempDir::new();
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
        fs::write(dir.path().join("a.txt"), "one").unwrap();
        repository
            .track(&["a.txt".to_owned()], &actor(), &AllowAll)
            .unwrap();
        let _first = repository.commit("first", &actor(), &AllowAll).unwrap();
        let place = TempDir::new();
        let path = place.path().join("linked");
        let _added = repository
            .add_worktree(
                &path,
                &WorktreeBranch::Create(BranchName::try_new("feat").unwrap()),
                Some(&actor()),
                &actor(),
                &AllowAll,
            )
            .unwrap();
        fs::write(path.join("loose.txt"), "three").unwrap();
        // An unsaved tracked change makes the snapshot write an entry.
        fs::write(path.join("a.txt"), "changed").unwrap();

        let error = repository
            .remove_worktree("linked", false, &actor(), &AllowAll)
            .unwrap_err();

        assert!(
            matches!(error, WorktreeError::AfterSnapshot { late: None, .. }),
            "{error}"
        );
        assert!(!error.pointer_moved());
        assert!(!error.wrote_its_entry());
        assert!(path.join("loose.txt").exists(), "nothing was removed");
    }
}
