//! The answers to a ref update that is not accepted (ADR-025): the typed reasons, and the lines
//! of the two error messages that carry them ([`ErrorMessage::Stale`] and
//! [`ErrorMessage::Refused`]).
//!
//! A stale update does not continue the chains of its names. The answer names every reference
//! that does not match with its state on the hub ([`Stale`]), and the actor decides what to sign
//! next:
//!
//! ```text
//! stemma-protocol 1
//! error stale
//! ref <qualified name> <entry> <commit>            one or more, in the order of the names
//! ```
//!
//! `entry` is the hash of the last record entry of the name and `commit` is the commit of the
//! reference; each of them is `-` where there is none.
//!
//! A refused update is one that the register, the policy, the write ownership, or the gate does
//! not allow ([`Refused`]). Either the update as a whole is refused, with one `update` line, or
//! references of it are, with one `ref` line each:
//!
//! ```text
//! stemma-protocol 1
//! error refused
//! update hub <address>                             one `update` line:
//! update repository <id>                           what the hub is, where the update names
//! update key <commit>                              another, the commit whose register lacks the
//! update no-main -                                 key, or a repository without `main`
//!
//! stemma-protocol 1
//! error refused
//! ref <qualified name> <reason> <facts>            one or more, in the order of the names
//! ```
//!
//! The reasons of a reference and their facts are those of [`RefusalReason`]:
//!
//! ```text
//! denied <operation>                 leased <human or agent> <id> <expiry>
//! gate -                             reserved -
//! no-register -                      not-owner -
//! bad-register <one line>            conflict <one line>
//! incomplete <one line>              policy <one line>
//! ```
//!
//! Nothing of it leaks: who is answered a refusal may read the repository and could compute all
//! of it. A refusal is recorded in no shared format; the record of ref updates holds accepted
//! updates only.
//!
//! [`ErrorMessage::Stale`]: super::ErrorMessage::Stale
//! [`ErrorMessage::Refused`]: super::ErrorMessage::Refused

use std::error::Error;
use std::fmt;

use super::record::{HubAddress, RecordProblem, Stale, StaleReference};
use super::{
    is_reason, malformed, parse_commit, Lines, Malformed, ParseError, QualifiedName, Reason,
    RepositoryId,
};
use crate::object::{parse_seconds, CommitId};
use crate::oplog::OperationName;
use crate::register::Identity;

/// The text of a value that is not there.
const NONE: &str = "-";

/// Why one reference of a ref update is refused (ADR-025). The reason carries what the sender
/// needs to act: who holds the branch and until when, or which rule stands in the way.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefusalReason {
    /// The authorizer refused the operation on the reference: `push` for a branch, `tag` to set a
    /// tag, `untag` to delete one.
    Denied {
        /// The name of the operation.
        operation: OperationName,
    },
    /// Another actor holds the write ownership of the branch, and its lease has not ended.
    Leased {
        /// The write owner: the actor of the last record entry of the name.
        holder: Identity,
        /// The end of the lease, in seconds since the Unix epoch, by the time of the hub.
        expires: i64,
    },
    /// The reference is `main` in a repository that has one. `main` moves through the merge gate
    /// only, and the gate on a hub is not part of this version of the accepting operation.
    Gate,
    /// The name of a new reference starts with a reserved prefix (ADR-008).
    Reserved,
    /// The first push: the commit that it sets `main` to holds no register. A hub accepts only
    /// enforcing repositories (ADR-024).
    NoRegister,
    /// The first push: the register of the commit that it sets `main` to cannot be used. Carries
    /// what the reader of the register reported, or that the commit records an open conflict,
    /// which has no one tree to read a register from.
    BadRegister(Reason),
    /// The first push: the register of the commit that it sets `main` to does not hold the key
    /// of the update as an active key of an owner (ADR-024).
    NotOwner,
    /// The name of a new reference cannot be taken: another reference, or a directory of
    /// references, holds it or a part of it, also one that differs in case only. Carries the name
    /// that is in the way.
    Conflict(Reason),
    /// The objects of the new commit do not hold together: one is missing, has another kind, or
    /// is refused by its reader. The sender uploads again (ADR-023). Carries what the
    /// connectivity check reported.
    Incomplete(Reason),
    /// The policy at the tip of `main`, which names the duration of the lease, cannot be used.
    /// Carries what its reader reported.
    Policy(Reason),
}

impl fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied { operation } => write!(f, "the operation `{operation}` is not allowed"),
            Self::Leased { holder, expires } => write!(
                f,
                "the branch is leased to {holder} until {expires} (seconds since the Unix epoch)"
            ),
            Self::Gate => f.write_str("`main` moves through the merge gate only"),
            Self::Reserved => f.write_str("the name starts with a reserved prefix"),
            Self::NoRegister => f.write_str("the commit of the first push holds no register"),
            Self::BadRegister(reason) => {
                write!(
                    f,
                    "the register of the commit of the first push cannot be used: {reason}"
                )
            }
            Self::NotOwner => f.write_str(
                "the register of the commit of the first push does not hold the key of the ref \
                 update as an active key of an owner",
            ),
            Self::Conflict(with) => write!(f, "the name is held by `{with}`"),
            Self::Incomplete(reason) => {
                write!(
                    f,
                    "the objects of the new commit are not complete: {reason}"
                )
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

impl RefusalReason {
    /// The word of the reason and its facts, as the `ref` line carries them.
    fn encode(&self) -> String {
        match self {
            Self::Denied { operation } => format!("denied {operation}"),
            Self::Leased { holder, expires } => format!("leased {holder} {expires}"),
            Self::Gate => format!("gate {NONE}"),
            Self::Reserved => format!("reserved {NONE}"),
            Self::NoRegister => format!("no-register {NONE}"),
            Self::BadRegister(reason) => format!("bad-register {reason}"),
            Self::NotOwner => format!("not-owner {NONE}"),
            Self::Conflict(with) => format!("conflict {with}"),
            Self::Incomplete(reason) => format!("incomplete {reason}"),
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
        let text = |wrap: fn(Reason) -> Self| {
            if is_reason(facts) {
                Ok(wrap(Reason(facts.to_owned())))
            } else {
                Err(malformed(line, Malformed::Reason))
            }
        };
        match word {
            "denied" => OperationName::try_new(facts)
                .map(|operation| Self::Denied { operation })
                .map_err(|_| malformed(line, Malformed::Fields)),
            "leased" => {
                let (holder, expires) = facts
                    .rsplit_once(' ')
                    .ok_or(malformed(line, Malformed::Fields))?;
                let holder = holder
                    .parse::<Identity>()
                    .ok()
                    .filter(|identity| identity.to_string() == holder)
                    .ok_or(problem(line, RecordProblem::Actor))?;
                let expires =
                    parse_seconds(expires).map_err(|_| problem(line, RecordProblem::Time))?;
                Ok(Self::Leased { holder, expires })
            }
            "gate" => none(Self::Gate),
            "reserved" => none(Self::Reserved),
            "no-register" => none(Self::NoRegister),
            "bad-register" => text(Self::BadRegister),
            "not-owner" => none(Self::NotOwner),
            "conflict" => text(Self::Conflict),
            "incomplete" => text(Self::Incomplete),
            "policy" => text(Self::Policy),
            _ => Err(malformed(line, Malformed::Fields)),
        }
    }
}

fn problem(line: usize, reason: RecordProblem) -> ParseError {
    ParseError::Record { line, reason }
}

/// A reference of a ref update that is refused, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedReference {
    /// The qualified name.
    pub name: QualifiedName,
    /// Why the reference is refused.
    pub reason: RefusalReason,
}

/// The refused references of one ref update: at least one, each once, in the order of the
/// qualified names. No reference of the update moved (ADR-025); a reference of the update that
/// is not named here was not refused itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedReferences(Vec<RefusedReference>);

impl RefusedReferences {
    /// The refused references. They are sorted by their qualified names, so the order of the
    /// argument does not matter.
    ///
    /// # Errors
    ///
    /// [`RecordProblem::NoReference`] if `references` is empty, and
    /// [`RecordProblem::DuplicateReference`] if a qualified name is named twice.
    pub fn try_new(mut references: Vec<RefusedReference>) -> Result<Self, RecordProblem> {
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
        Ok(Self(references))
    }

    /// The refused references, in the order of their qualified names, at least one.
    pub fn references(&self) -> &[RefusedReference] {
        &self.0
    }

    /// The reason for which the reference is refused, or `None` if it is not named.
    pub fn reason_of(&self, name: &QualifiedName) -> Option<&RefusalReason> {
        self.0
            .iter()
            .find(|refused| &refused.name == name)
            .map(|refused| &refused.reason)
    }
}

/// Why a ref update is refused (ADR-025): the update as a whole, or references of it. No
/// reference of the update moved, and nothing was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refused {
    /// The update names another address than the one under which the hub was called, so it was
    /// signed for another hub (ADR-026).
    Hub {
        /// The address under which the hub was called.
        called: HubAddress,
    },
    /// The update names another repository than the one it was sent to.
    Repository {
        /// The ID of the repository that the update was sent to.
        id: RepositoryId,
    },
    /// The register at the tip of `main` does not hold the key of the update as an active key.
    Key {
        /// The commit of `main` whose register was read.
        register: CommitId,
    },
    /// The repository has no `main`, and the update does not create it. The first push into an
    /// empty repository sets `main`, whose register every later update is judged by (ADR-024).
    NoMain,
    /// References of the update are refused, each with its reason.
    References(RefusedReferences),
}

impl Refused {
    /// Whether a reference is refused because the objects of its new commit do not hold
    /// together ([`RefusalReason::Incomplete`]). The sender then uploads once more and sends the
    /// same update again; a carrier names this class of refusal apart (ADR-023). It is `true`
    /// as soon as one reference is refused so, also beside references that are refused for
    /// another reason, which no upload mends.
    pub fn wants_upload(&self) -> bool {
        matches!(self, Self::References(refused) if refused
            .0
            .iter()
            .any(|refused| matches!(refused.reason, RefusalReason::Incomplete(_))))
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hub { called } => write!(
                f,
                "the ref update names another hub than {called}, under which this one was called"
            ),
            Self::Repository { id } => write!(
                f,
                "the ref update names another repository than {id}, which it was sent to"
            ),
            Self::Key { register } => write!(
                f,
                "the register of `main` at {register} does not hold the key of the ref update as \
                 an active key"
            ),
            Self::NoMain => f.write_str(
                "the repository has no `main`, and the ref update does not create it; the first \
                 push sets `main`",
            ),
            Self::References(refused) => {
                f.write_str("the ref update is refused: ")?;
                for (index, refused) in refused.0.iter().enumerate() {
                    if index > 0 {
                        f.write_str("; ")?;
                    }
                    write!(f, "{}: {}", refused.name, refused.reason)?;
                }
                Ok(())
            }
        }
    }
}

impl Error for Refused {}

/// Writes a value, or `-` where there is none.
fn or_none<T: fmt::Display>(value: &Option<T>) -> String {
    value.as_ref().map_or_else(|| NONE.to_owned(), T::to_string)
}

/// Reads a value that may be `-`.
fn parse_optional<T>(
    text: &str,
    read: impl FnOnce(&str) -> Result<T, ParseError>,
) -> Result<Option<T>, ParseError> {
    if text == NONE {
        Ok(None)
    } else {
        read(text).map(Some)
    }
}

/// Reads the qualified name of a `ref` line, which must come after the name of the line before.
fn parse_name(
    line: usize,
    text: &str,
    before: Option<&QualifiedName>,
) -> Result<QualifiedName, ParseError> {
    let name: QualifiedName = text
        .parse()
        .map_err(|source| ParseError::InvalidName { line, source })?;
    if before.is_some_and(|before| *before >= name) {
        return Err(malformed(line, Malformed::Order));
    }
    Ok(name)
}

/// The lines of an `error stale` after the line of its kind.
pub(super) fn encode_stale(stale: &Stale) -> String {
    stale
        .references()
        .iter()
        .map(|stale| {
            format!(
                "ref {} {} {}\n",
                stale.name,
                or_none(&stale.entry),
                or_none(&stale.commit)
            )
        })
        .collect()
}

/// Reads the lines of an `error stale` after the line of its kind, to the end of the message.
pub(super) fn read_stale(lines: &mut Lines<'_>) -> Result<Stale, ParseError> {
    let mut references: Vec<StaleReference> = Vec::new();
    let mut last = lines.line;
    while let Some((line, value)) = lines.take("ref")? {
        let fields: Vec<&str> = value.split(' ').collect();
        let [name, entry, commit] = fields[..] else {
            return Err(malformed(line, Malformed::Fields));
        };
        let name = parse_name(line, name, references.last().map(|stale| &stale.name))?;
        let entry = parse_optional(entry, |text| {
            text.parse()
                .map_err(|source| ParseError::InvalidRecordHash { line, source })
        })?;
        let commit = parse_optional(commit, |text| parse_commit(line, text))?;
        references.push(StaleReference {
            name,
            entry,
            commit,
        });
        last = line;
    }
    if references.is_empty() {
        return Err(malformed(lines.line, Malformed::Expected("ref")));
    }
    lines.end()?;
    // The names are in order and at least one is there, so only the rule of the state is left.
    Stale::try_new(references).map_err(|reason| problem(last, reason))
}

/// The lines of an `error refused` after the line of its kind.
pub(super) fn encode_refused(refused: &Refused) -> String {
    match refused {
        Refused::Hub { called } => format!("update hub {called}\n"),
        Refused::Repository { id } => format!("update repository {id}\n"),
        Refused::Key { register } => format!("update key {register}\n"),
        Refused::NoMain => format!("update no-main {NONE}\n"),
        Refused::References(refused) => refused
            .0
            .iter()
            .map(|refused| format!("ref {} {}\n", refused.name, refused.reason.encode()))
            .collect(),
    }
}

/// Reads the lines of an `error refused` after the line of its kind, to the end of the message.
pub(super) fn read_refused(lines: &mut Lines<'_>) -> Result<Refused, ParseError> {
    if let Some((line, value)) = lines.take("update")? {
        let (word, fact) = value
            .split_once(' ')
            .ok_or(malformed(line, Malformed::Fields))?;
        let refused = match word {
            "hub" => Refused::Hub {
                called: HubAddress::try_new(fact).map_err(|reason| problem(line, reason))?,
            },
            "repository" => Refused::Repository {
                id: fact
                    .parse()
                    .map_err(|source| ParseError::InvalidRepositoryId { line, source })?,
            },
            "key" => Refused::Key {
                register: parse_commit(line, fact)?,
            },
            "no-main" if fact == NONE => Refused::NoMain,
            _ => return Err(malformed(line, Malformed::Fields)),
        };
        lines.end()?;
        return Ok(refused);
    }
    let mut references: Vec<RefusedReference> = Vec::new();
    while let Some((line, value)) = lines.take("ref")? {
        let mut fields = value.splitn(3, ' ');
        let (Some(name), Some(word), Some(facts)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(malformed(line, Malformed::Fields));
        };
        let name = parse_name(line, name, references.last().map(|refused| &refused.name))?;
        let reason = RefusalReason::parse(line, word, facts)?;
        references.push(RefusedReference { name, reason });
    }
    if references.is_empty() {
        return Err(malformed(lines.line, Malformed::Expected("ref")));
    }
    lines.end()?;
    Ok(Refused::References(RefusedReferences(references)))
}

#[cfg(test)]
pub(crate) mod tests;
