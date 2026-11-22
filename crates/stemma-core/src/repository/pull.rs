//! The operations on a pull request (ADR-004, STEMMA-238).
//!
//! Every operation takes the lock of the references, reads the log under it, checks the rule, and
//! writes the entry. The lock makes the check and the entry one step: two actors never open a
//! pull request for the same branch, and nobody closes one that another merges (ADR-008). The
//! operations move no pointer; the merge gate that moves `main` records its merge with
//! [`Repository::record_pull_merge`] (ADR-019).

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::io;

use super::{now, Repository};
use crate::allowed::{self_approval, SelfApproval};
use crate::fault;
use crate::history::{self, HistoryError, Parents};
use crate::lease;
use crate::object::{CommitId, Kind};
use crate::oplog::{
    Entry, EntryError, OpLogError, OperationName, PullEntry, PR_APPROVE, PR_CLOSE, PR_MERGE,
    PR_OPEN,
};
use crate::pull::{self, Approval, PullRequest, Recount, TARGET};
use crate::refs::{BranchName, RefError};
use crate::register::{self, Identity, ProofError, ProofFile, Register};
use crate::repository::LeaseError;
use crate::signature::{self, PayloadKind, SignError, Signer};
use crate::store::StoreError;
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
                .lease(branch)?
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
            self.ensure_commit(commit)?;
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
        self.record_pull_merge_locked(&lock, number, actor, commit)
    }

    /// The body of [`record_pull_merge`](Self::record_pull_merge) for a caller that holds the lock
    /// of the references already, as the merge gate does: it records the merge in the same
    /// critical section that moved `main`, so that no second gate merges the pull request again.
    pub(super) fn record_pull_merge_locked(
        &self,
        lock: &crate::refs::WriteLock,
        number: u64,
        actor: &Actor,
        commit: CommitId,
    ) -> Result<Entry, PullError> {
        let pulls = self.pull_requests()?;
        open_pull_of(&pulls, number)?;
        self.ensure_commit(commit)?;
        let entry = PullEntry::merge(now(), actor.clone(), number, commit)?;
        Ok(self.append_pull(lock, &entry)?)
    }

    /// `actor` approves the head of the open pull request `number`, which is the tip of its
    /// branch now. The entry `pr-approve` names the pull request and that head; it binds the
    /// approval to it, and any later commit voids it (ADR-004 rule 4).
    ///
    /// The rules, checked under the lock of the references:
    ///
    /// - the pull request is open, and its branch still has the head that `actor` reviewed, which
    ///   is the tip when the call began;
    /// - `actor` is neither the opener, nor the author of a commit that the pull request brings,
    ///   nor the actor of an entry that moved its branch to such a commit ([`self_approval`]);
    /// - where the repository has a `register`, the approval is signed: `signer` signs
    ///   [`pull::approval_payload`] with a key that the register holds for `actor`, and the
    ///   signature is checked before the entry is written. The proof is the file `proofs/<entry>`
    ///   ([`ProofFile`]). In open mode there is no register and no signature.
    ///
    /// The signature is made before the lock is taken, because a key with a passphrase may ask a
    /// person, and nobody should wait for the lock meanwhile.
    ///
    /// The entry is written before the proof. A crash in between leaves an approval without a
    /// proof, which the recount does not count: the safe side.
    ///
    /// # Errors
    ///
    /// Returns [`PullError::Denied`], [`PullError::NoSuchPull`], [`PullError::NotOpen`],
    /// [`PullError::NoSuchBranch`], [`PullError::SelfApproval`] for the ban, [`PullError::HeadMoved`]
    /// if the branch moved while the signature was made, [`PullError::SignatureRequired`] for a
    /// register and no signer, [`PullError::Sign`] and [`PullError::Proof`] for a signature that
    /// cannot be made or that the register does not accept, [`PullError::KeyNotActive`] for a
    /// retired key, [`PullError::ProofNotWritten`] if the
    /// entry is in the log and its proof is not, and the errors of the log and the lock (see
    /// [`open_pull`](Self::open_pull)). Nothing is written for any error but the last two.
    pub fn approve_pull(
        &self,
        number: u64,
        actor: &Actor,
        register: Option<&Register>,
        signer: Option<&dyn Signer>,
        authorizer: &dyn Authorizer,
    ) -> Result<Entry, PullError> {
        let signed = match register {
            None => None,
            Some(_) => {
                let signer = signer.ok_or(PullError::SignatureRequired)?;
                let pulls = self.pull_requests()?;
                let pull = open_pull_of(&pulls, number)?;
                let head = self.branch_head(pull.branch())?;
                let payload = pull::approval_payload(number, &head);
                let signature = signature::sign(signer, PayloadKind::Approval, &payload)
                    .map_err(PullError::Sign)?;
                Some((head, payload, signature))
            }
        };
        let lock = self.lock_for_pull(PR_APPROVE, &format!("pull {number}"), actor, authorizer)?;
        let pulls = self.pull_requests()?;
        let pull = open_pull_of(&pulls, number)?;
        let head = self.branch_head(pull.branch())?;
        if let Some((signed_head, ..)) = &signed {
            if *signed_head != head {
                return Err(PullError::HeadMoved { number });
            }
        }
        let authors = self.pull_authors(&head)?;
        if let Some(reason) = self_approval(actor, pull.opener(), &authors) {
            return Err(PullError::SelfApproval { number, reason });
        }
        if let (Some(register), Some((_, payload, signature))) = (register, &signed) {
            let verified = register
                .verify_approval(actor, payload, signature, None)
                .map_err(|error| PullError::Proof(Box::new(error)))?;
            // A retired key keeps the approvals it made, but it signs nothing new.
            if !register.is_active_key(&Identity::of(actor), verified.key()) {
                return Err(PullError::KeyNotActive);
            }
        }
        let entry = PullEntry::approve(now(), actor.clone(), number, head)?;
        let written = self.append_pull(&lock, &entry)?;
        if let Some((_, _, signature)) = signed {
            let proof = ProofFile {
                actor: actor.clone(),
                signature,
            };
            register::write_proof(&self.metadata_dir(), written.number(), &proof).map_err(
                |source| PullError::ProofNotWritten {
                    entry: written.number(),
                    source,
                },
            )?;
        }
        Ok(written)
    }

    /// Which approvals of the pull request `number` count for its current head, as
    /// [`pull::recount`] decides, with the log, the history, and the proof files as its input. The
    /// head is the tip of the branch, so the pull request must be open. The `register` is the one
    /// of the base commit; without one there are no proofs to check.
    ///
    /// The recount trusts no writer: it asks the ban on self-approval again for every entry, and
    /// it checks every proof against the register.
    ///
    /// # Errors
    ///
    /// Returns [`PullError::NoSuchPull`], [`PullError::NotOpen`], [`PullError::NoSuchBranch`], and
    /// the errors of reading
    /// the log, the references, and the history. A damaged proof file is no error: the approval
    /// does not count, and the reason says why.
    pub fn recount_approvals(
        &self,
        number: u64,
        register: Option<&Register>,
    ) -> Result<Recount, PullError> {
        let entries = self.oplog().entries().map_err(PullError::Log)?;
        let pulls = pull::pull_requests(&entries);
        let pull = pulls
            .iter()
            .find(|pull| pull.number() == number)
            .ok_or(PullError::NoSuchPull { number })?;
        // Only an open pull request is counted: the authors are the commits that `main` lacks,
        // and after a merge `main` has all of them, so no author would be found.
        if !pull.is_open() {
            return Err(PullError::NotOpen { number });
        }
        let head = self.branch_head(pull.branch())?;
        let authors = self.pull_authors(&head)?;
        let metadata = self.metadata_dir();
        let payload = pull::approval_payload(number, &head);
        let proof = |approval: &Approval| -> Result<(), String> {
            let Some(register) = register else {
                return Ok(());
            };
            let path = register::proof_path(&metadata, approval.entry());
            let bytes = fault::read(&path).map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => "the approval has no proof".to_owned(),
                _ => format!("cannot read the proof: {error}"),
            })?;
            let text = String::from_utf8(bytes)
                .map_err(|_| "the proof file is not valid UTF-8".to_owned())?;
            let file = ProofFile::parse(&text).map_err(|error| error.to_string())?;
            if &file.actor != approval.reviewer() {
                return Err("the actor of the proof is not the reviewer".to_owned());
            }
            register
                .verify_approval(
                    approval.reviewer(),
                    &payload,
                    &file.signature,
                    Some(approval.entry()),
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        };
        Ok(pull::recount(
            pull,
            &head,
            &authors,
            &pull::approvals(&entries),
            &proof,
        ))
    }

    /// The tip of `branch`.
    pub(super) fn branch_head(&self, branch: &BranchName) -> Result<CommitId, PullError> {
        self.refs()
            .branch(branch)
            .map_err(PullError::Refs)?
            .ok_or_else(|| PullError::NoSuchBranch {
                branch: branch.clone(),
            })
    }

    /// The writers of the commits that a pull request with the head `head` brings: those that
    /// `head` reaches and `main` does not.
    ///
    /// A writer is the author of such a commit, and the actor of the entry of the log that first
    /// brought such a commit under a branch, whichever branch that was. A later move to a commit
    /// that a branch reached before, such as a branch that a reviewer makes at the tip to look at
    /// it, wrote nothing and does not count. The author is a claim of whoever made the commit, and
    /// `update_branch` does not compare it with the actor; the actor of the entry is the one that
    /// the authorizer let in (in open mode that is a claim as well, ADR-003). So a commit under a
    /// foreign author name still counts the actor that put it on a branch (ADR-004 rule 4,
    /// STEMMA-255).
    fn pull_authors(&self, head: &CommitId) -> Result<Vec<Actor>, PullError> {
        let store = self.objects();
        let own = history::ancestors(store, *head, Parents::All, None)?;
        let main = BranchName::try_new(TARGET).expect("`main` is a branch name");
        let known: HashSet<CommitId> = match self.refs().branch(&main).map_err(PullError::Refs)? {
            Some(tip) => history::ancestors(store, tip, Parents::All, None)?
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
            None => HashSet::new(),
        };
        let mut authors: Vec<Actor> = Vec::new();
        for (id, commit) in &own {
            if !known.contains(id) && !authors.contains(commit.author()) {
                authors.push(commit.author().clone());
            }
        }
        let brought: HashSet<CommitId> = own
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| !known.contains(id))
            .collect();
        // The entries are in order. A commit is reached once a branch moved to it or to a
        // descendant; the entry that does so first is the one that brought it.
        let mut reached: HashSet<CommitId> = HashSet::new();
        for entry in self.oplog().entries().map_err(PullError::Log)? {
            let Some(change) = entry.change() else {
                continue;
            };
            for new in change
                .branches()
                .iter()
                .filter_map(|(_, moved)| moved.after())
            {
                if !brought.contains(new) || reached.contains(new) {
                    continue;
                }
                if !authors.contains(change.actor()) {
                    authors.push(change.actor().clone());
                }
                reached.insert(*new);
                // A commit that the store cannot read is only itself reached: the safe side.
                if let Ok(ancestors) = history::ancestors(store, *new, Parents::All, None) {
                    reached.extend(ancestors.into_iter().map(|(id, _)| id));
                }
            }
        }
        Ok(authors)
    }

    /// Refuses an ID that the store does not hold as a commit: a purge reads every commit that an
    /// entry names, so an ID of a blob or a tree would stop every later purge.
    pub(super) fn ensure_commit(&self, commit: CommitId) -> Result<(), PullError> {
        match self.objects().kind_of(&commit.object_id())? {
            Some(Kind::Commit | Kind::Commit2) => Ok(()),
            Some(Kind::Blob | Kind::Tree) | None => Err(PullError::NoSuchCommit { commit }),
        }
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
pub(super) fn open_pull_of(pulls: &[PullRequest], number: u64) -> Result<&PullRequest, PullError> {
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
    /// The reviewer opened the pull request or wrote one of its commits (ADR-004 rule 4).
    SelfApproval {
        /// The number of the pull request.
        number: u64,
        /// Which of the two it is.
        reason: SelfApproval,
    },
    /// The branch has another head than the one that the reviewer signed. Nothing was written;
    /// review the new head.
    HeadMoved {
        /// The number of the pull request.
        number: u64,
    },
    /// The repository has a register, so an approval needs a signature, and the caller gave no
    /// signer.
    SignatureRequired,
    /// The key of the signature is retired: it keeps its earlier approvals but signs no new one.
    KeyNotActive,
    /// The approval could not be signed.
    Sign(SignError),
    /// The register does not accept the signature of the approval.
    Proof(Box<ProofError>),
    /// The entry of the approval is in the log, and its proof file is not. The approval does not
    /// count; approve again.
    ProofNotWritten {
        /// The number of the entry.
        entry: u64,
        /// What failed.
        source: io::Error,
    },
    /// The history of the branch cannot be read.
    History(HistoryError),
    /// The lock of the references or the references failed. Nothing was written.
    Refs(RefError),
    /// The log could not be read or written. See [`OpLogError`].
    Log(OpLogError),
    /// The lease file of the branch could not be read or is damaged. Nothing was written.
    Lease(LeaseError),
    /// The object store could not be read. Nothing was written.
    Store(StoreError),
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
            Self::SelfApproval { number, reason } => write!(
                f,
                "no approval of your own work: {reason} (pull request {number})"
            ),
            Self::HeadMoved { number } => write!(
                f,
                "the branch of the pull request {number} has a new head since you began; review \
                 the new head and approve again"
            ),
            Self::SignatureRequired => {
                f.write_str("the repository has an actor register, so an approval must be signed")
            }
            Self::KeyNotActive => {
                f.write_str("the key is retired and signs no new approval; sign with an active key")
            }
            Self::Sign(error) => write!(f, "cannot sign the approval: {error}"),
            Self::Proof(error) => write!(f, "the register does not accept the approval: {error}"),
            Self::ProofNotWritten { entry, source } => write!(
                f,
                "entry {entry} is in the log, but its proof is not written ({source}); the \
                 approval does not count, so approve again"
            ),
            Self::History(error) => error.fmt(f),
            Self::Refs(error) => error.fmt(f),
            Self::Log(error) => error.fmt(f),
            Self::Lease(error) => error.fmt(f),
            Self::Store(error) => error.fmt(f),
            Self::Entry(error) => error.fmt(f),
        }
    }
}

impl Error for PullError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sign(error) => Some(error),
            Self::Proof(error) => Some(error.as_ref()),
            Self::ProofNotWritten { source, .. } => Some(source),
            Self::History(error) => Some(error),
            Self::Refs(error) => Some(error),
            Self::Log(error) => Some(error),
            Self::Lease(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Entry(error) => Some(error),
            _ => None,
        }
    }
}

impl From<HistoryError> for PullError {
    fn from(error: HistoryError) -> Self {
        Self::History(error)
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

impl From<LeaseError> for PullError {
    fn from(error: LeaseError) -> Self {
        Self::Lease(error)
    }
}

impl From<StoreError> for PullError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
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
    use crate::allowed::Allowed;
    use crate::object::Blob;
    use crate::oplog::PullKind;
    use crate::pull::PullState;
    use crate::repository::InitOptions;
    use crate::signature::PublicKey;
    use crate::store::ObjectStore;
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
        assert_eq!(format(&dir), "stemma-repository 11\n");
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
    fn an_expired_lease_is_no_write_owner_and_a_damaged_one_is_reported() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let register = register();
        let leases = dir.path().join(".stemma/leases");
        std::fs::create_dir_all(&leases).unwrap();
        let path = leases.join(lease::file_name(&feature()));
        let expired = lease::Lease::new(feature(), mia(), 100, 200).unwrap();
        std::fs::write(&path, expired.encode()).unwrap();
        let before = repository.oplog().entries().unwrap().len();
        let open =
            || repository.open_pull(&feature(), &mia(), vec![], None, Some(&register), &AllowAll);
        assert!(matches!(open(), Err(PullError::NotWriteOwner { .. })));
        std::fs::write(&path, b"not a lease").unwrap();
        let error = open().unwrap_err();
        assert!(matches!(error, PullError::Lease(_)), "{error:?}");
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
    }

    #[test]
    fn an_id_that_is_no_commit_is_refused_for_repairs_and_for_a_merge() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let blob = repository
            .objects()
            .put_blob(&Blob::new("some text"))
            .unwrap();
        let id = CommitId::new(blob.object_id());
        let error = repository
            .open_pull(&feature(), &daniel(), vec![], Some(id), None, &AllowAll)
            .unwrap_err();
        assert!(matches!(error, PullError::NoSuchCommit { .. }), "{error:?}");
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        let error = repository
            .record_pull_merge(number, &daniel(), id, &AllowAll)
            .unwrap_err();
        assert!(matches!(error, PullError::NoSuchCommit { .. }), "{error:?}");
    }

    #[test]
    fn closing_and_merging_ask_the_authorizer_and_open_mode_lets_only_the_opener_close() {
        let dir = TempDir::new();
        let (repository, start) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        let before = repository.oplog().entries().unwrap().len();
        assert!(matches!(
            repository.close_pull(number, &daniel(), None, &DenyAll),
            Err(PullError::Denied { .. })
        ));
        assert!(matches!(
            repository.record_pull_merge(number, &daniel(), start, &DenyAll),
            Err(PullError::Denied { .. })
        ));
        assert!(matches!(
            repository.close_pull(number, &mia(), None, &AllowAll),
            Err(PullError::NotAllowedToClose { .. })
        ));
        assert!(matches!(
            repository.record_pull_merge(number + 9, &daniel(), start, &AllowAll),
            Err(PullError::NoSuchPull { .. })
        ));
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
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

    // ----- approvals (ADR-004 rule 4) -----

    struct Seeded(PrivateKey);

    impl Seeded {
        fn new(seed: u8) -> Self {
            Self(PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32])))
        }
    }

    impl Signer for Seeded {
        fn sign(&self, message: &[u8]) -> Result<crate::signature::Signature, SignError> {
            let pem = self
                .0
                .sign(signature::NAMESPACE, ssh_key::HashAlg::Sha512, message)
                .unwrap()
                .to_pem(ssh_key::LineEnding::LF)
                .unwrap();
            Ok(crate::signature::Signature::from_armored(&pem).unwrap())
        }
    }

    fn ana() -> Actor {
        Actor::human("ana").unwrap()
    }

    /// Daniel owns the register, Mia and Ana are members; the keys are those of seeds 1, 2, 3.
    fn approvers() -> Register {
        let text = format!(
            "stemma-register 1\nentry human ana member\nkey {} active\n\
             entry human daniel owner\nkey {} active\n\
             entry human mia member\nkey {} active\n",
            key(3),
            key(1),
            key(2)
        );
        Register::parse(text.as_bytes()).unwrap()
    }

    /// Stores a commit by `author` on top of the tip of `feature/x` and moves the branch to it.
    fn push_commit(repository: &Repository, author: &Actor) -> CommitId {
        let tip = repository.refs().branch(&feature()).unwrap().unwrap();
        let tree = repository
            .objects()
            .get_commit(&tip)
            .unwrap()
            .unwrap()
            .tree();
        let commit = crate::object::Commit::try_new(
            tree,
            vec![tip],
            crate::change::sample_change(),
            author.clone(),
            1,
            "more".to_owned(),
        )
        .unwrap();
        let new = repository.objects().put_commit(&commit).unwrap();
        let operation = OperationName::try_new("commit").unwrap();
        repository
            .update_branch(&operation, &feature(), Some(tip), new, author, &AllowAll)
            .unwrap();
        new
    }

    fn proof_files(dir: &TempDir) -> usize {
        std::fs::read_dir(dir.path().join(".stemma/proofs"))
            .map(|read| read.count())
            .unwrap_or(0)
    }

    #[test]
    fn an_approval_in_open_mode_names_the_head_and_counts_once_per_identity() {
        let dir = TempDir::new();
        let (repository, start) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        let entry = repository
            .approve_pull(number, &mia(), None, None, &AllowAll)
            .unwrap();
        let approve = entry.pull().unwrap();
        assert_eq!(approve.kind(), PullKind::Approve);
        assert_eq!(
            (approve.pull(), approve.commit()),
            (Some(number), Some(&start))
        );
        // The same identity again, in another session, is no second approval.
        repository
            .approve_pull(number, &mia(), None, None, &AllowAll)
            .unwrap();
        repository
            .approve_pull(number, &ana(), None, None, &AllowAll)
            .unwrap();
        let recount = repository.recount_approvals(number, None).unwrap();
        let counted: Vec<&Actor> = recount.counted().iter().map(Approval::reviewer).collect();
        assert_eq!(counted, [&mia(), &ana()]);
        assert_eq!(recount.void().len(), 1);
        assert_eq!(recount.void()[0].1, pull::NotCounted::Repeat);
        // No proof files in open mode, and the pull request stays open.
        assert_eq!(proof_files(&dir), 0);
        assert!(repository.pull_requests().unwrap()[0].is_open());
    }

    #[test]
    fn nobody_approves_what_they_opened_or_wrote_and_nothing_is_written() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        // Daniel opened it, and wrote the commit that the branch starts from.
        let before = repository.oplog().entries().unwrap().len();
        let error = repository
            .approve_pull(number, &daniel(), None, None, &AllowAll)
            .unwrap_err();
        assert!(
            matches!(
                error,
                PullError::SelfApproval {
                    reason: SelfApproval::Opener,
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        // Zoe writes the next commit, so she may not approve it either.
        let zoe = Actor::human("zoe").unwrap();
        push_commit(&repository, &zoe);
        let before = repository.oplog().entries().unwrap().len();
        let error = repository
            .approve_pull(number, &zoe, None, None, &AllowAll)
            .unwrap_err();
        assert!(
            matches!(
                error,
                PullError::SelfApproval {
                    reason: SelfApproval::Author,
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        // An approval that was written by hand does not count either: the recount asks again.
        let head = repository.refs().branch(&feature()).unwrap().unwrap();
        let forged = PullEntry::approve(5, zoe.clone(), number, head).unwrap();
        let lock = repository
            .refs_store()
            .lock_writer(&Allowed::granted())
            .unwrap();
        repository.append_pull(&lock, &forged).unwrap();
        drop(lock);
        let recount = repository.recount_approvals(number, None).unwrap();
        assert_eq!(recount.count(), 0);
        assert_eq!(
            recount.void()[0].1,
            pull::NotCounted::SelfApproval(SelfApproval::Author)
        );
    }

    /// STEMMA-255: the author of a commit is a claim of its writer. `update_branch` asks the
    /// authorizer about the actor and never compares the actor with the author, so a writer who
    /// puts a foreign name into the field must still count as a writer of the change, in the
    /// approval and in the recount.
    #[test]
    fn a_commit_under_a_foreign_author_name_does_not_let_its_writer_approve() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        // Mia moves the branch to a commit that names Ana as its author.
        let tip = repository.refs().branch(&feature()).unwrap().unwrap();
        let tree = repository
            .objects()
            .get_commit(&tip)
            .unwrap()
            .unwrap()
            .tree();
        let commit = crate::object::Commit::try_new(
            tree,
            vec![tip],
            crate::change::sample_change(),
            ana(),
            1,
            "more".to_owned(),
        )
        .unwrap();
        let new = repository.objects().put_commit(&commit).unwrap();
        let operation = OperationName::try_new("commit").unwrap();
        repository
            .update_branch(&operation, &feature(), Some(tip), new, &mia(), &AllowAll)
            .unwrap();
        // Mia put the change on the branch and must not approve it.
        let before = repository.oplog().entries().unwrap().len();
        let error = repository
            .approve_pull(number, &mia(), None, None, &AllowAll)
            .unwrap_err();
        assert!(
            matches!(
                error,
                PullError::SelfApproval {
                    reason: SelfApproval::Author,
                    ..
                }
            ),
            "mia approved her own change: {error:?}"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        // An approval of hers that was written by hand does not count: the recount asks again.
        let forged = PullEntry::approve(5, mia(), number, new).unwrap();
        let lock = repository
            .refs_store()
            .lock_writer(&Allowed::granted())
            .unwrap();
        repository.append_pull(&lock, &forged).unwrap();
        drop(lock);
        let recount = repository.recount_approvals(number, None).unwrap();
        assert_eq!(recount.count(), 0);
        assert_eq!(
            recount.void()[0].1,
            pull::NotCounted::SelfApproval(SelfApproval::Author)
        );
    }

    /// Mia makes the commit on a branch of her own, and the owner of the branch of the pull request
    /// moves it there. The first entry that brought the commit is hers, so she is a writer.
    #[test]
    fn the_writer_who_made_the_commit_on_another_branch_may_not_approve_either() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        let tip = repository.refs().branch(&feature()).unwrap().unwrap();
        let tree = repository
            .objects()
            .get_commit(&tip)
            .unwrap()
            .unwrap()
            .tree();
        let commit = crate::object::Commit::try_new(
            tree,
            vec![tip],
            crate::change::sample_change(),
            ana(),
            1,
            "more".to_owned(),
        )
        .unwrap();
        let new = repository.objects().put_commit(&commit).unwrap();
        let operation = OperationName::try_new("commit").unwrap();
        let scratch = BranchName::try_new("scratch").unwrap();
        repository
            .update_branch(&operation, &scratch, None, new, &mia(), &AllowAll)
            .unwrap();
        repository
            .update_branch(&operation, &feature(), Some(tip), new, &daniel(), &AllowAll)
            .unwrap();
        let error = repository
            .approve_pull(number, &mia(), None, None, &AllowAll)
            .unwrap_err();
        assert!(
            matches!(
                error,
                PullError::SelfApproval {
                    reason: SelfApproval::Author,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    /// A reviewer who only makes another branch at the tip, to look at it, wrote nothing.
    #[test]
    fn a_reviewer_who_makes_another_branch_at_the_tip_may_approve() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        let zoe = Actor::human("zoe").unwrap();
        push_commit(&repository, &zoe);
        let tip = repository.refs().branch(&feature()).unwrap().unwrap();
        let look = BranchName::try_new("look").unwrap();
        let operation = OperationName::try_new("branch").unwrap();
        repository
            .update_branch(&operation, &look, None, tip, &mia(), &AllowAll)
            .unwrap();
        repository
            .approve_pull(number, &mia(), None, None, &AllowAll)
            .unwrap();
    }

    #[test]
    fn a_new_commit_voids_the_approvals_of_the_old_head() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        repository
            .approve_pull(number, &mia(), None, None, &AllowAll)
            .unwrap();
        assert_eq!(
            repository.recount_approvals(number, None).unwrap().count(),
            1
        );
        let head = push_commit(&repository, &daniel());
        let recount = repository.recount_approvals(number, None).unwrap();
        assert_eq!(recount.count(), 0);
        assert_eq!(recount.void()[0].1, pull::NotCounted::OtherHead);
        // A review of the new head counts again, and the old one stays void.
        let entry = repository
            .approve_pull(number, &mia(), None, None, &AllowAll)
            .unwrap();
        assert_eq!(entry.pull().unwrap().commit(), Some(&head));
        assert_eq!(
            repository.recount_approvals(number, None).unwrap().count(),
            1
        );
    }

    #[test]
    fn only_an_open_pull_request_is_recounted() {
        let dir = TempDir::new();
        let (repository, start) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        repository
            .record_pull_merge(number, &daniel(), start, &AllowAll)
            .unwrap();
        assert!(matches!(
            repository.recount_approvals(number, None),
            Err(PullError::NotOpen { .. })
        ));
        assert!(matches!(
            repository.recount_approvals(number + 9, None),
            Err(PullError::NoSuchPull { .. })
        ));
    }

    #[test]
    fn only_an_open_pull_request_is_approved() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        assert!(matches!(
            repository.approve_pull(number + 9, &mia(), None, None, &AllowAll),
            Err(PullError::NoSuchPull { .. })
        ));
        assert!(matches!(
            repository.approve_pull(number, &mia(), None, None, &DenyAll),
            Err(PullError::Denied { .. })
        ));
        repository
            .close_pull(number, &daniel(), None, &AllowAll)
            .unwrap();
        assert!(matches!(
            repository.approve_pull(number, &mia(), None, None, &AllowAll),
            Err(PullError::NotOpen { .. })
        ));
    }

    #[test]
    fn a_register_needs_a_signature_that_it_accepts_and_the_proof_is_a_file() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let register = approvers();
        repository
            .claim_lease(&feature(), &daniel(), 60, &AllowAll)
            .unwrap();
        let number = repository
            .open_pull(
                &feature(),
                &daniel(),
                vec![],
                None,
                Some(&register),
                &AllowAll,
            )
            .unwrap()
            .number();
        let before = repository.oplog().entries().unwrap().len();
        let approve = |actor: &Actor, signer: Option<&dyn Signer>| {
            repository.approve_pull(number, actor, Some(&register), signer, &AllowAll)
        };
        assert!(matches!(
            approve(&mia(), None),
            Err(PullError::SignatureRequired)
        ));
        // The key of Ana does not speak for Mia.
        assert!(matches!(
            approve(&mia(), Some(&Seeded::new(3))),
            Err(PullError::Proof(_))
        ));
        // Nobody who is not in the register approves.
        let zoe = Actor::human("zoe").unwrap();
        assert!(matches!(
            approve(&zoe, Some(&Seeded::new(9))),
            Err(PullError::Proof(_))
        ));
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        let entry = approve(&mia(), Some(&Seeded::new(2))).unwrap();
        assert_eq!(proof_files(&dir), 1);
        let recount = repository
            .recount_approvals(number, Some(&register))
            .unwrap();
        assert_eq!(recount.count(), 1);
        // The log and its proofs hold together against the register.
        let entries = repository.oplog().verify().unwrap();
        assert_eq!(
            register::check_proofs(&repository.metadata_dir(), &register, &entries).unwrap(),
            1
        );
        assert_eq!(entry.pull().unwrap().kind(), PullKind::Approve);
    }

    #[test]
    fn an_approval_without_its_proof_or_with_a_proof_for_another_head_does_not_count() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let register = approvers();
        repository
            .claim_lease(&feature(), &daniel(), 60, &AllowAll)
            .unwrap();
        let number = repository
            .open_pull(
                &feature(),
                &daniel(),
                vec![],
                None,
                Some(&register),
                &AllowAll,
            )
            .unwrap()
            .number();
        let entry = repository
            .approve_pull(
                number,
                &mia(),
                Some(&register),
                Some(&Seeded::new(2)),
                &AllowAll,
            )
            .unwrap();
        let path = register::proof_path(&repository.metadata_dir(), entry.number());
        let proof = std::fs::read_to_string(&path).unwrap();
        // A crash between the entry and the proof leaves the entry alone.
        std::fs::remove_file(&path).unwrap();
        let recount = repository
            .recount_approvals(number, Some(&register))
            .unwrap();
        assert_eq!(recount.count(), 0);
        assert!(
            matches!(&recount.void()[0].1, pull::NotCounted::Proof(why) if why.contains("no proof"))
        );
        // The proof of an approval of another head does not carry over: Mia signs another head,
        // and the file is put under the entry of her approval.
        std::fs::write(&path, &proof).unwrap();
        assert_eq!(
            repository
                .recount_approvals(number, Some(&register))
                .unwrap()
                .count(),
            1
        );
        let other = Seeded::new(2)
            .sign(&signature::signed_bytes(
                PayloadKind::Approval,
                signature::FORMAT_VERSION,
                &pull::approval_payload(number, &CommitId::new(Blob::new("x").id().object_id())),
            ))
            .unwrap();
        let swapped = ProofFile {
            actor: mia(),
            signature: other,
        };
        std::fs::write(&path, swapped.encode()).unwrap();
        assert_eq!(
            repository
                .recount_approvals(number, Some(&register))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn a_revoked_key_voids_the_approval_and_a_retired_one_keeps_it() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let mut register = approvers();
        repository
            .claim_lease(&feature(), &daniel(), 60, &AllowAll)
            .unwrap();
        let number = repository
            .open_pull(
                &feature(),
                &daniel(),
                vec![],
                None,
                Some(&register),
                &AllowAll,
            )
            .unwrap()
            .number();
        for (actor, seed) in [(mia(), 2), (ana(), 3)] {
            repository
                .approve_pull(
                    number,
                    &actor,
                    Some(&register),
                    Some(&Seeded::new(seed)),
                    &AllowAll,
                )
                .unwrap();
        }
        let mia_key = key(2);
        let ana_key = key(3);
        register
            .revoke(&Identity::of(&mia()), &mia_key, 100)
            .unwrap();
        register
            .retire(&Identity::of(&ana()), &ana_key, 100)
            .unwrap();
        let recount = repository
            .recount_approvals(number, Some(&register))
            .unwrap();
        let counted: Vec<&Actor> = recount.counted().iter().map(Approval::reviewer).collect();
        assert_eq!(counted, [&ana()]);
        // `op verify` does not call a void approval damage.
        let entries = repository.oplog().verify().unwrap();
        register::check_proofs(&repository.metadata_dir(), &register, &entries).unwrap();
    }

    /// Stores a commit with the tree of its first parent and moves `branch` from `old` to it.
    fn put_commit(
        repository: &Repository,
        branch: &BranchName,
        old: CommitId,
        parents: Vec<CommitId>,
        author: &Actor,
    ) -> CommitId {
        let tree = repository
            .objects()
            .get_commit(&parents[0])
            .unwrap()
            .unwrap()
            .tree();
        let commit = crate::object::Commit::try_new(
            tree,
            parents,
            crate::change::sample_change(),
            author.clone(),
            1,
            "more".to_owned(),
        )
        .unwrap();
        let new = repository.objects().put_commit(&commit).unwrap();
        let operation = OperationName::try_new("commit").unwrap();
        repository
            .update_branch(&operation, branch, Some(old), new, author, &AllowAll)
            .unwrap();
        new
    }

    #[test]
    fn an_author_of_main_may_approve_and_an_author_of_the_branch_may_not() {
        let dir = TempDir::new();
        let (repository, start) = repository(&dir);
        let main = BranchName::try_new(TARGET).unwrap();
        let zoe = Actor::human("zoe").unwrap();
        repository
            .create_branch(
                &OperationName::try_new("branch").unwrap(),
                &main,
                start.object_id(),
                &daniel(),
                &AllowAll,
            )
            .unwrap();
        // Ana wrote a commit that `main` takes over: it is no commit of the pull request.
        let by_ana = push_commit(&repository, &ana());
        repository
            .update_branch(
                &OperationName::try_new("commit").unwrap(),
                &main,
                Some(start),
                by_ana,
                &ana(),
                &AllowAll,
            )
            .unwrap();
        // Zoe writes the commit that only the branch has.
        push_commit(&repository, &zoe);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        repository
            .approve_pull(number, &ana(), None, None, &AllowAll)
            .unwrap();
        let error = repository
            .approve_pull(number, &zoe, None, None, &AllowAll)
            .unwrap_err();
        assert!(
            matches!(
                error,
                PullError::SelfApproval {
                    reason: SelfApproval::Author,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn an_author_on_the_second_parent_of_a_merge_may_not_approve() {
        let dir = TempDir::new();
        let (repository, start) = repository(&dir);
        let zoe = Actor::human("zoe").unwrap();
        // Zoe wrote a side commit, and Daniel merged it into the branch.
        let side_branch = BranchName::try_new("side").unwrap();
        repository
            .create_branch(
                &OperationName::try_new("branch").unwrap(),
                &side_branch,
                start.object_id(),
                &zoe,
                &AllowAll,
            )
            .unwrap();
        let side = put_commit(&repository, &side_branch, start, vec![start], &zoe);
        put_commit(&repository, &feature(), start, vec![start, side], &daniel());
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        let error = repository
            .approve_pull(number, &zoe, None, None, &AllowAll)
            .unwrap_err();
        assert!(
            matches!(
                error,
                PullError::SelfApproval {
                    reason: SelfApproval::Author,
                    ..
                }
            ),
            "{error:?}"
        );
        repository
            .approve_pull(number, &ana(), None, None, &AllowAll)
            .unwrap();
    }

    #[test]
    fn two_sessions_of_one_agent_are_one_approval() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, &AllowAll)
            .unwrap()
            .number();
        for session in ["s1", "s2"] {
            let agent = Actor::agent("claude", "opus-5-5", session, "mia").unwrap();
            repository
                .approve_pull(number, &agent, None, None, &AllowAll)
                .unwrap();
        }
        let recount = repository.recount_approvals(number, None).unwrap();
        assert_eq!(recount.count(), 1);
        assert_eq!(recount.void()[0].1, pull::NotCounted::Repeat);
    }

    #[test]
    fn a_head_that_moves_while_signing_is_refused_and_nothing_is_written() {
        struct Moving<'a>(&'a Repository);

        impl Signer for Moving<'_> {
            fn sign(&self, message: &[u8]) -> Result<crate::signature::Signature, SignError> {
                push_commit(self.0, &Actor::human("zoe").unwrap());
                Seeded::new(2).sign(message)
            }
        }

        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let register = approvers();
        repository
            .claim_lease(&feature(), &daniel(), 60, &AllowAll)
            .unwrap();
        let number = repository
            .open_pull(
                &feature(),
                &daniel(),
                vec![],
                None,
                Some(&register),
                &AllowAll,
            )
            .unwrap()
            .number();
        let before = repository.oplog().entries().unwrap().len();
        let error = repository
            .approve_pull(
                number,
                &mia(),
                Some(&register),
                Some(&Moving(&repository)),
                &AllowAll,
            )
            .unwrap_err();
        assert!(matches!(error, PullError::HeadMoved { .. }), "{error:?}");
        // The only new entry is the commit that moved the head.
        assert_eq!(repository.oplog().entries().unwrap().len(), before + 1);
        assert!(pull::approvals(&repository.oplog().entries().unwrap()).is_empty());
        assert_eq!(proof_files(&dir), 0);
        // A new try on the new head counts.
        repository
            .approve_pull(
                number,
                &mia(),
                Some(&register),
                Some(&Seeded::new(2)),
                &AllowAll,
            )
            .unwrap();
        assert_eq!(
            repository
                .recount_approvals(number, Some(&register))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn a_key_retired_before_the_entry_voids_it_in_the_recount() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let mut register = approvers();
        repository
            .claim_lease(&feature(), &daniel(), 60, &AllowAll)
            .unwrap();
        let number = repository
            .open_pull(
                &feature(),
                &daniel(),
                vec![],
                None,
                Some(&register),
                &AllowAll,
            )
            .unwrap()
            .number();
        let entry = repository
            .approve_pull(
                number,
                &ana(),
                Some(&register),
                Some(&Seeded::new(3)),
                &AllowAll,
            )
            .unwrap();
        // Retired from the entry on: this approval is not one of before the retirement.
        register
            .retire(&Identity::of(&ana()), &key(3), entry.number())
            .unwrap();
        let recount = repository
            .recount_approvals(number, Some(&register))
            .unwrap();
        assert_eq!(recount.count(), 0);
        assert!(matches!(recount.void()[0].1, pull::NotCounted::Proof(_)));
        // Retired one entry later, the approval was made before and counts.
        let mut register = approvers();
        register
            .retire(&Identity::of(&ana()), &key(3), entry.number() + 1)
            .unwrap();
        assert_eq!(
            repository
                .recount_approvals(number, Some(&register))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn a_forged_proof_of_a_revoked_key_is_damage_of_the_log() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let mut register = approvers();
        repository
            .claim_lease(&feature(), &daniel(), 60, &AllowAll)
            .unwrap();
        let number = repository
            .open_pull(
                &feature(),
                &daniel(),
                vec![],
                None,
                Some(&register),
                &AllowAll,
            )
            .unwrap()
            .number();
        let entry = repository
            .approve_pull(
                number,
                &mia(),
                Some(&register),
                Some(&Seeded::new(2)),
                &AllowAll,
            )
            .unwrap();
        // The proof now holds a signature of the key of Mia over something else.
        let forged = signature::sign(&Seeded::new(2), PayloadKind::Approval, b"other").unwrap();
        let path = register::proof_path(&repository.metadata_dir(), entry.number());
        let proof = ProofFile {
            actor: mia(),
            signature: forged,
        };
        std::fs::write(path, proof.encode()).unwrap();
        register
            .revoke(&Identity::of(&mia()), &key(2), 100)
            .unwrap();
        let entries = repository.oplog().verify().unwrap();
        let error =
            register::check_proofs(&repository.metadata_dir(), &register, &entries).unwrap_err();
        assert!(
            matches!(
                error,
                register::CheckError::Proof {
                    error: ProofError::Signature(_),
                    ..
                }
            ),
            "{error:?}"
        );
    }
}
