//! The operations on the lease of a branch (ADR-004, STEMMA-225).
//!
//! Every operation that changes a lease holds the lock of the references, so the check of the
//! lease and the move of the pointer that follows it are one step: two actors never write at once
//! (ADR-008). The order is the rule of ADR-011: the entry first, then the file. A crash between
//! the two leaves the old file, and the log shows an acquisition that did not take effect, as it
//! shows a move that did not happen. The next claim reads the old file and decides again.

use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{now, Repository};
use crate::fault;
use crate::lease::{self, Claim, Held, Lease, LeaseFileError};
use crate::oplog::{
    Entry, EntryError, LeaseEntry, OpLogError, OperationName, LEASE_OVERRIDE, LEASE_RELEASE,
};
use crate::refs::{BranchName, RefError, WriteLock};
use crate::register::{Identity, IdentityKind, Register};
use crate::repository::PolicyError;
use crate::store::{create_dirs_synced, create_temp, sync_directory, unique_temp_name, StoreError};
use crate::{Actor, Authorizer};

/// The name that the authorizer is asked for when an actor claims, renews, or releases a lease.
const CLAIM: &str = "lease";

/// What [`Repository::claim_lease`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    /// The lease after the claim.
    pub lease: Lease,
    /// The entry that records an acquisition or a takeover, or `None` for a renewal, which writes
    /// none.
    pub entry: Option<Entry>,
}

impl Repository {
    /// The directory of the files of the leases.
    fn leases_dir(&self) -> PathBuf {
        self.metadata_dir().join(lease::DIR)
    }

    /// The lease of a branch, or `None` if it has none. A lease that has expired is returned as it
    /// is; [`Lease::is_expired`] tells.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::Corrupt`] for a file that is not a lease of this branch and
    /// [`LeaseError::Io`] if it cannot be read.
    pub fn lease(&self, branch: &BranchName) -> Result<Option<Lease>, LeaseError> {
        read_lease(&self.leases_dir(), branch)
    }

    /// What `actor` has to do to write `branch`, and does it: acquires the lease, renews it, or
    /// takes it over after the expiry. The first writing operation on a branch without a valid
    /// lease acquires it; the holder renews it with every writing operation, for `minutes`. The
    /// caller decides whether the branch has a lease at all ([`lease::applies`]).
    ///
    /// The check and the writing of the entry happen under the lock of the references. An
    /// acquisition and a takeover write an entry; a renewal writes none (STEMMA-225).
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::Denied`] if the authorizer refuses, [`LeaseError::Held`] if another
    /// actor holds a valid lease (nothing is written), [`LeaseError::Refs`] for the lock (among
    /// them [`RefError::Locked`]: try again), and [`LeaseError::Log`] and [`LeaseError::Io`] for a
    /// failure of the entry or of the file. After [`LeaseError::Io`] from the file the entry is in
    /// the log, and the file is the old one or, if only the flush of the directory failed, the new
    /// one. After [`OpLogError::NotFlushed`] the entry may be in the log and no file is written.
    /// In every case the next claim reads the file and decides again.
    pub fn claim_lease(
        &self,
        branch: &BranchName,
        actor: &Actor,
        minutes: u32,
        authorizer: &dyn Authorizer,
    ) -> Result<Claimed, LeaseError> {
        let lock = self.lock_for_lease(branch, actor, CLAIM, authorizer)?;
        self.claim_lease_locked(&lock, branch, actor, minutes, now())
    }

    /// [`claim_lease`](Self::claim_lease) for a caller that holds the lock of the references
    /// already, as a writing operation does before it moves the pointer.
    pub(crate) fn claim_lease_locked(
        &self,
        lock: &WriteLock,
        branch: &BranchName,
        actor: &Actor,
        minutes: u32,
        now: i64,
    ) -> Result<Claimed, LeaseError> {
        let current = read_lease(&self.leases_dir(), branch)?;
        let expires = lease::expiry(now, minutes);
        let (entry, lease) = match lease::claim(current.as_ref(), actor, now)? {
            Claim::Renew => {
                let current = current.expect("a renewal needs a lease");
                (None, current.renewed(now, minutes))
            }
            Claim::Acquire => {
                let entry = LeaseEntry::acquire(now, actor.clone(), branch.clone(), expires)?;
                (Some(entry.clone()), granted(&entry))
            }
            Claim::Takeover { from } => {
                let entry =
                    LeaseEntry::takeover(now, actor.clone(), branch.clone(), from, expires)?;
                (Some(entry.clone()), granted(&entry))
            }
        };
        ensure_readable(&lease)?;
        let entry = match entry {
            Some(lease_entry) => Some(self.append_lease(lock, &lease_entry)?),
            None => None,
        };
        self.write_lease_file(&lease)?;
        Ok(Claimed { lease, entry })
    }

    /// The holder gives the lease back: the file is removed and the entry `lease-release` is
    /// written. Anyone may write the branch afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::NoLease`] if the branch has no valid lease (an expired one is taken
    /// over, not released), [`LeaseError::NotHolder`] if `actor` is not its holder, and the
    /// errors of [`claim_lease`](Self::claim_lease). After [`LeaseError::Io`] the entry
    /// `lease-release` is in the log, and the file may still stand (or come back after a crash)
    /// until its expiry; a second release writes a second entry.
    pub fn release_lease(
        &self,
        branch: &BranchName,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Entry, LeaseError> {
        let lock = self.lock_for_lease(branch, actor, LEASE_RELEASE, authorizer)?;
        let current = read_lease(&self.leases_dir(), branch)?
            .filter(|current| !current.is_expired(now()))
            .ok_or_else(|| LeaseError::NoLease {
                branch: branch.clone(),
            })?;
        if current.holder() != actor {
            return Err(LeaseError::NotHolder {
                branch: branch.clone(),
                holder: Box::new(current.holder().clone()),
            });
        }
        let entry = self.append_lease(
            &lock,
            &LeaseEntry::release(now(), actor.clone(), branch.clone()),
        )?;
        let path = self.leases_dir().join(lease::file_name(branch));
        fault::remove_file(&path).map_err(LeaseError::io("remove the lease file", &path))?;
        // An unflushed removal can come back after a crash as the old file, which is valid until
        // its expiry, so the release is not reported as done until the directory is flushed.
        sync_directory(&self.leases_dir())?;
        Ok(entry)
    }

    /// An owner ends the lease of another actor and holds it for `minutes`, for a `reason`. Only a
    /// human with the role owner in `register` may (ADR-004 rule 2); the entry names the earlier
    /// holder and the reason. The branch and the work of the earlier holder stay. The override
    /// changes who writes the branch and nothing about what the merge gate asks for (ADR-019).
    ///
    /// The entry is the act that the owner signs: the caller signs its hash (`Entry::hash`) with
    /// the key of the owner and writes the proof (`register::write_proof`), as for every entry
    /// that depends on who acts. The authorizer of an enforcing repository asks for that actor.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::NotOwner`] if `owner` is not a human with the role owner,
    /// [`LeaseError::NoLease`] if the branch has no valid lease (a lease that has expired is taken
    /// over, no override is needed), [`LeaseError::Entry`] for a reason that is empty, longer than
    /// 1 KiB, or on several lines, or if the owner holds the lease already, and the errors of
    /// [`claim_lease`](Self::claim_lease).
    pub fn override_lease(
        &self,
        branch: &BranchName,
        owner: &Actor,
        register: &Register,
        minutes: u32,
        reason: &str,
        authorizer: &dyn Authorizer,
    ) -> Result<Claimed, LeaseError> {
        if Identity::of(owner).kind() != IdentityKind::Human || !register.is_owner(owner) {
            return Err(LeaseError::NotOwner {
                actor: Box::new(owner.clone()),
            });
        }
        let lock = self.lock_for_lease(branch, owner, LEASE_OVERRIDE, authorizer)?;
        let now = now();
        let current = read_lease(&self.leases_dir(), branch)?
            .filter(|current| !current.is_expired(now))
            .ok_or_else(|| LeaseError::NoLease {
                branch: branch.clone(),
            })?;
        if current.holder() == owner {
            return Err(LeaseError::AlreadyHolder {
                branch: branch.clone(),
            });
        }
        let entry = LeaseEntry::owner_override(
            now,
            owner.clone(),
            branch.clone(),
            current.holder().clone(),
            lease::expiry(now, minutes),
            reason,
        )?;
        let lease = granted(&entry);
        ensure_readable(&lease)?;
        let written = self.append_lease(&lock, &entry)?;
        self.write_lease_file(&lease)?;
        Ok(Claimed {
            lease,
            entry: Some(written),
        })
    }

    /// Asks the authorizer for `operation` on `branch` and takes the lock of the references.
    fn lock_for_lease(
        &self,
        branch: &BranchName,
        actor: &Actor,
        operation: &str,
        authorizer: &dyn Authorizer,
    ) -> Result<WriteLock, LeaseError> {
        let operation =
            OperationName::try_new(operation).expect("a lease operation is a valid name");
        let allowed = self
            .authorize(actor, authorizer, &operation, branch.as_str())
            .map_err(|_| LeaseError::Denied {
                operation: operation.to_string(),
                branch: branch.clone(),
            })?;
        Ok(self.refs_store().lock_writer(&allowed)?)
    }

    /// Replaces the file of the lease as a whole: a temporary file that is flushed, a rename, and
    /// a flush of the directory.
    fn write_lease_file(&self, lease: &Lease) -> Result<(), LeaseError> {
        let metadata = self.metadata_dir();
        let dir = self.leases_dir();
        create_dirs_synced(&metadata, &dir)?;
        let temp_dir = metadata.join("tmp");
        create_dirs_synced(&metadata, &temp_dir)?;
        let (mut file, mut temp) = create_temp(&temp_dir, &mut unique_temp_name)?;
        file.write_all(lease.encode().as_bytes())
            .map_err(LeaseError::io("write the lease file", &temp.path))?;
        file.sync_all()
            .map_err(LeaseError::io("flush the lease file", &temp.path))?;
        drop(file);
        let path = dir.join(lease::file_name(lease.branch()));
        fault::rename(&temp.path, &path)
            .map_err(LeaseError::io("replace the lease file", &path))?;
        temp.disarm();
        sync_directory(&dir)?;
        Ok(())
    }
}

/// The lease that an entry of an acquisition, a takeover, or an override grants.
fn granted(entry: &LeaseEntry) -> Lease {
    Lease::of_entry(entry).expect("an entry that grants a lease has an expiry")
}

/// Refuses a lease whose file the reader would refuse, before any entry is written: a file over
/// the limit would lock the branch for everyone, because a damaged file is never taken for a free
/// branch.
fn ensure_readable(lease: &Lease) -> Result<(), LeaseError> {
    let len = lease.encode().len();
    if len as u64 > lease::MAX_BYTES {
        return Err(LeaseError::TooLarge { len });
    }
    Ok(())
}

/// Reads the file of a lease in `dir`.
fn read_lease(dir: &Path, branch: &BranchName) -> Result<Option<Lease>, LeaseError> {
    let path = dir.join(lease::file_name(branch));
    let bytes = match fault::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(LeaseError::Io {
                operation: "read the lease file",
                path,
                source,
            })
        }
    };
    if bytes.len() as u64 > lease::MAX_BYTES {
        return Err(LeaseError::Corrupt {
            path,
            source: LeaseFileError::Shape,
        });
    }
    let lease = Lease::parse(&bytes).map_err(|source| LeaseError::Corrupt {
        path: path.clone(),
        source,
    })?;
    // The name of the file and the branch in it must agree, or a file was moved by hand.
    if lease.branch() != branch {
        return Err(LeaseError::Corrupt {
            path,
            source: LeaseFileError::Shape,
        });
    }
    Ok(Some(lease))
}

/// Why an operation on a lease fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum LeaseError {
    /// The authorizer refused the operation. Nothing was written.
    Denied {
        /// The name of the operation.
        operation: String,
        /// The branch.
        branch: BranchName,
    },
    /// Another actor holds a valid lease. Nothing was written.
    Held(Box<Held>),
    /// The lock of the references or the references failed. Nothing was written.
    Refs(RefError),
    /// The entry could not be written. See [`OpLogError`].
    Log(OpLogError),
    /// The entry that was built breaks a rule of the entry. Nothing was written.
    Entry(EntryError),
    /// The file of the lease is not a lease of this branch.
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        source: LeaseFileError,
    },
    /// The file of the lease would be longer than [`lease::MAX_BYTES`] (an actor with very long
    /// fields). Nothing was written.
    TooLarge {
        /// The length of the file in bytes.
        len: usize,
    },
    /// The owner holds the valid lease already: an override is for the lease of someone else.
    AlreadyHolder {
        /// The branch.
        branch: BranchName,
    },
    /// The branch has no lease (or, for an override, no valid one).
    NoLease {
        /// The branch.
        branch: BranchName,
    },
    /// The actor is not the holder of the lease it wants to release.
    NotHolder {
        /// The branch.
        branch: BranchName,
        /// The holder.
        holder: Box<Actor>,
    },
    /// An override needs a human with the role owner in the register.
    NotOwner {
        /// The actor who tried.
        actor: Box<Actor>,
    },
    /// The policy of the base commit that names the duration cannot be used (an enforcing
    /// repository). Nothing was written.
    Policy(PolicyError),
    /// The file system failed.
    Io {
        /// What was tried.
        operation: &'static str,
        /// The file.
        path: PathBuf,
        /// The error.
        source: io::Error,
    },
}

impl LeaseError {
    fn io(operation: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Self {
        let path = path.to_path_buf();
        move |source| Self::Io {
            operation,
            path,
            source,
        }
    }
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied { operation, branch } => {
                write!(f, "`{operation}` on `{branch}` is not allowed")
            }
            Self::Held(held) => held.fmt(f),
            Self::Refs(error) => error.fmt(f),
            Self::Log(error) => error.fmt(f),
            Self::Entry(error) => error.fmt(f),
            Self::Corrupt { path, source } => {
                write!(
                f,
                "the lease file {} is damaged or belongs to another branch: {source}; check it \
                 and remove it by hand once you know who held the lease",
                path.display()
            )
            }
            Self::TooLarge { len } => write!(
                f,
                "the lease file would be {len} bytes, more than the limit of {} bytes; the actor \
                 is too long",
                lease::MAX_BYTES
            ),
            Self::AlreadyHolder { branch } => write!(
                f,
                "you hold the lease of `{branch}` already; claim it again to renew it"
            ),
            Self::NoLease { branch } => write!(f, "the branch `{branch}` has no valid lease"),
            Self::NotHolder { branch, holder } => write!(
                f,
                "the lease of `{branch}` is held by {holder}, who alone can release it"
            ),
            Self::NotOwner { actor } => write!(
                f,
                "{actor} is not a human with the role owner, so it cannot override a lease"
            ),
            Self::Policy(error) => error.fmt(f),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "cannot {operation} {}: {source}", path.display()),
        }
    }
}

impl Error for LeaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Held(error) => Some(error.as_ref()),
            Self::Refs(error) => Some(error),
            Self::Log(error) => Some(error),
            Self::Entry(error) => Some(error),
            Self::Policy(error) => Some(error),
            Self::Corrupt { source, .. } => Some(source),
            Self::Io { source, .. } => Some(source),
            Self::Denied { .. }
            | Self::TooLarge { .. }
            | Self::AlreadyHolder { .. }
            | Self::NoLease { .. }
            | Self::NotHolder { .. }
            | Self::NotOwner { .. } => None,
        }
    }
}

impl From<Box<Held>> for LeaseError {
    fn from(error: Box<Held>) -> Self {
        Self::Held(error)
    }
}

impl From<RefError> for LeaseError {
    fn from(error: RefError) -> Self {
        Self::Refs(error)
    }
}

impl From<OpLogError> for LeaseError {
    fn from(error: OpLogError) -> Self {
        Self::Log(error)
    }
}

impl From<EntryError> for LeaseError {
    fn from(error: EntryError) -> Self {
        Self::Entry(error)
    }
}

impl From<StoreError> for LeaseError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Io {
                operation,
                path,
                source,
            } => Self::Io {
                operation,
                path,
                source,
            },
            other => Self::Io {
                operation: "write the lease file",
                path: PathBuf::new(),
                source: io::Error::other(other.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ssh_key::private::Ed25519Keypair;
    use ssh_key::PrivateKey;

    use super::*;
    use crate::fault;
    use crate::oplog::{LeaseKind, Record, EXPIRED};
    use crate::repository::InitOptions;
    use crate::signature::PublicKey;
    use crate::testutil::TempDir;
    use crate::{AllowAll, Decision};

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &OperationName, _target: &str) -> Decision {
            Decision::Deny
        }
    }

    const MINUTES: u32 = 60;

    fn daniel() -> Actor {
        Actor::human("daniel").unwrap()
    }

    fn mia() -> Actor {
        Actor::human("mia").unwrap()
    }

    fn agent() -> Actor {
        Actor::agent("a1", "model", "s1", "mia").unwrap()
    }

    fn feature() -> BranchName {
        BranchName::try_new("feature/x").unwrap()
    }

    fn repository(dir: &TempDir) -> Repository {
        Repository::init(dir.path(), &InitOptions::default(), &daniel(), &AllowAll).unwrap()
    }

    fn key(seed: u8) -> PublicKey {
        let private = PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
        PublicKey::from_openssh(&private.public_key().to_openssh().unwrap()).unwrap()
    }

    /// Daniel owns the register; Mia is a member and so is the agent.
    fn register() -> Register {
        let text = format!(
            "stemma-register 1\nentry human daniel owner\nkey {} active\n\
             entry human mia member\nkey {} active\n",
            key(1),
            key(2)
        );
        Register::parse(text.as_bytes()).unwrap()
    }

    fn lease_entries(repository: &Repository) -> Vec<LeaseEntry> {
        repository
            .oplog()
            .entries()
            .unwrap()
            .iter()
            .filter_map(|entry| entry.lease().cloned())
            .collect()
    }

    fn claim(repository: &Repository, actor: &Actor, now: i64) -> Result<Claimed, LeaseError> {
        let lock = repository
            .lock_for_lease(&feature(), actor, CLAIM, &AllowAll)
            .unwrap();
        repository.claim_lease_locked(&lock, &feature(), actor, MINUTES, now)
    }

    #[test]
    fn the_first_write_acquires_and_the_next_renews_without_an_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        assert_eq!(repository.lease(&feature()).unwrap(), None);
        let first = repository
            .claim_lease(&feature(), &mia(), MINUTES, &AllowAll)
            .unwrap();
        let entry = first.entry.expect("an acquisition writes an entry");
        assert_eq!(entry.lease().unwrap().kind(), LeaseKind::Acquire);
        assert_eq!(first.lease.holder(), &mia());
        assert_eq!(
            repository.lease(&feature()).unwrap(),
            Some(first.lease.clone())
        );
        let before = repository.oplog().entries().unwrap().len();

        let again = repository
            .claim_lease(&feature(), &mia(), MINUTES, &AllowAll)
            .unwrap();
        assert_eq!(again.entry, None, "a renewal rides with the operation");
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        assert_eq!(again.lease.acquired(), first.lease.acquired());
        assert!(again.lease.expires() >= first.lease.expires());
    }

    #[test]
    fn another_actor_is_refused_with_holder_and_expiry_and_nothing_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let held = repository
            .claim_lease(&feature(), &mia(), MINUTES, &AllowAll)
            .unwrap();
        let before = repository.oplog().entries().unwrap().len();
        let error = repository
            .claim_lease(&feature(), &agent(), MINUTES, &AllowAll)
            .unwrap_err();
        let LeaseError::Held(held_error) = &error else {
            panic!("{error:?}");
        };
        assert_eq!(held_error.holder, mia());
        assert_eq!(held_error.expires, held.lease.expires());
        assert!(error.to_string().contains("human mia"), "{error}");
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        assert_eq!(repository.lease(&feature()).unwrap(), Some(held.lease));
    }

    #[test]
    fn the_lease_of_one_branch_does_not_touch_another() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        repository
            .claim_lease(&feature(), &mia(), MINUTES, &AllowAll)
            .unwrap();
        let other = BranchName::try_new("feature-x").unwrap();
        repository
            .claim_lease(&other, &agent(), MINUTES, &AllowAll)
            .unwrap();
        assert_eq!(
            repository.lease(&feature()).unwrap().unwrap().holder(),
            &mia()
        );
        assert_eq!(
            repository.lease(&other).unwrap().unwrap().holder(),
            &agent()
        );
    }

    #[test]
    fn after_the_expiry_anyone_takes_it_over_and_the_earlier_holder_acquires_again() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let t0 = 1_700_000_000;
        claim(&repository, &mia(), t0).unwrap();
        let end = t0 + i64::from(MINUTES) * 60;
        // One second before the end the lease holds.
        assert!(matches!(
            claim(&repository, &agent(), end - 1),
            Err(LeaseError::Held(_))
        ));
        // At the end it counts as free, and the next writer takes it with an entry.
        let taken = claim(&repository, &agent(), end).unwrap();
        let entry = taken.entry.unwrap();
        let lease = entry.lease().unwrap();
        assert_eq!(lease.kind(), LeaseKind::Takeover);
        assert_eq!(lease.from(), Some(&mia()));
        assert_eq!(lease.reason(), Some(EXPIRED));
        assert_eq!(taken.lease.holder(), &agent());
        // Nobody took the lease of the agent: the agent acquires its own expired lease again.
        let later = end + i64::from(MINUTES) * 60;
        let again = claim(&repository, &agent(), later).unwrap();
        assert_eq!(
            again.entry.unwrap().lease().unwrap().kind(),
            LeaseKind::Acquire
        );
        let kinds: Vec<_> = lease_entries(&repository)
            .iter()
            .map(LeaseEntry::kind)
            .collect();
        assert_eq!(
            kinds,
            [LeaseKind::Acquire, LeaseKind::Takeover, LeaseKind::Acquire]
        );
        repository.oplog().verify().expect("the chain holds");
    }

    #[test]
    fn a_crash_between_the_entry_and_the_file_leaves_the_old_file_and_the_next_claim_decides_again()
    {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let t0 = 1_700_000_000;
        claim(&repository, &mia(), t0).unwrap();
        let end = t0 + i64::from(MINUTES) * 60;
        let leases = repository.metadata_dir().join(lease::DIR);
        {
            // The rename of the file fails after the entry of the takeover is in the log.
            let _guard = fault::deny_writes_in(&leases);
            let error = claim(&repository, &agent(), end).unwrap_err();
            assert!(matches!(error, LeaseError::Io { .. }), "{error:?}");
        }
        assert_eq!(
            lease_entries(&repository).last().unwrap().kind(),
            LeaseKind::Takeover,
            "the entry names a takeover that did not take effect"
        );
        let kept = repository.lease(&feature()).unwrap().unwrap();
        assert_eq!(kept.holder(), &mia(), "the file is the old one, whole");
        // The next claim reads the file again and takes over: the state follows the file.
        let retry = claim(&repository, &agent(), end + 1).unwrap();
        assert_eq!(retry.lease.holder(), &agent());
        assert_eq!(
            repository.lease(&feature()).unwrap().unwrap().holder(),
            &agent()
        );
        repository.oplog().verify().expect("the chain holds");
    }

    #[test]
    fn a_lease_that_the_reader_would_refuse_is_never_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let before = repository.oplog().entries().unwrap().len();
        let long = Actor::agent("a1", "model", "s".repeat(4100), "mia").unwrap();
        let error = repository
            .claim_lease(&feature(), &long, MINUTES, &AllowAll)
            .unwrap_err();
        assert!(matches!(error, LeaseError::TooLarge { .. }), "{error:?}");
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        assert_eq!(repository.lease(&feature()).unwrap(), None);
    }

    #[test]
    fn a_locked_repository_refuses_every_lease_operation_and_writes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        repository
            .claim_lease(&feature(), &agent(), MINUTES, &AllowAll)
            .unwrap();
        let before = repository.oplog().entries().unwrap().len();
        let file = repository
            .metadata_dir()
            .join(lease::DIR)
            .join(lease::file_name(&feature()));
        let bytes = fs::read(&file).unwrap();
        fs::write(repository.metadata_dir().join("refs.lock"), "someone").unwrap();
        let locked = |error: LeaseError| {
            assert!(
                matches!(error, LeaseError::Refs(RefError::Locked { .. })),
                "{error:?}"
            );
        };
        locked(
            repository
                .claim_lease(&feature(), &mia(), MINUTES, &AllowAll)
                .unwrap_err(),
        );
        locked(
            repository
                .release_lease(&feature(), &agent(), &AllowAll)
                .unwrap_err(),
        );
        locked(
            repository
                .override_lease(
                    &feature(),
                    &daniel(),
                    &register(),
                    MINUTES,
                    "why",
                    &AllowAll,
                )
                .unwrap_err(),
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        assert_eq!(fs::read(&file).unwrap(), bytes);
    }

    #[test]
    fn an_expired_lease_is_not_released_and_an_owner_does_not_override_their_own() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let file = repository
            .metadata_dir()
            .join(lease::DIR)
            .join(lease::file_name(&feature()));
        repository
            .claim_lease(&feature(), &daniel(), MINUTES, &AllowAll)
            .unwrap();
        assert!(matches!(
            repository.override_lease(
                &feature(),
                &daniel(),
                &register(),
                MINUTES,
                "why",
                &AllowAll
            ),
            Err(LeaseError::AlreadyHolder { .. })
        ));
        let expired = Lease::new(feature(), agent(), 1, 2).unwrap();
        fs::write(&file, expired.encode()).unwrap();
        let before = repository.oplog().entries().unwrap().len();
        for actor in [agent(), mia()] {
            assert!(matches!(
                repository.release_lease(&feature(), &actor, &AllowAll),
                Err(LeaseError::NoLease { .. })
            ));
        }
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
    }

    #[test]
    fn a_failure_before_the_entry_writes_neither_entry_nor_file() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let before = repository.oplog().entries().unwrap().len();
        let error = repository
            .claim_lease(&feature(), &mia(), MINUTES, &DenyAll)
            .unwrap_err();
        assert!(matches!(error, LeaseError::Denied { .. }), "{error:?}");
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        assert_eq!(repository.lease(&feature()).unwrap(), None);
    }

    #[test]
    fn a_damaged_file_is_reported_and_is_not_taken_for_a_free_branch() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        repository
            .claim_lease(&feature(), &mia(), MINUTES, &AllowAll)
            .unwrap();
        let path = repository
            .metadata_dir()
            .join(lease::DIR)
            .join(lease::file_name(&feature()));
        fs::write(&path, "garbage\n").unwrap();
        let error = repository
            .claim_lease(&feature(), &agent(), MINUTES, &AllowAll)
            .unwrap_err();
        assert!(matches!(error, LeaseError::Corrupt { .. }), "{error:?}");
        // A lease of another branch under this name is damage as well.
        let other = lease::Lease::new(BranchName::try_new("main").unwrap(), mia(), 1, 2).unwrap();
        fs::write(&path, other.encode()).unwrap();
        assert!(matches!(
            repository.lease(&feature()),
            Err(LeaseError::Corrupt { .. })
        ));
    }

    #[test]
    fn the_holder_releases_and_anyone_may_write_afterwards() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        assert!(matches!(
            repository.release_lease(&feature(), &mia(), &AllowAll),
            Err(LeaseError::NoLease { .. })
        ));
        repository
            .claim_lease(&feature(), &mia(), MINUTES, &AllowAll)
            .unwrap();
        assert!(matches!(
            repository.release_lease(&feature(), &agent(), &AllowAll),
            Err(LeaseError::NotHolder { .. })
        ));
        let entry = repository
            .release_lease(&feature(), &mia(), &AllowAll)
            .unwrap();
        assert_eq!(entry.lease().unwrap().kind(), LeaseKind::Release);
        assert_eq!(repository.lease(&feature()).unwrap(), None);
        let taken = repository
            .claim_lease(&feature(), &agent(), MINUTES, &AllowAll)
            .unwrap();
        assert_eq!(
            taken.entry.unwrap().lease().unwrap().kind(),
            LeaseKind::Acquire
        );
    }

    #[test]
    fn an_owner_overrides_a_valid_lease_with_a_reason_and_the_branch_stays() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let register = register();
        let before = repository.oplog().entries().unwrap().len();
        repository
            .claim_lease(&feature(), &agent(), MINUTES, &AllowAll)
            .unwrap();
        let claimed = repository
            .override_lease(
                &feature(),
                &daniel(),
                &register,
                MINUTES,
                "the agent is stuck",
                &AllowAll,
            )
            .unwrap();
        let entry = claimed.entry.unwrap();
        let lease = entry.lease().unwrap();
        assert_eq!(lease.kind(), LeaseKind::Override);
        assert_eq!(lease.from(), Some(&agent()));
        assert_eq!(lease.reason(), Some("the agent is stuck"));
        assert_eq!(entry.actor(), &daniel());
        assert_eq!(
            repository.lease(&feature()).unwrap().unwrap().holder(),
            &daniel()
        );
        // The override moved no pointer: no branch and no commit came with it.
        assert!(entry.commits().is_empty());
        assert_eq!(repository.oplog().entries().unwrap().len(), before + 2);
        repository.oplog().verify().expect("the chain holds");
    }

    #[test]
    fn only_a_human_owner_overrides() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let register = register();
        repository
            .claim_lease(&feature(), &agent(), MINUTES, &AllowAll)
            .unwrap();
        for actor in [mia(), agent(), Actor::human("stranger").unwrap()] {
            let error = repository
                .override_lease(&feature(), &actor, &register, MINUTES, "why", &AllowAll)
                .unwrap_err();
            assert!(matches!(error, LeaseError::NotOwner { .. }), "{error:?}");
        }
        assert_eq!(
            repository.lease(&feature()).unwrap().unwrap().holder(),
            &agent()
        );
    }

    #[test]
    fn an_override_needs_a_valid_lease_and_a_plain_reason() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let register = register();
        assert!(matches!(
            repository.override_lease(&feature(), &daniel(), &register, MINUTES, "why", &AllowAll),
            Err(LeaseError::NoLease { .. })
        ));
        repository
            .claim_lease(&feature(), &agent(), MINUTES, &AllowAll)
            .unwrap();
        for reason in ["", "two\nlines", &"x".repeat(1025)] {
            assert!(
                matches!(
                    repository.override_lease(
                        &feature(),
                        &daniel(),
                        &register,
                        MINUTES,
                        reason,
                        &AllowAll
                    ),
                    Err(LeaseError::Entry(EntryError::BadReason))
                ),
                "{reason:?}"
            );
        }
        // An override of an expired lease is no override: that one is taken over.
        let file = repository
            .metadata_dir()
            .join(lease::DIR)
            .join(lease::file_name(&feature()));
        let expired = Lease::new(feature(), agent(), 1, 2).unwrap();
        fs::write(file, expired.encode()).unwrap();
        assert!(matches!(
            repository.override_lease(&feature(), &daniel(), &register, MINUTES, "why", &AllowAll),
            Err(LeaseError::NoLease { .. })
        ));
    }

    #[test]
    fn a_crash_between_the_release_entry_and_the_removal_keeps_the_old_lease_in_force() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        repository
            .claim_lease(&feature(), &mia(), MINUTES, &AllowAll)
            .unwrap();
        let leases = repository.metadata_dir().join(lease::DIR);
        {
            let _guard = fault::deny_writes_in(&leases);
            let error = repository
                .release_lease(&feature(), &mia(), &AllowAll)
                .unwrap_err();
            assert!(matches!(error, LeaseError::Io { .. }), "{error:?}");
        }
        assert_eq!(
            lease_entries(&repository).last().unwrap().kind(),
            LeaseKind::Release
        );
        // The file decides: the lease still holds against another actor.
        assert!(matches!(
            repository.claim_lease(&feature(), &agent(), MINUTES, &AllowAll),
            Err(LeaseError::Held(_))
        ));
        // The holder releases again, which writes a second entry and removes the file.
        repository
            .release_lease(&feature(), &mia(), &AllowAll)
            .unwrap();
        assert_eq!(repository.lease(&feature()).unwrap(), None);
        repository.oplog().verify().expect("the chain holds");
    }

    #[test]
    fn a_crash_between_the_override_entry_and_the_file_leaves_the_old_holder() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        repository
            .claim_lease(&feature(), &agent(), MINUTES, &AllowAll)
            .unwrap();
        let leases = repository.metadata_dir().join(lease::DIR);
        {
            let _guard = fault::deny_writes_in(&leases);
            let error = repository
                .override_lease(
                    &feature(),
                    &daniel(),
                    &register(),
                    MINUTES,
                    "why",
                    &AllowAll,
                )
                .unwrap_err();
            assert!(matches!(error, LeaseError::Io { .. }), "{error:?}");
        }
        assert_eq!(
            lease_entries(&repository).last().unwrap().kind(),
            LeaseKind::Override
        );
        assert_eq!(
            repository.lease(&feature()).unwrap().unwrap().holder(),
            &agent()
        );
        // The owner repeats the override, and now it takes effect.
        repository
            .override_lease(
                &feature(),
                &daniel(),
                &register(),
                MINUTES,
                "why",
                &AllowAll,
            )
            .unwrap();
        assert_eq!(
            repository.lease(&feature()).unwrap().unwrap().holder(),
            &daniel()
        );
    }

    #[test]
    fn a_branch_with_a_long_name_can_hold_a_lease() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let long = BranchName::try_new(format!("{}x", "a.".repeat(99))).unwrap();
        let claimed = repository
            .claim_lease(&long, &mia(), MINUTES, &AllowAll)
            .unwrap();
        assert_eq!(repository.lease(&long).unwrap(), Some(claimed.lease));
    }

    #[test]
    fn a_lease_entry_cannot_be_restored_and_no_purge_lists_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let claimed = repository
            .claim_lease(&feature(), &mia(), MINUTES, &AllowAll)
            .unwrap();
        let number = claimed.entry.unwrap().number();
        let error = repository
            .restore(number, false, &daniel(), &AllowAll)
            .map(|_| ())
            .unwrap_err();
        assert!(
            matches!(error, crate::repository::RestoreError::IsLease { .. }),
            "{error:?}"
        );
        assert!(matches!(
            repository.oplog().read(number).unwrap().record(),
            Record::Lease(_)
        ));
    }

    #[test]
    fn the_first_lease_entry_raises_the_format_and_a_newer_format_is_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let format = repository.metadata_dir().join("format");
        assert_eq!(
            fs::read_to_string(&format).unwrap(),
            "stemma-repository 11\n"
        );
        // A repository of version 7 moves to the current version with its next entry, a lease
        // entry among them.
        fs::write(&format, "stemma-repository 7\n").unwrap();
        repository
            .claim_lease(&feature(), &mia(), MINUTES, &AllowAll)
            .unwrap();
        assert_eq!(
            fs::read_to_string(&format).unwrap(),
            "stemma-repository 11\n"
        );
        // A program that reads up to version 11 refuses version 12: the number is not one of
        // its own.
        fs::write(&format, "stemma-repository 12\n").unwrap();
        let error = Repository::open(dir.path()).map(|_| ()).unwrap_err();
        assert!(
            matches!(error, crate::repository::OpenError::UnsupportedFormat { ref found, .. } if found == "12"),
            "{error:?}"
        );
    }
}
