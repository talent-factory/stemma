//! Pinning the key of a hub again (ADR-026, STEMMA-297).
//!
//! A clone pins the key of the hub in the configuration of the remote on the first contact
//! ([`RemoteConfig::pin_first_contact`]) and refuses another key from then on
//! ([`RemoteConfig::hub_keys`]). When the hub rotates its key, a human compares the fingerprint of
//! the new key on a second channel and pins it again. That is an operation of its own: it writes
//! the entry `remote-pin` with the old and the new key, and then replaces the configuration, both
//! under the lock of the references.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use super::{now, Repository};
use crate::oplog::{Entry, OpLogError, OperationName, RemotePinEntry, REMOTE_PIN};
use crate::protocol::RefList;
use crate::refs::{RefError, WriteLock};
use crate::register::{Identity, IdentityKind};
use crate::remote::{self, RemoteConfig, RemoteError, RemoteName};
use crate::signature::KeyFingerprint;
use crate::{Actor, Authorizer};

impl Repository {
    /// Pins the key that `list` names as the key of the hub of `remote`, in place of the key that
    /// the configuration pinned (ADR-026). A human does it, after comparing the fingerprint of
    /// the new key on a second channel; `expected` is that fingerprint. The list is the one that
    /// the hub just sent, so its former keys verified for the new key when it was read.
    ///
    /// The authorizer is asked for `remote-pin` on the name of the remote. Under the lock of the
    /// references the entry `remote-pin` is written first, then the configuration is replaced
    /// (ADR-011). The same key as the pinned one changes nothing and writes no entry: the result
    /// is `None`.
    ///
    /// # Errors
    ///
    /// [`PinError::NotHuman`] for an agent, [`PinError::Unexpected`] if the key of the list does
    /// not have the fingerprint `expected`, [`PinError::Denied`] if the authorizer refuses,
    /// [`PinError::Refs`] for the lock, [`PinError::Remote`] if the configuration cannot be
    /// read, [`PinError::NoRemote`] if there is no remote of that name,
    /// [`PinError::OtherRepository`] if the list is that of another repository, and
    /// [`PinError::Log`] if the entry cannot be written; nothing is pinned then. The repository
    /// may have moved to the current format before the log refused the entry, as every entry
    /// moves it first (ADR-009). After [`PinError::Log`] with [`OpLogError::NotFlushed`] and
    /// after [`PinError::Io`] the entry is in the log and the configuration may still hold the
    /// old key: pinning again writes a second entry.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn pin_hub_key_again(
        &self,
        remote: &RemoteName,
        list: &RefList,
        expected: &KeyFingerprint,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Option<Entry>, PinError> {
        self.require_oplog("pin the key of a hub again")?;
        if Identity::of(actor).kind() != IdentityKind::Human {
            return Err(PinError::NotHuman {
                actor: Box::new(actor.clone()),
            });
        }
        let offered = list.key().fingerprint();
        if offered != *expected {
            return Err(PinError::Unexpected {
                expected: *expected,
                offered,
            });
        }
        let operation = OperationName::try_new(REMOTE_PIN).expect("a valid operation name");
        let allowed = self
            .authorize(actor, authorizer, &operation, remote.as_str())
            .map_err(|_| PinError::Denied {
                remote: remote.clone(),
            })?;
        let lock = self.refs_store().lock_writer(&allowed)?;
        let mut config = self
            .remote(remote)?
            .ok_or_else(|| PinError::NoRemote(remote.clone()))?;
        if list.repository() != config.repository {
            return Err(PinError::OtherRepository(remote.clone()));
        }
        if config.hub_key.as_ref() == Some(list.key()) {
            return Ok(None);
        }
        let pin = RemotePinEntry::try_new(
            now(),
            actor.clone(),
            remote.clone(),
            config.hub_key.take(),
            list.key().clone(),
        )
        .expect("a human pins, and the old key differs from the new one");
        let entry = self.append_remote_pin(&lock, &pin)?;
        config.hub_key = Some(list.key().clone());
        self.write_remote_config(&lock, remote, &config)?;
        Ok(Some(entry))
    }

    /// Replaces the configuration file of a remote as a whole, with the writer that a clone
    /// uses ([`remote::write_config`]). The caller holds the lock of the references.
    fn write_remote_config(
        &self,
        _lock: &WriteLock,
        remote: &RemoteName,
        config: &RemoteConfig,
    ) -> Result<(), PinError> {
        let metadata = self.metadata_dir();
        remote::write_config(&metadata, &metadata.join("tmp"), remote, config).map_err(|error| {
            match error {
                RemoteError::Io {
                    operation,
                    path,
                    source,
                } => PinError::Io {
                    operation,
                    path,
                    source,
                },
                other => PinError::Remote(other),
            }
        })
    }
}

/// Why the key of a hub could not be pinned again. Each variant before [`Log`](Self::Log) changed
/// nothing; after [`Log`](Self::Log) with [`OpLogError::NotFlushed`] and after
/// [`Io`](Self::Io) the entry is in the log and the configuration holds the old key.
#[derive(Debug)]
#[non_exhaustive]
pub enum PinError {
    /// The actor is an agent. A human pins the key of a hub (ADR-026).
    NotHuman {
        /// The actor.
        actor: Box<Actor>,
    },
    /// The key that the hub names does not have the fingerprint from the second channel.
    Unexpected {
        /// The fingerprint that the caller passed.
        expected: KeyFingerprint,
        /// The fingerprint of the key that the hub names.
        offered: KeyFingerprint,
    },
    /// The authorizer refused `remote-pin` on the remote.
    Denied {
        /// The remote.
        remote: RemoteName,
    },
    /// The repository has no remote of this name.
    NoRemote(RemoteName),
    /// The list of references is that of another repository than the one of the remote.
    OtherRepository(RemoteName),
    /// The lock of the references could not be taken, or the repository is bare.
    Refs(RefError),
    /// The configuration of the remote could not be read.
    Remote(RemoteError),
    /// The entry could not be written. With [`OpLogError::NotFlushed`] it is in the log.
    Log(OpLogError),
    /// The configuration could not be replaced after the entry was written.
    Io {
        /// What was being done.
        operation: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error of the file system. Its message ends the message of this error.
        source: io::Error,
    },
}

impl PinError {
    /// Returns the error of the file system, if that is what failed (see the crate
    /// documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Refs(error) => error.io_error(),
            Self::Remote(error) => error.io_error(),
            Self::Log(error) => error.io_error(),
            Self::NotHuman { .. }
            | Self::Unexpected { .. }
            | Self::Denied { .. }
            | Self::NoRemote(_)
            | Self::OtherRepository(_) => None,
        }
    }
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotHuman { actor } => {
                write!(f, "{actor} is no human; only a human pins the key of a hub")
            }
            Self::Unexpected { expected, offered } => write!(
                f,
                "the hub names the key {offered}, but the key {expected} was expected; nothing \
                 was pinned"
            ),
            Self::Denied { remote } => {
                write!(
                    f,
                    "not allowed to pin the key of the hub of the remote `{remote}`"
                )
            }
            Self::NoRemote(remote) => write!(f, "there is no remote `{remote}`"),
            Self::OtherRepository(remote) => write!(
                f,
                "the list of references is that of another repository than the one of the \
                 remote `{remote}`; nothing was pinned"
            ),
            Self::Refs(error) => error.fmt(f),
            Self::Remote(error) => error.fmt(f),
            Self::Log(error @ OpLogError::NotFlushed { .. }) => {
                write!(f, "{error}; the configuration still holds the old key")
            }
            Self::Log(error) => error.fmt(f),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                f,
                "could not {operation} `{}`: {source}; the entry `remote-pin` is in the log",
                path.display()
            ),
        }
    }
}

impl Error for PinError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message has the message of the cause, so the source is the cause of the cause.
        match self {
            Self::Refs(error) => error.source(),
            Self::Remote(error) => error.source(),
            Self::Log(error) => error.source(),
            Self::Io { source, .. } => source.source(),
            Self::NotHuman { .. }
            | Self::Unexpected { .. }
            | Self::Denied { .. }
            | Self::NoRemote(_)
            | Self::OtherRepository(_) => None,
        }
    }
}

impl From<RefError> for PinError {
    fn from(error: RefError) -> Self {
        Self::Refs(error)
    }
}

impl From<RemoteError> for PinError {
    fn from(error: RemoteError) -> Self {
        Self::Remote(error)
    }
}

impl From<OpLogError> for PinError {
    fn from(error: OpLogError) -> Self {
        Self::Log(error)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::oplog::{Entry, Record};
    use crate::protocol::record::tests::{signed_former_of, Seeded};
    use crate::protocol::{FormerKey, FormerKeys};
    use crate::remote::RemoteAddress;
    use crate::repository::InitOptions;
    use crate::testutil::TempDir;
    use crate::{AllowAll, Decision};

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &OperationName, _target: &str) -> Decision {
            Decision::Deny
        }
    }

    fn daniel() -> Actor {
        Actor::human("daniel").unwrap()
    }

    fn origin() -> RemoteName {
        RemoteName::try_new("origin").unwrap()
    }

    fn old_hub() -> Seeded {
        Seeded::new(0x60)
    }

    fn new_hub() -> Seeded {
        Seeded::new(0x61)
    }

    /// The list of the hub after it rotated from the old key to the new one at entry 41.
    fn rotated() -> RefList {
        let former = vec![FormerKey::try_new(1, 40, old_hub().public()).unwrap()];
        RefList::try_new(
            "0123456789abcdef0123456789abcdef".parse().unwrap(),
            new_hub().public(),
            signed_former_of(
                "0123456789abcdef0123456789abcdef".parse().unwrap(),
                &new_hub(),
                former,
            ),
            Vec::new(),
        )
        .unwrap()
    }

    /// A repository with the remote `origin`, which pinned `key`.
    fn repository(dir: &TempDir, key: Option<&Seeded>) -> Repository {
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &daniel(), &AllowAll).unwrap();
        let mut config = RemoteConfig::new(
            RemoteAddress::try_new("https://hub.example/talent-factory/stemma").unwrap(),
            "0123456789abcdef0123456789abcdef".parse().unwrap(),
        );
        config.hub_key = key.map(Seeded::public);
        let remotes = repository.metadata_dir().join(crate::remote::REMOTES_DIR);
        fs::create_dir_all(&remotes).unwrap();
        fs::write(remotes.join("origin"), config.encode()).unwrap();
        repository
    }

    fn entries(repository: &Repository) -> Vec<Entry> {
        repository.oplog().entries().unwrap()
    }

    #[test]
    fn pinning_again_writes_its_entry_and_then_the_new_key() {
        let dir = TempDir::new();
        let repository = repository(&dir, Some(&old_hub()));
        let before = entries(&repository).len();
        // A repository of the version before: the entry moves it to the version that knows it.
        let format = repository.metadata_dir().join("format");
        fs::write(&format, "stemma-repository 17\n").unwrap();
        let list = rotated();
        let expected = new_hub().public().fingerprint();
        let entry = repository
            .pin_hub_key_again(&origin(), &list, &expected, &daniel(), &AllowAll)
            .unwrap()
            .unwrap();
        let log = entries(&repository);
        assert_eq!(log.len(), before + 1);
        assert_eq!(log.last(), Some(&entry));
        let Record::RemotePin(pin) = entry.record() else {
            panic!("{entry:?}");
        };
        assert_eq!(entry.remote_pin(), Some(pin));
        assert_eq!(pin.remote(), &origin());
        assert_eq!(pin.old(), Some(&old_hub().public()));
        assert_eq!(pin.new_key(), &new_hub().public());
        assert_eq!(pin.actor(), &daniel());
        assert!(entry.commits().is_empty());
        // The configuration holds the new key, and the old entries are checked through the
        // former keys of the list.
        let config = repository.remote(&origin()).unwrap().unwrap();
        assert_eq!(config.pinned_key(), Ok(&new_hub().public()));
        let keys = config.hub_keys(&list).unwrap();
        assert_eq!(keys.at(40), &old_hub().public());
        assert_eq!(keys.at(41), &new_hub().public());
        // The entry moved the repository to the current format, and the log verifies.
        assert_eq!(
            fs::read_to_string(&format).unwrap(),
            "stemma-repository 18\n"
        );
        assert!(repository.oplog().verify().is_ok());

        // The same key again changes nothing and writes nothing.
        assert_eq!(
            repository
                .pin_hub_key_again(&origin(), &list, &expected, &daniel(), &AllowAll)
                .unwrap(),
            None
        );
        assert_eq!(entries(&repository).len(), before + 1);
    }

    #[test]
    fn a_failure_after_the_entry_leaves_the_old_key_and_pinning_again_finishes() {
        let dir = TempDir::new();
        let repository = repository(&dir, Some(&old_hub()));
        let before = entries(&repository).len();
        let list = rotated();
        let expected = new_hub().public().fingerprint();
        let remotes = repository.metadata_dir().join(crate::remote::REMOTES_DIR);
        let error = {
            let _guard = crate::fault::deny_writes_in(&remotes);
            repository
                .pin_hub_key_again(&origin(), &list, &expected, &daniel(), &AllowAll)
                .unwrap_err()
        };
        assert!(matches!(error, PinError::Io { .. }), "{error:?}");
        assert!(error.io_error().is_some());
        assert!(error.to_string().contains("remote-pin"), "{error}");
        // The entry is in the log, and the configuration still holds the old key.
        let log = entries(&repository);
        assert_eq!(log.len(), before + 1);
        assert!(log.last().unwrap().remote_pin().is_some());
        let config = repository.remote(&origin()).unwrap().unwrap();
        assert_eq!(config.pinned_key(), Ok(&old_hub().public()));
        // Pinning again writes a second entry, from the same old key, and finishes.
        let entry = repository
            .pin_hub_key_again(&origin(), &list, &expected, &daniel(), &AllowAll)
            .unwrap()
            .unwrap();
        assert_eq!(entry.remote_pin().unwrap().old(), Some(&old_hub().public()));
        assert_eq!(entries(&repository).len(), before + 2);
        let config = repository.remote(&origin()).unwrap().unwrap();
        assert_eq!(config.pinned_key(), Ok(&new_hub().public()));
    }

    #[test]
    fn a_remote_without_a_key_is_pinned_again_with_old_unset() {
        let dir = TempDir::new();
        let repository = repository(&dir, None);
        let expected = new_hub().public().fingerprint();
        let entry = repository
            .pin_hub_key_again(&origin(), &rotated(), &expected, &daniel(), &AllowAll)
            .unwrap()
            .unwrap();
        assert_eq!(entry.remote_pin().unwrap().old(), None);
        assert!(String::from_utf8(entry.encode())
            .unwrap()
            .contains("\nold -\n"));
    }

    #[test]
    fn pinning_again_is_refused_before_anything_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir, Some(&old_hub()));
        let before = entries(&repository).len();
        let config = repository.remote(&origin()).unwrap();
        let list = rotated();
        let expected = new_hub().public().fingerprint();
        let agent = Actor::agent("a1", "model", "s1", "daniel").unwrap();
        let refused = [
            // An agent does not pin the key of a hub.
            repository
                .pin_hub_key_again(&origin(), &list, &expected, &agent, &AllowAll)
                .unwrap_err(),
            // The fingerprint from the second channel is another one.
            repository
                .pin_hub_key_again(
                    &origin(),
                    &list,
                    &old_hub().public().fingerprint(),
                    &daniel(),
                    &AllowAll,
                )
                .unwrap_err(),
            repository
                .pin_hub_key_again(&origin(), &list, &expected, &daniel(), &DenyAll)
                .unwrap_err(),
            repository
                .pin_hub_key_again(
                    &RemoteName::try_new("upstream").unwrap(),
                    &list,
                    &expected,
                    &daniel(),
                    &AllowAll,
                )
                .unwrap_err(),
        ];
        assert!(
            matches!(refused[0], PinError::NotHuman { .. }),
            "{:?}",
            refused[0]
        );
        assert!(
            matches!(
                &refused[1],
                PinError::Unexpected { expected: e, offered: o }
                    if *e == old_hub().public().fingerprint() && *o == expected
            ),
            "{:?}",
            refused[1]
        );
        assert!(
            matches!(refused[2], PinError::Denied { .. }),
            "{:?}",
            refused[2]
        );
        assert!(
            matches!(refused[3], PinError::NoRemote(_)),
            "{:?}",
            refused[3]
        );
        for error in &refused {
            assert!(!error.to_string().is_empty());
            assert!(error.io_error().is_none());
            crate::fuzz::check_rejection(error);
        }
        assert_eq!(entries(&repository).len(), before);
        assert_eq!(repository.remote(&origin()).unwrap(), config);
        // The list of another repository of the hub is refused as well.
        let other = RefList::try_new(
            "ffeeddccbbaa99887766554433221100".parse().unwrap(),
            new_hub().public(),
            FormerKeys::none(),
            Vec::new(),
        )
        .unwrap();
        let error = repository
            .pin_hub_key_again(&origin(), &other, &expected, &daniel(), &AllowAll)
            .unwrap_err();
        assert!(matches!(error, PinError::OtherRepository(_)), "{error:?}");
        assert!(!error.to_string().is_empty());
        assert_eq!(entries(&repository).len(), before);
        assert_eq!(repository.remote(&origin()).unwrap(), config);
    }

    #[test]
    fn a_bare_repository_refuses_to_pin_again() {
        let dir = TempDir::new();
        let repository =
            Repository::init_bare(dir.path().join("hub"), &daniel(), &AllowAll).unwrap();
        let error = repository
            .pin_hub_key_again(
                &origin(),
                &rotated(),
                &new_hub().public().fingerprint(),
                &daniel(),
                &AllowAll,
            )
            .unwrap_err();
        assert!(
            matches!(error, PinError::Refs(RefError::Bare { .. })),
            "{error:?}"
        );
    }

    #[test]
    fn the_unsigned_former_keys_of_none_fit_the_new_key() {
        // A hub that never rotated has no former keys; pinning its key is pinning the key alone.
        let list = RefList::try_new(
            "0123456789abcdef0123456789abcdef".parse().unwrap(),
            new_hub().public(),
            FormerKeys::none(),
            Vec::new(),
        )
        .unwrap();
        let dir = TempDir::new();
        let repository = repository(&dir, Some(&old_hub()));
        let expected = new_hub().public().fingerprint();
        assert!(repository
            .pin_hub_key_again(&origin(), &list, &expected, &daniel(), &AllowAll)
            .unwrap()
            .is_some());
    }
}
