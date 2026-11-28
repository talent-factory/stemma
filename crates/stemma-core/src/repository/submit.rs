//! The submission of statements to a hub and the reading of the statements that it holds, the
//! client side of the requests of STEMMA-299 (ADR-025, STEMMA-312).
//!
//! A statement counts in the clone that made it at once, with or without a hub. It reaches the
//! hub when it names the repository of the hub (`repository <id>`, see
//! [`Repository::open_pull_for`] and [`Repository::record_check_for`]) and is submitted: right
//! after it was made, or later, for a clone that was offline. A submission is safe to repeat,
//! because the hub answers a statement that it holds already with the lines that it holds it
//! with, so a later submission sends every statement of a pull request again and learns which
//! of them the hub holds.
//!
//! The answer to a submission is a held statement, whose signature of the hub is checked
//! against the keys of the hub that the clone trusts (ADR-026), or a typed refusal
//! ([`StatementRefusal`]). The statements of a hub are read by a range of numbers. Both calls
//! read the list of references first, so that the repository ID and the key of the hub are
//! checked as a fetch checks them, and a held statement counts as signed by the hub if its key
//! is the key of the hub at its time: the pinned key, or a former key that the list names for
//! the range of record numbers that its `record` line, or the entry after it, falls in.
//!
//! Nothing here writes into the clone: neither the log nor the set of statements changes. The
//! lines of the hub are not kept in the clone; the entry `fetch` that brings statements into the
//! set belongs to STEMMA-325.

use std::error::Error;
use std::fmt;
use std::io::{self, Read};

use super::fetch::read_ref_list;
use super::{FetchError, PullError, Repository};
use crate::evidence::Placed;
use crate::object::CommitId;
use crate::oplog::OperationName;
use crate::protocol::held::{read_held, HeldParseError, HeldStatement, Range, StatementRefusal};
use crate::protocol::record::HubKeys;
use crate::protocol::statement::{Body, Statement, StatementHash, StatementKind};
use crate::protocol::{ErrorMessage, ParseError, RepositoryId};
use crate::refs::RefError;
use crate::register::Register;
use crate::remote::{HubKeyError, RemoteConfig, RemoteError, RemoteName};
use crate::statements::{SetError, MAX_STATEMENT_LEN};
use crate::transport::{Answer, Body as AnswerBody, Transport, TransportError};
use crate::{Actor, Authorizer, Decision};

/// The operation that the authorizer is asked for before a statement is submitted, with the
/// target `remote <name>`.
pub const SUBMIT: &str = "submit";

/// The operation that the authorizer is asked for before the statements of a hub are read: the
/// reading is a fetch that writes nothing.
const READ: &str = "fetch";

/// The longest answer to a submission that is read: a statement and the lines of the hub.
const MAX_HELD_ANSWER: u64 = MAX_STATEMENT_LEN as u64 + (16 << 10);

/// The longest answer with the held statements of a repository that is read: 256 MiB, about
/// sixty thousand statements of the longest form, many more of the usual one.
pub const MAX_HELD_LIST: u64 = 256 << 20;

/// The longest error message that is read in place of an answer: 64 KiB.
const MAX_ERROR_MESSAGE: u64 = 64 << 10;

/// What the hub answered to one submitted statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The hub holds the statement, with these lines; a statement that it held already is
    /// answered with the lines of the first submission.
    Held(Box<HeldStatement>),
    /// The hub refused the statement, for this reason. The statement still counts in the
    /// clone.
    Refused(StatementRefusal),
}

/// One submitted statement and the answer of the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Submission {
    /// The hash of the statement.
    pub hash: StatementHash,
    /// Its kind.
    pub kind: StatementKind,
    /// What the hub answered.
    pub outcome: Outcome,
}

impl Submission {
    /// Whether the hub refused the statement, other than for an opening whose pull request
    /// ended on the hub. The hub answers an opening that it holds already with `not-open` once
    /// its pull request is closed or merged (ADR-025): it holds the opening, so a submission
    /// that sends the opening again to bring a later statement along has lost nothing.
    pub fn is_refused(&self) -> bool {
        match &self.outcome {
            Outcome::Held(_) => false,
            Outcome::Refused(StatementRefusal::NotOpen) => self.kind != StatementKind::Opening,
            Outcome::Refused(_) => true,
        }
    }
}

impl Repository {
    /// The statements of the pull request `number` that this clone counts, in the order of
    /// their entries (so a check recorded before the opening comes first): the opening, the
    /// checks of a head of the pull request (the head of the opening or one that an approval
    /// names), the approvals, and the closing. They name the repository that the opening
    /// names, or no repository for a pull request of this clone only
    /// ([`PullRequest::repository`](crate::pull::PullRequest::repository)). They are the statements that
    /// [`submit_statements`](Self::submit_statements) sends for the pull request. A closing or
    /// a failing run that stands in for a lost statement is no statement that a hub could check,
    /// and is not among them.
    ///
    /// # Errors
    ///
    /// [`SubmitError::Pull`] if the log or the set cannot be read,
    /// and [`SubmitError::NoSuchPull`] if no pull request has the number.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn statements_of_pull(
        &self,
        number: u64,
        register: Option<&Register>,
    ) -> Result<Vec<Statement>, SubmitError> {
        self.require_oplog("read the statements of a pull request")
            .map_err(SubmitError::Refs)?;
        let entries = self
            .oplog()
            .entries()
            .map_err(|error| SubmitError::Pull(PullError::Log(error)))?;
        let evidence = self
            .evidence_of(&entries, register)
            .map_err(|error| SubmitError::Pull(PullError::Statements(error)))?;
        let pull = evidence
            .pulls
            .iter()
            .find(|pull| pull.number() == number)
            .ok_or(SubmitError::NoSuchPull(number))?;
        let repository = pull.repository();
        let (placed, _) = self
            .placed(&entries, register)
            .map_err(|error| SubmitError::Pull(PullError::Statements(error)))?;
        let id = pull.id();
        let mut heads: Vec<CommitId> = vec![*pull.head()];
        for placed in &placed {
            if let Body::Approval(approval) = placed.statement().body() {
                if approval.pull() == id && !heads.contains(&approval.head()) {
                    heads.push(approval.head());
                }
            }
        }
        let belongs = |placed: &Placed| match placed.statement().body() {
            Body::Opening(_) => placed.hash() == id,
            Body::Approval(approval) => approval.pull() == id,
            Body::Closing(closing) => closing.pull() == id,
            Body::Check(check) => heads.contains(&check.commit()),
            Body::Override(_) => false,
        };
        let mut found: Vec<Statement> = Vec::new();
        for placed in &placed {
            let same = placed.statement().body().repository() == repository;
            let new = !found
                .iter()
                .any(|statement| statement.hash() == placed.hash());
            if !placed.is_stand_in() && same && belongs(placed) && new {
                found.push(placed.statement().clone());
            }
        }
        Ok(found)
    }

    /// Submits the statements `hashes` of the set, in this order, to the remote `remote`
    /// through `transport`, and returns the answer of the hub to each. The authorizer is asked
    /// for `submit` with the target `remote <name>`. Every statement must name the repository
    /// of the remote, which is checked for all of them before the first is sent. The list of
    /// references is read first, for the repository ID and the keys of the hub, as a fetch
    /// reads it. A held statement must be the submitted one and carry a signature of the key of
    /// the hub at its time: the pinned key, or a former key that the list names for its range.
    /// A remote whose configuration says `sign-in yes` is signed in on `transport` by the
    /// caller first.
    ///
    /// A refusal of the hub is an answer, not an error: the next statement is sent all the
    /// same, so that one refused approval does not keep back a closing.
    ///
    /// # Errors
    ///
    /// Before anything is sent: [`SubmitError::Refs`] in a bare repository,
    /// [`SubmitError::Denied`], [`SubmitError::Remote`], [`SubmitError::NoSuchRemote`],
    /// [`SubmitError::HubKey`] if the configuration pinned no key of the hub,
    /// [`SubmitError::Statements`] and [`SubmitError::Missing`] for a statement that the set
    /// does not give, and [`SubmitError::OtherRepository`] for a statement that names another
    /// repository or none.
    ///
    /// For a request: [`SubmitError::List`] for the list of references and
    /// [`SubmitError::HubKey`] for a list of another repository or with another key,
    /// [`SubmitError::Transport`], [`SubmitError::Body`],
    /// [`SubmitError::TooLarge`], [`SubmitError::Answer`] for an answer that the reader
    /// refuses, [`SubmitError::Refused`] for an error of the hub other than a refused
    /// statement, [`SubmitError::NotSubmitted`] for a held statement that is not the submitted
    /// one, and [`SubmitError::WrongHubKey`] for one that another key signed. If an answer came
    /// before, the error is [`SubmitError::After`] with the answers so far.
    pub fn submit_statements(
        &self,
        remote: &RemoteName,
        hashes: &[StatementHash],
        transport: &mut dyn Transport,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Vec<Submission>, SubmitError> {
        self.require_oplog("submit statements")
            .map_err(SubmitError::Refs)?;
        authorize(SUBMIT, remote, actor, authorizer)?;
        let config = self.config_of(remote)?;
        config.pinned_key().map_err(SubmitError::HubKey)?;
        let set = self.statement_set();
        let mut statements = Vec::with_capacity(hashes.len());
        for hash in hashes {
            let statement = set
                .get(hash)
                .map_err(SubmitError::Statements)?
                .ok_or(SubmitError::Missing(*hash))?;
            let named = statement.body().repository();
            if named != Some(config.repository) {
                return Err(SubmitError::OtherRepository {
                    hash: *hash,
                    named,
                    remote: config.repository,
                });
            }
            statements.push(statement);
        }
        // The list gives the former keys of the hub: a statement that the hub held before it
        // rotated its key comes back with the signature of the key of that time.
        let list = read_ref_list(transport).map_err(|error| SubmitError::List(Box::new(error)))?;
        let keys = config.hub_keys(&list).map_err(SubmitError::HubKey)?;
        let mut submitted = Vec::with_capacity(statements.len());
        for statement in &statements {
            match submit_one(statement, transport, &keys) {
                Ok(outcome) => submitted.push(Submission {
                    hash: statement.hash(),
                    kind: statement.kind(),
                    outcome,
                }),
                Err(error) if submitted.is_empty() => return Err(error),
                Err(error) => {
                    return Err(SubmitError::After {
                        submitted,
                        source: Box::new(error),
                    })
                }
            }
        }
        Ok(submitted)
    }

    /// Reads every statement that the hub of the remote `remote` holds, through `transport`, in
    /// the order of their numbers. The authorizer is asked for `fetch` with the target
    /// `remote <name>`, as the reading is a fetch that writes nothing. The list of references
    /// is read first: its repository ID must be the one of the configuration and its key the
    /// pinned one, and every held statement must name the repository and carry a signature of
    /// the key of the hub at its time, as for a submission.
    ///
    /// # Errors
    ///
    /// [`SubmitError::Refs`] in a bare repository, [`SubmitError::Denied`],
    /// [`SubmitError::Remote`], [`SubmitError::NoSuchRemote`], [`SubmitError::List`] for a
    /// failure of the list of references, [`SubmitError::HubKey`] for a key or a repository of
    /// the list that is not the one of the configuration or a key that is not pinned (checked
    /// once the list is read), the errors of a request as for
    /// [`submit_statements`](Self::submit_statements), [`SubmitError::Held`] for an answer that
    /// the reader refuses, [`SubmitError::NotOfRemote`] for a held statement of another
    /// repository, and [`SubmitError::WrongHubKey`].
    pub fn held_statements(
        &self,
        remote: &RemoteName,
        transport: &mut dyn Transport,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Vec<HeldStatement>, SubmitError> {
        self.require_oplog("read the statements of a hub")
            .map_err(SubmitError::Refs)?;
        authorize(READ, remote, actor, authorizer)?;
        let config = self.config_of(remote)?;
        let list = read_ref_list(transport).map_err(|error| SubmitError::List(Box::new(error)))?;
        let keys = config.hub_keys(&list).map_err(SubmitError::HubKey)?;
        let range = Range::all().encode();
        let answer = transport
            .statements(&mut range.as_slice())
            .map_err(SubmitError::Transport)?;
        let bytes = match answer {
            Answer::Message(body) => read_bounded(body, MAX_HELD_LIST)?,
            Answer::Error(body) => return Err(SubmitError::Refused(error_of(body)?)),
        };
        let held = read_held(&bytes).map_err(SubmitError::Held)?;
        for statement in &held {
            if statement.statement().body().repository() != Some(config.repository) {
                return Err(SubmitError::NotOfRemote(statement.hash()));
            }
            if !signed_by_hub(statement, &keys) {
                return Err(SubmitError::WrongHubKey(statement.hash()));
            }
        }
        Ok(held)
    }

    /// The configuration of the remote `remote`.
    fn config_of(&self, remote: &RemoteName) -> Result<RemoteConfig, SubmitError> {
        self.remote(remote)
            .map_err(SubmitError::Remote)?
            .ok_or_else(|| SubmitError::NoSuchRemote(remote.clone()))
    }
}

/// Asks the authorizer for `operation` with the target `remote <name>`.
fn authorize(
    operation: &str,
    remote: &RemoteName,
    actor: &Actor,
    authorizer: &dyn Authorizer,
) -> Result<(), SubmitError> {
    // Nothing is written, so no lock is taken, and the answer is not kept as a proof.
    let name = OperationName::try_new(operation).expect("the operation names are valid");
    let target = format!("remote {remote}");
    match authorizer.authorize(actor, &name, &target) {
        Decision::Allow => Ok(()),
        _ => Err(SubmitError::Denied {
            operation: operation.to_owned(),
            target,
        }),
    }
}

/// Submits one statement and reads the answer.
fn submit_one(
    statement: &Statement,
    transport: &mut dyn Transport,
    keys: &HubKeys,
) -> Result<Outcome, SubmitError> {
    let encoded = statement.encode();
    let answer = transport
        .submit_statement(&mut encoded.as_slice())
        .map_err(SubmitError::Transport)?;
    match answer {
        Answer::Message(body) => {
            let bytes = read_bounded(body, MAX_HELD_ANSWER)?;
            let held = HeldStatement::parse(&bytes).map_err(SubmitError::Answer)?;
            if held.statement() != statement {
                return Err(SubmitError::NotSubmitted(statement.hash()));
            }
            if !signed_by_hub(&held, keys) {
                return Err(SubmitError::WrongHubKey(statement.hash()));
            }
            Ok(Outcome::Held(Box::new(held)))
        }
        Answer::Error(body) => match error_of(body)? {
            ErrorMessage::StatementRefused(refusal) => Ok(Outcome::Refused(refusal)),
            other => Err(SubmitError::Refused(other)),
        },
    }
}

/// Whether the key of the hub at the time of `held` signed it. The hub accepted the statement
/// after the record entry of its `record` line and before the next one, so the key is the one
/// that signed either of the two: a rotation between them leaves both possible. A former key
/// counts only for its range, as for a record entry ([`HubKeys::at`]).
fn signed_by_hub(held: &HeldStatement, keys: &HubKeys) -> bool {
    let key = held.hub_key();
    key == *keys.at(held.record()) || key == *keys.at(held.record().saturating_add(1))
}

/// The error that the hub answered.
fn error_of(body: AnswerBody<'_>) -> Result<ErrorMessage, SubmitError> {
    let bytes = read_bounded(body, MAX_ERROR_MESSAGE)?;
    ErrorMessage::parse(&bytes).map_err(SubmitError::Answer)
}

/// Reads an answer to its end, up to `bound` bytes.
fn read_bounded(body: AnswerBody<'_>, bound: u64) -> Result<Vec<u8>, SubmitError> {
    let mut bytes = Vec::new();
    body.take(bound + 1)
        .read_to_end(&mut bytes)
        .map_err(SubmitError::Body)?;
    if bytes.len() as u64 > bound {
        return Err(SubmitError::TooLarge { bound });
    }
    Ok(bytes)
}

/// Why a submission or the reading of the statements of a hub failed. None of them changed
/// anything in the clone.
#[derive(Debug)]
#[non_exhaustive]
pub enum SubmitError {
    /// The references could not be read, or the repository is bare ([`RefError::Bare`]).
    Refs(RefError),
    /// The log or the set of statements could not be read.
    Pull(PullError),
    /// No pull request has this number.
    NoSuchPull(u64),
    /// The authorizer refused the operation.
    Denied {
        /// The operation.
        operation: String,
        /// Its target.
        target: String,
    },
    /// The configuration of the remote could not be read.
    Remote(RemoteError),
    /// The repository has no remote of this name.
    NoSuchRemote(RemoteName),
    /// The configuration pinned no key of the hub, or the list of references names another key
    /// or another repository.
    HubKey(HubKeyError),
    /// A statement could not be read from the set.
    Statements(SetError),
    /// The set holds no statement of this hash.
    Missing(StatementHash),
    /// The statement names another repository than the one of the remote, or none.
    OtherRepository {
        /// The hash of the statement.
        hash: StatementHash,
        /// The repository that it names.
        named: Option<RepositoryId>,
        /// The repository of the remote.
        remote: RepositoryId,
    },
    /// The list of references could not be read.
    List(Box<FetchError>),
    /// A request got no answer.
    Transport(TransportError),
    /// The body of an answer could not be read to its end.
    Body(io::Error),
    /// An answer is longer than its bound.
    TooLarge {
        /// The bound, in bytes.
        bound: u64,
    },
    /// The answer to a submission, or an error in place of an answer, is not readable.
    Answer(ParseError),
    /// The answer with the held statements is not readable.
    Held(HeldParseError),
    /// The hub answered an error other than a refused statement.
    Refused(ErrorMessage),
    /// The hub answered with a held statement that is not the submitted one.
    NotSubmitted(StatementHash),
    /// A held statement carries the signature of another key than the one of the hub.
    WrongHubKey(StatementHash),
    /// The hub answered with a held statement of another repository.
    NotOfRemote(StatementHash),
    /// A request failed after the hub had answered to the statements before it.
    After {
        /// The answers so far.
        submitted: Vec<Submission>,
        /// Why the next request failed.
        source: Box<SubmitError>,
    },
}

impl SubmitError {
    /// The answers that came before the failure.
    pub fn submitted(&self) -> &[Submission] {
        match self {
            Self::After { submitted, .. } => submitted,
            _ => &[],
        }
    }

    /// The error of the file system, if that is what failed (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Refs(error) => error.io_error(),
            Self::Remote(error) => error.io_error(),
            Self::Statements(error) => error.io_error(),
            Self::List(error) => error.io_error(),
            Self::Transport(error) => error.io_error(),
            Self::Body(error) => Some(error),
            Self::After { source, .. } => source.io_error(),
            _ => None,
        }
    }

    /// Whether the hub could not be reached or its answer broke off, so that a later call can
    /// succeed: the statements still count in the clone. A failure of the carrier and of the
    /// body of an answer are such; something that came back but is no answer of the protocol
    /// ([`TransportError::NoAnswer`], such as the page of a proxy or a busy hub without a body
    /// of the protocol) is not, as the status of the answer says more about it, and neither is
    /// a body that came back whole but cannot be decoded (an error of the kind
    /// [`io::ErrorKind::InvalidData`], as the transport gives it for a body of zstd that
    /// cannot be decoded).
    pub fn is_unreachable(&self) -> bool {
        match self {
            Self::Transport(error) => matches!(error, TransportError::Carrier(_)),
            Self::Body(error) => broke_off(error),
            Self::List(error) => match &**error {
                FetchError::Transport(TransportError::Carrier(_)) => true,
                FetchError::Body(error) => broke_off(error),
                _ => false,
            },
            Self::After { source, .. } => source.is_unreachable(),
            _ => false,
        }
    }
}

/// Whether the error of reading a body is one of a body that broke off, and not of one that
/// came back but cannot be decoded ([`SubmitError::is_unreachable`]).
pub fn broke_off(error: &io::Error) -> bool {
    error.kind() != io::ErrorKind::InvalidData
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refs(error) => error.fmt(f),
            Self::Pull(error) => error.fmt(f),
            Self::NoSuchPull(number) => write!(f, "there is no pull request {number}"),
            Self::Denied { operation, target } => {
                write!(f, "the authorizer refuses {operation} {target}")
            }
            Self::Remote(error) => error.fmt(f),
            Self::NoSuchRemote(remote) => write!(f, "there is no remote {remote}"),
            Self::HubKey(error) => error.fmt(f),
            Self::Statements(error) => error.fmt(f),
            Self::Missing(hash) => write!(f, "the set of statements holds no statement {hash}"),
            Self::OtherRepository {
                hash,
                named: Some(named),
                remote,
            } => write!(
                f,
                "statement {hash} names the repository {named}, not {remote} of the remote"
            ),
            Self::OtherRepository {
                hash,
                named: None,
                remote,
            } => write!(
                f,
                "statement {hash} names no repository of a hub, so the hub of {remote} does \
                 not accept it"
            ),
            Self::List(error) => write!(f, "the list of references: {error}"),
            Self::Transport(error) => error.fmt(f),
            Self::Body(error) => write!(f, "the answer could not be read: {error}"),
            Self::TooLarge { bound } => {
                write!(f, "the answer of the hub is longer than {bound} bytes")
            }
            Self::Answer(error) => write!(f, "the answer of the hub is not readable: {error}"),
            Self::Held(error) => write!(f, "the held statements are not readable: {error}"),
            Self::Refused(message) => write!(f, "the hub refused: {message}"),
            Self::NotSubmitted(hash) => write!(
                f,
                "the hub answered statement {hash} with another statement"
            ),
            Self::WrongHubKey(hash) => write!(
                f,
                "statement {hash} carries the signature of neither the pinned key of the hub nor \
                 a former key of the hub for the record entry that it names"
            ),
            Self::NotOfRemote(hash) => write!(
                f,
                "the hub holds statement {hash} of another repository than the one of the \
                 remote"
            ),
            Self::After { submitted, source } => write!(
                f,
                "{source}, after the hub had answered {} statement{}",
                submitted.len(),
                if submitted.len() == 1 { "" } else { "s" }
            ),
        }
    }
}

impl Error for SubmitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Refs(error) => Some(error),
            Self::Pull(error) => Some(error),
            Self::Remote(error) => Some(error),
            Self::HubKey(error) => Some(error),
            Self::Statements(error) => Some(error),
            Self::List(error) => Some(error.as_ref()),
            Self::Transport(error) => Some(error),
            Self::Body(error) => Some(error),
            Self::Answer(error) => Some(error),
            Self::Held(error) => Some(error),
            Self::After { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
