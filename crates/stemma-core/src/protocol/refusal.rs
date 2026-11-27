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
//! bad-register <one line>            conflict <name>
//! incomplete <one line>              policy <one line>
//! gate <verdict> <facts>
//! ```
//!
//! `gate -` refuses a move of `main` that is no move from its tip `T` to a merge `M`, such as
//! its deletion. `gate <verdict> <facts>` is the refusal of the gate on a hub (STEMMA-303), whose
//! verdicts and facts are those of [`GateRefusal`]:
//!
//! ```text
//! no-pull -                          not-open -
//! parents -                          not-the-merge -
//! branch-moved <commit or ->         conflict -
//! policy <one line>                  level <one line>
//! unusable <one line>
//! not-checked <level> <source>
//! not-approved <level> <source> <agents> <agents needed> <humans> <humans needed>
//! red-tip <commit>                   unverified <commits> <bound>
//! reserved <pull> <end of the reservation>
//! ```
//!
//! `source` is `paths` for the level of the paths of the change and `weakened` where the
//! escalation for weakened tests set it (ADR-004, rule 10).
//!
//! Nothing of it leaks: who is answered a refusal may read the repository and could compute all
//! of it. A refusal is recorded in no shared format; the record of ref updates holds accepted
//! updates only.
//!
//! [`ErrorMessage::Stale`]: super::ErrorMessage::Stale
//! [`ErrorMessage::Refused`]: super::ErrorMessage::Refused

use std::error::Error;
use std::fmt;

use super::parse_positive;
use super::record::StatementHash;
use super::record::{HubAddress, RecordProblem, Stale, StaleReference};
use super::{
    is_reason, malformed, parse_commit, Lines, Malformed, ParseError, QualifiedName, Reason,
    RepositoryId,
};
use crate::object::{parse_seconds, CommitId};
use crate::oplog::OperationName;
use crate::policy::RiskLevel;
use crate::pull::TARGET;
use crate::refs::{BranchName, RefKind};
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
        /// The operation.
        operation: RefOperation,
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
    /// that is in the way, a reference of the same kind or a directory of them.
    Conflict(BranchName),
    /// The objects of the new commit do not hold together: one is missing, has another kind, or
    /// is refused by its reader. The sender uploads again (ADR-023). Carries what the
    /// connectivity check reported.
    Incomplete(Reason),
    /// The policy at the tip of `main`, which names the duration of the lease, cannot be used.
    /// Carries what its reader reported.
    Policy(Reason),
    /// The gate on a hub refuses the move of `main` from its tip `T` to the merge `M`
    /// (STEMMA-303), with the verdict and its facts.
    Verdict(GateRefusal),
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
            Self::Verdict(verdict) => write!(f, "the gate refuses the merge: {verdict}"),
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
            Self::Verdict(verdict) => format!("gate {}", verdict.encode()),
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
            "denied" => RefOperation::ALL
                .into_iter()
                .find(|operation| operation.name() == facts)
                .map(|operation| Self::Denied { operation })
                .ok_or(malformed(line, Malformed::Fields)),
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
            "gate" if facts == NONE => Ok(Self::Gate),
            "gate" => GateRefusal::parse(line, facts).map(Self::Verdict),
            "reserved" => none(Self::Reserved),
            "no-register" => none(Self::NoRegister),
            "bad-register" => text(Self::BadRegister),
            "not-owner" => none(Self::NotOwner),
            "conflict" => facts
                .parse()
                .map(Self::Conflict)
                .map_err(|_| malformed(line, Malformed::Fields)),
            "incomplete" => text(Self::Incomplete),
            "policy" => text(Self::Policy),
            _ => Err(malformed(line, Malformed::Fields)),
        }
    }
}

fn problem(line: usize, reason: RecordProblem) -> ParseError {
    ParseError::Record { line, reason }
}

/// A reference of a ref update that is refused, with the reason. Every value carries a reason
/// that can be given for its reference ([`try_new`](Self::try_new)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedReference {
    name: QualifiedName,
    reason: RefusalReason,
}

impl RefusedReference {
    /// A refused reference.
    ///
    /// # Errors
    ///
    /// [`RecordProblem::RefusalReason`] if the reason does not fit the reference:
    /// [`Gate`](RefusalReason::Gate), [`Verdict`](RefusalReason::Verdict),
    /// [`NoRegister`](RefusalReason::NoRegister),
    /// [`BadRegister`](RefusalReason::BadRegister), and [`NotOwner`](RefusalReason::NotOwner)
    /// are reasons of `heads/main` only; [`Leased`](RefusalReason::Leased) and
    /// [`Policy`](RefusalReason::Policy) are reasons of a branch other than `main`; and
    /// [`Denied`](RefusalReason::Denied) names `push` for a branch and `tag` or `untag` for a
    /// tag. [`Reserved`](RefusalReason::Reserved), [`Conflict`](RefusalReason::Conflict), and
    /// [`Incomplete`](RefusalReason::Incomplete) fit every reference.
    pub fn try_new(name: QualifiedName, reason: RefusalReason) -> Result<Self, RecordProblem> {
        let branch = name.kind() == RefKind::Branch;
        let main = branch && name.name().as_str() == TARGET;
        let fits = match &reason {
            RefusalReason::Gate
            | RefusalReason::Verdict(_)
            | RefusalReason::NoRegister
            | RefusalReason::BadRegister(_)
            | RefusalReason::NotOwner => main,
            RefusalReason::Leased { .. } | RefusalReason::Policy(_) => branch && !main,
            RefusalReason::Denied { operation } => (operation.kind() == RefKind::Branch) == branch,
            RefusalReason::Reserved | RefusalReason::Conflict(_) | RefusalReason::Incomplete(_) => {
                true
            }
        };
        if fits {
            Ok(Self { name, reason })
        } else {
            Err(RecordProblem::RefusalReason)
        }
    }

    /// The qualified name.
    pub fn name(&self) -> &QualifiedName {
        &self.name
    }

    /// Why the reference is refused.
    pub fn reason(&self) -> &RefusalReason {
        &self.reason
    }
}

/// An operation that the authorizer is asked for when a reference moves on a hub (ADR-025).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefOperation {
    /// A branch is created, moved, or deleted.
    Push,
    /// A tag is set.
    Tag,
    /// A tag is deleted.
    Untag,
}

impl RefOperation {
    /// Every operation.
    pub const ALL: [Self; 3] = [Self::Push, Self::Tag, Self::Untag];

    /// The word of the operation: `push`, `tag`, or `untag`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Tag => "tag",
            Self::Untag => "untag",
        }
    }

    /// The kind of reference that the operation moves.
    pub const fn kind(self) -> RefKind {
        match self {
            Self::Push => RefKind::Branch,
            Self::Tag | Self::Untag => RefKind::Tag,
        }
    }

    /// The name that the authorizer is asked with, as the operation log names operations.
    pub fn operation_name(self) -> OperationName {
        OperationName::try_new(self.name()).expect("a valid operation name")
    }
}

impl fmt::Display for RefOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
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

// The verdict of the gate on a hub (STEMMA-303).

/// Where the level of a change that the gate refuses comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LevelSource {
    /// The paths of the difference between `T` and `M`, by the policies.
    Paths,
    /// The escalation for weakened tests (ADR-004, rule 10), which raised the level of the paths.
    Weakened,
}

impl LevelSource {
    /// The word of the source: `paths` or `weakened`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Paths => "paths",
            Self::Weakened => "weakened",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        [Self::Paths, Self::Weakened]
            .into_iter()
            .find(|source| source.name() == word)
    }
}

/// Why the gate on a hub refuses to move `main` from its tip `T` to the merge `M` (ADR-025,
/// STEMMA-303). The hub recomputes the verdict that the local gate reaches, from the commits,
/// the register and the policy of `T`, and the statements it holds; nothing that the sender
/// claims enters it. The facts are what the sender needs to see why; the statements that do
/// not count are visible to every reader, who recomputes the rest (ADR-025).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GateRefusal {
    /// The move names no evidence, or an opening that the hub does not hold.
    NoPull,
    /// The pull request that the move names has ended: it was closed or merged.
    NotOpen,
    /// `M` is no commit with the single parent `T`.
    Parents,
    /// The tree of `M` is not the clean merge of the head onto `T`.
    NotTheMerge,
    /// The branch of the pull request is not at the head of its opening on the hub. Carries the
    /// commit of the branch, or `None` if it does not exist.
    BranchMoved {
        /// The commit of the branch.
        head: Option<CommitId>,
    },
    /// The merge of the head onto `T` has a conflict, which never enters `main`.
    Conflict,
    /// The policy of `T` cannot be used. Carries what its reader reported.
    Policy(Reason),
    /// The level of the change cannot be told, or the change has no file that differs from `T`.
    /// Carries what was found.
    Level(Reason),
    /// The register or the policy of `M` cannot be used: `M` holds no register, or one or a
    /// policy that its reader refuses. Every later update reads the register of the tip of
    /// `main`, and every later move of `main` its policy, so such an `M` would end the use of
    /// the repository on the hub. Carries what the reader reported.
    Unusable(Reason),
    /// No result of a check counts at the level of the change (ADR-019).
    NotChecked {
        /// The level of the change.
        level: RiskLevel,
        /// Where the level comes from.
        source: LevelSource,
    },
    /// The approvals that count are fewer than the level asks for (ADR-004).
    NotApproved {
        /// The level of the change.
        level: RiskLevel,
        /// Where the level comes from.
        source: LevelSource,
        /// The approvals of agents that count.
        agents: u32,
        /// The approvals of agents that the level asks for.
        agents_needed: u32,
        /// The approvals of humans that count for the level: of a member at level 2, of an
        /// owner at level 3.
        humans: u32,
        /// The approvals of humans that the level asks for: 1 at the levels 2 and 3, else 0.
        humans_needed: u32,
    },
    /// The latest verified tip of `main` is red, and the change has level 2 or 3 and is no
    /// repair of it (ADR-019). Carries the first red tip.
    RedTip {
        /// The oldest commit after the last green tip that a failing run covered.
        red: CommitId,
    },
    /// The bound `max_unverified` is reached (ADR-019).
    Unverified {
        /// The commits after the latest verified tip.
        count: u64,
        /// The bound of the policy.
        max: u32,
    },
    /// Another pull request holds the reservation (ADR-019).
    Reserved {
        /// The pull request that holds it: the hash of its opening.
        pull: StatementHash,
        /// The end of the reservation, in seconds since the Unix epoch, by the time of the hub.
        until: i64,
    },
}

impl fmt::Display for GateRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = |source: &LevelSource| match source {
            LevelSource::Paths => "by its paths",
            LevelSource::Weakened => "by the escalation for weakened tests",
        };
        match self {
            Self::NoPull => f.write_str("the move names no pull request that the hub holds"),
            Self::NotOpen => f.write_str("the pull request is not open"),
            Self::Parents => {
                f.write_str("the merge does not have the tip of `main` as its only parent")
            }
            Self::NotTheMerge => {
                f.write_str("the tree of the merge is not the clean merge of the head onto the tip")
            }
            Self::BranchMoved { head: Some(head) } => write!(
                f,
                "the branch of the pull request is at {head}, not at the head of its opening"
            ),
            Self::BranchMoved { head: None } => {
                f.write_str("the branch of the pull request does not exist")
            }
            Self::Conflict => f.write_str("the head conflicts with the tip of `main`"),
            Self::Policy(reason) => write!(f, "the policy of the tip cannot be used: {reason}"),
            Self::Level(reason) => write!(f, "the level of the change cannot be told: {reason}"),
            Self::Unusable(reason) => {
                write!(
                    f,
                    "the register or the policy of the merge cannot be used: {reason}"
                )
            }
            Self::NotChecked {
                level,
                source: from,
            } => write!(
                f,
                "no result of a check counts at level {level}, {}",
                source(from)
            ),
            Self::NotApproved {
                level,
                source: from,
                agents,
                agents_needed,
                humans,
                humans_needed,
            } => write!(
                f,
                "level {level}, {}, needs {agents_needed} approvals of agents and \
                 {humans_needed} of humans; {agents} and {humans} count",
                source(from)
            ),
            Self::RedTip { red } => write!(
                f,
                "the latest verified tip of `main` is red since {red}; only a repair of it lands \
                 at level 2 or 3"
            ),
            Self::Unverified { count, max } => write!(
                f,
                "{count} commits of `main` lie after the latest verified tip (the bound is {max})"
            ),
            Self::Reserved { pull, until } => write!(
                f,
                "pull request {pull} holds the reservation until {until} (seconds since the Unix \
                 epoch)"
            ),
        }
    }
}

impl GateRefusal {
    /// The word of the verdict and its facts, as the `ref` line carries them after `gate`.
    fn encode(&self) -> String {
        match self {
            Self::NoPull => format!("no-pull {NONE}"),
            Self::NotOpen => format!("not-open {NONE}"),
            Self::Parents => format!("parents {NONE}"),
            Self::NotTheMerge => format!("not-the-merge {NONE}"),
            Self::BranchMoved { head } => format!("branch-moved {}", or_none(head)),
            Self::Conflict => format!("conflict {NONE}"),
            Self::Policy(reason) => format!("policy {reason}"),
            Self::Level(reason) => format!("level {reason}"),
            Self::Unusable(reason) => format!("unusable {reason}"),
            Self::NotChecked { level, source } => {
                format!("not-checked {level} {}", source.name())
            }
            Self::NotApproved {
                level,
                source,
                agents,
                agents_needed,
                humans,
                humans_needed,
            } => format!(
                "not-approved {level} {} {agents} {agents_needed} {humans} {humans_needed}",
                source.name()
            ),
            Self::RedTip { red } => format!("red-tip {red}"),
            Self::Unverified { count, max } => format!("unverified {count} {max}"),
            Self::Reserved { pull, until } => format!("reserved {pull} {until}"),
        }
    }

    /// Reads the word of a verdict and its facts, the text after `gate`.
    fn parse(line: usize, text: &str) -> Result<Self, ParseError> {
        let fields = || malformed(line, Malformed::Fields);
        let (word, facts) = text.split_once(' ').ok_or_else(fields)?;
        let none = |verdict: Self| {
            if facts == NONE {
                Ok(verdict)
            } else {
                Err(fields())
            }
        };
        let reason = |wrap: fn(Reason) -> Self| {
            if is_reason(facts) {
                Ok(wrap(Reason(facts.to_owned())))
            } else {
                Err(malformed(line, Malformed::Reason))
            }
        };
        let count = |text: &str| parse_count::<u32>(text).ok_or(malformed(line, Malformed::Number));
        let level = |text: &str| {
            text.parse::<u32>()
                .ok()
                .filter(|_| text.len() == 1)
                .and_then(RiskLevel::from_number)
                .ok_or(malformed(line, Malformed::Fields))
        };
        let source =
            |text: &str| LevelSource::parse(text).ok_or(malformed(line, Malformed::Fields));
        let words: Vec<&str> = facts.split(' ').collect();
        match word {
            "no-pull" => none(Self::NoPull),
            "not-open" => none(Self::NotOpen),
            "parents" => none(Self::Parents),
            "not-the-merge" => none(Self::NotTheMerge),
            "conflict" => none(Self::Conflict),
            "branch-moved" if facts == NONE => Ok(Self::BranchMoved { head: None }),
            "branch-moved" => Ok(Self::BranchMoved {
                head: Some(parse_commit(line, facts)?),
            }),
            "policy" => reason(Self::Policy),
            "level" => reason(Self::Level),
            "unusable" => reason(Self::Unusable),
            "not-checked" => match words.as_slice() {
                [found, from] => Ok(Self::NotChecked {
                    level: level(found)?,
                    source: source(from)?,
                }),
                _ => Err(fields()),
            },
            "not-approved" => match words.as_slice() {
                [found, from, agents, agents_needed, humans, humans_needed] => {
                    Ok(Self::NotApproved {
                        level: level(found)?,
                        source: source(from)?,
                        agents: count(agents)?,
                        agents_needed: count(agents_needed)?,
                        humans: count(humans)?,
                        humans_needed: count(humans_needed)?,
                    })
                }
                _ => Err(fields()),
            },
            "red-tip" => Ok(Self::RedTip {
                red: parse_commit(line, facts)?,
            }),
            "unverified" => match words.as_slice() {
                [found, max] => Ok(Self::Unverified {
                    count: parse_count::<u64>(found).ok_or(malformed(line, Malformed::Number))?,
                    max: count(max)?,
                }),
                _ => Err(fields()),
            },
            "reserved" => match words.as_slice() {
                [pull, until] => Ok(Self::Reserved {
                    pull: pull
                        .parse()
                        .map_err(|source| ParseError::InvalidObjectId { line, source })?,
                    until: parse_seconds(until).map_err(|_| problem(line, RecordProblem::Time))?,
                }),
                _ => Err(fields()),
            },
            _ => Err(fields()),
        }
    }
}

/// Reads a count: `0`, or a number from 1 in decimal digits without a leading zero.
fn parse_count<T: std::str::FromStr + From<u8>>(text: &str) -> Option<T> {
    if text == "0" {
        Some(T::from(0))
    } else {
        parse_positive(text)
    }
}

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
        let refused =
            RefusedReference::try_new(name, reason).map_err(|reason| problem(line, reason))?;
        references.push(refused);
    }
    if references.is_empty() {
        return Err(malformed(lines.line, Malformed::Expected("ref")));
    }
    lines.end()?;
    Ok(Refused::References(RefusedReferences(references)))
}

#[cfg(test)]
pub(crate) mod tests;
