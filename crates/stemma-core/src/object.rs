//! Content-addressed objects: blobs, trees, and commits (ADR-001, ADR-002).

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use crate::Actor;

/// Algorithm prefix of BLAKE3 object IDs (ADR-001).
const PREFIX: &str = "b3:";

/// Length of the hexadecimal digest of a BLAKE3 hash in bytes times two.
const HEX_LEN: usize = 64;

/// Identifies a stored object by the hash of its canonical encoding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(String);

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reasons why a string is not a valid object ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The string does not start with the algorithm prefix `b3:`.
    MissingPrefix,
    /// The digest does not have 64 hexadecimal characters.
    WrongLength,
    /// The digest contains characters other than lowercase hexadecimal digits.
    NotHex,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefix => write!(f, "object ID lacks the prefix `{PREFIX}`"),
            Self::WrongLength => write!(f, "object ID digest must have {HEX_LEN} characters"),
            Self::NotHex => write!(f, "object ID digest must be lowercase hexadecimal"),
        }
    }
}

impl Error for ParseError {}

impl FromStr for ObjectId {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let digest = s.strip_prefix(PREFIX).ok_or(ParseError::MissingPrefix)?;
        if digest.len() != HEX_LEN {
            return Err(ParseError::WrongLength);
        }
        if !digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(ParseError::NotHex);
        }
        Ok(Self(s.to_owned()))
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
    fn name(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
        }
    }
}

/// Hashes an encoded object. The header carries the kind and length, so the same bytes
/// stored as different kinds get different IDs.
fn hash_encoding(kind: Kind, body: &[u8]) -> ObjectId {
    let header = format!("{} {}\0", kind.name(), body.len());
    let mut hasher = blake3::Hasher::new();
    hasher.update(header.as_bytes());
    hasher.update(body);
    let digest = hasher.finalize().to_hex();
    ObjectId(format!("{PREFIX}{}", digest.as_str()))
}

/// Raw file content.
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

/// Reasons why a set of tree entries is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeError {
    /// An entry name is empty, is `.` or `..`, or contains `/` or a newline.
    InvalidName(String),
    /// Two entries share the same name.
    DuplicateName(String),
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => write!(f, "invalid tree entry name `{name}`"),
            Self::DuplicateName(name) => write!(f, "duplicate tree entry name `{name}`"),
        }
    }
}

impl Error for TreeError {}

/// File mode of a tree entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryMode {
    /// A regular file.
    File,
    /// A regular file with the executable bit set.
    Executable,
}

impl EntryMode {
    fn code(self) -> &'static str {
        match self {
            Self::File => "100644",
            Self::Executable => "100755",
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
    /// Creates an entry. The name is checked when the tree is built.
    pub fn new(name: impl Into<String>, mode: EntryMode, id: ObjectId) -> Self {
        Self {
            name: name.into(),
            mode,
            id,
        }
    }
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\n'])
}

/// A directory: a sorted list of named entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    entries: Vec<TreeEntry>,
}

impl Tree {
    /// Builds a tree. Entries are sorted by name, so the input order does not matter.
    pub fn new(mut entries: Vec<TreeEntry>) -> Result<Self, TreeError> {
        for entry in &entries {
            if !is_valid_name(&entry.name) {
                return Err(TreeError::InvalidName(entry.name.clone()));
            }
        }
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

    /// Returns the canonical encoding: one `mode name` line and one ID line per entry.
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

/// A snapshot of a tree with its parents, its author, and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: Actor,
    message: String,
}

impl Commit {
    /// Creates a commit. Parents are kept in the given order.
    pub fn new(
        tree: ObjectId,
        parents: Vec<ObjectId>,
        author: Actor,
        message: impl Into<String>,
    ) -> Self {
        Self {
            tree,
            parents,
            author,
            message: message.into(),
        }
    }

    /// Returns the canonical encoding: headers, a blank line, then the message.
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

/// Encodes an actor on one line. Agents also carry model, session, and sponsor.
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

    #[test]
    fn blob_id_has_prefix_and_full_digest() {
        let id = Blob::new("hello").id().to_string();
        assert!(id.starts_with("b3:"));
        assert_eq!(id.len(), PREFIX.len() + HEX_LEN);
    }

    #[test]
    fn same_content_gives_same_id_and_different_content_differs() {
        assert_eq!(Blob::new("a").id(), Blob::new("a").id());
        assert_ne!(Blob::new("a").id(), Blob::new("b").id());
    }

    #[test]
    fn kind_is_part_of_the_hash() {
        assert_ne!(
            hash_encoding(Kind::Blob, b"x"),
            hash_encoding(Kind::Tree, b"x")
        );
    }

    #[test]
    fn object_id_roundtrips_through_text() {
        let id = Blob::new("data").id();
        let parsed: ObjectId = id.to_string().parse().expect("own output must parse");
        assert_eq!(parsed, id);
    }

    #[test]
    fn parse_rejects_malformed_ids() {
        assert_eq!(
            "sha1:00".parse::<ObjectId>(),
            Err(ParseError::MissingPrefix)
        );
        assert_eq!("b3:abc".parse::<ObjectId>(), Err(ParseError::WrongLength));
        let upper = format!("b3:{}", "A".repeat(HEX_LEN));
        assert_eq!(upper.parse::<ObjectId>(), Err(ParseError::NotHex));
    }

    #[test]
    fn tree_id_does_not_depend_on_entry_order() {
        let blob = Blob::new("x").id();
        let a = TreeEntry::new("a.txt", EntryMode::File, blob.clone());
        let b = TreeEntry::new("b.txt", EntryMode::Executable, blob);
        let forward = Tree::new(vec![a.clone(), b.clone()]).expect("valid tree");
        let backward = Tree::new(vec![b, a]).expect("valid tree");
        assert_eq!(forward.id(), backward.id());
    }

    #[test]
    fn tree_rejects_bad_and_duplicate_names() {
        let id = Blob::new("x").id();
        let bad = TreeEntry::new("a/b", EntryMode::File, id.clone());
        assert_eq!(
            Tree::new(vec![bad]),
            Err(TreeError::InvalidName("a/b".into()))
        );
        let first = TreeEntry::new("same", EntryMode::File, id.clone());
        let second = TreeEntry::new("same", EntryMode::Executable, id);
        assert_eq!(
            Tree::new(vec![first, second]),
            Err(TreeError::DuplicateName("same".into()))
        );
    }

    #[test]
    fn commit_id_changes_with_parent() {
        let tree = Tree::new(vec![]).expect("empty tree is valid").id();
        let root = Commit::new(tree.clone(), vec![], human(), "first");
        let child = Commit::new(tree, vec![root.id()], human(), "second");
        assert_ne!(root.id(), child.id());
    }

    #[test]
    fn commit_records_agent_sponsor_and_model() {
        let tree = Tree::new(vec![]).expect("empty tree is valid").id();
        let commit = Commit::new(tree, vec![], agent(), "work");
        let text = String::from_utf8(commit.encode()).expect("encoding is UTF-8");
        assert!(
            text.contains("author agent agent-1 model=claude-haiku-5-5 session=s1 sponsor=daniel")
        );
    }
}
