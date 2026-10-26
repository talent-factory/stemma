//! The difference between two commits, or between a commit and the working copy (ADR-010).
//!
//! [`Repository::diff`] names its sides as the operation log names them: a commit by its
//! identifier, and the working copy as `working-copy` with the identifier of its commit. Without
//! an operand it compares the working-copy commit with its parent, with one operand a commit with
//! the working copy, and with two operands the two commits. A side that is the working copy is
//! first saved as a snapshot, as [`Repository::status`] does, so that the diff shows the files as
//! they are now. The lines of a text file are compared as stored, with their line ends, by the
//! shortest edit script of [`crate::diff`]. A binary file is named without its lines.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use super::{Repository, SnapshotError, UpdateError};
use crate::diff::{diff, hunks, is_binary, Hunk};
use crate::object::{Commit, CommitId, EntryMode, Kind, ObjectId, Tree, TreeId};
use crate::refs::RefError;
use crate::store::{ObjectStore, RawObject, StoreError};
use crate::{Actor, Authorizer};

/// The unchanged lines on either side of a change that a hunk shows, as git shows them by default.
const CONTEXT: usize = 3;

/// One side of a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Side {
    /// A commit, named by its identifier.
    Commit(CommitId),
    /// The working-copy commit, named by its identifier (the snapshot identifier).
    WorkingCopy(CommitId),
}

impl Side {
    /// Returns the identifier of the commit behind the side.
    pub fn commit(self) -> CommitId {
        match self {
            Self::Commit(id) | Self::WorkingCopy(id) => id,
        }
    }
}

impl fmt::Display for Side {
    /// Writes the side as the operation log writes it: `<id>`, or `working-copy <id>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Commit(id) => write!(f, "{id}"),
            Self::WorkingCopy(id) => write!(f, "working-copy {id}"),
        }
    }
}

/// The commits that a diff compares, as the command line gives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Comparison<'a> {
    /// The working-copy commit against its parent: the state that `status` summarises.
    WithParent,
    /// The commit with this identifier against the working-copy commit.
    WithWorkingCopy(&'a str),
    /// The first commit against the second.
    Commits(&'a str, &'a str),
}

/// The text of one file that differs: its lines on both sides, and the hunks that show the
/// changes between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDiff {
    old: Vec<String>,
    new: Vec<String>,
    hunks: Vec<Hunk>,
}

impl TextDiff {
    /// Builds the text of a difference. Every index of a hunk refers to `old` or `new`.
    fn new(old: Vec<String>, new: Vec<String>, hunks: Vec<Hunk>) -> Self {
        Self { old, new, hunks }
    }

    /// Returns the lines of the old side as they are stored, with their line ends. The list is
    /// empty if the file is new.
    pub fn old_lines(&self) -> &[String] {
        &self.old
    }

    /// Returns the lines of the new side as they are stored, with their line ends. The list is
    /// empty if the file is deleted.
    pub fn new_lines(&self) -> &[String] {
        &self.new
    }

    /// Returns the changes with their context, in the order of the file.
    pub fn hunks(&self) -> &[Hunk] {
        &self.hunks
    }
}

/// What differs in a file that is on at least one side.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Content {
    /// The file is binary on at least one side, so its lines are not shown.
    Binary,
    /// The file is text.
    Text(TextDiff),
}

/// A file that differs between the two sides of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    path: String,
    content: Content,
    old_exists: bool,
    new_exists: bool,
}

impl FileDiff {
    /// Builds the entry of one file. The two flags tell whether the file is on each side; an
    /// empty file is on its side, so it is not the same as a file that is not there.
    fn new(path: String, content: Content, old_exists: bool, new_exists: bool) -> Self {
        Self {
            path,
            content,
            old_exists,
            new_exists,
        }
    }

    /// Tells whether the file is on the old side. It is not, if the file was added.
    pub fn old_exists(&self) -> bool {
        self.old_exists
    }

    /// Tells whether the file is on the new side. It is not, if the file was deleted.
    pub fn new_exists(&self) -> bool {
        self.new_exists
    }

    /// Returns the path, relative to the root of the working copy (the directory that contains
    /// `.stemma`), and written with `/`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns what differs in the file.
    pub fn content(&self) -> &Content {
        &self.content
    }
}

/// The difference between two sides, with the files that differ, sorted by path.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diff {
    old: Option<Side>,
    new: Side,
    files: Vec<FileDiff>,
}

impl Diff {
    /// Returns the old side, or `None` if the working-copy commit has no parent and so the old
    /// side is the empty tree. The old side is `None` only for [`Comparison::WithParent`].
    pub fn old_side(&self) -> Option<Side> {
        self.old
    }

    /// Returns the new side.
    pub fn new_side(&self) -> Side {
        self.new
    }

    /// Returns the files that differ, sorted by path. A file that is equal on both sides is not
    /// in the list.
    pub fn files(&self) -> &[FileDiff] {
        &self.files
    }
}

/// Why [`Repository::diff`] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum DiffError {
    /// The working copy could not be saved as a snapshot before the diff was read.
    Snapshot(SnapshotError),
    /// The working-copy reference could not be read.
    Refs(RefError),
    /// The store could not read an object that the diff needs.
    Store(StoreError),
    /// An object that the diff needs is not in the store.
    Missing {
        /// The identifier of the object.
        id: ObjectId,
    },
    /// An object that the diff needs is in the store, but it does not parse.
    Damaged {
        /// The identifier of the object.
        id: ObjectId,
        /// Why the object does not parse.
        reason: String,
    },
    /// An operand is not the identifier of a commit in the store: it is not an identifier, or it
    /// names no object, or it names an object of another kind.
    UnknownCommit {
        /// The operand as it was given.
        operand: String,
    },
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot(error) => write!(f, "the working copy could not be saved: {error}"),
            Self::Refs(error) => write!(f, "the references could not be read: {error}"),
            Self::Store(error) => write!(f, "an object could not be read from the store: {error}"),
            Self::Missing { id } => write!(f, "object {id} is missing from the store"),
            Self::Damaged { id, reason } => write!(f, "object {id} is damaged: {reason}"),
            Self::UnknownCommit { operand } => {
                write!(
                    f,
                    "`{operand}` is not the identifier of a commit in this repository"
                )
            }
        }
    }
}

impl Error for DiffError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Snapshot(error) => Some(error),
            Self::Refs(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Missing { .. } | Self::Damaged { .. } | Self::UnknownCommit { .. } => None,
        }
    }
}

impl DiffError {
    /// Tells whether the snapshot moved the working-copy commit, but could not flush the move, so
    /// the diff was not written: the same case as `stemma status` reports with exit code 3.
    pub fn not_durable(&self) -> bool {
        matches!(
            self,
            Self::Snapshot(SnapshotError::Update(UpdateError::NotDurable { .. }))
        )
    }
}

impl Repository {
    /// Compares two sides as `comparison` names them, and returns the files that differ.
    ///
    /// A working-copy side is saved first as a snapshot, which is authorized and written as in
    /// [`Repository::snapshot`]. The commits are read from the store; an operand that is not a
    /// commit identifier, or names no commit in the store, is [`DiffError::UnknownCommit`].
    ///
    /// # Errors
    ///
    /// Returns [`DiffError::Snapshot`] if the snapshot fails, [`DiffError::Refs`] if the
    /// working-copy reference cannot be read, [`DiffError::UnknownCommit`] for an operand that
    /// names no commit, and [`DiffError::Store`], [`DiffError::Missing`] or
    /// [`DiffError::Damaged`] if a commit, a tree, or a blob that the diff needs cannot be read.
    pub fn diff(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
        comparison: Comparison<'_>,
    ) -> Result<Diff, DiffError> {
        let (old, new) = match comparison {
            Comparison::WithParent => {
                let working = self.working_copy_side(actor, authorizer)?;
                let parent = commit_of(self.objects(), working.commit())?
                    .parents()
                    .first()
                    .copied();
                (parent.map(Side::Commit), working)
            }
            Comparison::WithWorkingCopy(commit) => {
                let old = Side::Commit(self.commit_operand(commit)?);
                (Some(old), self.working_copy_side(actor, authorizer)?)
            }
            Comparison::Commits(first, second) => {
                let old = Side::Commit(self.commit_operand(first)?);
                (Some(old), Side::Commit(self.commit_operand(second)?))
            }
        };
        let old_files = match old {
            Some(side) => self.files_of(side.commit())?,
            None => BTreeMap::new(),
        };
        let new_files = self.files_of(new.commit())?;
        let paths: BTreeSet<&String> = old_files.keys().chain(new_files.keys()).collect();
        let mut files = Vec::new();
        for path in paths {
            let before = old_files.get(path).copied();
            let after = new_files.get(path).copied();
            if before == after {
                continue;
            }
            let old_bytes = self.blob_or_empty(before)?;
            let new_bytes = self.blob_or_empty(after)?;
            let content = if is_binary(&old_bytes) || is_binary(&new_bytes) {
                Content::Binary
            } else {
                // The lines are compared as bytes, with their line ends, so that a change of the
                // line ends or of a byte that is not UTF-8 shows. Only the display decodes them.
                let old_lines = split_lines(&old_bytes);
                let new_lines = split_lines(&new_bytes);
                let edits = diff(&old_lines, &new_lines);
                Content::Text(TextDiff::new(
                    decode_lines(&old_lines),
                    decode_lines(&new_lines),
                    hunks(&edits, CONTEXT),
                ))
            };
            files.push(FileDiff::new(
                path.clone(),
                content,
                before.is_some(),
                after.is_some(),
            ));
        }
        Ok(Diff { old, new, files })
    }

    /// Saves the working copy as a snapshot, and returns its commit as the working-copy side.
    fn working_copy_side(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Side, DiffError> {
        self.snapshot(actor, authorizer)
            .map_err(DiffError::Snapshot)?;
        let id = self.refs().working_copy().map_err(DiffError::Refs)?;
        Ok(Side::WorkingCopy(id))
    }

    /// Reads an operand as the identifier of a commit. Only an object of the commit kind counts;
    /// a tree or a blob, and an absent object, are [`DiffError::UnknownCommit`].
    fn commit_operand(&self, operand: &str) -> Result<CommitId, DiffError> {
        let unknown = || DiffError::UnknownCommit {
            operand: operand.to_owned(),
        };
        let id: ObjectId = operand.parse().map_err(|_| unknown())?;
        match self.objects().get_expecting(&id, Kind::Commit) {
            Ok(Some(_)) => Ok(CommitId::new(id)),
            Ok(None) | Err(StoreError::KindMismatch { .. }) => Err(unknown()),
            Err(error) => Err(DiffError::Store(error)),
        }
    }

    /// Lists the files of the tree of a commit with the IDs of their blobs.
    fn files_of(&self, commit: CommitId) -> Result<BTreeMap<String, ObjectId>, DiffError> {
        let tree = commit_of(self.objects(), commit)?.tree();
        files(self.objects(), tree)
    }

    /// Reads the content of a blob, or the empty content for a file that a side lacks.
    fn blob_or_empty(&self, id: Option<ObjectId>) -> Result<Vec<u8>, DiffError> {
        let Some(id) = id else {
            return Ok(Vec::new());
        };
        Ok(read_raw(self.objects(), id, Kind::Blob)?.body().to_vec())
    }
}

/// Reads an object of the kind, or fails if the store lacks it.
fn read_raw(objects: &dyn ObjectStore, id: ObjectId, kind: Kind) -> Result<RawObject, DiffError> {
    objects
        .get_expecting(&id, kind)
        .map_err(DiffError::Store)?
        .ok_or(DiffError::Missing { id })
}

/// Reads and parses a commit.
fn commit_of(objects: &dyn ObjectStore, id: CommitId) -> Result<Commit, DiffError> {
    let raw = read_raw(objects, id.object_id(), Kind::Commit)?;
    Commit::parse(raw.body()).map_err(|error| DiffError::Damaged {
        id: id.object_id(),
        reason: error.to_string(),
    })
}

/// Lists the files of a tree, depth first, with the IDs of their blobs. A directory is read and
/// not listed; the mode of a file is not compared, so only a change of content shows.
fn files(objects: &dyn ObjectStore, root: TreeId) -> Result<BTreeMap<String, ObjectId>, DiffError> {
    let mut found = BTreeMap::new();
    let mut pending = vec![(String::new(), root)];
    while let Some((prefix, id)) = pending.pop() {
        let raw = read_raw(objects, id.object_id(), Kind::Tree)?;
        let tree = Tree::parse(raw.body()).map_err(|error| DiffError::Damaged {
            id: id.object_id(),
            reason: error.to_string(),
        })?;
        for entry in tree.entries() {
            let path = if prefix.is_empty() {
                entry.name().to_owned()
            } else {
                format!("{prefix}/{}", entry.name())
            };
            match entry.mode() {
                EntryMode::Directory => pending.push((path, TreeId::new(entry.id()))),
                _ => {
                    found.insert(path, entry.id());
                }
            }
        }
    }
    Ok(found)
}

/// Splits a text into its lines, each with its line end. The last line has no line end if the
/// text does not end with one. The lines are kept as bytes, so no byte is changed.
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split_inclusive(|byte| *byte == b'\n').collect()
}

/// Decodes lines for display. A byte that is not UTF-8 is shown as the replacement character.
fn decode_lines(lines: &[&[u8]]) -> Vec<String> {
    lines
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect()
}
