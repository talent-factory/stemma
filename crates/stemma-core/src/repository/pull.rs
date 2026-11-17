//! The operations on a pull request (ADR-004, STEMMA-238).
//!
//! Every operation takes the lock of the references, reads the log under it, checks the rule, and
//! writes the entry. The lock makes the check and the entry one step: two actors never open a
//! pull request for the same branch, and nobody closes one that another merges (ADR-008). The
//! operations move no pointer; the merge gate that moves `main` records its merge with
//! [`Repository::record_pull_merge`] (ADR-019).

use std::error::Error;
use std::fmt;

use super::{now, Repository};
use crate::lease;
use crate::object::CommitId;
use crate::oplog::{
    Entry, EntryError, OpLogError, OperationName, PullEntry, PR_CLOSE, PR_MERGE, PR_OPEN,
};
use crate::pull::{self, PullRequest, TARGET};
use crate::refs::{BranchName, RefError};
use crate::register::{Identity, Register};
use crate::store::ObjectStore;
use crate::{Actor, Authorizer};

impl Repository {
    /// The pull requests that the log tells of, in the order of their openings (see
    /// [`pull::pull_requests`]).
    ///
    /// # Errors
    ///
    /// Returns [`PullError::Log`] if the log cannot be read.
    pub fn pull_requests(&self) -> Result<Vec<PullRequest>, PullError> {
        let entries = self.oplog().entries().map_err(PullError::Log)?;
        Ok(pull::pull_requests(&entries))
    }

    /// The write owner of `branch` opens a pull request that would merge it into `main`.
    ///
    /// The rules, all checked under the lock of the references:
    ///
    /// - the branch exists and is not `main`;
    /// - only the write owner opens: where the branch has a lease ([`lease::applies`]), `actor`
    ///   holds a valid one; in open mode there is no lease and no other writer;
    /// - at most one pull request is open for a branch;
    /// - the opener names the reviewers, who are identities of the register, not the opener, and
    ///   without a repeat; in open mode there is no register, so there are none;
    /// - `repairs`, if given, is a commit of the store (ADR-019).
    ///
    /// The number of the pull request is the number of the entry that is returned.
    ///
    /// # Errors
    ///
    /// Returns [`PullError::Denied`] if the authorizer refuses, [`PullError::TargetBranch`],
    /// [`PullError::NoSuchBranch`], [`PullError::NotWriteOwner`], [`PullError::AlreadyOpen`],
    /// [`PullError::UnknownReviewer`], [`PullError::NoSuchCommit`] for a broken rule (nothing is
    /// written), and [`PullError::Refs`], [`PullError::Log`], and [`PullError::Entry`] for a
    /// failure of the lock, the log, or the entry.
    pub fn open_pull(
        &self,
        branch: &BranchName,
        actor: &Actor,
        reviewers: Vec<Actor>,
        repairs: Option<CommitId>,
        register: Option<&Register>,
        authorizer: &dyn Authorizer,
    ) -> Result<Entry, PullError> {
        let lock = self.lock_for_pull(PR_OPEN, branch.as_str(), actor, authorizer)?;
        if branch.as_str() == TARGET {
            return Err(PullError::TargetBranch);
        }
        let head = self
            .refs()
            .branch(branch)
            .map_err(PullError::Refs)?
            .ok_or_else(|| PullError::NoSuchBranch {
                branch: branch.clone(),
            })?;
        let now = now();
        if lease::applies(register, false) {
            let holds = self
                .lease(branch)
                .map_err(|_| PullError::NotWriteOwner {
                    branch: branch.clone(),
                })?
                .is_some_and(|lease| !lease.is_expired(now) && lease.holder() == actor);
            if !holds {
                return Err(PullError::NotWriteOwner {
                    branch: branch.clone(),
                });
            }
        }
        let pulls = self.pull_requests()?;
        if let Some(open) = pull::open_for(&pulls, branch) {
            return Err(PullError::AlreadyOpen {
                branch: branch.clone(),
                number: open.number(),
            });
        }
        for reviewer in &reviewers {
            let known =
                register.is_some_and(|register| register.entry(&Identity::of(reviewer)).is_some());
            if !known {
                return Err(PullError::UnknownReviewer {
                    reviewer: Box::new(reviewer.clone()),
                });
            }
        }
        if let Some(commit) = repairs {
            let present = self
                .objects()
                .contains(&commit.object_id())
                .map_err(|_| PullError::NoSuchCommit { commit })?;
            if !present {
                return Err(PullError::NoSuchCommit { commit });
            }
        }
        let entry = PullEntry::open(now, actor.clone(), branch.clone(), head, reviewers, repairs)?;
        Ok(self.append_pull(&lock, &entry)?)
    }

    /// Ends the open pull request `number` without a merge. Only the opener and, where the
    /// repository has a register, an owner may.
    ///
    /// # Errors
    ///
    /// Returns [`PullError::Denied`], [`PullError::NoSuchPull`] for a number that no opening in the
    /// log has, [`PullError::NotOpen`] for a pull request that is closed or merged already,
    /// [`PullError::NotAllowedToClose`] for an actor who may not, and the errors of the log and the
    /// lock (see [`open_pull`](Self::open_pull)).
    pub fn close_pull(
        &self,
        number: u64,
        actor: &Actor,
        register: Option<&Register>,
        authorizer: &dyn Authorizer,
    ) -> Result<Entry, PullError> {
        let lock = self.lock_for_pull(PR_CLOSE, &format!("pull {number}"), actor, authorizer)?;
        let pulls = self.pull_requests()?;
        let pull = open_pull_of(&pulls, number)?;
        let is_owner = register.is_some_and(|register| register.is_owner(actor));
        if pull.opener() != actor && !is_owner {
            return Err(PullError::NotAllowedToClose { number });
        }
        let entry = PullEntry::close(now(), actor.clone(), number)?;
        Ok(self.append_pull(&lock, &entry)?)
    }

    /// Records that the open pull request `number` was merged with the head `commit`. The entry is
    /// the record of the merge, not the merge: it moves no pointer, checks no approval, and runs
    /// no gate. The merge gate calls it after it moved `main` (ADR-019).
    ///
    /// # Errors
    ///
    /// Returns [`PullError::Denied`], [`PullError::NoSuchPull`], [`PullError::NotOpen`],
    /// [`PullError::NoSuchCommit`] if the store lacks `commit`, and the errors of the log and the
    /// lock (see [`open_pull`](Self::open_pull)).
    pub fn record_pull_merge(
        &self,
        number: u64,
        actor: &Actor,
        commit: CommitId,
        authorizer: &dyn Authorizer,
    ) -> Result<Entry, PullError> {
        let lock = self.lock_for_pull(PR_MERGE, &format!("pull {number}"), actor, authorizer)?;
        let pulls = self.pull_requests()?;
        open_pull_of(&pulls, number)?;
        let present = self
            .objects()
            .contains(&commit.object_id())
            .map_err(|_| PullError::NoSuchCommit { commit })?;
        if !present {
            return Err(PullError::NoSuchCommit { commit });
        }
        let entry = PullEntry::merge(now(), actor.clone(), number, commit)?;
        Ok(self.append_pull(&lock, &entry)?)
    }

    /// Asks the authorizer for `operation` and takes the lock of the references.
    fn lock_for_pull(
        &self,
        operation: &str,
        target: &str,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<crate::refs::WriteLock, PullError> {
        let name = OperationName::try_new(operation).expect("a pull request operation is a name");
        let allowed = self
            .authorize(actor, authorizer, &name, target)
            .map_err(|_| PullError::Denied {
                operation: operation.to_owned(),
                target: target.to_owned(),
            })?;
        Ok(self.refs_store().lock_writer(&allowed)?)
    }
}

/// The pull request `number` if it is open.
fn open_pull_of(pulls: &[PullRequest], number: u64) -> Result<&PullRequest, PullError> {
    let pull = pulls
        .iter()
        .find(|pull| pull.number() == number)
        .ok_or(PullError::NoSuchPull { number })?;
    if pull.is_open() {
        Ok(pull)
    } else {
        Err(PullError::NotOpen { number })
    }
}

/// Why an operation on a pull request fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum PullError {
    /// The authorizer refused the operation. Nothing was written.
    Denied {
        /// The name of the operation.
        operation: String,
        /// What it was asked for.
        target: String,
    },
    /// The branch is `main`, the target of every pull request.
    TargetBranch,
    /// The branch does not exist.
    NoSuchBranch {
        /// The branch.
        branch: BranchName,
    },
    /// The actor does not hold the lease of the branch: only the write owner opens.
    NotWriteOwner {
        /// The branch.
        branch: BranchName,
    },
    /// A pull request is open for the branch already.
    AlreadyOpen {
        /// The branch.
        branch: BranchName,
        /// The number of the open pull request.
        number: u64,
    },
    /// A reviewer is no identity of the register, or the repository has none.
    UnknownReviewer {
        /// The reviewer.
        reviewer: Box<Actor>,
    },
    /// The store has no commit of that ID.
    NoSuchCommit {
        /// The commit.
        commit: CommitId,
    },
    /// The log has no opening with this number.
    NoSuchPull {
        /// The number.
        number: u64,
    },
    /// The pull request is closed or merged.
    NotOpen {
        /// The number.
        number: u64,
    },
    /// Only the opener and an owner close a pull request.
    NotAllowedToClose {
        /// The number.
        number: u64,
    },
    /// The lock of the references or the references failed. Nothing was written.
    Refs(RefError),
    /// The log could not be read or written. See [`OpLogError`].
    Log(OpLogError),
    /// The entry breaks a rule of the entry, such as the reviewers. Nothing was written.
    Entry(EntryError),
}

impl fmt::Display for PullError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied { operation, target } => {
                write!(f, "`{operation}` on `{target}` is not allowed")
            }
            Self::TargetBranch => write!(
                f,
                "`{TARGET}` is the target of every pull request and cannot be its branch"
            ),
            Self::NoSuchBranch { branch } => write!(f, "the branch `{branch}` does not exist"),
            Self::NotWriteOwner { branch } => write!(
                f,
                "only the write owner of `{branch}` opens a pull request, and you do not hold its \
                 lease"
            ),
            Self::AlreadyOpen { branch, number } => write!(
                f,
                "the branch `{branch}` has an open pull request already: {number}"
            ),
            Self::UnknownReviewer { reviewer } => write!(
                f,
                "{reviewer} is no identity of the register; the opener names reviewers from it"
            ),
            Self::NoSuchCommit { commit } => write!(f, "the store has no commit {commit}"),
            Self::NoSuchPull { number } => write!(f, "the log has no pull request {number}"),
            Self::NotOpen { number } => {
                write!(f, "the pull request {number} is closed or merged already")
            }
            Self::NotAllowedToClose { number } => write!(
                f,
                "only the opener and an owner close the pull request {number}"
            ),
            Self::Refs(error) => error.fmt(f),
            Self::Log(error) => error.fmt(f),
            Self::Entry(error) => error.fmt(f),
        }
    }
}

impl Error for PullError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Refs(error) => Some(error),
            Self::Log(error) => Some(error),
            Self::Entry(error) => Some(error),
            _ => None,
        }
    }
}

impl From<RefError> for PullError {
    fn from(error: RefError) -> Self {
        Self::Refs(error)
    }
}

impl From<OpLogError> for PullError {
    fn from(error: OpLogError) -> Self {
        Self::Log(error)
    }
}

impl From<EntryError> for PullError {
    fn from(error: EntryError) -> Self {
        Self::Entry(error)
    }
}

#[cfg(test)]
mod tests {
    use ssh_key::private::Ed25519Keypair;
    use ssh_key::PrivateKey;

    use super::*;
    use crate::object::Blob;
    use crate::oplog::PullKind;
    use crate::pull::PullState;
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

    fn daniel() -> Actor {
        Actor::human("daniel").unwrap()
    }

    fn mia() -> Actor {
        Actor::human("mia").unwrap()
    }

    fn feature() -> BranchName {
        BranchName::try_new("feature/x").unwrap()
    }

    fn key(seed: u8) -> PublicKey {
        let private = PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
        PublicKey::from_openssh(&private.public_key().to_openssh().unwrap()).unwrap()
    }

    /// Daniel owns the register; Mia is a member.
    fn register() -> Register {
        let text = format!(
            "stemma-register 1\nentry human daniel owner\nkey {} active\n\
             entry human mia member\nkey {} active\n",
            key(1),
            key(2)
        );
        Register::parse(text.as_bytes()).unwrap()
    }

    /// A repository with the branch `feature/x` at the commit of the working copy.
    fn repository(dir: &crate::testutil::TempDir) -> (Repository, CommitId) {
        let repository =
            Repository::init(dir.path(), &InitOptions::default(), &daniel(), &AllowAll).unwrap();
        let start = repository.refs().working_copy().unwrap();
        let operation = OperationName::try_new("branch").unwrap();
        repository
            .create_branch(
                &operation,
                &feature(),
                start.object_id(),
                &daniel(),
                &AllowAll,
            )
            .unwrap();
        (repository, start)
    }

    fn format(dir: &TempDir) -> String {
        std::fs::read_to_string(dir.path().join(".stemma/format")).unwrap()
    }

    #[test]
    fn the_first_opening_writes_the_entry_and_raises_the_format() {
        let dir = TempDir::new();
        let (repository, start) = repository(&dir);
        let entry = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap();
        let pull = entry.pull().unwrap();
        assert_eq!(pull.kind(), PullKind::Open);
        assert_eq!(pull.commit(), Some(&start));
        assert_eq!(format(&dir), "stemma-repository 9\n");
        let pulls = repository.pull_requests().unwrap();
        assert_eq!(pulls.len(), 1);
        assert_eq!(pulls[0].number(), entry.number());
        assert!(pulls[0].is_open());
    }

    #[test]
    fn one_open_pull_request_per_branch_and_a_new_one_after_the_end() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let first = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap();
        let before = repository.oplog().entries().unwrap().len();
        let error = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap_err();
        assert!(
            matches!(error, PullError::AlreadyOpen { number, .. } if number == first.number()),
            "{error:?}"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        repository
            .close_pull(first.number(), &daniel(), None, &AllowAll)
            .unwrap();
        let second = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap();
        assert!(second.number() > first.number());
    }

    #[test]
    fn the_rules_of_an_opening_refuse_before_anything_is_written() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let before = repository.oplog().entries().unwrap().len();
        let main = BranchName::try_new("main").unwrap();
        let missing = BranchName::try_new("nowhere").unwrap();
        let other_commit = CommitId::new(Blob::new("not stored").id().object_id());
        let open = |branch: &BranchName, reviewers, repairs, authorizer: &dyn Authorizer| {
            repository.open_pull(branch, &daniel(), reviewers, repairs, None, authorizer)
        };
        assert!(matches!(
            open(&main, vec![], None, &AllowAll),
            Err(PullError::TargetBranch)
        ));
        assert!(matches!(
            open(&missing, vec![], None, &AllowAll),
            Err(PullError::NoSuchBranch { .. })
        ));
        assert!(matches!(
            open(&feature(), vec![mia()], None, &AllowAll),
            Err(PullError::UnknownReviewer { .. })
        ));
        assert!(matches!(
            open(&feature(), vec![], Some(other_commit), &AllowAll),
            Err(PullError::NoSuchCommit { .. })
        ));
        assert!(matches!(
            open(&feature(), vec![], None, &DenyAll),
            Err(PullError::Denied { .. })
        ));
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        assert!(repository.pull_requests().unwrap().is_empty());
    }

    #[test]
    fn only_the_write_owner_opens_where_the_branch_has_a_lease() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let register = register();
        repository
            .claim_lease(&feature(), &mia(), 60, &AllowAll)
            .unwrap();
        // The owner of the register is no write owner of the branch.
        let error = repository
            .open_pull(
                &feature(),
                &daniel(),
                vec![],
                None,
                Some(&register),
                &AllowAll,
            )
            .unwrap_err();
        assert!(
            matches!(error, PullError::NotWriteOwner { .. }),
            "{error:?}"
        );
        let entry = repository
            .open_pull(
                &feature(),
                &mia(),
                vec![daniel()],
                None,
                Some(&register),
                &AllowAll,
            )
            .unwrap();
        assert_eq!(entry.pull().unwrap().reviewers(), [daniel()]);
        assert_eq!(entry.actor(), &mia());
    }

    #[test]
    fn a_branch_without_a_lease_cannot_be_opened_where_a_lease_applies() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let error = repository
            .open_pull(
                &feature(),
                &mia(),
                vec![],
                None,
                Some(&register()),
                &AllowAll,
            )
            .unwrap_err();
        assert!(
            matches!(error, PullError::NotWriteOwner { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_reviewers_come_from_the_register_and_are_not_the_opener() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let register = register();
        repository
            .claim_lease(&feature(), &mia(), 60, &AllowAll)
            .unwrap();
        let open = |reviewers| {
            repository.open_pull(
                &feature(),
                &mia(),
                reviewers,
                None,
                Some(&register),
                &AllowAll,
            )
        };
        let stranger = Actor::human("zoe").unwrap();
        assert!(matches!(
            open(vec![stranger]),
            Err(PullError::UnknownReviewer { .. })
        ));
        assert!(matches!(
            open(vec![mia()]),
            Err(PullError::Entry(EntryError::BadReviewers))
        ));
        assert!(matches!(
            open(vec![daniel(), daniel()]),
            Err(PullError::Entry(EntryError::BadReviewers))
        ));
        assert!(open(vec![daniel()]).is_ok());
    }

    #[test]
    fn only_the_opener_and_an_owner_close() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let register = register();
        repository
            .claim_lease(&feature(), &mia(), 60, &AllowAll)
            .unwrap();
        let number = repository
            .open_pull(&feature(), &mia(), vec![], None, Some(&register), &AllowAll)
            .unwrap()
            .number();
        let stranger = Actor::human("zoe").unwrap();
        let error = repository
            .close_pull(number, &stranger, Some(&register), &AllowAll)
            .unwrap_err();
        assert!(
            matches!(error, PullError::NotAllowedToClose { .. }),
            "{error:?}"
        );
        let closed = repository
            .close_pull(number, &daniel(), Some(&register), &AllowAll)
            .unwrap();
        assert_eq!(closed.pull().unwrap().pull(), Some(number));
        let pulls = repository.pull_requests().unwrap();
        assert!(matches!(pulls[0].state(), PullState::Closed { by, .. } if by == &daniel()));
        // A closed pull request cannot be closed, merged, or found under another number.
        assert!(matches!(
            repository.close_pull(number, &mia(), None, &AllowAll),
            Err(PullError::NotOpen { .. })
        ));
        assert!(matches!(
            repository.close_pull(number + 50, &mia(), None, &AllowAll),
            Err(PullError::NoSuchPull { .. })
        ));
    }

    #[test]
    fn a_merge_is_recorded_once_and_ends_the_pull_request() {
        let dir = TempDir::new();
        let (repository, start) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        let absent = CommitId::new(Blob::new("not stored").id().object_id());
        assert!(matches!(
            repository.record_pull_merge(number, &daniel(), absent, &AllowAll),
            Err(PullError::NoSuchCommit { .. })
        ));
        let entry = repository
            .record_pull_merge(number, &daniel(), start, &AllowAll)
            .unwrap();
        assert_eq!(entry.pull().unwrap().commit(), Some(&start));
        let pulls = repository.pull_requests().unwrap();
        assert!(matches!(
            pulls[0].state(),
            PullState::Merged { commit, .. } if commit == &start
        ));
        for error in [
            repository
                .record_pull_merge(number, &daniel(), start, &AllowAll)
                .unwrap_err(),
            repository
                .close_pull(number, &daniel(), None, &AllowAll)
                .unwrap_err(),
        ] {
            assert!(matches!(error, PullError::NotOpen { .. }), "{error:?}");
        }
        repository.oplog().verify().expect("the chain holds");
    }

    #[test]
    fn an_entry_of_a_pull_request_cannot_be_restored() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let entry = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap();
        let error = repository
            .restore(entry.number(), false, &daniel(), &AllowAll)
            .unwrap_err();
        assert!(
            matches!(error, crate::repository::RestoreError::IsPull { .. }),
            "{error:?}"
        );
    }
}
