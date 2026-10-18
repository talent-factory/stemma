//! The index: the set of files the next commit will contain.
//!
//! The index maps a relative path to a file mode and the ID of the blob that holds the content.
//! It is a plain in-memory model with a canonical text encoding. This module touches neither the
//! file system nor an object store.
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
//! `100644`, `100755`, or `120000` and `<id>` is the textual object ID. Entries are sorted by
//! path in UTF-8 byte order, comparing the whole path string and not component by component:
//! `a-b`, `a.b`, and `a/z` appear in this order, while a tree orders the names of one directory
//! only. A tree builder therefore has to group the entries by directory. The path comes last
//! because it may contain spaces; it cannot contain a newline. The reader is strict: it rejects
//! anything that [`Index::encode`] would not have written, so one index has exactly one
//! encoding.
//!
//! An incompatible change to the encoding raises the number in the header line. A reader reports
//! a header with another number as [`IndexParseError::UnsupportedVersion`] instead of reading
//! it as version 1. The index is a file, not an object, so the kind-name rule for objects does
//! not apply to it.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::ops::Bound;
use std::str::FromStr;

use crate::object::{name_problem, EntryMode, NameProblem, ObjectId, ObjectIdParseError};

/// First line of the encoding. It carries the format version.
const HEADER: &str = "stemma-index 1\n";

/// Start of the first line of every version of the encoding.
const HEADER_PREFIX: &str = "stemma-index ";

/// Shape of an entry line, used in error messages.
const ENTRY_SHAPE: &str = "expected `<mode> <id> <path>`";

/// File mode of an index entry. The index holds no directories, so there is no directory mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// A regular file.
    File,
    /// A regular file with the executable bit set.
    Executable,
    /// A symbolic link. The blob holds the link target.
    Symlink,
}

impl FileMode {
    /// Returns the file mode for a tree entry mode, or `None` for a directory.
    pub fn from_entry_mode(mode: EntryMode) -> Option<Self> {
        match mode {
            EntryMode::File => Some(Self::File),
            EntryMode::Executable => Some(Self::Executable),
            EntryMode::Symlink => Some(Self::Symlink),
            EntryMode::Directory => None,
        }
    }
}

impl From<FileMode> for EntryMode {
    fn from(mode: FileMode) -> Self {
        match mode {
            FileMode::File => Self::File,
            FileMode::Executable => Self::Executable,
            FileMode::Symlink => Self::Symlink,
        }
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
        reason: &'static str,
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
            Self::InvalidObjectId { source, .. } => Some(source),
            Self::Entry { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// One staged file: its mode and the ID of its content blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    mode: FileMode,
    id: ObjectId,
}

impl IndexEntry {
    /// Returns the file mode.
    pub fn mode(&self) -> FileMode {
        self.mode
    }

    /// Returns the ID of the content blob.
    pub fn id(&self) -> ObjectId {
        self.id
    }
}

/// The staging area: staged files by path, sorted by path in UTF-8 byte order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    entries: BTreeMap<String, IndexEntry>,
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

    /// Stages a file, replacing the entry at the same path if there is one. The ID is meant to
    /// be the ID of a blob; the index cannot check that, because an [`ObjectId`] does not record
    /// the kind of object it names.
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
        id: ObjectId,
    ) -> Result<(), IndexError> {
        let path = path.into();
        if let Some(problem) = path_problem(&path) {
            return Err(IndexError::InvalidPath { path, problem });
        }
        // No parent directory of the path may be a staged file.
        let staged_parent = path
            .match_indices('/')
            .map(|(end, _)| &path[..end])
            .find(|parent| self.entries.contains_key(*parent))
            .map(str::to_owned);
        if let Some(parent) = staged_parent {
            return Err(IndexError::ParentIsFile { path, parent });
        }
        // The path must not be a directory that holds staged files. The first key from
        // `<path>/` onwards starts with it exactly if there is such a file.
        let prefix = format!("{path}/");
        let first_from_prefix = self
            .entries
            .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
            .next();
        if let Some((child, _)) = first_from_prefix {
            if child.starts_with(&prefix) {
                return Err(IndexError::HasChildren {
                    child: child.clone(),
                    path,
                });
            }
        }
        self.entries.insert(path, IndexEntry { mode, id });
        Ok(())
    }

    /// Removes the file at the path and returns its entry. Returns `None` if no file is staged
    /// at exactly this path; a directory name is not a path in the index, so removing one
    /// removes nothing.
    pub fn remove(&mut self, path: &str) -> Option<IndexEntry> {
        self.entries.remove(path)
    }

    /// Returns the entry at the path.
    pub fn get(&self, path: &str) -> Option<&IndexEntry> {
        self.entries.get(path)
    }

    /// Returns the number of staged files.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether nothing is staged.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates over the entries in path order, comparing whole paths. This is not the order of
    /// a tree; see the module documentation.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &IndexEntry)> {
        self.entries
            .iter()
            .map(|(path, entry)| (path.as_str(), entry))
    }

    /// Returns the canonical encoding: the header line, then one line
    /// `<mode> <id> <path>` per entry in path order.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = String::from(HEADER);
        for (path, entry) in &self.entries {
            let mode = EntryMode::from(entry.mode);
            out.push_str(&format!("{} {} {}\n", mode.code(), entry.id, path));
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
                reason: "the last line has no terminating newline",
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
            let (code, rest) = line.split_once(' ').ok_or_else(|| malformed(ENTRY_SHAPE))?;
            let (id, path) = rest.split_once(' ').ok_or_else(|| malformed(ENTRY_SHAPE))?;
            let mode = EntryMode::from_code(code)
                .and_then(FileMode::from_entry_mode)
                .ok_or_else(|| IndexParseError::UnknownMode {
                    line: line_number,
                    code: code.to_owned(),
                })?;
            let id = ObjectId::from_str(id).map_err(|source| IndexParseError::InvalidObjectId {
                line: line_number,
                source,
            })?;
            index
                .insert(path, mode, id)
                .map_err(|source| IndexParseError::Entry {
                    line: line_number,
                    source,
                })?;
            if let Some(previous) = previous.filter(|previous| *previous >= path) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors: these IDs are the blob IDs of `hello` and of the empty content, written out
    // so that a change to the hash or to the ID format cannot move the test with it.
    const HELLO_ID: &str = "b3:9db8039d1b509628ad5b125ea89780b69fcde5c272215d690a2e4dc69eac75ad";
    const EMPTY_ID: &str = "b3:4a0b6603381c7c38c557e9ceb3cba8876f0a7863146e7309801352e80b073b65";

    fn id(text: &str) -> ObjectId {
        text.parse().expect("test ID must be valid")
    }

    fn hello() -> ObjectId {
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
            reason: ENTRY_SHAPE,
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
            ("a/STEMMA~1", name("STEMMA~1", NameProblem::ReservedName)),
            (
                "a/\u{200b}/b",
                name("\u{200b}", NameProblem::FormatCharacter),
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
    fn windows_separators_and_case_variants_are_the_checkouts_business() {
        // Valid on Linux and macOS and accepted here, as in trees (ADR-005).
        let index = index_of(&[
            "back\\slash",
            "x\\..\\.stemma\\config",
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
    fn the_order_is_by_whole_path_bytes_and_shows_in_the_encoding() {
        let index = index_of(&["b", "a/z", "a-b", "a.b"]);
        let paths: Vec<&str> = index.iter().map(|(path, _)| path).collect();
        assert_eq!(paths, ["a-b", "a.b", "a/z", "b"]);
        let encoded = String::from_utf8(index.encode()).unwrap();
        let encoded_paths: Vec<&str> = encoded
            .lines()
            .skip(1)
            .map(|line| line.splitn(3, ' ').nth(2).unwrap())
            .collect();
        assert_eq!(encoded_paths, ["a-b", "a.b", "a/z", "b"]);
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
             100644 {HELLO_ID} a-b\n\
             100755 {EMPTY_ID} a.b\n\
             120000 {HELLO_ID} a/z\n\
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
                    reason: "the last line has no terminating newline",
                },
            ),
            (
                input(&format!("{good}100644 {HELLO_ID} b")),
                IndexParseError::Malformed {
                    line: 3,
                    reason: "the last line has no terminating newline",
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
            // `-` sorts before `/`, so a directory must not come before its look-alike `a-b`.
            (
                input(&format!("100644 {HELLO_ID} a/z\n100644 {HELLO_ID} a-b\n")),
                IndexParseError::OutOfOrder {
                    line: 3,
                    previous: "a/z".into(),
                    path: "a-b".into(),
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
    fn parse_errors_carry_their_cause() {
        let error = Index::parse(&input("100644 b3:abcd a\n")).unwrap_err();
        let cause = error.source().expect("the ID error is the cause");
        assert!(cause.to_string().contains("found 4"), "{cause}");
        assert!(error.to_string().starts_with("line 2: "), "{error}");
        let error = Index::parse(&input(&format!("100644 {HELLO_ID} /a\n"))).unwrap_err();
        assert!(error.source().is_some());
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
}
