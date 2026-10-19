//! The index: the set of files the next commit will contain.
//!
//! The index maps a relative path to a file mode and the ID of the blob that holds the content.
//! It is a plain in-memory model with a canonical text encoding. It never touches the file system.
//! Only [`Index::verify`] and [`Index::write_tree`] use an object store.
//!
//! Paths are `/`-separated and relative. Each component must satisfy the same rules as a tree
//! entry name (ADR-005). Names that are dangerous on Windows, such as `\` and `:`, are accepted
//! here and are the checkout's responsibility, as for trees. The index holds files and symbolic
//! links only ([`FileMode`]): a directory exists because a path runs through it, never as an
//! entry of its own. A path therefore cannot be both a file and a directory. The check is
//! byte-exact: `Foo` and `foo/bar` do not conflict, and neither do two spellings of the same
//! name in different Unicode normalization forms. A checkout on a case-insensitive or
//! normalizing file system has to detect such collisions.
//!
//! The encoding is version 1 and starts with the line `stemma-index 1`. Each entry is one line
//! `<mode> <id> <path>`, for example `100644 b3:9db8…75ad src/main.rs`, where `<mode>` is
//! `100644`, `100755`, or `120000` and `<id>` is the textual object ID. Entries are sorted
//! component by component: the path is split at `/` and the components are compared in UTF-8 byte
//! order, so a shorter path that is a prefix of another comes first. `a/z`, `a-b`, and `a.b`
//! appear in this order, because the component `a` sorts before `a-b`. That is the order of a
//! depth-first walk through trees, which sort the names of one directory in UTF-8 byte order
//! (ADR-005), so an index and a tree can be compared in one pass. The sort of the whole path
//! string would put `a-b` and `a.b` before `a/z`, and is not used. The path comes last
//! because it may contain spaces; it cannot contain a newline. The reader is strict: it rejects
//! anything that [`Index::encode`] would not have written, so one index has exactly one
//! encoding.
//!
//! Reading and encoding use no store. The reader claims each ID as a blob, because every entry of
//! the index is a file, but it cannot know; [`Index::insert`] trusts the [`BlobId`] it is given.
//! [`Index::verify`] checks the claim against a store. [`Index::write_tree`], the builder of trees
//! from the index, calls it itself, so that no caller can forget the check. It does not read the
//! blobs again: if the store changes after the check, the tree is still written, and refers to
//! the blob that is no longer there.
//!
//! An incompatible change to the encoding raises the number in the header line. A reader reports
//! a header with another number as [`IndexParseError::UnsupportedVersion`] instead of reading
//! it as version 1. The index is a file, not an object, so the kind-name rule for objects does
//! not apply to it.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::ops::Bound;
use std::str::FromStr;

pub use crate::object::FileMode;
use crate::object::{
    name_problem, BlobId, EntryMode, NameProblem, ObjectId, ObjectIdParseError, Tree, TreeEntry,
    TreeError, TreeId,
};
use crate::store::{ObjectStore, StoreError};

/// First line of the encoding. It carries the format version.
const HEADER: &str = "stemma-index 1\n";

/// Start of the first line of every version of the encoding.
const HEADER_PREFIX: &str = "stemma-index ";

/// What is wrong with the lines of an index, as far as the shape goes. The `Display` form is the
/// message; a caller that wants to act on the reason matches on the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexMalformed {
    /// The last line has no terminating newline: the input is cut off.
    CutOff,
    /// An entry line is not of the form `<mode> <id> <path>`.
    EntryShape,
}

impl fmt::Display for IndexMalformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CutOff => "the last line has no terminating newline",
            Self::EntryShape => "expected `<mode> <id> <path>`",
        })
    }
}

/// The rule that a rejected index path violates.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathProblem {
    /// The path is empty, starts or ends with `/`, or contains `//`.
    EmptyComponent,
    /// A component violates a tree entry naming rule, which includes `.` and `..`. Paths are
    /// split at `/` and empty components are reported first, so [`NameProblem::Empty`] and
    /// [`NameProblem::PathSeparator`] cannot occur here.
    Name {
        /// The first component that violates a rule.
        component: String,
        /// The rule it violates.
        problem: NameProblem,
    },
}

impl fmt::Display for PathProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyComponent => f.write_str("the path has an empty component"),
            Self::Name { component, problem } => {
                write!(f, "component `{}`: {problem}", component.escape_debug())
            }
        }
    }
}

/// Reasons why an entry is rejected by [`Index::insert`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexError {
    /// The path violates a path rule.
    InvalidPath {
        /// The rejected path.
        path: String,
        /// The rule it violates.
        problem: PathProblem,
    },
    /// A parent directory of the path is a staged file, so the path would be below a file.
    ParentIsFile {
        /// The path that was inserted.
        path: String,
        /// The staged file that is a parent directory of the path.
        parent: String,
    },
    /// The path is a directory that holds staged files, so it cannot also be a file.
    HasChildren {
        /// The path that was inserted.
        path: String,
        /// The lowest-sorting staged file below the path.
        child: String,
    },
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { path, problem } => {
                write!(f, "invalid index path `{}`: {problem}", path.escape_debug())
            }
            Self::ParentIsFile { path, parent } => write!(
                f,
                "cannot stage `{}`: `{}` is a file, and a path cannot be both a file and a directory",
                path.escape_debug(),
                parent.escape_debug()
            ),
            Self::HasChildren { path, child } => write!(
                f,
                "cannot stage `{}` as a file: `{}` is below it, and a path cannot be both a file and a directory",
                path.escape_debug(),
                child.escape_debug()
            ),
        }
    }
}

impl Error for IndexError {}

/// Reasons why an encoded index is rejected by [`Index::parse`].
///
/// Line numbers count from 1 and include the header, so the first entry is on line 2.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexParseError {
    /// The input is not valid UTF-8.
    NotUtf8 {
        /// Number of bytes from the start that are valid UTF-8.
        valid_up_to: usize,
    },
    /// The input does not start with the version header: it is empty, it is something else, or
    /// the header line is cut off or not canonical (for example it ends in `\r\n`).
    MissingHeader,
    /// The input starts with the header of another version. Carries the version number.
    UnsupportedVersion(String),
    /// An entry line does not have the shape `<mode> <id> <path>`, or the last line has no
    /// terminating newline.
    Malformed {
        /// Line number of the entry.
        line: usize,
        /// What is wrong with it.
        reason: IndexMalformed,
    },
    /// The mode is not one of `100644`, `100755`, and `120000`. This includes the tree mode
    /// `040000`, because the index holds no directories.
    UnknownMode {
        /// Line number of the entry.
        line: usize,
        /// The mode as written.
        code: String,
    },
    /// The object ID is not in canonical form.
    InvalidObjectId {
        /// Line number of the entry.
        line: usize,
        /// Why the ID is invalid.
        source: ObjectIdParseError,
    },
    /// The path is not greater than the path before it. A duplicate is also reported this way.
    OutOfOrder {
        /// Line number of the entry.
        line: usize,
        /// The path on the line before.
        previous: String,
        /// The path on this line.
        path: String,
    },
    /// An entry line is well formed, but the entry is rejected.
    Entry {
        /// Line number of the entry.
        line: usize,
        /// Why the entry was rejected.
        source: IndexError,
    },
}

impl fmt::Display for IndexParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8 { valid_up_to } => {
                write!(
                    f,
                    "the index is not valid UTF-8 (valid up to byte {valid_up_to})"
                )
            }
            Self::MissingHeader => f.write_str("the index does not start with `stemma-index 1`"),
            Self::UnsupportedVersion(version) => write!(
                f,
                "the index has version {version}, but only version 1 is supported"
            ),
            Self::Malformed { line, reason } => write!(f, "line {line}: {reason}"),
            Self::UnknownMode { line, code } => {
                write!(f, "line {line}: unknown mode `{}`", code.escape_debug())
            }
            Self::InvalidObjectId { line, source } => {
                write!(f, "line {line}: invalid object ID: {source}")
            }
            Self::OutOfOrder {
                line,
                previous,
                path,
            } => write!(
                f,
                "line {line}: `{}` does not sort after `{}`; paths must be strictly ascending",
                path.escape_debug(),
                previous.escape_debug()
            ),
            Self::Entry { line, source } => write!(f, "line {line}: {source}"),
        }
    }
}

impl Error for IndexParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message of the variant includes the message of the cause, so the source is the
            // cause of the cause (see the crate documentation).
            Self::InvalidObjectId { source, .. } => source.source(),
            Self::Entry { source, .. } => source.source(),
            _ => None,
        }
    }
}

/// Why an index fails [`Index::verify`]. Carries the path of the first failing entry in index
/// order.
#[derive(Debug)]
#[non_exhaustive]
pub enum IndexVerifyError {
    /// The store has no object with the ID of the entry, for example because the blob was never
    /// stored.
    Missing {
        /// The path of the entry.
        path: String,
        /// The ID the entry names.
        id: BlobId,
    },
    /// The store could not resolve the ID as a blob: the object has another kind
    /// ([`StoreError::KindMismatch`]), it is damaged, or the storage failed.
    Store {
        /// The path of the entry.
        path: String,
        /// What the store reported. Boxed because the unboxed error is too large for clippy's
        /// `result_large_err` lint.
        source: Box<StoreError>,
    },
}

impl fmt::Display for IndexVerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { path, id } => {
                write!(f, "`{}`: the store has no object {id}", path.escape_debug())
            }
            Self::Store { path, source } => write!(f, "`{}`: {source}", path.escape_debug()),
        }
    }
}

impl Error for IndexVerifyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes that of the store error; see the crate documentation.
            Self::Store { source, .. } => source.source(),
            Self::Missing { .. } => None,
        }
    }
}

/// Why [`Index::write_tree`] fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum WriteTreeError {
    /// The index failed [`Index::verify`]. No tree was written.
    Verify(IndexVerifyError),
    /// The store could not write a tree. The function does not remove the trees of
    /// subdirectories that it wrote before; they are valid objects that nothing refers to yet.
    Store(Box<StoreError>),
    /// A tree could not be built from the entries. The rules of the index are those of a tree, so
    /// this does not happen for an index that was built with [`Index::insert`] or
    /// [`Index::parse`]; the variant keeps the function free of a panic.
    Tree(TreeError),
}

impl fmt::Display for WriteTreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Verify(source) => write!(f, "the index does not verify: {source}"),
            Self::Store(source) => write!(f, "cannot write a tree: {source}"),
            Self::Tree(source) => write!(f, "cannot build a tree: {source}"),
        }
    }
}

impl Error for WriteTreeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message of each variant includes the message of the cause, so the source is the
            // cause of the cause (see the crate documentation).
            Self::Verify(source) => source.source(),
            Self::Store(source) => source.source(),
            Self::Tree(source) => source.source(),
        }
    }
}

/// One staged file: its mode and the ID of its content blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    mode: FileMode,
    id: BlobId,
}

impl IndexEntry {
    /// Returns the file mode.
    pub fn mode(&self) -> FileMode {
        self.mode
    }

    /// Returns the ID of the content blob. In a parsed index it is only a claim that the ID names a
    /// blob, until [`Index::verify`] has checked it against a store.
    pub fn id(&self) -> BlobId {
        self.id
    }
}

/// Compares two paths component by component: the components, split at `/`, are compared in
/// UTF-8 byte order, and a path that is a prefix of another comes first. This is the order of a
/// depth-first walk through trees (ADR-005).
fn compare_paths(a: &str, b: &str) -> Ordering {
    a.split('/').cmp(b.split('/'))
}

/// A path as a map key, ordered by [`compare_paths`] and not by the bytes of the string.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PathKey(String);

impl Ord for PathKey {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_paths(&self.0, &other.0)
    }
}

impl PartialOrd for PathKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The staging area: staged files by path, sorted component by component.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    entries: BTreeMap<PathKey, IndexEntry>,
}

/// Returns the first path rule the path violates, if any.
fn path_problem(path: &str) -> Option<PathProblem> {
    for component in path.split('/') {
        if component.is_empty() {
            return Some(PathProblem::EmptyComponent);
        }
        if let Some(problem) = name_problem(component) {
            return Some(PathProblem::Name {
                component: component.to_owned(),
                problem,
            });
        }
    }
    None
}

/// Returns the version number if the text starts with the header of a version other than 1.
fn other_version(text: &str) -> Option<&str> {
    let first_line = text.split_once('\n').map_or(text, |(first, _)| first);
    let version = first_line.strip_prefix(HEADER_PREFIX)?;
    let is_number = !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit());
    (is_number && version != "1").then_some(version)
}

impl Index {
    /// Creates an empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stages a file, replacing the entry at the same path if there is one.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::InvalidPath`] if a path component violates a rule,
    /// [`IndexError::ParentIsFile`] if a parent directory of the path is a staged file, and
    /// [`IndexError::HasChildren`] if the path is a directory that holds staged files. The
    /// index is unchanged on error.
    pub fn insert(
        &mut self,
        path: impl Into<String>,
        mode: FileMode,
        id: BlobId,
    ) -> Result<(), IndexError> {
        let path = path.into();
        if let Some(problem) = path_problem(&path) {
            return Err(IndexError::InvalidPath { path, problem });
        }
        // No parent directory of the path may be a staged file.
        let staged_parent = path
            .match_indices('/')
            .map(|(end, _)| &path[..end])
            .find(|parent| self.entries.contains_key(&PathKey((*parent).to_owned())))
            .map(str::to_owned);
        if let Some(parent) = staged_parent {
            return Err(IndexError::ParentIsFile { path, parent });
        }
        // The path must not be a directory that holds staged files. The files below a directory
        // follow it directly in the component order, so the first key after the path starts
        // with `<path>/` exactly if there is such a file.
        let prefix = format!("{path}/");
        let key = PathKey(path);
        let first_after = self
            .entries
            .range((Bound::Excluded(&key), Bound::Unbounded))
            .next();
        if let Some((child, _)) = first_after {
            if child.0.starts_with(&prefix) {
                return Err(IndexError::HasChildren {
                    child: child.0.clone(),
                    path: key.0,
                });
            }
        }
        self.entries.insert(key, IndexEntry { mode, id });
        Ok(())
    }

    /// Removes the file at the path and returns its entry. Returns `None` if no file is staged
    /// at exactly this path; a directory name is not a path in the index, so removing one
    /// removes nothing.
    pub fn remove(&mut self, path: &str) -> Option<IndexEntry> {
        self.entries.remove(&PathKey(path.to_owned()))
    }

    /// Returns the entry at the path. Its ID is unverified until [`Index::verify`].
    pub fn get(&self, path: &str) -> Option<&IndexEntry> {
        self.entries.get(&PathKey(path.to_owned()))
    }

    /// Returns the number of staged files.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether nothing is staged.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates over the entries in path order, component by component; see the module
    /// documentation. The IDs are unverified until [`Index::verify`].
    pub fn iter(&self) -> impl Iterator<Item = (&str, &IndexEntry)> {
        self.entries
            .iter()
            .map(|(path, entry)| (path.0.as_str(), entry))
    }

    /// Returns the canonical encoding: the header line, then one line
    /// `<mode> <id> <path>` per entry in path order.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = String::from(HEADER);
        for (path, entry) in &self.entries {
            let mode = EntryMode::from(entry.mode);
            out.push_str(&format!("{} {} {}\n", mode.code(), entry.id, path.0));
        }
        out.into_bytes()
    }

    /// Reads an encoded index.
    ///
    /// # Errors
    ///
    /// Returns an error for anything [`encode`](Self::encode) would not have written: input that
    /// is not UTF-8, a missing or non-canonical header, another version, a line without a
    /// terminating newline or without the shape `<mode> <id> <path>`, an unknown mode, an object
    /// ID that is not in canonical form, paths that are not in strictly ascending order (which
    /// also rules out duplicates), and entries that [`insert`](Self::insert) rejects. Errors
    /// split into the shape of a line ([`IndexParseError::Malformed`] and the variants for the
    /// mode, the ID, and the order) and the rules for entries ([`IndexParseError::Entry`]). A
    /// line with several problems reports the first in this order: shape, mode, ID, the
    /// entry rules, then the order.
    pub fn parse(bytes: &[u8]) -> Result<Self, IndexParseError> {
        let text = std::str::from_utf8(bytes).map_err(|error| IndexParseError::NotUtf8 {
            valid_up_to: error.valid_up_to(),
        })?;
        let Some(body) = text.strip_prefix(HEADER) else {
            return Err(match other_version(text) {
                Some(version) => IndexParseError::UnsupportedVersion(version.to_owned()),
                None => IndexParseError::MissingHeader,
            });
        };
        if !body.is_empty() && !body.ends_with('\n') {
            // The header is line 1, so the last line of the body is line `count + 1`.
            return Err(IndexParseError::Malformed {
                line: body.lines().count() + 1,
                reason: IndexMalformed::CutOff,
            });
        }

        let mut index = Self::new();
        let mut previous: Option<&str> = None;
        for (offset, line) in body.split_terminator('\n').enumerate() {
            let line_number = offset + 2;
            let malformed = |reason| IndexParseError::Malformed {
                line: line_number,
                reason,
            };
            let (code, rest) = line
                .split_once(' ')
                .ok_or_else(|| malformed(IndexMalformed::EntryShape))?;
            let (id, path) = rest
                .split_once(' ')
                .ok_or_else(|| malformed(IndexMalformed::EntryShape))?;
            let mode = EntryMode::from_code(code)
                .and_then(FileMode::from_entry_mode)
                .ok_or_else(|| IndexParseError::UnknownMode {
                    line: line_number,
                    code: code.to_owned(),
                })?;
            // Every entry of the index is a file, so the ID is claimed as a blob. No store has
            // confirmed it; `Index::verify` does.
            let id = ObjectId::from_str(id).map(BlobId::new).map_err(|source| {
                IndexParseError::InvalidObjectId {
                    line: line_number,
                    source,
                }
            })?;
            index
                .insert(path, mode, id)
                .map_err(|source| IndexParseError::Entry {
                    line: line_number,
                    source,
                })?;
            if let Some(previous) =
                previous.filter(|previous| compare_paths(previous, path).is_ge())
            {
                return Err(IndexParseError::OutOfOrder {
                    line: line_number,
                    previous: previous.to_owned(),
                    path: path.to_owned(),
                });
            }
            previous = Some(path);
        }
        Ok(index)
    }

    /// Checks that every entry names a blob that the store has. The index cannot guarantee this
    /// itself: [`Index::parse`] only claims each ID as a blob, and [`Index::insert`] trusts the
    /// [`BlobId`] it is given, which may be the ID of a blob that was never stored.
    ///
    /// The result holds at the time of the call. The check reads and verifies each blob completely
    /// (see [`ObjectStore::resolve_blob`]), so its cost grows with the size of the staged files.
    ///
    /// # Errors
    ///
    /// Returns the first problem in index order: [`IndexVerifyError::Missing`] if the store has no
    /// such object, and [`IndexVerifyError::Store`] if the object is not a blob, is damaged, or the
    /// storage fails.
    pub fn verify(&self, store: &dyn ObjectStore) -> Result<(), IndexVerifyError> {
        for (path, entry) in &self.entries {
            match store.resolve_blob(&ObjectId::from(entry.id)) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(IndexVerifyError::Missing {
                        path: path.0.clone(),
                        id: entry.id,
                    })
                }
                Err(source) => {
                    return Err(IndexVerifyError::Store {
                        path: path.0.clone(),
                        source: Box::new(source),
                    })
                }
            }
        }
        Ok(())
    }

    /// Writes the trees of the index to the store and returns the ID of the root tree. The empty
    /// index gives the empty tree.
    ///
    /// The index is verified first ([`Index::verify`]), so a tree never refers to a blob that is
    /// missing, damaged, or of another kind at that time. No tree is written if the check fails.
    /// The check costs as much as reading every staged file; see there. The blobs are not read
    /// again, so a change of the store after the check goes unnoticed: the tree then refers to a
    /// blob that is missing or damaged.
    ///
    /// The trees are written from the innermost directory outwards, and the order of the entries
    /// is that of the depth-first walk, so each directory is complete when it is written. The
    /// work is a loop and not a recursion.
    ///
    /// # Errors
    ///
    /// Returns [`WriteTreeError::Verify`] if the check fails, [`WriteTreeError::Store`] if the
    /// store cannot write a tree, and [`WriteTreeError::Tree`] if a tree cannot be built, which
    /// the rules of the index rule out.
    pub fn write_tree(&self, store: &dyn ObjectStore) -> Result<TreeId, WriteTreeError> {
        self.verify(store).map_err(WriteTreeError::Verify)?;
        // The root and the directories that are open below it, from the outermost to the current
        // one. The root is kept apart, so that an open directory always has a parent.
        let mut root: Vec<TreeEntry> = Vec::new();
        let mut open: Vec<OpenDirectory> = Vec::new();
        for (path, entry) in self.iter() {
            let (directories, name) = path.rsplit_once('/').unwrap_or(("", path));
            let wanted: Vec<&str> = if directories.is_empty() {
                Vec::new()
            } else {
                directories.split('/').collect()
            };
            let shared = open
                .iter()
                .zip(&wanted)
                .take_while(|(directory, wanted_name)| directory.name == **wanted_name)
                .count();
            while let Some(directory) = pop_beyond(&mut open, shared) {
                close_directory(store, directory, entries_of(&mut root, &mut open))?;
            }
            for directory in &wanted[shared..] {
                open.push(OpenDirectory {
                    name: directory,
                    entries: Vec::new(),
                });
            }
            let file = TreeEntry::file(name, entry.mode, entry.id).map_err(WriteTreeError::Tree)?;
            entries_of(&mut root, &mut open).push(file);
        }
        while let Some(directory) = pop_beyond(&mut open, 0) {
            close_directory(store, directory, entries_of(&mut root, &mut open))?;
        }
        write_directory(store, root)
    }
}

/// A directory below the root whose entries are still being collected.
struct OpenDirectory<'a> {
    name: &'a str,
    entries: Vec<TreeEntry>,
}

/// Removes and returns the innermost open directory while more than `keep` are open.
fn pop_beyond<'a>(open: &mut Vec<OpenDirectory<'a>>, keep: usize) -> Option<OpenDirectory<'a>> {
    if open.len() > keep {
        open.pop()
    } else {
        None
    }
}

/// Returns the entries of the innermost open directory, or those of the root if none is open.
fn entries_of<'e>(
    root: &'e mut Vec<TreeEntry>,
    open: &'e mut [OpenDirectory<'_>],
) -> &'e mut Vec<TreeEntry> {
    match open.last_mut() {
        Some(directory) => &mut directory.entries,
        None => root,
    }
}

/// Writes the tree of the entries and returns its ID. The ID is computed from the tree and not
/// taken from the store, which has to return the same one.
fn write_directory(
    store: &dyn ObjectStore,
    entries: Vec<TreeEntry>,
) -> Result<TreeId, WriteTreeError> {
    let tree = Tree::new(entries).map_err(WriteTreeError::Tree)?;
    let id = tree.id();
    let stored = store
        .put_tree(&tree)
        .map_err(|source| WriteTreeError::Store(Box::new(source)))?;
    debug_assert_eq!(stored, id, "the store returned another ID for the tree");
    Ok(id)
}

/// Writes a closed directory and adds it as an entry to its parent.
fn close_directory(
    store: &dyn ObjectStore,
    directory: OpenDirectory<'_>,
    parent: &mut Vec<TreeEntry>,
) -> Result<(), WriteTreeError> {
    let id = write_directory(store, directory.entries)?;
    parent.push(TreeEntry::directory(directory.name, id).map_err(WriteTreeError::Tree)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Tree, TreeEntry};

    // Golden vectors: these IDs are the blob IDs of `hello` and of the empty content, written out
    // so that a change to the hash or to the ID format cannot move the test with it.
    const HELLO_ID: &str = "b3:9db8039d1b509628ad5b125ea89780b69fcde5c272215d690a2e4dc69eac75ad";
    const EMPTY_ID: &str = "b3:4a0b6603381c7c38c557e9ceb3cba8876f0a7863146e7309801352e80b073b65";

    fn id(text: &str) -> BlobId {
        BlobId::new(text.parse().expect("test ID must be valid"))
    }

    fn hello() -> BlobId {
        id(HELLO_ID)
    }

    fn index_of(paths: &[&str]) -> Index {
        let mut index = Index::new();
        for path in paths {
            index
                .insert(*path, FileMode::File, hello())
                .expect("valid entry");
        }
        index
    }

    fn input(entries: &str) -> Vec<u8> {
        format!("stemma-index 1\n{entries}").into_bytes()
    }

    fn entry_error(line: usize, path: &str, problem: PathProblem) -> IndexParseError {
        IndexParseError::Entry {
            line,
            source: IndexError::InvalidPath {
                path: path.to_owned(),
                problem,
            },
        }
    }

    fn shape(line: usize) -> IndexParseError {
        IndexParseError::Malformed {
            line,
            reason: IndexMalformed::EntryShape,
        }
    }

    fn name(component: &str, problem: NameProblem) -> PathProblem {
        PathProblem::Name {
            component: component.to_owned(),
            problem,
        }
    }

    // Insert, get, remove

    #[test]
    fn insert_and_get() {
        let mut index = Index::new();
        assert!(index.is_empty());
        index
            .insert("src/main.rs", FileMode::Executable, hello())
            .unwrap();
        let entry = index.get("src/main.rs").unwrap();
        assert_eq!(entry.mode(), FileMode::Executable);
        assert_eq!(entry.id(), hello());
        assert_eq!(index.len(), 1);
        assert!(index.get("src").is_none());
    }

    #[test]
    fn insert_replaces_the_entry_at_the_same_path_including_its_mode() {
        let mut index = index_of(&["a.txt"]);
        index
            .insert("a.txt", FileMode::Executable, id(EMPTY_ID))
            .unwrap();
        assert_eq!(index.len(), 1);
        let entry = index.get("a.txt").unwrap();
        assert_eq!(entry.mode(), FileMode::Executable);
        assert_eq!(entry.id(), id(EMPTY_ID));
    }

    #[test]
    fn remove_returns_the_entry_and_frees_the_path() {
        let mut index = index_of(&["a"]);
        assert!(index.remove("a").is_some());
        assert!(index.remove("a").is_none());
        assert!(index.is_empty());
        // The path `a` is free to become a directory now.
        index.insert("a/b", FileMode::File, hello()).unwrap();
    }

    #[test]
    fn removing_a_directory_name_removes_nothing() {
        let mut index = index_of(&["src/main.rs"]);
        assert!(index.remove("src").is_none());
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn file_mode_converts_to_the_tree_mode() {
        for (file_mode, entry_mode) in [
            (FileMode::File, EntryMode::File),
            (FileMode::Executable, EntryMode::Executable),
            (FileMode::Symlink, EntryMode::Symlink),
        ] {
            assert_eq!(EntryMode::from(file_mode), entry_mode);
            assert_eq!(FileMode::from_entry_mode(entry_mode), Some(file_mode));
        }
        assert_eq!(FileMode::from_entry_mode(EntryMode::Directory), None);
    }

    // Path rules

    #[test]
    fn invalid_paths_are_rejected_and_leave_the_index_unchanged() {
        let cases = [
            ("", PathProblem::EmptyComponent),
            ("/a", PathProblem::EmptyComponent),
            ("a/", PathProblem::EmptyComponent),
            ("a//b", PathProblem::EmptyComponent),
            ("a/../b", name("..", NameProblem::DotName)),
            ("./a", name(".", NameProblem::DotName)),
            (".//", name(".", NameProblem::DotName)),
            ("a/.../b", name("...", NameProblem::DotName)),
            ("a/.stemma/x", name(".stemma", NameProblem::ReservedName)),
            (".stemma", name(".stemma", NameProblem::ReservedName)),
            (
                "src/.stemma::$INDEX_ALLOCATION",
                name(".stemma::$INDEX_ALLOCATION", NameProblem::ReservedName),
            ),
            (
                "a/x\\..\\.stemma\\config",
                name("x\\..\\.stemma\\config", NameProblem::ReservedName),
            ),
            ("a/STEMMA~1", name("STEMMA~1", NameProblem::ReservedName)),
            (
                "a/\u{200b}/b",
                name("\u{200b}", NameProblem::InvisibleCharacter),
            ),
            ("a\nb", name("a\nb", NameProblem::ControlCharacter)),
            ("a/b\0", name("b\0", NameProblem::ControlCharacter)),
        ];
        let mut index = index_of(&["keep.txt"]);
        let before = index.clone();
        for (path, problem) in cases {
            let error = index.insert(path, FileMode::File, hello()).expect_err(path);
            assert_eq!(
                error,
                IndexError::InvalidPath {
                    path: path.to_owned(),
                    problem
                },
                "{path:?}"
            );
            assert_eq!(index, before, "{path:?}");
        }
    }

    #[test]
    fn windows_separators_are_the_checkouts_business_except_in_the_reserved_name() {
        // Valid on Linux and macOS and accepted here, as in trees (ADR-005).
        let index = index_of(&[
            "back\\slash",
            "x\\.stemma-notes\\config",
            "report:final.txt",
            "C:",
            "Foo",
            "foo/bar",
            "\u{e9}",
            "e\u{301}/x",
            "README",
            "readme",
        ]);
        assert_eq!(index.len(), 10);
    }

    #[test]
    fn path_errors_name_the_component_and_escape_the_path() {
        let error = Index::new()
            .insert("a/b\nc/d", FileMode::File, hello())
            .unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("a/b\\nc/d") && text.contains("b\\nc"),
            "{text}"
        );
        assert!(!text.contains('\n'), "{text}");
    }

    // File and directory conflicts

    #[test]
    fn a_file_cannot_have_children_and_a_directory_cannot_become_a_file() {
        let mut index = index_of(&["a", "x/y"]);
        let before = index.clone();
        let cases = [
            (
                "a/b",
                IndexError::ParentIsFile {
                    path: "a/b".into(),
                    parent: "a".into(),
                },
            ),
            (
                "a/b/c",
                IndexError::ParentIsFile {
                    path: "a/b/c".into(),
                    parent: "a".into(),
                },
            ),
            (
                "x",
                IndexError::HasChildren {
                    path: "x".into(),
                    child: "x/y".into(),
                },
            ),
            (
                "x/y/z",
                IndexError::ParentIsFile {
                    path: "x/y/z".into(),
                    parent: "x/y".into(),
                },
            ),
        ];
        for (path, expected) in cases {
            let error = index.insert(path, FileMode::File, hello()).unwrap_err();
            assert_eq!(error, expected, "{path}");
            assert_eq!(index, before, "{path}: the index must be unchanged");
        }
    }

    #[test]
    fn conflicts_are_found_across_several_levels() {
        let mut index = index_of(&["a/b/c"]);
        for (path, child) in [("a", "a/b/c"), ("a/b", "a/b/c")] {
            let error = index.insert(path, FileMode::File, hello()).unwrap_err();
            assert_eq!(
                error,
                IndexError::HasChildren {
                    path: path.into(),
                    child: child.into()
                }
            );
        }
        // A sibling and a deeper sibling are fine.
        index.insert("a/b/d", FileMode::File, hello()).unwrap();
        index.insert("a/e/f", FileMode::File, hello()).unwrap();
    }

    #[test]
    fn a_symbolic_link_is_a_file_for_conflicts() {
        let mut index = Index::new();
        index.insert("link", FileMode::Symlink, hello()).unwrap();
        assert!(matches!(
            index.insert("link/x", FileMode::File, hello()),
            Err(IndexError::ParentIsFile { .. })
        ));
        index.insert("d/l", FileMode::Symlink, hello()).unwrap();
        assert!(matches!(
            index.insert("d", FileMode::File, hello()),
            Err(IndexError::HasChildren { .. })
        ));
    }

    #[test]
    fn similar_prefixes_do_not_conflict_in_either_order() {
        // `a-b` and `a.b` sort next to `a/`, but neither is inside the directory `a`.
        let paths = ["a-b", "a.b", "ab", "a/c"];
        let index = index_of(&paths);
        assert_eq!(index.len(), 4);
        let reversed: Vec<&str> = paths.iter().rev().copied().collect();
        assert_eq!(index_of(&reversed), index);
        // With no file below `a`, the file `a` fits between its look-alike neighbours.
        let neighbours = index_of(&["a-b", "a.b", "ab"]);
        let mut with_a = neighbours.clone();
        with_a.insert("a", FileMode::File, hello()).unwrap();
        assert_eq!(with_a.len(), 4);
    }

    // Encoding

    #[test]
    fn the_order_is_by_component_and_shows_in_the_encoding() {
        let index = index_of(&["b", "a/z", "a-b", "a.b"]);
        let paths: Vec<&str> = index.iter().map(|(path, _)| path).collect();
        assert_eq!(paths, ["a/z", "a-b", "a.b", "b"]);
        let encoded = String::from_utf8(index.encode()).unwrap();
        let encoded_paths: Vec<&str> = encoded
            .lines()
            .skip(1)
            .map(|line| line.splitn(3, ' ').nth(2).unwrap())
            .collect();
        assert_eq!(encoded_paths, ["a/z", "a-b", "a.b", "b"]);
    }

    #[test]
    fn insertion_order_does_not_change_the_encoding() {
        let forward = index_of(&["a", "b/c", "d"]);
        let backward = index_of(&["d", "b/c", "a"]);
        assert_eq!(forward.encode(), backward.encode());
    }

    #[test]
    fn the_encoding_matches_a_literal_golden_vector_in_both_directions() {
        let mut index = Index::new();
        index.insert("b c", FileMode::File, id(EMPTY_ID)).unwrap();
        index.insert("a/z", FileMode::Symlink, hello()).unwrap();
        index
            .insert("a.b", FileMode::Executable, id(EMPTY_ID))
            .unwrap();
        index.insert("a-b", FileMode::File, hello()).unwrap();
        let golden = format!(
            "stemma-index 1\n\
             120000 {HELLO_ID} a/z\n\
             100644 {HELLO_ID} a-b\n\
             100755 {EMPTY_ID} a.b\n\
             100644 {EMPTY_ID} b c\n"
        );
        assert_eq!(index.encode(), golden.as_bytes());
        let parsed = Index::parse(golden.as_bytes()).unwrap();
        assert_eq!(parsed, index);
        assert_eq!(parsed.encode(), golden.as_bytes());
    }

    #[test]
    fn the_empty_index_is_the_header_only() {
        assert_eq!(Index::new().encode(), b"stemma-index 1\n");
        assert_eq!(Index::parse(b"stemma-index 1\n").unwrap(), Index::new());
    }

    #[test]
    fn encode_then_parse_roundtrips_awkward_paths_and_every_mode() {
        let mut index = Index::new();
        let entries = [
            ("a/b/c.txt", FileMode::File),
            ("a/d", FileMode::Executable),
            ("e f/g h", FileMode::File),
            (" leading", FileMode::File),
            ("trailing ", FileMode::File),
            ("two  spaces", FileMode::File),
            ("\u{ff5e}", FileMode::File),
            ("\u{1f600}", FileMode::File),
            ("\u{e9}", FileMode::File),
            ("e\u{301}", FileMode::File),
            ("README", FileMode::File),
            ("readme", FileMode::File),
            ("back\\slash", FileMode::File),
            ("c:d", FileMode::File),
            ("link", FileMode::Symlink),
        ];
        for (path, mode) in entries {
            index.insert(path, mode, hello()).unwrap();
        }
        let bytes = index.encode();
        let parsed = Index::parse(&bytes).unwrap();
        assert_eq!(parsed, index);
        assert_eq!(parsed.encode(), bytes, "one index, one encoding");
    }

    // Strict reader

    #[test]
    fn parse_reports_the_exact_error_for_every_non_canonical_input() {
        let good = format!("100644 {HELLO_ID} a\n");
        let digest = HELLO_ID.strip_prefix("b3:").unwrap();
        let cases: Vec<(Vec<u8>, IndexParseError)> = vec![
            // Line shape and the final newline.
            (
                input(&format!("100644 {HELLO_ID} a")),
                IndexParseError::Malformed {
                    line: 2,
                    reason: IndexMalformed::CutOff,
                },
            ),
            (
                input(&format!("{good}100644 {HELLO_ID} b")),
                IndexParseError::Malformed {
                    line: 3,
                    reason: IndexMalformed::CutOff,
                },
            ),
            (input("100644\n"), shape(2)),
            (input(&format!("100644 {HELLO_ID}\n")), shape(2)),
            (input("\n"), shape(2)),
            (input(&format!("{good}\n")), shape(3)),
            (input("stemma-index 1\n"), shape(2)),
            // The separator is exactly one space.
            (input(&format!("100644\t{HELLO_ID} a\n")), shape(2)),
            (
                input(&format!("100644  {HELLO_ID} a\n")),
                IndexParseError::InvalidObjectId {
                    line: 2,
                    source: ObjectIdParseError::MissingPrefix,
                },
            ),
            // Mode.
            (
                input(&format!("100666 {HELLO_ID} a\n")),
                IndexParseError::UnknownMode {
                    line: 2,
                    code: "100666".into(),
                },
            ),
            (
                input(&format!("0100644 {HELLO_ID} a\n")),
                IndexParseError::UnknownMode {
                    line: 2,
                    code: "0100644".into(),
                },
            ),
            (
                input(&format!("040000 {HELLO_ID} d\n")),
                IndexParseError::UnknownMode {
                    line: 2,
                    code: "040000".into(),
                },
            ),
            // Object ID.
            (
                input("100644 sha1:abc a\n"),
                IndexParseError::InvalidObjectId {
                    line: 2,
                    source: ObjectIdParseError::MissingPrefix,
                },
            ),
            (
                input(&format!("100644 b3:{} a\n", digest.to_uppercase())),
                IndexParseError::InvalidObjectId {
                    line: 2,
                    source: ObjectIdParseError::NotHex(1),
                },
            ),
            (
                input("100644 b3:abcd a\n"),
                IndexParseError::InvalidObjectId {
                    line: 2,
                    source: ObjectIdParseError::WrongLength(4),
                },
            ),
            // Order.
            (
                input(&format!("{good}{good}")),
                IndexParseError::OutOfOrder {
                    line: 3,
                    previous: "a".into(),
                    path: "a".into(),
                },
            ),
            (
                input(&format!("100644 {HELLO_ID} b\n100644 {HELLO_ID} a\n")),
                IndexParseError::OutOfOrder {
                    line: 3,
                    previous: "b".into(),
                    path: "a".into(),
                },
            ),
            // The component `a` sorts before `a-b`, so the whole-path order is not accepted.
            (
                input(&format!("100644 {HELLO_ID} a-b\n100644 {HELLO_ID} a/z\n")),
                IndexParseError::OutOfOrder {
                    line: 3,
                    previous: "a-b".into(),
                    path: "a/z".into(),
                },
            ),
            // Line endings and control characters in the path reach the path rules.
            (
                input(&format!("100644 {HELLO_ID} a\r\n")),
                entry_error(2, "a\r", name("a\r", NameProblem::ControlCharacter)),
            ),
            (
                input(&format!("100644 {HELLO_ID} a\0b\n")),
                entry_error(2, "a\0b", name("a\0b", NameProblem::ControlCharacter)),
            ),
            (
                input(&format!("100644 {HELLO_ID} \n")),
                entry_error(2, "", PathProblem::EmptyComponent),
            ),
            (
                input(&format!("100644 {HELLO_ID} /a\n")),
                entry_error(2, "/a", PathProblem::EmptyComponent),
            ),
            (
                input(&format!("100644 {HELLO_ID} a/\n")),
                entry_error(2, "a/", PathProblem::EmptyComponent),
            ),
        ];
        for (bytes, expected) in cases {
            let shown = String::from_utf8_lossy(&bytes).into_owned();
            assert_eq!(Index::parse(&bytes), Err(expected), "{shown:?}");
        }
    }

    #[test]
    fn a_path_with_leading_or_doubled_spaces_is_a_valid_path() {
        // After the ID, a single space separates the path; further spaces belong to it.
        let parsed = Index::parse(&input(&format!("100644 {HELLO_ID}  a\n"))).unwrap();
        assert!(parsed.get(" a").is_some());
    }

    #[test]
    fn parse_rejects_bad_headers_with_the_right_error() {
        let missing = [
            &b""[..],
            b"garbage",
            b"stemma-index 1",
            b"stemma-index 1\r\n",
            b" stemma-index 1\n",
            b"\xef\xbb\xbfstemma-index 1\n",
            b"Stemma-Index 1\n",
            b"stemma-index\n",
            b"stemma-index x\n",
            b"stemma-index \n",
            b"stemma-index 1.0\n",
        ];
        for bytes in missing {
            assert_eq!(
                Index::parse(bytes),
                Err(IndexParseError::MissingHeader),
                "{bytes:?}"
            );
        }
        for (bytes, version) in [
            (&b"stemma-index 2\n"[..], "2"),
            (b"stemma-index 10\n", "10"),
            (b"stemma-index 01\n", "01"),
            (b"stemma-index 0\n", "0"),
            (b"stemma-index 2", "2"),
            (b"stemma-index 2\nanything", "2"),
        ] {
            assert_eq!(
                Index::parse(bytes),
                Err(IndexParseError::UnsupportedVersion(version.into())),
                "{bytes:?}"
            );
        }
    }

    #[test]
    fn parse_reports_where_the_utf8_ends() {
        assert_eq!(
            Index::parse(b"stemma-index 1\n\xff\n"),
            Err(IndexParseError::NotUtf8 { valid_up_to: 15 })
        );
        assert_eq!(
            Index::parse(b"\xffstemma-index 1\n"),
            Err(IndexParseError::NotUtf8 { valid_up_to: 0 })
        );
    }

    #[test]
    fn a_header_that_is_repeated_is_not_an_entry() {
        assert_eq!(
            Index::parse(b"stemma-index 1\nstemma-index 1\n"),
            Err(shape(2))
        );
    }

    #[test]
    fn parse_applies_the_insert_rules_and_reports_the_line() {
        let invalid_path = input(&format!("100644 {HELLO_ID} a/../b\n"));
        assert_eq!(
            Index::parse(&invalid_path),
            Err(entry_error(2, "a/../b", name("..", NameProblem::DotName)))
        );
        let parent_is_file = input(&format!("100644 {HELLO_ID} a\n100644 {HELLO_ID} a/b\n"));
        assert_eq!(
            Index::parse(&parent_is_file),
            Err(IndexParseError::Entry {
                line: 3,
                source: IndexError::ParentIsFile {
                    path: "a/b".into(),
                    parent: "a".into()
                }
            })
        );
    }

    #[test]
    fn an_entry_error_is_reported_before_an_order_error_on_the_same_line() {
        // `a` after `b` is out of order, and `a/../b` is invalid; the path rule wins.
        let both = input(&format!("100644 {HELLO_ID} b\n100644 {HELLO_ID} a/../b\n"));
        assert!(matches!(
            Index::parse(&both),
            Err(IndexParseError::Entry { line: 3, .. })
        ));
    }

    #[test]
    fn parse_errors_print_their_cause_once_and_keep_it_in_the_variant() {
        let error = Index::parse(&input("100644 b3:abcd a\n")).unwrap_err();
        let IndexParseError::InvalidObjectId { source, .. } = &error else {
            panic!("unexpected error: {error}");
        };
        assert!(source.to_string().contains("found 4"), "{source}");
        assert!(error.to_string().starts_with("line 2: "), "{error}");
        assert!(error.to_string().ends_with(&source.to_string()), "{error}");
        // The message has the cause, so the chain goes on with the cause of the cause.
        assert!(error.source().is_none());
        let error = Index::parse(&input(&format!("100644 {HELLO_ID} /a\n"))).unwrap_err();
        assert!(matches!(error, IndexParseError::Entry { .. }));
        assert!(error.source().is_none());
        assert!(Index::parse(b"x").unwrap_err().source().is_none());
    }

    #[test]
    fn error_messages_escape_hostile_values() {
        let error = Index::parse(&input(&format!("100666\u{7} {HELLO_ID} a\n"))).unwrap_err();
        // The mode is everything before the first space.
        assert!(error.to_string().contains("\\u{7}"), "{error}");
        let error = Index::parse(&input(&format!(
            "100644 {HELLO_ID} b\n100644 {HELLO_ID} a\n"
        )))
        .unwrap_err();
        assert!(error.to_string().contains("strictly ascending"), "{error}");
    }

    // Verification against a store

    #[test]
    fn verify_accepts_an_empty_index_and_one_of_stored_blobs() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        assert!(Index::new().verify(&store).is_ok());

        let mut index = Index::new();
        for (path, content) in [("a.txt", "one"), ("src/b.rs", "two"), ("src/c.rs", "")] {
            let blob = store.put_blob(&crate::object::Blob::new(content)).unwrap();
            index.insert(path, FileMode::File, blob).unwrap();
        }
        assert!(index.verify(&store).is_ok());
    }

    #[test]
    fn verify_reports_a_blob_that_was_never_stored_with_its_path() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        // The ID is right for the content, but the blob is not in the store.
        let blob = crate::object::Blob::new("not stored").id();
        let mut index = Index::new();
        index.insert("docs/new.md", FileMode::File, blob).unwrap();
        match index.verify(&store).unwrap_err() {
            IndexVerifyError::Missing { path, id } => {
                assert_eq!((path.as_str(), id), ("docs/new.md", blob));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn verify_reports_an_id_that_the_reader_accepted_as_a_blob_but_names_a_tree() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let tree = store
            .put_tree(&crate::object::Tree::new(vec![]).unwrap())
            .unwrap();
        // The reader claims every ID as a blob and cannot tell; the store can.
        let text = format!("stemma-index 1\n100644 {tree} a.txt\n");
        let index = Index::parse(text.as_bytes()).expect("the reader accepts the claim");
        let error = index.verify(&store).unwrap_err();
        match &error {
            IndexVerifyError::Store { path, source } => {
                assert_eq!(path, "a.txt");
                assert!(
                    matches!(
                        source.as_ref(),
                        StoreError::KindMismatch {
                            expected: crate::object::Kind::Blob,
                            found: crate::object::Kind::Tree,
                            ..
                        }
                    ),
                    "{source}"
                );
            }
            other => panic!("unexpected error: {other}"),
        }
        assert!(error.source().is_none());
        assert!(
            error.to_string().contains("is a tree, not a blob"),
            "{error}"
        );
    }

    #[test]
    fn verify_reports_the_first_problem_in_index_order() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let stored = store.put_blob(&crate::object::Blob::new("ok")).unwrap();
        let absent = crate::object::Blob::new("absent").id();
        let mut index = Index::new();
        index.insert("c", FileMode::File, absent).unwrap();
        index.insert("b", FileMode::File, absent).unwrap();
        index.insert("a", FileMode::File, stored).unwrap();
        match index.verify(&store).unwrap_err() {
            IndexVerifyError::Missing { path, id } => {
                assert_eq!((path.as_str(), id), ("b", absent))
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn verify_follows_the_component_order_and_not_the_order_of_the_path_strings() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let in_dir = crate::object::Blob::new("in the directory").id();
        let dashed = crate::object::Blob::new("with a dash").id();
        let mut index = Index::new();
        // As strings `a-b` sorts before `a/z`; as components `a/z` comes first.
        index.insert("a-b", FileMode::File, dashed).unwrap();
        index.insert("a/z", FileMode::File, in_dir).unwrap();
        match index.verify(&store).unwrap_err() {
            IndexVerifyError::Missing { path, id } => {
                assert_eq!((path.as_str(), id), ("a/z", in_dir));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn verify_reports_a_damaged_blob_as_a_store_error_and_not_as_missing() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let blob = store.put_blob(&crate::object::Blob::new("hello")).unwrap();
        let file = store.object_path(&ObjectId::from(blob));
        crate::testutil::overwrite(&file, b"blob 5\0other");
        let mut index = Index::new();
        index.insert("src/lib.rs", FileMode::File, blob).unwrap();
        match index.verify(&store).unwrap_err() {
            IndexVerifyError::Store { path, source } => {
                assert_eq!(path, "src/lib.rs");
                assert!(matches!(*source, StoreError::Corrupt { .. }), "{source}");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn verify_reports_a_wrong_kind_before_a_later_missing_blob_and_the_reverse() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let tree = store
            .put_tree(&crate::object::Tree::new(vec![]).unwrap())
            .unwrap();
        let absent = crate::object::Blob::new("absent").id();
        let parse = |first: &dyn std::fmt::Display, second: &dyn std::fmt::Display| {
            let text = format!("stemma-index 1\n100644 {first} a\n100644 {second} b\n");
            Index::parse(text.as_bytes()).expect("the reader accepts the claims")
        };
        match parse(&tree, &absent).verify(&store).unwrap_err() {
            IndexVerifyError::Store { path, .. } => assert_eq!(path, "a"),
            other => panic!("unexpected error: {other}"),
        }
        match parse(&absent, &tree).verify(&store).unwrap_err() {
            IndexVerifyError::Missing { path, .. } => assert_eq!(path, "a"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn verify_prints_the_escaped_path_before_the_message_of_the_store() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let tree = store
            .put_tree(&crate::object::Tree::new(vec![]).unwrap())
            .unwrap();
        let text = format!("stemma-index 1\n100644 {tree} say\"hi\n");
        let index = Index::parse(text.as_bytes()).expect("the reader accepts the claim");
        let error = index.verify(&store).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("`say\\\"hi`: object {tree} is a tree, not a blob")
        );
    }

    #[test]
    fn verify_errors_name_the_path_escaped_and_the_cause() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let absent = crate::object::Blob::new("absent").id();
        let mut index = Index::new();
        index.insert("quote\"d", FileMode::File, absent).unwrap();
        let error = index.verify(&store).unwrap_err();
        let text = error.to_string();
        assert!(
            text.starts_with("`quote\\\"d`: the store has no object b3:"),
            "{text}"
        );
        assert!(error.source().is_none());
    }

    // ---- write_tree

    /// Stores the content of each file and stages the paths, with `hello` content for all.
    fn staged(store: &dyn ObjectStore, paths: &[&str]) -> Index {
        let blob = store.put_blob(&crate::object::Blob::new("hello")).unwrap();
        let mut index = Index::new();
        for path in paths {
            index.insert(*path, FileMode::File, blob).unwrap();
        }
        index
    }

    fn file(name: &str) -> TreeEntry {
        TreeEntry::file(name, FileMode::File, hello()).unwrap()
    }

    fn directory(name: &str, entries: Vec<TreeEntry>) -> (TreeEntry, TreeId) {
        let id = Tree::new(entries).unwrap().id();
        (TreeEntry::directory(name, id).unwrap(), id)
    }

    #[test]
    fn write_tree_of_the_empty_index_is_the_empty_tree() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let expected = Tree::new(vec![]).unwrap().id();
        assert_eq!(Index::new().write_tree(&store).unwrap(), expected);
        assert!(store.contains(&ObjectId::from(expected)).unwrap());
    }

    #[test]
    fn write_tree_builds_the_nested_trees_that_a_hand_built_walk_gives() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let index = staged(
            &store,
            &[
                "README", "a-b", "a.b", "a/b/c", "a/d", "e/f/g", "e/h", "z/y/x/w",
            ],
        );
        let (b, b_id) = directory("b", vec![file("c")]);
        let (a, a_id) = directory("a", vec![b, file("d")]);
        let (f, f_id) = directory("f", vec![file("g")]);
        let (e, e_id) = directory("e", vec![f, file("h")]);
        let (x, x_id) = directory("x", vec![file("w")]);
        let (y, y_id) = directory("y", vec![x]);
        let (z, z_id) = directory("z", vec![y]);
        let root = Tree::new(vec![file("README"), file("a-b"), file("a.b"), a, e, z]).unwrap();
        let root_id = index.write_tree(&store).unwrap();
        assert_eq!(root_id, root.id());
        for id in [b_id, a_id, f_id, e_id, x_id, y_id, z_id, root_id] {
            assert!(store.contains(&ObjectId::from(id)).unwrap(), "{id}");
        }
    }

    #[test]
    fn write_tree_keeps_the_modes_and_ignores_the_order_of_insertion() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let blob = store.put_blob(&crate::object::Blob::new("hello")).unwrap();
        let mut forward = Index::new();
        let mut backward = Index::new();
        let entries = [
            ("bin/run", FileMode::Executable),
            ("bin/link", FileMode::Symlink),
            ("doc", FileMode::File),
        ];
        for (path, mode) in entries {
            forward.insert(path, mode, blob).unwrap();
        }
        for (path, mode) in entries.into_iter().rev() {
            backward.insert(path, mode, blob).unwrap();
        }
        let (bin, _) = directory(
            "bin",
            vec![
                TreeEntry::file("link", FileMode::Symlink, hello()).unwrap(),
                TreeEntry::file("run", FileMode::Executable, hello()).unwrap(),
            ],
        );
        let expected = Tree::new(vec![bin, file("doc")]).unwrap().id();
        assert_eq!(forward.write_tree(&store).unwrap(), expected);
        assert_eq!(backward.write_tree(&store).unwrap(), expected);
    }

    #[test]
    fn write_tree_of_a_single_file_at_the_root() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let index = staged(&store, &["a"]);
        let expected = Tree::new(vec![file("a")]).unwrap().id();
        assert_eq!(index.write_tree(&store).unwrap(), expected);
    }

    #[test]
    fn write_tree_closes_several_levels_at_once_and_keeps_shared_prefixes() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        // Two levels shared (`a/b`), three levels closed at once (`a/b/c/x` to `f`), and the same
        // names at other depths (`b` below `a`, `g`, and `x`; `x` below `a` and at the root).
        let index = staged(
            &store,
            &[
                "a/b/c/x", "a/b/d", "a/e", "a/x/b/d", "f", "g/b/h", "g/b/i/j", "x/b/d",
            ],
        );
        let (c, _) = directory("c", vec![file("x")]);
        let (b, _) = directory("b", vec![c, file("d")]);
        let (ax_b, _) = directory("b", vec![file("d")]);
        let (ax, _) = directory("x", vec![ax_b]);
        let (a, _) = directory("a", vec![b, file("e"), ax]);
        let (i, _) = directory("i", vec![file("j")]);
        let (gb, _) = directory("b", vec![file("h"), i]);
        let (g, _) = directory("g", vec![gb]);
        let (xb, _) = directory("b", vec![file("d")]);
        let (x, _) = directory("x", vec![xb]);
        let expected = Tree::new(vec![a, file("f"), g, x]).unwrap().id();
        assert_eq!(index.write_tree(&store).unwrap(), expected);
    }

    /// Makes the store unable to write the tree: the directory for its file is a file.
    fn block(store: &crate::store::FsObjectStore, tree: TreeId) {
        let path = store.object_path(&ObjectId::from(tree));
        let fan_out = path.parent().expect("an object file has a directory");
        std::fs::create_dir_all(fan_out.parent().expect("an algorithm directory")).unwrap();
        std::fs::write(fan_out, b"in the way").unwrap();
    }

    #[test]
    fn write_tree_reports_a_store_that_fails_for_the_root_and_keeps_the_inner_trees() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let index = staged(&store, &["a/b", "c"]);
        let (a, a_id) = directory("a", vec![file("b")]);
        let root = Tree::new(vec![a, file("c")]).unwrap().id();
        block(&store, root);
        let error = index.write_tree(&store).unwrap_err();
        assert!(matches!(error, WriteTreeError::Store(_)), "{error}");
        assert!(store.contains(&ObjectId::from(a_id)).unwrap());
    }

    #[test]
    fn write_tree_reports_a_store_that_fails_for_an_inner_tree_and_writes_no_root() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let index = staged(&store, &["a/b", "c"]);
        let (a, a_id) = directory("a", vec![file("b")]);
        let root = Tree::new(vec![a, file("c")]).unwrap().id();
        block(&store, a_id);
        let error = index.write_tree(&store).unwrap_err();
        assert!(matches!(error, WriteTreeError::Store(_)), "{error}");
        assert!(!store.contains(&ObjectId::from(root)).unwrap());
    }

    #[test]
    fn write_tree_twice_gives_the_same_tree() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let index = staged(&store, &["a/b", "c"]);
        assert_eq!(
            index.write_tree(&store).unwrap(),
            index.write_tree(&store).unwrap()
        );
    }

    #[test]
    fn write_tree_with_a_missing_blob_fails_and_writes_nothing() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let mut index = staged(&store, &["a/b"]);
        let absent = crate::object::Blob::new("absent").id();
        index.insert("c/d", FileMode::File, absent).unwrap();
        match index.write_tree(&store).unwrap_err() {
            WriteTreeError::Verify(IndexVerifyError::Missing { path, id }) => {
                assert_eq!(path, "c/d");
                assert_eq!(id, absent);
            }
            other => panic!("unexpected error: {other}"),
        }
        // Not even the tree of the valid directory `a` was written.
        let tree_of_a = Tree::new(vec![file("b")]).unwrap().id();
        assert!(!store.contains(&ObjectId::from(tree_of_a)).unwrap());
    }

    #[test]
    fn write_tree_rejects_an_id_that_names_a_tree_and_writes_no_tree() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let tree = store.put_tree(&Tree::new(vec![]).unwrap()).unwrap();
        let text = format!("stemma-index 1\n100644 {tree} a\n");
        let index = Index::parse(text.as_bytes()).expect("the reader accepts the claim");
        match index.write_tree(&store).unwrap_err() {
            WriteTreeError::Verify(IndexVerifyError::Store { path, source }) => {
                assert_eq!(path, "a");
                assert!(
                    matches!(*source, StoreError::KindMismatch { .. }),
                    "{source}"
                );
            }
            other => panic!("unexpected error: {other}"),
        }
        let claimed = BlobId::new(ObjectId::from(tree));
        let would_be = Tree::new(vec![TreeEntry::file("a", FileMode::File, claimed).unwrap()]);
        assert!(!store
            .contains(&ObjectId::from(would_be.unwrap().id()))
            .unwrap());
    }

    #[test]
    fn write_tree_reports_a_store_that_cannot_write() {
        let dir = crate::testutil::TempDir::new();
        let store = crate::store::FsObjectStore::new(dir.path().join("absent"));
        let error = Index::new().write_tree(&store).unwrap_err();
        assert!(matches!(error, WriteTreeError::Store(_)), "{error}");
        let WriteTreeError::Store(store_error) = &error else {
            unreachable!("matched above");
        };
        // The message has the message of the store error, and that has the one of the I/O error.
        assert!(
            error
                .to_string()
                .starts_with("cannot write a tree: could not open the object store directory"),
            "{error}"
        );
        assert!(
            error.to_string().ends_with(&match &**store_error {
                StoreError::Io { source, .. } => source.to_string(),
                other => panic!("unexpected store error: {other}"),
            }),
            "{error}"
        );
        assert!(error.source().is_none());
    }

    #[test]
    fn write_tree_error_messages_name_the_cause_and_the_source() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let mut index = Index::new();
        let absent = crate::object::Blob::new("absent").id();
        index.insert("q\"d", FileMode::File, absent).unwrap();
        let error = index.write_tree(&store).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("the index does not verify: `q\\\"d`: the store has no object b3:"),
            "{error}"
        );
        assert!(error.source().is_none());
        let tree = WriteTreeError::Tree(TreeError::DuplicateName("x".to_owned()));
        assert_eq!(
            tree.to_string(),
            "cannot build a tree: duplicate tree entry name `x`"
        );
        assert!(tree.source().is_none());
    }

    #[test]
    fn write_tree_gives_the_right_tree_for_a_deeply_nested_path() {
        let dir = crate::testutil::TempDir::new();
        let store = dir.store();
        let path = vec!["d"; 200].join("/");
        let index = staged(&store, &[&path]);
        let mut expected = Tree::new(vec![file("d")]).unwrap().id();
        for _ in 1..200 {
            expected = Tree::new(vec![TreeEntry::directory("d", expected).unwrap()])
                .unwrap()
                .id();
        }
        assert_eq!(index.write_tree(&store).unwrap(), expected);
    }

    #[test]
    fn the_shape_problems_of_an_index_have_their_own_variants_and_messages() {
        let cut = Index::parse(&input(&format!("100644 {HELLO_ID} a"))).unwrap_err();
        let not_an_entry = Index::parse(&input("100644\n")).unwrap_err();
        assert_eq!(
            cut,
            IndexParseError::Malformed {
                line: 2,
                reason: IndexMalformed::CutOff
            }
        );
        assert_eq!(not_an_entry, shape(2));
        assert_eq!(
            cut.to_string(),
            "line 2: the last line has no terminating newline"
        );
        assert_eq!(
            not_an_entry.to_string(),
            "line 2: expected `<mode> <id> <path>`"
        );
    }
}
