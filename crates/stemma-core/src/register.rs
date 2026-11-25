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
//! `member`); the `key` lines that follow belong to it. The entry of an agent also names its
//! sponsor and carries the co-signature of the sponsor (see *Agents*). Entries are sorted by kind (humans first)
//! and then by identifier in byte order, the keys of an entry by their text, and no key stands
//! twice in the file, so a key names exactly one identity. Every entry has a key, and at least one
//! owner with an active key exists. Only a human can be an owner. The number after `retired` or
//! `revoked` is the number of the first log entry that the key no longer counts for.
//!
//! An owner adds a further human with [`Register::add_human`]: an entry with a role and a first
//! key, without a sponsor and without a co-signature. The owner signs the entry by approving the
//! commit that carries the file (level 3).
//!
//! # Agents
//!
//! An agent has an entry of its own, with its own keys and role (ADR-004, STEMMA-222). The entry
//! names the sponsor and carries the co-signature of the sponsor:
//!
//! ```text
//! entry agent bot member sponsor daniel
//! cosign <the SSHSIG signature of the sponsor, as one line of base64>
//! key ssh-ed25519 AAAAC3Nzd… active
//! ```
//!
//! - The sponsor is a human entry, never an agent, so there are no sponsor chains.
//! - The role of the agent does not exceed the role of the sponsor ([`Role`]).
//! - The sponsor signs [`sponsorship_payload`] with a key of the sponsor
//!   ([`PayloadKind::Sponsorship`]); the owner who changes the register signs the entry by approving the commit (level 3).
//! - The `sponsor` of an actor line must be the sponsor of the entry, or the proof does not count
//!   ([`ProofError::SponsorMismatch`]). The fields `model` and `session` stay descriptive.
//! - The co-signature binds agent, role, and sponsor, not a key and not the repository: the owner
//!   decides which key an agent gets, and a rotation keeps the signature. A register that was
//!   hand-written under a revoked co-signing key cannot give the agent an active key
//!   ([`RegisterProblem::SponsorRevoked`]).
//! - Revoking a key of a sponsor revokes every key of their agents from the same log entry
//!   ([`Register::revoke`]). Retiring a key does not: a rotation keeps the agents.
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
//! against a register. The proof of an entry `pr-approve` signs the pull request and the head
//! ([`pull::approval_payload`]) as [`PayloadKind::Approval`]; any other proof signs the hash of
//! its entry as [`PayloadKind::LogEntry`].

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::actor::{Actor, ActorRef};
use crate::object::{Kind, ObjectId};
use crate::oplog::{Entry, PullKind, Record, StubKind};
use crate::pull;
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

impl Role {
    /// The word of the role in the register file: `owner` or `member`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Member => "member",
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Owner => 1,
            Self::Member => 0,
        }
    }

    /// Whether this role is at most `limit`: the bound that the role of a sponsor sets for the
    /// role of their agent.
    pub const fn within(self, limit: Self) -> bool {
        self.rank() <= limit.rank()
    }
}

/// The bytes that the sponsor of an agent signs ([`PayloadKind::Sponsorship`]): the identity of
/// the agent, its role, and the sponsor. Keys are no part of it, so a rotation of the keys of the
/// agent keeps the co-signature.
pub fn sponsorship_payload(agent: &Identity, role: Role, sponsor: &Identity) -> Vec<u8> {
    format!("{agent}\nrole {}\nsponsor {sponsor}\n", role.name()).into_bytes()
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
    sponsor: Option<Identity>,
    cosign: Option<Signature>,
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

    /// The sponsor of an agent; `None` for a human.
    pub fn sponsor(&self) -> Option<&Identity> {
        self.sponsor.as_ref()
    }

    /// The co-signature of the sponsor of an agent; `None` for a human.
    pub fn cosign(&self) -> Option<&Signature> {
        self.cosign.as_ref()
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
                sponsor: None,
                cosign: None,
                keys: vec![KeyEntry {
                    key,
                    state: KeyState::Active,
                }],
            }],
        })
    }

    /// Adds the entry of a further human: the role and the first key. A human has no sponsor and
    /// no co-signature; the owner who calls this signs the entry by approving the commit that
    /// carries the file. The entry takes its place in the order of the file.
    ///
    /// # Errors
    ///
    /// [`RegisterError::NotAHuman`] if `human` is an agent, [`RegisterError::EntryKnown`],
    /// [`RegisterError::KeyKnown`] if any entry holds the key already, and
    /// [`RegisterError::TooManyEntries`]. The register stays as it was on an error.
    pub fn add_human(
        &mut self,
        human: Identity,
        role: Role,
        key: PublicKey,
    ) -> Result<(), RegisterError> {
        if human.kind != IdentityKind::Human {
            return Err(RegisterError::NotAHuman);
        }
        if self.entry(&human).is_some() {
            return Err(RegisterError::EntryKnown(human));
        }
        if self.holds(&key) {
            return Err(RegisterError::KeyKnown);
        }
        if self.entries.len() >= MAX_ENTRIES {
            return Err(RegisterError::TooManyEntries);
        }
        self.entries.push(RegisterEntry {
            identity: human,
            role,
            sponsor: None,
            cosign: None,
            keys: vec![KeyEntry {
                key,
                state: KeyState::Active,
            }],
        });
        self.entries.sort_by(|a, b| a.identity.cmp(&b.identity));
        Ok(())
    }

    /// Adds the entry of an agent: its role, its sponsor, the co-signature of the sponsor, and its
    /// first key. The owner who calls this signs the entry by approving the commit that carries
    /// the file.
    ///
    /// # Errors
    ///
    /// [`RegisterError::AgentOwner`] for the role owner, [`RegisterError::NotAnAgent`] if
    /// `agent` is a human, [`RegisterError::EntryKnown`], [`RegisterError::KeyKnown`],
    /// [`RegisterError::TooManyEntries`], [`RegisterError::UnknownIdentity`] if the sponsor has no
    /// entry, [`RegisterError::SponsorNotHuman`], [`RegisterError::RoleAboveSponsor`], and
    /// [`RegisterError::BadCosign`] if the signature is not a signature of an active key of the
    /// sponsor over [`sponsorship_payload`].
    pub fn add_agent(
        &mut self,
        agent: Identity,
        role: Role,
        sponsor: Identity,
        key: PublicKey,
        cosign: Signature,
    ) -> Result<(), RegisterError> {
        if agent.kind != IdentityKind::Agent {
            return Err(RegisterError::NotAnAgent);
        }
        if role == Role::Owner {
            return Err(RegisterError::AgentOwner);
        }
        if self.entry(&agent).is_some() {
            return Err(RegisterError::EntryKnown(agent));
        }
        if self.holds(&key) {
            return Err(RegisterError::KeyKnown);
        }
        if self.entries.len() >= MAX_ENTRIES {
            return Err(RegisterError::TooManyEntries);
        }
        self.check_sponsorship(&agent, role, &sponsor, &cosign, true)?;
        self.entries.push(RegisterEntry {
            identity: agent,
            role,
            sponsor: Some(sponsor),
            cosign: Some(cosign),
            keys: vec![KeyEntry {
                key,
                state: KeyState::Active,
            }],
        });
        self.entries.sort_by(|a, b| a.identity.cmp(&b.identity));
        Ok(())
    }

    /// Checks the sponsor of an agent and their co-signature. `active_only` asks for an active
    /// key of the sponsor (a new entry); a register that is read accepts any key that the sponsor
    /// holds, because the key may be retired or revoked since.
    fn check_sponsorship(
        &self,
        agent: &Identity,
        role: Role,
        sponsor: &Identity,
        cosign: &Signature,
        active_only: bool,
    ) -> Result<(), RegisterError> {
        let entry = self
            .entry(sponsor)
            .ok_or_else(|| RegisterError::UnknownIdentity(sponsor.clone()))?;
        if sponsor.kind != IdentityKind::Human {
            return Err(RegisterError::SponsorNotHuman);
        }
        if !role.within(entry.role) {
            return Err(RegisterError::RoleAboveSponsor);
        }
        let key = cosign.signing_key();
        let held = entry
            .keys
            .iter()
            .find(|held| held.key == key)
            .ok_or(RegisterError::BadCosign)?;
        if active_only && held.state != KeyState::Active {
            return Err(RegisterError::BadCosign);
        }
        signature::verify(
            &key,
            PayloadKind::Sponsorship,
            signature::FORMAT_VERSION,
            &sponsorship_payload(agent, role, sponsor),
            cosign,
        )
        .map_err(|_| RegisterError::BadCosign)
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

    /// Whether the sponsorship of an agent still holds: the key that co-signed the entry is not
    /// revoked, and the sponsor has an active key. Always true for a human. An agent whose
    /// sponsorship does not hold can have no active key, so a revocation of the sponsor stays in
    /// force until the agent gets a new identifier and a new co-signature.
    fn sponsorships_hold(&self) -> bool {
        self.entries.iter().all(|entry| {
            !entry.keys.iter().any(|held| held.state == KeyState::Active)
                || self.sponsor_stands(entry)
        })
    }

    fn sponsor_stands(&self, entry: &RegisterEntry) -> bool {
        let (Some(sponsor), Some(cosign)) = (&entry.sponsor, &entry.cosign) else {
            return true;
        };
        let Some(sponsor) = self.entry(sponsor) else {
            return false;
        };
        let cosigner = cosign.signing_key();
        sponsor
            .keys
            .iter()
            .any(|held| held.state == KeyState::Active)
            && sponsor
                .keys
                .iter()
                .all(|held| held.key != cosigner || !matches!(held.state, KeyState::Revoked { .. }))
    }

    /// Whether an agent whose sponsorship does not hold has no way to act: no active key, and no
    /// key that outlasts the revocation of the co-signing key (every key is revoked from that
    /// entry on at the latest).
    fn agent_is_shut_down(&self, entry: &RegisterEntry, has_active: bool) -> bool {
        if has_active {
            return false;
        }
        let bound = entry.cosign.as_ref().and_then(|cosign| {
            let cosigner = cosign.signing_key();
            self.entry(entry.sponsor.as_ref()?)?
                .keys
                .iter()
                .find_map(|held| match held.state {
                    KeyState::Revoked { from } if held.key == cosigner => Some(from),
                    _ => None,
                })
        });
        match bound {
            Some(bound) => entry
                .keys
                .iter()
                .all(|held| matches!(held.state, KeyState::Revoked { from } if from <= bound)),
            None => true,
        }
    }

    /// Adds an active key to an identity, for a rotation: the new key first, then
    /// [`retire`](Self::retire) of the old one.
    ///
    /// # Errors
    ///
    /// [`RegisterError::UnknownIdentity`], [`RegisterError::KeyKnown`] if any entry holds the key
    /// already, [`RegisterError::TooManyKeys`], and [`RegisterError::SponsorRevoked`] for an agent
    /// whose sponsorship no longer holds.
    pub fn add_key(&mut self, identity: &Identity, key: PublicKey) -> Result<(), RegisterError> {
        if self.holds(&key) {
            return Err(RegisterError::KeyKnown);
        }
        if self
            .entry(identity)
            .is_some_and(|entry| !self.sponsor_stands(entry))
        {
            return Err(RegisterError::SponsorRevoked);
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
    /// repository could then never change its register again), [`RegisterError::ZeroPosition`],
    /// and [`RegisterError::SponsorRevoked`] if a human sponsor would be left without an active key
    /// while an agent still has one: add the new key first, or revoke, which shuts the agents
    /// down.
    pub fn retire(
        &mut self,
        identity: &Identity,
        key: &PublicKey,
        from: u64,
    ) -> Result<(), RegisterError> {
        let mut changed = self.clone();
        changed.change(identity, key, from, |state| match state {
            KeyState::Active => Ok(KeyState::Retired { from }),
            _ => Err(RegisterError::NotActive),
        })?;
        if !changed.sponsorships_hold() {
            return Err(RegisterError::SponsorRevoked);
        }
        *self = changed;
        Ok(())
    }

    /// Revokes a key that is active or retired. A revoked key counts for the log entries below
    /// `from` only, and not for approvals. A key that is retired from an earlier entry keeps the
    /// earlier bound.
    ///
    /// Revoking a key of a human revokes every key of the agents that the human sponsors, from the
    /// same `from` (a key that is revoked or retired from an earlier entry keeps the earlier
    /// bound). Retiring a key does not cascade. The result lists the agents whose keys changed.
    ///
    /// # Errors
    ///
    /// As [`retire`](Self::retire), with [`RegisterError::AlreadyRevoked`] for a revoked key. The
    /// register stays as it was on an error.
    pub fn revoke(
        &mut self,
        identity: &Identity,
        key: &PublicKey,
        from: u64,
    ) -> Result<Vec<Identity>, RegisterError> {
        let mut changed = self.clone();
        changed.change(identity, key, from, |state| match state {
            KeyState::Active => Ok(KeyState::Revoked { from }),
            KeyState::Retired { from: retired } => Ok(KeyState::Revoked {
                from: retired.min(from),
            }),
            KeyState::Revoked { .. } => Err(RegisterError::AlreadyRevoked),
        })?;
        // The bound of the revoked key itself: a key that was retired earlier keeps its earlier
        // bound, and the agents follow it, or the reader would refuse the file.
        let bound = changed
            .entry(identity)
            .and_then(|entry| entry.keys.iter().find(|held| &held.key == key))
            .map_or(from, |held| match held.state {
                KeyState::Revoked { from } => from,
                _ => from,
            });
        let mut revoked = Vec::new();
        for entry in &mut changed.entries {
            if entry.sponsor.as_ref() != Some(identity) {
                continue;
            }
            let mut touched = false;
            for held in &mut entry.keys {
                let next = match held.state {
                    KeyState::Active => KeyState::Revoked { from: bound },
                    KeyState::Retired { from: earlier } => KeyState::Revoked {
                        from: earlier.min(bound),
                    },
                    KeyState::Revoked { from: earlier } if earlier > bound => {
                        KeyState::Revoked { from: bound }
                    }
                    KeyState::Revoked { .. } => continue,
                };
                held.state = next;
                touched = true;
            }
            if touched {
                revoked.push(entry.identity.clone());
            }
        }
        if !changed.sponsorships_hold() {
            return Err(RegisterError::SponsorRevoked);
        }
        *self = changed;
        Ok(revoked)
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
                ["entry", kind, id, role, tail @ ..] => {
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
                    let sponsor = match (kind, tail) {
                        (IdentityKind::Human, []) => None,
                        (IdentityKind::Agent, ["sponsor", sponsor]) => Some(
                            Actor::human(*sponsor)
                                .map(|a| Identity::of(&a))
                                .map_err(|_| fail(number, RegisterProblem::BadEntry))?,
                        ),
                        (IdentityKind::Agent, []) => {
                            return Err(fail(number, RegisterProblem::NoSponsor))
                        }
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
                        sponsor,
                        cosign: None,
                        keys: Vec::new(),
                    });
                }
                ["cosign", data] => {
                    let entry = entries
                        .last_mut()
                        .filter(|entry| entry.sponsor.is_some() && entry.keys.is_empty())
                        .ok_or_else(|| fail(number, RegisterProblem::CosignOutsideAgent))?;
                    if entry.cosign.is_some() {
                        return Err(fail(number, RegisterProblem::CosignOutsideAgent));
                    }
                    entry.cosign = Some(
                        signature_from_line(data)
                            .ok_or_else(|| fail(number, RegisterProblem::BadCosign))?,
                    );
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
        for entry in &register.entries {
            let (Some(sponsor), cosign) = (&entry.sponsor, &entry.cosign) else {
                continue;
            };
            let has_active = entry.keys.iter().any(|held| held.state == KeyState::Active);
            let Some(cosign) = cosign else {
                return Err(fail(0, RegisterProblem::NoCosign));
            };
            register
                .check_sponsorship(&entry.identity, entry.role, sponsor, cosign, false)
                .map_err(|error| {
                    fail(
                        0,
                        match error {
                            RegisterError::UnknownIdentity(_) => RegisterProblem::UnknownSponsor,
                            RegisterError::SponsorNotHuman => RegisterProblem::UnknownSponsor,
                            RegisterError::RoleAboveSponsor => RegisterProblem::RoleAboveSponsor,
                            _ => RegisterProblem::BadCosign,
                        },
                    )
                })?;
            if !register.sponsor_stands(entry) && !register.agent_is_shut_down(entry, has_active) {
                return Err(fail(0, RegisterProblem::SponsorRevoked));
            }
        }
        Ok(register)
    }

    /// The bytes of the file. [`parse`](Self::parse) reads them back.
    pub fn encode(&self) -> String {
        let mut text = format!("{HEADER}\n");
        for entry in &self.entries {
            let role = entry.role.name();
            text.push_str(&format!("entry {} {role}", entry.identity));
            if let Some(sponsor) = &entry.sponsor {
                text.push_str(&format!(" sponsor {}", sponsor.id));
            }
            text.push('\n');
            if let Some(cosign) = &entry.cosign {
                text.push_str(&format!("cosign {}\n", signature_to_line(cosign)));
            }
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

    /// Checks the proof of an approval. A revoked key never counts. A retired key counts for the
    /// approvals written before its retirement: `entry` is the number of the approval entry, and
    /// `None` is a new approval whose entry is not written yet, where the caller asks
    /// [`Register::is_active_key`] besides.
    ///
    /// # Errors
    ///
    /// [`ProofError`] with the reason.
    pub fn verify_approval(
        &self,
        actor: &Actor,
        payload: &[u8],
        signature: &Signature,
        entry: Option<u64>,
    ) -> Result<Verified, ProofError> {
        self.verify(actor, PayloadKind::Approval, payload, signature, entry)
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
        if let (ActorRef::Agent { sponsor, .. }, Some(registered)) = (actor.view(), &entry.sponsor)
        {
            if sponsor != registered.id {
                return Err(ProofError::SponsorMismatch {
                    claimed: sponsor.to_owned(),
                    registered: registered.id.clone(),
                });
            }
        }
        let key = signature.signing_key();
        let held = entry
            .keys
            .iter()
            .find(|held| held.key == key)
            .ok_or(ProofError::KeyNotOfActor)?;
        // The signature first: a key that no longer counts must not hide a forged proof.
        signature::verify(&key, kind, signature::FORMAT_VERSION, payload, signature)
            .map_err(ProofError::Signature)?;
        let approval = kind == PayloadKind::Approval;
        let counts = match (held.state, number) {
            (KeyState::Active, _) | (KeyState::Retired { .. }, None) => true,
            (KeyState::Retired { from }, Some(number)) => number < from,
            // The approvals of a revoked key are void, whatever their number.
            (KeyState::Revoked { .. }, _) if approval => false,
            (KeyState::Revoked { from }, Some(number)) => number < from,
            (KeyState::Revoked { .. }, None) => false,
        };
        if !counts {
            return Err(ProofError::KeyNoLongerCounts {
                state: held.state,
                number: number
                    .filter(|_| !(approval && matches!(held.state, KeyState::Revoked { .. }))),
            });
        }
        Ok(Verified {
            actor: actor.clone(),
            key,
        })
    }

    /// Whether `key` is an active key of `identity`: a retired or revoked key signs nothing new.
    pub fn is_active_key(&self, identity: &Identity, key: &PublicKey) -> bool {
        self.entry(identity).is_some_and(|entry| {
            entry
                .keys
                .iter()
                .any(|held| &held.key == key && held.state == KeyState::Active)
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
                    return Err(ModeError::NotRegisterFile);
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
                    return Err(ModeError::NotRegisterFile);
                }
                return Self::parse(raw.body()).map(Some).map_err(ModeError::Parse);
            }
        }
        Ok(None)
    }
}

const PEM_BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";
const PEM_END: &str = "-----END SSH SIGNATURE-----";

/// The body of an armored signature on one line, the form of a `cosign` line.
pub(crate) fn signature_to_line(signature: &Signature) -> String {
    signature
        .to_armored()
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect()
}

/// Reads [`signature_to_line`] back. The text must be the one that it writes, so one signature has
/// one encoding in the file.
pub(crate) fn signature_from_line(text: &str) -> Option<Signature> {
    if text.is_empty()
        || !text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
    {
        return None;
    }
    let mut armored = format!("{PEM_BEGIN}\n");
    for chunk in text.as_bytes().chunks(70) {
        armored.push_str(std::str::from_utf8(chunk).ok()?);
        armored.push('\n');
    }
    armored.push_str(PEM_END);
    armored.push('\n');
    let signature = Signature::from_armored(&armored).ok()?;
    (signature_to_line(&signature) == text).then_some(signature)
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
    /// An agent without `sponsor <human>`.
    NoSponsor,
    /// A `cosign` line that is not directly under the `entry` line of an agent, or stands twice.
    CosignOutsideAgent,
    /// A `cosign` that is not a signature in the form that the file writes.
    BadCosign,
    /// An agent without a `cosign` line.
    NoCosign,
    /// The sponsor of an agent has no entry, or is an agent.
    UnknownSponsor,
    /// An agent whose role is above the role of the sponsor.
    RoleAboveSponsor,
    /// An agent with an active key whose sponsorship does not hold (see [`Register::revoke`]).
    SponsorRevoked,
}

impl fmt::Display for RegisterProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "the register is larger than 64 KiB",
            Self::NotUtf8 => "the register is not UTF-8",
            Self::NoFinalNewline => "the last line does not end with a newline",
            Self::BadHeader => "the first line must be `stemma-register 1`",
            Self::BadLine => "not a line of the register",
            Self::BadEntry => {
                "not an entry: `entry human <id> <owner|member>` or \
                 `entry agent <id> member sponsor <human id>`"
            }
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
            Self::NoSponsor => "an agent needs `sponsor <human id>`",
            Self::CosignOutsideAgent => {
                "a `cosign` line belongs directly under the entry of an agent, once"
            }
            Self::BadCosign => {
                "the co-signature is not a signature of a key of the sponsor over the entry"
            }
            Self::NoCosign => "an agent needs the co-signature of its sponsor",
            Self::UnknownSponsor => "the sponsor of an agent must be a human entry",
            Self::RoleAboveSponsor => "the role of an agent cannot exceed the role of its sponsor",
            Self::SponsorRevoked => {
                "an agent with an active key needs a sponsor with an active key and a co-signing \
                 key that is not revoked"
            }
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
    /// The identity is a human, and the call needs an agent.
    NotAnAgent,
    /// The identity is an agent, and the call needs a human.
    NotAHuman,
    /// The register has an entry for the identity already.
    EntryKnown(Identity),
    /// The register has [`MAX_ENTRIES`] entries.
    TooManyEntries,
    /// The sponsor of an agent is an agent: there are no sponsor chains.
    SponsorNotHuman,
    /// The role of an agent exceeds the role of its sponsor.
    RoleAboveSponsor,
    /// The co-signature is no signature of an active key of the sponsor over the entry.
    BadCosign,
    /// The sponsorship of the agent does not hold: the key that co-signed is revoked, or the
    /// sponsor has no active key.
    SponsorRevoked,
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
            Self::NotAnAgent => f.write_str("the identity is not an agent"),
            Self::NotAHuman => f.write_str("the identity is not a human"),
            Self::EntryKnown(identity) => {
                write!(f, "the register has an entry for {identity} already")
            }
            Self::TooManyEntries => write!(f, "a register holds at most {MAX_ENTRIES} entries"),
            Self::SponsorNotHuman => {
                f.write_str("the sponsor of an agent is a human, never an agent")
            }
            Self::RoleAboveSponsor => {
                f.write_str("the role of an agent cannot exceed the role of its sponsor")
            }
            Self::SponsorRevoked => f.write_str(
                "the sponsorship of the agent does not hold: its co-signing key is revoked or the \
                 sponsor has no active key; an agent whose sponsorship was revoked stays shut \
                 down, so add a new agent with a new identifier and a new co-signature",
            ),
            Self::BadCosign => f.write_str(
                "the co-signature is not a signature of an active key of the sponsor over the \
                 entry of the agent",
            ),
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
    /// The actor line names another sponsor than the register does.
    SponsorMismatch {
        /// The sponsor of the actor line.
        claimed: String,
        /// The sponsor of the register entry.
        registered: String,
    },
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
            Self::SponsorMismatch {
                claimed,
                registered,
            } => write!(
                f,
                "the actor names {claimed} as its sponsor, and the register names {registered}"
            ),
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
    /// commit, the register file of the working copy. A bare repository has neither (ADR-009):
    /// the call fails with [`ModeError::Head`], and a caller reads the register of a commit with
    /// [`Register::at_commit`].
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
    /// The path of the register is taken by something that is not a register file, such as a
    /// directory or a link. It is no register, and no reason to fall back to the open mode.
    NotRegisterFile,
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
            Self::NotRegisterFile => write!(f, "{FILE} in the base commit is not a regular file"),
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
/// ([`io::ErrorKind::AlreadyExists`]) and stays as it is. A bare repository has no working copy
/// for the register file: the call is refused before it writes, with
/// [`io::ErrorKind::Unsupported`] and [`RefError::Bare`](crate::refs::RefError::Bare) as the
/// inner error. If the register file cannot be created,
/// the marker that this call set is removed again.
pub fn enter_enforcing(repository: &Repository, register: &Register) -> io::Result<()> {
    // The register file is a file of the working copy. A bare repository has none (ADR-009), and
    // its directory is the metadata directory, so the file would land among the stores.
    repository
        .require_working_copy("enter the enforcing mode")
        .map_err(|error| io::Error::new(io::ErrorKind::Unsupported, error))?;
    // The marker comes first and is flushed: a failure or a crash between the two steps leaves a
    // repository that is enforcing without a register (an error), never an open one that has a
    // register file.
    let metadata = repository.metadata_dir();
    let marker_path = metadata.join(MARKER);
    let marker_was_set = marker_path.exists();
    let mut marker = fs::File::create(&marker_path)?;
    io::Write::write_all(&mut marker, MARKER_CONTENT.as_bytes())?;
    marker.sync_all()?;
    fs::File::open(&metadata)?.sync_all()?;
    let path = repository.path().join(FILE);
    // As long as no register file exists, this call can take its own marker back. A failure after
    // the file exists leaves both in place: the repository is then damaged, not open.
    let created = (|| {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
    })();
    let mut file = match created {
        Ok(file) => file,
        Err(error) => {
            if !marker_was_set {
                // Best effort: if this fails as well, the marker stays and the repository is
                // reported as damaged, which is the safe side.
                let _ = fs::remove_file(&marker_path);
            }
            return Err(error);
        }
    };
    io::Write::write_all(&mut file, register.encode().as_bytes())?;
    file.sync_all()?;
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
            Ok(number) if name.len() == 12 && name.bytes().all(|b| b.is_ascii_digit()) => {
                numbers.push(number);
            }
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
        let approval = entry
            .pull()
            .filter(|pull| pull.kind() == PullKind::Approve)
            .and_then(|pull| Some(pull::approval_payload(pull.pull()?, pull.commit()?)));
        let checked = match &approval {
            // The proof of an approval signs the pull request and the head, not the hash of the
            // entry. A key that was revoked since makes the approval void, which the recount
            // shows; it is no damage of the log.
            Some(payload) => {
                match register.verify_approval(
                    &proof.actor,
                    payload,
                    &proof.signature,
                    Some(number),
                ) {
                    Err(ProofError::KeyNoLongerCounts { .. }) => Ok(()),
                    other => other.map(|_| ()),
                }
            }
            None => register
                .verify_log_entry(
                    &proof.actor,
                    number,
                    hash.to_string().as_bytes(),
                    &proof.signature,
                )
                .map(|_| ())
                // A purge may have replaced an approval with a stub, which keeps the hash but no
                // longer has what the proof signed (pull request and head). Only the stub of an
                // approval, or an older stub that does not say what it replaced, lets a signature
                // that does not verify through; the stub of any other entry keeps the hash that
                // the proof signed. A key that is not the actor's still fails.
                .or_else(|error| {
                    let lenient = match entry.record() {
                        Record::Stub(stub) => match stub.replaced() {
                            Some(StubKind::Other) => false,
                            Some(StubKind::Approval) | None => true,
                        },
                        _ => false,
                    };
                    if lenient && matches!(error, ProofError::Signature(_)) {
                        Ok(())
                    } else {
                        Err(error)
                    }
                }),
        };
        checked.map_err(|error| CheckError::Proof { number, error })?;
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
            register.verify_approval(&daniel(), b"b3:abc", &signature, None),
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
        // An approval is bound to a commit, not to a position: one from before the retirement
        // stays valid, and one from after it does not count.
        let approval = signature::sign(&signer, PayloadKind::Approval, b"c").unwrap();
        assert!(register
            .verify_approval(&mia_actor, b"c", &approval, Some(9))
            .is_ok());
        assert!(matches!(
            register.verify_approval(&mia_actor, b"c", &approval, Some(10)),
            Err(ProofError::KeyNoLongerCounts { .. })
        ));
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
                .verify_approval(&mia_actor, b"c", &approval, Some(5))
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
            .verify_approval(&daniel(), b"c", &signature, None)
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
    fn a_register_that_cannot_be_created_takes_its_marker_back() {
        let dir = crate::testutil::TempDir::new();
        let repository = Repository::init(
            dir.path(),
            &crate::repository::InitOptions::default(),
            &daniel(),
            &crate::AllowAll,
        )
        .unwrap();
        // A file where the directory of the register belongs: the register cannot be created.
        fs::write(dir.path().join(".stemma-actors"), "in the way").unwrap();
        assert!(enter_enforcing(&repository, &register()).is_err());
        assert!(!is_enforcing(&repository.metadata_dir()).unwrap());
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

    #[test]
    fn the_stub_of_an_entry_that_is_no_approval_keeps_its_proof_checked() {
        let (_dir, repository) = enforcing_repository();
        let metadata = repository.metadata_dir();
        let entries = repository.oplog().verify().unwrap();
        let stubs = [entries[0].to_stub().unwrap()];
        // Daniel's own key signs something other than the hash of the entry.
        let wrong =
            signature::sign(&Seeded::new(1), PayloadKind::LogEntry, b"not the hash").unwrap();
        let proof = ProofFile {
            actor: Actor::human("daniel").unwrap(),
            signature: wrong,
        };
        write_proof(&metadata, 1, &proof).unwrap();
        assert!(matches!(
            check_proofs(&metadata, &register(), &stubs),
            Err(CheckError::Proof { number: 1, .. })
        ));
        // The hash that the stub keeps is what the proof signs: the right proof is accepted, a
        // key that is not the actor's is refused.
        let hash = entries[0].hash().unwrap().to_string();
        let proof_of = |signer: u8| ProofFile {
            actor: Actor::human("daniel").unwrap(),
            signature: signature::sign(
                &Seeded::new(signer),
                PayloadKind::LogEntry,
                hash.as_bytes(),
            )
            .unwrap(),
        };
        fs::remove_file(proof_path(&metadata, 1)).unwrap();
        write_proof(&metadata, 1, &proof_of(1)).unwrap();
        assert_eq!(check_proofs(&metadata, &register(), &stubs).unwrap(), 1);
        fs::remove_file(proof_path(&metadata, 1)).unwrap();
        write_proof(&metadata, 1, &proof_of(9)).unwrap();
        assert!(matches!(
            check_proofs(&metadata, &register(), &stubs),
            Err(CheckError::Proof { number: 1, .. })
        ));
        // A stub from before the kind was kept does not say what it replaced, so a signature that
        // does not verify still passes, as for the stub of an approval.
        let legacy = {
            let text = String::from_utf8(stubs[0].encode()).unwrap();
            let text = text
                .replace("stemma-oplog 4", "stemma-oplog 3")
                .replace(" other\n", "\n");
            Entry::parse(text.as_bytes()).unwrap()
        };
        assert_eq!(legacy.hash(), stubs[0].hash());
        fs::remove_file(proof_path(&metadata, 1)).unwrap();
        write_proof(&metadata, 1, &proof).unwrap();
        assert_eq!(check_proofs(&metadata, &register(), &[legacy]).unwrap(), 1);
    }

    #[test]
    fn a_proof_of_an_approval_is_checked_by_its_payload() {
        use crate::oplog::{Chain, PullEntry, Record};
        let (_dir, repository) = enforcing_repository();
        let metadata = repository.metadata_dir();
        let head = repository.refs().working_copy().unwrap();
        let approve = |number: u64, previous: Chain| {
            let entry = PullEntry::approve(1, daniel(), 1, head).unwrap();
            Entry::linked(number, previous, Record::Pull(entry)).unwrap()
        };
        let entry = approve(2, Chain::Start);
        let entries = [entry.clone()];
        let sign = |kind, payload: &[u8]| signature::sign(&Seeded::new(1), kind, payload).unwrap();
        let proof_of = |signature| ProofFile {
            actor: daniel(),
            signature,
        };
        // The payload of the pull request and the head holds.
        let payload = pull::approval_payload(1, &head);
        write_proof(
            &metadata,
            2,
            &proof_of(sign(PayloadKind::Approval, &payload)),
        )
        .unwrap();
        assert_eq!(check_proofs(&metadata, &register(), &entries).unwrap(), 1);
        // A signature of the hash of the entry is the proof of another kind of entry.
        fs::remove_file(proof_path(&metadata, 2)).unwrap();
        let hash = entry.hash().unwrap().to_string();
        let wrong = sign(PayloadKind::LogEntry, hash.as_bytes());
        write_proof(&metadata, 2, &proof_of(wrong)).unwrap();
        assert!(matches!(
            check_proofs(&metadata, &register(), &entries),
            Err(CheckError::Proof { number: 2, .. })
        ));
        // The stub of an approval keeps no payload to check, so a signature that does not verify
        // is let through; a key that is not the actor's is not.
        fs::remove_file(proof_path(&metadata, 2)).unwrap();
        let stub = entry.to_stub().unwrap();
        let stubs = [stub];
        write_proof(
            &metadata,
            2,
            &proof_of(sign(PayloadKind::Approval, &payload)),
        )
        .unwrap();
        assert_eq!(check_proofs(&metadata, &register(), &stubs).unwrap(), 1);
        fs::remove_file(proof_path(&metadata, 2)).unwrap();
        let foreign = signature::sign(&Seeded::new(9), PayloadKind::Approval, &payload).unwrap();
        write_proof(&metadata, 2, &proof_of(foreign)).unwrap();
        assert!(matches!(
            check_proofs(&metadata, &register(), &stubs),
            Err(CheckError::Proof { number: 2, .. })
        ));
    }

    #[test]
    fn a_further_human_takes_its_place_in_the_order_of_the_file() {
        let mut register = with_bot();
        let ana = Identity::human("ana").unwrap();
        let zoe = Identity::human("zoe").unwrap();
        register
            .add_human(zoe.clone(), Role::Owner, Seeded::new(5).public())
            .unwrap();
        register
            .add_human(ana.clone(), Role::Member, Seeded::new(4).public())
            .unwrap();
        // Humans by identifier, then the agent: the order that the reader asks for.
        let order: Vec<String> = register
            .entries()
            .iter()
            .map(|entry| entry.identity().to_string())
            .collect();
        assert_eq!(
            order,
            [
                "human ana",
                "human daniel",
                "human mia",
                "human zoe",
                "agent bot"
            ]
        );
        let read = Register::parse(register.encode().as_bytes()).unwrap();
        assert_eq!(read, register);
        let entry = read.entry(&ana).unwrap();
        assert_eq!(entry.role(), Role::Member);
        assert_eq!(entry.sponsor(), None);
        assert!(entry.cosign().is_none());
        assert_eq!(entry.keys()[0].state, KeyState::Active);
        assert_eq!(read.entry(&zoe).unwrap().role(), Role::Owner);
    }

    #[test]
    fn a_further_human_needs_a_new_identifier_and_a_new_key() {
        let ana = Identity::human("ana").unwrap();
        let cases = [
            // An agent is added with a sponsor, by `add_agent`.
            (bot(), Seeded::new(4).public(), RegisterError::NotAHuman),
            // Mia has an entry.
            (
                mia(),
                Seeded::new(4).public(),
                RegisterError::EntryKnown(mia()),
            ),
            // The key of Mia names Mia and nobody else.
            (ana, Seeded::new(2).public(), RegisterError::KeyKnown),
        ];
        for (human, key, error) in cases {
            let mut changed = register();
            assert_eq!(changed.add_human(human, Role::Member, key), Err(error));
            assert_eq!(changed, register());
        }
    }

    #[test]
    fn a_full_register_takes_no_further_human() {
        // Daniel and Mia hold the keys of the seeds 1 and 2; every other seed fills the register.
        let mut register = register();
        for seed in (0..=u8::MAX).filter(|seed| !(1..=2).contains(seed)) {
            let human = Identity::human(&format!("h{seed}")).unwrap();
            register
                .add_human(human, Role::Member, Seeded::new(seed).public())
                .unwrap();
        }
        assert_eq!(register.entries().len(), MAX_ENTRIES);
        let key = PublicKey::from_openssh(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMt6BisKCOOMdkWowXYqXUrS1zKF4aE6QenzZ1SSB/Md",
        )
        .unwrap();
        assert_eq!(
            register.add_human(Identity::human("last").unwrap(), Role::Member, key),
            Err(RegisterError::TooManyEntries)
        );
    }

    fn bot() -> Identity {
        Identity::of(&Actor::agent("bot", "m", "s", "mia").unwrap())
    }

    fn cosign_of(signer: &Seeded, agent: &Identity, role: Role, sponsor: &Identity) -> Signature {
        signature::sign(
            signer,
            PayloadKind::Sponsorship,
            &sponsorship_payload(agent, role, sponsor),
        )
        .unwrap()
    }

    /// The register of the tests with the agent `bot`, sponsored by Mia (key of seed 2), key of
    /// seed 3.
    fn with_bot() -> Register {
        let mut register = register();
        let cosign = cosign_of(&Seeded::new(2), &bot(), Role::Member, &mia());
        register
            .add_agent(bot(), Role::Member, mia(), Seeded::new(3).public(), cosign)
            .unwrap();
        register
    }

    #[test]
    fn an_agent_entry_with_a_sponsor_is_written_as_it_is_read() {
        let register = with_bot();
        let text = register.encode();
        assert!(text.contains("entry agent bot member sponsor mia\ncosign "));
        let read = Register::parse(text.as_bytes()).unwrap();
        assert_eq!(read, register);
        let entry = read.entry(&bot()).unwrap();
        assert_eq!(entry.sponsor(), Some(&mia()));
        assert!(entry.cosign().is_some());
        // Humans first, then agents.
        assert_eq!(read.entries().last().unwrap().identity(), &bot());
    }

    #[test]
    fn an_agent_needs_a_human_sponsor_with_a_matching_cosign() {
        let cosign = cosign_of(&Seeded::new(2), &bot(), Role::Member, &mia());
        let key = Seeded::new(3).public();
        let add = |register: &mut Register, role, sponsor: Identity, cosign: Signature| {
            register.add_agent(bot(), role, sponsor, key.clone(), cosign)
        };
        // An agent cannot own.
        assert_eq!(
            add(&mut register(), Role::Owner, mia(), cosign.clone()),
            Err(RegisterError::AgentOwner)
        );
        // The sponsor needs an entry.
        let nobody = Identity::human("nobody").unwrap();
        assert_eq!(
            add(
                &mut register(),
                Role::Member,
                nobody.clone(),
                cosign.clone()
            ),
            Err(RegisterError::UnknownIdentity(nobody))
        );
        // Only the sponsor signs: the key of Daniel is no key of Mia.
        let wrong_signer = cosign_of(&Seeded::new(1), &bot(), Role::Member, &mia());
        assert_eq!(
            add(&mut register(), Role::Member, mia(), wrong_signer),
            Err(RegisterError::BadCosign)
        );
        // The signature is bound to the agent, the role, and the sponsor.
        let other_agent = cosign_of(
            &Seeded::new(2),
            &Identity::of(&Actor::agent("other", "m", "s", "mia").unwrap()),
            Role::Member,
            &mia(),
        );
        assert_eq!(
            add(&mut register(), Role::Member, mia(), other_agent),
            Err(RegisterError::BadCosign)
        );
        // A signature of another type does not count (domain separation).
        let approval = signature::sign(
            &Seeded::new(2),
            PayloadKind::Approval,
            &sponsorship_payload(&bot(), Role::Member, &mia()),
        )
        .unwrap();
        assert_eq!(
            add(&mut register(), Role::Member, mia(), approval),
            Err(RegisterError::BadCosign)
        );
        // No second entry, no key twice, and a human is no agent.
        let mut register = with_bot();
        assert_eq!(
            add(&mut register, Role::Member, mia(), cosign.clone()),
            Err(RegisterError::EntryKnown(bot()))
        );
        assert_eq!(
            register.add_agent(
                Identity::human("eve").unwrap(),
                Role::Member,
                mia(),
                key.clone(),
                cosign
            ),
            Err(RegisterError::NotAnAgent)
        );
    }

    #[test]
    fn there_are_no_sponsor_chains() {
        let mut register = with_bot();
        let second = Identity::of(&Actor::agent("second", "m", "s", "bot").unwrap());
        let cosign = cosign_of(&Seeded::new(3), &second, Role::Member, &bot());
        assert_eq!(
            register.add_agent(second, Role::Member, bot(), Seeded::new(4).public(), cosign),
            Err(RegisterError::SponsorNotHuman)
        );
    }

    #[test]
    fn the_role_of_an_agent_is_bound_by_the_role_of_its_sponsor() {
        assert!(Role::Member.within(Role::Member));
        assert!(Role::Member.within(Role::Owner));
        assert!(Role::Owner.within(Role::Owner));
        assert!(!Role::Owner.within(Role::Member));
        // A member sponsor with an agent that claims the owner role in a file that is read.
        let register = with_bot();
        let text = register
            .encode()
            .replace("entry agent bot member", "entry agent bot owner");
        assert_eq!(problem(&text), RegisterProblem::AgentOwner);
    }

    #[test]
    fn the_reader_refuses_an_agent_entry_that_is_not_complete_and_sound() {
        let good = with_bot().encode();
        let cosign_line = good
            .lines()
            .find(|l| l.starts_with("cosign "))
            .unwrap()
            .to_owned();
        // No sponsor.
        assert_eq!(
            problem(&good.replace(" sponsor mia", "")),
            RegisterProblem::NoSponsor
        );
        // A human with a sponsor.
        assert_eq!(
            problem(&good.replace(
                "entry human mia member",
                "entry human mia member sponsor daniel"
            )),
            RegisterProblem::BadEntry
        );
        // No co-signature.
        assert_eq!(
            problem(&good.replace(&format!("{cosign_line}\n"), "")),
            RegisterProblem::NoCosign
        );
        // Twice, or under a human.
        assert_eq!(
            problem(&good.replace(&cosign_line, &format!("{cosign_line}\n{cosign_line}"))),
            RegisterProblem::CosignOutsideAgent
        );
        assert_eq!(
            problem(&good.replace(
                "entry human mia member\n",
                &format!("entry human mia member\n{cosign_line}\n")
            )),
            RegisterProblem::CosignOutsideAgent
        );
        // Not a signature, or a signature by a key that the sponsor does not hold.
        assert_eq!(
            problem(&good.replace(&cosign_line, "cosign AAAA")),
            RegisterProblem::BadCosign
        );
        let by_daniel = cosign_of(&Seeded::new(1), &bot(), Role::Member, &mia());
        assert_eq!(
            problem(&good.replace(
                &cosign_line,
                &format!("cosign {}", signature_to_line(&by_daniel))
            )),
            RegisterProblem::BadCosign
        );
        // A sponsor that is not in the register.
        assert_eq!(
            problem(&good.replace("sponsor mia", "sponsor nobody")),
            RegisterProblem::UnknownSponsor
        );
        // The sponsor must be bound in the signature too: the same signature under another sponsor.
        assert_eq!(
            problem(&good.replace("sponsor mia", "sponsor daniel")),
            RegisterProblem::BadCosign
        );
    }

    #[test]
    fn revoking_a_sponsor_revokes_their_agents() {
        let mut register = with_bot();
        // A retired key of the agent keeps its earlier bound.
        register.add_key(&bot(), Seeded::new(4).public()).unwrap();
        register
            .retire(&bot(), &Seeded::new(3).public(), 5)
            .unwrap();
        let revoked = register
            .revoke(&mia(), &Seeded::new(2).public(), 20)
            .unwrap();
        assert_eq!(revoked, vec![bot()]);
        let states: Vec<KeyState> = register
            .entry(&bot())
            .unwrap()
            .keys()
            .iter()
            .map(|held| held.state)
            .collect();
        assert!(states.contains(&KeyState::Revoked { from: 5 }));
        assert!(states.contains(&KeyState::Revoked { from: 20 }));
        assert_eq!(states.len(), 2);
        // The revoked file is still a register that can be read.
        assert_eq!(
            Register::parse(register.encode().as_bytes()).unwrap(),
            register
        );
        // The proofs of the agent: a log entry before the bound counts, one after does not.
        let agent = Actor::agent("bot", "m", "s", "mia").unwrap();
        let signer = Seeded::new(4);
        let proof = signature::sign(&signer, PayloadKind::LogEntry, b"x").unwrap();
        assert!(register.verify_log_entry(&agent, 19, b"x", &proof).is_ok());
        assert!(matches!(
            register.verify_log_entry(&agent, 20, b"x", &proof),
            Err(ProofError::KeyNoLongerCounts { .. })
        ));
        // Approvals of a revoked key are void.
        let approval = signature::sign(&signer, PayloadKind::Approval, b"x").unwrap();
        assert!(register
            .verify_approval(&agent, b"x", &approval, Some(5))
            .is_err());
    }

    #[test]
    fn retiring_a_sponsor_key_keeps_the_agents() {
        let mut register = with_bot();
        register.add_key(&mia(), Seeded::new(5).public()).unwrap();
        register
            .retire(&mia(), &Seeded::new(2).public(), 9)
            .unwrap();
        let held = register.entry(&bot()).unwrap().keys();
        assert_eq!(held[0].state, KeyState::Active);
        // The co-signature stays valid with the retired key of the sponsor.
        assert!(Register::parse(register.encode().as_bytes()).is_ok());
    }

    #[test]
    fn a_failed_revoke_changes_nothing() {
        let mut register = with_bot();
        let before = register.clone();
        assert_eq!(
            register.revoke(&mia(), &Seeded::new(9).public(), 4),
            Err(RegisterError::UnknownKey)
        );
        assert_eq!(register, before);
    }

    #[test]
    fn revoking_the_key_of_an_agent_leaves_the_sponsor() {
        let mut register = with_bot();
        let revoked = register
            .revoke(&bot(), &Seeded::new(3).public(), 8)
            .unwrap();
        assert!(revoked.is_empty());
        assert_eq!(
            register.entry(&mia()).unwrap().keys()[0].state,
            KeyState::Active
        );
    }

    #[test]
    fn the_sponsor_of_the_actor_line_is_the_one_of_the_register() {
        let register = with_bot();
        let signer = Seeded::new(3);
        let proof = signature::sign(&signer, PayloadKind::LogEntry, b"x").unwrap();
        let right = Actor::agent("bot", "any-model", "any-session", "mia").unwrap();
        assert!(register.verify_log_entry(&right, 1, b"x", &proof).is_ok());
        let wrong = Actor::agent("bot", "m", "s", "daniel").unwrap();
        assert_eq!(
            register.verify_log_entry(&wrong, 1, b"x", &proof),
            Err(ProofError::SponsorMismatch {
                claimed: "daniel".to_owned(),
                registered: "mia".to_owned()
            })
        );
    }

    #[test]
    fn a_retired_or_revoked_sponsor_key_adds_no_new_agent() {
        let cosign = cosign_of(&Seeded::new(2), &bot(), Role::Member, &mia());
        for revoke in [false, true] {
            let mut register = register();
            register.add_key(&mia(), Seeded::new(5).public()).unwrap();
            if revoke {
                register
                    .revoke(&mia(), &Seeded::new(2).public(), 3)
                    .unwrap();
            } else {
                register
                    .retire(&mia(), &Seeded::new(2).public(), 3)
                    .unwrap();
            }
            assert_eq!(
                register.add_agent(
                    bot(),
                    Role::Member,
                    mia(),
                    Seeded::new(3).public(),
                    cosign.clone()
                ),
                Err(RegisterError::BadCosign)
            );
        }
    }

    #[test]
    fn an_agent_cannot_take_a_key_that_the_register_holds() {
        let cosign = cosign_of(&Seeded::new(2), &bot(), Role::Member, &mia());
        // The key of its own sponsor.
        assert_eq!(
            register().add_agent(bot(), Role::Member, mia(), Seeded::new(2).public(), cosign),
            Err(RegisterError::KeyKnown)
        );
    }

    #[test]
    fn the_cascade_takes_the_earlier_bound_skips_revoked_keys_and_spares_other_agents() {
        let mut register = with_bot();
        // A second agent of Mia, and an agent of Daniel.
        let second = Identity::of(&Actor::agent("second", "m", "s", "mia").unwrap());
        let cosign = cosign_of(&Seeded::new(2), &second, Role::Member, &mia());
        register
            .add_agent(
                second.clone(),
                Role::Member,
                mia(),
                Seeded::new(4).public(),
                cosign,
            )
            .unwrap();
        let other = Identity::of(&Actor::agent("other", "m", "s", "daniel").unwrap());
        let cosign = cosign_of(
            &Seeded::new(1),
            &other,
            Role::Member,
            &Identity::of(&daniel()),
        );
        register
            .add_agent(
                other.clone(),
                Role::Member,
                Identity::of(&daniel()),
                Seeded::new(6).public(),
                cosign,
            )
            .unwrap();
        // `bot` retired later than the revocation, `second` revoked already.
        register.add_key(&bot(), Seeded::new(7).public()).unwrap();
        register
            .retire(&bot(), &Seeded::new(3).public(), 30)
            .unwrap();
        register
            .revoke(&second, &Seeded::new(4).public(), 9)
            .unwrap();
        let revoked = register
            .revoke(&mia(), &Seeded::new(2).public(), 20)
            .unwrap();
        assert_eq!(revoked, vec![bot()]);
        let bot_keys = register.entry(&bot()).unwrap().keys();
        assert!(bot_keys
            .iter()
            .all(|held| held.state == KeyState::Revoked { from: 20 }));
        assert_eq!(
            register.entry(&second).unwrap().keys()[0].state,
            KeyState::Revoked { from: 9 }
        );
        assert_eq!(
            register.entry(&other).unwrap().keys()[0].state,
            KeyState::Active
        );
    }

    #[test]
    fn the_reader_refuses_an_agent_as_sponsor_and_a_cosign_after_the_keys() {
        let good = with_bot().encode();
        let key3 = Seeded::new(3).public();
        assert_eq!(
            problem(&good.replace("sponsor mia", "sponsor bot")),
            RegisterProblem::UnknownSponsor
        );
        let cosign_line = good
            .lines()
            .find(|l| l.starts_with("cosign "))
            .unwrap()
            .to_owned();
        let moved = good.replace(&format!("{cosign_line}\n"), "").replace(
            &format!("key {key3} active\n"),
            &format!("key {key3} active\n{cosign_line}\n"),
        );
        assert_eq!(problem(&moved), RegisterProblem::CosignOutsideAgent);
        // The same bytes with a changed last character of the base64 are no other encoding.
        let mut tampered = cosign_line.clone();
        tampered.pop();
        tampered.push(if cosign_line.ends_with('A') { 'B' } else { 'A' });
        assert_eq!(
            problem(&good.replace(&cosign_line, &tampered)),
            RegisterProblem::BadCosign
        );
    }

    #[test]
    fn a_revoked_sponsorship_cannot_be_undone_by_a_new_agent_key() {
        // The only key of Mia is revoked: the agent has no active key and gets none.
        let mut register = with_bot();
        register.add_key(&mia(), Seeded::new(5).public()).unwrap();
        register
            .revoke(&mia(), &Seeded::new(2).public(), 9)
            .unwrap();
        // The co-signing key is revoked, so the new key of Mia does not help.
        assert_eq!(
            register.add_key(&bot(), Seeded::new(8).public()),
            Err(RegisterError::SponsorRevoked)
        );
        // A hand-written file with an active key under the revoked sponsorship is refused.
        let forged = register.encode().replace(
            &format!("key {} revoked 9", Seeded::new(3).public()),
            &format!("key {} active", Seeded::new(3).public()),
        );
        assert_eq!(problem(&forged), RegisterProblem::SponsorRevoked);
    }

    #[test]
    fn a_sponsor_without_an_active_key_leaves_the_agent_without_one() {
        let mut register = with_bot();
        // The only key of Mia cannot go while the agent has an active key: the register could not
        // be read again.
        assert_eq!(
            register.retire(&mia(), &Seeded::new(2).public(), 9),
            Err(RegisterError::SponsorRevoked)
        );
        assert!(Register::parse(register.encode().as_bytes()).is_ok());
        // After a revocation the agent has no active key, and the register reads back.
        register
            .revoke(&mia(), &Seeded::new(2).public(), 9)
            .unwrap();
        assert!(Register::parse(register.encode().as_bytes()).is_ok());
        assert_eq!(
            register.add_key(&bot(), Seeded::new(8).public()),
            Err(RegisterError::SponsorRevoked)
        );
    }

    #[test]
    fn a_key_of_a_shut_down_agent_cannot_outlast_the_revocation() {
        let mut register = with_bot();
        register.add_key(&mia(), Seeded::new(5).public()).unwrap();
        register
            .revoke(&mia(), &Seeded::new(2).public(), 9)
            .unwrap();
        assert!(Register::parse(register.encode().as_bytes()).is_ok());
        let key3 = Seeded::new(3).public();
        for later in ["revoked 50", "retired 50"] {
            let forged = register.encode().replace(
                &format!("key {key3} revoked 9"),
                &format!("key {key3} {later}"),
            );
            assert_eq!(problem(&forged), RegisterProblem::SponsorRevoked, "{later}");
        }
    }

    #[test]
    fn revoking_a_retired_cosigning_key_keeps_the_register_readable() {
        let mut register = with_bot();
        register.add_key(&mia(), Seeded::new(5).public()).unwrap();
        register
            .retire(&mia(), &Seeded::new(2).public(), 5)
            .unwrap();
        register
            .revoke(&mia(), &Seeded::new(2).public(), 10)
            .unwrap();
        assert_eq!(
            Register::parse(register.encode().as_bytes()).unwrap(),
            register
        );
        let agent = register.entry(&bot()).unwrap().keys()[0].state;
        assert_eq!(agent, KeyState::Revoked { from: 5 });
    }

    #[test]
    fn a_later_revocation_of_the_cosigning_key_lowers_the_bound_of_the_agent() {
        let mut register = with_bot();
        register.add_key(&mia(), Seeded::new(5).public()).unwrap();
        // First a key that did not co-sign, then the co-signing key at an earlier entry.
        register
            .revoke(&mia(), &Seeded::new(5).public(), 30)
            .unwrap();
        assert!(Register::parse(register.encode().as_bytes()).is_ok());
        register
            .revoke(&mia(), &Seeded::new(2).public(), 10)
            .unwrap();
        assert_eq!(
            Register::parse(register.encode().as_bytes()).unwrap(),
            register
        );
        let agent = register.entry(&bot()).unwrap().keys()[0].state;
        assert_eq!(agent, KeyState::Revoked { from: 10 });
    }
}
