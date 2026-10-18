//! Content-addressed objects: blobs, trees, and commits.
//!
//! Object IDs use BLAKE3 with the `b3:` prefix (ADR-001). Commits record their author as an
//! [`Actor`] (ADR-002). ADR-005 records the decisions behind the rules for names, actor fields,
//! timestamps, and versioning.
//!
//! The canonical encodings are defined in this module and pinned by the golden-vector tests at
//! the end of this file. They are persistent formats: any change to them changes the ID of
//! every affected object, and stored data written under the old encoding no longer matches.
//!
//! The encodings described here are version 1. An incompatible change must not reuse an object
//! kind name: it introduces a new one (for example `commit2`) together with an ADR, so that
//! old and new objects can exist side by side. A future reader has to reject what it does not
//! recognize (ADR-005); this module only writes encodings and has no reader yet.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use unicode_general_category::{get_general_category, GeneralCategory};

use crate::Actor;

/// Algorithm prefix of BLAKE3 object IDs (ADR-001).
const PREFIX: &str = "b3:";

/// Name of Stemma's own metadata directory. A tree entry name or an index path component must
/// not use it or the aliases listed at [`NameProblem::ReservedName`], so that a checkout does
/// not overwrite repository data through them. Other aliases are left to the checkout layer
/// (ADR-005).
pub const METADATA_DIR: &str = ".stemma";

/// Number of hexadecimal characters in a BLAKE3 digest (32 bytes, two characters per byte).
const HEX_LEN: usize = 64;

/// Identifies a stored object by the hash of its canonical encoding.
///
/// The textual form is `b3:` followed by 64 lowercase hexadecimal digits. Values are produced
/// only by [`Blob::id`], [`Tree::id`], [`Commit::id`], or by parsing a string.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId([u8; 32]);

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}{}", blake3::Hash::from_bytes(self.0).to_hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({self})")
    }
}

/// Reasons why a string is not a valid object ID.
///
/// A string is checked for the prefix first, then the length, then the characters.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjectIdParseError {
    /// The string does not start with the algorithm prefix `b3:`.
    MissingPrefix,
    /// The digest is not 64 bytes long. Carries the length found, in bytes.
    WrongLength(usize),
    /// The digest contains a character other than a lowercase hexadecimal digit. Carries the
    /// byte offset of the first offending character within the digest, not counting the prefix.
    NotHex(usize),
}

impl fmt::Display for ObjectIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefix => write!(f, "object ID lacks the prefix `{PREFIX}`"),
            Self::WrongLength(found) => write!(
                f,
                "object ID digest must be {HEX_LEN} bytes long, found {found}"
            ),
            Self::NotHex(position) => write!(
                f,
                "object ID digest must be lowercase hexadecimal (offending byte at digest offset {position})"
            ),
        }
    }
}

impl Error for ObjectIdParseError {}

impl FromStr for ObjectId {
    type Err = ObjectIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let digest = s
            .strip_prefix(PREFIX)
            .ok_or(ObjectIdParseError::MissingPrefix)?;
        if digest.len() != HEX_LEN {
            return Err(ObjectIdParseError::WrongLength(digest.len()));
        }
        let mut bytes = [0u8; 32];
        let nibble = |position: usize| {
            hex_value(digest.as_bytes()[position]).ok_or(ObjectIdParseError::NotHex(position))
        };
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = (nibble(2 * i)? << 4) | nibble(2 * i + 1)?;
        }
        Ok(Self(bytes))
    }
}

/// Returns the value of a lowercase hexadecimal digit, or `None` for any other byte. Uppercase
/// is rejected on purpose, so that every ID has exactly one textual form.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// The three kinds of stored objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Blob,
    Tree,
    Commit,
}

impl Kind {
    /// These names are hashed into the object ID. Changing them invalidates all existing IDs.
    fn name(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
        }
    }
}

/// Hashes an encoded object. The hashed input is the ASCII header `<kind> <body length in
/// decimal>`, a NUL byte, and the body. Because the kind is part of the hashed header,
/// identical bodies of different kinds get different IDs.
fn hash_encoding(kind: Kind, body: &[u8]) -> ObjectId {
    let header = format!("{} {}\0", kind.name(), body.len());
    let mut hasher = blake3::Hasher::new();
    hasher.update(header.as_bytes());
    hasher.update(body);
    ObjectId(*hasher.finalize().as_bytes())
}

/// Raw file content, hashed exactly as given: no line-ending or encoding normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    content: Vec<u8>,
}

impl Blob {
    /// Creates a blob from raw bytes.
    pub fn new(content: impl Into<Vec<u8>>) -> Self {
        Self {
            content: content.into(),
        }
    }

    /// Returns the raw bytes.
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Returns the object ID of this blob.
    pub fn id(&self) -> ObjectId {
        hash_encoding(Kind::Blob, &self.content)
    }
}

/// The rule that a rejected name violates. The rules apply to tree entry names and to the
/// components of index paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NameProblem {
    /// The name is empty.
    Empty,
    /// The name consists only of dots and spaces: `.` and `..`, and names such as `...` or
    /// `. ` that Windows reduces to them.
    DotName,
    /// The name contains `/`.
    PathSeparator,
    /// The name contains a control character in the sense of `char::is_control` (C0, DEL, C1),
    /// including NUL and newline, and also the line and paragraph separators U+2028 and U+2029.
    /// The tree and index encodings are line-based, so a newline would
    /// make them ambiguous; the other control characters are rejected for hygiene.
    ControlCharacter,
    /// The name contains an invisible character: a format character (Unicode category Cf) or a
    /// default-ignorable code point (property `Default_Ignorable_Code_Point`). Examples are the
    /// zero-width space, a bidirectional override, a Hangul filler, and a variation selector.
    /// Such characters let two names look identical on screen. Both sets follow Unicode 16.0,
    /// which the pinned `unicode-general-category` crate and the table in this module share
    /// (ADR-005).
    InvisibleCharacter,
    /// The name is [`METADATA_DIR`] or resolves to it on a common file system, alone or as one
    /// part of the name. It differs only in case (including look-alike letters such as the long
    /// s), has trailing dots or spaces (Windows drops them), or is a short name `STEMMA~N` with
    /// one or more digits. It may also be followed by a colon and a stream name, as in
    /// `.stemma::$INDEX_ALLOCATION`, which NTFS reads as the directory itself. And it may stand
    /// between backslashes, as in `x\.stemma\config`, which Windows reads as a path. The other
    /// uses of `:` and `\` in names are not rejected (ADR-005).
    ReservedName,
}

impl fmt::Display for NameProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "the name is empty",
            Self::DotName => "the name consists only of dots and spaces",
            Self::PathSeparator => "the name contains `/`",
            Self::ControlCharacter => "the name contains a control character",
            Self::InvisibleCharacter => "the name contains an invisible character",
            Self::ReservedName => "the name is reserved for the metadata directory",
        })
    }
}

/// Reasons why a tree entry or a set of tree entries is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TreeError {
    /// An entry name violates a naming rule.
    InvalidName {
        /// The rejected name.
        name: String,
        /// The rule it violates.
        problem: NameProblem,
    },
    /// Two entries share the same name.
    DuplicateName(String),
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name, problem } => {
                write!(
                    f,
                    "invalid tree entry name `{}`: {problem}",
                    name.escape_debug()
                )
            }
            Self::DuplicateName(name) => {
                write!(f, "duplicate tree entry name `{}`", name.escape_debug())
            }
        }
    }
}

impl Error for TreeError {}

/// File mode of a tree entry.
///
/// The entry ID of a [`Directory`](Self::Directory) is meant to refer to a tree; the ID of
/// every other mode refers to a blob. This module does not verify it, because an [`ObjectId`]
/// does not record the kind of object it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryMode {
    /// A regular file.
    File,
    /// A regular file with the executable bit set.
    Executable,
    /// A symbolic link. The blob holds the link target.
    Symlink,
    /// A subdirectory. The entry ID refers to a tree.
    Directory,
}

impl EntryMode {
    /// Mode string written into the tree and index encodings. In a tree it is part of the
    /// object ID, and in the index file it is part of the file format.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::File => "100644",
            Self::Executable => "100755",
            Self::Symlink => "120000",
            Self::Directory => "040000",
        }
    }

    /// Inverse of [`code`](Self::code). Accepts exactly the four canonical codes.
    pub(crate) fn from_code(code: &str) -> Option<Self> {
        match code {
            "100644" => Some(Self::File),
            "100755" => Some(Self::Executable),
            "120000" => Some(Self::Symlink),
            "040000" => Some(Self::Directory),
            _ => None,
        }
    }
}

/// One named entry of a tree.
///
/// Names are UTF-8 strings, so a file whose name is not valid UTF-8 cannot be stored; the
/// layer that reads files has to report that as an error. The encoding writes a name as its
/// raw UTF-8 bytes. A later switch to byte-string names would therefore leave the ID of every
/// existing tree unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    name: String,
    mode: EntryMode,
    id: ObjectId,
}

impl TreeEntry {
    /// Creates an entry.
    ///
    /// # Errors
    ///
    /// Returns [`TreeError::InvalidName`] if the name is empty, consists only of dots and
    /// spaces, contains `/` or a control or invisible character, or is a reserved name.
    pub fn new(name: impl Into<String>, mode: EntryMode, id: ObjectId) -> Result<Self, TreeError> {
        let name = name.into();
        if let Some(problem) = name_problem(&name) {
            return Err(TreeError::InvalidName { name, problem });
        }
        Ok(Self { name, mode, id })
    }

    /// Returns the entry name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the file mode.
    pub fn mode(&self) -> EntryMode {
        self.mode
    }

    /// Returns the ID of the referenced object.
    pub fn id(&self) -> ObjectId {
        self.id
    }
}

/// Code points with the Unicode property `Default_Ignorable_Code_Point` as inclusive ranges in
/// ascending order, merged where adjacent. The data is `DerivedCoreProperties.txt` of Unicode
/// 16.0, the version of the pinned `unicode-general-category` crate. The set includes code points
/// that are not assigned yet, such as U+2065 and U+FFF0 to U+FFF8, because Unicode reserves them
/// as ignorable. A new Unicode version needs a review of this table together with the crate.
const DEFAULT_IGNORABLE: &[(char, char)] = &[
    ('\u{AD}', '\u{AD}'),
    ('\u{34F}', '\u{34F}'),
    ('\u{61C}', '\u{61C}'),
    ('\u{115F}', '\u{1160}'),
    ('\u{17B4}', '\u{17B5}'),
    ('\u{180B}', '\u{180F}'),
    ('\u{200B}', '\u{200F}'),
    ('\u{202A}', '\u{202E}'),
    ('\u{2060}', '\u{206F}'),
    ('\u{3164}', '\u{3164}'),
    ('\u{FE00}', '\u{FE0F}'),
    ('\u{FEFF}', '\u{FEFF}'),
    ('\u{FFA0}', '\u{FFA0}'),
    ('\u{FFF0}', '\u{FFF8}'),
    ('\u{1BCA0}', '\u{1BCA3}'),
    ('\u{1D173}', '\u{1D17A}'),
    ('\u{E0000}', '\u{E0FFF}'),
];

/// Whether the character is invisible: a format character (Unicode general category Cf) or a
/// default-ignorable code point. The two sets overlap but neither contains the other.
fn is_invisible_character(c: char) -> bool {
    get_general_category(c) == GeneralCategory::Format
        || DEFAULT_IGNORABLE
            .binary_search_by(|&(first, last)| {
                if c < first {
                    std::cmp::Ordering::Greater
                } else if c > last {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
}

/// Whether the character is a control character (`char::is_control`) or a line or paragraph
/// separator (U+2028 and U+2029), which break a line in a listing like a newline.
fn is_control_or_separator(c: char) -> bool {
    c.is_control() || c == '\u{2028}' || c == '\u{2029}'
}

/// Whether the name is the metadata directory or an alias of it on a common file system, or
/// contains it as a backslash-separated segment.
///
/// Windows reads `\` as a path separator, so each segment between backslashes is checked on its
/// own. The part of a segment before the first `:` is the file name, and the rest is an NTFS
/// stream, so only that part is compared. Trailing dots and spaces are ignored because Windows
/// drops them. The comparison folds case by Unicode uppercase mapping, which also catches
/// look-alikes such as the long s (U+017F). The 8.3 short name of `.stemma` is `STEMMA~N`, where
/// N is a number. Invisible characters are rejected before this check. Short names that
/// Windows derives from a hash, as it does when many similar names exist, cannot be listed.
fn is_reserved_name(name: &str) -> bool {
    name.split('\\').any(|segment| {
        let file_name = segment.split(':').next().unwrap_or(segment);
        let folded = file_name.trim_end_matches(['.', ' ']).to_uppercase();
        folded == METADATA_DIR.to_uppercase()
            || folded.strip_prefix("STEMMA~").is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            })
    })
}

/// Returns the first naming rule the name violates, if any. The rules are checked in the order
/// of the [`NameProblem`] variants. The name is a tree entry name or one component of an index
/// path.
///
/// Only the absence of `\n` is needed for the line-based tree and index encodings to be
/// unambiguous.
/// The other rules protect checkouts and readers: `/` is rejected because names are single path
/// components, dot-only names because Windows reduces them to `.` or `..`, and control and
/// invisible characters so that names do not hide their content. Names are compared and hashed as
/// given: there is no Unicode normalization, and no case folding for uniqueness. The
/// reserved-name check is the one place that folds case, on purpose.
pub(crate) fn name_problem(name: &str) -> Option<NameProblem> {
    if name.is_empty() {
        Some(NameProblem::Empty)
    } else if name.chars().all(|c| c == '.' || c == ' ') {
        Some(NameProblem::DotName)
    } else if name.contains('/') {
        Some(NameProblem::PathSeparator)
    } else if name.chars().any(is_control_or_separator) {
        Some(NameProblem::ControlCharacter)
    } else if name.chars().any(is_invisible_character) {
        Some(NameProblem::InvisibleCharacter)
    } else if is_reserved_name(name) {
        Some(NameProblem::ReservedName)
    } else {
        None
    }
}

/// A directory: a list of named entries, sorted by name in UTF-8 byte order. Unlike in Git,
/// directories get no special treatment in the order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    entries: Vec<TreeEntry>,
}

impl Tree {
    /// Builds a tree. Entries are sorted by name, so the input order does not matter.
    ///
    /// # Errors
    ///
    /// Returns [`TreeError::DuplicateName`] if two entries share a name. Names were already
    /// validated by [`TreeEntry::new`].
    pub fn new(mut entries: Vec<TreeEntry>) -> Result<Self, TreeError> {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        if let Some(pair) = entries.windows(2).find(|pair| pair[0].name == pair[1].name) {
            return Err(TreeError::DuplicateName(pair[0].name.clone()));
        }
        Ok(Self { entries })
    }

    /// Returns the entries in name order.
    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    /// Returns the canonical encoding. Each entry is `<mode> <name>\n<id>\n`, where `<mode>` is
    /// the code of the [`EntryMode`] and `<id>` is the textual form (`b3:` plus hex), not raw
    /// bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in &self.entries {
            out.extend_from_slice(
                format!("{} {}\n{}\n", entry.mode.code(), entry.name, entry.id).as_bytes(),
            );
        }
        out
    }

    /// Returns the object ID of this tree.
    pub fn id(&self) -> ObjectId {
        hash_encoding(Kind::Tree, &self.encode())
    }
}

/// Reasons why a commit is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommitError {
    /// An author field is empty or contains a character outside visible ASCII
    /// (U+0021 to U+007E, no space). Whitespace and control characters would make the line-based
    /// encoding ambiguous; invisible characters and look-alike letters from other scripts
    /// would let identities be spoofed (look-alikes within ASCII, such as `l` and `I`, remain
    /// possible). Carries the name of the field: `id`, `model`, `session`, or `sponsor`.
    InvalidActorField(&'static str),
    /// The same parent is listed twice.
    DuplicateParent(ObjectId),
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidActorField(field) => write!(
                f,
                "author field `{field}` must be non-empty visible ASCII (U+0021 to U+007E)"
            ),
            Self::DuplicateParent(id) => write!(f, "duplicate parent {id}"),
        }
    }
}

impl Error for CommitError {}

/// A snapshot of a tree with its parents, its author, and a message.
///
/// The timestamp is the time the author claims, in seconds since the Unix epoch (UTC). It is
/// self-reported and proves nothing; only signatures make it trustworthy (ADR-003, ADR-004).
/// Two commits that differ only in the timestamp are different objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: Actor,
    timestamp: i64,
    message: String,
}

impl Commit {
    /// Creates a commit. Parents are kept in the given order, and the order is part of the ID.
    /// Any `i64` is accepted as the timestamp, including negative values (before 1970).
    ///
    /// # Errors
    ///
    /// Returns [`CommitError::InvalidActorField`] if an author field is empty or not visible ASCII, and
    /// [`CommitError::DuplicateParent`] if a parent is listed twice. The message is not
    /// validated: it comes last in the encoding and cannot interfere with the headers.
    pub fn new(
        tree: ObjectId,
        parents: Vec<ObjectId>,
        author: Actor,
        timestamp: i64,
        message: impl Into<String>,
    ) -> Result<Self, CommitError> {
        validate_actor(&author)?;
        for (i, parent) in parents.iter().enumerate() {
            if parents[..i].contains(parent) {
                return Err(CommitError::DuplicateParent(*parent));
            }
        }
        Ok(Self {
            tree,
            parents,
            author,
            timestamp,
            message: message.into(),
        })
    }

    /// Returns the ID of the tree this commit snapshots.
    pub fn tree(&self) -> ObjectId {
        self.tree
    }

    /// Returns the parents in the order given at creation.
    pub fn parents(&self) -> &[ObjectId] {
        &self.parents
    }

    /// Returns the author.
    pub fn author(&self) -> &Actor {
        &self.author
    }

    /// Returns the claimed time of the commit in seconds since the Unix epoch (UTC).
    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }

    /// Returns the message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the canonical encoding: the headers `tree <id>`, zero or more `parent <id>`,
    /// `author <actor line>`, and `time <seconds>`, in this order and each on its own line, then
    /// a blank line, then the message verbatim, with nothing appended. IDs are in textual
    /// form, the seconds are a decimal integer without a plus sign or leading zeros, and the
    /// actor line is described at `actor_line`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = format!("tree {}\n", self.tree);
        for parent in &self.parents {
            out.push_str(&format!("parent {parent}\n"));
        }
        out.push_str(&format!("author {}\n", actor_line(&self.author)));
        out.push_str(&format!("time {}\n", self.timestamp));
        out.push('\n');
        out.push_str(&self.message);
        out.into_bytes()
    }

    /// Returns the object ID of this commit.
    pub fn id(&self) -> ObjectId {
        hash_encoding(Kind::Commit, &self.encode())
    }
}

/// Returns the label and the fields of an actor, the identifier first. This is the single list
/// from which both [`validate_actor`] and [`actor_line`] work, so a new field is always
/// checked and always encoded.
fn actor_fields(actor: &Actor) -> (&'static str, Vec<(&'static str, &str)>) {
    match actor {
        Actor::Human { id } => ("human", vec![("id", id)]),
        Actor::Agent {
            id,
            model,
            session,
            sponsor,
        } => (
            "agent",
            vec![
                ("id", id),
                ("model", model),
                ("session", session),
                ("sponsor", sponsor),
            ],
        ),
    }
}

/// Checks that every field of the actor is non-empty visible ASCII (U+0021 to U+007E). That
/// makes the header line unambiguous and keeps identities free of invisible characters and of
/// look-alike letters from other scripts. The first invalid field in the order of
/// [`actor_fields`] is reported.
fn validate_actor(actor: &Actor) -> Result<(), CommitError> {
    for (field, value) in actor_fields(actor).1 {
        if value.is_empty() || !value.chars().all(|c| c.is_ascii_graphic()) {
            return Err(CommitError::InvalidActorField(field));
        }
    }
    Ok(())
}

/// Encodes an actor on one line: `human <id>` or
/// `agent <id> model=<model> session=<session> sponsor=<sponsor>`.
///
/// The fields are not escaped. [`validate_actor`] guarantees that none is empty or contains a
/// space, so the line splits unambiguously at spaces. A value may still contain `=`, so a
/// parser must split each `key=value` token at its first `=`.
fn actor_line(actor: &Actor) -> String {
    let (label, fields) = actor_fields(actor);
    let mut line = label.to_owned();
    for (i, (name, value)) in fields.into_iter().enumerate() {
        if i == 0 {
            line.push_str(&format!(" {value}"));
        } else {
            line.push_str(&format!(" {name}={value}"));
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors. The body of each was assembled by hand as `<kind> <length>\0<body>` and
    // hashed with `blake3::hash` in a throwaway program, not through this module. To check one,
    // rebuild the body from the expected encoding asserted in the tests below.
    const TIME: i64 = 1_700_000_000;
    const HELLO_ID: &str = "b3:9db8039d1b509628ad5b125ea89780b69fcde5c272215d690a2e4dc69eac75ad";
    const EMPTY_ID: &str = "b3:4a0b6603381c7c38c557e9ceb3cba8876f0a7863146e7309801352e80b073b65";
    const TREE_ID: &str = "b3:0eec8324e510a63821fcbfda90ce4ed54b01a4db9efd829f7440967808796eb6";
    const HUMAN_COMMIT_ID: &str =
        "b3:1d719c9e1d926eb0f33467406bf50911eca1d59f976105abd3eb5ccfaabdeed9";
    const AGENT_COMMIT_ID: &str =
        "b3:4feb2d73a2c706e33c2a71f6f03cbfb48f0a20b48567936fca293159e7e3b32c";
    const EMPTY_TREE_ID: &str =
        "b3:06b8469ad46d7051ad63f82809ef66c6d738d08cc3f1a5c842f3d66c05f64cf7";
    const ALL_MODES_TREE_ID: &str =
        "b3:7550c6d417278c76a35d3399bd4d18fab9fac01132b48cc1825b52c287450d3a";
    const MERGE_COMMIT_ID: &str =
        "b3:8cb48a697fe25a78eeb950e011e7dc9897e8216d13e21f3210db70ce2d9db3e8";

    fn human() -> Actor {
        Actor::Human {
            id: "daniel".into(),
        }
    }

    fn agent() -> Actor {
        Actor::Agent {
            id: "agent-1".into(),
            model: "claude-haiku-5-5".into(),
            session: "s1".into(),
            sponsor: "daniel".into(),
        }
    }

    fn id(text: &str) -> ObjectId {
        text.parse().expect("test ID must be valid")
    }

    fn entry(name: &str, mode: EntryMode, target: ObjectId) -> TreeEntry {
        TreeEntry::new(name, mode, target).expect("test entry must be valid")
    }

    fn sample_tree() -> Tree {
        Tree::new(vec![
            entry("b.sh", EntryMode::Executable, id(EMPTY_ID)),
            entry("a.txt", EntryMode::File, id(HELLO_ID)),
        ])
        .expect("valid tree")
    }

    fn empty_tree_id() -> ObjectId {
        Tree::new(vec![]).expect("empty tree is valid").id()
    }

    fn commit(parents: Vec<ObjectId>, author: Actor, message: &str) -> Commit {
        Commit::new(empty_tree_id(), parents, author, TIME, message).expect("valid commit")
    }

    // Object IDs

    #[test]
    fn blob_ids_match_golden_vectors() {
        assert_eq!(Blob::new("hello").id().to_string(), HELLO_ID);
        assert_eq!(Blob::new("").id().to_string(), EMPTY_ID);
    }

    #[test]
    fn hashed_input_is_kind_length_nul_and_body() {
        // A body of 12 bytes shows that the length is written in decimal.
        let expected = blake3::hash(b"blob 12\0hello, world");
        assert_eq!(
            hash_encoding(Kind::Blob, b"hello, world").0,
            *expected.as_bytes()
        );
    }

    #[test]
    fn blob_keeps_binary_content_unchanged() {
        let bytes = [0u8, 255, 10, 13, 0];
        let blob = Blob::new(bytes);
        assert_eq!(blob.content(), bytes);
        assert_ne!(blob.id(), Blob::new([0u8, 255, 10, 10, 0]).id());
    }

    #[test]
    fn same_content_gives_same_id_and_different_content_differs() {
        assert_eq!(Blob::new("a").id(), Blob::new("a").id());
        assert_ne!(Blob::new("a").id(), Blob::new("b").id());
    }

    #[test]
    fn kind_is_part_of_the_hash_for_real_objects() {
        let tree = sample_tree();
        assert_ne!(Blob::new(tree.encode()).id(), tree.id());
        let commit = commit(vec![], human(), "m");
        assert_ne!(Blob::new(commit.encode()).id(), commit.id());
        assert_ne!(
            hash_encoding(Kind::Tree, &commit.encode()),
            hash_encoding(Kind::Commit, &commit.encode())
        );
    }

    #[test]
    fn object_id_roundtrips_through_text() {
        let original = Blob::new("data").id();
        let parsed: ObjectId = original.to_string().parse().expect("own output must parse");
        assert_eq!(parsed, original);
        assert_eq!(id(HELLO_ID).to_string(), HELLO_ID);
    }

    #[test]
    fn parse_rejects_malformed_ids() {
        let digest = "0".repeat(HEX_LEN);
        let cases: Vec<(String, ObjectIdParseError)> = vec![
            (String::new(), ObjectIdParseError::MissingPrefix),
            ("sha1:00".into(), ObjectIdParseError::MissingPrefix),
            ("B3:00".into(), ObjectIdParseError::MissingPrefix),
            (format!(" b3:{digest}"), ObjectIdParseError::MissingPrefix),
            ("b3:".into(), ObjectIdParseError::WrongLength(0)),
            ("b3:abc".into(), ObjectIdParseError::WrongLength(3)),
            (
                format!("b3:{}", &digest[1..]),
                ObjectIdParseError::WrongLength(63),
            ),
            (format!("b3:{digest}0"), ObjectIdParseError::WrongLength(65)),
            (
                format!("b3:{digest}\n"),
                ObjectIdParseError::WrongLength(65),
            ),
            (
                format!("b3:{}", "A".repeat(HEX_LEN)),
                ObjectIdParseError::NotHex(0),
            ),
            (
                format!("b3:{}g", &digest[1..]),
                ObjectIdParseError::NotHex(63),
            ),
            (
                format!("b3:{}A0", &digest[2..]),
                ObjectIdParseError::NotHex(62),
            ),
            // 32 two-byte characters make 64 bytes, so the length check passes first.
            (
                format!("b3:{}", "é".repeat(32)),
                ObjectIdParseError::NotHex(0),
            ),
            // 30 two-byte characters are 60 bytes: the length check reports it.
            (
                format!("b3:{}", "é".repeat(30)),
                ObjectIdParseError::WrongLength(60),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(input.parse::<ObjectId>(), Err(expected), "input {input:?}");
        }
    }

    #[test]
    fn parse_error_messages_name_the_cause() {
        assert!(ObjectIdParseError::WrongLength(3)
            .to_string()
            .contains("found 3"));
        assert!(ObjectIdParseError::NotHex(7)
            .to_string()
            .contains("offset 7"));
        assert!(ObjectIdParseError::MissingPrefix
            .to_string()
            .contains("b3:"));
    }

    // Trees

    #[test]
    fn tree_encoding_and_id_match_golden_vectors() {
        let expected = format!("100644 a.txt\n{HELLO_ID}\n100755 b.sh\n{EMPTY_ID}\n");
        let tree = sample_tree();
        assert_eq!(tree.encode(), expected.as_bytes());
        assert_eq!(tree.id().to_string(), TREE_ID);
    }

    #[test]
    fn empty_tree_encodes_to_nothing_and_matches_its_golden_id() {
        let tree = Tree::new(vec![]).expect("empty tree is valid");
        assert!(tree.encode().is_empty());
        assert_eq!(tree.id().to_string(), EMPTY_TREE_ID);
    }

    #[test]
    fn tree_with_all_four_modes_matches_its_golden_vector() {
        let tree = Tree::new(vec![
            entry("d-run", EntryMode::Executable, id(EMPTY_ID)),
            entry("c-link", EntryMode::Symlink, id(HELLO_ID)),
            entry("b-file", EntryMode::File, id(HELLO_ID)),
            entry("a-dir", EntryMode::Directory, id(TREE_ID)),
        ])
        .expect("valid tree");
        let expected = format!(
            "040000 a-dir\n{TREE_ID}\n100644 b-file\n{HELLO_ID}\n\
             120000 c-link\n{HELLO_ID}\n100755 d-run\n{EMPTY_ID}\n"
        );
        assert_eq!(tree.encode(), expected.as_bytes());
        assert_eq!(tree.id().to_string(), ALL_MODES_TREE_ID);
    }

    #[test]
    fn tree_id_does_not_depend_on_entry_order() {
        let a = entry("a.txt", EntryMode::File, id(HELLO_ID));
        let b = entry("b.txt", EntryMode::Executable, id(HELLO_ID));
        let forward = Tree::new(vec![a.clone(), b.clone()]).expect("valid tree");
        let backward = Tree::new(vec![b, a]).expect("valid tree");
        assert_eq!(forward.id(), backward.id());
        assert_eq!(forward.entries(), backward.entries());
    }

    #[test]
    fn tree_sorts_by_utf8_byte_order() {
        let tree = Tree::new(
            ["a.txt", "a-b", "a", "B", "é"]
                .map(|name| entry(name, EntryMode::File, id(HELLO_ID)))
                .to_vec(),
        )
        .expect("valid tree");
        let names: Vec<&str> = tree.entries().iter().map(TreeEntry::name).collect();
        assert_eq!(names, ["B", "a", "a-b", "a.txt", "é"]);
    }

    #[test]
    fn names_are_encoded_as_raw_utf8_bytes() {
        let tree =
            Tree::new(vec![entry("\u{e9}", EntryMode::File, id(HELLO_ID))]).expect("valid tree");
        let expected = [
            b"100644 ".as_slice(),
            &[0xc3, 0xa9],
            format!("\n{HELLO_ID}\n").as_bytes(),
        ]
        .concat();
        assert_eq!(tree.encode(), expected);
    }

    #[test]
    fn directories_sort_like_files_not_as_if_they_ended_in_a_slash() {
        // Git sorts the directory `a` as `a/`, after `a-b` and `a.txt`. Stemma does not.
        let tree = Tree::new(vec![
            entry("a.txt", EntryMode::File, id(HELLO_ID)),
            entry("a-b", EntryMode::File, id(HELLO_ID)),
            entry("a", EntryMode::Directory, id(TREE_ID)),
        ])
        .expect("valid tree");
        let names: Vec<&str> = tree.entries().iter().map(TreeEntry::name).collect();
        assert_eq!(names, ["a", "a-b", "a.txt"]);
    }

    #[test]
    fn tree_order_is_utf8_byte_order_not_utf16() {
        // U+FF5E is EF BD 9E in UTF-8 and U+1F600 is F0 9F 98 80, so U+FF5E sorts first. In
        // UTF-16 the order would be reversed (D83D DE00 before FF5E).
        let tree = Tree::new(
            ["\u{1f600}", "\u{ff5e}"]
                .map(|name| entry(name, EntryMode::File, id(HELLO_ID)))
                .to_vec(),
        )
        .expect("valid tree");
        let names: Vec<&str> = tree.entries().iter().map(TreeEntry::name).collect();
        assert_eq!(names, ["\u{ff5e}", "\u{1f600}"]);
    }

    #[test]
    fn tree_id_depends_on_mode_and_target() {
        let build = |mode, target| {
            Tree::new(vec![entry("f", mode, target)])
                .expect("valid tree")
                .id()
        };
        let base = build(EntryMode::File, id(HELLO_ID));
        assert_ne!(base, build(EntryMode::Executable, id(HELLO_ID)));
        assert_ne!(base, build(EntryMode::Symlink, id(HELLO_ID)));
        assert_ne!(base, build(EntryMode::Directory, id(HELLO_ID)));
        assert_ne!(base, build(EntryMode::File, id(EMPTY_ID)));
    }

    #[test]
    fn tree_entry_rejects_invalid_names() {
        let cases = [
            ("", NameProblem::Empty),
            (".", NameProblem::DotName),
            ("..", NameProblem::DotName),
            ("a/b", NameProblem::PathSeparator),
            ("/", NameProblem::PathSeparator),
            ("a\nb", NameProblem::ControlCharacter),
            ("x\n100755 evil", NameProblem::ControlCharacter),
            ("a\rb", NameProblem::ControlCharacter),
            ("a\0b", NameProblem::ControlCharacter),
            ("a\u{7f}b", NameProblem::ControlCharacter),
            ("a\u{85}b", NameProblem::ControlCharacter),
            ("READ\u{200b}ME", NameProblem::InvisibleCharacter),
            ("a\u{200d}b", NameProblem::InvisibleCharacter),
            ("a\u{202e}b", NameProblem::InvisibleCharacter),
            ("a\u{feff}", NameProblem::InvisibleCharacter),
            ("a\u{ad}b", NameProblem::InvisibleCharacter),
            ("a\u{e0001}", NameProblem::InvisibleCharacter),
            // Default-ignorable code points that are not format characters.
            ("a\u{34f}b", NameProblem::InvisibleCharacter),
            ("a\u{115f}", NameProblem::InvisibleCharacter),
            ("a\u{1160}", NameProblem::InvisibleCharacter),
            ("\u{3164}", NameProblem::InvisibleCharacter),
            ("\u{ffa0}", NameProblem::InvisibleCharacter),
            ("READ\u{fe0f}ME", NameProblem::InvisibleCharacter),
            ("\u{2764}\u{fe0f}", NameProblem::InvisibleCharacter),
            ("a\u{e0100}b", NameProblem::InvisibleCharacter),
            ("a\u{180b}", NameProblem::InvisibleCharacter),
            ("a\u{17b4}", NameProblem::InvisibleCharacter),
            // Format characters that are visible are rejected too: Cf is part of the rule.
            ("a\u{600}b", NameProblem::InvisibleCharacter),
            ("a\u{70f}b", NameProblem::InvisibleCharacter),
            // Line and paragraph separators count as control characters.
            ("a\u{2028}b", NameProblem::ControlCharacter),
            ("a\u{2029}b", NameProblem::ControlCharacter),
            ("\u{2028}\u{200b}", NameProblem::ControlCharacter),
            ("...", NameProblem::DotName),
            (" ", NameProblem::DotName),
            (". ", NameProblem::DotName),
            (".. ", NameProblem::DotName),
            (" . . ", NameProblem::DotName),
            ("a\u{200c}b", NameProblem::InvisibleCharacter),
            (".stemma", NameProblem::ReservedName),
            (".STEMMA", NameProblem::ReservedName),
            (".Stemma", NameProblem::ReservedName),
            (".stemma.", NameProblem::ReservedName),
            (".stemma ", NameProblem::ReservedName),
            (".stemma. .", NameProblem::ReservedName),
            ("stemma~1", NameProblem::ReservedName),
            ("stemma~2", NameProblem::ReservedName),
            ("STEMMA~10", NameProblem::ReservedName),
            // NTFS streams: the part before the first colon is the file name.
            (".stemma:x", NameProblem::ReservedName),
            (".stemma::$INDEX_ALLOCATION", NameProblem::ReservedName),
            (".STEMMA::$DATA", NameProblem::ReservedName),
            (".stemma .:x", NameProblem::ReservedName),
            (".stemma:", NameProblem::ReservedName),
            ("STEMMA~1:x", NameProblem::ReservedName),
            ("stemma~2::$INDEX_ALLOCATION", NameProblem::ReservedName),
            // Windows reads a backslash as a path separator.
            ("x\\.stemma", NameProblem::ReservedName),
            (".stemma\\x", NameProblem::ReservedName),
            ("x\\.stemma\\config", NameProblem::ReservedName),
            ("x\\..\\.stemma\\config", NameProblem::ReservedName),
            ("a\\STEMMA~1\\b", NameProblem::ReservedName),
            ("a\\.stemma. \\b", NameProblem::ReservedName),
            ("a\\.stemma:x", NameProblem::ReservedName),
            ("C:\\.stemma", NameProblem::ReservedName),
            (".\\.stemma", NameProblem::ReservedName),
            ("STEMMA~1.", NameProblem::ReservedName),
            ("stemma~1 .", NameProblem::ReservedName),
            // An invisible character is reported before the reserved name it would hide.
            (".stem\u{200b}ma", NameProblem::InvisibleCharacter),
            ("STEMMA~1", NameProblem::ReservedName),
            (".\u{17f}temma", NameProblem::ReservedName),
            // A control character is reported before an invisible character.
            ("\u{200b}\n", NameProblem::ControlCharacter),
            // The first violated rule wins: a separator is reported before a control character.
            ("a/\n", NameProblem::PathSeparator),
            ("/.", NameProblem::PathSeparator),
        ];
        for (name, problem) in cases {
            assert_eq!(
                TreeEntry::new(name, EntryMode::File, id(HELLO_ID)),
                Err(TreeError::InvalidName {
                    name: name.into(),
                    problem
                }),
                "name {name:?}"
            );
        }
    }

    #[test]
    fn only_the_metadata_directory_itself_is_reserved() {
        for name in [
            ".stemma-notes",
            ".stemma.bak",
            "a.stemma",
            "stemma",
            "stemma~",
            "stemma~x",
            "stemma~1x",
            ".stemma~1",
            ".stemmas",
            "_.stemma",
        ] {
            let built = TreeEntry::new(name, EntryMode::File, id(HELLO_ID));
            assert!(built.is_ok(), "name {name:?} should be accepted");
        }
        assert_eq!(METADATA_DIR, ".stemma");
    }

    #[test]
    fn tree_entry_accepts_unusual_but_valid_names() {
        for name in [
            ".hidden",
            "a b",
            "tab\u{a0}nbsp",
            "日本語",
            "e\u{301}",
            "a.",
            ".a",
            "..a",
            // Valid on Linux and macOS. The checkout layer has to guard Windows (ADR-005).
            "report:final.txt",
            "back\\slash",
            "x\\.stemma-notes\\config",
            "a:b:c",
            // Only the part before the first colon is the file name.
            "a:.stemma",
            "a:b\\c",
            "C:\\Users",
            ".stemma-notes:x",
            "stemma:x",
            // Visible marks and letters next to the ranges of invisible characters.
            "a\u{ac}b",
            "a\u{ae}b",
            "\u{1f600}",
            "\u{2764}",
            "a\u{2800}b",
            "\u{3163}\u{3165}",
            "a\u{fdff}b",
        ] {
            let built = TreeEntry::new(name, EntryMode::File, id(HELLO_ID));
            assert!(built.is_ok(), "name {name:?} should be accepted");
        }
    }

    #[test]
    fn tree_entry_exposes_its_fields() {
        let e = entry("run.sh", EntryMode::Executable, id(HELLO_ID));
        assert_eq!(
            (e.name(), e.mode(), e.id()),
            ("run.sh", EntryMode::Executable, id(HELLO_ID))
        );
    }

    #[test]
    fn tree_rejects_duplicate_names_even_when_not_adjacent_in_the_input() {
        let tree = Tree::new(vec![
            entry("same", EntryMode::File, id(HELLO_ID)),
            entry("other", EntryMode::File, id(HELLO_ID)),
            entry("same", EntryMode::Executable, id(EMPTY_ID)),
        ]);
        assert_eq!(tree, Err(TreeError::DuplicateName("same".into())));
    }

    #[test]
    fn names_differing_only_in_case_or_normalization_are_distinct() {
        let tree = Tree::new(vec![
            entry("README", EntryMode::File, id(HELLO_ID)),
            entry("readme", EntryMode::File, id(HELLO_ID)),
            entry("\u{e9}", EntryMode::File, id(HELLO_ID)),
            entry("e\u{301}", EntryMode::File, id(HELLO_ID)),
        ]);
        assert_eq!(tree.expect("distinct names").entries().len(), 4);
    }

    #[test]
    fn tree_error_message_escapes_control_characters() {
        let err = TreeEntry::new("a\nb", EntryMode::File, id(HELLO_ID)).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("a\\nb") && !text.contains('\n'), "{text}");
    }

    // Commits

    #[test]
    fn human_commit_encoding_and_id_match_golden_vectors() {
        let commit = Commit::new(
            id(TREE_ID),
            vec![id(EMPTY_ID)],
            human(),
            TIME,
            "first change",
        )
        .expect("valid commit");
        let expected = format!(
            "tree {TREE_ID}\nparent {EMPTY_ID}\nauthor human daniel\ntime 1700000000\n\nfirst change"
        );
        assert_eq!(commit.encode(), expected.as_bytes());
        assert_eq!(commit.id().to_string(), HUMAN_COMMIT_ID);
    }

    #[test]
    fn agent_commit_encoding_and_id_match_golden_vectors() {
        let commit = Commit::new(id(TREE_ID), vec![], agent(), TIME, "work").expect("valid commit");
        let expected = format!(
            "tree {TREE_ID}\nauthor agent agent-1 model=claude-haiku-5-5 session=s1 sponsor=daniel\ntime 1700000000\n\nwork"
        );
        assert_eq!(commit.encode(), expected.as_bytes());
        assert_eq!(commit.id().to_string(), AGENT_COMMIT_ID);
    }

    #[test]
    fn merge_commit_encoding_and_id_match_golden_vectors() {
        let commit = Commit::new(
            id(TREE_ID),
            vec![id(HELLO_ID), id(EMPTY_ID)],
            human(),
            TIME,
            "merge",
        )
        .expect("valid commit");
        let expected = format!(
            "tree {TREE_ID}\nparent {HELLO_ID}\nparent {EMPTY_ID}\nauthor human daniel\ntime 1700000000\n\nmerge"
        );
        assert_eq!(commit.encode(), expected.as_bytes());
        assert_eq!(commit.id().to_string(), MERGE_COMMIT_ID);
    }

    #[test]
    fn timestamp_is_part_of_the_id_and_written_as_a_plain_decimal() {
        let at = |time: i64| {
            Commit::new(empty_tree_id(), vec![], human(), time, "m").expect("valid commit")
        };
        assert_ne!(at(1).id(), at(2).id());
        for (time, text) in [
            (0, "time 0\n"),
            (7, "time 7\n"),
            (-1, "time -1\n"),
            (i64::MAX, "time 9223372036854775807\n"),
            (i64::MIN, "time -9223372036854775808\n"),
        ] {
            let commit = at(time);
            let encoded = String::from_utf8(commit.encode()).expect("encoding is UTF-8");
            assert!(encoded.contains(text), "{time}: {encoded}");
            assert_eq!(commit.timestamp(), time);
        }
    }

    #[test]
    fn headers_come_in_the_fixed_order_tree_parent_author_time() {
        let c = commit(vec![id(HELLO_ID)], human(), "m");
        let text = String::from_utf8(c.encode()).expect("encoding is UTF-8");
        let order: Vec<&str> = text
            .split("\n\n")
            .next()
            .expect("headers precede the blank line")
            .lines()
            .map(|line| line.split(' ').next().expect("every line has a key"))
            .collect();
        assert_eq!(order, ["tree", "parent", "author", "time"]);
    }

    #[test]
    fn equals_sign_inside_a_value_is_allowed_and_encoded_verbatim() {
        let author = Actor::Agent {
            id: "a".into(),
            model: "m=1".into(),
            session: "s".into(),
            sponsor: "p".into(),
        };
        let c = commit(vec![], author, "m");
        let text = String::from_utf8(c.encode()).expect("encoding is UTF-8");
        assert!(text.contains("\nauthor agent a model=m=1 session=s sponsor=p\n"));
    }

    #[test]
    fn human_and_agent_with_the_same_id_differ() {
        let as_human = Actor::Human { id: "x".into() };
        let as_agent = Actor::Agent {
            id: "x".into(),
            model: "m".into(),
            session: "s".into(),
            sponsor: "p".into(),
        };
        assert_ne!(
            commit(vec![], as_human, "m").id(),
            commit(vec![], as_agent, "m").id()
        );
    }

    #[test]
    fn commit_id_changes_with_parent() {
        let root = commit(vec![], human(), "same message");
        let child = commit(vec![root.id()], human(), "same message");
        assert_ne!(root.id(), child.id());
    }

    #[test]
    fn parent_order_is_part_of_the_id_and_merges_list_every_parent() {
        let (a, b) = (id(HELLO_ID), id(EMPTY_ID));
        let forward = commit(vec![a, b], human(), "merge");
        let backward = commit(vec![b, a], human(), "merge");
        assert_ne!(forward.id(), backward.id());
        assert_eq!(forward.parents(), [a, b]);
        let text = String::from_utf8(forward.encode()).expect("encoding is UTF-8");
        assert_eq!(text.matches("\nparent ").count(), 2);
    }

    #[test]
    fn message_is_kept_verbatim_after_the_blank_line() {
        for message in ["", "line\n\nbody\n", "\nstarts with newline"] {
            let c = commit(vec![], human(), message);
            let text = String::from_utf8(c.encode()).expect("encoding is UTF-8");
            let (_, body) = text
                .split_once("\n\n")
                .expect("blank line separates headers");
            assert_eq!(body, message);
            assert_eq!(c.message(), message);
        }
        assert_ne!(
            commit(vec![], human(), "a").id(),
            commit(vec![], human(), "b").id()
        );
    }

    #[test]
    fn commit_rejects_duplicate_parents() {
        let p = id(HELLO_ID);
        let result = Commit::new(
            empty_tree_id(),
            vec![p, id(EMPTY_ID), p],
            human(),
            TIME,
            "m",
        );
        assert_eq!(result, Err(CommitError::DuplicateParent(p)));
    }

    #[test]
    fn commit_rejects_author_fields_that_are_not_visible_ascii() {
        let with_agent = |id: &str, model: &str, session: &str, sponsor: &str| Actor::Agent {
            id: id.into(),
            model: model.into(),
            session: session.into(),
            sponsor: sponsor.into(),
        };
        let cases = [
            (Actor::Human { id: String::new() }, "id"),
            (Actor::Human { id: "x\n".into() }, "id"),
            (
                Actor::Human {
                    id: "x\n\nfake".into(),
                },
                "id",
            ),
            (
                Actor::Human {
                    id: "two words".into(),
                },
                "id",
            ),
            (with_agent("a", "m session=s", "t", "p"), "model"),
            (with_agent("a", "m", "s sponsor=q", "p"), "session"),
            (with_agent("a", "m", "s", "p\r"), "sponsor"),
            (with_agent("a", "m", "s", ""), "sponsor"),
            (with_agent("a\0", "m", "s", "p"), "id"),
            (with_agent("a", "", "s", "p"), "model"),
            (with_agent("a", "m", "", "p"), "session"),
            (
                Actor::Human {
                    id: "a\u{7f}".into(),
                },
                "id",
            ),
            (
                Actor::Human {
                    id: "a\u{85}".into(),
                },
                "id",
            ),
            (
                Actor::Human {
                    id: "a\u{a0}b".into(),
                },
                "id",
            ),
            (
                Actor::Human {
                    id: "a\u{2028}b".into(),
                },
                "id",
            ),
            (
                Actor::Human {
                    id: "a\u{2003}b".into(),
                },
                "id",
            ),
            (
                Actor::Human {
                    id: "dan\u{200b}iel".into(),
                },
                "id",
            ),
            (
                Actor::Human {
                    id: "dani\u{202e}el".into(),
                },
                "id",
            ),
            (
                Actor::Human {
                    id: "d\u{e4}niel".into(),
                },
                "id",
            ),
            // Cyrillic small a looks like the Latin one.
            (
                Actor::Human {
                    id: "d\u{430}niel".into(),
                },
                "id",
            ),
            (with_agent("a", "m", "s", "p\u{1f600}"), "sponsor"),
            // With several invalid fields, the first in field order is reported.
            (with_agent("a b", "m c", "", "p\n"), "id"),
            (with_agent("a", "m c", "", "p\n"), "model"),
        ];
        for (author, field) in cases {
            assert_eq!(
                Commit::new(empty_tree_id(), vec![], author.clone(), TIME, "m"),
                Err(CommitError::InvalidActorField(field)),
                "author {author:?}"
            );
        }
    }

    #[test]
    fn actor_fields_accept_the_whole_visible_ascii_range() {
        let all: String = ('\u{21}'..='\u{7e}').collect();
        let author = Actor::Agent {
            id: all.clone(),
            model: "!".into(),
            session: "~".into(),
            sponsor: all,
        };
        assert!(Commit::new(empty_tree_id(), vec![], author, TIME, "m").is_ok());
    }

    #[test]
    fn commit_exposes_its_fields() {
        let tree = empty_tree_id();
        let c = Commit::new(tree, vec![id(HELLO_ID)], agent(), TIME, "msg").expect("valid commit");
        assert_eq!(c.tree(), tree);
        assert_eq!(c.parents(), [id(HELLO_ID)]);
        assert_eq!(c.author(), &agent());
        assert_eq!(c.message(), "msg");
        assert_eq!(c.timestamp(), TIME);
    }

    #[test]
    fn mode_codes_are_pinned_and_roundtrip() {
        for (mode, code) in [
            (EntryMode::File, "100644"),
            (EntryMode::Executable, "100755"),
            (EntryMode::Symlink, "120000"),
            (EntryMode::Directory, "040000"),
        ] {
            assert_eq!(mode.code(), code);
            assert_eq!(EntryMode::from_code(code), Some(mode));
        }
        for unknown in [
            "", "100666", "0100644", "40000", "100644 ", " 100644", "file",
        ] {
            assert_eq!(EntryMode::from_code(unknown), None, "{unknown:?}");
        }
    }

    #[test]
    fn the_default_ignorable_table_is_sorted_disjoint_and_not_adjacent() {
        for pair in DEFAULT_IGNORABLE.windows(2) {
            let (_, last) = pair[0];
            let (first, _) = pair[1];
            assert!(
                (last as u32) + 1 < first as u32,
                "ranges must be ascending, disjoint, and merged: {pair:?}"
            );
        }
        for &(first, last) in DEFAULT_IGNORABLE {
            assert!(first <= last, "{first:?}..{last:?}");
        }
    }

    #[test]
    fn every_range_of_the_table_is_rejected_at_both_ends_and_its_neighbours_are_not_listed() {
        for &(first, last) in DEFAULT_IGNORABLE {
            assert!(is_invisible_character(first), "{first:?}");
            assert!(is_invisible_character(last), "{last:?}");
            // The code point just outside each range is not in the table.
            for outside in [(first as u32).checked_sub(1), Some(last as u32 + 1)]
                .into_iter()
                .flatten()
                .filter_map(char::from_u32)
            {
                let in_table = DEFAULT_IGNORABLE
                    .iter()
                    .any(|&(a, b)| a <= outside && outside <= b);
                assert!(!in_table, "{outside:?} next to {first:?}..{last:?}");
            }
        }
    }

    #[test]
    fn every_format_character_is_rejected() {
        // Cf is part of the rule, not only the default-ignorable table.
        let mut count = 0;
        for c in ('\0'..=char::MAX).filter(|&c| get_general_category(c) == GeneralCategory::Format)
        {
            assert!(is_invisible_character(c), "{c:?}");
            count += 1;
        }
        assert!(
            count > 100,
            "Cf should hold more than 100 code points, found {count}"
        );
    }

    #[test]
    fn the_rule_is_the_union_of_cf_and_default_ignorable() {
        let mut only_cf = 0;
        let mut only_default_ignorable = 0;
        for c in '\0'..=char::MAX {
            let cf = get_general_category(c) == GeneralCategory::Format;
            let default_ignorable = DEFAULT_IGNORABLE
                .iter()
                .any(|&(first, last)| first <= c && c <= last);
            assert_eq!(is_invisible_character(c), cf || default_ignorable, "{c:?}");
            only_cf += usize::from(cf && !default_ignorable);
            only_default_ignorable += usize::from(default_ignorable && !cf);
        }
        // Neither set contains the other: the visible Arabic number sign U+0600 is only in Cf,
        // and the variation selectors are only default-ignorable.
        assert!(only_cf > 0 && only_default_ignorable > 0);
        assert!(is_invisible_character('\u{600}') && !is_default_ignorable('\u{600}'));
        assert!(is_invisible_character('\u{fe0f}') && !is_format('\u{fe0f}'));
    }

    fn is_default_ignorable(c: char) -> bool {
        DEFAULT_IGNORABLE
            .iter()
            .any(|&(first, last)| first <= c && c <= last)
    }

    fn is_format(c: char) -> bool {
        get_general_category(c) == GeneralCategory::Format
    }

    #[test]
    fn index_paths_and_tree_names_share_the_invisible_character_rule() {
        // Keycap emoji start with an ASCII digit and use U+FE0F; they are rejected for now.
        assert_eq!(
            name_problem("1\u{fe0f}\u{20e3}"),
            Some(NameProblem::InvisibleCharacter)
        );
        // The emoji itself and the keycap sign alone are fine.
        assert_eq!(name_problem("\u{20e3}"), None);
        assert_eq!(name_problem("\u{2764}"), None);
    }
}
