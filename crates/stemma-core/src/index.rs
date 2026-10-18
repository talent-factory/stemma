//! The index: the set of files the next commit will contain.
//!
//! The index maps a relative path to a file mode and the ID of the blob that holds the content.
//! It is a plain in-memory model with a canonical text encoding. Reading files, writing blobs
//! to a store, and building trees from the index are separate steps.
//!
//! Paths are `/`-separated and relative. Each component must satisfy the same rules as a tree
//! entry name (ADR-005). The index holds files and symbolic links only: a directory exists
//! because a path runs through it, never as an entry of its own. A path therefore cannot be
//! both a file and a directory.
//!
//! The encoding is version 1 and starts with the line `stemma-index 1`. Each entry is one line
//! `<mode> <id> <path>`, sorted by path in UTF-8 byte order. The path comes last because it may
//! contain spaces; it cannot contain a newline. The reader is strict: it rejects anything that
//! [`Index::encode`] would not have written, so one index has exactly one encoding.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

use crate::object::{name_problem, EntryMode, NameProblem, ObjectId};

/// First line of the encoding. It carries the format version.
const HEADER: &str = "stemma-index 1\n";

/// The rule that a rejected index path violates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathProblem {
    /// The path is empty, starts or ends with `/`, or contains `//`.
    EmptyComponent,
    /// A component violates a tree entry naming rule, which includes `.` and `..`.
    Name(NameProblem),
}

impl fmt::Display for PathProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyComponent => f.write_str("the path has an empty component"),
            Self::Name(problem) => write!(f, "{problem}"),
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
    /// The mode is [`EntryMode::Directory`]. The index holds no directory entries.
    DirectoryEntry(String),
    /// The path would be both a file and a directory.
    Conflict {
        /// The path that was inserted.
        path: String,
        /// The existing entry it collides with.
        existing: String,
    },
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { path, problem } => {
                write!(f, "invalid index path `{}`: {problem}", path.escape_debug())
            }
            Self::DirectoryEntry(path) => {
                write!(
                    f,
                    "`{}` is a directory entry; the index holds files only",
                    path.escape_debug()
                )
            }
            Self::Conflict { path, existing } => write!(
                f,
                "`{}` conflicts with `{}`: a path cannot be both a file and a directory",
                path.escape_debug(),
                existing.escape_debug()
            ),
        }
    }
}

impl Error for IndexError {}

/// Reasons why an encoded index is rejected by [`Index::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexParseError {
    /// The input is not valid UTF-8.
    NotUtf8,
    /// The input does not start with the version header.
    MissingHeader,
    /// An entry line is not in canonical form. `line` counts from 1 and includes the header.
    Malformed {
        /// Line number of the entry.
        line: usize,
        /// What is wrong with it.
        reason: &'static str,
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
            Self::NotUtf8 => f.write_str("the index is not valid UTF-8"),
            Self::MissingHeader => f.write_str("the index does not start with `stemma-index 1`"),
            Self::Malformed { line, reason } => write!(f, "line {line}: {reason}"),
            Self::Entry { line, source } => write!(f, "line {line}: {source}"),
        }
    }
}

impl Error for IndexParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Entry { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// One staged file: its mode and the ID of its content blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    mode: EntryMode,
    id: ObjectId,
}

impl IndexEntry {
    /// Returns the file mode.
    pub fn mode(&self) -> EntryMode {
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

/// Checks the path rules and returns the problem, if any.
fn path_problem(path: &str) -> Option<PathProblem> {
    for component in path.split('/') {
        if component.is_empty() {
            return Some(PathProblem::EmptyComponent);
        }
        if let Some(problem) = name_problem(component) {
            return Some(PathProblem::Name(problem));
        }
    }
    None
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
    /// [`IndexError::DirectoryEntry`] for [`EntryMode::Directory`], and
    /// [`IndexError::Conflict`] if a parent of the path is a file or the path is a directory
    /// that holds entries. The index is unchanged on error.
    pub fn insert(
        &mut self,
        path: impl Into<String>,
        mode: EntryMode,
        id: ObjectId,
    ) -> Result<(), IndexError> {
        let path = path.into();
        if let Some(problem) = path_problem(&path) {
            return Err(IndexError::InvalidPath { path, problem });
        }
        if mode == EntryMode::Directory {
            return Err(IndexError::DirectoryEntry(path));
        }
        // A parent directory of the path must not be a staged file.
        let staged_parent = path
            .match_indices('/')
            .map(|(end, _)| &path[..end])
            .find(|parent| self.entries.contains_key(*parent))
            .map(str::to_owned);
        if let Some(existing) = staged_parent {
            return Err(IndexError::Conflict { path, existing });
        }
        // The path must not be a directory that holds staged files. The first key from
        // `<path>/` onwards starts with it exactly if there is such a file.
        let prefix = format!("{path}/");
        if let Some((existing, _)) = self.entries.range(prefix.clone()..).next() {
            if existing.starts_with(&prefix) {
                return Err(IndexError::Conflict {
                    existing: existing.clone(),
                    path,
                });
            }
        }
        self.entries.insert(path, IndexEntry { mode, id });
        Ok(())
    }

    /// Removes the entry at the path and returns it.
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

    /// Iterates over the entries in path order.
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
            out.push_str(&format!("{} {} {}\n", entry.mode.code(), entry.id, path));
        }
        out.into_bytes()
    }

    /// Reads an encoded index.
    ///
    /// # Errors
    ///
    /// Returns an error for anything [`encode`](Self::encode) would not have written: a
    /// missing header, a line without a terminating newline, an unknown mode, an object ID that
    /// is not in canonical form, paths that are not in strictly ascending order (which also
    /// rules out duplicates), and entries that [`insert`](Self::insert) rejects.
    pub fn parse(bytes: &[u8]) -> Result<Self, IndexParseError> {
        let text = std::str::from_utf8(bytes).map_err(|_| IndexParseError::NotUtf8)?;
        let body = text
            .strip_prefix(HEADER)
            .ok_or(IndexParseError::MissingHeader)?;
        if !body.is_empty() && !body.ends_with('\n') {
            return Err(IndexParseError::Malformed {
                line: body.lines().count() + 2,
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
            let (code, rest) = line
                .split_once(' ')
                .ok_or_else(|| malformed("expected `<mode> <id> <path>`"))?;
            let (id, path) = rest
                .split_once(' ')
                .ok_or_else(|| malformed("expected `<mode> <id> <path>`"))?;
            let mode = EntryMode::from_code(code).ok_or_else(|| malformed("unknown mode"))?;
            let id = ObjectId::from_str(id).map_err(|_| malformed("invalid object ID"))?;
            if previous.is_some_and(|previous| previous >= path) {
                return Err(malformed("paths are not in strictly ascending order"));
            }
            previous = Some(path);
            index
                .insert(path, mode, id)
                .map_err(|source| IndexParseError::Entry {
                    line: line_number,
                    source,
                })?;
        }
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Blob;

    fn blob_id(content: &str) -> ObjectId {
        Blob::new(content).id()
    }

    fn index_of(paths: &[&str]) -> Index {
        let mut index = Index::new();
        for path in paths {
            index
                .insert(*path, EntryMode::File, blob_id(path))
                .expect("valid entry");
        }
        index
    }

    #[test]
    fn insert_and_get() {
        let mut index = Index::new();
        assert!(index.is_empty());
        index
            .insert(
                "src/main.rs",
                EntryMode::Executable,
                blob_id("fn main() {}"),
            )
            .unwrap();
        let entry = index.get("src/main.rs").unwrap();
        assert_eq!(entry.mode(), EntryMode::Executable);
        assert_eq!(entry.id(), blob_id("fn main() {}"));
        assert_eq!(index.len(), 1);
        assert!(index.get("src").is_none());
    }

    #[test]
    fn insert_replaces_entry_at_same_path() {
        let mut index = index_of(&["a.txt"]);
        index
            .insert("a.txt", EntryMode::File, blob_id("new"))
            .unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index.get("a.txt").unwrap().id(), blob_id("new"));
    }

    #[test]
    fn remove_returns_entry() {
        let mut index = index_of(&["a.txt"]);
        assert!(index.remove("a.txt").is_some());
        assert!(index.remove("a.txt").is_none());
        assert!(index.is_empty());
    }

    #[test]
    fn invalid_paths_are_rejected_and_leave_index_unchanged() {
        let cases = [
            ("", PathProblem::EmptyComponent),
            ("/a", PathProblem::EmptyComponent),
            ("a/", PathProblem::EmptyComponent),
            ("a//b", PathProblem::EmptyComponent),
            ("a/../b", PathProblem::Name(NameProblem::DotName)),
            ("./a", PathProblem::Name(NameProblem::DotName)),
            ("a/.stemma/x", PathProblem::Name(NameProblem::ReservedName)),
            ("a\nb", PathProblem::Name(NameProblem::ControlCharacter)),
        ];
        let mut index = index_of(&["keep.txt"]);
        let before = index.clone();
        for (path, problem) in cases {
            let error = index
                .insert(path, EntryMode::File, blob_id("x"))
                .expect_err(path);
            assert_eq!(
                error,
                IndexError::InvalidPath {
                    path: path.to_owned(),
                    problem
                },
                "{path:?}"
            );
        }
        assert_eq!(index, before);
    }

    #[test]
    fn directory_mode_is_rejected() {
        let mut index = Index::new();
        let error = index
            .insert("dir", EntryMode::Directory, blob_id("x"))
            .unwrap_err();
        assert_eq!(error, IndexError::DirectoryEntry("dir".to_owned()));
    }

    #[test]
    fn file_cannot_become_directory_or_the_reverse() {
        let mut index = index_of(&["a", "x/y"]);
        let error = index
            .insert("a/b", EntryMode::File, blob_id("x"))
            .unwrap_err();
        assert_eq!(
            error,
            IndexError::Conflict {
                path: "a/b".to_owned(),
                existing: "a".to_owned()
            }
        );
        let error = index
            .insert("x", EntryMode::File, blob_id("x"))
            .unwrap_err();
        assert_eq!(
            error,
            IndexError::Conflict {
                path: "x".to_owned(),
                existing: "x/y".to_owned()
            }
        );
    }

    #[test]
    fn similar_prefixes_do_not_conflict() {
        // `a-b` and `a.b` sort next to `a/`, but neither is inside the directory `a`.
        let index = index_of(&["a/c", "a-b", "a.b", "ab"]);
        assert_eq!(index.len(), 4);
    }

    #[test]
    fn encoding_is_sorted_by_path_bytes() {
        let index = index_of(&["b", "a/z", "a-b", "a.b"]);
        let paths: Vec<&str> = index.iter().map(|(path, _)| path).collect();
        assert_eq!(paths, ["a-b", "a.b", "a/z", "b"]);
    }

    #[test]
    fn insertion_order_does_not_change_encoding() {
        let forward = index_of(&["a", "b/c", "d"]);
        let backward = index_of(&["d", "b/c", "a"]);
        assert_eq!(forward.encode(), backward.encode());
    }

    #[test]
    fn encoding_format_is_pinned() {
        let mut index = Index::new();
        let id = blob_id("hello");
        index.insert("my file.txt", EntryMode::File, id).unwrap();
        index.insert("run.sh", EntryMode::Executable, id).unwrap();
        let expected = format!("stemma-index 1\n100644 {id} my file.txt\n100755 {id} run.sh\n");
        assert_eq!(index.encode(), expected.into_bytes());
    }

    #[test]
    fn empty_index_encodes_to_header_only() {
        assert_eq!(Index::new().encode(), b"stemma-index 1\n");
        assert_eq!(Index::parse(b"stemma-index 1\n").unwrap(), Index::new());
    }

    #[test]
    fn encode_then_parse_roundtrips() {
        let mut index = index_of(&["a/b/c.txt", "a/d", "e f/g h", "z"]);
        index
            .insert("link", EntryMode::Symlink, blob_id("target"))
            .unwrap();
        let parsed = Index::parse(&index.encode()).unwrap();
        assert_eq!(parsed, index);
    }

    #[test]
    fn parse_rejects_non_canonical_input() {
        let id = blob_id("x");
        let good = format!("stemma-index 1\n100644 {id} a\n");
        assert!(Index::parse(good.as_bytes()).is_ok());

        let line = |text: &str| format!("stemma-index 1\n{text}");
        let malformed = [
            // missing newline at the end
            format!("stemma-index 1\n100644 {id} a"),
            // unknown mode and a mode in non-canonical form
            line(&format!("100666 {id} a\n")),
            line(&format!("0100644 {id} a\n")),
            // object ID: wrong prefix, upper case, too short
            line("100644 sha1:abc a\n"),
            line(&format!("100644 {} a\n", id.to_string().to_uppercase())),
            line("100644 b3:abcd a\n"),
            // missing fields and an empty line
            line("100644\n"),
            line(&format!("100644 {id}\n")),
            line("\n"),
            // not ascending: duplicate and reversed
            line(&format!("100644 {id} a\n100644 {id} a\n")),
            line(&format!("100644 {id} b\n100644 {id} a\n")),
        ];
        for input in &malformed {
            assert!(
                matches!(
                    Index::parse(input.as_bytes()),
                    Err(IndexParseError::Malformed { .. })
                ),
                "{input:?}"
            );
        }
    }

    #[test]
    fn parse_rejects_bad_header_and_encoding() {
        assert_eq!(Index::parse(b""), Err(IndexParseError::MissingHeader));
        assert_eq!(
            Index::parse(b"stemma-index 2\n"),
            Err(IndexParseError::MissingHeader)
        );
        assert_eq!(
            Index::parse(b"stemma-index 1\n\xff\n"),
            Err(IndexParseError::NotUtf8)
        );
    }

    #[test]
    fn parse_applies_insert_rules() {
        let id = blob_id("x");
        let invalid_path = format!("stemma-index 1\n100644 {id} a/../b\n");
        assert!(matches!(
            Index::parse(invalid_path.as_bytes()),
            Err(IndexParseError::Entry {
                line: 2,
                source: IndexError::InvalidPath { .. }
            })
        ));
        let directory = format!("stemma-index 1\n040000 {id} d\n");
        assert!(matches!(
            Index::parse(directory.as_bytes()),
            Err(IndexParseError::Entry {
                line: 2,
                source: IndexError::DirectoryEntry(_)
            })
        ));
        let conflict = format!("stemma-index 1\n100644 {id} a\n100644 {id} a/b\n");
        assert!(matches!(
            Index::parse(conflict.as_bytes()),
            Err(IndexParseError::Entry {
                line: 3,
                source: IndexError::Conflict { .. }
            })
        ));
    }
}
