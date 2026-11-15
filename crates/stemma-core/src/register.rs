//! The actor register: which key speaks for which identity (ADR-003, ADR-004).
//!
//! The register is the file [`FILE`] in the repository, level 3 (ADR-004). It binds an identity,
//! a human or an agent, to Ed25519 keys. A signature counts only against the register of the base
//! commit, and no clock is involved: the time of a key is a position in the operation log, the
//! number of an entry.
//!
//! # The file
//!
//! The reader is as strict as the one of the policy file: one encoding for one register. The file
//! is UTF-8, every line ends with a single `\n` (no `\r`), no line is empty, no line has a space
//! at its end, and the file is at most [`MAX_BYTES`] long.
//!
//! ```text
//! stemma-register 1
//! entry human daniel owner
//! key ssh-ed25519 AAAAC3Nza… active
//! key ssh-ed25519 AAAAC3Nzb… retired 12
//! entry human mia member
//! key ssh-ed25519 AAAAC3Nzc… revoked 40
//! ```
//!
//! An `entry` line names the kind (`human` or `agent`), the identifier, and the role (`owner` or
//! `member`); the `key` lines that follow belong to it. Entries are sorted by kind (humans first)
//! and then by identifier in byte order, the keys of an entry by their text, and no key stands
//! twice in the file, so a key names exactly one identity. Every entry has a key, and at least one
//! owner with an active key exists. Only a human can be an owner. The number after `retired` or
//! `revoked` is the number of the first log entry that the key no longer counts for.
//!
//! # Keys
//!
//! - **active**: signs new records.
//! - **retired** (from entry *n*): the key was replaced. The records that it signed stay valid, a
//!   log entry only if its number is below *n*. Approvals, which no number bounds, stay valid.
//! - **revoked** (from entry *n*): the key may be compromised. A log entry counts only if it
//!   stands in the log before *n*, and an approval is void.
//!
//! # Modes
//!
//! A repository without a register is *open*: the actor is a claim that nobody checks. A
//! repository is *enforcing* once it has a register. The step has no way back: the file
//! [`MARKER`] in the metadata directory records it, and an enforcing repository whose base commit
//! has no register is an error ([`ModeError::RegisterMissing`]), not an open repository.
//!
//! # Proofs
//!
//! A proof of a log entry is a file `proofs/<number>` in the metadata directory ([`ProofFile`]):
//! the actor and the SSHSIG signature over the hash of the entry. [`check_proofs`] checks them
//! against a register. Nothing writes a proof yet; the operations whose rule depends on who acts
//! come with the later slices of phase 4.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::actor::{Actor, ActorRef};
use crate::object::{Kind, ObjectId};
use crate::oplog::Entry;
use crate::repository::Repository;
use crate::signature::{self, PayloadKind, PublicKey, Signature};
use crate::store::{ObjectStore, ReadError};
use crate::{Authorizer, Decision, OperationName};

/// The path of the register file, from the root of the repository.
pub const FILE: &str = ".stemma-actors/register";

/// The name of the file of the metadata directory that records the enforcing mode.
pub const MARKER: &str = "enforcing";

/// The directory of the metadata directory that holds the proofs of log entries.
pub const PROOF_DIR: &str = "proofs";

/// The longest register file that a reader accepts, in bytes.
pub const MAX_BYTES: usize = 64 * 1024;

/// The most entries in a register.
pub const MAX_ENTRIES: usize = 256;

/// The most keys of one entry.
pub const MAX_KEYS: usize = 8;

const HEADER: &str = "stemma-register 1";
const PROOF_HEADER: &str = "stemma-proof-file 1";
const MARKER_CONTENT: &str = "stemma-enforcing 1\n";

/// The kind of an identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdentityKind {
    /// A human.
    Human,
    /// An agent.
    Agent,
}

impl IdentityKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }
}

/// Who a key speaks for: the kind and the identifier of an actor. The model, the session, and the
/// sponsor of an agent are descriptive and no part of the identity (ADR-004).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Identity {
    kind: IdentityKind,
    id: String,
}

impl Identity {
    /// The identity of an actor.
    pub fn of(actor: &Actor) -> Self {
        match actor.view() {
            ActorRef::Human { id } => Self {
                kind: IdentityKind::Human,
                id: id.to_owned(),
            },
            ActorRef::Agent { id, .. } => Self {
                kind: IdentityKind::Agent,
                id: id.to_owned(),
            },
        }
    }

    /// The identity of a human.
    ///
    /// # Errors
    ///
    /// The error of [`Actor::human`] for an identifier that is not valid.
    pub fn human(id: &str) -> Result<Self, crate::actor::ActorError> {
        Actor::human(id).map(|actor| Self::of(&actor))
    }

    /// The kind.
    pub fn kind(&self) -> IdentityKind {
        self.kind
    }

    /// The identifier.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl FromStr for Identity {
    type Err = IdentityParseError;

    /// Reads `human <id>` or `agent <id>`, the text that [`Display`](fmt::Display) writes.
    fn from_str(text: &str) -> Result<Self, IdentityParseError> {
        let (kind, id) = text.split_once(' ').ok_or(IdentityParseError)?;
        let actor = match kind {
            "human" => Actor::human(id),
            "agent" => Actor::agent(id, "m", "s", "h"),
            _ => return Err(IdentityParseError),
        };
        actor
            .map(|actor| Self::of(&actor))
            .map_err(|_| IdentityParseError)
    }
}

/// The text is not `human <id>` or `agent <id>` with a valid identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityParseError;

impl fmt::Display for IdentityParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected `human <id>` or `agent <id>` with an identifier of visible ASCII")
    }
}

impl Error for IdentityParseError {}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.kind.name(), self.id)
    }
}

/// The role of an entry. Roles that the policy gives beyond these two come with the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// May change the register and holds the level 3 rights (ADR-004). Only a human.
    Owner,
    /// Any other identity.
    Member,
}

/// The state of a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    /// Signs new records.
    Active,
    /// Replaced; counts for log entries below `from`, and for approvals.
    Retired {
        /// The number of the first log entry that the key no longer counts for.
        from: u64,
    },
    /// Withdrawn; counts for log entries below `from`, and not for approvals.
    Revoked {
        /// The number of the first log entry that the key no longer counts for.
        from: u64,
    },
}

/// A key and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEntry {
    /// The key.
    pub key: PublicKey,
    /// Its state.
    pub state: KeyState,
}

/// One entry of the register: an identity, its role, and its keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterEntry {
    identity: Identity,
    role: Role,
    keys: Vec<KeyEntry>,
}

impl RegisterEntry {
    /// The identity.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The role.
    pub fn role(&self) -> Role {
        self.role
    }

    /// The keys, sorted by their text.
    pub fn keys(&self) -> &[KeyEntry] {
        &self.keys
    }
}

/// The actor register of a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Register {
    entries: Vec<RegisterEntry>,
}

/// An actor whose proof was checked against a register. Only [`Register`] makes one, so a value
/// of this type is the evidence that the key of the actor counted at the moment of the check. The
/// authorizer of an enforcing repository ([`RegisterAuthorizer`]) asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    actor: Actor,
    key: PublicKey,
}

impl Verified {
    /// The actor.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// The key that signed for the actor.
    pub fn key(&self) -> &PublicKey {
        &self.key
    }
}

impl Register {
    /// A register with one owner and one active key, the register that `init --owner-key` makes.
    ///
    /// # Errors
    ///
    /// [`RegisterError::AgentOwner`] if the identity is an agent.
    pub fn with_owner(owner: Identity, key: PublicKey) -> Result<Self, RegisterError> {
        if owner.kind != IdentityKind::Human {
            return Err(RegisterError::AgentOwner);
        }
        Ok(Self {
            entries: vec![RegisterEntry {
                identity: owner,
                role: Role::Owner,
                keys: vec![KeyEntry {
                    key,
                    state: KeyState::Active,
                }],
            }],
        })
    }

    /// The entries, sorted.
    pub fn entries(&self) -> &[RegisterEntry] {
        &self.entries
    }

    /// The entry of an identity.
    pub fn entry(&self, identity: &Identity) -> Option<&RegisterEntry> {
        self.entries
            .iter()
            .find(|entry| &entry.identity == identity)
    }

    /// Whether the actor is a human with the role owner.
    pub fn is_owner(&self, actor: &Actor) -> bool {
        self.entry(&Identity::of(actor))
            .is_some_and(|entry| entry.role == Role::Owner)
    }

    fn holds(&self, key: &PublicKey) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.keys.iter().any(|held| &held.key == key))
    }

    fn entry_mut(&mut self, identity: &Identity) -> Result<&mut RegisterEntry, RegisterError> {
        self.entries
            .iter_mut()
            .find(|entry| &entry.identity == identity)
            .ok_or_else(|| RegisterError::UnknownIdentity(identity.clone()))
    }

    fn has_active_owner_key(&self) -> bool {
        self.entries.iter().any(|entry| {
            entry.role == Role::Owner
                && entry.keys.iter().any(|held| held.state == KeyState::Active)
        })
    }

    /// Adds an active key to an identity, for a rotation: the new key first, then
    /// [`retire`](Self::retire) of the old one.
    ///
    /// # Errors
    ///
    /// [`RegisterError::UnknownIdentity`], [`RegisterError::KeyKnown`] if any entry holds the key
    /// already, and [`RegisterError::TooManyKeys`].
    pub fn add_key(&mut self, identity: &Identity, key: PublicKey) -> Result<(), RegisterError> {
        if self.holds(&key) {
            return Err(RegisterError::KeyKnown);
        }
        let entry = self.entry_mut(identity)?;
        if entry.keys.len() >= MAX_KEYS {
            return Err(RegisterError::TooManyKeys);
        }
        entry.keys.push(KeyEntry {
            key,
            state: KeyState::Active,
        });
        entry
            .keys
            .sort_by_key(|held: &KeyEntry| held.key.to_openssh());
        Ok(())
    }

    /// Retires an active key: it signs no new records from the log entry `from` on, and the
    /// records before stay valid.
    ///
    /// # Errors
    ///
    /// [`RegisterError::UnknownIdentity`], [`RegisterError::UnknownKey`],
    /// [`RegisterError::NotActive`] if the key is retired or revoked already,
    /// [`RegisterError::NoOwnerKey`] if the register would have no active owner key left (the
    /// repository could then never change its register again), and [`RegisterError::ZeroPosition`].
    pub fn retire(
        &mut self,
        identity: &Identity,
        key: &PublicKey,
        from: u64,
    ) -> Result<(), RegisterError> {
        self.change(identity, key, from, |state| match state {
            KeyState::Active => Ok(KeyState::Retired { from }),
            _ => Err(RegisterError::NotActive),
        })
    }

    /// Revokes a key that is active or retired. A revoked key counts for the log entries below
    /// `from` only, and not for approvals. A key that is retired from an earlier entry keeps the
    /// earlier bound.
    ///
    /// # Errors
    ///
    /// As [`retire`](Self::retire), with [`RegisterError::AlreadyRevoked`] for a revoked key.
    pub fn revoke(
        &mut self,
        identity: &Identity,
        key: &PublicKey,
        from: u64,
    ) -> Result<(), RegisterError> {
        self.change(identity, key, from, |state| match state {
            KeyState::Active => Ok(KeyState::Revoked { from }),
            KeyState::Retired { from: retired } => Ok(KeyState::Revoked {
                from: retired.min(from),
            }),
            KeyState::Revoked { .. } => Err(RegisterError::AlreadyRevoked),
        })
    }

    fn change(
        &mut self,
        identity: &Identity,
        key: &PublicKey,
        from: u64,
        next: impl FnOnce(KeyState) -> Result<KeyState, RegisterError>,
    ) -> Result<(), RegisterError> {
        if from == 0 {
            return Err(RegisterError::ZeroPosition);
        }
        let mut changed = self.clone();
        let entry = changed.entry_mut(identity)?;
        let held = entry
            .keys
            .iter_mut()
            .find(|held| &held.key == key)
            .ok_or(RegisterError::UnknownKey)?;
        held.state = next(held.state)?;
        if !changed.has_active_owner_key() {
            return Err(RegisterError::NoOwnerKey);
        }
        *self = changed;
        Ok(())
    }

    /// Reads a register from the bytes of the file.
    ///
    /// # Errors
    ///
    /// [`RegisterParseError`] with the number of the line, for anything that
    /// [`encode`](Self::encode) does not write.
    pub fn parse(bytes: &[u8]) -> Result<Self, RegisterParseError> {
        let fail = |line: usize, problem: RegisterProblem| RegisterParseError {
            line: (line > 0).then_some(line),
            problem,
        };
        if bytes.len() > MAX_BYTES {
            return Err(fail(0, RegisterProblem::TooLarge));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| fail(0, RegisterProblem::NotUtf8))?;
        if !text.is_empty() && !text.ends_with('\n') {
            return Err(fail(0, RegisterProblem::NoFinalNewline));
        }
        let mut lines = text
            .split_inclusive('\n')
            .enumerate()
            .map(|(index, raw)| (index + 1, raw.strip_suffix('\n').unwrap_or(raw)));
        match lines.next() {
            Some((_, HEADER)) => {}
            Some((number, _)) => return Err(fail(number, RegisterProblem::BadHeader)),
            None => return Err(fail(0, RegisterProblem::BadHeader)),
        }
        let mut entries: Vec<RegisterEntry> = Vec::new();
        for (number, line) in lines {
            if line.is_empty() || line.ends_with(' ') || line.contains('\r') {
                return Err(fail(number, RegisterProblem::BadLine));
            }
            let words: Vec<&str> = line.split(' ').collect();
            match words.as_slice() {
                ["entry", kind, id, role] => {
                    let kind = match *kind {
                        "human" => IdentityKind::Human,
                        "agent" => IdentityKind::Agent,
                        _ => return Err(fail(number, RegisterProblem::BadEntry)),
                    };
                    let identity = (match kind {
                        IdentityKind::Human => Actor::human(*id).map(|a| Identity::of(&a)),
                        IdentityKind::Agent => {
                            Actor::agent(*id, "m", "s", "h").map(|a| Identity::of(&a))
                        }
                    })
                    .map_err(|_| fail(number, RegisterProblem::BadEntry))?;
                    let role = match *role {
                        "owner" if kind == IdentityKind::Human => Role::Owner,
                        "owner" => return Err(fail(number, RegisterProblem::AgentOwner)),
                        "member" => Role::Member,
                        _ => return Err(fail(number, RegisterProblem::BadEntry)),
                    };
                    if let Some(last) = entries.last() {
                        match last.identity.cmp(&identity) {
                            std::cmp::Ordering::Less => {}
                            std::cmp::Ordering::Equal => {
                                return Err(fail(number, RegisterProblem::Duplicate))
                            }
                            std::cmp::Ordering::Greater => {
                                return Err(fail(number, RegisterProblem::NotSorted))
                            }
                        }
                        if last.keys.is_empty() {
                            return Err(fail(number, RegisterProblem::NoKeys));
                        }
                    }
                    if entries.len() == MAX_ENTRIES {
                        return Err(fail(number, RegisterProblem::TooManyEntries));
                    }
                    entries.push(RegisterEntry {
                        identity,
                        role,
                        keys: Vec::new(),
                    });
                }
                ["key", algorithm, data, state @ ..] => {
                    let entry = entries
                        .last_mut()
                        .ok_or_else(|| fail(number, RegisterProblem::KeyOutsideEntry))?;
                    let key = PublicKey::from_openssh(&format!("{algorithm} {data}"))
                        .map_err(|_| fail(number, RegisterProblem::BadKey))?;
                    if key.to_openssh() != format!("{algorithm} {data}") {
                        return Err(fail(number, RegisterProblem::BadKey));
                    }
                    let state = match state {
                        ["active"] => KeyState::Active,
                        ["retired", from] => KeyState::Retired {
                            from: parse_position(from)
                                .ok_or_else(|| fail(number, RegisterProblem::BadState))?,
                        },
                        ["revoked", from] => KeyState::Revoked {
                            from: parse_position(from)
                                .ok_or_else(|| fail(number, RegisterProblem::BadState))?,
                        },
                        _ => return Err(fail(number, RegisterProblem::BadState)),
                    };
                    if let Some(last) = entry.keys.last() {
                        if last.key.to_openssh() >= key.to_openssh() {
                            return Err(fail(number, RegisterProblem::NotSorted));
                        }
                    }
                    if entry.keys.len() == MAX_KEYS {
                        return Err(fail(number, RegisterProblem::TooManyKeysInEntry));
                    }
                    entry.keys.push(KeyEntry { key, state });
                }
                _ => return Err(fail(number, RegisterProblem::BadLine)),
            }
        }
        let register = Self { entries };
        let mut seen = std::collections::HashSet::new();
        for held in register.entries.iter().flat_map(|entry| &entry.keys) {
            if !seen.insert(held.key.clone()) {
                return Err(fail(0, RegisterProblem::KeyShared));
            }
        }
        if register.entries.last().is_some_and(|e| e.keys.is_empty()) {
            return Err(fail(0, RegisterProblem::NoKeys));
        }
        if !register.has_active_owner_key() {
            return Err(fail(0, RegisterProblem::NoOwnerKey));
        }
        Ok(register)
    }

    /// The bytes of the file. [`parse`](Self::parse) reads them back.
    pub fn encode(&self) -> String {
        let mut text = format!("{HEADER}\n");
        for entry in &self.entries {
            let role = match entry.role {
                Role::Owner => "owner",
                Role::Member => "member",
            };
            text.push_str(&format!("entry {} {role}\n", entry.identity));
            for held in &entry.keys {
                let key = held.key.to_openssh();
                match held.state {
                    KeyState::Active => text.push_str(&format!("key {key} active\n")),
                    KeyState::Retired { from } => {
                        text.push_str(&format!("key {key} retired {from}\n"));
                    }
                    KeyState::Revoked { from } => {
                        text.push_str(&format!("key {key} revoked {from}\n"));
                    }
                }
            }
        }
        text
    }

    /// Checks the proof of a log entry: `signature` is a signature of a key of `actor` over the
    /// hash of the entry number `number`, and the key counts for that number.
    ///
    /// # Errors
    ///
    /// [`ProofError`] with the reason.
    pub fn verify_log_entry(
        &self,
        actor: &Actor,
        number: u64,
        entry_hash: &[u8],
        signature: &Signature,
    ) -> Result<Verified, ProofError> {
        self.verify(
            actor,
            PayloadKind::LogEntry,
            entry_hash,
            signature,
            Some(number),
        )
    }

    /// Checks the proof of an approval. A revoked key never counts; a retired one does.
    ///
    /// # Errors
    ///
    /// [`ProofError`] with the reason.
    pub fn verify_approval(
        &self,
        actor: &Actor,
        payload: &[u8],
        signature: &Signature,
    ) -> Result<Verified, ProofError> {
        self.verify(actor, PayloadKind::Approval, payload, signature, None)
    }

    fn verify(
        &self,
        actor: &Actor,
        kind: PayloadKind,
        payload: &[u8],
        signature: &Signature,
        number: Option<u64>,
    ) -> Result<Verified, ProofError> {
        let identity = Identity::of(actor);
        let entry = self
            .entry(&identity)
            .ok_or_else(|| ProofError::UnknownIdentity(identity.clone()))?;
        let key = signature.signing_key();
        let held = entry
            .keys
            .iter()
            .find(|held| held.key == key)
            .ok_or(ProofError::KeyNotOfActor)?;
        let counts = match (held.state, number) {
            (KeyState::Active, _) => true,
            (KeyState::Retired { from } | KeyState::Revoked { from }, Some(number)) => {
                number < from
            }
            (KeyState::Retired { .. }, None) => true,
            (KeyState::Revoked { .. }, None) => false,
        };
        if !counts {
            return Err(ProofError::KeyNoLongerCounts {
                state: held.state,
                number,
            });
        }
        signature::verify(&key, kind, signature::FORMAT_VERSION, payload, signature)
            .map_err(ProofError::Signature)?;
        Ok(Verified {
            actor: actor.clone(),
            key,
        })
    }

    /// Reads the register of a commit, the base commit of a check. `None` if the commit has no
    /// register file.
    ///
    /// # Errors
    ///
    /// [`ModeError::Store`] if an object cannot be read, and [`ModeError::Parse`] for a register
    /// that is not valid.
    pub fn at_commit(
        store: &dyn ObjectStore,
        commit: &crate::object::CommitId,
    ) -> Result<Option<Self>, ModeError> {
        let commit = store
            .get_commit(commit)
            .map_err(ModeError::Store)?
            .ok_or(ModeError::Missing)?;
        let mut tree = store
            .get_tree(&commit.tree())
            .map_err(ModeError::Store)?
            .ok_or(ModeError::Missing)?;
        let mut parts = FILE.split('/').peekable();
        while let Some(part) = parts.next() {
            let Some(found) = tree.entries().iter().find(|entry| entry.name() == part) else {
                return Ok(None);
            };
            let id: ObjectId = found.id();
            if parts.peek().is_some() {
                let Some(raw) = store
                    .get(&id)
                    .map_err(|e| ModeError::Store(ReadError::Store(Box::new(e))))?
                else {
                    return Err(ModeError::Missing);
                };
                if raw.kind() != Kind::Tree {
                    return Ok(None);
                }
                tree = store
                    .get_tree(&crate::object::TreeId::new(id))
                    .map_err(ModeError::Store)?
                    .ok_or(ModeError::Missing)?;
            } else {
                let Some(raw) = store
                    .get(&id)
                    .map_err(|e| ModeError::Store(ReadError::Store(Box::new(e))))?
                else {
                    return Err(ModeError::Missing);
                };
                if raw.kind() != Kind::Blob {
                    return Ok(None);
                }
                return Self::parse(raw.body()).map(Some).map_err(ModeError::Parse);
            }
        }
        Ok(None)
    }
}

fn parse_position(text: &str) -> Option<u64> {
    if text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// What is wrong with a register file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegisterProblem {
    /// The file is longer than [`MAX_BYTES`].
    TooLarge,
    /// The file is not UTF-8.
    NotUtf8,
    /// The last line does not end with a newline.
    NoFinalNewline,
    /// The first line is not `stemma-register 1`.
    BadHeader,
    /// An empty line, a space at the end, a carriage return, or a line of no known form.
    BadLine,
    /// An `entry` line with a kind, identifier, or role that is not valid.
    BadEntry,
    /// An agent with the role owner.
    AgentOwner,
    /// An entry stands twice.
    Duplicate,
    /// Entries or keys are not in ascending order.
    NotSorted,
    /// An entry without a key.
    NoKeys,
    /// A `key` line before the first entry.
    KeyOutsideEntry,
    /// A key that is not an Ed25519 key in its canonical text.
    BadKey,
    /// A state that is not `active`, `retired <n>`, or `revoked <n>`, or an `n` that is not a
    /// decimal number from 1 without a leading zero.
    BadState,
    /// More than [`MAX_ENTRIES`] entries.
    TooManyEntries,
    /// More than [`MAX_KEYS`] keys in an entry.
    TooManyKeysInEntry,
    /// A key stands in more than one entry.
    KeyShared,
    /// No owner has an active key.
    NoOwnerKey,
}

impl fmt::Display for RegisterProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "the register is larger than 64 KiB",
            Self::NotUtf8 => "the register is not UTF-8",
            Self::NoFinalNewline => "the last line does not end with a newline",
            Self::BadHeader => "the first line must be `stemma-register 1`",
            Self::BadLine => "not a line of the register",
            Self::BadEntry => "not an entry: `entry <human|agent> <id> <owner|member>`",
            Self::AgentOwner => "an agent cannot be an owner",
            Self::Duplicate => "the entry is listed twice",
            Self::NotSorted => "entries and keys must be sorted",
            Self::NoKeys => "an entry needs at least one key",
            Self::KeyOutsideEntry => "a key before the first entry",
            Self::BadKey => "not an Ed25519 key in OpenSSH form without a comment",
            Self::BadState => "the state is `active`, `retired <n>`, or `revoked <n>`",
            Self::TooManyEntries => "too many entries",
            Self::TooManyKeysInEntry => "too many keys in an entry",
            Self::KeyShared => "a key may belong to one entry only",
            Self::NoOwnerKey => "the register needs an owner with an active key",
        })
    }
}

/// The error of [`Register::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisterParseError {
    line: Option<usize>,
    problem: RegisterProblem,
}

impl RegisterParseError {
    /// The number of the line, if the problem has one.
    pub fn line(&self) -> Option<usize> {
        self.line
    }

    /// The problem.
    pub fn problem(&self) -> RegisterProblem {
        self.problem
    }
}

impl fmt::Display for RegisterParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line} of the register: {}", self.problem),
            None => write!(f, "the register: {}", self.problem),
        }
    }
}

impl Error for RegisterParseError {}

/// Why a change of the register is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegisterError {
    /// The owner is an agent.
    AgentOwner,
    /// The identity has no entry.
    UnknownIdentity(Identity),
    /// The entry does not hold the key.
    UnknownKey,
    /// Some entry holds the key already.
    KeyKnown,
    /// The entry has [`MAX_KEYS`] keys.
    TooManyKeys,
    /// The key is retired or revoked already.
    NotActive,
    /// The key is revoked already.
    AlreadyRevoked,
    /// The change leaves no owner with an active key.
    NoOwnerKey,
    /// A position is a number from 1.
    ZeroPosition,
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AgentOwner => f.write_str("an agent cannot be an owner"),
            Self::UnknownIdentity(identity) => {
                write!(f, "the register has no entry for {identity}")
            }
            Self::UnknownKey => f.write_str("the entry does not hold that key"),
            Self::KeyKnown => f.write_str("the register holds that key already"),
            Self::TooManyKeys => write!(f, "an entry holds at most {MAX_KEYS} keys"),
            Self::NotActive => f.write_str("the key is retired or revoked already"),
            Self::AlreadyRevoked => f.write_str("the key is revoked already"),
            Self::NoOwnerKey => f.write_str(
                "the change would leave no owner with an active key; add the new key first",
            ),
            Self::ZeroPosition => f.write_str("a position in the log starts at 1"),
        }
    }
}

impl Error for RegisterError {}

/// Why a proof does not count.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProofError {
    /// The register has no entry for the identity of the actor.
    UnknownIdentity(Identity),
    /// The key of the signature is not a key of the actor.
    KeyNotOfActor,
    /// The key is retired or revoked, and the proof stands at or after its bound, or is an
    /// approval of a revoked key.
    KeyNoLongerCounts {
        /// The state of the key.
        state: KeyState,
        /// The number of the log entry, or `None` for an approval.
        number: Option<u64>,
    },
    /// The signature does not verify.
    Signature(signature::VerifyError),
}

impl fmt::Display for ProofError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownIdentity(identity) => {
                write!(f, "the register has no entry for {identity}")
            }
            Self::KeyNotOfActor => {
                f.write_str("the key of the signature is not a key of the actor")
            }
            Self::KeyNoLongerCounts { state, number } => match (state, number) {
                (KeyState::Revoked { from }, Some(number)) => write!(
                    f,
                    "the key is revoked from entry {from}, and the proof is for entry {number}"
                ),
                (KeyState::Retired { from }, Some(number)) => write!(
                    f,
                    "the key is retired from entry {from}, and the proof is for entry {number}"
                ),
                _ => f.write_str("the key is revoked, so its approvals are void"),
            },
            Self::Signature(error) => write!(f, "{error}"),
        }
    }
}

impl Error for ProofError {}

/// The mode of a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// No register: the actor is a claim.
    Open,
    /// A register binds the actors to keys.
    Enforcing(Register),
}

impl Mode {
    /// The mode of a repository: the register of the commit that HEAD names, or, before the first
    /// commit, the register file of the working copy.
    ///
    /// # Errors
    ///
    /// [`ModeError::RegisterMissing`] if the marker says enforcing and no register is there, and
    /// the other errors of [`ModeError`].
    pub fn of(repository: &Repository) -> Result<Self, ModeError> {
        let marked = is_enforcing(&repository.metadata_dir()).map_err(ModeError::Io)?;
        let head = repository
            .refs()
            .resolve_head()
            .map_err(|e| ModeError::Head(e.to_string()))?;
        let register = match head {
            Some(commit) => Register::at_commit(repository.objects(), &commit)?,
            None => match fs::read(repository.path().join(FILE)) {
                Ok(bytes) => Some(Register::parse(&bytes).map_err(ModeError::Parse)?),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(ModeError::Io(e)),
            },
        };
        match (register, marked) {
            (Some(register), _) => Ok(Self::Enforcing(register)),
            (None, true) => Err(ModeError::RegisterMissing),
            (None, false) => Ok(Self::Open),
        }
    }
}

/// Why the mode of a repository cannot be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum ModeError {
    /// The repository is enforcing, but its base has no register. The step to the enforcing mode
    /// has no way back, so this is an error and not the open mode.
    RegisterMissing,
    /// The register is not valid.
    Parse(RegisterParseError),
    /// An object cannot be read.
    Store(ReadError),
    /// A commit or tree that the history names is not in the store.
    Missing,
    /// HEAD cannot be resolved.
    Head(String),
    /// The file system fails.
    Io(io::Error),
}

impl fmt::Display for ModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegisterMissing => write!(
                f,
                "the repository is enforcing but its base commit has no {FILE}; \
                 the step to the enforcing mode has no way back"
            ),
            Self::Parse(error) => error.fmt(f),
            Self::Store(error) => error.fmt(f),
            Self::Missing => f.write_str("an object of the history is missing"),
            Self::Head(reason) => write!(f, "cannot resolve HEAD: {reason}"),
            Self::Io(error) => write!(f, "cannot read the mode: {error}"),
        }
    }
}

impl Error for ModeError {}

/// Whether the marker of the enforcing mode is in the metadata directory.
///
/// # Errors
///
/// An error of the file system other than a missing file.
pub fn is_enforcing(metadata_dir: &Path) -> io::Result<bool> {
    match fs::read_to_string(metadata_dir.join(MARKER)) {
        Ok(text) if text == MARKER_CONTENT => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the marker of the enforcing mode is not in the form that this program writes",
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Writes the register file into the working copy and sets the marker of the enforcing mode: the
/// step from the open to the enforcing mode, as `init --owner-key` takes it. Nothing removes the
/// marker. The register file is part of the first commit, which the caller makes.
///
/// # Errors
///
/// An error of the file system. An existing register file is an error
/// ([`io::ErrorKind::AlreadyExists`]) and stays as it is.
pub fn enter_enforcing(repository: &Repository, register: &Register) -> io::Result<()> {
    let path = repository.path().join(FILE);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    io::Write::write_all(&mut file, register.encode().as_bytes())?;
    file.sync_all()?;
    let marker = repository.metadata_dir().join(MARKER);
    fs::write(&marker, MARKER_CONTENT)?;
    Ok(())
}

/// The authorizer of a repository with a mode: open lets everything through (the check of who
/// acts is the claim), and enforcing asks for a [`Verified`] actor that is the actor of the call.
#[derive(Debug)]
pub struct RegisterAuthorizer<'a> {
    mode: &'a Mode,
    verified: Option<&'a Verified>,
}

impl<'a> RegisterAuthorizer<'a> {
    /// An authorizer for `mode`. `verified` is the actor whose proof was checked, if any.
    pub fn new(mode: &'a Mode, verified: Option<&'a Verified>) -> Self {
        Self { mode, verified }
    }
}

impl Authorizer for RegisterAuthorizer<'_> {
    fn authorize(&self, actor: &Actor, _operation: &OperationName, _target: &str) -> Decision {
        match (self.mode, self.verified) {
            (Mode::Open, _) => Decision::Allow,
            (Mode::Enforcing(_), Some(verified)) if verified.actor() == actor => Decision::Allow,
            (Mode::Enforcing(_), _) => Decision::Deny,
        }
    }
}

/// The proof of one log entry, as the file `proofs/<number>` holds it.
///
/// ```text
/// stemma-proof-file 1
/// actor human daniel
/// -----BEGIN SSH SIGNATURE-----
/// …
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofFile {
    /// The actor that claims the proof.
    pub actor: Actor,
    /// The signature over the hash of the entry.
    pub signature: Signature,
}

impl ProofFile {
    /// The text of the file.
    pub fn encode(&self) -> String {
        format!(
            "{PROOF_HEADER}\nactor {}\n{}",
            self.actor,
            self.signature.to_armored()
        )
    }

    /// Reads the text of a file.
    ///
    /// # Errors
    ///
    /// [`ProofFileError`] if the form is not the one that [`encode`](Self::encode) writes.
    pub fn parse(text: &str) -> Result<Self, ProofFileError> {
        let mut lines = text.splitn(3, '\n');
        if lines.next() != Some(PROOF_HEADER) {
            return Err(ProofFileError::Header);
        }
        let actor_line = lines
            .next()
            .and_then(|line| line.strip_prefix("actor "))
            .ok_or(ProofFileError::Actor)?;
        let actor = Actor::from_str(actor_line).map_err(|_| ProofFileError::Actor)?;
        let signature = Signature::from_armored(lines.next().unwrap_or(""))
            .map_err(|e| ProofFileError::Signature(e.to_string()))?;
        Ok(Self { actor, signature })
    }
}

/// Why a proof file is not read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProofFileError {
    /// The first line is not `stemma-proof-file 1`.
    Header,
    /// The second line is not `actor <actor>` with a valid actor.
    Actor,
    /// The signature is not valid; carries the message.
    Signature(String),
}

impl fmt::Display for ProofFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header => write!(f, "the first line must be `{PROOF_HEADER}`"),
            Self::Actor => f.write_str("the second line must be `actor <actor>`"),
            Self::Signature(reason) => write!(f, "{reason}"),
        }
    }
}

impl Error for ProofFileError {}

/// The path of the proof file of an entry.
pub fn proof_path(metadata_dir: &Path, number: u64) -> PathBuf {
    metadata_dir.join(PROOF_DIR).join(format!("{number:012}"))
}

/// Writes the proof of the entry `number`. The file is new: a proof is never replaced.
///
/// # Errors
///
/// An error of the file system, among others [`io::ErrorKind::AlreadyExists`].
pub fn write_proof(metadata_dir: &Path, number: u64, proof: &ProofFile) -> io::Result<()> {
    let path = proof_path(metadata_dir, number);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    io::Write::write_all(&mut file, proof.encode().as_bytes())?;
    file.sync_all()
}

/// A proof that does not hold.
#[derive(Debug)]
#[non_exhaustive]
pub enum CheckError {
    /// The directory or a file cannot be read.
    Io(io::Error),
    /// A name in the directory of proofs is not the number of an entry.
    UnexpectedFile(String),
    /// A proof file is not valid.
    File {
        /// The number of the entry.
        number: u64,
        /// The reason.
        error: ProofFileError,
    },
    /// A proof names an entry that is not in the log, or one that is not in the hash chain.
    NoSuchEntry {
        /// The number of the entry.
        number: u64,
    },
    /// The actor of the proof is not the actor of the entry.
    ActorMismatch {
        /// The number of the entry.
        number: u64,
    },
    /// The register does not accept the proof.
    Proof {
        /// The number of the entry.
        number: u64,
        /// The reason.
        error: ProofError,
    },
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "cannot read the proofs: {error}"),
            Self::UnexpectedFile(name) => {
                write!(f, "{name:?} in the directory of proofs is no entry number")
            }
            Self::File { number, error } => write!(f, "proof of entry {number}: {error}"),
            Self::NoSuchEntry { number } => {
                write!(
                    f,
                    "proof of entry {number}: no such entry in the hash chain"
                )
            }
            Self::ActorMismatch { number } => write!(
                f,
                "proof of entry {number}: the actor of the proof is not the actor of the entry"
            ),
            Self::Proof { number, error } => write!(f, "proof of entry {number}: {error}"),
        }
    }
}

impl Error for CheckError {}

/// Checks every proof file of the metadata directory against the register: the entry exists and is
/// in the hash chain, the actor is the actor of the entry, and the register accepts the signature
/// for the number. Returns how many proofs were checked. A directory without proofs is fine: no
/// operation needs a proof yet.
///
/// # Errors
///
/// The first [`CheckError`], in the order of the numbers.
pub fn check_proofs(
    metadata_dir: &Path,
    register: &Register,
    entries: &[Entry],
) -> Result<usize, CheckError> {
    let dir = metadata_dir.join(PROOF_DIR);
    let names = match fs::read_dir(&dir) {
        Ok(read) => read
            .map(|item| item.map(|item| item.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(CheckError::Io)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(CheckError::Io(e)),
    };
    let mut numbers = Vec::new();
    for name in names {
        let name = name.to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        match name.parse::<u64>() {
            Ok(number) if name.len() == 12 => numbers.push(number),
            _ => return Err(CheckError::UnexpectedFile(name)),
        }
    }
    numbers.sort_unstable();
    for &number in &numbers {
        let text = fs::read_to_string(proof_path(metadata_dir, number)).map_err(CheckError::Io)?;
        let proof = ProofFile::parse(&text).map_err(|error| CheckError::File { number, error })?;
        let entry = entries
            .iter()
            .find(|entry| entry.number() == number)
            .ok_or(CheckError::NoSuchEntry { number })?;
        let hash = entry.hash().ok_or(CheckError::NoSuchEntry { number })?;
        if entry.actor() != &proof.actor {
            return Err(CheckError::ActorMismatch { number });
        }
        register
            .verify_log_entry(
                &proof.actor,
                number,
                hash.to_string().as_bytes(),
                &proof.signature,
            )
            .map_err(|error| CheckError::Proof { number, error })?;
    }
    Ok(numbers.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::{SignError, Signer, NAMESPACE};
    use ssh_key::private::Ed25519Keypair;
    use ssh_key::{HashAlg, LineEnding, PrivateKey};

    struct Seeded(PrivateKey);

    impl Seeded {
        fn new(seed: u8) -> Self {
            Self(PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32])))
        }

        fn public(&self) -> PublicKey {
            PublicKey::from_openssh(&self.0.public_key().to_openssh().unwrap()).unwrap()
        }
    }

    impl Signer for Seeded {
        fn sign(&self, message: &[u8]) -> Result<Signature, SignError> {
            let pem = self
                .0
                .sign(NAMESPACE, HashAlg::Sha512, message)
                .unwrap()
                .to_pem(LineEnding::LF)
                .unwrap();
            Ok(Signature::from_armored(&pem).unwrap())
        }
    }

    fn daniel() -> Actor {
        Actor::human("daniel").unwrap()
    }

    fn mia() -> Identity {
        Identity::human("mia").unwrap()
    }

    /// Daniel owns the register; Mia is a member with the key of seed 2.
    fn register() -> Register {
        let text = format!(
            "{HEADER}\nentry human daniel owner\nkey {} active\nentry human mia member\nkey {} active\n",
            Seeded::new(1).public(),
            Seeded::new(2).public()
        );
        Register::parse(text.as_bytes()).unwrap()
    }

    fn problem(text: &str) -> RegisterProblem {
        Register::parse(text.as_bytes()).unwrap_err().problem()
    }

    #[test]
    fn a_register_is_written_as_it_is_read() {
        let register = register();
        assert_eq!(
            Register::parse(register.encode().as_bytes()).unwrap(),
            register
        );
        let owner = Register::with_owner(Identity::of(&daniel()), Seeded::new(1).public()).unwrap();
        assert_eq!(Register::parse(owner.encode().as_bytes()).unwrap(), owner);
    }

    #[test]
    fn the_reader_is_strict() {
        let key1 = Seeded::new(1).public();
        let key2 = Seeded::new(2).public();
        let ok = format!("{HEADER}\nentry human daniel owner\nkey {key1} active\n");
        assert!(Register::parse(ok.as_bytes()).is_ok());
        assert_eq!(problem(""), RegisterProblem::BadHeader);
        assert_eq!(problem("stemma-register 2\n"), RegisterProblem::BadHeader);
        assert_eq!(
            problem(&ok[..ok.len() - 1]),
            RegisterProblem::NoFinalNewline
        );
        assert_eq!(
            problem(&ok.replace('\n', "\r\n")),
            RegisterProblem::BadHeader
        );
        assert_eq!(
            problem(&ok.replace("active\n", "active\r\n")),
            RegisterProblem::BadLine
        );
        assert_eq!(problem(&format!("{ok}\n")), RegisterProblem::BadLine);
        assert_eq!(problem(&format!("{ok}key x \n")), RegisterProblem::BadLine);
        assert_eq!(
            problem(&format!("{HEADER}\nkey {key1} active\n")),
            RegisterProblem::KeyOutsideEntry
        );
        assert_eq!(
            problem(&format!("{HEADER}\nentry human daniel owner\n")),
            RegisterProblem::NoKeys
        );
        assert_eq!(
            problem(&format!(
                "{HEADER}\nentry agent bot owner\nkey {key1} active\n"
            )),
            RegisterProblem::AgentOwner
        );
        // The same key in two entries.
        assert_eq!(
            problem(&format!("{ok}entry human mia member\nkey {key1} active\n")),
            RegisterProblem::KeyShared
        );
        // Entries out of order and twice.
        assert_eq!(
            problem(&format!(
                "{HEADER}\nentry human mia member\nkey {key2} active\nentry human daniel owner\nkey {key1} active\n"
            )),
            RegisterProblem::NotSorted
        );
        assert_eq!(
            problem(&format!(
                "{ok}entry human daniel member\nkey {key2} active\n"
            )),
            RegisterProblem::Duplicate
        );
        // A state with a leading zero, a missing number, and an unknown word.
        for state in [
            "retired 01",
            "retired",
            "revoked 0x",
            "paused 3",
            "active 1",
        ] {
            assert_eq!(
                problem(&format!(
                    "{HEADER}\nentry human daniel owner\nkey {key1} active\nentry human mia member\nkey {key2} {state}\n"
                )),
                RegisterProblem::BadState,
                "{state}"
            );
        }
        // An entry without a key, in the middle.
        assert_eq!(
            problem(&format!(
                "{ok}entry human mia member\nentry human zed member\nkey {key2} active\n"
            )),
            RegisterProblem::NoKeys
        );
        // A key with a comment is not canonical.
        assert_eq!(
            problem(&format!(
                "{HEADER}\nentry human daniel owner\nkey {key1} extra active\n"
            )),
            RegisterProblem::BadState
        );
        assert_eq!(
            problem(&"x".repeat(MAX_BYTES + 1)),
            RegisterProblem::TooLarge
        );
    }

    #[test]
    fn the_error_names_the_line() {
        let error = Register::parse(format!("{HEADER}\nbogus\n").as_bytes()).unwrap_err();
        assert_eq!(error.line(), Some(2));
        assert!(error.to_string().starts_with("line 2 of the register"));
    }

    #[test]
    fn a_proof_counts_for_the_actor_with_an_active_key() {
        let register = register();
        let signer = Seeded::new(1);
        let signature = signature::sign(&signer, PayloadKind::LogEntry, b"b3:abc").unwrap();
        let verified = register
            .verify_log_entry(&daniel(), 5, b"b3:abc", &signature)
            .unwrap();
        assert_eq!(verified.actor(), &daniel());
        assert_eq!(verified.key(), &signer.public());
    }

    #[test]
    fn a_proof_of_another_identity_or_payload_is_refused() {
        let register = register();
        let signature = signature::sign(&Seeded::new(1), PayloadKind::LogEntry, b"b3:abc").unwrap();
        // The key of Daniel does not speak for Mia.
        assert_eq!(
            register
                .verify_log_entry(&Actor::human("mia").unwrap(), 5, b"b3:abc", &signature)
                .unwrap_err(),
            ProofError::KeyNotOfActor
        );
        assert!(matches!(
            register.verify_log_entry(&Actor::human("zed").unwrap(), 5, b"b3:abc", &signature),
            Err(ProofError::UnknownIdentity(_))
        ));
        assert!(matches!(
            register.verify_log_entry(&daniel(), 5, b"b3:other", &signature),
            Err(ProofError::Signature(_))
        ));
        // A signature of a log entry is no approval.
        assert!(matches!(
            register.verify_approval(&daniel(), b"b3:abc", &signature),
            Err(ProofError::Signature(_))
        ));
        // A key that the register does not hold.
        let stranger = signature::sign(&Seeded::new(9), PayloadKind::LogEntry, b"x").unwrap();
        assert_eq!(
            register
                .verify_log_entry(&daniel(), 5, b"x", &stranger)
                .unwrap_err(),
            ProofError::KeyNotOfActor
        );
    }

    #[test]
    fn a_retired_key_keeps_its_old_signatures_and_signs_no_new_ones() {
        let mut register = register();
        let signer = Seeded::new(2);
        register.retire(&mia(), &signer.public(), 10).unwrap();
        let old = signature::sign(&signer, PayloadKind::LogEntry, b"h").unwrap();
        let mia_actor = Actor::human("mia").unwrap();
        assert!(register.verify_log_entry(&mia_actor, 9, b"h", &old).is_ok());
        assert!(matches!(
            register.verify_log_entry(&mia_actor, 10, b"h", &old),
            Err(ProofError::KeyNoLongerCounts { .. })
        ));
        // An approval is bound to a commit, not to a position: it stays valid.
        let approval = signature::sign(&signer, PayloadKind::Approval, b"c").unwrap();
        assert!(register
            .verify_approval(&mia_actor, b"c", &approval)
            .is_ok());
        // The state is kept in the file.
        assert!(register.encode().contains(" retired 10\n"));
        assert_eq!(
            Register::parse(register.encode().as_bytes()).unwrap(),
            register
        );
    }

    #[test]
    fn a_revoked_key_counts_only_before_the_revocation_and_voids_approvals() {
        let mut register = register();
        let signer = Seeded::new(2);
        register.revoke(&mia(), &signer.public(), 20).unwrap();
        let mia_actor = Actor::human("mia").unwrap();
        let entry = signature::sign(&signer, PayloadKind::LogEntry, b"h").unwrap();
        assert!(register
            .verify_log_entry(&mia_actor, 19, b"h", &entry)
            .is_ok());
        assert!(matches!(
            register.verify_log_entry(&mia_actor, 20, b"h", &entry),
            Err(ProofError::KeyNoLongerCounts { .. })
        ));
        let approval = signature::sign(&signer, PayloadKind::Approval, b"c").unwrap();
        assert_eq!(
            register
                .verify_approval(&mia_actor, b"c", &approval)
                .unwrap_err(),
            ProofError::KeyNoLongerCounts {
                state: KeyState::Revoked { from: 20 },
                number: None
            }
        );
    }

    #[test]
    fn revoking_a_retired_key_keeps_the_earlier_bound_and_a_revoked_key_stays_revoked() {
        let mut register = register();
        let key = Seeded::new(2).public();
        register.retire(&mia(), &key, 5).unwrap();
        register.revoke(&mia(), &key, 30).unwrap();
        assert_eq!(
            register.entry(&mia()).unwrap().keys()[0].state,
            KeyState::Revoked { from: 5 }
        );
        assert_eq!(
            register.revoke(&mia(), &key, 31),
            Err(RegisterError::AlreadyRevoked)
        );
        assert_eq!(
            register.retire(&mia(), &key, 31),
            Err(RegisterError::NotActive)
        );
    }

    #[test]
    fn a_change_that_would_leave_no_owner_key_is_refused_and_changes_nothing() {
        let mut register = register();
        let owner = Identity::of(&daniel());
        let key = Seeded::new(1).public();
        let before = register.clone();
        assert_eq!(
            register.revoke(&owner, &key, 3),
            Err(RegisterError::NoOwnerKey)
        );
        assert_eq!(register, before);
        // A rotation: the new key first, then the old one goes.
        register.add_key(&owner, Seeded::new(3).public()).unwrap();
        register.retire(&owner, &key, 3).unwrap();
        assert_eq!(
            register.add_key(&owner, key.clone()),
            Err(RegisterError::KeyKnown)
        );
        assert_eq!(
            register.retire(&owner, &Seeded::new(8).public(), 3),
            Err(RegisterError::UnknownKey)
        );
        assert_eq!(
            register.retire(&Identity::human("nobody").unwrap(), &key, 3),
            Err(RegisterError::UnknownIdentity(
                Identity::human("nobody").unwrap()
            ))
        );
        assert_eq!(
            register.retire(&owner, &Seeded::new(3).public(), 0),
            Err(RegisterError::ZeroPosition)
        );
    }

    #[test]
    fn an_agent_cannot_own_the_register() {
        let agent = Identity::of(&Actor::agent("bot", "m", "s", "daniel").unwrap());
        assert_eq!(
            Register::with_owner(agent, Seeded::new(1).public()),
            Err(RegisterError::AgentOwner)
        );
        assert!(register().is_owner(&daniel()));
        assert!(!register().is_owner(&Actor::human("mia").unwrap()));
    }

    #[test]
    fn the_authorizer_of_an_enforcing_repository_wants_a_verified_actor() {
        let register = register();
        let signature = signature::sign(&Seeded::new(1), PayloadKind::Approval, b"c").unwrap();
        let verified = register
            .verify_approval(&daniel(), b"c", &signature)
            .unwrap();
        let operation = OperationName::try_new("commit").unwrap();
        let open = Mode::Open;
        let enforcing = Mode::Enforcing(register);
        assert_eq!(
            RegisterAuthorizer::new(&open, None).authorize(&daniel(), &operation, "x"),
            Decision::Allow
        );
        assert_eq!(
            RegisterAuthorizer::new(&enforcing, None).authorize(&daniel(), &operation, "x"),
            Decision::Deny
        );
        assert_eq!(
            RegisterAuthorizer::new(&enforcing, Some(&verified)).authorize(
                &daniel(),
                &operation,
                "x"
            ),
            Decision::Allow
        );
        // The proof of Daniel does not carry another actor.
        assert_eq!(
            RegisterAuthorizer::new(&enforcing, Some(&verified)).authorize(
                &Actor::human("mia").unwrap(),
                &operation,
                "x"
            ),
            Decision::Deny
        );
    }

    #[test]
    fn a_proof_file_is_written_as_it_is_read() {
        let signature = signature::sign(&Seeded::new(1), PayloadKind::LogEntry, b"h").unwrap();
        let proof = ProofFile {
            actor: daniel(),
            signature,
        };
        assert_eq!(ProofFile::parse(&proof.encode()).unwrap(), proof);
        assert_eq!(ProofFile::parse("x\n").unwrap_err(), ProofFileError::Header);
        assert_eq!(
            ProofFile::parse(&format!("{PROOF_HEADER}\nwho\n")).unwrap_err(),
            ProofFileError::Actor
        );
        assert!(matches!(
            ProofFile::parse(&format!(
                "{PROOF_HEADER}\nactor human daniel\nnot a signature"
            )),
            Err(ProofFileError::Signature(_))
        ));
    }

    /// A repository made by `init`, with the register of [`register`] written by
    /// [`enter_enforcing`]; the first log entry is in the chain.
    fn enforcing_repository() -> (crate::testutil::TempDir, Repository) {
        let dir = crate::testutil::TempDir::new();
        let repository = Repository::init(
            dir.path(),
            &crate::repository::InitOptions::default(),
            &daniel(),
            &crate::AllowAll,
        )
        .unwrap();
        enter_enforcing(&repository, &register()).unwrap();
        (dir, repository)
    }

    #[test]
    fn the_mode_follows_the_register_file_and_never_goes_back_to_open() {
        let dir = crate::testutil::TempDir::new();
        let repository = Repository::init(
            dir.path(),
            &crate::repository::InitOptions::default(),
            &daniel(),
            &crate::AllowAll,
        )
        .unwrap();
        assert_eq!(Mode::of(&repository).unwrap(), Mode::Open);
        enter_enforcing(&repository, &register()).unwrap();
        assert_eq!(Mode::of(&repository).unwrap(), Mode::Enforcing(register()));
        // The register is never replaced by a second start.
        assert_eq!(
            enter_enforcing(&repository, &register())
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        // Without the file the marker still holds: the repository is damaged, not open.
        fs::remove_file(dir.path().join(FILE)).unwrap();
        assert!(matches!(
            Mode::of(&repository),
            Err(ModeError::RegisterMissing)
        ));
        // A file that is not a register is an error as well.
        fs::write(dir.path().join(FILE), "garbage").unwrap();
        assert!(matches!(Mode::of(&repository), Err(ModeError::Parse(_))));
    }

    #[test]
    fn a_marker_in_another_form_is_an_error() {
        let dir = crate::testutil::TempDir::new();
        fs::write(dir.path().join(MARKER), "yes").unwrap();
        assert!(is_enforcing(dir.path()).is_err());
        assert!(!is_enforcing(&dir.path().join("none")).unwrap());
    }

    #[test]
    fn the_proofs_of_the_log_are_checked_against_the_register() {
        let (dir, repository) = enforcing_repository();
        let metadata = repository.metadata_dir();
        let entries = repository.oplog().verify().unwrap();
        let entry = &entries[0];
        let hash = entry.hash().expect("the first entry is in the chain");
        assert_eq!(check_proofs(&metadata, &register(), &entries).unwrap(), 0);

        let signer = Seeded::new(1);
        let signature =
            signature::sign(&signer, PayloadKind::LogEntry, hash.to_string().as_bytes()).unwrap();
        let proof = ProofFile {
            actor: daniel(),
            signature,
        };
        write_proof(&metadata, entry.number(), &proof).unwrap();
        assert_eq!(
            write_proof(&metadata, entry.number(), &proof)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(check_proofs(&metadata, &register(), &entries).unwrap(), 1);

        // The same key, once it is revoked from this entry on, no longer counts.
        let mut revoked = register();
        revoked
            .add_key(&Identity::of(&daniel()), Seeded::new(3).public())
            .unwrap();
        revoked
            .revoke(&Identity::of(&daniel()), &signer.public(), entry.number())
            .unwrap();
        assert!(matches!(
            check_proofs(&metadata, &revoked, &entries),
            Err(CheckError::Proof { .. })
        ));
        // From the next entry on it still counts for this one.
        let mut later = register();
        later
            .add_key(&Identity::of(&daniel()), Seeded::new(3).public())
            .unwrap();
        later
            .revoke(
                &Identity::of(&daniel()),
                &signer.public(),
                entry.number() + 1,
            )
            .unwrap();
        assert_eq!(check_proofs(&metadata, &later, &entries).unwrap(), 1);
        drop(dir);
    }

    #[test]
    fn a_proof_of_another_actor_or_for_no_entry_is_refused() {
        let (_dir, repository) = enforcing_repository();
        let metadata = repository.metadata_dir();
        let entries = repository.oplog().verify().unwrap();
        let hash = entries[0].hash().unwrap();
        let signature = signature::sign(
            &Seeded::new(2),
            PayloadKind::LogEntry,
            hash.to_string().as_bytes(),
        )
        .unwrap();
        // Mia signs for an entry that Daniel wrote.
        let proof = ProofFile {
            actor: Actor::human("mia").unwrap(),
            signature: signature.clone(),
        };
        write_proof(&metadata, 1, &proof).unwrap();
        assert!(matches!(
            check_proofs(&metadata, &register(), &entries),
            Err(CheckError::ActorMismatch { number: 1 })
        ));
        fs::remove_file(proof_path(&metadata, 1)).unwrap();
        // A proof for a number that is not in the log.
        write_proof(&metadata, 7, &proof).unwrap();
        assert!(matches!(
            check_proofs(&metadata, &register(), &entries),
            Err(CheckError::NoSuchEntry { number: 7 })
        ));
        fs::remove_file(proof_path(&metadata, 7)).unwrap();
        // A name that is no number.
        fs::write(metadata.join(PROOF_DIR).join("x"), "").unwrap();
        assert!(matches!(
            check_proofs(&metadata, &register(), &entries),
            Err(CheckError::UnexpectedFile(_))
        ));
    }
}
