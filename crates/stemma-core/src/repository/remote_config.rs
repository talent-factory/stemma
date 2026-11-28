//! Adding and removing a remote (ADR-009, ADR-011, ADR-026, STEMMA-305).
//!
//! `remote add` is the first contact with a hub: it reads the list of references, pins the key of
//! the hub that the list names, against a fingerprint from a second channel if one is given, and
//! writes the configuration of the remote with the repository ID of the list. `remote remove`
//! deletes the configuration and every remote reference of the remote. Each writes one entry of
//! its own under the lock of the references, before the files change (ADR-011): `remote-add`
//! with the configuration, and `remote-remove` with the configuration and the remote references
//! that it deletes, so the entry holds what was there. `op restore` does not undo either; the two
//! commands undo each other, and a fetch after `remote add` brings the remote references back.
//!
//! The level of this code is 2 by ADR-004 (network, references, and the operation log); the
//! authorizer is asked for `remote-add` and `remote-remove` with the name of the remote.

use std::error::Error;
use std::fmt;
use std::io;

use super::{now, KeepNamespaceError, Repository, UpdateError};
use crate::oplog::{Entry, OpLogError, OperationName, RemoteConfigEntry, RemoteConfigKind};
use crate::protocol::RefList;
use crate::refs::{RefError, RefStore};
use crate::remote::{
    self, CaFile, HubKeyError, RemoteAddress, RemoteConfig, RemoteError, RemoteName, RemoteRef,
    RemoteRefName,
};
use crate::signature::KeyFingerprint;
use crate::transport::Transport;
use crate::{Actor, Authorizer};

use super::fetch::{read_ref_list, FetchError};

/// What the first contact with a hub needs besides the list of references: the address, the CA
/// file and the sign-in that the configuration keeps, and the fingerprint of the key of the hub
/// that a human got on a second channel, if there is one (ADR-026). `remote add` and `clone`
/// take it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSetup {
    /// The address of the repository on the hub.
    pub address: RemoteAddress,
    /// The CA file for the connection, if the hub has a certificate of its own CA.
    pub ca_file: Option<CaFile>,
    /// Whether the hub asks for a sign-in.
    pub sign_in: bool,
    /// The fingerprint that the key of the hub must have; `None` takes the first contact on
    /// trust and shows the fingerprint.
    pub expected: Option<KeyFingerprint>,
}

impl RemoteSetup {
    /// A setup with the address alone: no CA file, no sign-in, and no expected fingerprint.
    pub fn new(address: RemoteAddress) -> Self {
        Self {
            address,
            ca_file: None,
            sign_in: false,
            expected: None,
        }
    }

    /// The configuration that the setup gives for a remote whose hub just sent `list`: its
    /// address, CA file and sign-in, the repository ID of the list, and the key of the hub that
    /// the list names, pinned against the expected fingerprint.
    ///
    /// # Errors
    ///
    /// [`HubKeyError::Unexpected`] if the key of the list does not have the expected fingerprint.
    pub(crate) fn first_contact(&self, list: &RefList) -> Result<RemoteConfig, HubKeyError> {
        let mut config = RemoteConfig::new(self.address.clone(), list.repository());
        config.ca_file = self.ca_file.clone();
        config.sign_in = self.sign_in;
        config.pin_first_contact(list, self.expected.as_ref())?;
        Ok(config)
    }

    /// The configuration that a transport is made from before the first contact, when the
    /// repository ID is not known yet: the address, the CA file and the sign-in of the setup,
    /// with an ID of zeros that no request carries.
    pub fn provisional(&self) -> RemoteConfig {
        let id = "0"
            .repeat(32)
            .parse()
            .expect("32 hexadecimal digits are an ID");
        let mut config = RemoteConfig::new(self.address.clone(), id);
        config.ca_file = self.ca_file.clone();
        config.sign_in = self.sign_in;
        config
    }
}

/// What [`Repository::add_remote`] did.
#[derive(Debug)]
#[non_exhaustive]
pub struct AddedRemote {
    /// The entry `remote-add`.
    pub entry: Entry,
    /// The configuration that was written, with the pinned key.
    pub config: RemoteConfig,
    /// The fingerprint of the pinned key of the hub, which the command shows (ADR-026).
    pub fingerprint: KeyFingerprint,
}

/// What [`Repository::remove_remote`] did.
#[derive(Debug)]
#[non_exhaustive]
pub struct RemovedRemote {
    /// The entry `remote-remove`.
    pub entry: Entry,
    /// The configuration that was deleted.
    pub config: RemoteConfig,
    /// The remote references that were deleted, sorted by name.
    pub refs: Vec<(RemoteRefName, RemoteRef)>,
}

impl Repository {
    /// Adds the remote `remote` with the setup `setup`, through `transport` to its hub: the
    /// first contact of ADR-026. The authorizer is asked for `remote-add` on the name of the
    /// remote first; a remote of that name is refused before the hub is asked. Then the list of
    /// references is read, the key of the hub is pinned against the expected fingerprint, and
    /// under the lock of the references the namespace policy that the list names is kept (or a
    /// kept one of an earlier remote of the name removed), then the entry `remote-add` is written
    /// and the configuration after it. No remote reference moves: a fetch brings them. A remote whose hub asks for a
    /// sign-in is signed in on `transport` by the caller first.
    ///
    /// # Errors
    ///
    /// Nothing is written for [`RemoteConfigError::Refs`] (a bare repository, the lock),
    /// [`RemoteConfigError::Denied`], [`RemoteConfigError::Remote`] (a configuration that cannot
    /// be read), [`RemoteConfigError::Exists`], [`RemoteConfigError::List`] (the list of
    /// references, as for a fetch), [`RemoteConfigError::HubKey`] (another key than the
    /// expected one), [`RemoteConfigError::Namespace`] (the kept namespace policy), and
    /// [`RemoteConfigError::Log`] unless its error is [`OpLogError::NotFlushed`].
    /// [`RemoteConfigError::Written`] means that the entry is in the
    /// log and the configuration could not be written: `remote add` again writes a second entry.
    pub fn add_remote(
        &self,
        remote: &RemoteName,
        setup: &RemoteSetup,
        transport: &mut dyn Transport,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<AddedRemote, RemoteConfigError> {
        self.require_oplog("add a remote")
            .map_err(RemoteConfigError::Refs)?;
        let operation = operation(RemoteConfigKind::Add);
        let allowed = self
            .authorize(actor, authorizer, &operation, remote.as_str())
            .map_err(|_| RemoteConfigError::Denied {
                operation: operation.to_string(),
                remote: remote.clone(),
            })?;
        // A remote of that name is refused before the hub is asked, and once more under the lock.
        if self
            .remote(remote)
            .map_err(RemoteConfigError::Remote)?
            .is_some()
        {
            return Err(RemoteConfigError::Exists(remote.clone()));
        }
        let list =
            read_ref_list(transport).map_err(|error| RemoteConfigError::List(Box::new(error)))?;
        let config = setup
            .first_contact(&list)
            .map_err(RemoteConfigError::HubKey)?;
        let fingerprint = list.key().fingerprint();
        let lock = self
            .refs_store()
            .lock_writer(&allowed)
            .map_err(RemoteConfigError::Refs)?;
        if self
            .remote(remote)
            .map_err(RemoteConfigError::Remote)?
            .is_some()
        {
            return Err(RemoteConfigError::Exists(remote.clone()));
        }
        // The local gate reads the kept policy of every configured remote, so it is in place
        // before the configuration; without it the gate would judge without the namespace until
        // the first fetch.
        self.keep_namespace(remote, list.namespace())
            .map_err(RemoteConfigError::Namespace)?;
        let record = RemoteConfigEntry::try_new(
            now(),
            actor.clone(),
            RemoteConfigKind::Add,
            remote.clone(),
            config.clone(),
            Vec::new(),
        )
        .expect("an addition names no remote reference");
        let entry = self
            .append_remote_config(&lock, &record)
            .map_err(RemoteConfigError::Log)?;
        let metadata = self.metadata_dir();
        if let Err(source) = remote::write_config(&metadata, &metadata.join("tmp"), remote, &config)
        {
            return Err(RemoteConfigError::Written {
                entry: Box::new(entry),
                source,
            });
        }
        drop(lock);
        Ok(AddedRemote {
            entry,
            config,
            fingerprint,
        })
    }

    /// Removes the remote `remote`: its configuration and every remote reference of it. The
    /// authorizer is asked for `remote-remove` on the name of the remote. Under the lock of the
    /// references the entry `remote-remove` is written with the configuration and the remote
    /// references, then the remote references are deleted, and then the configuration. The hub
    /// is not asked.
    ///
    /// # Errors
    ///
    /// Nothing is written for [`RemoteConfigError::Refs`], [`RemoteConfigError::Denied`],
    /// [`RemoteConfigError::Remote`], [`RemoteConfigError::NoSuchRemote`], and
    /// [`RemoteConfigError::Log`] unless its error is [`OpLogError::NotFlushed`].
    /// [`RemoteConfigError::Update`] with an entry means that the entry is written and the
    /// remote references were not all deleted, and [`RemoteConfigError::Written`] that they were
    /// and the configuration stayed: `remote remove` again finishes it with a second entry.
    pub fn remove_remote(
        &self,
        remote: &RemoteName,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<RemovedRemote, RemoteConfigError> {
        self.require_oplog("remove a remote")
            .map_err(RemoteConfigError::Refs)?;
        let operation = operation(RemoteConfigKind::Remove);
        let allowed = self
            .authorize(actor, authorizer, &operation, remote.as_str())
            .map_err(|_| RemoteConfigError::Denied {
                operation: operation.to_string(),
                remote: remote.clone(),
            })?;
        let lock = self
            .refs_store()
            .lock_writer(&allowed)
            .map_err(RemoteConfigError::Refs)?;
        let config = self
            .remote(remote)
            .map_err(RemoteConfigError::Remote)?
            .ok_or_else(|| RemoteConfigError::NoSuchRemote(remote.clone()))?;
        let targets: Vec<(RemoteRefName, Option<RemoteRef>)> = self
            .refs_store()
            .remote_refs()
            .map_err(RemoteConfigError::Refs)?
            .into_iter()
            .filter(|(name, _)| name.remote == *remote)
            .map(|(name, _)| (name, None))
            .collect();
        let entry_of = |refs: Vec<(RemoteRefName, RemoteRef)>| {
            let record = RemoteConfigEntry::try_new(
                now(),
                actor.clone(),
                RemoteConfigKind::Remove,
                remote.clone(),
                config.clone(),
                refs,
            )
            .expect("the remote references of one remote, each once");
            self.append_remote_config(&lock, &record)
        };
        let (entry, refs) = if targets.is_empty() {
            (
                entry_of(Vec::new()).map_err(RemoteConfigError::Log)?,
                Vec::new(),
            )
        } else {
            let mut deleted = Vec::new();
            let logged = self
                .refs_store()
                .move_remotes_after(&lock, &targets, self.objects(), |moves| {
                    deleted = moves
                        .iter()
                        .filter_map(|(name, old, _)| old.map(|old| (name.clone(), old)))
                        .collect();
                    entry_of(deleted.clone())
                })
                .map_err(RemoteConfigError::Refs)?;
            match logged.into_result() {
                Ok(Some(entry)) => (entry, deleted),
                // Every listed reference is deleted under the lock, so a plan without a move is
                // not expected; the entry is written all the same, without references.
                Ok(None) => (
                    entry_of(Vec::new()).map_err(RemoteConfigError::Log)?,
                    Vec::new(),
                ),
                Err(error) => return Err(RemoteConfigError::Update(error)),
            }
        };
        if let Err(source) = remote::delete_config(&self.metadata_dir(), remote) {
            return Err(RemoteConfigError::Written {
                entry: Box::new(entry),
                source,
            });
        }
        drop(lock);
        Ok(RemovedRemote {
            entry,
            config,
            refs,
        })
    }
}

/// The operation of an entry of a remote.
fn operation(kind: RemoteConfigKind) -> OperationName {
    OperationName::try_new(kind.operation()).expect("a valid operation name")
}

/// Why a remote could not be added or removed.
#[derive(Debug)]
#[non_exhaustive]
pub enum RemoteConfigError {
    /// The references could not be read or locked, or the repository is bare.
    Refs(RefError),
    /// The authorizer refused the operation on the remote.
    Denied {
        /// The operation, `remote-add` or `remote-remove`.
        operation: String,
        /// The remote.
        remote: RemoteName,
    },
    /// The configuration of a remote could not be read.
    Remote(RemoteError),
    /// A remote of this name exists already.
    Exists(RemoteName),
    /// The repository has no remote of this name.
    NoSuchRemote(RemoteName),
    /// The list of references could not be read from the hub. Boxed, as the error is large.
    List(Box<FetchError>),
    /// The key of the hub does not have the expected fingerprint. Nothing was written.
    HubKey(HubKeyError),
    /// The namespace policy that the list names could not be kept (ADR-027). No entry and no
    /// configuration were written.
    Namespace(KeepNamespaceError),
    /// The entry could not be written. With [`OpLogError::NotFlushed`] it is in the log.
    Log(OpLogError),
    /// The entry is written, and the remote references were not all deleted (see
    /// [`UpdateError`]).
    Update(UpdateError),
    /// The entry is written (and a removal deleted the remote references), but the configuration
    /// could not be written or deleted.
    Written {
        /// The entry.
        entry: Box<Entry>,
        /// Why the configuration did not change.
        source: RemoteError,
    },
}

impl RemoteConfigError {
    /// The entry of the operation, if it was written.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Written { entry, .. } => Some(entry),
            Self::Update(error) => error.entry(),
            _ => None,
        }
    }

    /// The error of the file system, if that is what failed (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Refs(error) => error.io_error(),
            Self::Remote(error) | Self::Written { source: error, .. } => error.io_error(),
            Self::List(error) => error.io_error(),
            Self::Log(error) => error.io_error(),
            Self::Update(error) => error.io_error(),
            Self::Namespace(KeepNamespaceError::Io { source, .. }) => Some(source),
            Self::Namespace(KeepNamespaceError::Format(error)) => error.io_error(),
            Self::Denied { .. } | Self::Exists(_) | Self::NoSuchRemote(_) | Self::HubKey(_) => None,
        }
    }
}

impl fmt::Display for RemoteConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refs(error) => error.fmt(f),
            Self::Denied { operation, remote } => {
                write!(
                    f,
                    "the operation `{operation}` on the remote `{remote}` is not allowed"
                )
            }
            Self::Remote(error) => error.fmt(f),
            Self::Exists(name) => write!(f, "the remote `{name}` exists already"),
            Self::NoSuchRemote(name) => write!(f, "there is no remote `{name}`"),
            Self::List(error) => error.fmt(f),
            Self::HubKey(error) => write!(f, "{error}; nothing was written"),
            Self::Namespace(error) => write!(
                f,
                "the namespace policy of the hub could not be kept: {error}; nothing was written"
            ),
            Self::Log(error) => error.fmt(f),
            Self::Update(error) => error.fmt(f),
            Self::Written { entry, source } => write!(
                f,
                "entry {} is written, but the configuration of the remote did not change: \
                 {source}",
                entry.number()
            ),
        }
    }
}

impl Error for RemoteConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message includes the message of the cause, so the source is the cause of the cause
        // (see the crate documentation).
        match self {
            Self::Refs(error) => error.source(),
            Self::Remote(error) | Self::Written { source: error, .. } => error.source(),
            Self::List(error) => error.source(),
            Self::Log(error) => error.source(),
            Self::Update(error) => error.source(),
            Self::Namespace(error) => error.source(),
            Self::Denied { .. } | Self::Exists(_) | Self::NoSuchRemote(_) | Self::HubKey(_) => None,
        }
    }
}

#[cfg(test)]
mod tests;
