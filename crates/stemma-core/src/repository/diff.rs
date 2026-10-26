//! The files whose content differs between two trees, with the content on both sides (`stemma
//! diff`, chapter 5). The comparison reads the trees and the blobs that differ; it writes nothing.

use std::collections::{BTreeMap, BTreeSet};

use super::snapshot::{read_commit, read_object, SnapshotError};
use super::Repository;
use crate::diff::is_binary;
use crate::object::{BlobId, CommitId, EntryMode, FileMode, Kind, Tree, TreeId};
use crate::store::ObjectStore;

/// A file whose content differs between the old and the new side of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// The path of the file, with `/` between the names.
    pub path: String,
    /// The content in the old tree, or `None` if the file is not there.
    pub old: Option<Vec<u8>>,
    /// The content in the new tree, or `None` if the file is not there.
    pub new: Option<Vec<u8>>,
}

impl FileDiff {
    /// Whether one side is a binary file, so that its content is not shown.
    pub fn is_binary(&self) -> bool {
        self.old.as_deref().is_some_and(is_binary) || self.new.as_deref().is_some_and(is_binary)
    }
}

impl Repository {
    /// The tree of a commit.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] if the commit is missing or damaged.
    pub fn commit_tree(&self, id: CommitId) -> Result<TreeId, SnapshotError> {
        read_commit(self.objects(), id).map(|commit| commit.tree())
    }

    /// The files whose content differs between the trees `old` and `new`, sorted by path. `None`
    /// is the empty tree. A file that differs only in its mode is not listed, because the diff
    /// shows content only.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] if a tree or a blob is missing or damaged.
    pub fn file_diffs(
        &self,
        old: Option<TreeId>,
        new: TreeId,
    ) -> Result<Vec<FileDiff>, SnapshotError> {
        let objects = self.objects();
        let old_files = match old {
            Some(root) => blob_files(objects, root)?,
            None => BTreeMap::new(),
        };
        let new_files = blob_files(objects, new)?;
        let paths: BTreeSet<&String> = old_files.keys().chain(new_files.keys()).collect();
        let mut diffs = Vec::new();
        for path in paths {
            let before = old_files.get(path).copied();
            let after = new_files.get(path).copied();
            if before == after {
                continue;
            }
            diffs.push(FileDiff {
                path: path.clone(),
                old: before.map(|blob| content(objects, blob)).transpose()?,
                new: after.map(|blob| content(objects, blob)).transpose()?,
            });
        }
        Ok(diffs)
    }
}

/// The files of a tree, with the ID of their blob, keyed by path. Directories are walked; a
/// symbolic link or another entry that is not a file is left out, as the status leaves it out.
fn blob_files(
    objects: &dyn ObjectStore,
    root: TreeId,
) -> Result<BTreeMap<String, BlobId>, SnapshotError> {
    let mut files = BTreeMap::new();
    let mut pending = vec![(String::new(), root)];
    while let Some((prefix, id)) = pending.pop() {
        let raw = read_object(objects, id.object_id(), Kind::Tree)?;
        let tree = Tree::parse(raw.body()).map_err(|source| SnapshotError::Tree { id, source })?;
        for entry in tree.entries() {
            let path = if prefix.is_empty() {
                entry.name().to_owned()
            } else {
                format!("{prefix}/{}", entry.name())
            };
            if entry.mode() == EntryMode::Directory {
                pending.push((path, TreeId::new(entry.id())));
            } else if FileMode::from_entry_mode(entry.mode()).is_some() {
                files.insert(path, BlobId::new(entry.id()));
            }
        }
    }
    Ok(files)
}

/// The content of a blob.
fn content(objects: &dyn ObjectStore, blob: BlobId) -> Result<Vec<u8>, SnapshotError> {
    read_object(objects, blob.object_id(), Kind::Blob).map(|raw| raw.into_body())
}
