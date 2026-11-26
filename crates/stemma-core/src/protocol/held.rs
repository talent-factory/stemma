//! The statements that a hub holds (ADR-025): a signed statement with the lines of the hub, the
//! request that reads a range of the statements or of the record, and the reasons for which a
//! submitted statement is not accepted.
//!
//! A clone submits a statement ([`Statement`]) as it is. A hub that accepts it keeps it with its
//! own lines and its own signature, and the answer to the submission is that held statement
//! ([`HeldStatement`]):
//!
//! ```text
//! <the statement, from its first line to its `signature` line>
//! number <n>                                       the running number of the statement
//! time <seconds since the Unix epoch>              the time of the hub
//! register <commit>                                the tip of `main` whose register was used
//! record <n>                                       the number of the last record entry
//! actor <human or agent> <id>                      the identity that the register names
//! hub-signature <SSHSIG signature on one line>
//! ```
//!
//! The hash of a held statement covers everything before the `hub-signature` line, the
//! statement with its signature and the lines of the hub; the hub signs the text of that hash as
//! a proof of the type [`PayloadKind::HeldStatement`], as it signs a record entry. The actor
//! signs one part, the statement, and the lines of the hub are a statement of the hub alone.
//!
//! The held statements of a repository are written one after the other into a destination of
//! the hub and read back with [`read_held`], and they are answered so to a reader: the answer to
//! a range is the held statements of the range one after the other, as the answer to a range of
//! the record is its entries one after the other ([`read_record`](super::record::read_record)).
//!
//! # The order of the hub
//!
//! The `record` line places a statement in the order of the hub (ADR-025): the statement came
//! after the record entry of that number and before the next one. It lies before an entry whose
//! number is larger than its `record` line ([`HeldStatement::lies_before`]), so before a
//! retirement or a revocation that this entry brought to `main`.
//!
//! # A range
//!
//! A reader asks for the statements or for the record by numbers ([`Range`]):
//!
//! ```text
//! stemma-protocol 1
//! range <first number> <last number, or `-` for the end>
//! ```
//!
//! # A refused statement
//!
//! A statement that the rules of the writer refuse is answered with an error of its own kind
//! ([`ErrorMessage::StatementRefused`](super::ErrorMessage::StatementRefused)), one line with the
//! reason ([`StatementRefusal`]):
//!
//! ```text
//! stemma-protocol 1
//! error statement-refused
//! refused <reason> <facts>
//! ```

use std::error::Error;
use std::fmt;

use super::record::{SealError, StatementHash};
use super::statement::{Statement, StatementKind};
use super::{
    first_line, is_reason, malformed, parse_commit, parse_positive, Lines, Malformed, ParseError,
    Reason, RepositoryId, VERSION,
};
use crate::object::{parse_seconds, CommitId, ObjectId};
use crate::oplog::OperationName;
use crate::register::{signature_from_line, signature_to_line, Identity};
use crate::signature::{
    self, PayloadKind, PublicKey, Signature, Signer, VerifyError, FORMAT_VERSION,
};

/// The text of a value that is not there.
const NONE: &str = "-";

/// The first line of the text that the hash of a held statement is computed over, so that the
/// hash cannot pass for the hash of another kind of text.
const HASH_HEADER: &str = "stemma-held-hash 1";

/// The keyword of the last line of a held statement and of a record entry.
const LAST_LINE: &[u8] = b"\nhub-signature ";

/// What is wrong with a held statement or a range beyond the shape of its lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HeldProblem {
    /// A time is not seconds since the Unix epoch in canonical form.
    Time,
    /// An actor is not `human <id>` or `agent <id>` in canonical form.
    Actor,
    /// A signature of the hub is not in its one-line form.
    SignatureForm,
    /// The signature of the hub does not verify for the key that it names.
    HubSignature,
    /// The last number of a range lies before its first.
    Range,
    /// A reason of a refused statement is not known to this version, or its facts do not fit
    /// it.
    Refusal,
}

impl fmt::Display for HeldProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Time => "the time is not seconds since the Unix epoch in canonical form",
            Self::Actor => "the actor is not `human <id>` or `agent <id>` in canonical form",
            Self::SignatureForm => "the signature of the hub is not in its one-line form",
            Self::HubSignature => "the signature of the hub does not verify",
            Self::Range => "the last number of the range lies before its first",
            Self::Refusal => "the reason of the refused statement is not known or does not fit",
        })
    }
}

impl Error for HeldProblem {}

fn problem(line: usize, reason: HeldProblem) -> ParseError {
    ParseError::Held { line, reason }
}

/// What a hub states about a statement that it accepts (ADR-025): its place in the order of
/// the hub, and who the register says signed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubLines {
    /// The running number of the statement in its repository, from 1.
    pub number: u64,
    /// The time of the hub, in seconds since the Unix epoch (UTC).
    pub time: i64,
    /// The tip of `main` whose register the hub used.
    pub register: CommitId,
    /// The number of the last record entry of the repository when the statement was accepted.
    pub record: u64,
    /// The identity of the actor from the register. It is a statement of the hub and no proof.
    pub actor: Identity,
}

/// A statement as a hub holds it (ADR-025): the statement as it was submitted, the lines of the
/// hub, and the signature of the hub over the hash of both.
///
/// Every value carries two signatures that verify, where the statement is signed: that of the
/// actor for the key of the statement, and that of the hub for the key that its signature names
/// ([`hub_key`](Self::hub_key)). Whether that is the key of the hub is for the caller to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldStatement {
    statement: Statement,
    lines: HubLines,
    signature: Signature,
}

impl HeldStatement {
    /// Signs the lines of the hub for `statement` with the key of the hub.
    ///
    /// # Errors
    ///
    /// [`SealError::Sign`] with the error of the signer, and [`SealError::Verify`] if the
    /// signature that it returns does not verify for the key that it named.
    pub fn seal(
        statement: Statement,
        lines: HubLines,
        signer: &dyn Signer,
    ) -> Result<Self, SealError> {
        let hashed = hashed(&statement, &lines);
        let text = hash_text(&hashed);
        let signature = signature::sign(signer, PayloadKind::HeldStatement, text.as_bytes())
            .map_err(SealError::Sign)?;
        let key = signer.public_key().map_err(SealError::Sign)?;
        if signature.signing_key() != key {
            return Err(SealError::Verify(VerifyError::WrongKey));
        }
        verify_hub(&text, &signature).map_err(SealError::Verify)?;
        Ok(Self {
            statement,
            lines,
            signature,
        })
    }

    /// The statement, as it was submitted.
    pub fn statement(&self) -> &Statement {
        &self.statement
    }

    /// The hash of the statement, which a clone keeps it under.
    pub fn hash(&self) -> StatementHash {
        self.statement.hash()
    }

    /// The lines of the hub.
    pub fn lines(&self) -> &HubLines {
        &self.lines
    }

    /// The running number of the statement in its repository, from 1.
    pub fn number(&self) -> u64 {
        self.lines.number
    }

    /// The time of the hub, in seconds since the Unix epoch (UTC).
    pub fn time(&self) -> i64 {
        self.lines.time
    }

    /// The tip of `main` whose register the hub used. A claim, as the register commit of a
    /// record entry is.
    pub fn register(&self) -> CommitId {
        self.lines.register
    }

    /// The number of the last record entry when the statement was accepted.
    pub fn record(&self) -> u64 {
        self.lines.record
    }

    /// The identity of the actor, as the hub read it from the register.
    pub fn actor(&self) -> &Identity {
        &self.lines.actor
    }

    /// Whether the statement lies before the record entry `number` in the order of the hub: it
    /// was accepted before that entry was written, so before a retirement or a revocation that
    /// the entry brought to `main` (ADR-025).
    pub fn lies_before(&self, number: u64) -> bool {
        self.lines.record < number
    }

    /// The signature of the hub.
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    /// The key that signed the lines of the hub, as the signature names it.
    pub fn hub_key(&self) -> PublicKey {
        self.signature.signing_key()
    }

    /// Writes the held statement.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = hashed(&self.statement, &self.lines);
        bytes.extend_from_slice(
            format!("hub-signature {}\n", signature_to_line(&self.signature)).as_bytes(),
        );
        bytes
    }

    /// Reads one held statement, as [`encode`](Self::encode) writes it, and nothing else, and
    /// checks both signatures.
    ///
    /// # Errors
    ///
    /// Everything that [`Statement::parse`] refuses about the statement, its signature
    /// included, then a line of the hub that is missing, repeated, unknown, or out of order, a
    /// number that is not a decimal integer from 1 without a leading zero, a time, a commit, or
    /// an actor that is not in canonical form, a signature that is not in its one-line form,
    /// anything after the `hub-signature` line, and last a signature of the hub that does not
    /// verify ([`HeldProblem::HubSignature`]).
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        // The form of the whole message is judged first, then the signature of the actor, then
        // the one of the hub, as the reader of a record entry does.
        let (held, hub_line) = read_form(bytes)?;
        Statement::parse(&bytes[..statement_len(bytes)])?;
        verify_hub(
            &hash_text(&hashed(&held.statement, &held.lines)),
            &held.signature,
        )
        .map_err(|_| problem(hub_line, HeldProblem::HubSignature))?;
        Ok(held)
    }

    /// Reads the form without checking the signatures, for the fuzz target.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn parse_unverified(bytes: &[u8]) -> Result<Self, ParseError> {
        read_form(bytes).map(|(held, _)| held)
    }
}

/// The bytes that the hash of a held statement covers: the statement and the lines of the hub.
fn hashed(statement: &Statement, lines: &HubLines) -> Vec<u8> {
    let mut bytes = statement.encode();
    bytes.extend_from_slice(
        format!(
            "number {}\ntime {}\nregister {}\nrecord {}\nactor {}\n",
            lines.number, lines.time, lines.register, lines.record, lines.actor
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
        PayloadKind::HeldStatement,
        FORMAT_VERSION,
        text.as_bytes(),
        signature,
    )
}

/// The length of the statement at the start of a held statement: up to the line feed of its
/// `signature` line, or all of `bytes` if there is none. Only the last line of a statement
/// begins with that keyword, and no value holds a line feed.
fn statement_len(bytes: &[u8]) -> usize {
    line_end(bytes, b"\nsignature ").unwrap_or(bytes.len())
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

/// Reads the form of one held statement, which is all of `bytes`, without checking a
/// signature. Returns the line of the signature of the hub.
fn read_form(bytes: &[u8]) -> Result<(HeldStatement, usize), ParseError> {
    let (first, rest) = bytes.split_at(statement_len(bytes));
    let statement = Statement::parse_form(first)?;
    let text = std::str::from_utf8(rest).map_err(|error| ParseError::NotUtf8 {
        valid_up_to: first.len() + error.valid_up_to(),
    })?;
    let before = first.iter().filter(|&&byte| byte == b'\n').count();
    let mut lines = Lines {
        rest: text,
        line: before + 1,
    };
    let (line, value) = lines.require("number")?;
    let number: u64 = parse_positive(value).ok_or(malformed(line, Malformed::Number))?;
    let (line, value) = lines.require("time")?;
    let time = parse_seconds(value).map_err(|_| problem(line, HeldProblem::Time))?;
    let (line, value) = lines.require("register")?;
    let register = parse_commit(line, value)?;
    let (line, value) = lines.require("record")?;
    let record: u64 = parse_positive(value).ok_or(malformed(line, Malformed::Number))?;
    let (line, value) = lines.require("actor")?;
    let actor = value
        .parse::<Identity>()
        .ok()
        .filter(|actor| actor.to_string() == value)
        .ok_or(problem(line, HeldProblem::Actor))?;
    let (hub_line, value) = lines.require("hub-signature")?;
    let signature =
        signature_from_line(value).ok_or(problem(hub_line, HeldProblem::SignatureForm))?;
    lines.end()?;
    let held = HeldStatement {
        statement,
        lines: HubLines {
            number,
            time,
            register,
            record,
            actor,
        },
        signature,
    };
    Ok((held, hub_line))
}

/// Why a held statement of a list is refused by [`read_held`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldParseError {
    /// The position of the held statement in the input, from 1.
    pub position: usize,
    /// Why it is refused. Its lines count from the first line of that held statement.
    pub error: ParseError,
}

impl fmt::Display for HeldParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "held statement {}: {}", self.position, self.error)
    }
}

impl Error for HeldParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message includes the message of the cause, so the source is the cause of the cause
        // (see the crate documentation).
        self.error.source()
    }
}

/// Reads held statements that were written one after the other: zero or more, each as
/// [`HeldStatement::parse`] reads it. An empty input is an empty list. That the numbers follow
/// one another is not asked here.
///
/// # Errors
///
/// [`HeldParseError`] with the position of the first held statement that
/// [`HeldStatement::parse`] refuses, which includes one that is cut off.
pub fn read_held(bytes: &[u8]) -> Result<Vec<HeldStatement>, HeldParseError> {
    let mut held = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let len = line_end(rest, LAST_LINE).unwrap_or(rest.len());
        let (first, after) = rest.split_at(len);
        let one = HeldStatement::parse(first).map_err(|error| HeldParseError {
            position: held.len() + 1,
            error,
        })?;
        held.push(one);
        rest = after;
    }
    Ok(held)
}

/// A range of numbers, by which a reader asks for the held statements or the entries of the
/// record of a repository (ADR-025): from the first number to the last, both included, or to
/// the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Range {
    first: u64,
    last: Option<u64>,
}

impl Range {
    /// The range from `first` to `last`, or to the end for `None`. Numbers count from 1.
    ///
    /// # Errors
    ///
    /// [`HeldProblem::Range`] if `first` is 0 or `last` lies before `first`.
    pub fn try_new(first: u64, last: Option<u64>) -> Result<Self, HeldProblem> {
        if first == 0 || last.is_some_and(|last| last < first) {
            return Err(HeldProblem::Range);
        }
        Ok(Self { first, last })
    }

    /// Everything, from number 1 to the end.
    pub fn all() -> Self {
        Self {
            first: 1,
            last: None,
        }
    }

    /// The first number.
    pub fn first(&self) -> u64 {
        self.first
    }

    /// The last number, or `None` for the end.
    pub fn last(&self) -> Option<u64> {
        self.last
    }

    /// Whether `number` lies in the range.
    pub fn contains(&self, number: u64) -> bool {
        number >= self.first && self.last.is_none_or(|last| number <= last)
    }

    /// Writes the request.
    pub fn encode(&self) -> Vec<u8> {
        let last = self
            .last
            .map_or_else(|| NONE.to_owned(), |last| last.to_string());
        format!("{}range {} {last}\n", first_line(VERSION), self.first).into_bytes()
    }

    /// Reads the request, as [`encode`](Self::encode) writes it, and nothing else.
    ///
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a first line that is missing or malformed,
    /// another protocol version, a line that is missing, unknown, or repeated, a number that is
    /// not a decimal integer from 1 without a leading zero, and a last number that lies before
    /// the first.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let mut lines = Lines::open(bytes)?;
        let (line, value) = lines.require("range")?;
        let (first, last) = value
            .split_once(' ')
            .ok_or(malformed(line, Malformed::Fields))?;
        let first: u64 = parse_positive(first).ok_or(malformed(line, Malformed::Number))?;
        let last = if last == NONE {
            None
        } else {
            Some(parse_positive::<u64>(last).ok_or(malformed(line, Malformed::Number))?)
        };
        lines.end()?;
        Self::try_new(first, last).map_err(|reason| problem(line, reason))
    }
}

/// Why a hub does not accept a submitted statement (ADR-025): the rules of the local writer of
/// phase 4, asked against the register at the tip of `main` and the state of the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StatementRefusal {
    /// The statement names another repository than the one it was sent to, or none. Carries
    /// the ID of the repository that it was sent to.
    Repository {
        /// The ID of the repository on the hub.
        id: RepositoryId,
    },
    /// The statement carries no signature. A hub holds a register, and no register accepts the
    /// form of the open mode.
    Unsigned,
    /// The repository has no `main`, so there is no register to judge the statement by.
    NoMain,
    /// The register at the tip of `main` does not hold the key of the statement as an active
    /// key. Carries the commit whose register was read.
    Key {
        /// The tip of `main`.
        register: CommitId,
    },
    /// The hub does not accept statements of this kind yet.
    Kind(StatementKind),
    /// The authorizer refused the operation. Carries its name.
    Denied(OperationName),
    /// The opener is not the write owner of the branch, or the lease has ended.
    NotWriteOwner,
    /// The branch of the opening is not at its head on the hub, or the head of the pull request
    /// that an approval names is not the approved one. Carries the tip of the branch on the
    /// hub, or `None` if the branch does not exist.
    Head(Option<CommitId>),
    /// The branch has an open pull request already. Carries its name, the hash of its opening.
    AlreadyOpen(StatementHash),
    /// A reviewer of the opening has no entry in the register. Carries the identity.
    UnknownReviewer(Identity),
    /// The opener is one of the reviewers of the opening.
    SelfReview,
    /// The pull request that the statement names is not known to the hub.
    NoPull,
    /// The pull request that the statement names is closed or merged.
    NotOpen,
    /// The approver opened the pull request or wrote a commit that it brings (rule 4 of
    /// ADR-004).
    SelfApproval,
    /// The closer is neither the opener nor an owner.
    NotCloser,
    /// A commit that the statement names is reached by no branch and no tag of the hub. Carries
    /// the first such commit.
    Unreachable(CommitId),
    /// The policy at the tip of `main`, which names the duration of the lease, cannot be used.
    /// Carries what its reader reported.
    Policy(Reason),
}

impl StatementRefusal {
    /// The word of the reason and its facts, as the `refused` line carries them.
    fn encode(&self) -> String {
        match self {
            Self::Repository { id } => format!("repository {id}"),
            Self::Unsigned => format!("unsigned {NONE}"),
            Self::NoMain => format!("no-main {NONE}"),
            Self::Key { register } => format!("key {register}"),
            Self::Kind(kind) => format!("kind {kind}"),
            Self::Denied(operation) => format!("denied {operation}"),
            Self::NotWriteOwner => format!("not-write-owner {NONE}"),
            Self::Head(tip) => format!(
                "head {}",
                tip.map_or_else(|| NONE.to_owned(), |tip| tip.to_string())
            ),
            Self::AlreadyOpen(pull) => format!("already-open {pull}"),
            Self::UnknownReviewer(identity) => format!("unknown-reviewer {identity}"),
            Self::SelfReview => format!("self-review {NONE}"),
            Self::NoPull => format!("no-pull {NONE}"),
            Self::NotOpen => format!("not-open {NONE}"),
            Self::SelfApproval => format!("self-approval {NONE}"),
            Self::NotCloser => format!("not-closer {NONE}"),
            Self::Unreachable(commit) => format!("unreachable {commit}"),
            Self::Policy(reason) => format!("policy {reason}"),
        }
    }

    /// Reads the word of a reason and its facts.
    fn parse(line: usize, word: &str, facts: &str) -> Result<Self, ParseError> {
        let none = |reason: Self| {
            if facts == NONE {
                Ok(reason)
            } else {
                Err(malformed(line, Malformed::Fields))
            }
        };
        let refusal = || problem(line, HeldProblem::Refusal);
        match word {
            "repository" => Ok(Self::Repository {
                id: facts
                    .parse()
                    .map_err(|source| ParseError::InvalidRepositoryId { line, source })?,
            }),
            "unsigned" => none(Self::Unsigned),
            "no-main" => none(Self::NoMain),
            "key" => Ok(Self::Key {
                register: parse_commit(line, facts)?,
            }),
            "kind" => StatementKind::from_name(facts)
                .map(Self::Kind)
                .ok_or_else(refusal),
            "denied" => OperationName::try_new(facts)
                .ok()
                .filter(|operation| operation.to_string() == facts)
                .map(Self::Denied)
                .ok_or_else(refusal),
            "not-write-owner" => none(Self::NotWriteOwner),
            "head" if facts == NONE => Ok(Self::Head(None)),
            "head" => Ok(Self::Head(Some(parse_commit(line, facts)?))),
            "already-open" => facts.parse().map(Self::AlreadyOpen).map_err(|_| refusal()),
            "unknown-reviewer" => facts
                .parse::<Identity>()
                .ok()
                .filter(|identity| identity.to_string() == facts)
                .map(Self::UnknownReviewer)
                .ok_or_else(refusal),
            "self-review" => none(Self::SelfReview),
            "no-pull" => none(Self::NoPull),
            "not-open" => none(Self::NotOpen),
            "self-approval" => none(Self::SelfApproval),
            "not-closer" => none(Self::NotCloser),
            "unreachable" => Ok(Self::Unreachable(parse_commit(line, facts)?)),
            "policy" if is_reason(facts) => Ok(Self::Policy(Reason(facts.to_owned()))),
            "policy" => Err(malformed(line, Malformed::Reason)),
            _ => Err(refusal()),
        }
    }
}

impl fmt::Display for StatementRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the statement is refused: ")?;
        match self {
            Self::Repository { id } => write!(
                f,
                "it names another repository than {id}, which it was sent to"
            ),
            Self::Unsigned => f.write_str("it carries no signature"),
            Self::NoMain => f.write_str("the repository has no `main`"),
            Self::Key { register } => write!(
                f,
                "the register of `main` at {register} does not hold its key as an active key"
            ),
            Self::Kind(kind) => write!(f, "the hub does not accept a statement `{kind}` yet"),
            Self::Denied(operation) => write!(f, "the operation `{operation}` is not allowed"),
            Self::NotWriteOwner => f.write_str("the opener is not the write owner of the branch"),
            Self::Head(None) => f.write_str("the branch does not exist on the hub"),
            Self::Head(Some(tip)) => write!(f, "the branch is at {tip} on the hub"),
            Self::AlreadyOpen(pull) => write!(f, "the branch has the open pull request {pull}"),
            Self::UnknownReviewer(identity) => {
                write!(f, "the reviewer {identity} is not in the register")
            }
            Self::SelfReview => f.write_str("the opener is one of the reviewers"),
            Self::NoPull => f.write_str("the hub knows no such pull request"),
            Self::NotOpen => f.write_str("the pull request is closed or merged"),
            Self::SelfApproval => {
                f.write_str("the approver opened the pull request or wrote a commit that it brings")
            }
            Self::NotCloser => f.write_str("the closer is neither the opener nor an owner"),
            Self::Unreachable(commit) => {
                write!(f, "no branch and no tag of the hub reaches {commit}")
            }
            Self::Policy(reason) => {
                write!(
                    f,
                    "the policy at the tip of `main` cannot be used: {reason}"
                )
            }
        }
    }
}

impl Error for StatementRefusal {}

/// The line of an `error statement-refused` after the line of its kind.
pub(super) fn encode_refusal(refusal: &StatementRefusal) -> String {
    format!("refused {}\n", refusal.encode())
}

/// Reads the line of an `error statement-refused` after the line of its kind, to the end of the
/// message.
pub(super) fn read_refusal(lines: &mut Lines<'_>) -> Result<StatementRefusal, ParseError> {
    let (line, value) = lines.require("refused")?;
    let (word, facts) = value
        .split_once(' ')
        .ok_or(malformed(line, Malformed::Fields))?;
    let refusal = StatementRefusal::parse(line, word, facts)?;
    lines.end()?;
    Ok(refusal)
}

#[cfg(test)]
pub(crate) mod tests;
