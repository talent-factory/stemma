//! The ref update and the record of ref updates (ADR-025, ADR-026): the signed request of an
//! actor to move references on a hub, the entry that the hub writes for an accepted request, and
//! the chains that a reader of the record checks.
//!
//! A ref update ([`RefUpdate`]) is a message of the protocol. Its lines between the first line and
//! the `signature` line are the payload that the actor signs ([`RefUpdatePayload`]), a proof of
//! the type [`PayloadKind::RefUpdate`]:
//!
//! ```text
//! stemma-protocol 1
//! hub <address of the hub>
//! repository <32 lowercase hexadecimal digits>
//! ref <qualified name> <previous> <old> <new>      one or more pairs of `ref` and `evidence`,
//! evidence <hash of a statement>                   in the order of the qualified names
//! key ssh-ed25519 <base64>
//! signature <SSHSIG signature on one line>
//! ```
//!
//! `previous` is the hash of the last record entry of the name, `old` and `new` are commits, and
//! `evidence` is the hash of a signed statement; each of them is `-` where there is none.
//!
//! A record entry ([`RecordEntry`]) is the ref update byte for byte as the hub received it, the
//! first line included, then the lines of the hub, then the signature of the hub. The answer to an
//! accepted ref update, the receipt, is this entry:
//!
//! ```text
//! <the ref update, from its first line to its `signature` line>
//! number <n>
//! time <seconds since the Unix epoch>
//! register <commit>
//! policy <commit>                                  zero or more
//! fast-forward <qualified name> <yes or no>        one for each `ref` line, in the same order
//! actor <human or agent> <id>
//! previous <hash of the entry before, or `-`>
//! hub-signature <SSHSIG signature on one line>
//! ```
//!
//! The hash of an entry ([`RecordEntry::hash`]) covers everything before the `hub-signature`
//! line: the payload of the actor, the signature of the actor, and the lines of the hub. The hub
//! signs the text of that hash as a proof of the type [`PayloadKind::RecordEntry`], so its
//! signature stands outside the hash and `previous` does not depend on it. The actor signs one
//! part, the payload of the ref update, and the entry holds no second kind of signed part.
//!
//! The entries of one repository are a record: they are written one after the other into a
//! destination of the caller and read back with [`read_record`]. [`Chain`] is the state that a
//! record leaves behind, the last entry of the repository and of every name, and it holds the one
//! rule of the chains: [`Chain::append`] writes an entry by it and [`Chain::accept`] reads one by
//! it, so the writer and the reader ask the same question. A reader may begin in the middle of a
//! record and then skips the rule for the names that it has not seen yet ([`Chain::after`]); the
//! writer refuses such a chain.
//!
//! The readers hold the rule of ADR-005, as the readers of the other messages do, and they check
//! the signatures as well: a value of [`RefUpdate`] carries a signature that verifies for the key
//! of its payload, and a value of [`RecordEntry`] also one of the hub that verifies for the key
//! that this signature names. Whether those keys are the right ones is decided elsewhere: the key
//! of the actor by the register ([`RecordEntry::standing`]), the key of the hub by what the clone
//! pinned ([`RecordEntry::check_receipt`], [`HubKeys`]).
//!
//! Not part of this module: the accepting operation that decides whether an update may be written
//! ([`Repository::accept_ref_update`](crate::repository::Repository::accept_ref_update),
//! ADR-025), the errors that answer a stale and a refused update ([`refusal`](super::refusal)),
//! and the request that reads a range of the record ([`Range`](super::held::Range)).

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::str::FromStr;

use super::{
    first_line, malformed, parse_commit, parse_key, parse_positive, FormerKey, Lines, Malformed,
    ParseError, QualifiedName, RecordHash, RefListError, RepositoryId, VERSION,
};
use crate::conflict::AnyCommit;
use crate::object::{parse_seconds, CommitId, ObjectId, ObjectIdParseError};
use crate::refs::RefKind;
use crate::register::{signature_from_line, signature_to_line, Identity, ModeError, Register};
use crate::signature::{
    self, PayloadKind, PublicKey, SignError, Signature, Signer, VerifyError, FORMAT_VERSION,
};
use crate::store::ObjectStore;

/// The text of a value that is not there: a name that the hub never saw, a reference that does
/// not exist, a move without evidence, the first entry of a record.
const NONE: &str = "-";

/// The first line of the text that the hash of a record entry is computed over, so that the hash
/// of an entry cannot pass for the hash of another kind of text.
const HASH_HEADER: &str = "stemma-record-hash 1";

/// Writes a value, or `-` where there is none.
fn or_none<T: fmt::Display>(value: &Option<T>) -> String {
    value.as_ref().map_or_else(|| NONE.to_owned(), T::to_string)
}

/// What is wrong with a ref update or a record entry beyond the shape of its lines. The parts
/// refuse a value by it ([`HubAddress::try_new`], [`Move::try_new`],
/// [`RefUpdatePayload::try_new`]), and the readers refuse a line by it
/// ([`ParseError::Record`]), so both sides name a broken rule with the same word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RecordProblem {
    /// The address of the hub does not start with `https://` or `http://` and go on with at
    /// least one printable ASCII character and no space, without `@`, `?`, or `#` and without
    /// `/` at its start or its end.
    Address,
    /// The evidence is not `-` or the hash of a statement in canonical form.
    Evidence,
    /// `old` and `new` are equal, which includes a reference that does not exist before and
    /// after: the update moves nothing.
    NoMove,
    /// A tag is moved from one commit to another. A tag is set and deleted only (ADR-008).
    TagMoved,
    /// `old` names a commit while `previous` is `-`: a name that the hub never saw has no value.
    /// The same holds for the state of a reference in the answer to a stale update, which names
    /// a commit only together with the entry that left it.
    OldWithoutPrevious,
    /// The update names no reference.
    NoReference,
    /// The update names a reference twice.
    DuplicateReference,
    /// A signature is not an SSHSIG signature of an Ed25519 key for the namespace of the proofs,
    /// written on one line.
    SignatureForm,
    /// The signature of the actor names another key than the `key` line of the update: another
    /// key signed, or the `key` line was changed.
    ActorKey,
    /// The signature of the actor names the key of the update and does not verify for it and
    /// the payload: the payload was changed, or the signature is not one of a ref update.
    ActorSignature,
    /// The signature of the hub does not verify for the key that it names and the hash of the
    /// entry.
    HubSignature,
    /// The time is not a canonical decimal integer that fits 64 bits.
    Time,
    /// The actor is not `human <id>` or `agent <id>` with a valid identifier.
    Actor,
    /// A `fast-forward` line does not name the reference of its `ref` line, its value is not
    /// `yes` or `no`, or it says `yes` for a reference that is created or deleted.
    FastForward,
    /// The first entry of a record names a previous entry, or a later entry names none.
    Previous,
    /// The reason of a refusal does not fit its reference: a reason of `main` for another name,
    /// a reason of a branch for a tag, or an operation of the other kind of reference.
    RefusalReason,
}

impl fmt::Display for RecordProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Address => {
                "the address of a hub starts with `https://` or `http://` and goes on with \
                 printable ASCII without a space, `@`, `?`, or `#` and without `/` at its start \
                 or its end"
            }
            Self::Evidence => "the evidence is not `-` or the hash of a statement",
            Self::NoMove => "`old` and `new` are equal, so nothing moves",
            Self::TagMoved => "a tag is set or deleted, not moved to another commit",
            Self::OldWithoutPrevious => {
                "`old` names a commit, but `previous` names no record entry"
            }
            Self::NoReference => "a ref update names at least one reference",
            Self::DuplicateReference => "a reference is named twice",
            Self::SignatureForm => {
                "the signature is not an SSHSIG signature of an Ed25519 key on one line"
            }
            Self::ActorKey => {
                "the signature was made by another key than the key of the ref update"
            }
            Self::ActorSignature => {
                "the signature does not verify for the key and the payload of the ref update"
            }
            Self::HubSignature => {
                "the signature of the hub does not verify for its key and the hash of the entry"
            }
            Self::Time => "the time is not a canonical decimal integer that fits 64 bits",
            Self::Actor => "the actor is not `human <id>` or `agent <id>`",
            Self::FastForward => {
                "the line does not name the reference of its `ref` line with `yes` or `no`, or \
                 says `yes` for a reference that is created or deleted"
            }
            Self::Previous => {
                "only the first entry of a record names no previous entry, and it names none"
            }
            Self::RefusalReason => "the reason of the refusal does not fit the reference",
        })
    }
}

impl Error for RecordProblem {}

/// The public address of a hub, as a client calls it: the part of the address of a remote before
/// `/<namespace>/<repository>` (ADR-023). The payload of a ref update names it, so that another
/// hub cannot pass the update on (ADR-026). It is kept as it was given and is not normalised, by
/// the rule of [`RemoteAddress`](crate::remote::RemoteAddress): no query, no fragment, no `@`,
/// and no `/` at its end. The case of the host and a default port are not folded, so the client
/// and the hub must spell the address alike, byte for byte.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HubAddress(String);

impl HubAddress {
    /// Checks the address.
    ///
    /// # Errors
    ///
    /// [`RecordProblem::Address`] unless the text starts with `https://` or `http://` and goes
    /// on with at least one printable ASCII character and no space, without `@`, `?`, or `#` and
    /// without `/` at its start or its end.
    pub fn try_new(address: impl Into<String>) -> Result<Self, RecordProblem> {
        let address = address.into();
        let rest = ["https://", "http://"]
            .iter()
            .find_map(|scheme| address.strip_prefix(scheme));
        match rest {
            Some(rest)
                if !rest.is_empty()
                    && rest.chars().all(|c| c.is_ascii_graphic())
                    && !rest.contains(['@', '?', '#'])
                    && !rest.starts_with('/')
                    && !rest.ends_with('/') =>
            {
                Ok(Self(address))
            }
            _ => Err(RecordProblem::Address),
        }
    }

    /// Returns the address.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HubAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The hash of a signed statement (ADR-025): the name of a pull request, which is the hash of
/// its opening, and the evidence that a move rests on (the opening of a pull request, a purge
/// statement, or an owner override). Its text is that of an object ID, `b3:` and 64 lowercase
/// hexadecimal digits, but it names a statement, not an object. It is computed by
/// [`Statement::hash`](super::statement::Statement::hash).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StatementHash(ObjectId);

impl StatementHash {
    /// Wraps the digest of a statement.
    pub(crate) fn from_digest(digest: blake3::Hash) -> Self {
        Self(ObjectId::from_digest(digest))
    }

    /// The 64 lowercase hexadecimal digits of the hash, the name of the file of the statement
    /// in a set of statements (ADR-009).
    pub fn hex(&self) -> String {
        self.0.hex()
    }
}

impl fmt::Display for StatementHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for StatementHash {
    type Err = ObjectIdParseError;

    /// Reads the canonical form only, as [`ObjectId`] does.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        ObjectId::from_str(text).map(Self)
    }
}

/// What a ref update asks for one reference (ADR-025): the name, the last record entry of that
/// name, the commit before and the commit after, and the evidence that the move rests on.
/// Creating and deleting are the same move, with no commit before or after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    name: QualifiedName,
    previous: Option<RecordHash>,
    old: Option<CommitId>,
    new: Option<CommitId>,
    evidence: Option<StatementHash>,
}

impl Move {
    /// The move of one reference. `previous` is `None` for a name that the hub never saw, `old`
    /// is `None` for a reference that does not exist, and `new` is `None` to delete it.
    ///
    /// # Errors
    ///
    /// [`RecordProblem::NoMove`] if `old` and `new` are equal, [`RecordProblem::TagMoved`] if a
    /// tag has a commit before and after, and [`RecordProblem::OldWithoutPrevious`] if `old` is a
    /// commit and `previous` is `None`. The rules are checked in this order, for a branch and a
    /// tag alike; only the second depends on the kind.
    pub fn try_new(
        name: QualifiedName,
        previous: Option<RecordHash>,
        old: Option<CommitId>,
        new: Option<CommitId>,
        evidence: Option<StatementHash>,
    ) -> Result<Self, RecordProblem> {
        if old == new {
            return Err(RecordProblem::NoMove);
        }
        if name.kind() == RefKind::Tag && old.is_some() && new.is_some() {
            return Err(RecordProblem::TagMoved);
        }
        if previous.is_none() && old.is_some() {
            return Err(RecordProblem::OldWithoutPrevious);
        }
        Ok(Self {
            name,
            previous,
            old,
            new,
            evidence,
        })
    }

    /// The qualified name of the reference.
    pub fn name(&self) -> &QualifiedName {
        &self.name
    }

    /// The hash of the last record entry of this name, or `None` for a name that the hub never
    /// saw. It is what makes the update count once (ADR-025).
    pub fn previous(&self) -> Option<RecordHash> {
        self.previous
    }

    /// The commit before the move, or `None` if the reference does not exist. From a reader this
    /// is a claim: no store has confirmed that the ID names a commit.
    pub fn old_commit(&self) -> Option<CommitId> {
        self.old
    }

    /// The commit after the move, or `None` if the reference is deleted. A claim, as
    /// [`old_commit`](Self::old_commit) is.
    pub fn new_commit(&self) -> Option<CommitId> {
        self.new
    }

    /// The hash of the statement that the move rests on, or `None`.
    pub fn evidence(&self) -> Option<StatementHash> {
        self.evidence
    }

    /// Whether the move can be a fast-forward at all: the reference exists before and after.
    fn has_both_ends(&self) -> bool {
        self.old.is_some() && self.new.is_some()
    }
}

/// The payload of a ref update, which the actor signs (ADR-025): the address of the hub, the ID
/// of the repository, one or more moves in the order of their qualified names, and the public
/// key. It holds no time, no name of the actor, and no message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdatePayload {
    hub: HubAddress,
    repository: RepositoryId,
    moves: Vec<Move>,
    key: PublicKey,
}

impl RefUpdatePayload {
    /// Builds a payload. The moves are sorted by their qualified names, so the order of the
    /// argument does not matter.
    ///
    /// # Errors
    ///
    /// [`RecordProblem::NoReference`] if `moves` is empty, and
    /// [`RecordProblem::DuplicateReference`] if a qualified name is named twice.
    pub fn try_new(
        hub: HubAddress,
        repository: RepositoryId,
        mut moves: Vec<Move>,
        key: PublicKey,
    ) -> Result<Self, RecordProblem> {
        if moves.is_empty() {
            return Err(RecordProblem::NoReference);
        }
        moves.sort_by(|a, b| a.name.cmp(&b.name));
        if moves.windows(2).any(|pair| pair[0].name == pair[1].name) {
            return Err(RecordProblem::DuplicateReference);
        }
        Ok(Self {
            hub,
            repository,
            moves,
            key,
        })
    }

    /// The address of the hub that the update is meant for.
    pub fn hub(&self) -> &HubAddress {
        &self.hub
    }

    /// The ID of the repository that the update is meant for.
    pub fn repository(&self) -> RepositoryId {
        self.repository
    }

    /// The moves, in the order of their qualified names, at least one.
    pub fn moves(&self) -> &[Move] {
        &self.moves
    }

    /// The public key that signs the update. Which actor it speaks for is decided by the
    /// register, not by the payload.
    pub fn key(&self) -> &PublicKey {
        &self.key
    }

    /// The bytes that the actor signs, as a proof of the type [`PayloadKind::RefUpdate`]: the
    /// lines of the message from `hub` to `key`.
    pub fn encode(&self) -> Vec<u8> {
        let mut text = format!("hub {}\nrepository {}\n", self.hub, self.repository);
        for moved in &self.moves {
            text.push_str(&format!(
                "ref {} {} {} {}\nevidence {}\n",
                moved.name,
                or_none(&moved.previous),
                or_none(&moved.old),
                or_none(&moved.new),
                or_none(&moved.evidence)
            ));
        }
        text.push_str(&format!("key {}\n", self.key));
        text.into_bytes()
    }

    /// Signs the payload and returns the ref update.
    ///
    /// # Errors
    ///
    /// [`SealError::Sign`] with the error of the signer, and [`SealError::Verify`] if the
    /// signature that it returns does not verify for the key of the payload, for example because
    /// the signer holds another key.
    pub fn sign(self, signer: &dyn Signer) -> Result<RefUpdate, SealError> {
        let signature = signature::sign(signer, PayloadKind::RefUpdate, &self.encode())
            .map_err(SealError::Sign)?;
        RefUpdate::try_new(self, signature).map_err(SealError::Verify)
    }
}

/// Why a payload could not be signed.
#[derive(Debug)]
#[non_exhaustive]
pub enum SealError {
    /// The signer failed.
    Sign(SignError),
    /// The signature of the signer does not verify for the key of the payload.
    Verify(VerifyError),
}

impl SealError {
    /// The error of the operating system, if the signer failed with one.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Sign(error) => error.io_error(),
            Self::Verify(_) => None,
        }
    }
}

impl fmt::Display for SealError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sign(error) => error.fmt(f),
            Self::Verify(error) => {
                write!(
                    f,
                    "the signer did not sign for the key of the payload: {error}"
                )
            }
        }
    }
}

// Each message is or includes the message of its cause, so the source is the cause of the cause
// (see the crate documentation), and neither cause has one.
impl Error for SealError {}

/// A ref update (ADR-025): a payload and the signature of its key over it. Every value carries a
/// signature that verifies, whether it was built here or read from a message. It says nothing
/// about the actor: whether the key is in a register, and what it may move, is decided by the
/// accepting operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    payload: RefUpdatePayload,
    signature: Signature,
}

impl RefUpdate {
    /// Joins a payload and its signature.
    ///
    /// # Errors
    ///
    /// The error of [`signature::verify`] if the signature is not one of the key of the payload
    /// over the payload, as a proof of the type [`PayloadKind::RefUpdate`].
    pub fn try_new(payload: RefUpdatePayload, signature: Signature) -> Result<Self, VerifyError> {
        signature::verify(
            &payload.key,
            PayloadKind::RefUpdate,
            FORMAT_VERSION,
            &payload.encode(),
            &signature,
        )?;
        Ok(Self { payload, signature })
    }

    /// The payload.
    pub fn payload(&self) -> &RefUpdatePayload {
        &self.payload
    }

    /// The signature of the key of the payload.
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    /// Writes the message.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = first_line(VERSION).into_bytes();
        bytes.extend_from_slice(&self.payload.encode());
        bytes.extend_from_slice(
            format!("signature {}\n", signature_to_line(&self.signature)).as_bytes(),
        );
        bytes
    }

    /// Reads the message, as [`encode`](Self::encode) writes it, and nothing else, and checks its
    /// signature.
    ///
    /// Every commit is claimed to be one; no store has confirmed it.
    ///
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a first line that is missing or malformed,
    /// another protocol version, a line that is missing, repeated, unknown, or out of order, a
    /// last line without a line feed, a line with the wrong number of fields, an address, a
    /// repository ID, a name, a hash, a commit, or a key that is not in canonical form, a
    /// reference that does not come after the one before it in the order of the qualified names,
    /// a move that [`Move::try_new`] refuses, and a signature that is not in its one-line form.
    /// The first problem is reported, with its line. A message whose form is right is refused
    /// last for its signature, at the `signature` line: as [`RecordProblem::ActorKey`] if the
    /// signature names another key than the `key` line, and as
    /// [`RecordProblem::ActorSignature`] if it names that key and does not verify.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let mut lines = Lines::open(bytes)?;
        let update = read_update(&mut lines)?;
        lines.end()?;
        update.verified()
    }

    /// Reads the form of the message without checking the signature, for the fuzz target: a
    /// fuzzer makes no signature, so the check of the signature would hide the reader from it.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn parse_unverified(bytes: &[u8]) -> Result<Self, ParseError> {
        let mut lines = Lines::open(bytes)?;
        let update = read_update(&mut lines)?;
        lines.end()?;
        Ok(update.unchecked())
    }
}

/// A ref update as its lines were read, with the line of its signature. The signature is not
/// checked yet, so this is no [`RefUpdate`].
struct UnverifiedUpdate {
    payload: RefUpdatePayload,
    signature: Signature,
    line: usize,
}

impl UnverifiedUpdate {
    /// The update, if its signature verifies.
    fn verified(self) -> Result<RefUpdate, ParseError> {
        RefUpdate::try_new(self.payload, self.signature)
            .map_err(|error| problem(self.line, actor_problem(error)))
    }

    /// The update without the check of its signature: for an entry whose reader checks it
    /// before it returns ([`read_entry`]), and for the fuzz target.
    fn unchecked(self) -> RefUpdate {
        RefUpdate {
            payload: self.payload,
            signature: self.signature,
        }
    }
}

/// Why the signature of an actor is refused: for its key, or for what it signs.
fn actor_problem(error: VerifyError) -> RecordProblem {
    match error {
        VerifyError::WrongKey => RecordProblem::ActorKey,
        _ => RecordProblem::ActorSignature,
    }
}

fn problem(line: usize, reason: RecordProblem) -> ParseError {
    ParseError::Record { line, reason }
}

/// Reads a value that may be `-`.
fn parse_optional<T, E>(
    text: &str,
    read: impl FnOnce(&str) -> Result<T, E>,
) -> Result<Option<T>, E> {
    if text == NONE {
        Ok(None)
    } else {
        read(text).map(Some)
    }
}

/// Reads the hash of a record entry, or `-`.
fn parse_entry_hash(line: usize, text: &str) -> Result<Option<RecordHash>, ParseError> {
    parse_optional(text, str::parse::<RecordHash>)
        .map_err(|source| ParseError::InvalidRecordHash { line, source })
}

/// Reads a signature in its one-line form.
fn parse_signature(line: usize, text: &str) -> Result<Signature, ParseError> {
    signature_from_line(text).ok_or(problem(line, RecordProblem::SignatureForm))
}

/// Reads the lines of a ref update after the first line, from `hub` to `signature`. The
/// signature is not checked.
fn read_update(lines: &mut Lines<'_>) -> Result<UnverifiedUpdate, ParseError> {
    let (line, value) = lines.require("hub")?;
    let hub = HubAddress::try_new(value).map_err(|reason| problem(line, reason))?;
    let (line, value) = lines.require("repository")?;
    let repository = value
        .parse()
        .map_err(|source| ParseError::InvalidRepositoryId { line, source })?;
    let mut moves: Vec<Move> = Vec::new();
    while let Some((line, value)) = lines.take("ref")? {
        let fields: Vec<&str> = value.split(' ').collect();
        let [name, previous, old, new] = fields[..] else {
            return Err(malformed(line, Malformed::Fields));
        };
        let name: QualifiedName = name
            .parse()
            .map_err(|source| ParseError::InvalidName { line, source })?;
        let previous = parse_entry_hash(line, previous)?;
        let old = parse_optional(old, |text| parse_commit(line, text))?;
        let new = parse_optional(new, |text| parse_commit(line, text))?;
        if moves.last().is_some_and(|before| before.name >= name) {
            return Err(malformed(line, Malformed::Order));
        }
        // The rules of the move do not depend on the evidence, so they are judged before its
        // line is read: the first problem of a message is the one that is reported.
        let mut moved = Move::try_new(name, previous, old, new, None)
            .map_err(|reason| problem(line, reason))?;
        let (line, value) = lines.require("evidence")?;
        moved.evidence = parse_optional(value, str::parse::<StatementHash>)
            .map_err(|_| problem(line, RecordProblem::Evidence))?;
        moves.push(moved);
    }
    if moves.is_empty() {
        return Err(malformed(lines.line, Malformed::Expected("ref")));
    }
    let (line, value) = lines.require("key")?;
    let key = parse_key(line, value)?;
    let (line, value) = lines.require("signature")?;
    let signature = parse_signature(line, value)?;
    let payload = RefUpdatePayload {
        hub,
        repository,
        moves,
        key,
    };
    Ok(UnverifiedUpdate {
        payload,
        signature,
        line,
    })
}

/// What a hub states about a ref update that it accepts (ADR-025), without what the record
/// itself says: the number and the hash of the previous entry come from the [`Chain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acceptance {
    /// The time of the hub, in seconds since the Unix epoch (UTC).
    pub time: i64,
    /// The commit of `main` whose register the hub used: the tip before the update, or the
    /// pushed commit for the first push into an empty repository (ADR-024).
    pub register: CommitId,
    /// The commits of the policies by which the verdict was reached, in the order in which the
    /// hub names them (ADR-027).
    pub policies: Vec<CommitId>,
    /// The references whose move was a fast-forward. Every other reference of the update moved
    /// otherwise: it was created, deleted, or moved to a commit that does not descend from the
    /// old one.
    pub fast_forward: BTreeSet<QualifiedName>,
    /// The identity of the actor from the register. It is a statement of the hub and no proof.
    pub actor: Identity,
}

/// An entry of the record of ref updates (ADR-025), and so the receipt of an accepted update:
/// the ref update as the hub received it, the lines of the hub, and the signature of the hub
/// over the hash of both.
///
/// Every value carries two signatures that verify: that of the actor for the key of the payload,
/// and that of the hub for the key that the signature names ([`hub_key`](Self::hub_key)).
/// Neither key is trusted by that alone; see [`check_receipt`](Self::check_receipt),
/// [`standing`](Self::standing), and [`Chain::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordEntry {
    update: RefUpdate,
    number: u64,
    time: i64,
    register: CommitId,
    policies: Vec<CommitId>,
    /// Whether each move was a fast-forward, in the order of the moves of the update.
    fast_forward: Vec<bool>,
    actor: Identity,
    previous: Option<RecordHash>,
    signature: Signature,
}

impl RecordEntry {
    /// The ref update, as the hub received it.
    pub fn update(&self) -> &RefUpdate {
        &self.update
    }

    /// The running number of the entry in its repository, from 1.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// The time of the hub, in seconds since the Unix epoch (UTC). It is a signed statement of
    /// the hub (ADR-026).
    pub fn time(&self) -> i64 {
        self.time
    }

    /// The commit of `main` whose register the hub used. A claim: no store has confirmed that
    /// the ID names a commit, and the record keeps no commit alive.
    pub fn register(&self) -> CommitId {
        self.register
    }

    /// The commits of the policies by which the verdict was reached. Claims, as
    /// [`register`](Self::register) is.
    pub fn policies(&self) -> &[CommitId] {
        &self.policies
    }

    /// Whether the move of the reference was a fast-forward, or `None` if the entry does not
    /// list the name. A reference that was created or deleted is never one.
    pub fn is_fast_forward(&self, name: &QualifiedName) -> Option<bool> {
        self.update
            .payload
            .moves
            .iter()
            .position(|moved| &moved.name == name)
            .map(|index| self.fast_forward[index])
    }

    /// The identity of the actor, as the hub read it from the register.
    pub fn actor(&self) -> &Identity {
        &self.actor
    }

    /// The hash of the entry before this one in the repository, or `None` for the first.
    pub fn previous(&self) -> Option<RecordHash> {
        self.previous
    }

    /// The signature of the hub over the hash of the entry.
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    /// The key that signed the entry, as the signature names it. The signature verifies for it;
    /// whether it is the key of the hub is for the caller to say ([`HubKeys`]).
    pub fn hub_key(&self) -> PublicKey {
        self.signature.signing_key()
    }

    /// The bytes that the hash covers: the ref update and the lines of the hub.
    fn hashed(&self) -> Vec<u8> {
        let mut text = format!(
            "number {}\ntime {}\nregister {}\n",
            self.number, self.time, self.register
        );
        for policy in &self.policies {
            text.push_str(&format!("policy {policy}\n"));
        }
        for (moved, forward) in self.update.payload.moves.iter().zip(&self.fast_forward) {
            let forward = if *forward { "yes" } else { "no" };
            text.push_str(&format!("fast-forward {} {forward}\n", moved.name));
        }
        text.push_str(&format!(
            "actor {}\nprevious {}\n",
            self.actor,
            or_none(&self.previous)
        ));
        let mut bytes = self.update.encode();
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }

    /// The hash of the entry, which the next entry of the repository and the next update of each
    /// of its names name as `previous`. It covers the ref update with its signature and the
    /// lines of the hub, and not the signature of the hub.
    pub fn hash(&self) -> RecordHash {
        hash_of(&self.hashed())
    }

    /// Writes the entry.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = self.hashed();
        bytes.extend_from_slice(
            format!("hub-signature {}\n", signature_to_line(&self.signature)).as_bytes(),
        );
        bytes
    }

    /// Reads one entry, as [`encode`](Self::encode) writes it, and nothing else, and checks both
    /// signatures. This is the reader of a receipt.
    ///
    /// # Errors
    ///
    /// Everything that [`RefUpdate::parse`] refuses about the form of the update, then a line of
    /// the hub that is missing, repeated, unknown, or out of order, a number that is not a
    /// decimal integer from 1 without a leading zero, a time, a commit, an actor, or a hash that
    /// is not in canonical form, a `fast-forward` line that does not name the reference of its
    /// `ref` line with `yes` or `no` or that says `yes` for a reference that is created or
    /// deleted, a first entry that names a previous entry or a later entry that names none, a
    /// signature that is not in its one-line form, and anything after the `hub-signature` line.
    /// An entry whose form is right is refused last for a signature of the actor
    /// ([`RecordProblem::ActorKey`], [`RecordProblem::ActorSignature`]) or of the hub
    /// ([`RecordProblem::HubSignature`]) that does not verify, in this order.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        read_entry(bytes)
    }

    /// Reads the form of one entry without checking the signatures, for the fuzz target (see
    /// [`RefUpdate::parse_unverified`]).
    #[cfg(any(test, fuzzing))]
    pub(crate) fn parse_unverified(bytes: &[u8]) -> Result<Self, ParseError> {
        Ok(read_entry_form(bytes)?.0)
    }

    /// Checks a receipt (ADR-025): the entry holds the update as it was sent, signature
    /// included, and it is signed by the key of the hub that the clone pinned. Only then does a
    /// clone move its remote references.
    ///
    /// The signature of the hub itself was checked when the entry was read. That the number
    /// grows from receipt to receipt (ADR-026) is not checked here: the clone knows its last
    /// number, this entry does not.
    ///
    /// # Errors
    ///
    /// [`ReceiptError::OtherUpdate`] if the entry holds another update, and
    /// [`ReceiptError::OtherKey`] if another key than `hub` signed it.
    pub fn check_receipt(&self, sent: &RefUpdate, hub: &PublicKey) -> Result<(), ReceiptError> {
        if &self.update != sent {
            return Err(ReceiptError::OtherUpdate);
        }
        if &self.hub_key() != hub {
            return Err(ReceiptError::OtherKey);
        }
        Ok(())
    }

    /// Checks the actor of the entry against the register that the entry names: the key of the
    /// update is an active key of the actor in the register of that commit.
    ///
    /// The record keeps no commit alive (ADR-025), so the commit may be gone. An entry whose
    /// register commit, its tree, a tree on the path of the register, or the register file
    /// itself is not in the store is [`Standing::NoLongerCheckable`]; that is no damage.
    ///
    /// # Errors
    ///
    /// [`StandingError::Conflict`] if the commit is there and records an open conflict, which
    /// `main` never holds (ADR-019), [`StandingError::NoRegister`] if it holds no register,
    /// [`StandingError::NotActive`] if its register does not hold the key as an active key of
    /// the actor, and [`StandingError::Unreadable`] if the store fails, the path of the register
    /// is taken by something that is no file, or the register is not valid.
    pub fn standing(&self, store: &dyn ObjectStore) -> Result<Standing, StandingError> {
        // A commit with a list of trees has no one tree to read a register from, and it never
        // was a tip of `main`: the entry names a commit that the hub cannot have used.
        match store.get_any_commit(&self.register) {
            Ok(Some(AnyCommit::Commit2(_))) => return Err(StandingError::Conflict),
            Ok(Some(AnyCommit::Commit(_))) => {}
            Ok(None) => return Ok(Standing::NoLongerCheckable),
            Err(error) => return Err(StandingError::Unreadable(ModeError::Store(error))),
        }
        let register = match Register::at_commit(store, &self.register) {
            Err(ModeError::Missing) => return Ok(Standing::NoLongerCheckable),
            Err(error) => return Err(StandingError::Unreadable(error)),
            Ok(None) => return Err(StandingError::NoRegister),
            Ok(Some(register)) => register,
        };
        if register.is_active_key(&self.actor, &self.update.payload.key) {
            Ok(Standing::Checked)
        } else {
            Err(StandingError::NotActive)
        }
    }
}

/// The hash of the bytes of an entry before its `hub-signature` line.
fn hash_of(hashed: &[u8]) -> RecordHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(HASH_HEADER.as_bytes());
    hasher.update(b"\n");
    hasher.update(hashed);
    RecordHash(ObjectId::from_digest(hasher.finalize()))
}

/// Checks the signature of a hub over the hash of an entry, for the key that the signature
/// names.
fn verify_hub(hash: RecordHash, signature: &Signature) -> Result<(), VerifyError> {
    signature::verify(
        &signature.signing_key(),
        PayloadKind::RecordEntry,
        FORMAT_VERSION,
        hash.to_string().as_bytes(),
        signature,
    )
}

/// An entry whose signatures are not checked yet, with the lines of its two signatures.
struct Unverified(RecordEntry, usize, usize);

/// Reads the form of one entry, which is all of `bytes`.
fn read_entry_form(bytes: &[u8]) -> Result<Unverified, ParseError> {
    let mut lines = Lines::open(bytes)?;
    let update = read_update(&mut lines)?;
    let actor_line = update.line;
    let update = update.unchecked();
    let (line, value) = lines.require("number")?;
    let number: u64 = parse_positive(value).ok_or(malformed(line, Malformed::Number))?;
    let (line, value) = lines.require("time")?;
    let time = parse_seconds(value).map_err(|_| problem(line, RecordProblem::Time))?;
    let (line, value) = lines.require("register")?;
    let register = parse_commit(line, value)?;
    let mut policies = Vec::new();
    while let Some((line, value)) = lines.take("policy")? {
        policies.push(parse_commit(line, value)?);
    }
    let mut fast_forward = Vec::new();
    for moved in &update.payload.moves {
        let (line, value) = lines.require("fast-forward")?;
        let forward = match value.split_once(' ') {
            Some((name, "yes")) if name == moved.name.to_string() => true,
            Some((name, "no")) if name == moved.name.to_string() => false,
            _ => return Err(problem(line, RecordProblem::FastForward)),
        };
        if forward && !moved.has_both_ends() {
            return Err(problem(line, RecordProblem::FastForward));
        }
        fast_forward.push(forward);
    }
    let (line, value) = lines.require("actor")?;
    let actor = value
        .parse::<Identity>()
        .ok()
        .filter(|actor| actor.to_string() == value)
        .ok_or(problem(line, RecordProblem::Actor))?;
    let (line, value) = lines.require("previous")?;
    let previous = parse_entry_hash(line, value)?;
    if (number == 1) != previous.is_none() {
        return Err(problem(line, RecordProblem::Previous));
    }
    let (hub_line, value) = lines.require("hub-signature")?;
    let signature = parse_signature(hub_line, value)?;
    lines.end()?;
    let entry = RecordEntry {
        update,
        number,
        time,
        register,
        policies,
        fast_forward,
        actor,
        previous,
        signature,
    };
    Ok(Unverified(entry, actor_line, hub_line))
}

/// Reads one entry, which is all of `bytes`, and checks its signatures.
fn read_entry(bytes: &[u8]) -> Result<RecordEntry, ParseError> {
    let Unverified(entry, actor_line, hub_line) = read_entry_form(bytes)?;
    let RefUpdate { payload, signature } = &entry.update;
    RefUpdate::try_new(payload.clone(), signature.clone())
        .map_err(|error| problem(actor_line, actor_problem(error)))?;
    verify_hub(entry.hash(), &entry.signature)
        .map_err(|_| problem(hub_line, RecordProblem::HubSignature))?;
    Ok(entry)
}

/// The length of the first entry of a record: up to the line feed of the first `hub-signature`
/// line, or all of `bytes` if there is none. Only the last line of an entry begins with that
/// keyword, and no value holds a line feed, so the split is where the writer ended the entry.
fn first_entry_len(bytes: &[u8]) -> usize {
    const LAST_LINE: &[u8] = b"\nhub-signature ";
    bytes
        .windows(LAST_LINE.len())
        .position(|window| window == LAST_LINE)
        .and_then(|start| {
            let value = start + LAST_LINE.len();
            bytes[value..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map(|end| value + end + 1)
        })
        .unwrap_or(bytes.len())
}

/// Why an entry of a record is refused by [`read_record`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordParseError {
    /// The position of the entry in the input, from 1. It is not the number that the entry
    /// names, which may be what is wrong.
    pub entry: usize,
    /// Why the entry is refused. Its lines count from the first line of that entry.
    pub error: ParseError,
}

impl fmt::Display for RecordParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "entry {} of the record: {}", self.entry, self.error)
    }
}

impl Error for RecordParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message includes the message of the cause, so the source is the cause of the cause
        // (see the crate documentation).
        self.error.source()
    }
}

/// Reads the entries of a record that were written one after the other, as [`Chain::append`]
/// writes them into one destination: zero or more entries, each as [`RecordEntry::parse`] reads
/// it, with both signatures checked. An empty input is an empty record.
///
/// The reader checks each entry on its own. That the entries follow one another is the question
/// of [`Chain::accept`] and [`Chain::verify`]. An entry ends with its `hub-signature` line, so
/// each entry is read once and the time is linear in the input; the whole record is held in
/// memory, and a caller that must not hold it reads entry by entry with [`RecordEntry::parse`].
///
/// # Errors
///
/// [`RecordParseError`] with the position of the first entry that
/// [`RecordEntry::parse`] refuses, which includes an entry that is cut off. What follows the
/// last `hub-signature` line is read as one more entry.
pub fn read_record(bytes: &[u8]) -> Result<Vec<RecordEntry>, RecordParseError> {
    let mut entries = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (first, after) = rest.split_at(first_entry_len(rest));
        let entry = read_entry(first).map_err(|error| RecordParseError {
            entry: entries.len() + 1,
            error,
        })?;
        entries.push(entry);
        rest = after;
    }
    Ok(entries)
}

/// Why a receipt is not accepted for an update ([`RecordEntry::check_receipt`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReceiptError {
    /// The entry does not hold the update that was sent.
    OtherUpdate,
    /// The entry is signed by another key than the key of the hub that was given.
    OtherKey,
}

impl fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OtherUpdate => "the receipt does not hold the ref update as it was sent",
            Self::OtherKey => "the receipt is not signed by the key of the hub",
        })
    }
}

impl Error for ReceiptError {}

/// What a reader can say about the actor of an entry ([`RecordEntry::standing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Standing {
    /// The register commit is in the store, and its register holds the key of the update as an
    /// active key of the actor.
    Checked,
    /// The register commit, a tree on the way to its register, or the register file is no longer
    /// in the store, so nothing can be said.
    /// The chains and the signatures of the entry hold all the same.
    NoLongerCheckable,
}

/// Why the actor of an entry does not stand ([`RecordEntry::standing`]).
#[derive(Debug)]
#[non_exhaustive]
pub enum StandingError {
    /// The register commit is in the store and records an open conflict. `main` never holds
    /// such a commit (ADR-019), so no hub read a register from it.
    Conflict,
    /// The register commit is in the store and holds no register.
    NoRegister,
    /// The register of the commit does not hold the key of the update as an active key of the
    /// actor that the hub names.
    NotActive,
    /// The store failed, the path of the register in the commit is taken by something that is
    /// no file, or the register is not valid.
    Unreadable(ModeError),
}

impl fmt::Display for StandingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => f.write_str(
                "the register commit records an open conflict, which `main` never holds",
            ),
            Self::NoRegister => f.write_str("the register commit holds no register"),
            Self::NotActive => f.write_str(
                "the register of the register commit does not hold the key of the ref update as \
                 an active key of the actor",
            ),
            Self::Unreadable(error) => {
                write!(
                    f,
                    "cannot read the register of the register commit: {error}"
                )
            }
        }
    }
}

// The message of `Unreadable` includes the message of its cause, which has no cause of its own.
impl Error for StandingError {}

/// The state of one reference on a hub, as an entry of the record left it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NameState {
    /// The hash of the last entry that lists the name.
    entry: RecordHash,
    /// The commit after that entry, or `None` if it deleted the reference.
    commit: Option<CommitId>,
}

/// A reference whose `previous` or `old` in an update does not match the record, with its state
/// in the record (ADR-025).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleReference {
    /// The qualified name.
    pub name: QualifiedName,
    /// The hash of the last record entry of the name, or `None` for a name that the record
    /// never listed.
    pub entry: Option<RecordHash>,
    /// The commit of the reference, or `None` if it does not exist.
    pub commit: Option<CommitId>,
}

/// Why an update does not continue the chains of its names: every reference that does not match,
/// at least one, in the order of the qualified names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stale(Vec<StaleReference>);

impl Stale {
    /// The answer to a stale update from its references, for the reader of the error that
    /// carries it ([`ErrorMessage::Stale`](super::ErrorMessage::Stale)). The references are
    /// sorted by their qualified names, so the order of the argument does not matter.
    ///
    /// # Errors
    ///
    /// [`RecordProblem::NoReference`] if `references` is empty,
    /// [`RecordProblem::DuplicateReference`] if a qualified name is named twice, and
    /// [`RecordProblem::OldWithoutPrevious`] if a reference names a commit and no entry: a name
    /// that the record never listed has no value.
    pub fn try_new(mut references: Vec<StaleReference>) -> Result<Self, RecordProblem> {
        if references.is_empty() {
            return Err(RecordProblem::NoReference);
        }
        references.sort_by(|a, b| a.name.cmp(&b.name));
        if references
            .windows(2)
            .any(|pair| pair[0].name == pair[1].name)
        {
            return Err(RecordProblem::DuplicateReference);
        }
        if references
            .iter()
            .any(|stale| stale.entry.is_none() && stale.commit.is_some())
        {
            return Err(RecordProblem::OldWithoutPrevious);
        }
        Ok(Self(references))
    }

    /// The references that do not match, each with its state in the record.
    pub fn references(&self) -> &[StaleReference] {
        &self.0
    }
}

impl fmt::Display for Stale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the ref update does not name the last record entry of ")?;
        for (index, stale) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(
                f,
                "{} (entry {}, commit {})",
                stale.name,
                or_none(&stale.entry),
                or_none(&stale.commit)
            )?;
        }
        Ok(())
    }
}

/// Why an update or an entry does not continue a record, or why a chain does not take an entry
/// at all ([`Full`](Self::Full), [`NotWhole`](Self::NotWhole)).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChainError {
    /// The update or the entry names another repository than the record.
    Repository {
        /// The ID of the repository of the record.
        expected: RepositoryId,
        /// The ID that the update names.
        found: RepositoryId,
    },
    /// The entry does not carry the next number of the record.
    Number {
        /// The number that the next entry carries.
        expected: u64,
        /// The number that the entry carries.
        found: u64,
    },
    /// The entry carries the number of the last entry of the record and has another hash: two
    /// entries for one number. Where both are signed by the hub, the hub rewrote its record
    /// (ADR-026).
    Rewritten {
        /// The number that both entries carry.
        number: u64,
    },
    /// The entry does not name the hash of the entry before it.
    Previous {
        /// The number of the entry.
        number: u64,
    },
    /// A reference of the update does not name the last entry of its name, or its `old` is not
    /// the commit that this entry left.
    Stale(Stale),
    /// The record holds the largest number that an entry can carry.
    Full,
    /// The chain began in the middle of the record ([`Chain::after`]), so it does not know the
    /// last entry of every name, and no entry is written by it.
    NotWhole,
    /// The time of the entry lies before the time of the entry before it. The time of the hub
    /// orders the leases (ADR-025), so it never steps back inside one record; two entries may
    /// carry the same second.
    Time {
        /// The time of the entry before.
        last: i64,
        /// The time of the entry.
        found: i64,
    },
}

impl fmt::Display for ChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Repository { expected, found } => write!(
                f,
                "the ref update names the repository {found}, the record is that of {expected}"
            ),
            Self::Number { expected, found } => write!(
                f,
                "the record entry carries the number {found}, the next number is {expected}"
            ),
            Self::Rewritten { number } => write!(
                f,
                "two different record entries carry the number {number}: the record was rewritten"
            ),
            Self::Previous { number } => write!(
                f,
                "the record entry {number} does not name the hash of the entry before it"
            ),
            Self::Stale(stale) => stale.fmt(f),
            Self::Full => f.write_str("the record holds the largest number of an entry"),
            Self::NotWhole => f.write_str(
                "the record was not read from its first entry, so no entry is written by it",
            ),
            Self::Time { last, found } => write!(
                f,
                "the time {found} of the record entry lies before the time {last} of the entry \
                 before it"
            ),
        }
    }
}

impl Error for ChainError {}

/// The state that the record of one repository leaves behind: the number and the hash of its
/// last entry, and for every name that an entry listed the hash of the last such entry with the
/// commit that it left. A deleted name stays, without a commit, so the chain of a name survives
/// its deletion (ADR-025).
///
/// The chain holds the one rule by which an update continues a record
/// ([`check`](Self::check)): the hub writes by it ([`append`](Self::append)) and every reader
/// reads by it ([`accept`](Self::accept)). A value is built by reading the record from its
/// first entry, or begun in the middle ([`after`](Self::after)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    repository: RepositoryId,
    /// The number of the last entry, or 0 before the first.
    last: u64,
    /// The hash of the last entry.
    tip: Option<RecordHash>,
    names: BTreeMap<QualifiedName, NameState>,
    /// Whether every entry from the first one was read, so that a name that is not in `names`
    /// was never listed.
    whole: bool,
    /// The time of the last entry that was written or read, or `None` before the first one.
    time: Option<i64>,
}

impl Chain {
    /// The record of a repository before its first entry.
    pub fn new(repository: RepositoryId) -> Self {
        Self {
            repository,
            last: 0,
            tip: None,
            names: BTreeMap::new(),
            whole: true,
            time: None,
        }
    }

    /// The record of a repository after the entry with this number and this hash, for a reader
    /// that begins in the middle, such as a clone that holds a receipt (ADR-026). The names of
    /// the entries before are not known: the first update of a name that is read from here on
    /// is taken as it is, and only the later ones are checked against it. The chain of the
    /// repository is checked from the first entry that is read on. Such a chain reads only:
    /// [`append`](Self::append) refuses it.
    ///
    /// With the number 0 there is no entry before: the hash is not used, and the chain is that
    /// of [`new`](Self::new).
    pub fn after(repository: RepositoryId, number: u64, hash: RecordHash) -> Self {
        Self {
            repository,
            last: number,
            tip: (number > 0).then_some(hash),
            names: BTreeMap::new(),
            whole: number == 0,
            time: None,
        }
    }

    /// The ID of the repository.
    pub fn repository(&self) -> RepositoryId {
        self.repository
    }

    /// The number of the last entry, or 0 for a record without an entry.
    pub fn last_number(&self) -> u64 {
        self.last
    }

    /// The hash of the last entry, or `None` for a record without an entry.
    pub fn tip(&self) -> Option<RecordHash> {
        self.tip
    }

    /// The hash of the last entry that lists the name, which the next update of the name names
    /// as `previous`, or `None` for a name that no entry listed since the chain began.
    pub fn entry_of(&self, name: &QualifiedName) -> Option<RecordHash> {
        self.names.get(name).map(|state| state.entry)
    }

    /// The commit that the record leaves a reference at, or `None` if the last entry of the name
    /// deleted it or no entry listed the name since the chain began.
    pub fn commit_of(&self, name: &QualifiedName) -> Option<CommitId> {
        self.names.get(name).and_then(|state| state.commit)
    }

    /// Whether an update continues the record: it names the repository of the record, and every
    /// reference names the last entry of its name as `previous` and the commit that this entry
    /// left as `old`. An update whose reference returned to an old commit does not pass again,
    /// because the last entry of the name is another one (A, B, A).
    ///
    /// A name that no entry listed must carry `previous -` in a chain that was read from the
    /// first entry. In a chain that began in the middle ([`after`](Self::after)) such a name is
    /// not checked.
    ///
    /// The address of the hub in the payload is not compared: a hub may move to another
    /// address (ADR-026), so one record can name several. That an update names the address under
    /// which the hub was called is asked by the accepting operation, which knows that address.
    ///
    /// # Errors
    ///
    /// [`ChainError::Repository`] for another repository, and [`ChainError::Stale`] with every
    /// reference that does not match and its state in the record.
    pub fn check(&self, update: &RefUpdate) -> Result<(), ChainError> {
        self.same_repository(update)?;
        let payload = update.payload();
        let stale: Vec<StaleReference> = payload
            .moves
            .iter()
            .filter_map(|moved| {
                let state = self.names.get(&moved.name);
                let matches = match state {
                    Some(state) => moved.previous == Some(state.entry) && moved.old == state.commit,
                    // `old` is `-` as well, by the rule of `Move::try_new`.
                    None if self.whole => moved.previous.is_none(),
                    None => true,
                };
                (!matches).then(|| StaleReference {
                    name: moved.name.clone(),
                    entry: state.map(|state| state.entry),
                    commit: state.and_then(|state| state.commit),
                })
            })
            .collect();
        if stale.is_empty() {
            Ok(())
        } else {
            Err(ChainError::Stale(Stale(stale)))
        }
    }

    /// Whether the update names the repository of the record.
    fn same_repository(&self, update: &RefUpdate) -> Result<(), ChainError> {
        let found = update.payload.repository;
        if found == self.repository {
            Ok(())
        } else {
            Err(ChainError::Repository {
                expected: self.repository,
                found,
            })
        }
    }

    /// The number of the next entry.
    fn next_number(&self) -> Result<u64, ChainError> {
        self.last.checked_add(1).ok_or(ChainError::Full)
    }

    /// Moves the chain past an entry that continues it.
    fn advance(&mut self, entry: &RecordEntry) {
        let hash = entry.hash();
        self.last = entry.number;
        self.tip = Some(hash);
        self.time = Some(entry.time);
        for moved in &entry.update.payload.moves {
            self.names.insert(
                moved.name.clone(),
                NameState {
                    entry: hash,
                    commit: moved.new,
                },
            );
        }
    }

    /// Writes the entry of an accepted update into `destination` and moves the chain past it.
    /// This is the writer of the record: the entry carries the next number and the hash of the
    /// last entry, the signer of the hub signs its hash, and the bytes of
    /// [`RecordEntry::encode`] are written in one call and flushed. The returned entry is the
    /// receipt. The update is borrowed, so the caller still holds it after a refusal, to answer
    /// the client or to write again. A flush is no promise that the bytes survive a crash: the
    /// caller makes the destination durable (for a file, a sync) before the receipt leaves the
    /// hub and before a reference moves, or a clone can hold a receipt for a number that the
    /// record gives to another entry later.
    ///
    /// The core never knows the key of the hub (ADR-026): `signer` is given the bytes to sign
    /// and returns the signature. Whether the update may be accepted (the register, the role,
    /// the gate) is not asked here; the caller decides it first and calls this under the lock
    /// of the references, before the first move (ADR-025).
    ///
    /// Every refusal comes before the first byte is written. If the destination fails, the
    /// chain does not move, and the destination may hold a part of the entry or, where only the
    /// flush failed, all of it. A part is refused by [`read_record`] as cut off; a whole entry
    /// is read as valid, although no receipt was returned for it. The caller therefore cuts the
    /// destination back to its length before the call, whatever the failure was, before it
    /// writes again: a second entry with the same number would read as a rewritten record.
    ///
    /// # Errors
    ///
    /// [`WriteError::Chain`] if the chain began in the middle of the record
    /// ([`after`](Self::after), [`ChainError::NotWhole`]): the reader of the whole record checks
    /// every name, so the writer must know every name too. The same if the update does not
    /// continue the record ([`check`](Self::check)) or the record is full.
    /// [`WriteError::FastForward`] if `acceptance` names a fast-forward for a reference that the
    /// update does not list or that it creates or deletes, [`WriteError::Chain`] with
    /// [`ChainError::Time`] if the time of `acceptance` lies before the time of the last entry,
    /// [`WriteError::Sign`] with the error of the signer, [`WriteError::Signature`] if the
    /// signature that it returns does not verify for the key that it names and the hash of the
    /// entry, and [`WriteError::Io`] if the destination fails.
    pub fn append(
        &mut self,
        update: &RefUpdate,
        acceptance: Acceptance,
        signer: &dyn Signer,
        destination: &mut dyn Write,
    ) -> Result<RecordEntry, WriteError> {
        if !self.whole {
            return Err(WriteError::Chain(ChainError::NotWhole));
        }
        self.check(update).map_err(WriteError::Chain)?;
        self.not_before(acceptance.time)
            .map_err(WriteError::Chain)?;
        let number = self.next_number().map_err(WriteError::Chain)?;
        let moves = &update.payload.moves;
        if let Some(name) = acceptance.fast_forward.iter().find(|name| {
            !moves
                .iter()
                .any(|moved| &&moved.name == name && moved.has_both_ends())
        }) {
            return Err(WriteError::FastForward(name.clone()));
        }
        let fast_forward = moves
            .iter()
            .map(|moved| acceptance.fast_forward.contains(&moved.name))
            .collect();
        // The signature is not known before the hash is: the entry is built with the signature
        // of the actor in its place, which the hash does not cover, hashed, and completed.
        let mut entry = RecordEntry {
            signature: update.signature.clone(),
            update: update.clone(),
            number,
            time: acceptance.time,
            register: acceptance.register,
            policies: acceptance.policies,
            fast_forward,
            actor: acceptance.actor,
            previous: self.tip,
        };
        let hash = entry.hash();
        entry.signature = signature::sign(
            signer,
            PayloadKind::RecordEntry,
            hash.to_string().as_bytes(),
        )
        .map_err(WriteError::Sign)?;
        verify_hub(hash, &entry.signature).map_err(WriteError::Signature)?;
        destination
            .write_all(&entry.encode())
            .and_then(|()| destination.flush())
            .map_err(WriteError::Io)?;
        self.advance(&entry);
        Ok(entry)
    }

    /// Reads an entry into the chain: the reader's side of [`append`](Self::append). The entry
    /// carries the next number, names the hash of the last entry as `previous`, and its update
    /// continues the record ([`check`](Self::check)). The signatures were checked when the entry
    /// was read; which key of a hub signed it, and whether the actor stands, is asked by
    /// [`verify`](Self::verify).
    ///
    /// # Errors
    ///
    /// [`ChainError::Repository`] for an entry of another repository, then
    /// [`ChainError::Rewritten`] if the entry carries the number of the last entry and has
    /// another hash, [`ChainError::Full`] if the last entry carries the largest number, so that
    /// no entry can follow it, [`ChainError::Number`] for any other number that is not the next
    /// one, which includes the last entry read twice, [`ChainError::Previous`] if `previous` is
    /// not the hash of the last entry, [`ChainError::Stale`] from [`check`](Self::check), and
    /// [`ChainError::Time`] if the time of the entry lies before the time of the entry before
    /// it. The chain does not move on an error.
    pub fn accept(&mut self, entry: &RecordEntry) -> Result<(), ChainError> {
        self.continues(entry)?;
        self.advance(entry);
        Ok(())
    }

    /// Whether an entry is the next link of the chain, by the rules of [`accept`](Self::accept).
    fn continues(&self, entry: &RecordEntry) -> Result<(), ChainError> {
        // An entry of another repository says nothing about this record, so it is no rewrite.
        self.same_repository(&entry.update)?;
        if entry.number == self.last && self.tip.is_some_and(|tip| tip != entry.hash()) {
            return Err(ChainError::Rewritten {
                number: entry.number,
            });
        }
        let expected = self.next_number()?;
        if entry.number != expected {
            return Err(ChainError::Number {
                expected,
                found: entry.number,
            });
        }
        if entry.previous != self.tip {
            return Err(ChainError::Previous {
                number: entry.number,
            });
        }
        self.check(&entry.update)?;
        self.not_before(entry.time)
    }

    /// Whether an entry with this time may follow the last one: the time of the hub does not
    /// step back. A chain that began in the middle knows no time until it read an entry.
    fn not_before(&self, time: i64) -> Result<(), ChainError> {
        match self.time {
            Some(last) if time < last => Err(ChainError::Time { last, found: time }),
            _ => Ok(()),
        }
    }

    /// Reads entries into the chain and checks everything that a reader of a record checks
    /// (ADR-025): the chain of the repository and the chain of every name
    /// ([`accept`](Self::accept)), that the key of the hub for the number of the entry signed it
    /// (`keys`), and that the key of the actor was an active key in the register that the entry
    /// names ([`RecordEntry::standing`]). The signatures themselves were checked when the
    /// entries were read.
    ///
    /// Returns the numbers of the entries whose actor could not be checked, because the
    /// register commit, a tree on the way to its register, or the register file is not in the
    /// store ([`Standing::NoLongerCheckable`]). They are no longer checkable, not damaged: their
    /// chains and signatures hold. The store cannot tell an object that was removed from one
    /// that never was there, so the list means that the actor of these entries is a statement
    /// of the hub alone; a caller that needs the actor checked does not drop it.
    ///
    /// # Errors
    ///
    /// [`RecordError`] for the first entry that fails: first for the key of the hub, then for
    /// the chains, then for the actor. The first and the last carry the number of the entry;
    /// an error of the chains carries what [`ChainError`] names. The chain has moved past the
    /// entries before it and not past the entry that failed.
    pub fn verify(
        &mut self,
        entries: &[RecordEntry],
        keys: &HubKeys,
        store: &dyn ObjectStore,
    ) -> Result<Vec<u64>, RecordError> {
        let mut unchecked = Vec::new();
        for entry in entries {
            let number = entry.number;
            if !keys.signed(entry) {
                return Err(RecordError::HubKey { number });
            }
            self.continues(entry).map_err(RecordError::Chain)?;
            match entry.standing(store) {
                Ok(Standing::Checked) => {}
                Ok(Standing::NoLongerCheckable) => unchecked.push(number),
                Err(source) => return Err(RecordError::Standing { number, source }),
            }
            self.advance(entry);
        }
        Ok(unchecked)
    }
}

/// Why an entry could not be written ([`Chain::append`]).
#[derive(Debug)]
#[non_exhaustive]
pub enum WriteError {
    /// The chain began in the middle of the record, the update does not continue the record, the
    /// record is full, or the time of the entry lies before the time of the last entry.
    Chain(ChainError),
    /// A fast-forward is named for a reference that the update does not list, or that it
    /// creates or deletes.
    FastForward(QualifiedName),
    /// The signer of the hub failed.
    Sign(SignError),
    /// The signature of the signer does not verify for the key that it names and the hash of
    /// the entry.
    Signature(VerifyError),
    /// The destination failed.
    Io(io::Error),
}

impl WriteError {
    /// The error of the operating system, if there is one.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sign(error) => error.io_error(),
            Self::Chain(_) | Self::FastForward(_) | Self::Signature(_) => None,
        }
    }
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Chain(error) => error.fmt(f),
            Self::FastForward(name) => write!(
                f,
                "the move of {name} is named a fast-forward, but the ref update does not move \
                 it from one commit to another"
            ),
            Self::Sign(error) => error.fmt(f),
            Self::Signature(error) => write!(
                f,
                "the signer of the hub did not sign the hash of the record entry: {error}"
            ),
            Self::Io(error) => write!(f, "cannot write the record entry: {error}"),
        }
    }
}

impl Error for WriteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            // The message is or includes the message of the cause, so the source is the cause
            // of the cause (see the crate documentation).
            Self::Io(error) => error.source(),
            Self::Chain(_) | Self::FastForward(_) | Self::Sign(_) | Self::Signature(_) => None,
        }
    }
}

/// Why a record does not hold ([`Chain::verify`]).
#[derive(Debug)]
#[non_exhaustive]
pub enum RecordError {
    /// An entry does not continue the chain of the repository or of one of its names.
    Chain(ChainError),
    /// An entry is not signed by the key of the hub for its number.
    HubKey {
        /// The number of the entry.
        number: u64,
    },
    /// The actor of an entry does not stand in the register that the entry names.
    Standing {
        /// The number of the entry.
        number: u64,
        /// Why.
        source: StandingError,
    },
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Chain(error) => error.fmt(f),
            Self::HubKey { number } => write!(
                f,
                "the record entry {number} is not signed by the key of the hub for its number"
            ),
            Self::Standing { number, source } => write!(f, "record entry {number}: {source}"),
        }
    }
}

// Each message is or includes the message of its cause, and no cause has a cause of its own.
impl Error for RecordError {}

/// The keys of a hub by the numbers of the record entries that they signed (ADR-026): the
/// current key, which a clone pinned, and the former keys with their ranges, which the list of
/// references names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubKeys {
    current: PublicKey,
    former: Vec<FormerKey>,
}

impl HubKeys {
    /// The keys of a hub: the current key, which the caller trusts (a clone passes the key that
    /// it pinned, not the key that a list of references names), and the former keys. A number
    /// in the range of a former key was signed by that key, and every other number by the
    /// current key.
    ///
    /// # Errors
    ///
    /// [`RefListError::OverlappingRanges`] if two former keys share a record number, by the rule
    /// of [`RefList::try_new`](super::RefList::try_new).
    pub fn try_new(current: PublicKey, mut former: Vec<FormerKey>) -> Result<Self, RefListError> {
        former.sort_by_key(FormerKey::first);
        if let Some(pair) = former
            .windows(2)
            .find(|pair| pair[1].first() <= pair[0].last())
        {
            return Err(RefListError::OverlappingRanges(pair[1].first()));
        }
        Ok(Self { current, former })
    }

    /// The current key, which signs every entry outside the ranges of the former keys.
    pub fn current(&self) -> &PublicKey {
        &self.current
    }

    /// The former keys, in the order of their ranges, which share no number.
    pub fn former(&self) -> &[FormerKey] {
        &self.former
    }

    /// The key that signed the entry with this number.
    pub fn at(&self, number: u64) -> &PublicKey {
        self.former
            .iter()
            .find(|former| (former.first()..=former.last()).contains(&number))
            .map_or(&self.current, FormerKey::key)
    }

    /// Whether the key for the number of the entry signed it.
    pub fn signed(&self, entry: &RecordEntry) -> bool {
        &entry.hub_key() == self.at(entry.number)
    }

    /// Whether two entries prove that the hub rewrote its record (ADR-026): both are of one
    /// repository and carry one number, their hashes differ, and the key of the hub for that
    /// number signed both.
    pub fn rewritten(&self, one: &RecordEntry, other: &RecordEntry) -> bool {
        one.number == other.number
            && one.update.payload.repository == other.update.payload.repository
            && one.hash() != other.hash()
            && self.signed(one)
            && self.signed(other)
    }
}

#[cfg(test)]
pub(crate) mod tests;
