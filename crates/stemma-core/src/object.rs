//! Content-addressed objects: blobs, trees, and commits.
//!
//! Object IDs use BLAKE3 with the `b3:` prefix (ADR-001). Commits record their author as an
//! [`Actor`] (ADR-002). The canonical encodings are defined in this module; they are
//! persistent formats, so any change to them changes the ID of every affected object.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use crate::Actor;

/// Algorithm prefix of BLAKE3 object IDs (ADR-001).
const PREFIX: &str = "b3:";

/// Number of hexadecimal characters in a BLAKE3 digest (32 bytes, two characters per byte).
const HEX_LEN: usize = 64;

/// Identifies a stored object by the hash of its canonical encoding.
///
/// The textual form is `b3:` followed by 64 lowercase hexadecimal digits. A value can only be
/// created by hashing or by parsing, so every `ObjectId` is well-formed.
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
/// The checks run in the order of the variants: prefix, length, then characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectIdParseError {
    /// The string does not start with the algorithm prefix `b3:`.
    MissingPrefix,
    /// The digest is not 64 bytes long. Carries the length found, in bytes.
    WrongLength(usize),
    /// The digest contains a character other than a lowercase hexadecimal digit. Carries the
    /// byte offset of the first offending character within the digest.
    NotHex(usize),
}

impl fmt::Display for ObjectIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefix => write!(f, "object ID lacks the prefix `{PREFIX}`"),
            Self::WrongLength(found) => write!(
                f,
                "object ID digest must have {HEX_LEN} characters, found {found} bytes"
            ),
            Self::NotHex(position) => write!(
                f,
                "object ID digest must be lowercase hexadecimal (offending byte at offset {position})"
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

/// Value of a lowercase hexadecimal digit.
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

/// Hashes an encoded object. The hashed input is the ASCII header `<kind> <body length>`, a
/// NUL byte, and the body. The header makes the same bytes stored as different kinds get
/// different IDs.
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

/// The rule that a rejected tree entry name violates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameProblem {
    /// The name is empty.
    Empty,
    /// The name is `.` or `..`.
    DotName,
    /// The name contains `/`.
    PathSeparator,
    /// The name contains a control character, including NUL and newline. The tree encoding is
    /// line-based, so a newline would make it ambiguous.
    ControlCharacter,
}

impl fmt::Display for NameProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "the name is empty",
            Self::DotName => "the name is `.` or `..`",
            Self::PathSeparator => "the name contains `/`",
            Self::ControlCharacter => "the name contains a control character",
        })
    }
}

/// Reasons why a tree entry or a set of tree entries is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
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
            Self::DuplicateName(name) => write!(f, "duplicate tree entry name `{name}`"),
        }
    }
}

impl Error for TreeError {}

/// File mode of a tree entry.
///
/// The entry ID of a [`Directory`](Self::Directory) refers to a tree; the ID of every other
/// mode refers to a blob. The object store has to enforce this once it exists.
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
    /// Mode string written into the tree encoding. It is part of the object ID.
    fn code(self) -> &'static str {
        match self {
            Self::File => "100644",
            Self::Executable => "100755",
            Self::Symlink => "120000",
            Self::Directory => "040000",
        }
    }
}

/// One named entry of a tree.
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
    /// Returns [`TreeError::InvalidName`] if the name is empty, is `.` or `..`, or contains
    /// `/` or a control character.
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

/// Returns the first naming rule the name violates, if any.
///
/// The rules must stay in sync with the line-based tree encoding. Names are compared as given:
/// there is no Unicode normalization and no case folding.
fn name_problem(name: &str) -> Option<NameProblem> {
    if name.is_empty() {
        Some(NameProblem::Empty)
    } else if name == "." || name == ".." {
        Some(NameProblem::DotName)
    } else if name.contains('/') {
        Some(NameProblem::PathSeparator)
    } else if name.chars().any(char::is_control) {
        Some(NameProblem::ControlCharacter)
    } else {
        None
    }
}

/// A directory: a list of named entries, sorted by name in UTF-8 byte order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    entries: Vec<TreeEntry>,
}

impl Tree {
    /// Builds a tree. Entries are sorted by name, so the input order does not matter.
    ///
    /// # Errors
    ///
    /// Returns [`TreeError::DuplicateName`] if two entries share a name.
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

    /// Returns the canonical encoding. Each entry is `<mode> <name>\n<id>\n`.
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
pub enum CommitError {
    /// An author field is empty or contains whitespace or control characters. The commit
    /// encoding separates fields by whitespace and lines, so such a value would make it
    /// ambiguous. Carries the name of the field.
    InvalidActorField(&'static str),
    /// The same parent is listed twice.
    DuplicateParent(ObjectId),
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidActorField(field) => write!(
                f,
                "author field `{field}` must be non-empty without whitespace or control characters"
            ),
            Self::DuplicateParent(id) => write!(f, "duplicate parent {id}"),
        }
    }
}

impl Error for CommitError {}

/// A snapshot of a tree with its parents, its author, and a message.
///
/// A commit carries no timestamp, so two commits with the same tree, parents, author, and
/// message are the same object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: Actor,
    message: String,
}

impl Commit {
    /// Creates a commit. Parents are kept in the given order, and the order is part of the ID.
    ///
    /// # Errors
    ///
    /// Returns [`CommitError::InvalidActorField`] if an author field is unsafe to encode, and
    /// [`CommitError::DuplicateParent`] if a parent is listed twice.
    pub fn new(
        tree: ObjectId,
        parents: Vec<ObjectId>,
        author: Actor,
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

    /// Returns the message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the canonical encoding: the headers `tree`, zero or more `parent`, and
    /// `author`, each on its own line, then a blank line, then the message verbatim.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = format!("tree {}\n", self.tree);
        for parent in &self.parents {
            out.push_str(&format!("parent {parent}\n"));
        }
        out.push_str(&format!("author {}\n", actor_line(&self.author)));
        out.push('\n');
        out.push_str(&self.message);
        out.into_bytes()
    }

    /// Returns the object ID of this commit.
    pub fn id(&self) -> ObjectId {
        hash_encoding(Kind::Commit, &self.encode())
    }
}

/// Checks that every field of the actor can be written into a header line unambiguously.
fn validate_actor(actor: &Actor) -> Result<(), CommitError> {
    let fields: Vec<(&'static str, &str)> = match actor {
        Actor::Human { id } => vec![("id", id)],
        Actor::Agent {
            id,
            model,
            session,
            sponsor,
        } => vec![
            ("id", id),
            ("model", model),
            ("session", session),
            ("sponsor", sponsor),
        ],
    };
    for (field, value) in fields {
        if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(CommitError::InvalidActorField(field));
        }
    }
    Ok(())
}

/// Encodes an actor on one line. Agents also carry model, session, and sponsor.
///
/// The fields are not escaped. [`validate_actor`] guarantees that none contains whitespace, so
/// the line splits unambiguously at spaces.
fn actor_line(actor: &Actor) -> String {
    match actor {
        Actor::Human { id } => format!("human {id}"),
        Actor::Agent {
            id,
            model,
            session,
            sponsor,
        } => format!("agent {id} model={model} session={session} sponsor={sponsor}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_ID: &str = "b3:9db8039d1b509628ad5b125ea89780b69fcde5c272215d690a2e4dc69eac75ad";
    const EMPTY_ID: &str = "b3:4a0b6603381c7c38c557e9ceb3cba8876f0a7863146e7309801352e80b073b65";
    const TREE_ID: &str = "b3:0eec8324e510a63821fcbfda90ce4ed54b01a4db9efd829f7440967808796eb6";
    const HUMAN_COMMIT_ID: &str =
        "b3:df6547b0e420552fbef53cc6f31a83c0ee88838e4fa8065fdd598f197c72e781";
    const AGENT_COMMIT_ID: &str =
        "b3:45401d23555f571cb26b85f43188ed412bdc537ca038baf1b4391ffd99600c7f";

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
        Commit::new(empty_tree_id(), parents, author, message).expect("valid commit")
    }

    // Object IDs

    #[test]
    fn blob_ids_match_golden_vectors() {
        assert_eq!(Blob::new("hello").id().to_string(), HELLO_ID);
        assert_eq!(Blob::new("").id().to_string(), EMPTY_ID);
    }

    #[test]
    fn hashed_input_is_kind_length_nul_and_body() {
        let expected = blake3::hash(b"blob 1\0x");
        assert_eq!(hash_encoding(Kind::Blob, b"x").0, *expected.as_bytes());
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
    fn empty_tree_encodes_to_nothing() {
        assert_eq!(empty_tree_id(), hash_encoding(Kind::Tree, b""));
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
    fn tree_entry_accepts_unusual_but_valid_names() {
        for name in [
            ".hidden",
            "...",
            "a b",
            "tab\u{a0}nbsp",
            "日本語",
            "e\u{301}",
            " ",
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
        let commit = Commit::new(id(TREE_ID), vec![id(EMPTY_ID)], human(), "first change")
            .expect("valid commit");
        let expected =
            format!("tree {TREE_ID}\nparent {EMPTY_ID}\nauthor human daniel\n\nfirst change");
        assert_eq!(commit.encode(), expected.as_bytes());
        assert_eq!(commit.id().to_string(), HUMAN_COMMIT_ID);
    }

    #[test]
    fn agent_commit_encoding_and_id_match_golden_vectors() {
        let commit = Commit::new(id(TREE_ID), vec![], agent(), "work").expect("valid commit");
        let expected = format!(
            "tree {TREE_ID}\nauthor agent agent-1 model=claude-haiku-5-5 session=s1 sponsor=daniel\n\nwork"
        );
        assert_eq!(commit.encode(), expected.as_bytes());
        assert_eq!(commit.id().to_string(), AGENT_COMMIT_ID);
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
        let root = commit(vec![], human(), "first");
        let child = commit(vec![root.id()], human(), "second");
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
        let result = Commit::new(empty_tree_id(), vec![p, id(EMPTY_ID), p], human(), "m");
        assert_eq!(result, Err(CommitError::DuplicateParent(p)));
    }

    #[test]
    fn commit_rejects_author_fields_that_would_make_the_encoding_ambiguous() {
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
        ];
        for (author, field) in cases {
            assert_eq!(
                Commit::new(empty_tree_id(), vec![], author.clone(), "m"),
                Err(CommitError::InvalidActorField(field)),
                "author {author:?}"
            );
        }
    }

    #[test]
    fn commit_exposes_its_fields() {
        let tree = empty_tree_id();
        let c = Commit::new(tree, vec![id(HELLO_ID)], agent(), "msg").expect("valid commit");
        assert_eq!(c.tree(), tree);
        assert_eq!(c.parents(), [id(HELLO_ID)]);
        assert_eq!(c.author(), &agent());
        assert_eq!(c.message(), "msg");
    }
}
