//! The removal of unreachable objects on a bare repository (ADR-007, STEMMA-292):
//! [`Repository::remove_unreachable_objects`]. The rules stand in the documentation of that
//! method, which is the public one.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use super::accept::RecordDestination;
use super::Repository;
use crate::object::{CommitId, EntryMode, Kind, ObjectId, TreeId};
use crate::protocol::record::{read_record, RecordParseError};
use crate::refs::RefError;
use crate::store::{ObjectStore, ReadError, StoreError};

/// What [`Repository::remove_unreachable_objects`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedObjects {
    removed: Vec<ObjectId>,
    young: usize,
    reachable: usize,
}

impl RemovedObjects {
    /// The objects that were removed, in the order of the removal.
    pub fn removed(&self) -> &[ObjectId] {
        &self.removed
    }

    /// The number of unreachable objects that were kept because their file is younger than the
    /// grace period.
    pub fn young(&self) -> usize {
        self.young
    }

    /// The number of objects that the roots reach: commits, trees, and the blobs that the trees
    /// name, whether the store has these blobs or not.
    pub fn reachable(&self) -> usize {
        self.reachable
    }
}

impl Repository {
    /// Removes the objects that no reference reaches and whose file is older than `grace`: the
    /// cleanup of a bare repository on a hub (ADR-007). A hub starts it rarely, for example once a
    /// day.
    ///
    /// The roots are the current branches and tags alone. The record of ref updates is no root,
    /// so a history that a reference left by a rewrite or a deletion goes (ADR-025). The one
    /// exception is the last entry of the record: an update whose entry was written and whose
    /// references did not move yet is completed by the next accepting call (ADR-025), so its new
    /// commits and its register count as roots. `record` is the record of this repository, the
    /// one that the hub passes to [`accept_ref_update`](Self::accept_ref_update).
    ///
    /// The walk reads every commit and every tree that the roots reach, with the strict readers
    /// (ADR-005), and no blob: a blob that a tree names is kept by its ID. An object that the walk
    /// does not reach is unnamed, whatever stored it: a refused upload, an upload whose ref update
    /// has not come yet, or a computation of the hub such as a merge of the gate (ADR-025). It is
    /// removed when its file is at least `grace` old by its time of modification, and kept
    /// otherwise; a file with a time after the clock of the system is young. The grace period
    /// spans the time between an upload ([`receive_objects`](Self::receive_objects)), which takes
    /// no lock, and its ref update. One that is too short costs a repeated upload and no data:
    /// the ref update reports the objects as missing, and the client uploads them again.
    ///
    /// The call holds the lock of the references from before it reads them to its end, so no ref
    /// update and no other writer of the repository names an object in between. It takes the lock
    /// itself and never waits for it. It asks no authorizer: it is an operation of the hub and not
    /// of an actor, like [`receive_objects`](Self::receive_objects).
    ///
    /// It refuses a repository with a working copy: there the operation log, the working copies,
    /// and the remote references are roots as well, and a garbage collection of it comes with the
    /// retention of snapshot entries (ADR-011).
    ///
    /// # Errors
    ///
    /// [`UnreachableError::NotBare`] for a repository with a working copy,
    /// [`UnreachableError::Refs`] if the lock is held ([`RefError::Locked`]) or the references
    /// cannot be read, [`UnreachableError::RecordIo`] and [`UnreachableError::Record`] if the
    /// record cannot be read or is damaged, [`UnreachableError::Missing`] and
    /// [`UnreachableError::Read`] if a commit or a tree that the roots reach is not in the store or
    /// cannot be read, and [`UnreachableError::Store`] if the objects cannot be listed, inspected,
    /// or removed. Every error but the last comes before the first removal. After an error of the
    /// last kind the objects that were removed stay removed: none of them was reachable.
    pub fn remove_unreachable_objects(
        &self,
        record: &mut dyn RecordDestination,
        grace: Duration,
    ) -> Result<RemovedObjects, UnreachableError> {
        self.remove_unreachable_objects_at(record, grace, SystemTime::now())
    }

    /// [`remove_unreachable_objects`](Self::remove_unreachable_objects) with the time of the
    /// clock as a parameter. The tests pass a later time in place of waiting for the grace period.
    pub(crate) fn remove_unreachable_objects_at(
        &self,
        record: &mut dyn RecordDestination,
        grace: Duration,
        now: SystemTime,
    ) -> Result<RemovedObjects, UnreachableError> {
        if !self.is_bare() {
            return Err(UnreachableError::NotBare {
                path: self.path().to_path_buf(),
            });
        }
        let _lock = self.refs_store().lock_held().map_err(refs)?;

        let view = self.refs();
        let mut roots: Vec<CommitId> = view
            .branches()
            .map_err(refs)?
            .into_iter()
            .chain(view.tags().map_err(refs)?)
            .map(|(_, commit)| commit)
            .collect();
        let bytes = record.read().map_err(UnreachableError::RecordIo)?;
        let entries =
            read_record(&bytes).map_err(|error| UnreachableError::Record(Box::new(error)))?;
        if let Some(last) = entries.last() {
            roots.extend(
                last.update()
                    .payload()
                    .moves()
                    .iter()
                    .filter_map(|moved| moved.new_commit()),
            );
            roots.push(last.register());
        }

        let reachable = self.reachable_from(roots)?;
        let mut removed = Vec::new();
        let mut young = 0;
        let objects = self.objects();
        let failed = |removed: &Vec<ObjectId>| {
            let removed = removed.len();
            move |error| UnreachableError::Store {
                source: Box::new(error),
                removed,
            }
        };
        for id in objects.ids().map_err(failed(&removed))? {
            if reachable.contains(&id) {
                continue;
            }
            let path = objects.object_path(&id);
            let modified = match fs::symlink_metadata(&path).and_then(|data| data.modified()) {
                Ok(modified) => modified,
                // Removed since the listing, by nothing in this crate.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(failed(&removed)(StoreError::Io {
                        operation: "read the time of the object file",
                        path,
                        source: error,
                    }));
                }
            };
            // A time after the clock is young: the age is not known.
            let old = now.duration_since(modified).is_ok_and(|age| age >= grace);
            if !old {
                young += 1;
                continue;
            }
            if objects.remove(&id).map_err(failed(&removed))? {
                removed.push(id);
            }
        }
        Ok(RemovedObjects {
            removed,
            young,
            reachable: reachable.len(),
        })
    }

    /// The commits, the trees, and the IDs of the blobs that the roots reach, through the parents
    /// and the trees of the commits and the entries of the trees. Blobs are not read.
    fn reachable_from(&self, roots: Vec<CommitId>) -> Result<HashSet<ObjectId>, UnreachableError> {
        let objects = self.objects();
        let mut seen = HashSet::new();
        let mut commits: Vec<(CommitId, Option<ObjectId>)> =
            roots.into_iter().map(|root| (root, None)).collect();
        let mut trees: Vec<(TreeId, ObjectId)> = Vec::new();
        while let Some((id, needed_by)) = commits.pop() {
            if !seen.insert(id.object_id()) {
                continue;
            }
            let commit = objects
                .get_any_commit(&id)
                .map_err(read(needed_by))?
                .ok_or(UnreachableError::Missing {
                    id: id.object_id(),
                    kind: Kind::Commit,
                    needed_by,
                })?;
            let by = Some(id.object_id());
            commits.extend(commit.parents().iter().map(|parent| (*parent, by)));
            trees.extend(
                commit
                    .tree_sum()
                    .terms()
                    .iter()
                    .map(|tree| (*tree, id.object_id())),
            );
            // The trees of this commit are walked before the next commit, so that the stack of
            // trees stays small.
            while let Some((tree_id, needed_by)) = trees.pop() {
                if !seen.insert(tree_id.object_id()) {
                    continue;
                }
                let tree = objects
                    .get_tree(&tree_id)
                    .map_err(read(Some(needed_by)))?
                    .ok_or(UnreachableError::Missing {
                        id: tree_id.object_id(),
                        kind: Kind::Tree,
                        needed_by: Some(needed_by),
                    })?;
                for entry in tree.entries() {
                    match entry.mode() {
                        EntryMode::Directory => {
                            trees.push((TreeId::new(entry.id()), tree_id.object_id()));
                        }
                        EntryMode::File | EntryMode::Executable | EntryMode::Symlink => {
                            seen.insert(entry.id());
                        }
                    }
                }
            }
        }
        Ok(seen)
    }
}

/// Why [`Repository::remove_unreachable_objects`] failed. The errors inside are boxed, because
/// the unboxed ones are too large for clippy's `result_large_err` lint.
#[derive(Debug)]
#[non_exhaustive]
pub enum UnreachableError {
    /// The repository has a working copy (see the method).
    NotBare {
        /// The repository.
        path: PathBuf,
    },
    /// The lock of the references is held, or the references cannot be read.
    Refs(Box<RefError>),
    /// The record of ref updates cannot be read.
    RecordIo(io::Error),
    /// The record of ref updates is damaged.
    Record(Box<RecordParseError>),
    /// A commit or a tree that the roots reach is not in the store. The repository is damaged,
    /// and nothing was removed.
    Missing {
        /// The object that is absent.
        id: ObjectId,
        /// The kind that the object must have. [`Kind::Commit`] stands for a commit of either
        /// kind.
        kind: Kind,
        /// The commit or the tree that names it, or `None` for a root.
        needed_by: Option<ObjectId>,
    },
    /// A commit or a tree that the roots reach cannot be read: the storage failed, its file is
    /// damaged, it has another kind, or the strict reader refuses its body. Nothing was removed.
    Read {
        /// The commit or the tree that names it, or `None` for a root.
        needed_by: Option<ObjectId>,
        /// The error of the read, which names the object.
        source: Box<ReadError>,
    },
    /// The objects cannot be listed, the time of a file cannot be read, or a file cannot be
    /// removed.
    Store {
        /// The error of the store.
        source: Box<StoreError>,
        /// The number of objects that were removed before the error. They stay removed: none of
        /// them was reachable.
        removed: usize,
    },
}

impl UnreachableError {
    /// The error of the storage at the root of this error, if there is one.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::RecordIo(source) => Some(source),
            Self::Refs(source) => source.io_error(),
            Self::Read { source, .. } => source.store_error().and_then(StoreError::io_error),
            Self::Store { source, .. } => source.io_error(),
            Self::NotBare { .. } | Self::Record(_) | Self::Missing { .. } => None,
        }
    }
}

impl fmt::Display for UnreachableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotBare { path } => write!(
                f,
                "{} has a working copy; unreachable objects are removed from a bare repository \
                 only",
                path.display()
            ),
            Self::Refs(error) => write!(f, "{error}"),
            Self::RecordIo(error) => write!(f, "cannot read the record of ref updates: {error}"),
            Self::Record(error) => write!(f, "the record of ref updates is damaged: {error}"),
            Self::Missing {
                id,
                kind,
                needed_by,
            } => {
                // The walk looks for commits and trees only.
                let kind = if *kind == Kind::Tree {
                    "tree"
                } else {
                    "commit"
                };
                match needed_by {
                    Some(by) => write!(f, "the {kind} {id} that {by} names is missing")?,
                    None => write!(f, "the commit {id} of a root is missing")?,
                }
                write!(f, "; nothing was removed")
            }
            Self::Read { needed_by, source } => {
                write!(f, "{source}")?;
                if let Some(by) = needed_by {
                    write!(f, " ({by} names the object)")?;
                }
                write!(f, "; nothing was removed")
            }
            Self::Store { source, removed } => {
                write!(
                    f,
                    "{source}; {removed} unreachable objects were removed before"
                )
            }
        }
    }
}

impl Error for UnreachableError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message holds the cause itself, so the chain starts below it.
            Self::RecordIo(source) => source.source(),
            Self::Refs(source) => source.source(),
            Self::Read { source, .. } => source.source(),
            Self::Store { source, .. } => source.source(),
            Self::Record(_) | Self::NotBare { .. } | Self::Missing { .. } => None,
        }
    }
}

fn refs(error: RefError) -> UnreachableError {
    UnreachableError::Refs(Box::new(error))
}

fn read(needed_by: Option<ObjectId>) -> impl FnOnce(ReadError) -> UnreachableError {
    move |error| UnreachableError::Read {
        needed_by,
        source: Box::new(error),
    }
}
