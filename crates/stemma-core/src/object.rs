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
//! old and new objects can exist side by side. The readers [`Tree::parse`] and [`Commit::parse`]
//! reject what they do not recognize (ADR-005), so that an object has one encoding and one ID.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use unicode_general_category::{get_general_category, GeneralCategory};

use crate::{Actor, ActorParseError};

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
/// only by [`BlobId`], [`TreeId`], and [`CommitId`] (from [`Blob::id`], [`Tree::id`],
/// [`Commit::id`]), or by parsing a string. It does not record the kind of object it names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
    /// Returns the algorithm prefix without the colon, for example `b3`.
    pub(crate) fn algorithm(&self) -> &'static str {
        PREFIX.trim_end_matches(':')
    }

    /// Returns the digest as 64 lowercase hexadecimal digits, without the prefix.
    pub(crate) fn hex(&self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }
}

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

/// Defines the ID of one kind of object: a wrapper around [`ObjectId`] that the type system keeps
/// apart from the IDs of the other kinds. The kind is not part of the ID bytes (it is hashed into
/// them, but not recorded), so these types only help while the program is written; an untyped
/// [`ObjectId`] that is read from text or from storage becomes a typed one only where the kind
/// is known.
macro_rules! typed_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(ObjectId);

        impl $name {
            /// Claims that the ID names an object of this kind. The caller has to know the kind.
            pub(crate) fn new(id: ObjectId) -> Self {
                Self(id)
            }

            /// Returns the untyped ID, for a lookup in a store.
            pub fn object_id(self) -> ObjectId {
                self.0
            }
        }

        impl From<$name> for ObjectId {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
    };
}

typed_id! {
    /// The ID of a [`Blob`].
    BlobId
}

typed_id! {
    /// The ID of a [`Tree`].
    TreeId
}

typed_id! {
    /// The ID of a [`Commit`].
    CommitId
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

/// The kinds of stored objects. An incompatible change to an encoding adds a kind with a new name
/// (ADR-005), so the enum is not exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Kind {
    /// File content ([`Blob`]).
    Blob,
    /// A directory ([`Tree`]).
    Tree,
    /// A snapshot with history ([`Commit`]).
    Commit,
}

impl Kind {
    /// Returns the kind name. These names are hashed into the object ID and written at the start
    /// of every stored object file. Changing them invalidates all existing IDs and files.
    pub fn name(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
        }
    }

    /// Inverse of [`name`](Self::name). Accepts exactly the canonical names.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "blob" => Some(Self::Blob),
            "tree" => Some(Self::Tree),
            "commit" => Some(Self::Commit),
            _ => None,
        }
    }

    /// Returns the ASCII header that precedes the body in the hashed input and in a stored
    /// object: `<kind> <body length in decimal>` and a NUL byte.
    pub(crate) fn header(self, body_len: usize) -> String {
        format!("{} {}\0", self.name(), body_len)
    }
}

/// Hashes an encoded object. The hashed input is the ASCII header `<kind> <body length in
/// decimal>`, a NUL byte, and the body. Because the kind is part of the hashed header,
/// identical bodies of different kinds get different IDs.
pub(crate) fn hash_encoding(kind: Kind, body: &[u8]) -> ObjectId {
    let header = kind.header(body.len());
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
    pub fn id(&self) -> BlobId {
        BlobId(hash_encoding(Kind::Blob, &self.content))
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
    /// zero-width space, a bidirectional override, and a Hangul filler. A variation selector is
    /// allowed only directly after a non-ASCII character that is not a selector itself, so it is
    /// rejected at the start, after an ASCII character, and after another selector.
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
/// The entry ID of a [`Directory`](Self::Directory) refers to a tree; the ID of every other mode
/// refers to a blob. [`TreeEntry::file`] and [`TreeEntry::directory`] take the matching typed ID,
/// so an entry cannot be built with the wrong kind. An entry that is read back from storage has
/// to be checked against the stored object kind.
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

/// File mode of a file entry, in a tree or in the index. A directory is not a file, so there is
/// no directory mode.
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
    /// Creates an entry for a file or a symbolic link. The ID is the ID of the blob that holds the
    /// content, or for a symbolic link the link target.
    ///
    /// # Errors
    ///
    /// Returns [`TreeError::InvalidName`] if the name is empty, consists only of dots and
    /// spaces, contains `/` or a control or invisible character, or is a reserved name.
    pub fn file(name: impl Into<String>, mode: FileMode, blob: BlobId) -> Result<Self, TreeError> {
        Self::checked(name.into(), mode.into(), blob.object_id())
    }

    /// Creates an entry for a subdirectory. The ID is the ID of the tree of the directory.
    ///
    /// # Errors
    ///
    /// Returns [`TreeError::InvalidName`] under the same conditions as [`file`](Self::file).
    pub fn directory(name: impl Into<String>, tree: TreeId) -> Result<Self, TreeError> {
        Self::checked(name.into(), EntryMode::Directory, tree.object_id())
    }

    fn checked(name: String, mode: EntryMode, id: ObjectId) -> Result<Self, TreeError> {
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

    /// Returns the ID of the referenced object: a blob unless the mode is
    /// [`EntryMode::Directory`], in which case it is a tree.
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

/// Whether the character has the Unicode property `Variation_Selector`: the Mongolian free
/// variation selectors, the variation selectors U+FE00 to U+FE0F, and the supplementary ones.
/// All are default-ignorable. The Mongolian vowel separator U+180E is not one of them.
fn is_variation_selector(c: char) -> bool {
    matches!(c, '\u{180B}'..='\u{180D}' | '\u{180F}' | '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}')
}

/// Whether the name contains an invisible character that the rules do not allow in that place.
/// A variation selector is allowed directly after a non-ASCII character that is not itself a
/// variation selector, as in `\u{26A0}\u{FE0F}`. After an ASCII character, at the start, or after
/// another selector it stays rejected, so `READ\u{FE0F}ME` does not look like `README` (ADR-005).
fn has_invisible_character(name: &str) -> bool {
    let mut previous: Option<char> = None;
    name.chars().any(|c| {
        let allowed = is_variation_selector(c)
            && previous.is_some_and(|p| !p.is_ascii() && !is_variation_selector(p));
        previous = Some(c);
        is_invisible_character(c) && !allowed
    })
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
    } else if has_invisible_character(name) {
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
    /// validated by [`TreeEntry::file`] and [`TreeEntry::directory`].
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
    pub fn id(&self) -> TreeId {
        TreeId(hash_encoding(Kind::Tree, &self.encode()))
    }
}

/// Why the body of a tree is rejected by [`Tree::parse`]. Line numbers count from 1; an entry takes
/// two lines, the mode and name first and the ID second.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TreeParseError {
    /// The body is not valid UTF-8. Names are UTF-8 and the rest is ASCII.
    NotUtf8 {
        /// Length of the longest valid prefix, in bytes.
        valid_up_to: usize,
    },
    /// A line does not have the form of the encoding, or the body does not end with a newline.
    Malformed {
        /// Number of the line.
        line: usize,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The mode is not one of the four canonical codes.
    UnknownMode {
        /// Number of the line.
        line: usize,
        /// The mode as written.
        code: String,
    },
    /// The object ID is not in canonical form.
    InvalidObjectId {
        /// Number of the line with the ID.
        line: usize,
        /// Why the ID is invalid.
        source: ObjectIdParseError,
    },
    /// The name is rejected by the naming rules.
    Entry {
        /// Number of the line with the mode and the name.
        line: usize,
        /// Why the entry was rejected.
        source: TreeError,
    },
    /// The name is not greater than the name before it. A duplicate is also reported this way.
    OutOfOrder {
        /// Number of the line with the mode and the name.
        line: usize,
        /// The name of the entry before.
        previous: String,
        /// The name of this entry.
        name: String,
    },
}

impl fmt::Display for TreeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8 { valid_up_to } => {
                write!(
                    f,
                    "the tree is not valid UTF-8 (valid up to byte {valid_up_to})"
                )
            }
            Self::Malformed { line, reason } => write!(f, "line {line}: {reason}"),
            Self::UnknownMode { line, code } => {
                write!(f, "line {line}: unknown mode `{}`", code.escape_debug())
            }
            Self::InvalidObjectId { line, source } => {
                write!(f, "line {line}: invalid object ID: {source}")
            }
            Self::Entry { line, source } => write!(f, "line {line}: {source}"),
            Self::OutOfOrder {
                line,
                previous,
                name,
            } => write!(
                f,
                "line {line}: `{}` does not sort after `{}`; names must be strictly ascending",
                name.escape_debug(),
                previous.escape_debug()
            ),
        }
    }
}

impl Error for TreeParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidObjectId { source, .. } => Some(source),
            Self::Entry { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Tree {
    /// Reads the body of a tree, as [`encode`](Self::encode) writes it. The reader is strict, by
    /// the rule of ADR-005: it rejects everything that `encode` would not have written, so a tree
    /// has exactly one encoding and one ID, and `parse(bytes).encode() == bytes` for every
    /// accepted input.
    ///
    /// The kind of each ID is claimed by the mode: a blob for a file or a symbolic link and a tree
    /// for a directory. Nothing has asked a store; follow an ID with
    /// [`ObjectStore::resolve_blob`](crate::store::ObjectStore::resolve_blob) or
    /// [`resolve_tree`](crate::store::ObjectStore::resolve_tree) before relying on it.
    ///
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a body that does not end with a newline, a
    /// line without the shape `<mode> <name>` or an entry without its ID line, an unknown mode, an
    /// object ID that is not in canonical form, a name that the naming rules reject, and names that
    /// are not in strictly ascending order (which also rules out duplicates). A line with several
    /// problems reports the first in this order: shape, mode, ID, name rules, then the order.
    pub fn parse(bytes: &[u8]) -> Result<Self, TreeParseError> {
        let text = std::str::from_utf8(bytes).map_err(|error| TreeParseError::NotUtf8 {
            valid_up_to: error.valid_up_to(),
        })?;
        if !text.is_empty() && !text.ends_with('\n') {
            return Err(TreeParseError::Malformed {
                line: text.split('\n').count(),
                reason: "the last line has no terminating newline",
            });
        }

        let mut entries: Vec<TreeEntry> = Vec::new();
        let mut lines = text.split_terminator('\n').enumerate();
        while let Some((index, head)) = lines.next() {
            let line = index + 1;
            let malformed = |reason| TreeParseError::Malformed { line, reason };
            let (code, name) = head
                .split_once(' ')
                .ok_or_else(|| malformed("expected `<mode> <name>`"))?;
            let mode = EntryMode::from_code(code).ok_or_else(|| TreeParseError::UnknownMode {
                line,
                code: code.to_owned(),
            })?;
            let (_, id_text) = lines
                .next()
                .ok_or_else(|| malformed("the entry has no line with its ID"))?;
            let id =
                ObjectId::from_str(id_text).map_err(|source| TreeParseError::InvalidObjectId {
                    line: line + 1,
                    source,
                })?;
            let entry = match FileMode::from_entry_mode(mode) {
                Some(file_mode) => TreeEntry::file(name, file_mode, BlobId::new(id)),
                None => TreeEntry::directory(name, TreeId::new(id)),
            }
            .map_err(|source| TreeParseError::Entry { line, source })?;
            if let Some(previous) = entries
                .last()
                .filter(|previous| previous.name >= entry.name)
            {
                return Err(TreeParseError::OutOfOrder {
                    line,
                    previous: previous.name.clone(),
                    name: entry.name,
                });
            }
            entries.push(entry);
        }
        Ok(Self { entries })
    }
}

/// Reasons why a commit is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommitError {
    /// The same parent is listed twice.
    DuplicateParent(CommitId),
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
    tree: TreeId,
    parents: Vec<CommitId>,
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
    /// Returns [`CommitError::DuplicateParent`] if a parent is listed twice. The author needs no
    /// check here, because an [`Actor`] is valid by construction. The message is not validated: it
    /// comes last in the encoding and cannot interfere with the headers.
    pub fn new(
        tree: TreeId,
        parents: Vec<CommitId>,
        author: Actor,
        timestamp: i64,
        message: impl Into<String>,
    ) -> Result<Self, CommitError> {
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
    pub fn tree(&self) -> TreeId {
        self.tree
    }

    /// Returns the parents in the order given at creation.
    pub fn parents(&self) -> &[CommitId] {
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
    pub fn id(&self) -> CommitId {
        CommitId(hash_encoding(Kind::Commit, &self.encode()))
    }
}

/// Why the body of a commit is rejected by [`Commit::parse`]. Line numbers count from 1.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommitParseError {
    /// The body is not valid UTF-8. The message is a string, so the whole body has to be.
    NotUtf8 {
        /// Length of the longest valid prefix, in bytes.
        valid_up_to: usize,
    },
    /// A header is missing, repeated, unknown, or out of order, the time is not a canonical
    /// decimal integer, or the empty line after the headers is missing.
    Malformed {
        /// Number of the line.
        line: usize,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// An object ID is not in canonical form.
    InvalidObjectId {
        /// Number of the line.
        line: usize,
        /// Why the ID is invalid.
        source: ObjectIdParseError,
    },
    /// The author line is rejected.
    InvalidAuthor {
        /// Number of the line.
        line: usize,
        /// Why the author is invalid.
        source: ActorParseError,
    },
    /// The headers are well formed, but the commit is rejected.
    Commit(CommitError),
}

impl fmt::Display for CommitParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8 { valid_up_to } => {
                write!(
                    f,
                    "the commit is not valid UTF-8 (valid up to byte {valid_up_to})"
                )
            }
            Self::Malformed { line, reason } => write!(f, "line {line}: {reason}"),
            Self::InvalidObjectId { line, source } => {
                write!(f, "line {line}: invalid object ID: {source}")
            }
            Self::InvalidAuthor { line, source } => write!(f, "line {line}: {source}"),
            Self::Commit(source) => write!(f, "{source}"),
        }
    }
}

impl Error for CommitParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidObjectId { source, .. } => Some(source),
            Self::InvalidAuthor { source, .. } => Some(source),
            Self::Commit(source) => Some(source),
            _ => None,
        }
    }
}

/// Removes the first line, without its newline, from the front of the text and returns it, or
/// returns `None` if the text has no newline.
fn take_line<'a>(rest: &mut &'a str) -> Option<&'a str> {
    let (line, tail) = rest.split_once('\n')?;
    *rest = tail;
    Some(line)
}

fn header_id(text: &str, line: usize) -> Result<ObjectId, CommitParseError> {
    ObjectId::from_str(text).map_err(|source| CommitParseError::InvalidObjectId { line, source })
}

/// Reads the seconds of the `time` header. Only the canonical form is accepted: a decimal integer
/// that fits `i64`, with a minus sign only for a negative value, no plus sign, no leading zeros,
/// and no `-0`.
fn parse_seconds(text: &str) -> Option<i64> {
    let negative = text.starts_with('-');
    let digits = if negative { &text[1..] } else { text };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if digits.starts_with('0') && (digits.len() > 1 || negative) {
        return None;
    }
    text.parse().ok()
}

impl Commit {
    /// Reads the body of a commit, as [`encode`](Self::encode) writes it. The reader is strict, by
    /// the rule of ADR-005: it rejects everything that `encode` would not have written, so a commit
    /// has exactly one encoding and one ID, and `parse(bytes).encode() == bytes` for every
    /// accepted input. The headers are `tree`, `parent` (zero or more), `author`, and `time`, in
    /// this order and each once, then an empty line, then the message, which is kept verbatim.
    ///
    /// The kind of each ID is claimed by its header: a tree for `tree` and a commit for `parent`.
    /// Nothing has asked a store; follow an ID with
    /// [`ObjectStore::resolve_tree`](crate::store::ObjectStore::resolve_tree) or
    /// [`resolve_commit`](crate::store::ObjectStore::resolve_commit) before relying on it.
    ///
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a header that is missing, repeated, unknown,
    /// or out of order, an object ID that is not in canonical form, an author line that
    /// [`Actor::from_str`] rejects, a time that is not canonical or does not fit `i64`, a missing
    /// empty line after the headers, and a parent that is listed twice. The first problem in the
    /// order of the lines is reported.
    pub fn parse(bytes: &[u8]) -> Result<Self, CommitParseError> {
        let text = std::str::from_utf8(bytes).map_err(|error| CommitParseError::NotUtf8 {
            valid_up_to: error.valid_up_to(),
        })?;
        let mut rest = text;
        let mut line = 1;
        let malformed = |line, reason| CommitParseError::Malformed { line, reason };

        let tree = take_line(&mut rest)
            .and_then(|header| header.strip_prefix("tree "))
            .ok_or_else(|| malformed(line, "expected the header `tree <id>` first"))?;
        let tree = TreeId::new(header_id(tree, line)?);

        let mut parents = Vec::new();
        let author = loop {
            line += 1;
            let header = take_line(&mut rest)
                .ok_or_else(|| malformed(line, "the headers end before `author <actor>`"))?;
            match header.strip_prefix("parent ") {
                Some(id) => parents.push(CommitId::new(header_id(id, line)?)),
                None => break header,
            }
        };
        let author = author
            .strip_prefix("author ")
            .ok_or_else(|| malformed(line, "expected the header `author <actor>`"))?;
        let author = Actor::from_str(author)
            .map_err(|source| CommitParseError::InvalidAuthor { line, source })?;

        line += 1;
        let seconds = take_line(&mut rest)
            .and_then(|header| header.strip_prefix("time "))
            .ok_or_else(|| malformed(line, "expected the header `time <seconds>`"))?;
        let timestamp = parse_seconds(seconds).ok_or_else(|| {
            malformed(
                line,
                "the time is not a canonical decimal integer of seconds",
            )
        })?;

        line += 1;
        if take_line(&mut rest) != Some("") {
            return Err(malformed(line, "expected an empty line after the headers"));
        }
        Commit::new(tree, parents, author, timestamp, rest).map_err(CommitParseError::Commit)
    }
}

/// Encodes an actor on one line: `human <id>` or
/// `agent <id> model=<model> session=<session> sponsor=<sponsor>`.
///
/// The fields are not escaped. [`Actor`] guarantees that no field is empty or contains a space, so
/// the line splits unambiguously at spaces. A value may still contain `=`, so a parser must split
/// each `key=value` token at its first `=`.
fn actor_line(actor: &Actor) -> String {
    let (label, fields) = actor.fields();
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
        Actor::human("daniel").expect("valid actor")
    }

    fn agent() -> Actor {
        Actor::agent("agent-1", "claude-haiku-5-5", "s1", "daniel").expect("valid actor")
    }

    fn id(text: &str) -> ObjectId {
        text.parse().expect("test ID must be valid")
    }

    fn tree_id_of(text: &str) -> TreeId {
        TreeId::new(id(text))
    }

    fn commit_id(text: &str) -> CommitId {
        CommitId::new(id(text))
    }

    /// Builds an entry from a mode and an untyped ID, which the tests write as literals.
    fn tree_entry(name: &str, mode: EntryMode, target: ObjectId) -> Result<TreeEntry, TreeError> {
        match FileMode::from_entry_mode(mode) {
            Some(file_mode) => TreeEntry::file(name, file_mode, BlobId::new(target)),
            None => TreeEntry::directory(name, TreeId::new(target)),
        }
    }

    fn entry(name: &str, mode: EntryMode, target: ObjectId) -> TreeEntry {
        tree_entry(name, mode, target).expect("test entry must be valid")
    }

    fn sample_tree() -> Tree {
        Tree::new(vec![
            entry("b.sh", EntryMode::Executable, id(EMPTY_ID)),
            entry("a.txt", EntryMode::File, id(HELLO_ID)),
        ])
        .expect("valid tree")
    }

    fn empty_tree_id() -> TreeId {
        Tree::new(vec![]).expect("empty tree is valid").id()
    }

    fn commit(parents: Vec<CommitId>, author: Actor, message: &str) -> Commit {
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
        assert_ne!(
            Blob::new(tree.encode()).id().object_id(),
            tree.id().object_id()
        );
        let commit = commit(vec![], human(), "m");
        assert_ne!(
            Blob::new(commit.encode()).id().object_id(),
            commit.id().object_id()
        );
        assert_ne!(
            hash_encoding(Kind::Tree, &commit.encode()),
            hash_encoding(Kind::Commit, &commit.encode())
        );
    }

    #[test]
    fn object_id_roundtrips_through_text() {
        let original = Blob::new("data").id().object_id();
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
            ("a\u{e0100}b", NameProblem::InvisibleCharacter),
            ("a\u{180b}", NameProblem::InvisibleCharacter),
            // A variation selector at the start, after another selector, or after a joiner.
            ("\u{fe0f}\u{2764}", NameProblem::InvisibleCharacter),
            ("\u{2764}\u{fe0f}\u{fe0f}", NameProblem::InvisibleCharacter),
            ("\u{2764}\u{fe0f}\u{e0100}", NameProblem::InvisibleCharacter),
            ("\u{2764}\u{200d}\u{fe0f}", NameProblem::InvisibleCharacter),
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
                tree_entry(name, EntryMode::File, id(HELLO_ID)),
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
            let built = tree_entry(name, EntryMode::File, id(HELLO_ID));
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
            // A variation selector directly after a non-ASCII character.
            "\u{26a0}\u{fe0f} TODO.md",
            "\u{2764}\u{fe0f}",
            "\u{2764}\u{fe0f}x",
            "\u{e9}\u{fe0f}",
            "\u{8fba}\u{e0100}",
            "\u{1820}\u{180b}",
        ] {
            let built = tree_entry(name, EntryMode::File, id(HELLO_ID));
            assert!(built.is_ok(), "name {name:?} should be accepted");
        }
    }

    #[test]
    fn file_and_directory_entries_fix_the_mode_and_take_the_matching_id() {
        let blob = Blob::new("hello").id();
        let tree = sample_tree().id();
        let file = TreeEntry::file("run.sh", FileMode::Executable, blob).unwrap();
        assert_eq!(
            (file.mode(), file.id()),
            (EntryMode::Executable, blob.object_id())
        );
        let directory = TreeEntry::directory("src", tree).unwrap();
        assert_eq!(
            (directory.mode(), directory.id()),
            (EntryMode::Directory, tree.object_id())
        );
        // The same name rules apply to both constructors.
        assert!(matches!(
            TreeEntry::directory("a/b", tree),
            Err(TreeError::InvalidName { .. })
        ));
    }

    #[test]
    fn typed_ids_print_like_the_untyped_id_and_name_their_kind_in_debug() {
        let blob = Blob::new("hello").id();
        assert_eq!(blob.to_string(), HELLO_ID);
        assert_eq!(ObjectId::from(blob).to_string(), HELLO_ID);
        assert_eq!(format!("{blob:?}"), format!("BlobId({HELLO_ID})"));
        let tree = sample_tree().id();
        assert!(format!("{tree:?}").starts_with("TreeId(b3:"));
        let commit = commit(vec![], human(), "m").id();
        assert!(format!("{commit:?}").starts_with("CommitId(b3:"));
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
        let err = tree_entry("a\nb", EntryMode::File, id(HELLO_ID)).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("a\\nb") && !text.contains('\n'), "{text}");
    }

    // Commits

    #[test]
    fn human_commit_encoding_and_id_match_golden_vectors() {
        let commit = Commit::new(
            tree_id_of(TREE_ID),
            vec![commit_id(EMPTY_ID)],
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
        let commit =
            Commit::new(tree_id_of(TREE_ID), vec![], agent(), TIME, "work").expect("valid commit");
        let expected = format!(
            "tree {TREE_ID}\nauthor agent agent-1 model=claude-haiku-5-5 session=s1 sponsor=daniel\ntime 1700000000\n\nwork"
        );
        assert_eq!(commit.encode(), expected.as_bytes());
        assert_eq!(commit.id().to_string(), AGENT_COMMIT_ID);
    }

    #[test]
    fn merge_commit_encoding_and_id_match_golden_vectors() {
        let commit = Commit::new(
            tree_id_of(TREE_ID),
            vec![commit_id(HELLO_ID), commit_id(EMPTY_ID)],
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
        let c = commit(vec![commit_id(HELLO_ID)], human(), "m");
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
        let author = Actor::agent("a", "m=1", "s", "p").expect("valid actor");
        let c = commit(vec![], author, "m");
        let text = String::from_utf8(c.encode()).expect("encoding is UTF-8");
        assert!(text.contains("\nauthor agent a model=m=1 session=s sponsor=p\n"));
    }

    #[test]
    fn human_and_agent_with_the_same_id_differ() {
        let as_human = Actor::human("x").expect("valid actor");
        let as_agent = Actor::agent("x", "m", "s", "p").expect("valid actor");
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
        let (a, b) = (commit_id(HELLO_ID), commit_id(EMPTY_ID));
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
        let p = commit_id(HELLO_ID);
        let result = Commit::new(
            empty_tree_id(),
            vec![p, commit_id(EMPTY_ID), p],
            human(),
            TIME,
            "m",
        );
        assert_eq!(result, Err(CommitError::DuplicateParent(p)));
    }

    #[test]
    fn commit_exposes_its_fields() {
        let tree = empty_tree_id();
        let c = Commit::new(tree, vec![commit_id(HELLO_ID)], agent(), TIME, "msg")
            .expect("valid commit");
        assert_eq!(c.tree(), tree);
        assert_eq!(c.parents(), [commit_id(HELLO_ID)]);
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
        // An emoji with a presentation selector is accepted in both.
        assert_eq!(name_problem("\u{26a0}\u{fe0f}"), None);
        // The emoji itself and the keycap sign alone are fine.
        assert_eq!(name_problem("\u{20e3}"), None);
        assert_eq!(name_problem("\u{2764}"), None);
    }

    #[test]
    fn kind_names_roundtrip_and_only_the_canonical_ones_parse() {
        for (kind, name) in [
            (Kind::Blob, "blob"),
            (Kind::Tree, "tree"),
            (Kind::Commit, "commit"),
        ] {
            assert_eq!(kind.name(), name);
            assert_eq!(Kind::from_name(name), Some(kind));
        }
        for unknown in [
            "", "Blob", "BLOB", "blob ", " blob", "blob\0", "commit2", "tag",
        ] {
            assert_eq!(Kind::from_name(unknown), None, "{unknown:?}");
        }
    }

    #[test]
    fn the_header_is_the_kind_the_decimal_length_and_a_nul_byte() {
        assert_eq!(Kind::Blob.header(5), "blob 5\0");
        assert_eq!(Kind::Tree.header(0), "tree 0\0");
        assert_eq!(Kind::Commit.header(1234), "commit 1234\0");
    }

    #[test]
    fn the_id_parts_are_the_algorithm_and_lowercase_hex() {
        let stored = Blob::new("hello").id().object_id();
        assert_eq!(stored.algorithm(), "b3");
        assert_eq!(
            stored.hex(),
            "9db8039d1b509628ad5b125ea89780b69fcde5c272215d690a2e4dc69eac75ad"
        );
        assert_eq!(stored.to_string(), format!("b3:{}", stored.hex()));
    }

    // Readers

    /// Feeds every single-byte change of a canonical encoding (a byte replaced, inserted, or
    /// deleted) to a reader. `reencode` returns the encoding of what the reader accepted, or `None`
    /// if it rejected the input. Whatever is accepted has to encode back to exactly the bytes it
    /// was given: a reader that accepts a second spelling of an object would give one object two
    /// IDs. Returns how many changed inputs were accepted, so that a test can see it was not
    /// vacuous.
    fn accepted_changes_encode_back(
        golden: &[u8],
        reencode: impl Fn(&[u8]) -> Option<Vec<u8>>,
    ) -> usize {
        assert_eq!(
            reencode(golden).as_deref(),
            Some(golden),
            "the canonical encoding itself must be accepted unchanged"
        );
        const BYTES: &[u8] = b" \n\r\t\0+-=09a:\xff";
        let mut accepted = 0;
        let mut check = |changed: Vec<u8>| {
            if let Some(again) = reencode(&changed) {
                assert_eq!(
                    again,
                    changed,
                    "accepted a second spelling: {:?}",
                    String::from_utf8_lossy(&changed)
                );
                accepted += 1;
            }
        };
        for position in 0..golden.len() {
            let mut deleted = golden.to_vec();
            deleted.remove(position);
            check(deleted);
            for &byte in BYTES {
                let mut replaced = golden.to_vec();
                replaced[position] = byte;
                check(replaced);
                let mut inserted = golden.to_vec();
                inserted.insert(position, byte);
                check(inserted);
            }
        }
        accepted
    }

    fn all_modes_tree() -> Tree {
        Tree::new(vec![
            entry("a-dir", EntryMode::Directory, id(TREE_ID)),
            entry("b file", EntryMode::File, id(HELLO_ID)),
            entry("c-link", EntryMode::Symlink, id(HELLO_ID)),
            entry("d-run.sh", EntryMode::Executable, id(EMPTY_ID)),
            entry("\u{e9}t\u{e9}", EntryMode::File, id(HELLO_ID)),
        ])
        .expect("valid tree")
    }

    fn tree_text(entries: &str) -> Vec<u8> {
        entries.as_bytes().to_vec()
    }

    #[test]
    fn a_tree_is_read_back_from_its_encoding() {
        for tree in [Tree::new(vec![]).unwrap(), sample_tree(), all_modes_tree()] {
            assert_eq!(Tree::parse(&tree.encode()), Ok(tree));
        }
    }

    #[test]
    fn a_tree_reader_claims_the_kind_of_an_id_by_the_mode() {
        let tree = Tree::parse(&all_modes_tree().encode()).unwrap();
        let by_name = |name: &str| {
            tree.entries()
                .iter()
                .find(|entry| entry.name() == name)
                .map(|entry| (entry.mode(), entry.id()))
                .unwrap()
        };
        assert_eq!(by_name("a-dir"), (EntryMode::Directory, id(TREE_ID)));
        assert_eq!(by_name("b file"), (EntryMode::File, id(HELLO_ID)));
        assert_eq!(by_name("c-link"), (EntryMode::Symlink, id(HELLO_ID)));
        assert_eq!(by_name("d-run.sh"), (EntryMode::Executable, id(EMPTY_ID)));
    }

    #[test]
    fn a_tree_reader_rejects_every_non_canonical_form() {
        let ok = format!("100644 a\n{HELLO_ID}\n");
        assert!(Tree::parse(ok.as_bytes()).is_ok());
        let line_of = |bytes: Vec<u8>| match Tree::parse(&bytes).unwrap_err() {
            TreeParseError::Malformed { line, .. }
            | TreeParseError::UnknownMode { line, .. }
            | TreeParseError::InvalidObjectId { line, .. }
            | TreeParseError::Entry { line, .. }
            | TreeParseError::OutOfOrder { line, .. } => line,
            other => panic!("unexpected error: {other}"),
        };

        // Not UTF-8: the longest valid prefix is `100644 a`.
        assert_eq!(
            Tree::parse(b"100644 a\xff\n"),
            Err(TreeParseError::NotUtf8 { valid_up_to: 8 })
        );
        // No terminating newline, a missing space, a missing ID line, a blank line.
        assert_eq!(line_of(tree_text(&format!("100644 a\n{HELLO_ID}"))), 2);
        assert_eq!(line_of(tree_text(&format!("100644a\n{HELLO_ID}\n"))), 1);
        assert_eq!(line_of(tree_text("100644 a\n")), 1);
        assert_eq!(line_of(tree_text(&format!("\n{HELLO_ID}\n"))), 1);
        assert_eq!(line_of(tree_text(&format!("100644 a\n{HELLO_ID}\n\n"))), 3);
        // Modes: only the four canonical codes.
        for code in ["100664", "40000", "0100644", "", "120001"] {
            let error = Tree::parse(format!("{code} a\n{HELLO_ID}\n").as_bytes()).unwrap_err();
            assert!(
                matches!(error, TreeParseError::UnknownMode { line: 1, .. }),
                "mode {code:?}: {error}"
            );
        }
        // A name may start with a space, so a second space after the mode is part of the name.
        let spaced = format!("100644  a\n{HELLO_ID}\n");
        assert_eq!(
            Tree::parse(spaced.as_bytes()).unwrap().entries()[0].name(),
            " a"
        );
        // IDs: uppercase, wrong length, no prefix, a carriage return, trailing space.
        let upper = HELLO_ID.to_uppercase();
        for bad in [
            upper.as_str(),
            "b3:abcd",
            &HELLO_ID[3..],
            &format!("{HELLO_ID}\r"),
            &format!("{HELLO_ID} "),
        ] {
            let error = Tree::parse(format!("100644 a\n{bad}\n").as_bytes()).unwrap_err();
            assert!(
                matches!(error, TreeParseError::InvalidObjectId { line: 2, .. }),
                "id {bad:?}: {error}"
            );
        }
        // Names: the naming rules apply, and the form is checked before them.
        for name in ["", ".", "..", "a/b", "a\rb", ".stemma", "a\u{200b}b"] {
            let error = Tree::parse(format!("100644 {name}\n{HELLO_ID}\n").as_bytes()).unwrap_err();
            assert!(
                matches!(error, TreeParseError::Entry { line: 1, .. }),
                "name {name:?}: {error}"
            );
        }
        // Order: descending and duplicate names; byte order, not case-folded.
        for names in [["b", "a"], ["a", "a"]] {
            let text = format!(
                "100644 {}\n{HELLO_ID}\n100644 {}\n{HELLO_ID}\n",
                names[0], names[1]
            );
            let error = Tree::parse(text.as_bytes()).unwrap_err();
            assert!(
                matches!(error, TreeParseError::OutOfOrder { line: 3, .. }),
                "{names:?}: {error}"
            );
        }
        let upper_first = format!("100644 B\n{HELLO_ID}\n100644 a\n{HELLO_ID}\n");
        assert!(Tree::parse(upper_first.as_bytes()).is_ok());
        // A directory has no special place in the order (ADR-005).
        let dir_text = format!("040000 a\n{TREE_ID}\n100644 a-b\n{HELLO_ID}\n");
        assert!(Tree::parse(dir_text.as_bytes()).is_ok());
    }

    #[test]
    fn a_tree_error_names_the_line_and_the_cause() {
        let error = Tree::parse(format!("100644 a/b\n{HELLO_ID}\n").as_bytes()).unwrap_err();
        assert!(error
            .to_string()
            .starts_with("line 1: invalid tree entry name `a/b`"));
        assert!(error.source().is_some());
        let error = Tree::parse(format!("100644 b\n{HELLO_ID}\n100644 a\n{HELLO_ID}\n").as_bytes())
            .unwrap_err();
        assert!(error.to_string().contains("strictly ascending"), "{error}");
        assert!(error.source().is_none());
    }

    #[test]
    fn a_tree_reader_accepts_no_second_spelling() {
        for tree in [sample_tree(), all_modes_tree()] {
            let accepted = accepted_changes_encode_back(&tree.encode(), |bytes| {
                Tree::parse(bytes).ok().map(|tree| tree.encode())
            });
            assert!(
                accepted > 0,
                "the check accepted nothing and proves nothing"
            );
        }
    }

    fn commit_bytes(tree: &str, parents: &[&str], author: &str, time: &str, rest: &str) -> Vec<u8> {
        let mut text = format!("tree {tree}\n");
        for parent in parents {
            text.push_str(&format!("parent {parent}\n"));
        }
        text.push_str(&format!("author {author}\ntime {time}\n{rest}"));
        text.into_bytes()
    }

    fn all_commits() -> Vec<Commit> {
        let message = |text: &str, parents: Vec<CommitId>, author: Actor, time: i64| {
            Commit::new(tree_id_of(TREE_ID), parents, author, time, text).expect("valid commit")
        };
        vec![
            message("", vec![], human(), 0),
            message("first change", vec![commit_id(EMPTY_ID)], human(), TIME),
            message("work", vec![], agent(), TIME),
            message(
                "merge",
                vec![commit_id(HELLO_ID), commit_id(EMPTY_ID)],
                human(),
                -1,
            ),
            message("line\n\nbody\n", vec![], human(), i64::MAX),
            message("\nstarts with a newline", vec![], human(), i64::MIN),
            message("trailing blank\n\n", vec![], agent(), 7),
            message("tree x\nparent y\n\nauthor z", vec![], human(), TIME),
        ]
    }

    #[test]
    fn a_commit_is_read_back_from_its_encoding() {
        for commit in all_commits() {
            assert_eq!(Commit::parse(&commit.encode()), Ok(commit));
        }
    }

    #[test]
    fn a_commit_reader_keeps_the_message_verbatim_after_the_first_empty_line() {
        let text = commit_bytes(TREE_ID, &[], "human daniel", "1", "\nfirst\n\nsecond");
        assert_eq!(Commit::parse(&text).unwrap().message(), "first\n\nsecond");
        let text = commit_bytes(TREE_ID, &[], "human daniel", "1", "\n");
        assert_eq!(Commit::parse(&text).unwrap().message(), "");
    }

    #[test]
    fn a_commit_reader_rejects_every_non_canonical_form() {
        let tree = TREE_ID;
        let parent = HELLO_ID;
        let human = "human daniel";
        let error = |bytes: Vec<u8>| Commit::parse(&bytes).unwrap_err();
        let malformed_at = |bytes: Vec<u8>, expected: usize| match error(bytes) {
            CommitParseError::Malformed { line, .. } => assert_eq!(line, expected),
            other => panic!("expected a malformed header at line {expected}: {other}"),
        };

        assert!(Commit::parse(&commit_bytes(tree, &[], human, "1", "\nm")).is_ok());
        // Not UTF-8, also in the message.
        assert!(matches!(
            Commit::parse(b"tree x\n\xff"),
            Err(CommitParseError::NotUtf8 { valid_up_to: 7 })
        ));
        let mut invalid_message = commit_bytes(tree, &[], human, "1", "\n");
        invalid_message.push(0xff);
        assert!(matches!(
            Commit::parse(&invalid_message),
            Err(CommitParseError::NotUtf8 { .. })
        ));
        // Headers: missing, repeated, out of order, unknown, with another spacing.
        malformed_at(b"".to_vec(), 1);
        malformed_at(format!("parent {parent}\ntree {tree}\n").into_bytes(), 1);
        malformed_at(format!("tree {tree}\ntree {tree}\n").into_bytes(), 2);
        malformed_at(format!("tree {tree}\ntime 1\n\n").into_bytes(), 2);
        malformed_at(format!("tree {tree}\ncommitter {human}\n").into_bytes(), 2);
        malformed_at(
            format!("tree {tree}\nauthor {human}\nparent {parent}\n").into_bytes(),
            3,
        );
        malformed_at(
            format!("tree {tree}\nauthor {human}\nauthor {human}\n").into_bytes(),
            3,
        );
        // A second space after the header name is part of the ID, which is then not canonical.
        let spaced = format!("tree  {tree}\nauthor {human}\ntime 1\n\n").into_bytes();
        assert!(matches!(
            error(spaced),
            CommitParseError::InvalidObjectId { line: 1, .. }
        ));
        malformed_at(
            format!("tree {tree}\nauthor {human}\ntime 1").into_bytes(),
            3,
        );
        // The empty line after the headers.
        malformed_at(
            format!("tree {tree}\nauthor {human}\ntime 1\n").into_bytes(),
            4,
        );
        malformed_at(
            format!("tree {tree}\nauthor {human}\ntime 1\nmessage").into_bytes(),
            4,
        );
        // Time: only the canonical decimal form that fits i64.
        for time in [
            "",
            "+1",
            "-0",
            "01",
            "00",
            "1.5",
            "0x1",
            " 1",
            "1 ",
            "1e3",
            "-",
            "--1",
            "\u{661}",
            "9223372036854775808",
            "-9223372036854775809",
            "99999999999999999999",
        ] {
            malformed_at(commit_bytes(tree, &[], human, time, "\n"), 3);
        }
        for time in ["0", "-1", "9223372036854775807", "-9223372036854775808"] {
            assert!(Commit::parse(&commit_bytes(tree, &[], human, time, "\n")).is_ok());
        }
        // IDs, with the line of the header.
        let bad_tree = commit_bytes("b3:xyz", &[], human, "1", "\n");
        assert!(matches!(
            error(bad_tree),
            CommitParseError::InvalidObjectId { line: 1, .. }
        ));
        let bad_parent = commit_bytes(tree, &[parent, "b3:00"], human, "1", "\n");
        assert!(matches!(
            error(bad_parent),
            CommitParseError::InvalidObjectId { line: 3, .. }
        ));
        let carriage = commit_bytes(&format!("{tree}\r"), &[], human, "1", "\n");
        assert!(matches!(
            error(carriage),
            CommitParseError::InvalidObjectId { line: 1, .. }
        ));
        // The author line: the form and the fields.
        for author in [
            "",
            "human",
            "human ",
            "robot x",
            "human  x",
            "agent a model=m",
        ] {
            let bytes = commit_bytes(tree, &[], author, "1", "\n");
            assert!(
                matches!(
                    error(bytes),
                    CommitParseError::InvalidAuthor { line: 2, .. }
                ),
                "author {author:?}"
            );
        }
        // A duplicate parent.
        assert_eq!(
            error(commit_bytes(tree, &[parent, parent], human, "1", "\n")),
            CommitParseError::Commit(CommitError::DuplicateParent(commit_id(HELLO_ID)))
        );
    }

    #[test]
    fn a_commit_error_names_the_line_and_the_cause() {
        let error = Commit::parse(&commit_bytes(TREE_ID, &[], "human a b", "1", "\n")).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("line 2: malformed actor line"),
            "{error}"
        );
        assert!(error.source().is_some());
        let error = Commit::parse(&commit_bytes(TREE_ID, &[], "human d", "01", "\n")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "line 3: the time is not a canonical decimal integer of seconds"
        );
        assert!(error.source().is_none());
    }

    #[test]
    fn a_commit_reader_accepts_no_second_spelling() {
        for commit in all_commits().into_iter().filter(|c| c.message().len() < 12) {
            let accepted = accepted_changes_encode_back(&commit.encode(), |bytes| {
                Commit::parse(bytes).ok().map(|commit| commit.encode())
            });
            assert!(
                accepted > 0,
                "the check accepted nothing and proves nothing"
            );
        }
    }

    #[test]
    fn an_actor_line_is_read_back_and_accepts_no_second_spelling() {
        let with_equals = Actor::agent("a=b", "m=1", "s=", "p==").unwrap();
        for actor in [human(), agent(), with_equals] {
            let line = actor_line(&actor);
            assert_eq!(line.parse::<Actor>(), Ok(actor));
            let accepted = accepted_changes_encode_back(line.as_bytes(), |bytes| {
                let text = std::str::from_utf8(bytes).ok()?;
                text.parse::<Actor>()
                    .ok()
                    .map(|actor| actor_line(&actor).into_bytes())
            });
            assert!(
                accepted > 0,
                "the check accepted nothing and proves nothing"
            );
        }
    }

    #[test]
    fn every_tree_error_has_a_stable_message_and_the_right_source() {
        let bad_id = "100644 a\nb3:00\n";
        let cases: [(Vec<u8>, &str, bool); 6] = [
            (
                b"100644 a\xff\n".to_vec(),
                "the tree is not valid UTF-8 (valid up to byte 8)",
                false,
            ),
            (
                b"100644a\nx\n".to_vec(),
                "line 1: expected `<mode> <name>`",
                false,
            ),
            (
                format!("100664 a\n{HELLO_ID}\n").into_bytes(),
                "line 1: unknown mode `100664`",
                false,
            ),
            (
                bad_id.as_bytes().to_vec(),
                "line 2: invalid object ID: object ID digest must be 64 bytes long, found 2",
                true,
            ),
            (
                format!("100644 a/b\n{HELLO_ID}\n").into_bytes(),
                "line 1: invalid tree entry name `a/b`: the name contains `/`",
                true,
            ),
            (
                format!("100644 b\n{HELLO_ID}\n100644 a\n{HELLO_ID}\n").into_bytes(),
                "line 3: `a` does not sort after `b`; names must be strictly ascending",
                false,
            ),
        ];
        for (bytes, message, has_source) in cases {
            let error = Tree::parse(&bytes).unwrap_err();
            assert_eq!(error.to_string(), message);
            assert_eq!(error.source().is_some(), has_source, "{message}");
        }
    }

    #[test]
    fn every_commit_error_has_a_stable_message_and_the_right_source() {
        let t = TREE_ID;
        let cases: [(Vec<u8>, String, bool); 5] = [
            (
                b"tree x\n\xff".to_vec(),
                "the commit is not valid UTF-8 (valid up to byte 7)".to_owned(),
                false,
            ),
            (
                commit_bytes(t, &[], "human d", "+1", "\n"),
                "line 3: the time is not a canonical decimal integer of seconds".to_owned(),
                false,
            ),
            (
                commit_bytes("b3:00", &[], "human d", "1", "\n"),
                "line 1: invalid object ID: object ID digest must be 64 bytes long, found 2"
                    .to_owned(),
                true,
            ),
            (
                commit_bytes(t, &[], "robot x", "1", "\n"),
                "line 2: malformed actor line: an actor line starts with `human` or `agent`"
                    .to_owned(),
                true,
            ),
            (
                commit_bytes(t, &[HELLO_ID, HELLO_ID], "human d", "1", "\n"),
                format!("duplicate parent {HELLO_ID}"),
                true,
            ),
        ];
        for (bytes, message, has_source) in cases {
            let error = Commit::parse(&bytes).unwrap_err();
            assert_eq!(error.to_string(), message);
            assert_eq!(error.source().is_some(), has_source, "{message}");
        }
    }
}
