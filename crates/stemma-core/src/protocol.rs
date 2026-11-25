//! The messages that a clone and a hub exchange (ADR-022, ADR-023): the protocol version, and one
//! writer and one strict reader for each message, which both sides share.
//!
//! Every message begins with the line `stemma-protocol <n>`. This module speaks one number,
//! [`VERSION`]. A reader that meets another number refuses with a typed error
//! ([`ParseError::UnsupportedVersion`], [`StreamError::UnsupportedVersion`]), whatever follows the
//! first line, and the answer to such a message is [`ErrorMessage::UnsupportedVersion`], which
//! names the versions spoken. That answer is the one exception: the reader of the error accepts
//! an `unsupported-version` under every number (see [`ErrorMessage`]).
//!
//! The small messages are line-oriented text: the first line, then lines `<keyword> <value>` in
//! a fixed order, each ended by a line feed. They are the list of references ([`RefList`]), the
//! fetch request ([`FetchRequest`]), the error ([`ErrorMessage`]), and, in the module [`record`],
//! the ref update and the record entry that answers it as a receipt (ADR-025). The object stream
//! is binary and begins with the same first line ([`ObjectStreamWriter`], [`ObjectStreamReader`]).
//! There are no capabilities and no optional fields: from the first release that carries the
//! protocol on, every change of a message is a new number (see [`VERSION`]).
//!
//! The readers hold the rule of ADR-005. A reader accepts only what its writer writes, so a
//! message has exactly one encoding and `parse(bytes).encode() == bytes` for every accepted
//! input; everything else is refused with a typed error, and no reader panics. What a reader
//! claims about an ID is a claim only: the kind of a commit ID and the existence of an object are
//! confirmed by a store, never by a message.
//!
//! The readers of the small messages take the whole message from memory, and the reader of the
//! stream takes the largest object it may hold. The bounds for the size of a request and of an
//! object are set by the receiver (ADR-023); this module names no value.
//!
//! No message carries a count or an end mark. A list of references that is cut after a whole
//! line from the `key` line on, a fetch request that is cut after a whole line from its first
//! `want` on, and a stream that is cut between two frames read as shorter messages. A cut before
//! a line that must be there is refused as a missing line, and so is every cut of an error
//! message; a cut inside a line or a frame is refused as cut off. The carrier must therefore
//! report a body that ends early, as HTTP does by the length of the body, and the caller must not
//! hand a body to a reader after such a report.
//! The caller also chooses the reader: an error arrives in place of the message that was asked
//! for, and the transport says which of the two it is (the status code, ADR-023).
//!
//! Not part of a message, because they belong to the transport: TLS, the token, the content
//! encoding, URL paths, and status codes (ADR-023).
//!
//! The messages of [`examples`] are checked in. The tests of this crate read them, and so does a
//! hub, as conformance tests through its dependency on this crate (ADR-022).

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io::{self, BufRead, Read, Write};
use std::str::FromStr;

use crate::object::{hash_encoding, CommitId, Kind, ObjectId, ObjectIdParseError};
use crate::refs::{BranchName, BranchNameError, RefKind};
use crate::signature::{KeyError, PublicKey};

/// The protocol version that this module speaks: the number in the first line of every message.
/// It is independent of the version of the crate and of `stemma-repository` (ADR-009). Number 1
/// is the protocol of phase 5: until the first release that carries the protocol, the slices of
/// the phase add their messages and lines to it. From that release on, a change of a message
/// needs an ADR and a new number (ADR-022, ADR-023).
pub const VERSION: u32 = 1;

/// The keyword of the first line of every message.
const VERSION_KEYWORD: &str = "stemma-protocol";

/// The longest first line that the reader of a stream reads before it gives up, without the line
/// feed: the keyword, a space, and the ten digits of the largest `u32`.
const MAX_FIRST_LINE: usize = VERSION_KEYWORD.len() + 1 + 10;

/// The longest header of a frame that the reader of a stream reads before it gives up, without
/// the NUL byte. The header of a known kind has at most 28 bytes (`commit2`, a space, and the
/// twenty digits of the largest `u64`); the rest is room for the name of a kind that this version
/// does not know, so that a name of up to that length is reported as such.
const MAX_FRAME_HEADER: usize = 64;

/// The first line of a message of the given version, with its line feed.
fn first_line(version: u32) -> String {
    format!("{VERSION_KEYWORD} {version}\n")
}

/// Reads a number from 1: decimal digits without a sign and without a leading zero, that fit
/// the type.
fn parse_positive<T: FromStr>(text: &str) -> Option<T> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The ID that a hub assigned to a repository: 128 random bits, written as 32 lowercase
/// hexadecimal digits (ADR-027). The list of references names it, and a ref update binds it
/// (ADR-025). A clone keeps it in the configuration of the remote
/// ([`RemoteConfig`](crate::remote::RemoteConfig)), so that it can notice when another repository
/// lies under the same address; that check belongs to a later slice.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepositoryId([u8; 16]);

impl RepositoryId {
    /// Wraps the 16 bytes of an ID.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Returns the 16 bytes of the ID.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for RepositoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for RepositoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepositoryId({self})")
    }
}

/// Why a string is not a repository ID. The length is checked first, then the characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RepositoryIdParseError {
    /// The string is not 32 bytes long. Carries the length found, in bytes.
    WrongLength(usize),
    /// A character is not a lowercase hexadecimal digit. Carries the byte offset of the first
    /// such character.
    NotHex(usize),
}

impl fmt::Display for RepositoryIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength(found) => {
                write!(f, "a repository ID is 32 bytes long, found {found}")
            }
            Self::NotHex(position) => write!(
                f,
                "a repository ID is lowercase hexadecimal (offending byte at offset {position})"
            ),
        }
    }
}

impl Error for RepositoryIdParseError {}

impl FromStr for RepositoryId {
    type Err = RepositoryIdParseError;

    /// Reads the canonical form only: uppercase digits are refused, so that an ID has one text.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let digits = text.as_bytes();
        if digits.len() != 32 {
            return Err(RepositoryIdParseError::WrongLength(digits.len()));
        }
        let nibble = |position: usize| match digits[position] {
            byte @ b'0'..=b'9' => Ok(byte - b'0'),
            byte @ b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(RepositoryIdParseError::NotHex(position)),
        };
        let mut bytes = [0u8; 16];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = (nibble(2 * i)? << 4) | nibble(2 * i + 1)?;
        }
        Ok(Self(bytes))
    }
}

/// The hash of an entry of the record of ref updates on a hub (ADR-025). Its text is that of an
/// object ID, `b3:` and 64 lowercase hexadecimal digits, but it names an entry of the record, not
/// an object. A value comes from a message or from the file of a remote reference
/// ([`RemoteRef`](crate::remote::RemoteRef)); the record and its hash belong to a later slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordHash(ObjectId);

impl fmt::Display for RecordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for RecordHash {
    type Err = ObjectIdParseError;

    /// Reads the canonical form only, as [`ObjectId`] does.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        ObjectId::from_str(text).map(Self)
    }
}

/// The directory of a kind of reference in a qualified name.
fn directory(kind: RefKind) -> &'static str {
    match kind {
        RefKind::Branch => "heads",
        RefKind::Tag => "tags",
    }
}

/// The name of a reference on a hub with its kind: `heads/<name>` for a branch and `tags/<name>`
/// for a tag (ADR-025). The name follows the rule of [`BranchName`] for both kinds, as in the
/// store (ADR-008).
///
/// The order is that of the bytes of the text, so every branch comes before every tag.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QualifiedName {
    kind: RefKind,
    name: BranchName,
}

impl QualifiedName {
    /// The qualified name of a reference of the given kind.
    pub fn new(kind: RefKind, name: BranchName) -> Self {
        Self { kind, name }
    }

    /// Whether the reference is a branch or a tag.
    pub fn kind(&self) -> RefKind {
        self.kind
    }

    /// The name without its kind.
    pub fn name(&self) -> &BranchName {
        &self.name
    }
}

impl fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", directory(self.kind), self.name)
    }
}

impl Ord for QualifiedName {
    fn cmp(&self, other: &Self) -> Ordering {
        directory(self.kind)
            .cmp(directory(other.kind))
            .then_with(|| self.name.cmp(&other.name))
    }
}

impl PartialOrd for QualifiedName {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Why a string is not a qualified name.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum QualifiedNameError {
    /// The string does not begin with `heads/` or `tags/`.
    UnknownKind,
    /// The name after the kind is refused.
    Name(BranchNameError),
}

impl fmt::Display for QualifiedNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKind => {
                f.write_str("the name of a reference begins with `heads/` or `tags/`")
            }
            Self::Name(source) => write!(f, "{source}"),
        }
    }
}

impl Error for QualifiedNameError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message is that of the cause, so the source is the cause of the cause (see the
            // crate documentation).
            Self::Name(source) => source.source(),
            Self::UnknownKind => None,
        }
    }
}

impl FromStr for QualifiedName {
    type Err = QualifiedNameError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (kind, name) = match text.split_once('/') {
            Some(("heads", name)) => (RefKind::Branch, name),
            Some(("tags", name)) => (RefKind::Tag, name),
            _ => return Err(QualifiedNameError::UnknownKind),
        };
        BranchName::try_new(name)
            .map(|name| Self { kind, name })
            .map_err(QualifiedNameError::Name)
    }
}

/// A reference as the list of references names it: its qualified name, its commit, and the hash
/// of the last record entry of this name (ADR-025), which a ref update names as `previous`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    name: QualifiedName,
    commit: CommitId,
    entry: RecordHash,
}

impl Reference {
    /// A reference at a commit, with the hash of its last record entry.
    pub fn new(name: QualifiedName, commit: CommitId, entry: RecordHash) -> Self {
        Self {
            name,
            commit,
            entry,
        }
    }

    /// The qualified name.
    pub fn name(&self) -> &QualifiedName {
        &self.name
    }

    /// The commit that the reference names. From a reader this is a claim: no store has confirmed
    /// that the ID names a commit.
    pub fn commit(&self) -> CommitId {
        self.commit
    }

    /// The hash of the last record entry of this name.
    pub fn entry(&self) -> RecordHash {
        self.entry
    }
}

/// A former key of a hub with the range of record numbers that it signed (ADR-026).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormerKey {
    first: u64,
    last: u64,
    key: PublicKey,
}

impl FormerKey {
    /// A key that signed the record entries from `first` to `last`, both included.
    ///
    /// # Errors
    ///
    /// [`RefListError::EmptyRange`] if `first` is 0, the record counts from 1, or lies after
    /// `last`.
    pub fn try_new(first: u64, last: u64, key: PublicKey) -> Result<Self, RefListError> {
        if first == 0 || first > last {
            return Err(RefListError::EmptyRange { first, last });
        }
        Ok(Self { first, last, key })
    }

    /// The number of the first record entry that the key signed.
    pub fn first(&self) -> u64 {
        self.first
    }

    /// The number of the last record entry that the key signed.
    pub fn last(&self) -> u64 {
        self.last
    }

    /// The key.
    pub fn key(&self) -> &PublicKey {
        &self.key
    }
}

/// Why a list of references cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefListError {
    /// The range of a former key begins at 0 or after its end.
    EmptyRange {
        /// The first number of the range.
        first: u64,
        /// The last number of the range.
        last: u64,
    },
    /// The ranges of two former keys share a record number. Carries the first number of the later
    /// range.
    OverlappingRanges(u64),
    /// The same qualified name is listed twice.
    DuplicateReference(QualifiedName),
}

impl fmt::Display for RefListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRange { first, last } => write!(
                f,
                "the range {first} to {last} of a former key does not begin at 1 or later and \
                 end at its beginning or later"
            ),
            Self::OverlappingRanges(first) => write!(
                f,
                "the range of a former key that begins at {first} overlaps the range before it"
            ),
            Self::DuplicateReference(name) => write!(f, "the reference {name} is listed twice"),
        }
    }
}

impl Error for RefListError {}

/// The list of references of a repository on a hub, the answer to the first request of a fetch
/// (ADR-023): the repository ID, the public key of the hub with its former keys (ADR-026), and
/// every reference with its commit and its record entry (ADR-025).
///
/// The list is whole, without a filter, and holds the references that exist: a name that was
/// deleted is not listed. The former keys stand in the order of their ranges, and the references
/// in the order of their qualified names.
///
/// ```text
/// stemma-protocol 1
/// repository <32 lowercase hexadecimal digits>
/// key ssh-ed25519 <base64>
/// former <first> <last> ssh-ed25519 <base64>      zero or more
/// ref heads/<name> <commit> <record entry>        zero or more, then the tags
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefList {
    repository: RepositoryId,
    key: PublicKey,
    former: Vec<FormerKey>,
    references: Vec<Reference>,
}

impl RefList {
    /// Builds a list. The former keys are sorted by their ranges and the references by their
    /// qualified names, so the order of the arguments does not matter.
    ///
    /// # Errors
    ///
    /// [`RefListError::OverlappingRanges`] if two former keys share a record number, and
    /// [`RefListError::DuplicateReference`] if a qualified name is listed twice.
    pub fn try_new(
        repository: RepositoryId,
        key: PublicKey,
        mut former: Vec<FormerKey>,
        mut references: Vec<Reference>,
    ) -> Result<Self, RefListError> {
        former.sort_by_key(|key| key.first);
        if let Some(pair) = former.windows(2).find(|pair| pair[1].first <= pair[0].last) {
            return Err(RefListError::OverlappingRanges(pair[1].first));
        }
        references.sort_by(|a, b| a.name.cmp(&b.name));
        if let Some(pair) = references
            .windows(2)
            .find(|pair| pair[0].name == pair[1].name)
        {
            return Err(RefListError::DuplicateReference(pair[0].name.clone()));
        }
        Ok(Self {
            repository,
            key,
            former,
            references,
        })
    }

    /// The ID of the repository.
    pub fn repository(&self) -> RepositoryId {
        self.repository
    }

    /// The public key of the hub. A clone pins it on the first contact (ADR-026).
    pub fn key(&self) -> &PublicKey {
        &self.key
    }

    /// The former keys of the hub, in the order of their ranges.
    pub fn former(&self) -> &[FormerKey] {
        &self.former
    }

    /// The references, in the order of their qualified names.
    pub fn references(&self) -> &[Reference] {
        &self.references
    }

    /// Writes the message.
    pub fn encode(&self) -> Vec<u8> {
        let mut text = first_line(VERSION);
        text.push_str(&format!("repository {}\n", self.repository));
        text.push_str(&format!("key {}\n", self.key));
        for former in &self.former {
            text.push_str(&format!(
                "former {} {} {}\n",
                former.first, former.last, former.key
            ));
        }
        for reference in &self.references {
            text.push_str(&format!(
                "ref {} {} {}\n",
                reference.name, reference.commit, reference.entry
            ));
        }
        text.into_bytes()
    }

    /// Reads the message, as [`encode`](Self::encode) writes it, and nothing else.
    ///
    /// The ID of a reference is claimed to name a commit; no store has confirmed it.
    ///
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a first line that is missing or malformed,
    /// another protocol version, a line that is missing, repeated, unknown, or out of order, a
    /// last line without a line feed, a line with the wrong number of fields, a repository ID, a
    /// key, a number, a name, or a hash that is not in canonical form, the range of a former key
    /// that is empty or does not lie after the range before it, and a reference that does not
    /// come after the one before it in the order of the qualified names. The first problem is
    /// reported, with its line.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let mut lines = Lines::open(bytes)?;
        let (line, value) = lines.require("repository")?;
        let repository = value
            .parse()
            .map_err(|source| ParseError::InvalidRepositoryId { line, source })?;
        let (line, value) = lines.require("key")?;
        let key = parse_key(line, value)?;
        let mut former: Vec<FormerKey> = Vec::new();
        while let Some((line, value)) = lines.take("former")? {
            let mut fields = value.splitn(3, ' ');
            let (Some(first), Some(last), Some(key)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(malformed(line, Malformed::Fields));
            };
            let (Some(first), Some(last)) =
                (parse_positive::<u64>(first), parse_positive::<u64>(last))
            else {
                return Err(malformed(line, Malformed::Number));
            };
            if first > last || former.last().is_some_and(|before| first <= before.last) {
                return Err(malformed(line, Malformed::Range));
            }
            let key = parse_key(line, key)?;
            former.push(FormerKey { first, last, key });
        }
        let mut references: Vec<Reference> = Vec::new();
        while let Some((line, value)) = lines.take("ref")? {
            let fields: Vec<&str> = value.split(' ').collect();
            let [name, commit, entry] = fields[..] else {
                return Err(malformed(line, Malformed::Fields));
            };
            let name: QualifiedName = name
                .parse()
                .map_err(|source| ParseError::InvalidName { line, source })?;
            let commit = parse_commit(line, commit)?;
            let entry = entry
                .parse()
                .map_err(|source| ParseError::InvalidRecordHash { line, source })?;
            if references.last().is_some_and(|before| before.name >= name) {
                return Err(malformed(line, Malformed::Order));
            }
            references.push(Reference {
                name,
                commit,
                entry,
            });
        }
        lines.end()?;
        Ok(Self {
            repository,
            key,
            former,
            references,
        })
    }
}

/// Why a fetch request cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FetchRequestError {
    /// The request wants no commit.
    NothingWanted,
}

impl fmt::Display for FetchRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NothingWanted => f.write_str("a fetch request wants at least one commit"),
        }
    }
}

impl Error for FetchRequestError {}

/// The request of a fetch (ADR-023): the commits that the client wants and the commits that it
/// has. The answer is the object stream of everything that is reachable from the wanted commits
/// and not from the known commits that the client has. A clone is the same request without a
/// commit that the client has.
///
/// Both are sets: each commit is written once, in the order of the IDs. A request wants at least
/// one commit. The same commit may be wanted and had.
///
/// ```text
/// stemma-protocol 1
/// want <commit>      one or more
/// have <commit>      zero or more
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    want: BTreeSet<CommitId>,
    have: BTreeSet<CommitId>,
}

impl FetchRequest {
    /// Builds a request. A commit that is named twice counts once.
    ///
    /// # Errors
    ///
    /// [`FetchRequestError::NothingWanted`] if `want` is empty.
    pub fn try_new(
        want: impl IntoIterator<Item = CommitId>,
        have: impl IntoIterator<Item = CommitId>,
    ) -> Result<Self, FetchRequestError> {
        let want: BTreeSet<CommitId> = want.into_iter().collect();
        if want.is_empty() {
            return Err(FetchRequestError::NothingWanted);
        }
        Ok(Self {
            want,
            have: have.into_iter().collect(),
        })
    }

    /// The commits that the client wants.
    pub fn want(&self) -> &BTreeSet<CommitId> {
        &self.want
    }

    /// The commits that the client has.
    pub fn have(&self) -> &BTreeSet<CommitId> {
        &self.have
    }

    /// Writes the message.
    pub fn encode(&self) -> Vec<u8> {
        let mut text = first_line(VERSION);
        for commit in &self.want {
            text.push_str(&format!("want {commit}\n"));
        }
        for commit in &self.have {
            text.push_str(&format!("have {commit}\n"));
        }
        text.into_bytes()
    }

    /// Reads the message, as [`encode`](Self::encode) writes it, and nothing else.
    ///
    /// Every ID is claimed to name a commit; no store has confirmed it.
    ///
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a first line that is missing or malformed,
    /// another protocol version, a request without a `want` line, a line that is unknown or out
    /// of order, a last line without a line feed, an object ID that is not in canonical form,
    /// and a commit that does not come after the one before it in the order of the IDs, which
    /// includes a commit that is named twice. The version is judged first, then the UTF-8 of the
    /// rest, then the lines in order; the first problem of a line is reported with its line.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let mut lines = Lines::open(bytes)?;
        let want = parse_commits(&mut lines, "want")?;
        if want.is_empty() {
            return Err(malformed(lines.line, Malformed::Expected("want")));
        }
        let have = parse_commits(&mut lines, "have")?;
        lines.end()?;
        Ok(Self { want, have })
    }
}

/// Reads the lines of one keyword that each name a commit, in the ascending order of the IDs.
fn parse_commits(
    lines: &mut Lines<'_>,
    keyword: &'static str,
) -> Result<BTreeSet<CommitId>, ParseError> {
    let mut commits = BTreeSet::new();
    while let Some((line, value)) = lines.take(keyword)? {
        let commit = parse_commit(line, value)?;
        if commits.last().is_some_and(|before| *before >= commit) {
            return Err(malformed(line, Malformed::Order));
        }
        commits.insert(commit);
    }
    Ok(commits)
}

/// The versions that the sender of an [`ErrorMessage::UnsupportedVersion`] speaks: at least one,
/// in ascending order, each once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spoken(Vec<u32>);

impl Spoken {
    /// The versions that this module speaks: [`VERSION`] alone.
    pub fn here() -> Self {
        Self(vec![VERSION])
    }

    /// Builds the list. The versions are sorted, and one that is named twice counts once.
    ///
    /// # Errors
    ///
    /// [`ErrorMessageError::NoVersion`] if no version is named, and
    /// [`ErrorMessageError::VersionZero`] if one of them is 0, which is no protocol version.
    pub fn try_new(versions: impl IntoIterator<Item = u32>) -> Result<Self, ErrorMessageError> {
        let versions: BTreeSet<u32> = versions.into_iter().collect();
        if versions.contains(&0) {
            return Err(ErrorMessageError::VersionZero);
        }
        if versions.is_empty() {
            return Err(ErrorMessageError::NoVersion);
        }
        Ok(Self(versions.into_iter().collect()))
    }

    /// The versions, in ascending order.
    pub fn versions(&self) -> &[u32] {
        &self.0
    }

    /// The highest version, which the first line of the message carries.
    fn highest(&self) -> u32 {
        *self.0.last().expect("at least one version is spoken")
    }
}

/// The text of an [`ErrorMessage::Malformed`]: one line that is not empty and holds no control
/// character and no line or paragraph separator (U+2028, U+2029). Other characters that a
/// terminal treats specially, such as the marks of the text direction, are part of a reason; a
/// program that shows the reason of another side escapes what its output cannot take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason(String);

impl Reason {
    /// Checks the text.
    ///
    /// # Errors
    ///
    /// [`ErrorMessageError::Reason`] if the text is empty or holds a control character, which
    /// includes a line feed, or a line or paragraph separator.
    pub fn try_new(text: impl Into<String>) -> Result<Self, ErrorMessageError> {
        let text = text.into();
        if is_reason(&text) {
            Ok(Self(text))
        } else {
            Err(ErrorMessageError::Reason)
        }
    }

    /// The reason from the message of an error. A control character and a line or paragraph
    /// separator become a space, and an empty message becomes `-`, so every message gives a
    /// reason.
    pub fn from_message(message: &dyn fmt::Display) -> Self {
        let text: String = message
            .to_string()
            .chars()
            .map(|c| if breaks_a_line(c) { ' ' } else { c })
            .collect();
        Self(if text.is_empty() {
            "-".to_owned()
        } else {
            text
        })
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether the character is a control character or a line or paragraph separator, which end a
/// line in a listing as a line feed does.
fn breaks_a_line(c: char) -> bool {
    c.is_control() || c == '\u{2028}' || c == '\u{2029}'
}

/// Whether the text is a reason: not empty, and without a character that breaks a line.
fn is_reason(text: &str) -> bool {
    !text.is_empty() && !text.chars().any(breaks_a_line)
}

/// Why a part of an error message cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorMessageError {
    /// No version is named as spoken.
    NoVersion,
    /// The version 0 is named as spoken.
    VersionZero,
    /// The reason is empty or holds a control character or a line or paragraph separator.
    Reason,
}

impl fmt::Display for ErrorMessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoVersion => "an unsupported version is answered with at least one version",
            Self::VersionZero => "0 is no protocol version",
            Self::Reason => {
                "a reason is one line, not empty, without a control character or a line separator"
            }
        })
    }
}

impl Error for ErrorMessageError {}

/// The error that one side answers instead of the message that was asked for (ADR-023): the
/// first line, then the kind of the error with its facts. The kind decides what the receiver
/// does; a status code of the transport names the coarse class only.
///
/// This version knows the two kinds that the reading of a message gives. The refusals of a ref
/// update, of a statement, and of the bounds come with the slices that build them.
///
/// ```text
/// stemma-protocol <highest version spoken>      stemma-protocol 1
/// error unsupported-version                     error malformed
/// spoken <version> <version> ...                reason <one line>
/// ```
///
/// `unsupported-version` is the one message that is read across versions: its form is the same
/// under every number, and its first line carries the highest version that its sender speaks, so
/// a receiver that speaks another number still learns the list. Every other kind is read under
/// [`VERSION`] only.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorMessage {
    /// The message had a protocol version that the sender of this error does not speak. Carries
    /// the versions that it speaks.
    UnsupportedVersion(Spoken),
    /// The message could not be read. Carries what the reader reported.
    Malformed(Reason),
}

impl ErrorMessage {
    /// Writes the message.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::UnsupportedVersion(spoken) => {
                let versions: Vec<String> = spoken.0.iter().map(u32::to_string).collect();
                format!(
                    "{}error unsupported-version\nspoken {}\n",
                    first_line(spoken.highest()),
                    versions.join(" ")
                )
            }
            Self::Malformed(reason) => {
                format!("{}error malformed\nreason {reason}\n", first_line(VERSION))
            }
        }
        .into_bytes()
    }

    /// Reads the message, as [`encode`](Self::encode) writes it, and nothing else.
    ///
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a first line that is missing or malformed,
    /// a line that is missing, repeated, unknown, or out of order, a last line without a line
    /// feed, and an unknown kind of error.
    ///
    /// `unsupported-version` is read under every number. It also refuses a `spoken` line without
    /// a version, a version that is not a canonical number, a version that does not come after
    /// the one before it, which includes a version that is named twice, and a first line whose
    /// number is not the last version named.
    ///
    /// Under another protocol version than [`VERSION`], everything that is not a line
    /// `error unsupported-version` after the first line is [`ParseError::UnsupportedVersion`]:
    /// another kind, another line, no line, and a line that is cut off. So are bytes that are
    /// not UTF-8 anywhere after the first line, also behind such a line, because nothing of a
    /// message that is no text is read.
    ///
    /// `malformed` also refuses a reason that is empty or holds a control character or a line or
    /// paragraph separator.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let (version, rest) = read_version(bytes)?;
        // Under another number only the second line of an `unsupported-version` is known, so
        // whatever else stands there is the message of a version that is not spoken here.
        let foreign = |error| match version {
            VERSION => error,
            found => ParseError::UnsupportedVersion { found },
        };
        let mut lines = Lines::after_first_line(bytes, rest).map_err(foreign)?;
        let kind = lines.take("error").map_err(foreign)?;
        if let Some((_, "unsupported-version")) = kind {
            let (line, value) = lines.require("spoken")?;
            let mut versions: Vec<u32> = Vec::new();
            for field in value.split(' ') {
                let Some(spoken) = parse_positive::<u32>(field) else {
                    return Err(malformed(line, Malformed::Number));
                };
                if versions.last().is_some_and(|before| *before >= spoken) {
                    return Err(malformed(line, Malformed::Order));
                }
                versions.push(spoken);
            }
            if versions.last() != Some(&version) {
                return Err(malformed(1, Malformed::NotHighestVersion));
            }
            lines.end()?;
            return Ok(Self::UnsupportedVersion(Spoken(versions)));
        }
        if version != VERSION {
            return Err(ParseError::UnsupportedVersion { found: version });
        }
        match kind {
            Some((_, "malformed")) => {
                let (line, value) = lines.require("reason")?;
                if !is_reason(value) {
                    return Err(malformed(line, Malformed::Reason));
                }
                lines.end()?;
                Ok(Self::Malformed(Reason(value.to_owned())))
            }
            Some((line, _)) => Err(malformed(line, Malformed::UnknownError)),
            None => Err(malformed(lines.line, Malformed::Expected("error"))),
        }
    }
}

impl fmt::Display for ErrorMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion(spoken) => {
                let versions: Vec<String> = spoken.0.iter().map(u32::to_string).collect();
                write!(
                    f,
                    "the other side does not speak this protocol version; it speaks: {}",
                    versions.join(", ")
                )
            }
            Self::Malformed(reason) => {
                write!(f, "the other side could not read the message: {reason}")
            }
        }
    }
}

impl Error for ErrorMessage {}

impl From<&ParseError> for ErrorMessage {
    /// The answer to a message that a reader of this module refused: the versions spoken here
    /// for another protocol version, and the message of the reader for everything else.
    fn from(error: &ParseError) -> Self {
        match error {
            ParseError::UnsupportedVersion { .. } => Self::UnsupportedVersion(Spoken::here()),
            other => Self::Malformed(Reason::from_message(other)),
        }
    }
}

/// What is wrong with the lines of a message, as far as the shape goes. The `Display` form is
/// the message; a caller that wants to act on the reason matches on the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Malformed {
    /// The last line has no line feed: the message is cut off.
    CutOff,
    /// The first line is missing or is not `stemma-protocol <number>` with a number from 1
    /// without a sign or a leading zero that fits 32 bits.
    FirstLine,
    /// A line with this keyword must stand here, and the message ends or goes on with another
    /// line. Carries the keyword.
    Expected(&'static str),
    /// A line stands where the message must end: it is unknown, repeated, or out of order.
    Unexpected,
    /// The line does not have the fields of its keyword, separated by single spaces.
    Fields,
    /// A number is not a decimal integer from 1 without a sign or a leading zero, or does not
    /// fit its type.
    Number,
    /// The range of a former key ends before it begins, or does not lie after the range of the
    /// line before.
    Range,
    /// A line does not come after the one before it in the order of the message, or repeats it;
    /// in a `spoken` line, a version does not come after the version before it.
    Order,
    /// A key is not in its canonical form: `ssh-ed25519`, a space, and the Base64 text, without
    /// a comment.
    KeyForm,
    /// The kind of an error is not known to this version.
    UnknownError,
    /// The first line of an `unsupported-version` does not carry the highest version that the
    /// message names.
    NotHighestVersion,
    /// The reason of an error is empty or holds a control character or a line or paragraph
    /// separator.
    Reason,
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CutOff => f.write_str("the line is cut off"),
            Self::FirstLine => write!(f, "the first line is not `{VERSION_KEYWORD} <number>`"),
            Self::Expected(keyword) => write!(f, "expected a line `{keyword} ...`"),
            Self::Unexpected => f.write_str("the line does not belong here"),
            Self::Fields => f.write_str("the line does not have the fields of its keyword"),
            Self::Number => f.write_str(
                "a number is not a decimal integer from 1 without a leading zero, or is too large",
            ),
            Self::Range => {
                f.write_str("the range is empty or does not lie after the range of the line before")
            }
            Self::Order => {
                f.write_str("the line or the version does not come after the one before it")
            }
            Self::KeyForm => {
                f.write_str("the key is not in the form `ssh-ed25519 <base64>` without a comment")
            }
            Self::UnknownError => f.write_str("unknown kind of error"),
            Self::NotHighestVersion => {
                f.write_str("the first line does not carry the highest version spoken")
            }
            Self::Reason => {
                f.write_str("the reason is empty or holds a control character or a line separator")
            }
        }
    }
}

/// Why a small message is refused by its reader. Line numbers count from 1; a line that is
/// missing is reported at the line where it was expected, which may be the line after the last.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseError {
    /// The message is not valid UTF-8.
    NotUtf8 {
        /// Length of the longest valid prefix, in bytes.
        valid_up_to: usize,
    },
    /// The first line is well formed and names another protocol version than [`VERSION`]. The
    /// answer to the sender is [`ErrorMessage::UnsupportedVersion`] with [`Spoken::here`]
    /// (see `From<&ParseError>` of [`ErrorMessage`]).
    UnsupportedVersion {
        /// The version that the message names.
        found: u32,
    },
    /// The shape of the lines or a plain value (a number, a range, the form of a key, a reason)
    /// is wrong; a value with an error type of its own (a repository ID, a key, a name, an
    /// object ID, the hash of a record entry) has a variant of its own.
    Malformed {
        /// Number of the line.
        line: usize,
        /// What is wrong with it.
        reason: Malformed,
    },
    /// A repository ID is not in canonical form.
    InvalidRepositoryId {
        /// Number of the line.
        line: usize,
        /// Why the ID is invalid.
        source: RepositoryIdParseError,
    },
    /// A key is no Ed25519 public key in the OpenSSH format.
    InvalidKey {
        /// Number of the line.
        line: usize,
        /// Why the key is invalid.
        source: KeyError,
    },
    /// The qualified name of a reference is refused.
    InvalidName {
        /// Number of the line.
        line: usize,
        /// Why the name is invalid.
        source: QualifiedNameError,
    },
    /// The ID of a commit is not in canonical form.
    InvalidObjectId {
        /// Number of the line.
        line: usize,
        /// Why the ID is invalid.
        source: ObjectIdParseError,
    },
    /// The hash of a record entry is not in canonical form.
    InvalidRecordHash {
        /// Number of the line.
        line: usize,
        /// Why the hash is invalid.
        source: ObjectIdParseError,
    },
    /// A rule of a ref update or of a record entry is broken ([`record`]).
    Record {
        /// Number of the line.
        line: usize,
        /// The rule.
        reason: record::RecordProblem,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8 { valid_up_to } => write!(
                f,
                "the message is not valid UTF-8 (valid up to byte {valid_up_to})"
            ),
            Self::UnsupportedVersion { found } => write!(
                f,
                "protocol version {found} is not spoken here; this side speaks version {VERSION}"
            ),
            Self::Malformed { line, reason } => write!(f, "line {line}: {reason}"),
            Self::InvalidRepositoryId { line, source } => {
                write!(f, "line {line}: invalid repository ID: {source}")
            }
            Self::InvalidKey { line, source } => write!(f, "line {line}: invalid key: {source}"),
            Self::InvalidName { line, source } => write!(f, "line {line}: {source}"),
            Self::InvalidObjectId { line, source } => {
                write!(f, "line {line}: invalid object ID: {source}")
            }
            Self::InvalidRecordHash { line, source } => {
                write!(f, "line {line}: invalid hash of a record entry: {source}")
            }
            Self::Record { line, reason } => write!(f, "line {line}: {reason}"),
        }
    }
}

impl Error for ParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message of each variant includes the message of the cause, so the source is the
            // cause of the cause (see the crate documentation).
            Self::InvalidRepositoryId { source, .. } => source.source(),
            Self::InvalidKey { source, .. } => source.source(),
            Self::InvalidName { source, .. } => source.source(),
            Self::InvalidObjectId { source, .. } | Self::InvalidRecordHash { source, .. } => {
                source.source()
            }
            Self::NotUtf8 { .. }
            | Self::UnsupportedVersion { .. }
            | Self::Malformed { .. }
            | Self::Record { .. } => None,
        }
    }
}

fn malformed(line: usize, reason: Malformed) -> ParseError {
    ParseError::Malformed { line, reason }
}

/// The message as text.
fn text(bytes: &[u8]) -> Result<&str, ParseError> {
    std::str::from_utf8(bytes).map_err(|error| ParseError::NotUtf8 {
        valid_up_to: error.valid_up_to(),
    })
}

/// The lines of a small message that are not read yet.
struct Lines<'a> {
    rest: &'a str,
    /// The number of the next line, from 1.
    line: usize,
}

impl<'a> Lines<'a> {
    /// The lines after the first line of a message of [`VERSION`]. The version is judged before
    /// the rest of the message is looked at, so a message of another version is refused as such
    /// even if this version cannot read a byte of it.
    fn open(bytes: &'a [u8]) -> Result<Self, ParseError> {
        match read_version(bytes)? {
            (VERSION, rest) => Self::after_first_line(bytes, rest),
            (found, _) => Err(ParseError::UnsupportedVersion { found }),
        }
    }

    /// The lines of `rest`, which is what follows the first line of `message`.
    fn after_first_line(message: &'a [u8], rest: &'a [u8]) -> Result<Self, ParseError> {
        let first_line = message.len() - rest.len();
        let rest = std::str::from_utf8(rest).map_err(|error| ParseError::NotUtf8 {
            valid_up_to: first_line + error.valid_up_to(),
        })?;
        Ok(Self { rest, line: 2 })
    }

    /// The next line without its line feed and what follows it, or `None` at the end of the
    /// message. A last line without a line feed is cut off.
    fn peek(&self) -> Result<Option<(&'a str, &'a str)>, ParseError> {
        if self.rest.is_empty() {
            return Ok(None);
        }
        self.rest
            .split_once('\n')
            .map(Some)
            .ok_or(malformed(self.line, Malformed::CutOff))
    }

    /// Takes the next line if it is `<keyword> <value>`, and returns its number and the value.
    /// Any other line stays, and so does the end of the message.
    fn take(&mut self, keyword: &str) -> Result<Option<(usize, &'a str)>, ParseError> {
        let Some((line, rest)) = self.peek()? else {
            return Ok(None);
        };
        let Some(value) = line
            .strip_prefix(keyword)
            .and_then(|value| value.strip_prefix(' '))
        else {
            return Ok(None);
        };
        let number = self.line;
        self.rest = rest;
        self.line += 1;
        Ok(Some((number, value)))
    }

    /// Takes the next line, which must be `<keyword> <value>`.
    fn require(&mut self, keyword: &'static str) -> Result<(usize, &'a str), ParseError> {
        self.take(keyword)?
            .ok_or(malformed(self.line, Malformed::Expected(keyword)))
    }

    /// The message must end here.
    fn end(&self) -> Result<(), ParseError> {
        match self.peek()? {
            None => Ok(()),
            Some(_) => Err(malformed(self.line, Malformed::Unexpected)),
        }
    }
}

/// Reads the first line of a message from its bytes and returns its number and what follows the
/// line. Only the first line is looked at.
fn read_version(bytes: &[u8]) -> Result<(u32, &[u8]), ParseError> {
    let Some(end) = bytes.iter().position(|&byte| byte == b'\n') else {
        text(bytes)?;
        let reason = if bytes.is_empty() {
            Malformed::FirstLine
        } else {
            Malformed::CutOff
        };
        return Err(malformed(1, reason));
    };
    let version = text(&bytes[..end])?
        .strip_prefix(VERSION_KEYWORD)
        .and_then(|value| value.strip_prefix(' '))
        .and_then(parse_positive::<u32>)
        .ok_or(malformed(1, Malformed::FirstLine))?;
    Ok((version, &bytes[end + 1..]))
}

/// Reads a key in its canonical form, which is the text that the writer writes for it.
fn parse_key(line: usize, text: &str) -> Result<PublicKey, ParseError> {
    let key = PublicKey::from_openssh(text).map_err(|source| {
        // The cause quotes parts of the text, which comes from the other side: escape it, so
        // that the message of the error holds no control character of a hub.
        let source = match source {
            KeyError::Malformed { reason } => KeyError::Malformed {
                reason: reason.escape_debug().to_string(),
            },
            other => other,
        };
        ParseError::InvalidKey { line, source }
    })?;
    if key.to_openssh() != text {
        return Err(malformed(line, Malformed::KeyForm));
    }
    Ok(key)
}

/// Reads an object ID and claims that it names a commit, which its keyword says.
fn parse_commit(line: usize, text: &str) -> Result<CommitId, ParseError> {
    text.parse()
        .map(CommitId::new)
        .map_err(|source| ParseError::InvalidObjectId { line, source })
}

/// An object as it travels in a stream: its kind and its body, in the encoding of the store
/// (ADR-005, ADR-007). The frame carries no ID; the receiver computes it ([`id`](Self::id)), so
/// a damaged object is another object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamObject {
    /// The kind of the object.
    pub kind: Kind,
    /// The body, which the stream reader has not read as a tree or a commit.
    pub body: Vec<u8>,
}

impl StreamObject {
    /// The ID of the object, computed from its kind and its body.
    pub fn id(&self) -> ObjectId {
        hash_encoding(self.kind, &self.body)
    }
}

/// Writes an object stream (ADR-023): the first line, then every object as `<kind> <length>`, a
/// NUL byte, and the body. The stream has no end mark and no count; it ends where its carrier
/// ends.
///
/// The order of the objects is part of the format and is the duty of the caller: ancestors
/// first, and for each commit its new blobs and trees before the commit itself.
///
/// ```text
/// stemma-protocol 1
/// <kind> <length in decimal> NUL <body>      zero or more
/// ```
#[derive(Debug)]
pub struct ObjectStreamWriter<W: Write> {
    writer: W,
}

impl<W: Write> ObjectStreamWriter<W> {
    /// Begins a stream: writes the first line.
    ///
    /// # Errors
    ///
    /// Returns the error of the writer.
    pub fn new(mut writer: W) -> io::Result<Self> {
        writer.write_all(first_line(VERSION).as_bytes())?;
        Ok(Self { writer })
    }

    /// Writes one object.
    ///
    /// # Errors
    ///
    /// Returns the error of the writer. The stream may then be cut off inside a frame, and the
    /// writer must not be used again.
    pub fn write(&mut self, kind: Kind, body: &[u8]) -> io::Result<()> {
        self.writer.write_all(kind.header(body.len()).as_bytes())?;
        self.writer.write_all(body)
    }

    /// Ends the stream and returns the writer. Nothing is written and nothing is flushed.
    pub fn finish(self) -> W {
        self.writer
    }
}

/// Reads an object stream, as [`ObjectStreamWriter`] writes it, and nothing else.
///
/// The reader checks the frames only: the first line, a known kind, a length in canonical
/// decimal form, and a body of exactly that length. It does not read a body as a tree or a
/// commit and does not know the order of the objects; both belong to the connectivity check of
/// the receiver (ADR-023). A stream that ends between two frames is complete for this reader,
/// because the stream carries no count: what is missing shows in the connectivity check.
///
/// The caller gives this reader the body of a successful answer only: an error message that
/// arrives in place of a stream begins with the same first line, and this reader would report its
/// lines as a damaged frame.
///
/// After an error the stream is lost: the reader does not know where the next frame begins, so
/// every later call returns [`StreamError::Failed`] and reads nothing. That holds for an error of
/// the carrier too, so a carrier that does not block cannot be asked again.
#[derive(Debug)]
pub struct ObjectStreamReader<R: BufRead> {
    reader: R,
    max_object_len: u64,
    /// The number of the next object, from 1.
    object: u64,
    /// Whether a call has returned an error.
    failed: bool,
}

impl<R: BufRead> ObjectStreamReader<R> {
    /// Begins to read a stream: reads and checks the first line. `max_object_len` is the longest
    /// body in bytes that the receiver accepts.
    ///
    /// # Errors
    ///
    /// [`StreamError::FirstLine`] if the first line is missing, cut off, or malformed,
    /// [`StreamError::UnsupportedVersion`] if it names another protocol version, and
    /// [`StreamError::Io`] if the reader fails.
    pub fn new(mut reader: R, max_object_len: u64) -> Result<Self, StreamError> {
        let Delimited::Found(line) = read_delimited(&mut reader, b'\n', MAX_FIRST_LINE)? else {
            return Err(StreamError::FirstLine);
        };
        let found = std::str::from_utf8(&line)
            .ok()
            .and_then(|line| line.strip_prefix(VERSION_KEYWORD))
            .and_then(|value| value.strip_prefix(' '))
            .and_then(parse_positive::<u32>)
            .ok_or(StreamError::FirstLine)?;
        if found != VERSION {
            return Err(StreamError::UnsupportedVersion { found });
        }
        Ok(Self {
            reader,
            max_object_len,
            object: 1,
            failed: false,
        })
    }

    /// Reads the next object, or returns `None` at the end of the stream. A call after the end
    /// asks the carrier again.
    ///
    /// # Errors
    ///
    /// [`StreamError::CutOff`] if the stream ends inside a frame, [`StreamError::Header`] if a
    /// frame does not begin with a header `<kind> <length>` of at most 64 bytes and a NUL byte,
    /// the header is not UTF-8, or the length is not `0` or a decimal integer without a sign or a
    /// leading zero that fits 64 bits, [`StreamError::UnknownKind`] for a kind that this version
    /// does not know, [`StreamError::TooLarge`] for a body that is longer than the bound, before
    /// any of it is read, [`StreamError::Io`] if the reader fails, and [`StreamError::Failed`]
    /// for every call after one of these.
    pub fn next_object(&mut self) -> Result<Option<StreamObject>, StreamError> {
        if self.failed {
            return Err(StreamError::Failed);
        }
        let next = self.read_object();
        self.failed = next.is_err();
        next
    }

    fn read_object(&mut self) -> Result<Option<StreamObject>, StreamError> {
        let object = self.object;
        let header = match read_delimited(&mut self.reader, 0, MAX_FRAME_HEADER)? {
            Delimited::End => return Ok(None),
            Delimited::Found(header) => header,
            Delimited::CutOff => return Err(StreamError::CutOff { object }),
            Delimited::TooLong => return Err(StreamError::Header { object }),
        };
        let (kind_name, length) = std::str::from_utf8(&header)
            .ok()
            .and_then(|header| header.split_once(' '))
            .ok_or(StreamError::Header { object })?;
        let kind = Kind::from_name(kind_name).ok_or_else(|| StreamError::UnknownKind {
            object,
            kind: kind_name.to_owned(),
        })?;
        let length = if length == "0" {
            0
        } else {
            parse_positive::<u64>(length).ok_or(StreamError::Header { object })?
        };
        if length > self.max_object_len {
            return Err(StreamError::TooLarge {
                object,
                length,
                limit: self.max_object_len,
            });
        }
        // The length is a claim of the sender: read up to it, and reserve nothing in advance.
        let mut body = Vec::new();
        (&mut self.reader)
            .take(length)
            .read_to_end(&mut body)
            .map_err(StreamError::Io)?;
        if body.len() as u64 != length {
            return Err(StreamError::CutOff { object });
        }
        self.object += 1;
        Ok(Some(StreamObject { kind, body }))
    }
}

/// What [`read_delimited`] found.
enum Delimited {
    /// The reader was at its end.
    End,
    /// The bytes before the delimiter.
    Found(Vec<u8>),
    /// The reader ended before the delimiter.
    CutOff,
    /// More than the limit of bytes came before the delimiter.
    TooLong,
}

/// Reads up to a delimiter, which is consumed and not returned, and at most `limit` bytes
/// before it.
fn read_delimited<R: BufRead>(
    reader: &mut R,
    delimiter: u8,
    limit: usize,
) -> Result<Delimited, StreamError> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_until(delimiter, &mut bytes)
        .map_err(StreamError::Io)?;
    Ok(match bytes.last() {
        None => Delimited::End,
        Some(&last) if last == delimiter => {
            bytes.pop();
            Delimited::Found(bytes)
        }
        Some(_) if bytes.len() > limit => Delimited::TooLong,
        Some(_) => Delimited::CutOff,
    })
}

/// Why an object stream is refused by its reader. Objects count from 1.
#[derive(Debug)]
#[non_exhaustive]
pub enum StreamError {
    /// The reader that carries the stream failed.
    Io(io::Error),
    /// The first line is missing, cut off, or is not `stemma-protocol <number>` with a number
    /// from 1 without a sign or a leading zero that fits 32 bits.
    FirstLine,
    /// The first line is well formed and names another protocol version than [`VERSION`]. The
    /// answer to the sender is [`ErrorMessage::UnsupportedVersion`] with [`Spoken::here`]
    /// ([`answer`](Self::answer)).
    UnsupportedVersion {
        /// The version that the stream names.
        found: u32,
    },
    /// The stream ends inside a frame.
    CutOff {
        /// Number of the object.
        object: u64,
    },
    /// A frame does not begin with a header `<kind> <length>` of at most 64 bytes and a NUL byte,
    /// the header is not UTF-8, or the length is not `0` or a decimal integer without a sign or a
    /// leading zero that fits 64 bits.
    Header {
        /// Number of the object.
        object: u64,
    },
    /// The kind of an object is not known to this version.
    UnknownKind {
        /// Number of the object.
        object: u64,
        /// The kind as written.
        kind: String,
    },
    /// The body of an object is longer than the receiver accepts.
    TooLarge {
        /// Number of the object.
        object: u64,
        /// The length that the frame names, in bytes.
        length: u64,
        /// The bound of the receiver, in bytes.
        limit: u64,
    },
    /// An earlier call on the same reader returned an error, so the stream is lost.
    Failed,
}

impl StreamError {
    /// The message that answers the sender of the stream, if this module has one:
    /// [`ErrorMessage::UnsupportedVersion`] with the versions spoken here for another protocol
    /// version, and [`ErrorMessage::Malformed`] for a first line or a frame that cannot be read,
    /// which includes a stream that is cut off and an unknown kind.
    ///
    /// `None` for [`Io`](Self::Io) and [`Failed`](Self::Failed), which say nothing about the
    /// stream, and for [`TooLarge`](Self::TooLarge): the stream is well formed, and the kind of
    /// error for a bound comes with the slice that sets the bounds.
    pub fn answer(&self) -> Option<ErrorMessage> {
        match self {
            Self::UnsupportedVersion { .. } => {
                Some(ErrorMessage::UnsupportedVersion(Spoken::here()))
            }
            Self::FirstLine
            | Self::CutOff { .. }
            | Self::Header { .. }
            | Self::UnknownKind { .. } => Some(ErrorMessage::Malformed(Reason::from_message(self))),
            Self::Io(_) | Self::TooLarge { .. } | Self::Failed => None,
        }
    }

    /// The error of the reader that carried the stream, if that is what failed.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "cannot read the object stream: {error}"),
            Self::FirstLine => write!(
                f,
                "the object stream does not begin with the line `{VERSION_KEYWORD} <number>`"
            ),
            Self::UnsupportedVersion { found } => write!(
                f,
                "protocol version {found} is not spoken here; this side speaks version {VERSION}"
            ),
            Self::CutOff { object } => write!(f, "the object stream ends inside object {object}"),
            Self::Header { object } => write!(
                f,
                "object {object}: the frame does not begin with a header `<kind> <length>` of \
                 at most {MAX_FRAME_HEADER} bytes and a NUL byte"
            ),
            Self::UnknownKind { object, kind } => {
                write!(f, "object {object}: unknown kind `{}`", kind.escape_debug())
            }
            Self::TooLarge {
                object,
                length,
                limit,
            } => write!(
                f,
                "object {object}: {length} bytes are more than the limit of {limit} bytes"
            ),
            Self::Failed => {
                f.write_str("an earlier read of the object stream failed, so the stream is lost")
            }
        }
    }
}

impl Error for StreamError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message includes the message of the cause, so the source is the cause of the
            // cause (see the crate documentation).
            Self::Io(error) => error.source(),
            _ => None,
        }
    }
}

pub mod examples {
    //! Example messages of protocol version 1, checked in under `protocol-examples/` of this
    //! crate. Every one is accepted by its reader and is exactly what its writer writes for the
    //! value it holds; the tests of this crate hold both. A hub reads them as conformance tests
    //! (ADR-022).

    /// The message that an example holds, which names its reader.
    ///
    /// The enum is exhaustive on purpose: a conformance test that matches on it stops to build
    /// when a message is added, and so cannot skip the new examples.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum Message {
        /// A list of references ([`RefList`](super::RefList)).
        RefList,
        /// A fetch request ([`FetchRequest`](super::FetchRequest)).
        FetchRequest,
        /// An object stream ([`ObjectStreamReader`](super::ObjectStreamReader)).
        ObjectStream,
        /// An error ([`ErrorMessage`](super::ErrorMessage)).
        Error,
        /// A ref update ([`RefUpdate`](super::record::RefUpdate)).
        RefUpdate,
        /// A receipt, which is one record entry ([`RecordEntry`](super::record::RecordEntry)).
        Receipt,
    }

    /// One example message.
    #[derive(Debug, Clone, Copy)]
    pub struct Example {
        /// The name of the file under `protocol-examples/`.
        pub name: &'static str,
        /// Which message it is.
        pub message: Message,
        /// The message, byte for byte.
        pub bytes: &'static [u8],
    }

    macro_rules! example {
        ($name:literal, $message:ident) => {
            Example {
                name: $name,
                message: Message::$message,
                bytes: include_bytes!(concat!("../protocol-examples/", $name)),
            }
        };
    }

    /// Every example.
    pub const ALL: &[Example] = &[
        example!("refs", RefList),
        example!("refs-empty", RefList),
        example!("fetch", FetchRequest),
        example!("clone", FetchRequest),
        example!("objects", ObjectStream),
        example!("objects-empty", ObjectStream),
        example!("error-unsupported-version", Error),
        example!("error-malformed", Error),
        example!("ref-update", RefUpdate),
        example!("receipt", Receipt),
    ];
}

pub mod record;

#[cfg(test)]
mod tests;
