//! The anchors of an operation log on a hub (ADR-026, STEMMA-300): the ID of a log, the anchor
//! that an actor signs, the anchor as a hub holds it with its confirmation, the request that
//! reads the anchors of a log, and the reasons for which an anchor is not confirmed.
//!
//! # The ID of a log
//!
//! A log belongs to one `.stemma`, not to one actor: one actor can have two clones with the same
//! key, and several actors write into one log. `init` and `clone` give a log a random ID of 128
//! bits ([`LogId`]), written as 32 lowercase hexadecimal digits; an existing repository gets it
//! when it is first needed. The hub keeps the anchors per repository and log ID.
//!
//! # An anchor
//!
//! A clone sends the number and the hash of the last entry of its log, and the anchor that it
//! last had confirmed, signed with the key of the actor as a proof of the type
//! [`PayloadKind::Anchor`] ([`Anchor`]):
//!
//! ```text
//! stemma-protocol 1
//! hub <address>                          the address under which the hub is called
//! repository <32 hex digits>             the ID of the repository on the hub
//! log <32 hex digits>                    the ID of the log
//! tip <number> <hash>                    the last entry of the log
//! confirmed <number> <hash>      or      confirmed -
//! key ssh-ed25519 <base64>
//! signature <SSHSIG signature on one line>
//! ```
//!
//! The payload is the lines from `hub` to `key`. It names the address of the hub, as a ref
//! update does, so a hub that poses as a mirror cannot pass the anchor on (ADR-026). The hub sees
//! no entry of the log and no secret: a number and a hash.
//!
//! # A held anchor and its confirmation
//!
//! A hub keeps every anchor that it receives, with its own lines and its own signature over the
//! hash of both, as a proof of the type [`PayloadKind::HeldAnchor`] ([`HeldAnchor`]):
//!
//! ```text
//! <the anchor, from its first line to its `signature` line>
//! number <n>                             the running number of the anchor in its log
//! time <seconds since the Unix epoch>    the time of the hub
//! register <commit>                      the tip of `main` whose register was used
//! record <n>                             the number of the last record entry, or 0
//! actor <human or agent> <id>            the identity that the register names
//! state confirmed            or          state contradicts
//! hub-signature <SSHSIG signature on one line>
//! ```
//!
//! A held anchor in the state `confirmed` is the confirmation of the anchor; the answer to an
//! anchor that the hub confirms is that held anchor. An anchor that contradicts the last
//! confirmed anchor of its log (the same number with another hash, or a smaller number) is kept
//! in the state `contradicts`, with the signature of the actor as evidence, and is answered with
//! the error [`AnchorRefusal::Contradicts`], which names the anchor that the hub holds. The list
//! is only appended to.
//!
//! # The order of the hub
//!
//! The `record` line places the confirmation in the order of the hub, as it places a held
//! statement: a log entry that a confirmed anchor covers stands before every record entry with a
//! larger number, and so before a revocation that such an entry brought to `main`
//! ([`stands_before`]).
//!
//! # The anchors of a log
//!
//! A reader asks for the anchors of one log ([`AnchorsRequest`]), and the answer is the held
//! anchors one after the other ([`read_held_anchors`]):
//!
//! ```text
//! stemma-protocol 1
//! anchors <32 hex digits>
//! ```
//!
//! # A refused anchor
//!
//! ```text
//! stemma-protocol 1
//! error anchor-refused
//! refused <reason> <facts>
//! ```

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use super::record::{HubAddress, SealError};
use super::{
    first_line, malformed, parse_commit, parse_key, parse_positive, Lines, Malformed, ParseError,
    RepositoryId, RepositoryIdParseError, VERSION,
};
use crate::object::{parse_seconds, CommitId, ObjectId};
use crate::oplog::EntryHash;
use crate::register::{signature_from_line, signature_to_line, Identity};
use crate::signature::{
    self, PayloadKind, PublicKey, Signature, Signer, VerifyError, FORMAT_VERSION,
};

/// The text of a value that is not there.
const NONE: &str = "-";

/// The first line of the text that the hash of a held anchor is computed over, so that the
/// hash cannot pass for the hash of another kind of text.
const HASH_HEADER: &str = "stemma-held-anchor-hash 1";

/// The keyword of the last line of a held anchor.
const LAST_LINE: &[u8] = b"\nhub-signature ";

/// The ID of an operation log: 128 random bits, written as 32 lowercase hexadecimal digits
/// (ADR-026). The file `log-id` of the metadata directory holds it (ADR-009).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogId([u8; 16]);

impl LogId {
    /// Wraps the 16 bytes of an ID.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Returns the 16 bytes of the ID.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// A new random ID from the random source of the operating system.
    ///
    /// # Errors
    ///
    /// The error of the random source.
    pub fn generate() -> std::io::Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(std::io::Error::from)?;
        Ok(Self(bytes))
    }
}

impl fmt::Display for LogId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for LogId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LogId({self})")
    }
}

impl FromStr for LogId {
    type Err = RepositoryIdParseError;

    /// Reads the canonical form only, by the rule of a repository ID: 32 lowercase hexadecimal
    /// digits.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse::<RepositoryId>().map(|id| Self(*id.as_bytes()))
    }
}

/// An entry of a log as an anchor names it: its number and its hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Point {
    /// The number of the entry.
    pub number: u64,
    /// The hash of the entry.
    pub hash: EntryHash,
}

impl fmt::Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.number, self.hash)
    }
}

/// What is wrong with an anchor, a held anchor, or a refused anchor beyond the shape of its
/// lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AnchorProblem {
    /// The address of the hub is not in canonical form.
    Address,
    /// The ID of a log is not 32 lowercase hexadecimal digits.
    LogId,
    /// The hash of an entry is not in canonical form.
    Hash,
    /// The anchor that the clone last had confirmed does not lie before its tip.
    Confirmed,
    /// A signature is not in its one-line form.
    SignatureForm,
    /// The signature of the actor names another key than the `key` line.
    ActorKey,
    /// The signature of the actor does not verify.
    ActorSignature,
    /// A time is not seconds since the Unix epoch in canonical form.
    Time,
    /// An actor is not `human <id>` or `agent <id>` in canonical form.
    Actor,
    /// The state is not `confirmed` or `contradicts`.
    State,
    /// The signature of the hub does not verify for the key that it names.
    HubSignature,
    /// A reason of a refused anchor is not known to this version, or its facts do not fit it.
    Refusal,
}

impl fmt::Display for AnchorProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Address => "the address of the hub is not in canonical form",
            Self::LogId => "the ID of the log is not 32 lowercase hexadecimal digits",
            Self::Hash => "the hash of the entry is not in canonical form",
            Self::Confirmed => "the confirmed anchor does not lie before the tip",
            Self::SignatureForm => "the signature is not in its one-line form",
            Self::ActorKey => "the signature of the actor names another key than the `key` line",
            Self::ActorSignature => "the signature of the actor does not verify",
            Self::Time => "the time is not seconds since the Unix epoch in canonical form",
            Self::Actor => "the actor is not `human <id>` or `agent <id>` in canonical form",
            Self::State => "the state is not `confirmed` or `contradicts`",
            Self::HubSignature => "the signature of the hub does not verify",
            Self::Refusal => "the reason of the refused anchor is not known or does not fit",
        })
    }
}

impl Error for AnchorProblem {}

fn problem(line: usize, reason: AnchorProblem) -> ParseError {
    ParseError::Anchor { line, reason }
}

/// Reads the ID of a log.
fn parse_log(line: usize, text: &str) -> Result<LogId, ParseError> {
    text.parse()
        .map_err(|_| problem(line, AnchorProblem::LogId))
}

/// Reads `<number> <hash>`.
fn parse_point(line: usize, text: &str) -> Result<Point, ParseError> {
    let (number, hash) = text
        .split_once(' ')
        .ok_or(malformed(line, Malformed::Fields))?;
    let number = parse_positive(number).ok_or(malformed(line, Malformed::Number))?;
    let hash = hash
        .parse()
        .map_err(|_| problem(line, AnchorProblem::Hash))?;
    Ok(Point { number, hash })
}

/// Reads a count from 0: `0`, or a number from 1 without a leading zero.
fn parse_count(text: &str) -> Option<u64> {
    if text == "0" {
        Some(0)
    } else {
        parse_positive(text)
    }
}

/// What an actor signs for an anchor (ADR-026): the hub and the repository it is meant for, the
/// log, its last entry, the anchor that the clone last had confirmed, and the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorPayload {
    hub: HubAddress,
    repository: RepositoryId,
    log: LogId,
    tip: Point,
    confirmed: Option<Point>,
    key: PublicKey,
}

impl AnchorPayload {
    /// The payload of an anchor of the entry `tip` of the log `log`, for the repository
    /// `repository` on the hub at `hub`, after the anchor `confirmed` that the hub confirmed
    /// last, signed by `key`.
    ///
    /// # Errors
    ///
    /// [`AnchorProblem::Confirmed`] if `confirmed` lies after `tip`, or names its number with
    /// another hash: a log grows, so the confirmed entry stands at or before the tip.
    pub fn try_new(
        hub: HubAddress,
        repository: RepositoryId,
        log: LogId,
        tip: Point,
        confirmed: Option<Point>,
        key: PublicKey,
    ) -> Result<Self, AnchorProblem> {
        if let Some(confirmed) = confirmed {
            if confirmed.number > tip.number
                || (confirmed.number == tip.number && confirmed.hash != tip.hash)
            {
                return Err(AnchorProblem::Confirmed);
            }
        }
        Ok(Self {
            hub,
            repository,
            log,
            tip,
            confirmed,
            key,
        })
    }

    /// The address of the hub that the anchor is meant for.
    pub fn hub(&self) -> &HubAddress {
        &self.hub
    }

    /// The ID of the repository that the anchor is meant for.
    pub fn repository(&self) -> RepositoryId {
        self.repository
    }

    /// The ID of the log.
    pub fn log(&self) -> LogId {
        self.log
    }

    /// The last entry of the log.
    pub fn tip(&self) -> Point {
        self.tip
    }

    /// The anchor that the clone last had confirmed, or `None` before the first confirmation.
    pub fn confirmed(&self) -> Option<Point> {
        self.confirmed
    }

    /// The key that signs the anchor. Which actor it speaks for is decided by the register.
    pub fn key(&self) -> &PublicKey {
        &self.key
    }

    /// The bytes that the actor signs, as a proof of the type [`PayloadKind::Anchor`]: the
    /// lines of the message from `hub` to `key`.
    pub fn encode(&self) -> Vec<u8> {
        let confirmed = self
            .confirmed
            .map_or_else(|| NONE.to_owned(), |point| point.to_string());
        format!(
            "hub {}\nrepository {}\nlog {}\ntip {}\nconfirmed {confirmed}\nkey {}\n",
            self.hub, self.repository, self.log, self.tip, self.key
        )
        .into_bytes()
    }

    /// Signs the payload and returns the anchor.
    ///
    /// # Errors
    ///
    /// [`SealError::Sign`] with the error of the signer, and [`SealError::Verify`] if the
    /// signature does not verify for the key of the payload.
    pub fn sign(self, signer: &dyn Signer) -> Result<Anchor, SealError> {
        let signature = signature::sign(signer, PayloadKind::Anchor, &self.encode())
            .map_err(SealError::Sign)?;
        Anchor::try_new(self, signature).map_err(SealError::Verify)
    }
}

/// An anchor (ADR-026): a payload and the signature of its key over it. Every value carries a
/// signature that verifies. Whether the key stands in a register is decided by the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    payload: AnchorPayload,
    signature: Signature,
}

impl Anchor {
    /// Joins a payload and its signature.
    ///
    /// # Errors
    ///
    /// The error of [`signature::verify`] if the signature is not one of the key of the payload
    /// over the payload, as a proof of the type [`PayloadKind::Anchor`].
    pub fn try_new(payload: AnchorPayload, signature: Signature) -> Result<Self, VerifyError> {
        signature::verify(
            &payload.key,
            PayloadKind::Anchor,
            FORMAT_VERSION,
            &payload.encode(),
            &signature,
        )?;
        Ok(Self { payload, signature })
    }

    /// The payload.
    pub fn payload(&self) -> &AnchorPayload {
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
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a first line that is missing or malformed,
    /// another protocol version, a line that is missing, repeated, unknown, or out of order, a
    /// last line without a line feed, an address, a repository ID, a log ID, a number, a hash,
    /// or a key that is not in canonical form, a confirmed anchor that does not lie before the
    /// tip, and a signature that is not in its one-line form. A message whose form is right is
    /// refused last for its signature, at the `signature` line.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let mut lines = Lines::open(bytes)?;
        let (anchor, line) = read_anchor(&mut lines)?;
        lines.end()?;
        verified(anchor, line)
    }

    /// Reads the form without checking the signature, for the fuzz target.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn parse_unverified(bytes: &[u8]) -> Result<Self, ParseError> {
        let mut lines = Lines::open(bytes)?;
        let (anchor, _) = read_anchor(&mut lines)?;
        lines.end()?;
        Ok(anchor)
    }
}

/// The anchor, if its signature verifies: refused at `line` otherwise.
fn verified(anchor: Anchor, line: usize) -> Result<Anchor, ParseError> {
    Anchor::try_new(anchor.payload, anchor.signature).map_err(|error| {
        problem(
            line,
            match error {
                VerifyError::WrongKey => AnchorProblem::ActorKey,
                _ => AnchorProblem::ActorSignature,
            },
        )
    })
}

/// Reads the lines of an anchor after the first line, from `hub` to `signature`, without the
/// check of the signature. Returns the line of the signature.
fn read_anchor(lines: &mut Lines<'_>) -> Result<(Anchor, usize), ParseError> {
    let (line, value) = lines.require("hub")?;
    let hub = HubAddress::try_new(value).map_err(|_| problem(line, AnchorProblem::Address))?;
    let (line, value) = lines.require("repository")?;
    let repository = value
        .parse()
        .map_err(|source| ParseError::InvalidRepositoryId { line, source })?;
    let (line, value) = lines.require("log")?;
    let log = parse_log(line, value)?;
    let (line, value) = lines.require("tip")?;
    let tip = parse_point(line, value)?;
    let (confirmed_line, value) = lines.require("confirmed")?;
    let confirmed = if value == NONE {
        None
    } else {
        Some(parse_point(confirmed_line, value)?)
    };
    let (line, value) = lines.require("key")?;
    let key = parse_key(line, value)?;
    let (signature_line, value) = lines.require("signature")?;
    let signature =
        signature_from_line(value).ok_or(problem(signature_line, AnchorProblem::SignatureForm))?;
    let payload = AnchorPayload::try_new(hub, repository, log, tip, confirmed, key)
        .map_err(|reason| problem(confirmed_line, reason))?;
    Ok((Anchor { payload, signature }, signature_line))
}

/// What a hub says about an anchor that it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnchorState {
    /// The hub confirms the anchor: it continues the last confirmed anchor of its log.
    Confirmed,
    /// The anchor contradicts the last confirmed anchor of its log: the same number with
    /// another hash, or a smaller number. The hub keeps it as evidence and does not confirm it.
    Contradicts,
}

impl AnchorState {
    const fn name(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Contradicts => "contradicts",
        }
    }
}

/// What a hub states about an anchor that it holds (ADR-026): its place in the list of its log
/// and in the order of the hub, who the register says signed it, and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorLines {
    /// The running number of the anchor in the list of its log, from 1.
    pub number: u64,
    /// The time of the hub, in seconds since the Unix epoch (UTC).
    pub time: i64,
    /// The tip of `main` whose register the hub used.
    pub register: CommitId,
    /// The number of the last record entry of the repository when the anchor came, 0 for none.
    pub record: u64,
    /// The identity of the actor from the register. A statement of the hub and no proof.
    pub actor: Identity,
    /// Whether the hub confirms the anchor.
    pub state: AnchorState,
}

/// An anchor as a hub holds it (ADR-026): the anchor as it was sent, the lines of the hub, and
/// the signature of the hub over the hash of both. In the state [`AnchorState::Confirmed`] it is
/// the confirmation of the anchor.
///
/// Every value carries two signatures that verify: that of the actor for the key of the anchor,
/// and that of the hub for the key that its signature names ([`hub_key`](Self::hub_key)).
/// Whether that is the key of the hub is for the caller to say, against the key that it pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldAnchor {
    anchor: Anchor,
    lines: AnchorLines,
    signature: Signature,
}

impl HeldAnchor {
    /// Signs the lines of the hub for `anchor` with the key of the hub.
    ///
    /// # Errors
    ///
    /// [`SealError::Sign`] with the error of the signer, and [`SealError::Verify`] if the
    /// signature that it returns does not verify for the key that it named.
    pub fn seal(
        anchor: Anchor,
        lines: AnchorLines,
        signer: &dyn Signer,
    ) -> Result<Self, SealError> {
        let text = hash_text(&hashed(&anchor, &lines));
        let signature = signature::sign(signer, PayloadKind::HeldAnchor, text.as_bytes())
            .map_err(SealError::Sign)?;
        let key = signer.public_key().map_err(SealError::Sign)?;
        if signature.signing_key() != key {
            return Err(SealError::Verify(VerifyError::WrongKey));
        }
        verify_hub(&text, &signature).map_err(SealError::Verify)?;
        Ok(Self {
            anchor,
            lines,
            signature,
        })
    }

    /// The anchor, as it was sent.
    pub fn anchor(&self) -> &Anchor {
        &self.anchor
    }

    /// The ID of the log of the anchor.
    pub fn log(&self) -> LogId {
        self.anchor.payload.log
    }

    /// The entry that the anchor names.
    pub fn tip(&self) -> Point {
        self.anchor.payload.tip
    }

    /// The lines of the hub.
    pub fn lines(&self) -> &AnchorLines {
        &self.lines
    }

    /// The running number of the anchor in the list of its log.
    pub fn number(&self) -> u64 {
        self.lines.number
    }

    /// The time of the hub.
    pub fn time(&self) -> i64 {
        self.lines.time
    }

    /// The number of the last record entry when the anchor came.
    pub fn record(&self) -> u64 {
        self.lines.record
    }

    /// The state that the hub gave the anchor.
    pub fn state(&self) -> AnchorState {
        self.lines.state
    }

    /// Whether the hub confirmed the anchor.
    pub fn is_confirmed(&self) -> bool {
        self.lines.state == AnchorState::Confirmed
    }

    /// The signature of the hub.
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    /// The key that signed the lines of the hub, as the signature names it.
    pub fn hub_key(&self) -> PublicKey {
        self.signature.signing_key()
    }

    /// Writes the held anchor.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = hashed(&self.anchor, &self.lines);
        bytes.extend_from_slice(
            format!("hub-signature {}\n", signature_to_line(&self.signature)).as_bytes(),
        );
        bytes
    }

    /// Reads one held anchor, as [`encode`](Self::encode) writes it, and nothing else, and checks
    /// both signatures.
    ///
    /// # Errors
    ///
    /// Everything that [`Anchor::parse`] refuses about the anchor, then a line of the hub that
    /// is missing, repeated, unknown, or out of order, a number, a time, a commit, an actor, or
    /// a state that is not in canonical form, a signature that is not in its one-line form,
    /// anything after the `hub-signature` line, then the signature of the actor, and last a
    /// signature of the hub that does not verify ([`AnchorProblem::HubSignature`]).
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let (held, actor_line, hub_line) = read_form(bytes)?;
        let anchor = verified(held.anchor, actor_line)?;
        let held = Self { anchor, ..held };
        verify_hub(
            &hash_text(&hashed(&held.anchor, &held.lines)),
            &held.signature,
        )
        .map_err(|_| problem(hub_line, AnchorProblem::HubSignature))?;
        Ok(held)
    }

    /// Reads the form without checking the signatures, for the fuzz target.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn parse_unverified(bytes: &[u8]) -> Result<Self, ParseError> {
        read_form(bytes).map(|(held, _, _)| held)
    }
}

/// The bytes that the hash of a held anchor covers: the anchor and the lines of the hub.
fn hashed(anchor: &Anchor, lines: &AnchorLines) -> Vec<u8> {
    let mut bytes = anchor.encode();
    bytes.extend_from_slice(
        format!(
            "number {}\ntime {}\nregister {}\nrecord {}\nactor {}\nstate {}\n",
            lines.number,
            lines.time,
            lines.register,
            lines.record,
            lines.actor,
            lines.state.name()
        )
        .as_bytes(),
    );
    bytes
}

/// The text of the hash of the bytes before the `hub-signature` line, which the hub signs.
fn hash_text(hashed: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(HASH_HEADER.as_bytes());
    hasher.update(b"\n");
    hasher.update(hashed);
    ObjectId::from_digest(hasher.finalize()).to_string()
}

fn verify_hub(text: &str, signature: &Signature) -> Result<(), VerifyError> {
    signature::verify(
        &signature.signing_key(),
        PayloadKind::HeldAnchor,
        FORMAT_VERSION,
        text.as_bytes(),
        signature,
    )
}

/// Reads the form of one held anchor, which is all of `bytes`, without checking a signature.
/// Returns the lines of the signature of the actor and of the hub.
fn read_form(bytes: &[u8]) -> Result<(HeldAnchor, usize, usize), ParseError> {
    let mut lines = Lines::open(bytes)?;
    let (anchor, actor_line) = read_anchor(&mut lines)?;
    let (line, value) = lines.require("number")?;
    let number = parse_positive(value).ok_or(malformed(line, Malformed::Number))?;
    let (line, value) = lines.require("time")?;
    let time = parse_seconds(value).map_err(|_| problem(line, AnchorProblem::Time))?;
    let (line, value) = lines.require("register")?;
    let register = parse_commit(line, value)?;
    let (line, value) = lines.require("record")?;
    let record = parse_count(value).ok_or(malformed(line, Malformed::Number))?;
    let (line, value) = lines.require("actor")?;
    let actor = value
        .parse::<Identity>()
        .ok()
        .filter(|actor| actor.to_string() == value)
        .ok_or(problem(line, AnchorProblem::Actor))?;
    let (line, value) = lines.require("state")?;
    let state = match value {
        "confirmed" => AnchorState::Confirmed,
        "contradicts" => AnchorState::Contradicts,
        _ => return Err(problem(line, AnchorProblem::State)),
    };
    let (hub_line, value) = lines.require("hub-signature")?;
    let signature =
        signature_from_line(value).ok_or(problem(hub_line, AnchorProblem::SignatureForm))?;
    lines.end()?;
    let held = HeldAnchor {
        anchor,
        lines: AnchorLines {
            number,
            time,
            register,
            record,
            actor,
            state,
        },
        signature,
    };
    Ok((held, actor_line, hub_line))
}

/// Why a held anchor of a list is refused by [`read_held_anchors`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldAnchorParseError {
    /// The position of the held anchor in the input, from 1.
    pub position: usize,
    /// Why it is refused. Its lines count from the first line of that held anchor.
    pub error: ParseError,
}

impl fmt::Display for HeldAnchorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "held anchor {}: {}", self.position, self.error)
    }
}

impl Error for HeldAnchorParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message includes the message of the cause (see the crate documentation).
        self.error.source()
    }
}

/// Reads held anchors that were written one after the other: zero or more, each as
/// [`HeldAnchor::parse`] reads it. An empty input is an empty list. Whether they belong to one
/// log and their numbers follow one another is not asked here.
///
/// # Errors
///
/// [`HeldAnchorParseError`] with the position of the first held anchor that
/// [`HeldAnchor::parse`] refuses, which includes one that is cut off.
pub fn read_held_anchors(bytes: &[u8]) -> Result<Vec<HeldAnchor>, HeldAnchorParseError> {
    let mut held = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let len = line_end(rest, LAST_LINE).unwrap_or(rest.len());
        let (first, after) = rest.split_at(len);
        let one = HeldAnchor::parse(first).map_err(|error| HeldAnchorParseError {
            position: held.len() + 1,
            error,
        })?;
        held.push(one);
        rest = after;
    }
    Ok(held)
}

/// The end of the first line that begins with `keyword` (a line feed and a word with its
/// space), after its line feed, or `None` if no such line is there whole.
fn line_end(bytes: &[u8], keyword: &[u8]) -> Option<usize> {
    let start = bytes
        .windows(keyword.len())
        .position(|window| window == keyword)?;
    let value = start + keyword.len();
    bytes[value..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map(|end| value + end + 1)
}

/// The request that reads the anchors of one log of a repository (ADR-026).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnchorsRequest {
    log: LogId,
}

impl AnchorsRequest {
    /// The request for the anchors of `log`.
    pub fn new(log: LogId) -> Self {
        Self { log }
    }

    /// The ID of the log.
    pub fn log(&self) -> LogId {
        self.log
    }

    /// Writes the request.
    pub fn encode(&self) -> Vec<u8> {
        format!("{}anchors {}\n", first_line(VERSION), self.log).into_bytes()
    }

    /// Reads the request, as [`encode`](Self::encode) writes it, and nothing else.
    ///
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a first line that is missing or malformed,
    /// another protocol version, a line that is missing, unknown, or repeated, and a log ID that
    /// is not in canonical form.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let mut lines = Lines::open(bytes)?;
        let (line, value) = lines.require("anchors")?;
        let log = parse_log(line, value)?;
        lines.end()?;
        Ok(Self { log })
    }
}

/// Why a hub does not confirm an anchor (ADR-026).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AnchorRefusal {
    /// The anchor names another address than the one under which the hub was called. Carries
    /// the address that was called.
    Hub {
        /// The address of the hub.
        called: HubAddress,
    },
    /// The anchor names another repository than the one it was sent to. Carries the ID of the
    /// repository that it was sent to.
    Repository {
        /// The ID of the repository on the hub.
        id: RepositoryId,
    },
    /// The repository has no `main`, so there is no register to judge the anchor by.
    NoMain,
    /// The register at the tip of `main` does not hold the key of the anchor as an active key.
    /// Carries the commit whose register was read.
    Key {
        /// The tip of `main`.
        register: CommitId,
    },
    /// The anchor contradicts the last confirmed anchor of its log: the same number with
    /// another hash, or a smaller number. The hub keeps it as evidence. Carries the entry that
    /// the confirmed anchor names.
    Contradicts {
        /// The entry of the last confirmed anchor of the log.
        held: Point,
    },
}

impl AnchorRefusal {
    /// The word of the reason and its facts, as the `refused` line carries them.
    fn encode(&self) -> String {
        match self {
            Self::Hub { called } => format!("hub {called}"),
            Self::Repository { id } => format!("repository {id}"),
            Self::NoMain => format!("no-main {NONE}"),
            Self::Key { register } => format!("key {register}"),
            Self::Contradicts { held } => format!("contradicts {held}"),
        }
    }

    /// Reads the word of a reason and its facts.
    fn parse(line: usize, word: &str, facts: &str) -> Result<Self, ParseError> {
        match word {
            "hub" => Ok(Self::Hub {
                called: HubAddress::try_new(facts)
                    .map_err(|_| problem(line, AnchorProblem::Address))?,
            }),
            "repository" => Ok(Self::Repository {
                id: facts
                    .parse()
                    .map_err(|source| ParseError::InvalidRepositoryId { line, source })?,
            }),
            "no-main" if facts == NONE => Ok(Self::NoMain),
            "no-main" => Err(malformed(line, Malformed::Fields)),
            "key" => Ok(Self::Key {
                register: parse_commit(line, facts)?,
            }),
            "contradicts" => Ok(Self::Contradicts {
                held: parse_point(line, facts)?,
            }),
            _ => Err(problem(line, AnchorProblem::Refusal)),
        }
    }
}

impl fmt::Display for AnchorRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the anchor is not confirmed: ")?;
        match self {
            Self::Hub { called } => write!(f, "it names another hub than {called}"),
            Self::Repository { id } => write!(
                f,
                "it names another repository than {id}, which it was sent to"
            ),
            Self::NoMain => f.write_str("the repository has no `main`"),
            Self::Key { register } => write!(
                f,
                "the register of `main` at {register} does not hold its key as an active key"
            ),
            Self::Contradicts { held } => write!(
                f,
                "it contradicts the confirmed anchor of entry {} with the hash {}, which the hub \
                 holds; the hub keeps it as evidence",
                held.number, held.hash
            ),
        }
    }
}

impl Error for AnchorRefusal {}

/// The line of an `error anchor-refused` after the line of its kind.
pub(super) fn encode_refusal(refusal: &AnchorRefusal) -> String {
    format!("refused {}\n", refusal.encode())
}

/// Reads the line of an `error anchor-refused` after the line of its kind, to the end of the
/// message.
pub(super) fn read_refusal(lines: &mut Lines<'_>) -> Result<AnchorRefusal, ParseError> {
    let (line, value) = lines.require("refused")?;
    let (word, facts) = value
        .split_once(' ')
        .ok_or(malformed(line, Malformed::Fields))?;
    let refusal = AnchorRefusal::parse(line, word, facts)?;
    lines.end()?;
    Ok(refusal)
}

/// Whether the entry `entry` of the log `log` of another clone stands before the record entry
/// `record` in the order of the hub (ADR-026): a confirmed anchor of that log covers it (its tip
/// is the entry or a later one) and its confirmation names a smaller record number. Otherwise
/// the entry counts as after it. A revocation acts across clones from the record entry that
/// moves `main` to the commit that holds it, so an entry of another clone counts before the
/// revocation only by this rule. `held` are the anchors that the hub holds, checked by the
/// caller against the key of the hub and against the log, whose entries they must hold with
/// their hashes ([`Repository::verify_anchors`](crate::repository::Repository::verify_anchors)):
/// the function trusts the number of the tip. Anchors of other logs and contradicting ones count
/// for nothing.
pub fn stands_before(held: &[HeldAnchor], log: LogId, entry: u64, record: u64) -> bool {
    held.iter().any(|anchor| {
        anchor.is_confirmed()
            && anchor.log() == log
            && anchor.tip().number >= entry
            && anchor.record() < record
    })
}

#[cfg(test)]
pub(crate) mod tests;
