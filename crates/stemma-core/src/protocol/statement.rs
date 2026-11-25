//! The signed statements (ADR-025): the portable form of the evidence about a pull request.
//!
//! A statement says one thing about a repository: a pull request is opened
//! ([`Opening`]), its head is approved ([`Approval`]), a check ran with a result ([`Checked`]),
//! a pull request is closed ([`Closing`]), or an owner overrides the write ownership of a branch
//! ([`OwnerOverride`]). It binds the repository, the pull request, and the commits, and no
//! number, chain, or time of a clone, so a clone and a hub read the same text.
//!
//! A statement is a message of the protocol. Its lines from `repository` to `key` are the
//! payload that its key signs ([`Statement::payload`]), as a proof whose type tag is the one of
//! its kind ([`StatementKind::payload_kind`]):
//!
//! ```text
//! stemma-protocol 1
//! statement <kind>
//! repository <32 lowercase hexadecimal digits, or `-`>
//! <the lines of the kind>
//! key ssh-ed25519 <base64>                         or `key -`
//! signature <SSHSIG signature on one line>         or `signature -`
//! ```
//!
//! The lines of the five kinds:
//!
//! ```text
//! pr-opening       branch <name>
//!                  commit <head>
//!                  reviewer <human or agent> <id>   zero or more, in ascending order
//!                  repairs <commit, or `-`>
//! pr-approval      pull <hash of the opening>
//!                  commit <head>
//! check-result     commit <commit>
//!                  base <commit, or `-`>
//!                  result <pass or fail>
//! pr-closing       pull <hash of the opening>
//! owner-override   branch <name>
//!                  holder <human or agent> <id>
//!                  commit <commit, or `-`>
//!                  reason <one line>
//! ```
//!
//! `repository` is the ID that a hub gave the repository (ADR-027). A statement that is made
//! without a remote names `-` there and counts locally only.
//!
//! The line `statement` is no part of the payload. The type tag of the signed bytes stands for
//! it: a statement whose kind was changed carries a signature of another type, which does not
//! verify.
//!
//! The hash of a statement ([`Statement::hash`]) covers the whole message, the signature
//! included. The hash of an opening names its pull request: nobody assigns the name, and it
//! holds in every clone and on a hub.
//!
//! # A statement without a signature
//!
//! A repository in the open mode has no register and no key (ADR-003), and its evidence has the
//! same form: a statement with `key -` and `signature -` ([`Statement::unsigned`]). Either both
//! lines name a value or neither does. An unsigned statement proves nothing. It counts only
//! where the verdict is reached without a register, and whoever holds a register refuses it,
//! a hub among them.
//!
//! # What a reader checks
//!
//! The reader holds the rule of ADR-005, and it checks the signature of a signed statement
//! against the key that the statement names, so a value of [`Statement`] that carries a
//! signature carries one that verifies. Whether the key is the right one is asked apart, against
//! a register ([`Register::verify_statement`](crate::register::Register::verify_statement)):
//! a statement names no actor, the actor follows from the key and the register.
//!
//! Not part of this module: the purge statement of ADR-025, the lines that a hub adds to a
//! statement it keeps, and the request that submits one. They come with the slices that build
//! them.

use std::error::Error;
use std::fmt;

use super::record::SealError;
use super::{
    first_line, malformed, parse_commit, parse_key, Lines, Malformed, ParseError, RepositoryId,
    VERSION,
};
use crate::object::CommitId;
use crate::oplog::{CheckResult, MAX_REASON_LEN, MAX_REVIEWERS};
use crate::pull::TARGET;
use crate::refs::BranchName;
use crate::register::{signature_from_line, signature_to_line, Identity};
use crate::signature::{
    self, PayloadKind, PublicKey, Signature, Signer, VerifyError, FORMAT_VERSION,
};

pub use super::record::StatementHash;

/// The text of a value that is not there: no repository of a hub, no repaired commit, no base,
/// no key, and no signature.
const NONE: &str = "-";

/// The first line of the text that the hash of a statement is computed over, so that the hash of
/// a statement cannot pass for the hash of another kind of text.
const HASH_HEADER: &str = "stemma-statement-hash 1";

/// Writes a value, or `-` where there is none.
fn or_none<T: fmt::Display>(value: &Option<T>) -> String {
    value.as_ref().map_or_else(|| NONE.to_owned(), T::to_string)
}

/// What a statement says.
///
/// The enum is exhaustive on purpose: a new kind of statement has to be handled by every
/// reader, which the compiler then shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatementKind {
    /// A pull request is opened ([`Opening`]).
    Opening,
    /// The head of a pull request is approved ([`Approval`]).
    Approval,
    /// A check ran with a result ([`Checked`]).
    Check,
    /// A pull request is closed ([`Closing`]).
    Closing,
    /// An owner overrides the write ownership of a branch ([`OwnerOverride`]).
    Override,
}

impl StatementKind {
    /// Every kind.
    pub const ALL: [Self; 5] = [
        Self::Opening,
        Self::Approval,
        Self::Check,
        Self::Closing,
        Self::Override,
    ];

    /// The type tag of the proof that signs a statement of this kind.
    pub const fn payload_kind(self) -> PayloadKind {
        match self {
            Self::Opening => PayloadKind::PullOpening,
            Self::Approval => PayloadKind::PullApproval,
            Self::Check => PayloadKind::CheckResult,
            Self::Closing => PayloadKind::PullClosing,
            Self::Override => PayloadKind::OwnerOverride,
        }
    }

    /// The word of the kind in the line `statement`. It is the name of the type tag.
    pub const fn name(self) -> &'static str {
        self.payload_kind().name()
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

impl fmt::Display for StatementKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What is wrong with a statement beyond the shape of its lines. The constructors refuse a
/// value by it, and the reader refuses a line by it ([`ParseError::Statement`]), so both sides
/// name a broken rule with the same word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StatementProblem {
    /// The line `statement` names no kind of this version.
    Kind,
    /// A branch name is refused.
    Branch,
    /// The branch of an opening is `main`, the target of every pull request.
    TargetBranch,
    /// A reviewer or a holder is not `human <id>` or `agent <id>` with a valid identifier.
    Identity,
    /// An opening names more than [`MAX_REVIEWERS`] reviewers.
    TooManyReviewers,
    /// The hash of an opening is not in canonical form.
    Pull,
    /// The base of a check is the commit that was tested: a run merged onto itself says nothing.
    BaseIsCommit,
    /// The result of a check is not `pass` or `fail`.
    Result,
    /// The reason of an override is empty, longer than [`MAX_REASON_LEN`] bytes, has a control
    /// character or a line or paragraph separator, or begins or ends with a space.
    Reason,
    /// A signature is not `-` or an SSHSIG signature of an Ed25519 key for the namespace of the
    /// proofs, written on one line.
    SignatureForm,
    /// The statement names a key and no signature, or a signature and no key.
    HalfSigned,
    /// The signature names another key than the `key` line: another key signed, or the `key`
    /// line was changed.
    Key,
    /// The signature names the key of the statement and does not verify for it and the payload:
    /// the payload or the kind was changed, or the signature is not one of a statement.
    Signature,
}

impl fmt::Display for StatementProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kind => f.write_str("unknown kind of statement"),
            Self::Branch => f.write_str("the branch name is not valid"),
            Self::TargetBranch => write!(
                f,
                "`{TARGET}` is the target of every pull request and cannot be its branch"
            ),
            Self::Identity => {
                f.write_str("expected `human <id>` or `agent <id>` with a valid identifier")
            }
            Self::TooManyReviewers => {
                write!(f, "an opening names at most {MAX_REVIEWERS} reviewers")
            }
            Self::Pull => f.write_str("the hash of the opening is not in canonical form"),
            Self::BaseIsCommit => f.write_str("the base of a check cannot be the commit itself"),
            Self::Result => f.write_str("the result is not `pass` or `fail`"),
            Self::Reason => write!(
                f,
                "the reason is one line of 1 to {MAX_REASON_LEN} bytes, without a control \
                 character and without a space at its ends"
            ),
            Self::SignatureForm => {
                f.write_str("the signature is not `-` or an SSHSIG signature on one line")
            }
            Self::HalfSigned => {
                f.write_str("a statement names a key and a signature, or neither of them")
            }
            Self::Key => f.write_str("the signature names another key than the `key` line"),
            Self::Signature => {
                f.write_str("the signature does not verify for the key and the statement")
            }
        }
    }
}

impl Error for StatementProblem {}

/// The opening of a pull request (ADR-025, ADR-004 rule 3): `branch` with `head` at its tip is
/// to be merged into `main`, the listed identities review it, and it may declare the red commit
/// that it repairs (ADR-019). The hash of the signed opening names the pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opening {
    repository: Option<RepositoryId>,
    branch: BranchName,
    head: CommitId,
    reviewers: Vec<Identity>,
    repairs: Option<CommitId>,
}

impl Opening {
    /// An opening. The reviewers are sorted by their text, and a reviewer that is named twice
    /// stands once.
    ///
    /// # Errors
    ///
    /// [`StatementProblem::TargetBranch`] if the branch is `main`, and
    /// [`StatementProblem::TooManyReviewers`] for more than [`MAX_REVIEWERS`] reviewers.
    pub fn try_new(
        repository: Option<RepositoryId>,
        branch: BranchName,
        head: CommitId,
        mut reviewers: Vec<Identity>,
        repairs: Option<CommitId>,
    ) -> Result<Self, StatementProblem> {
        if branch.as_str() == TARGET {
            return Err(StatementProblem::TargetBranch);
        }
        reviewers.sort_by_cached_key(ToString::to_string);
        reviewers.dedup();
        if reviewers.len() > MAX_REVIEWERS {
            return Err(StatementProblem::TooManyReviewers);
        }
        Ok(Self {
            repository,
            branch,
            head,
            reviewers,
            repairs,
        })
    }

    /// The branch that the pull request would merge into `main`.
    pub fn branch(&self) -> &BranchName {
        &self.branch
    }

    /// The commit at the tip of the branch when the pull request was opened.
    pub fn head(&self) -> CommitId {
        self.head
    }

    /// The reviewers, in the ascending order of their text, each once.
    pub fn reviewers(&self) -> &[Identity] {
        &self.reviewers
    }

    /// The red commit that the opening declares to repair.
    pub fn repairs(&self) -> Option<CommitId> {
        self.repairs
    }
}

/// The approval of a pull request (ADR-025, ADR-004 rule 4): the key approves `head` as the
/// head of the pull request that the hash `pull` names. A commit on the branch makes another
/// head, for which the approval does not stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    repository: Option<RepositoryId>,
    pull: StatementHash,
    head: CommitId,
}

impl Approval {
    /// An approval of `head` for the pull request `pull`.
    pub fn new(repository: Option<RepositoryId>, pull: StatementHash, head: CommitId) -> Self {
        Self {
            repository,
            pull,
            head,
        }
    }

    /// The pull request: the hash of its opening.
    pub fn pull(&self) -> StatementHash {
        self.pull
    }

    /// The head that was reviewed.
    pub fn head(&self) -> CommitId {
        self.head
    }
}

/// The result of a check (ADR-025, ADR-019): `commit` was tested, merged onto `base` (a tip of
/// `main`), or as it is where there is no base, and the run passed or failed. Nobody proves the
/// run: the result is the statement of the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    repository: Option<RepositoryId>,
    commit: CommitId,
    base: Option<CommitId>,
    result: CheckResult,
}

impl Checked {
    /// The result of a run.
    ///
    /// # Errors
    ///
    /// [`StatementProblem::BaseIsCommit`] if `base` is `commit`.
    pub fn try_new(
        repository: Option<RepositoryId>,
        commit: CommitId,
        base: Option<CommitId>,
        result: CheckResult,
    ) -> Result<Self, StatementProblem> {
        if base == Some(commit) {
            return Err(StatementProblem::BaseIsCommit);
        }
        Ok(Self {
            repository,
            commit,
            base,
            result,
        })
    }

    /// The commit that was tested.
    pub fn commit(&self) -> CommitId {
        self.commit
    }

    /// The tip of `main` that the commit was merged onto for the test, or `None` for a run on
    /// the commit as it is.
    pub fn base(&self) -> Option<CommitId> {
        self.base
    }

    /// The result.
    pub fn result(&self) -> CheckResult {
        self.result
    }
}

/// The closing of a pull request (ADR-025): the pull request that the hash `pull` names ends
/// without a merge. Who may close is a rule of whoever accepts the statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closing {
    repository: Option<RepositoryId>,
    pull: StatementHash,
}

impl Closing {
    /// A closing of the pull request `pull`.
    pub fn new(repository: Option<RepositoryId>, pull: StatementHash) -> Self {
        Self { repository, pull }
    }

    /// The pull request: the hash of its opening.
    pub fn pull(&self) -> StatementHash {
        self.pull
    }
}

/// The override of the write ownership of a branch by an owner (ADR-025, rule 9 of ADR-004):
/// the key takes `branch` from `holder` while it stands at `commit` (or does not exist), for a
/// reason that every reader of the repository sees. The commit binds the statement to one state
/// of the branch, so it does not stand for a later one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerOverride {
    repository: Option<RepositoryId>,
    branch: BranchName,
    holder: Identity,
    commit: Option<CommitId>,
    reason: String,
}

impl OwnerOverride {
    /// An override.
    ///
    /// # Errors
    ///
    /// [`StatementProblem::Reason`] unless the reason is one line of 1 to [`MAX_REASON_LEN`]
    /// bytes without a control character, without a line or paragraph separator, and without a
    /// space at its ends.
    pub fn try_new(
        repository: Option<RepositoryId>,
        branch: BranchName,
        holder: Identity,
        commit: Option<CommitId>,
        reason: impl Into<String>,
    ) -> Result<Self, StatementProblem> {
        let reason = reason.into();
        if !is_reason(&reason) {
            return Err(StatementProblem::Reason);
        }
        Ok(Self {
            repository,
            branch,
            holder,
            commit,
            reason,
        })
    }

    /// The branch.
    pub fn branch(&self) -> &BranchName {
        &self.branch
    }

    /// The holder whose write ownership is overridden.
    pub fn holder(&self) -> &Identity {
        &self.holder
    }

    /// The commit at the tip of the branch, or `None` for a branch that does not exist.
    pub fn commit(&self) -> Option<CommitId> {
        self.commit
    }

    /// The reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Whether `text` is the reason of an override: the rule of the entry `lease-override`
/// (ADR-011), and no separator of lines or paragraphs either, as the text travels.
fn is_reason(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_REASON_LEN
        && !text.starts_with(' ')
        && !text.ends_with(' ')
        && !text
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
}

/// What a statement says, without its key and its signature.
///
/// The enum is exhaustive on purpose, as [`StatementKind`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// A pull request is opened.
    Opening(Opening),
    /// The head of a pull request is approved.
    Approval(Approval),
    /// A check ran with a result.
    Check(Checked),
    /// A pull request is closed.
    Closing(Closing),
    /// An owner overrides the write ownership of a branch.
    Override(OwnerOverride),
}

impl Body {
    /// The kind.
    pub fn kind(&self) -> StatementKind {
        match self {
            Self::Opening(_) => StatementKind::Opening,
            Self::Approval(_) => StatementKind::Approval,
            Self::Check(_) => StatementKind::Check,
            Self::Closing(_) => StatementKind::Closing,
            Self::Override(_) => StatementKind::Override,
        }
    }

    /// The repository of a hub that the statement is about, or `None` for a statement that
    /// counts locally only.
    pub fn repository(&self) -> Option<RepositoryId> {
        match self {
            Self::Opening(body) => body.repository,
            Self::Approval(body) => body.repository,
            Self::Check(body) => body.repository,
            Self::Closing(body) => body.repository,
            Self::Override(body) => body.repository,
        }
    }

    /// Every commit that the statement names.
    pub fn commits(&self) -> Vec<CommitId> {
        match self {
            Self::Opening(body) => std::iter::once(body.head).chain(body.repairs).collect(),
            Self::Approval(body) => vec![body.head],
            Self::Check(body) => std::iter::once(body.commit).chain(body.base).collect(),
            Self::Closing(_) => Vec::new(),
            Self::Override(body) => body.commit.into_iter().collect(),
        }
    }

    /// The lines of the kind, between `repository` and `key`.
    fn lines(&self) -> String {
        match self {
            Self::Opening(body) => {
                let mut text = format!("branch {}\ncommit {}\n", body.branch, body.head);
                for reviewer in &body.reviewers {
                    text.push_str(&format!("reviewer {reviewer}\n"));
                }
                text.push_str(&format!("repairs {}\n", or_none(&body.repairs)));
                text
            }
            Self::Approval(body) => format!("pull {}\ncommit {}\n", body.pull, body.head),
            Self::Check(body) => format!(
                "commit {}\nbase {}\nresult {}\n",
                body.commit,
                or_none(&body.base),
                body.result.name()
            ),
            Self::Closing(body) => format!("pull {}\n", body.pull),
            Self::Override(body) => format!(
                "branch {}\nholder {}\ncommit {}\nreason {}\n",
                body.branch,
                body.holder,
                or_none(&body.commit),
                body.reason
            ),
        }
    }

    /// The payload for a key: the lines from `repository` to `key`.
    fn payload(&self, key: Option<&PublicKey>) -> Vec<u8> {
        format!(
            "repository {}\n{}key {}\n",
            or_none(&self.repository()),
            self.lines(),
            key.map_or_else(|| NONE.to_owned(), PublicKey::to_string)
        )
        .into_bytes()
    }
}

/// A statement (ADR-025): what it says, and the key with its signature, or neither in the open
/// mode. A value that carries a signature carries one that verifies for its key and its
/// payload, whether it was built here or read from a message. It says nothing about the actor:
/// whether the key is in a register is asked apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    body: Body,
    proof: Option<(PublicKey, Signature)>,
}

impl Statement {
    /// A statement without a key and a signature, the form of the open mode. It proves nothing
    /// and counts only where no register is asked.
    pub fn unsigned(body: Body) -> Self {
        Self { body, proof: None }
    }

    /// Signs `body` with the key of `signer`.
    ///
    /// # Errors
    ///
    /// [`SealError::Sign`] with the error of the signer, for its key or its signature, and
    /// [`SealError::Verify`] if the signature that it returns does not verify for the key that
    /// it named.
    pub fn sign(body: Body, signer: &dyn Signer) -> Result<Self, SealError> {
        let key = signer.public_key().map_err(SealError::Sign)?;
        let payload = body.payload(Some(&key));
        let signature = signature::sign(signer, body.kind().payload_kind(), &payload)
            .map_err(SealError::Sign)?;
        Self::try_new(body, key, signature).map_err(SealError::Verify)
    }

    /// Joins what a statement says with a key and its signature.
    ///
    /// # Errors
    ///
    /// The error of [`signature::verify`] if the signature is not one of `key` over the payload,
    /// as a proof of the type of the kind.
    pub fn try_new(body: Body, key: PublicKey, signature: Signature) -> Result<Self, VerifyError> {
        signature::verify(
            &key,
            body.kind().payload_kind(),
            FORMAT_VERSION,
            &body.payload(Some(&key)),
            &signature,
        )?;
        Ok(Self {
            body,
            proof: Some((key, signature)),
        })
    }

    /// What the statement says.
    pub fn body(&self) -> &Body {
        &self.body
    }

    /// The kind.
    pub fn kind(&self) -> StatementKind {
        self.body.kind()
    }

    /// The key that signed, or `None` for an unsigned statement.
    pub fn key(&self) -> Option<&PublicKey> {
        self.proof.as_ref().map(|(key, _)| key)
    }

    /// The signature of the key, or `None` for an unsigned statement.
    pub fn signature(&self) -> Option<&Signature> {
        self.proof.as_ref().map(|(_, signature)| signature)
    }

    /// Whether the statement carries a key and its signature.
    pub fn is_signed(&self) -> bool {
        self.proof.is_some()
    }

    /// The bytes that the key signs, as a proof of the type of the kind: the lines of the
    /// message from `repository` to `key`.
    pub fn payload(&self) -> Vec<u8> {
        self.body.payload(self.key())
    }

    /// Writes the message.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = first_line(VERSION).into_bytes();
        bytes.extend_from_slice(format!("statement {}\n", self.kind()).as_bytes());
        bytes.extend_from_slice(&self.payload());
        let signature = self
            .signature()
            .map_or_else(|| NONE.to_owned(), signature_to_line);
        bytes.extend_from_slice(format!("signature {signature}\n").as_bytes());
        bytes
    }

    /// The hash of the statement: the BLAKE3 hash of the line `stemma-statement-hash 1` and the
    /// message, its signature included. The hash of an opening names its pull request.
    pub fn hash(&self) -> StatementHash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(HASH_HEADER.as_bytes());
        hasher.update(b"\n");
        hasher.update(&self.encode());
        StatementHash::from_digest(hasher.finalize())
    }

    /// Reads the message, as [`encode`](Self::encode) writes it, and nothing else, and checks
    /// the signature of a signed statement.
    ///
    /// Every commit is claimed to be one; no store has confirmed it.
    ///
    /// # Errors
    ///
    /// Returns an error for input that is not UTF-8, a first line that is missing or malformed,
    /// another protocol version, a kind that this version does not know, a line that is
    /// missing, repeated, unknown, or out of order, a last line without a line feed, a
    /// repository ID, a name, a hash, a commit, an identity, or a key that is not in canonical
    /// form, a reviewer that does not come after the one before it, a value that the constructor
    /// of the kind refuses, a signature that is not in its one-line form, and a statement that
    /// names a key without a signature or a signature without a key. The first problem is
    /// reported, with its line. A message whose form is right is refused last for its
    /// signature, at the `signature` line: as [`StatementProblem::Key`] if the signature names
    /// another key than the `key` line, and as [`StatementProblem::Signature`] if it names that
    /// key and does not verify.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let (statement, line) = read(bytes)?;
        if let Some((key, signature)) = &statement.proof {
            let verified = signature::verify(
                key,
                statement.kind().payload_kind(),
                FORMAT_VERSION,
                &statement.payload(),
                signature,
            );
            verified.map_err(|error| {
                let reason = match error {
                    VerifyError::WrongKey => StatementProblem::Key,
                    _ => StatementProblem::Signature,
                };
                problem(line, reason)
            })?;
        }
        Ok(statement)
    }

    /// Reads the form of the message without checking the signature, for the fuzz target: a
    /// fuzzer makes no signature, so the check of the signature would hide the reader from it.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn parse_unverified(bytes: &[u8]) -> Result<Self, ParseError> {
        read(bytes).map(|(statement, _)| statement)
    }
}

fn problem(line: usize, reason: StatementProblem) -> ParseError {
    ParseError::Statement { line, reason }
}

/// Reads a value that may be `-`.
fn optional<T>(
    text: &str,
    read: impl FnOnce(&str) -> Result<T, ParseError>,
) -> Result<Option<T>, ParseError> {
    if text == NONE {
        Ok(None)
    } else {
        read(text).map(Some)
    }
}

fn parse_branch(line: usize, text: &str) -> Result<BranchName, ParseError> {
    BranchName::try_new(text).map_err(|_| problem(line, StatementProblem::Branch))
}

fn parse_identity(line: usize, text: &str) -> Result<Identity, ParseError> {
    let identity: Identity = text
        .parse()
        .map_err(|_| problem(line, StatementProblem::Identity))?;
    // One identity has one text.
    if identity.to_string() != text {
        return Err(problem(line, StatementProblem::Identity));
    }
    Ok(identity)
}

fn parse_pull(line: usize, text: &str) -> Result<StatementHash, ParseError> {
    text.parse()
        .map_err(|_| problem(line, StatementProblem::Pull))
}

/// Reads the lines of a statement. The signature is not checked; its line is returned.
fn read(bytes: &[u8]) -> Result<(Statement, usize), ParseError> {
    let mut lines = Lines::open(bytes)?;
    let (line, value) = lines.require("statement")?;
    let kind = StatementKind::from_name(value).ok_or(problem(line, StatementProblem::Kind))?;
    let (line, value) = lines.require("repository")?;
    let repository = optional(value, |text| {
        text.parse()
            .map_err(|source| ParseError::InvalidRepositoryId { line, source })
    })?;
    let body = match kind {
        StatementKind::Opening => {
            let (branch_line, value) = lines.require("branch")?;
            let branch = parse_branch(branch_line, value)?;
            let (line, value) = lines.require("commit")?;
            let head = parse_commit(line, value)?;
            let mut reviewers: Vec<Identity> = Vec::new();
            while let Some((line, value)) = lines.take("reviewer")? {
                let reviewer = parse_identity(line, value)?;
                let ascending = reviewers
                    .last()
                    .is_none_or(|before| before.to_string() < reviewer.to_string());
                if !ascending {
                    return Err(malformed(line, Malformed::Order));
                }
                reviewers.push(reviewer);
            }
            let (repairs_line, value) = lines.require("repairs")?;
            let repairs = optional(value, |text| parse_commit(repairs_line, text))?;
            Opening::try_new(repository, branch, head, reviewers, repairs)
                .map(Body::Opening)
                .map_err(|reason| match reason {
                    StatementProblem::TargetBranch => problem(branch_line, reason),
                    _ => problem(repairs_line, reason),
                })?
        }
        StatementKind::Approval => {
            let (line, value) = lines.require("pull")?;
            let pull = parse_pull(line, value)?;
            let (line, value) = lines.require("commit")?;
            Body::Approval(Approval::new(repository, pull, parse_commit(line, value)?))
        }
        StatementKind::Check => {
            let (line, value) = lines.require("commit")?;
            let commit = parse_commit(line, value)?;
            let (line, value) = lines.require("base")?;
            let base = optional(value, |text| parse_commit(line, text))?;
            let checked_line = line;
            let (line, value) = lines.require("result")?;
            let result = match value {
                "pass" => CheckResult::Pass,
                "fail" => CheckResult::Fail,
                _ => return Err(problem(line, StatementProblem::Result)),
            };
            Checked::try_new(repository, commit, base, result)
                .map(Body::Check)
                .map_err(|reason| problem(checked_line, reason))?
        }
        StatementKind::Closing => {
            let (line, value) = lines.require("pull")?;
            Body::Closing(Closing::new(repository, parse_pull(line, value)?))
        }
        StatementKind::Override => {
            let (line, value) = lines.require("branch")?;
            let branch = parse_branch(line, value)?;
            let (line, value) = lines.require("holder")?;
            let holder = parse_identity(line, value)?;
            let (line, value) = lines.require("commit")?;
            let commit = optional(value, |text| parse_commit(line, text))?;
            let (line, value) = lines.require("reason")?;
            OwnerOverride::try_new(repository, branch, holder, commit, value)
                .map(Body::Override)
                .map_err(|reason| problem(line, reason))?
        }
    };
    let (line, value) = lines.require("key")?;
    let key = optional(value, |text| parse_key(line, text))?;
    let (line, value) = lines.require("signature")?;
    let signature = optional(value, |text| {
        signature_from_line(text).ok_or(problem(line, StatementProblem::SignatureForm))
    })?;
    lines.end()?;
    let proof = match (key, signature) {
        (Some(key), Some(signature)) => Some((key, signature)),
        (None, None) => None,
        _ => return Err(problem(line, StatementProblem::HalfSigned)),
    };
    Ok((Statement { body, proof }, line))
}

#[cfg(test)]
pub(crate) mod tests;
