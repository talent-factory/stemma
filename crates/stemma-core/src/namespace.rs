//! The namespace policy and the owners that it demands (ADR-027, STEMMA-311).
//!
//! Every namespace on a hub has one namespace repository. The file
//! [`NAMESPACE_FILE_NAME`](crate::policy::NAMESPACE_FILE_NAME) in the root of the tip of its
//! `main` is the namespace policy, in the format of the policy file, and the human owners of its
//! register are the owners of the namespace. [`Namespace`] is what a hub reads there
//! ([`Repository::namespace`](crate::repository::Repository::namespace)) and passes to the
//! accepting
//! operation of every repository of the namespace
//! ([`AcceptContext::with_namespace`](crate::repository::AcceptContext::with_namespace)):
//!
//! - The verdict about `main` combines the policy of `T` and the namespace policy by the
//!   maximum ([`Combined`](crate::policy::Combined)), so the namespace policy can only raise.
//! - Where the namespace policy has the line `demand-namespace-owners`, the register of the new
//!   commit of `main` (of a first push and of every later update) must hold every demanded owner
//!   ([`Namespace::missing_owner`]): the same identity, with the role owner, and every key that is
//!   active for it in the register of the namespace repository as an active key.
//! - The record entry names the commit of the namespace policy among the commits of its
//!   policies.
//!
//! The list of references names the namespace policy ([`RefList`](crate::protocol::RefList)),
//! and a fetch keeps it in the clone ([`KeptNamespace`]), so the local gate evaluates the
//! namespace policy that it last fetched. The demanded owners are enforced by the hub alone; a
//! clone sees them in its register, where `op verify` finds them without the hub.

use std::error::Error;
use std::fmt;

use crate::object::CommitId;
use crate::policy::{Policy, PolicyParseError};
use crate::register::{Identity, IdentityKind, KeyState, Register, Role};
use crate::signature::PublicKey;

/// An owner of the namespace that a namespace policy demands in the register of every repository
/// of the namespace: a human with the role owner in the register of the namespace repository,
/// with its active keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemandedOwner {
    identity: Identity,
    keys: Vec<PublicKey>,
}

impl DemandedOwner {
    /// The identity.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The keys that are active for it in the register of the namespace repository.
    pub fn keys(&self) -> &[PublicKey] {
        &self.keys
    }

    /// Whether `register` holds the owner: the same identity with the role owner, and each of
    /// its keys as an active key of that identity.
    pub fn is_held_by(&self, register: &Register) -> bool {
        register.entry(&self.identity).is_some_and(|entry| {
            entry.role() == Role::Owner
                && self
                    .keys
                    .iter()
                    .all(|key| register.is_active_key(&self.identity, key))
        })
    }
}

/// The namespace policy at the tip of `main` of a namespace repository, its commit, and the
/// owners of the namespace that it demands (see the module).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Namespace {
    commit: CommitId,
    policy: Policy,
    owners: Vec<DemandedOwner>,
}

impl Namespace {
    /// The namespace of a policy at `commit` and of the register of that commit. The demanded
    /// owners are the humans with the role owner and at least one active key, with those keys,
    /// if the policy has the line `demand-namespace-owners`, and none otherwise.
    ///
    /// # Errors
    ///
    /// [`NoDemandedOwner`] if the policy has the line and the register holds no such human: the
    /// demand would ask for nothing, and a rule that silently does not apply is refused. The
    /// reader of a register refuses one without an owner with an active key, so a valid
    /// register never gets here; the check keeps the rule closed if that reader changes.
    pub fn new(
        commit: CommitId,
        policy: Policy,
        register: &Register,
    ) -> Result<Self, NoDemandedOwner> {
        let owners = if policy.demands_namespace_owners() {
            register
                .entries()
                .iter()
                .filter(|entry| {
                    entry.identity().kind() == IdentityKind::Human && entry.role() == Role::Owner
                })
                .map(|entry| DemandedOwner {
                    identity: entry.identity().clone(),
                    keys: entry
                        .keys()
                        .iter()
                        .filter(|held| held.state == KeyState::Active)
                        .map(|held| held.key.clone())
                        .collect(),
                })
                .filter(|owner| !owner.keys.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        if policy.demands_namespace_owners() && owners.is_empty() {
            return Err(NoDemandedOwner);
        }
        Ok(Self {
            commit,
            policy,
            owners,
        })
    }

    /// The commit of the namespace repository whose namespace policy this is: the tip of its
    /// `main` when the hub read it.
    pub fn commit(&self) -> CommitId {
        self.commit
    }

    /// The namespace policy.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The owners that the namespace policy demands, empty if it has no line
    /// `demand-namespace-owners`.
    pub fn owners(&self) -> &[DemandedOwner] {
        &self.owners
    }

    /// The first demanded owner that `register` does not hold, or `None`.
    pub fn missing_owner(&self, register: &Register) -> Option<&DemandedOwner> {
        self.owners.iter().find(|owner| !owner.is_held_by(register))
    }
}

/// The namespace policy demands the owners of the namespace, and the register of the namespace
/// repository holds no human owner with an active key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoDemandedOwner;

impl fmt::Display for NoDemandedOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "the namespace policy demands the owners of the namespace, and the register of the \
             namespace repository holds no human owner with an active key",
        )
    }
}

impl Error for NoDemandedOwner {}

/// The namespace policy that a clone fetched last from a remote: its commit and the policy.
/// The list of references carries it, and a fetch keeps it in `namespaces/<remote>` (ADR-009).
///
/// The file is strict text, in this form:
///
/// ```text
/// stemma-namespace 1
/// commit <commit>
/// <the policy in its canonical form>
/// ```
///
/// The policy follows in the form of [`Policy::encode`], from its header on, so the reader takes
/// only what the writer writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptNamespace {
    commit: CommitId,
    policy: Policy,
}

/// The first line of the file of a kept namespace policy.
const KEPT_HEADER: &str = "stemma-namespace 1";

/// The longest file of a kept namespace policy that is read: the two lines before the policy and
/// a policy file of at most 64 KiB fit in it with room to spare. A longer file is read up to one
/// byte past this bound and refused by the reader.
pub(crate) const MAX_KEPT_LEN: u64 = 8 << 20;

impl KeptNamespace {
    /// A kept namespace policy.
    pub fn new(commit: CommitId, policy: Policy) -> Self {
        Self { commit, policy }
    }

    /// The commit of the namespace repository that the hub named.
    pub fn commit(&self) -> CommitId {
        self.commit
    }

    /// The namespace policy.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The file in its one form.
    pub fn encode(&self) -> String {
        format!(
            "{KEPT_HEADER}\ncommit {}\n{}",
            self.commit,
            self.policy.encode()
        )
    }

    /// Reads the file.
    ///
    /// # Errors
    ///
    /// A [`KeptNamespaceError`] for every form that [`encode`](Self::encode) does not write.
    pub fn parse(bytes: &[u8]) -> Result<Self, KeptNamespaceError> {
        let text = std::str::from_utf8(bytes).map_err(|_| KeptNamespaceError::Form)?;
        let rest = text
            .strip_prefix(KEPT_HEADER)
            .and_then(|rest| rest.strip_prefix("\ncommit "))
            .ok_or(KeptNamespaceError::Form)?;
        let (commit, policy) = rest.split_once('\n').ok_or(KeptNamespaceError::Form)?;
        let commit = commit
            .parse()
            .map(CommitId::new)
            .map_err(|_| KeptNamespaceError::Form)?;
        let parsed = Policy::parse(policy.as_bytes()).map_err(KeptNamespaceError::Policy)?;
        if parsed.encode() != policy {
            return Err(KeptNamespaceError::Form);
        }
        Ok(Self {
            commit,
            policy: parsed,
        })
    }
}

/// Why the file of a kept namespace policy is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeptNamespaceError {
    /// The file is not in the form that a fetch writes.
    Form,
    /// The policy in it is not valid.
    Policy(PolicyParseError),
}

impl fmt::Display for KeptNamespaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Form => f.write_str("the kept namespace policy is not in the form of a fetch"),
            Self::Policy(error) => write!(f, "the kept namespace policy is not valid: {error}"),
        }
    }
}

impl Error for KeptNamespaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Policy(error) => Some(error),
            Self::Form => None,
        }
    }
}

#[cfg(test)]
mod tests;
