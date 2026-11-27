//! The operations on a pull request (ADR-004, ADR-025, STEMMA-238, STEMMA-290).
//!
//! An opening, a closing, and an approval are signed statements
//! ([`crate::protocol::statement`]), and the log records the act: every operation makes its
//! statement, takes the lock of the references, reads the evidence under it, checks the rule,
//! puts the statement into the set, and writes the entry that names its hash. The lock makes
//! the check and the entry one step: two actors never open a pull request for the same branch,
//! and nobody closes one that another merges (ADR-008). The operations move no pointer; the
//! merge gate that moves `main` records its merge with [`Repository::record_pull_merge`]
//! (ADR-019), which makes no statement, as "merged" is none.
//!
//! In an enforcing repository the statement is signed with a key of the actor, before the lock
//! is taken, because a key with a passphrase may ask a person, and the register is asked for
//! the signature before anything is written. In the open mode there is no key, and the
//! statement is unsigned. The statement comes before its entry: a stop in between leaves a
//! statement that no entry names, which counts for nothing, and the same call can be made
//! again. A statement made here names no repository of a hub and counts locally only.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use super::{now, Repository};
use crate::allowed::{self_approval, SelfApproval};
use crate::history::{self, HistoryError, Parents};
use crate::lease;
use crate::object::{CommitId, Kind};
use crate::oplog::{
    Entry, EntryError, OpLogError, OperationName, PullEntry, PullKind, PR_APPROVE, PR_CLOSE,
    PR_MERGE, PR_OPEN,
};
use crate::protocol::record::SealError;
use crate::protocol::statement::{self, Body, Closing, Opening, Statement, StatementProblem};
use crate::pull::{self, Approval, PullRequest, Recount, TARGET};
use crate::refs::{BranchName, RefError};
use crate::register::{Identity, ProofError, Register};
use crate::repository::statements::{proof_of, EvidenceError};
use crate::repository::LeaseError;
use crate::signature::Signer;
use crate::statements::SetError;
use crate::store::StoreError;
use crate::{Actor, Authorizer};

impl Repository {
    /// The pull requests that the statements tell of, in the order of their openings (see
    /// [`evidence::fold`](crate::evidence::fold)).
    ///
    /// `register` is the register of an enforcing repository: an opening makes a pull request
    /// there only with a signature that the register accepts for its opener, so one that was
    /// written by hand without it, or opened while the repository was in the open mode, is not
    /// among them. `None` is the open mode, where the opener is a claim and every opening that
    /// the log places counts.
    ///
    /// A pull request that was opened under a repository format before 16 is not among them:
    /// its entries name no statement, so they are no evidence (ADR-011). Open it again.
    ///
    /// # Errors
    ///
    /// Returns [`PullError::Log`] if the log cannot be read and [`PullError::Statements`] if a
    /// statement cannot be read for a failure of the file system.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn pull_requests(
        &self,
        register: Option<&Register>,
    ) -> Result<Vec<PullRequest>, PullError> {
        self.require_oplog("read the pull requests")?;
        Ok(self.evidence(register)?.pulls)
    }

    /// The write owner of `branch` opens a pull request that would merge it into `main`.
    ///
    /// The rules. Those that depend on nothing that moves (the branch is not `main`, the shape
    /// of the reviewers) are checked before the statement is signed, the others under the lock
    /// of the references:
    ///
    /// - the branch exists and is not `main`;
    /// - only the write owner opens: where the branch has a lease ([`lease::applies`]), `actor`
    ///   holds a valid one; in open mode there is no lease and no other writer;
    /// - at most one pull request is open for a branch;
    /// - the same opening is not accepted twice (ADR-025): a pull request that ended is final,
    ///   and the next one of the branch needs a new head, other reviewers, or another repair;
    /// - the opener names the reviewers, who are identities of the register, not the opener, and
    ///   at most [`MAX_REVIEWERS`](crate::oplog::MAX_REVIEWERS); in open mode there is no
    ///   register, so there are none;
    /// - `repairs`, if given, is a commit of the store (ADR-019).
    ///
    /// The opening is a signed statement ([`Opening`]). Where the repository has a `register`,
    /// `signer` signs it with a key that the register holds for `actor` as an active key, and
    /// the signature is checked before anything is written; in open mode there is no register,
    /// and the statement is unsigned. The statement is made with the head that the branch has
    /// when the call begins, before the lock is taken, and the head is read again under the
    /// lock.
    ///
    /// The hash of the opening names the pull request ([`PullRequest::id`]). Its number in this
    /// clone is the number of the entry that is returned.
    ///
    /// # Errors
    ///
    /// Returns [`PullError::Denied`] if the authorizer refuses, [`PullError::TargetBranch`],
    /// [`PullError::NoSuchBranch`], [`PullError::NotWriteOwner`], [`PullError::AlreadyOpen`],
    /// [`PullError::SameOpening`], [`PullError::UnknownReviewer`], [`PullError::NoSuchCommit`],
    /// and [`PullError::Entry`] for a broken rule, [`PullError::SignatureRequired`] for a
    /// register and no signer, [`PullError::Sign`], [`PullError::Proof`], and
    /// [`PullError::KeyNotActive`] for a signature that cannot be made or that the register
    /// does not accept, and [`PullError::BranchMoved`] if the branch moved while the statement
    /// was signed. Nothing is written for any of them. [`PullError::Refs`], [`PullError::Log`],
    /// and [`PullError::Statements`] are a failure of the lock, the log, or the set of
    /// statements; a failure of the log can leave the statement without its entry.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    #[allow(clippy::too_many_arguments)]
    pub fn open_pull(
        &self,
        branch: &BranchName,
        actor: &Actor,
        reviewers: Vec<Actor>,
        repairs: Option<CommitId>,
        register: Option<&Register>,
        signer: Option<&dyn Signer>,
        authorizer: &dyn Authorizer,
    ) -> Result<Entry, PullError> {
        self.require_oplog("open a pull request")?;
        let allowed = self.allowed_for_pull(PR_OPEN, branch.as_str(), actor, authorizer)?;
        if branch.as_str() == TARGET {
            return Err(PullError::TargetBranch);
        }
        let signed_head = self.branch_head(branch)?;
        let identities = reviewers.iter().map(Identity::of).collect();
        let opening = Opening::try_new(None, branch.clone(), signed_head, identities, repairs)
            .map_err(|problem| match problem {
                StatementProblem::TargetBranch => PullError::TargetBranch,
                _ => PullError::Entry(EntryError::BadReviewers),
            })?;
        // The entry holds the rules of the reviewers that need the opener, so it is built
        // before the statement is signed: a refusal comes before the key is asked.
        let entry = PullEntry::open(
            now(),
            actor.clone(),
            branch.clone(),
            signed_head,
            reviewers.clone(),
            repairs,
        )?;
        let statement = make_statement(Body::Opening(opening), actor, register, signer)?;
        let lock = self.refs_store().lock_writer(&allowed)?;
        if self.branch_head(branch)? != signed_head {
            return Err(PullError::BranchMoved {
                branch: branch.clone(),
            });
        }
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
        let pulls = self.evidence(register)?.pulls;
        if let Some(open) = pull::open_for(&pulls, branch) {
            return Err(PullError::AlreadyOpen {
                branch: branch.clone(),
                number: open.number(),
            });
        }
        // The log is asked, not only the fold: an entry whose statement was lost names the
        // opening still, and putting the statement back would bring its pull request back,
        // ended or not, while the new entry would name nothing.
        let id = statement.hash();
        let entries = self.oplog().entries().map_err(PullError::Log)?;
        let same = entries.iter().find(|entry| {
            entry.statement() == Some(id)
                && entry
                    .pull()
                    .is_some_and(|pull| pull.kind() == PullKind::Open)
        });
        if let Some(same) = same {
            return Err(PullError::SameOpening {
                number: same.number(),
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
        let hash = self.statement_set().put(&statement)?;
        Ok(self.append_pull(&lock, &entry.with_statement(hash)?)?)
    }

    /// Ends the open pull request `number` without a merge. Only the opener and, where the
    /// repository has a register, an owner may.
    ///
    /// The closing is a signed statement ([`Closing`]) that names the pull request by the hash
    /// of its opening, signed and checked as the opening is (see
    /// [`open_pull`](Self::open_pull)).
    ///
    /// # Errors
    ///
    /// Returns [`PullError::Denied`], [`PullError::NoSuchPull`] for a number that no opening
    /// has, [`PullError::NotOpen`] for a pull request that is closed or merged already,
    /// [`PullError::NotAllowedToClose`] for an actor who may not, the errors of the signature,
    /// and the errors of the log, the lock, and the set of statements (see
    /// [`open_pull`](Self::open_pull)).
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn close_pull(
        &self,
        number: u64,
        actor: &Actor,
        register: Option<&Register>,
        signer: Option<&dyn Signer>,
        authorizer: &dyn Authorizer,
    ) -> Result<Entry, PullError> {
        self.require_oplog("close a pull request")?;
        let allowed =
            self.allowed_for_pull(PR_CLOSE, &format!("pull {number}"), actor, authorizer)?;
        let may_close = |pull: &PullRequest| {
            let is_owner = register.is_some_and(|register| register.is_owner(actor));
            if pull.opener() == actor || is_owner {
                Ok(())
            } else {
                Err(PullError::NotAllowedToClose { number })
            }
        };
        // The name of the pull request does not change, so the statement can be made before
        // the lock; whether it is still open is asked again under it.
        let pulls = self.evidence(register)?.pulls;
        let pull = open_pull_of(&pulls, number)?;
        may_close(pull)?;
        let id = pull.id();
        let closing = Body::Closing(Closing::new(None, id));
        let statement = make_statement(closing, actor, register, signer)?;
        let lock = self.refs_store().lock_writer(&allowed)?;
        let pulls = self.evidence(register)?.pulls;
        may_close(open_pull_of(&pulls, number)?)?;
        let entry = PullEntry::close(now(), actor.clone(), number)?;
        let hash = self.statement_set().put(&statement)?;
        Ok(self.append_pull(&lock, &entry.with_statement(hash)?)?)
    }

    /// Records that the open pull request `number` was merged with the head `commit`. The entry is
    /// the record of the merge, not the merge: it moves no pointer, checks no approval, and runs
    /// no gate. The merge gate calls it after it moved `main` (ADR-019). It makes no statement:
    /// "merged" is a fact of whoever holds the statements (ADR-025), in a clone this entry.
    /// `register` says which openings count, as in [`pull_requests`](Self::pull_requests).
    ///
    /// # Errors
    ///
    /// Returns [`PullError::Denied`], [`PullError::NoSuchPull`], [`PullError::NotOpen`],
    /// [`PullError::NoSuchCommit`] if the store lacks `commit`, and the errors of the log and the
    /// lock (see [`open_pull`](Self::open_pull)).
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn record_pull_merge(
        &self,
        number: u64,
        actor: &Actor,
        commit: CommitId,
        register: Option<&Register>,
        authorizer: &dyn Authorizer,
    ) -> Result<Entry, PullError> {
        self.require_oplog("record the merge of a pull request")?;
        let allowed =
            self.allowed_for_pull(PR_MERGE, &format!("pull {number}"), actor, authorizer)?;
        let lock = self.refs_store().lock_writer(&allowed)?;
        self.record_pull_merge_locked(&lock, number, actor, commit, register)
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
        register: Option<&Register>,
    ) -> Result<Entry, PullError> {
        let pulls = self.evidence(register)?.pulls;
        open_pull_of(&pulls, number)?;
        self.ensure_commit(commit)?;
        let entry = PullEntry::merge(now(), actor.clone(), number, commit)?;
        Ok(self.append_pull(lock, &entry)?)
    }

    /// `actor` approves the head of the open pull request `number`, which is the tip of its
    /// branch now. The approval is a signed statement ([`statement::Approval`]) that names the
    /// pull request by the hash of its opening, and that head; it binds the approval to it, and
    /// any later commit voids it (ADR-004 rule 4). The entry `pr-approve` names the statement.
    ///
    /// The rules, checked under the lock of the references:
    ///
    /// - the pull request is open, and its branch still has the head that `actor` reviewed, which
    ///   is the tip when the call began;
    /// - `actor` is neither the opener, nor the author of a commit that the pull request brings,
    ///   nor the actor of the entry of the log that first brought such a commit under a branch, a
    ///   tag, or the working copy ([`self_approval`]);
    /// - where the repository has a `register`, the statement is signed: `signer` signs it with a
    ///   key that the register holds for `actor` as an active key, and the signature is checked
    ///   before anything is written. A retired key keeps the approvals it made, but signs no new
    ///   one. In open mode there is no register, and the statement is unsigned.
    ///
    /// The signature is made before the lock is taken, because a key with a passphrase may ask a
    /// person, and nobody should wait for the lock meanwhile.
    ///
    /// The signature is part of the statement, so an approval has no proof file. The statement
    /// is put into the set before the entry is written. A stop in between leaves a statement
    /// that no entry names, which the recount does not count: the safe side.
    ///
    /// # Errors
    ///
    /// Returns [`PullError::Denied`], [`PullError::NoSuchPull`], [`PullError::NotOpen`],
    /// [`PullError::NoSuchBranch`], [`PullError::SelfApproval`] for the ban, [`PullError::HeadMoved`]
    /// if the branch moved while the signature was made, [`PullError::SignatureRequired`] for a
    /// register and no signer, [`PullError::Sign`] and [`PullError::Proof`] for a signature that
    /// cannot be made or that the register does not accept, [`PullError::KeyNotActive`] for a
    /// retired key, and the errors of the log, the lock, and the set of statements (see
    /// [`open_pull`](Self::open_pull)). Nothing is written for any error but those of the log.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn approve_pull(
        &self,
        number: u64,
        actor: &Actor,
        register: Option<&Register>,
        signer: Option<&dyn Signer>,
        authorizer: &dyn Authorizer,
    ) -> Result<Entry, PullError> {
        self.require_oplog("approve a pull request")?;
        if register.is_some() && signer.is_none() {
            return Err(PullError::SignatureRequired);
        }
        let (signed_head, statement) = {
            let pulls = self.evidence(register)?.pulls;
            let pull = open_pull_of(&pulls, number)?;
            let head = self.branch_head(pull.branch())?;
            let approval = Body::Approval(statement::Approval::new(None, pull.id(), head));
            (head, make_statement(approval, actor, register, signer)?)
        };
        let allowed =
            self.allowed_for_pull(PR_APPROVE, &format!("pull {number}"), actor, authorizer)?;
        let lock = self.refs_store().lock_writer(&allowed)?;
        let pulls = self.evidence(register)?.pulls;
        let pull = open_pull_of(&pulls, number)?;
        let head = self.branch_head(pull.branch())?;
        if signed_head != head {
            return Err(PullError::HeadMoved { number });
        }
        let authors = self.pull_authors(&head)?;
        if let Some(reason) = self_approval(actor, pull.opener(), &authors) {
            return Err(PullError::SelfApproval { number, reason });
        }
        let entry = PullEntry::approve(now(), actor.clone(), number, head)?;
        let hash = self.statement_set().put(&statement)?;
        Ok(self.append_pull(&lock, &entry.with_statement(hash)?)?)
    }

    /// Which approvals of the pull request `number` count for its current head, as
    /// [`pull::recount`] decides, with the statements and the history as its input. The
    /// head is the tip of the branch, so the pull request must be open. The `register` is the one
    /// of the base commit; without one there is no signature to check.
    ///
    /// The recount trusts no writer: it asks the ban on self-approval again for every approval,
    /// and it checks the signature of every statement against the register, for the reviewer
    /// and the number of the entry that names it
    /// ([`Register::verify_statement`]). An unsigned statement does not count in an enforcing
    /// repository.
    ///
    /// # Errors
    ///
    /// Returns [`PullError::NoSuchPull`], [`PullError::NotOpen`], [`PullError::NoSuchBranch`], and
    /// the errors of reading
    /// the log, the references, and the history. A statement that is missing or damaged is no
    /// error: the approval is not listed, and `op verify` reports it.
    ///
    /// On a bare repository the call is refused before anything else, with the error
    /// [`Bare`](crate::refs::RefError::Bare) of the references; see
    /// [`is_bare`](crate::repository::Repository::is_bare).
    pub fn recount_approvals(
        &self,
        number: u64,
        register: Option<&Register>,
    ) -> Result<Recount, PullError> {
        self.require_oplog("read the pull requests")?;
        let evidence = self.evidence(register)?;
        let pull = evidence
            .pulls
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
        let proof = |approval: &Approval| -> Result<(), String> {
            proof_of(
                register,
                approval.reviewer(),
                approval.entry(),
                approval.statement(),
            )
            .map_err(|error| error.to_string())
        };
        Ok(pull::recount(
            pull,
            &head,
            &authors,
            &evidence.approvals,
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
    /// brought such a commit under a branch, a tag, or the working copy, whichever it was and
    /// directly or through a descendant of the commit. A later move to a commit
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
        // The entries are in order. A commit is reached once a branch, a tag, or the working copy
        // moved to it or to a descendant; the entry that does so first is the one that brought it.
        let mut reached: HashSet<CommitId> = HashSet::new();
        for entry in self.oplog().entries().map_err(PullError::Log)? {
            let Some(change) = entry.change() else {
                continue;
            };
            let moved = change
                .branches()
                .iter()
                .map(|(_, moved)| moved)
                .chain(change.tags().iter().map(|(_, moved)| moved))
                .chain(change.working_copy());
            for new in moved.filter_map(|moved| moved.after()) {
                // The walk goes down from `new` and stops at a commit that was reached before, so
                // every commit is read once over the whole log. A commit that the store does not
                // hold any more (a purge removed it, ADR-014) reaches only itself; any other
                // failure of the walk refuses the approval.
                let mut first = false;
                let mut stack = vec![(*new, None)];
                while let Some((id, child)) = stack.pop() {
                    if reached.contains(&id) {
                        continue;
                    }
                    reached.insert(id);
                    first |= brought.contains(&id);
                    match history::read_commit(store, id, child) {
                        Ok(commit) => {
                            stack.extend(commit.parents().iter().map(|&parent| (parent, Some(id))));
                        }
                        Err(HistoryError::Missing { .. }) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                if first && !authors.contains(change.actor()) {
                    authors.push(change.actor().clone());
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

    /// Asks the authorizer for `operation`. The answer opens the lock of the references.
    fn allowed_for_pull(
        &self,
        operation: &str,
        target: &str,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<crate::Allowed, PullError> {
        let name = OperationName::try_new(operation).expect("a pull request operation is a name");
        self.authorize(actor, authorizer, &name, target)
            .map_err(|_| PullError::Denied {
                operation: operation.to_owned(),
                target: target.to_owned(),
            })
    }
}

/// Makes the statement of `actor` for `body`: signed with `signer` where the repository has a
/// `register`, and unsigned in the open mode. A signed statement is checked against the
/// register before it is returned, so every refusal comes before the first write: the signature
/// is one of a key that the register holds for `actor`, and that key is active.
pub(super) fn make_statement(
    body: Body,
    actor: &Actor,
    register: Option<&Register>,
    signer: Option<&dyn Signer>,
) -> Result<Statement, NotSigned> {
    let Some(register) = register else {
        return Ok(Statement::unsigned(body));
    };
    let signer = signer.ok_or(NotSigned::SignatureRequired)?;
    let statement = Statement::sign(body, signer).map_err(NotSigned::Sign)?;
    let verified = register
        .verify_statement(actor, &statement, None)
        .map_err(|error| NotSigned::Proof(Box::new(error)))?;
    // A retired key keeps the statements it made, but it signs nothing new.
    if !register.is_active_key(&Identity::of(actor), verified.key()) {
        return Err(NotSigned::KeyNotActive);
    }
    Ok(statement)
}

/// Why [`make_statement`] made no statement: the four refusals of a signature, which every
/// writer of a statement reports as its own error.
#[derive(Debug)]
pub(super) enum NotSigned {
    /// The repository has a register, and the caller gave no signer.
    SignatureRequired,
    /// The key of the signature is retired.
    KeyNotActive,
    /// The statement could not be signed, or the signer signed with another key than it named.
    Sign(SealError),
    /// The register does not accept the signature for the actor.
    Proof(Box<ProofError>),
}

impl From<NotSigned> for PullError {
    fn from(error: NotSigned) -> Self {
        match error {
            NotSigned::SignatureRequired => Self::SignatureRequired,
            NotSigned::KeyNotActive => Self::KeyNotActive,
            NotSigned::Sign(error) => Self::Sign(error),
            NotSigned::Proof(error) => Self::Proof(error),
        }
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
    /// The same opening made a pull request before, which has ended. An ended pull request is
    /// final (ADR-025): the next one needs a new head, other reviewers, or another repair.
    SameOpening {
        /// The number of the pull request that the opening made.
        number: u64,
    },
    /// The branch has another head than the one that the opening was made for. Nothing was
    /// written; open again.
    BranchMoved {
        /// The branch.
        branch: BranchName,
    },
    /// The statements have no opening with this number.
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
    /// The repository has a register, so the statement of an opening, a closing, an approval,
    /// or a check needs a signature, and the caller gave no signer.
    SignatureRequired,
    /// The key of the signature is retired: it keeps what it signed before its retirement, but
    /// signs nothing new. A revoked key is refused as [`Proof`](Self::Proof).
    KeyNotActive,
    /// The statement could not be signed, or the signer signed with another key than the one
    /// it named.
    Sign(SealError),
    /// The register does not accept the signature of the statement for the actor.
    Proof(Box<ProofError>),
    /// The set of statements could not be read or written. Nothing was written to the log.
    Statements(SetError),
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
            Self::SameOpening { number } => write!(
                f,
                "this opening made the pull request {number}, which has ended; an ended pull \
                 request is final, so the next one needs a new head, other reviewers, or another \
                 repair"
            ),
            Self::BranchMoved { branch } => write!(
                f,
                "the branch `{branch}` has a new head since you began; open the pull request \
                 again"
            ),
            Self::NoSuchPull { number } => write!(f, "there is no pull request {number}"),
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
                f.write_str("the repository has an actor register, so the statement must be signed")
            }
            Self::KeyNotActive => f.write_str(
                "the key is not active and signs no new statement; sign with an active key",
            ),
            Self::Sign(error) => write!(f, "cannot sign the statement: {error}"),
            Self::Proof(error) => {
                write!(f, "the register does not accept the statement: {error}")
            }
            Self::Statements(error) => error.fmt(f),
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
            Self::Statements(error) => Some(error),
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

impl From<SetError> for PullError {
    fn from(error: SetError) -> Self {
        Self::Statements(error)
    }
}

impl From<EvidenceError> for PullError {
    fn from(error: EvidenceError) -> Self {
        match error {
            EvidenceError::Log(error) => Self::Log(error),
            EvidenceError::Set(error) => Self::Statements(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use ssh_key::private::Ed25519Keypair;
    use ssh_key::PrivateKey;

    use super::*;
    use crate::object::Blob;
    use crate::pull::PullState;
    use crate::register;
    use crate::repository::statements::by_hand;
    use crate::repository::{InitOptions, StatementCheckError};
    use crate::signature::{self, PayloadKind, PublicKey, SignError};
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
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
            .unwrap();
        let pull = entry.pull().unwrap();
        assert_eq!(pull.kind(), PullKind::Open);
        assert_eq!(pull.commit(), Some(&start));
        assert_eq!(format(&dir), "stemma-repository 19\n");
        let pulls = repository.pull_requests(None).unwrap();
        assert_eq!(pulls.len(), 1);
        assert_eq!(pulls[0].number(), entry.number());
        assert!(pulls[0].is_open());
    }

    #[test]
    fn one_open_pull_request_per_branch_and_a_new_one_after_the_end_needs_a_new_head() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let open =
            || repository.open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll);
        let first = open().unwrap();
        let before = repository.oplog().entries().unwrap().len();
        let error = open().unwrap_err();
        assert!(
            matches!(error, PullError::AlreadyOpen { number, .. } if number == first.number()),
            "{error:?}"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        repository
            .close_pull(first.number(), &daniel(), None, None, &AllowAll)
            .unwrap();
        // An ended pull request is final, and the same opening is not accepted twice (ADR-025).
        let before = repository.oplog().entries().unwrap().len();
        let error = open().unwrap_err();
        assert!(
            matches!(error, PullError::SameOpening { number } if number == first.number()),
            "{error:?}"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
        // A new head makes another opening, and so the next pull request of the branch.
        push_commit(&repository, &daniel());
        let second = open().unwrap();
        assert!(second.number() > first.number());
        let pulls = repository.pull_requests(None).unwrap();
        assert_eq!(pulls.len(), 2);
        assert_ne!(pulls[0].id(), pulls[1].id());
        assert_eq!(pulls[0].id(), first.statement().unwrap());
        assert_eq!(pulls[1].id(), second.statement().unwrap());
        assert!(!pulls[0].is_open() && pulls[1].is_open());
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
            repository.open_pull(
                branch,
                &daniel(),
                reviewers,
                repairs,
                None,
                None,
                authorizer,
            )
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
        assert!(repository.pull_requests(None).unwrap().is_empty());
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
                Some(&Seeded::new(1)),
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
                Some(&Seeded::new(2)),
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
                Some(&Seeded::new(2)),
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
                Some(&Seeded::new(2)),
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
            .open_pull(
                &feature(),
                &mia(),
                vec![],
                None,
                Some(&register),
                Some(&Seeded::new(2)),
                &AllowAll,
            )
            .unwrap()
            .number();
        let stranger = Actor::human("zoe").unwrap();
        let error = repository
            .close_pull(number, &stranger, Some(&register), None, &AllowAll)
            .unwrap_err();
        assert!(
            matches!(error, PullError::NotAllowedToClose { .. }),
            "{error:?}"
        );
        let closed = repository
            .close_pull(
                number,
                &daniel(),
                Some(&register),
                Some(&Seeded::new(1)),
                &AllowAll,
            )
            .unwrap();
        assert_eq!(closed.pull().unwrap().pull(), Some(number));
        let pulls = repository.pull_requests(None).unwrap();
        assert!(matches!(pulls[0].state(), PullState::Closed { by, .. } if by == &daniel()));
        // A closed pull request cannot be closed, merged, or found under another number.
        assert!(matches!(
            repository.close_pull(number, &mia(), None, None, &AllowAll),
            Err(PullError::NotOpen { .. })
        ));
        assert!(matches!(
            repository.close_pull(number + 50, &mia(), None, None, &AllowAll),
            Err(PullError::NoSuchPull { .. })
        ));
    }

    #[test]
    fn a_merge_is_recorded_once_and_ends_the_pull_request() {
        let dir = TempDir::new();
        let (repository, start) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
            .unwrap()
            .number();
        let absent = CommitId::new(Blob::new("not stored").id().object_id());
        assert!(matches!(
            repository.record_pull_merge(number, &daniel(), absent, None, &AllowAll),
            Err(PullError::NoSuchCommit { .. })
        ));
        let entry = repository
            .record_pull_merge(number, &daniel(), start, None, &AllowAll)
            .unwrap();
        assert_eq!(entry.pull().unwrap().commit(), Some(&start));
        let pulls = repository.pull_requests(None).unwrap();
        assert!(matches!(
            pulls[0].state(),
            PullState::Merged { commit, .. } if commit == &start
        ));
        for error in [
            repository
                .record_pull_merge(number, &daniel(), start, None, &AllowAll)
                .unwrap_err(),
            repository
                .close_pull(number, &daniel(), None, None, &AllowAll)
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
        let open = || {
            repository.open_pull(
                &feature(),
                &mia(),
                vec![],
                None,
                Some(&register),
                Some(&Seeded::new(2)),
                &AllowAll,
            )
        };
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
            .open_pull(
                &feature(),
                &daniel(),
                vec![],
                Some(id),
                None,
                None,
                &AllowAll,
            )
            .unwrap_err();
        assert!(matches!(error, PullError::NoSuchCommit { .. }), "{error:?}");
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
            .unwrap()
            .number();
        let error = repository
            .record_pull_merge(number, &daniel(), id, None, &AllowAll)
            .unwrap_err();
        assert!(matches!(error, PullError::NoSuchCommit { .. }), "{error:?}");
    }

    #[test]
    fn closing_and_merging_ask_the_authorizer_and_open_mode_lets_only_the_opener_close() {
        let dir = TempDir::new();
        let (repository, start) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
            .unwrap()
            .number();
        let before = repository.oplog().entries().unwrap().len();
        assert!(matches!(
            repository.close_pull(number, &daniel(), None, None, &DenyAll),
            Err(PullError::Denied { .. })
        ));
        assert!(matches!(
            repository.record_pull_merge(number, &daniel(), start, None, &DenyAll),
            Err(PullError::Denied { .. })
        ));
        assert!(matches!(
            repository.close_pull(number, &mia(), None, None, &AllowAll),
            Err(PullError::NotAllowedToClose { .. })
        ));
        assert!(matches!(
            repository.record_pull_merge(number + 9, &daniel(), start, None, &AllowAll),
            Err(PullError::NoSuchPull { .. })
        ));
        assert_eq!(repository.oplog().entries().unwrap().len(), before);
    }

    #[test]
    fn an_entry_of_a_pull_request_cannot_be_restored() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let entry = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
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
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
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
        assert!(repository.pull_requests(None).unwrap()[0].is_open());
    }

    #[test]
    fn nobody_approves_what_they_opened_or_wrote_and_nothing_is_written() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
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
        by_hand::approve(&repository, &zoe, number, head, None);
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
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
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
        by_hand::approve(&repository, &mia(), number, new, None);
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
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
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

    /// Mia moves a branch of her own to a commit above the one that the pull request brings, and
    /// the owner then sets the branch of the pull request back to the lower one: she brought it
    /// under a branch through its descendant.
    #[test]
    fn the_writer_who_moved_a_branch_to_a_descendant_may_not_approve_either() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
            .unwrap()
            .number();
        let tip = repository.refs().branch(&feature()).unwrap().unwrap();
        let tree = repository
            .objects()
            .get_commit(&tip)
            .unwrap()
            .unwrap()
            .tree();
        let make = |parent: CommitId| {
            let commit = crate::object::Commit::try_new(
                tree,
                vec![parent],
                crate::change::sample_change(),
                ana(),
                1,
                "more".to_owned(),
            )
            .unwrap();
            repository.objects().put_commit(&commit).unwrap()
        };
        let lower = make(tip);
        let upper = make(lower);
        let operation = OperationName::try_new("commit").unwrap();
        let scratch = BranchName::try_new("scratch").unwrap();
        repository
            .update_branch(&operation, &scratch, None, upper, &mia(), &AllowAll)
            .unwrap();
        repository
            .update_branch(
                &operation,
                &feature(),
                Some(tip),
                lower,
                &daniel(),
                &AllowAll,
            )
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

    /// Three commits, each put on the branch by another actor: all three are writers, and an
    /// unrelated reviewer still approves.
    #[test]
    fn every_actor_that_brought_a_commit_is_a_writer() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
            .unwrap()
            .number();
        let zoe = Actor::human("zoe").unwrap();
        let max = Actor::human("max").unwrap();
        push_commit(&repository, &zoe);
        push_commit(&repository, &max);
        push_commit(&repository, &mia());
        for writer in [&zoe, &max, &mia()] {
            let error = repository
                .approve_pull(number, writer, None, None, &AllowAll)
                .unwrap_err();
            assert!(matches!(error, PullError::SelfApproval { .. }), "{error:?}");
        }
        repository
            .approve_pull(number, &ana(), None, None, &AllowAll)
            .unwrap();
    }

    /// The owner sets the branch of the pull request to the lower commit first; Mia moves a branch
    /// of her own to the one above it afterwards. She did not bring the lower commit, so she may
    /// approve, and the owner stays barred.
    #[test]
    fn a_later_move_to_a_descendant_does_not_count() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
            .unwrap()
            .number();
        let zoe = Actor::human("zoe").unwrap();
        let lower = push_commit(&repository, &zoe);
        let tree = repository
            .objects()
            .get_commit(&lower)
            .unwrap()
            .unwrap()
            .tree();
        let above = crate::object::Commit::try_new(
            tree,
            vec![lower],
            crate::change::sample_change(),
            zoe.clone(),
            2,
            "above".to_owned(),
        )
        .unwrap();
        let above = repository.objects().put_commit(&above).unwrap();
        let operation = OperationName::try_new("commit").unwrap();
        let scratch = BranchName::try_new("scratch").unwrap();
        repository
            .update_branch(&operation, &scratch, None, above, &mia(), &AllowAll)
            .unwrap();
        repository
            .approve_pull(number, &mia(), None, None, &AllowAll)
            .unwrap();
        let recount = repository.recount_approvals(number, None).unwrap();
        assert_eq!((recount.count(), recount.void().len()), (1, 0));
        assert!(matches!(
            repository
                .approve_pull(number, &daniel(), None, None, &AllowAll)
                .unwrap_err(),
            PullError::SelfApproval { .. }
        ));
    }

    /// Mia reaches the commit only through a tag of hers; the owner then moves the branch of the
    /// pull request to it. The tag is the first entry that brought the commit, so she is a writer.
    #[test]
    fn the_writer_who_tagged_the_commit_first_may_not_approve_either() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
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
        let operation = OperationName::try_new("tag").unwrap();
        let name = BranchName::try_new("looked-at").unwrap();
        repository
            .create_tag(&operation, &name, new.object_id(), &mia(), &AllowAll)
            .unwrap();
        let operation = OperationName::try_new("commit").unwrap();
        repository
            .update_branch(&operation, &feature(), Some(tip), new, &daniel(), &AllowAll)
            .unwrap();
        let error = repository
            .approve_pull(number, &mia(), None, None, &AllowAll)
            .unwrap_err();
        assert!(matches!(error, PullError::SelfApproval { .. }), "{error:?}");
    }

    /// A reviewer who only makes another branch at the tip, to look at it, wrote nothing.
    #[test]
    fn a_reviewer_who_makes_another_branch_at_the_tip_may_approve() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
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
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
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
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
            .unwrap()
            .number();
        repository
            .record_pull_merge(number, &daniel(), start, None, &AllowAll)
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
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
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
            .close_pull(number, &daniel(), None, None, &AllowAll)
            .unwrap();
        assert!(matches!(
            repository.approve_pull(number, &mia(), None, None, &AllowAll),
            Err(PullError::NotOpen { .. })
        ));
    }

    #[test]
    fn a_register_needs_a_signature_that_it_accepts_and_the_statement_carries_it() {
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
                Some(&Seeded::new(1)),
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
        assert_eq!(
            repository.statement_set().hashes().unwrap().len(),
            1,
            "a refused approval leaves no statement"
        );
        let entry = approve(&mia(), Some(&Seeded::new(2))).unwrap();
        // The signature is part of the statement that the entry names: there is no proof file.
        assert_eq!(proof_files(&dir), 0);
        let hash = entry.statement().expect("the entry names its statement");
        let statement = repository.statement_set().get(&hash).unwrap().unwrap();
        assert_eq!(statement.key(), Some(&key(2)));
        let recount = repository
            .recount_approvals(number, Some(&register))
            .unwrap();
        assert_eq!(recount.count(), 1);
        assert_eq!(recount.counted()[0].statement(), &statement);
        // The approval names the pull request by the hash of its opening, not by its number.
        let pulls = repository.pull_requests(None).unwrap();
        assert_eq!(recount.counted()[0].pull(), pulls[0].id());
        // The log, the statements, and the register hold together.
        let entries = repository.oplog().verify().unwrap();
        assert_eq!(
            register::check_proofs(&repository.metadata_dir(), &register, &entries).unwrap(),
            0
        );
        let verified = repository
            .verify_statements(&entries, Some(&register))
            .unwrap();
        assert_eq!(
            (verified.statements, verified.unnamed, verified.unsigned),
            (2, 0, 0)
        );
        assert_eq!(entry.pull().unwrap().kind(), PullKind::Approve);
    }

    #[test]
    fn an_approval_without_its_statement_or_with_one_for_another_head_does_not_count() {
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
                Some(&Seeded::new(1)),
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
        let count = || {
            repository
                .recount_approvals(number, Some(&register))
                .unwrap()
        };
        assert_eq!(count().count(), 1);
        let path = repository
            .statement_set()
            .path_of(&entry.statement().unwrap());
        let statement = std::fs::read(&path).unwrap();
        // An entry whose statement is gone names nothing: the approval is not even listed, and
        // `op verify` reports the entry.
        std::fs::remove_file(&path).unwrap();
        let recount = count();
        assert_eq!((recount.count(), recount.void().len()), (0, 0));
        let entries = repository.oplog().verify().unwrap();
        let error = repository
            .verify_statements(&entries, Some(&register))
            .unwrap_err();
        assert!(
            matches!(error, StatementCheckError::Missing { number, .. } if number == entry.number()),
            "{error:?}"
        );
        std::fs::write(&path, &statement).unwrap();
        assert_eq!(count().count(), 1);
        // A statement of Mia for another head does not carry over. Under an entry that claims
        // the current head it places nothing, and `op verify` reports the difference.
        let head = repository.refs().branch(&feature()).unwrap().unwrap();
        let other_head = CommitId::new(Blob::new("x").id().object_id());
        let pull = repository.pull_requests(None).unwrap()[0].id();
        let other = by_hand::statement(
            Body::Approval(statement::Approval::new(None, pull, other_head)),
            Some(&Seeded::new(2)),
        );
        let claimed = PullEntry::approve(now(), ana(), number, head).unwrap();
        let claimed = by_hand::pull_naming(&repository, &ana(), claimed, &other);
        let recount = count();
        assert_eq!((recount.count(), recount.void().len()), (1, 0));
        let entries = repository.oplog().verify().unwrap();
        let error = repository
            .verify_statements(&entries, Some(&register))
            .unwrap_err();
        assert!(
            matches!(error, StatementCheckError::Disagrees { number, .. } if number == claimed.number()),
            "{error:?}"
        );
    }

    #[test]
    fn the_recount_asks_the_register_for_every_statement_that_an_entry_names() {
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
                Some(&Seeded::new(1)),
                &AllowAll,
            )
            .unwrap()
            .number();
        let head = repository.refs().branch(&feature()).unwrap().unwrap();
        let other_head = CommitId::new(Blob::new("x").id().object_id());
        // Written past the writer: an approval for another head, one without a signature, and
        // one of Ana that the key of Mia signed. The last one is the only approval of Mia's
        // key, and it does not count for Ana.
        let stale = by_hand::approve(
            &repository,
            &mia(),
            number,
            other_head,
            Some(&Seeded::new(2)),
        );
        let unsigned = by_hand::approve(&repository, &mia(), number, head, None);
        let foreign = by_hand::approve(&repository, &ana(), number, head, Some(&Seeded::new(2)));
        let recount = repository
            .recount_approvals(number, Some(&register))
            .unwrap();
        assert_eq!(recount.count(), 0);
        let void: Vec<(u64, &pull::NotCounted)> = recount
            .void()
            .iter()
            .map(|(approval, why)| (approval.entry(), why))
            .collect();
        assert_eq!(void.len(), 3);
        assert_eq!(void[0], (stale.number(), &pull::NotCounted::OtherHead));
        assert!(
            matches!(void[1], (entry, pull::NotCounted::Proof(why))
                if entry == unsigned.number() && why.contains("not signed")),
            "{void:?}"
        );
        assert!(
            matches!(void[2], (entry, pull::NotCounted::Proof(_)) if entry == foreign.number()),
            "{void:?}"
        );
        // The open mode asks no register: the two approvals of the head count, one per identity.
        assert_eq!(
            repository.recount_approvals(number, None).unwrap().count(),
            2
        );
        // `op verify` calls the foreign key damage; a statement without a signature only does
        // not count.
        let entries = repository.oplog().verify().unwrap();
        let error = repository
            .verify_statements(&entries, Some(&register))
            .unwrap_err();
        assert!(
            matches!(
                error,
                StatementCheckError::Proof { number, error: ProofError::KeyNotOfActor }
                    if number == foreign.number()
            ),
            "{error:?}"
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
                Some(&Seeded::new(1)),
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
        repository
            .verify_statements(&entries, Some(&register))
            .unwrap();
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
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
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
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
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
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
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

            fn public_key(&self) -> Result<PublicKey, SignError> {
                Ok(key(2))
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
                Some(&Seeded::new(1)),
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
        assert!(repository.evidence(None).unwrap().approvals.is_empty());
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
                Some(&Seeded::new(1)),
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

    /// An enforcing repository with the lease of the branch held by Daniel.
    fn enforcing(dir: &TempDir) -> (Repository, Register) {
        let (repository, _) = repository(dir);
        repository
            .claim_lease(&feature(), &daniel(), 60, &AllowAll)
            .unwrap();
        (repository, approvers())
    }

    fn open_signed(repository: &Repository, register: &Register) -> Result<Entry, PullError> {
        repository.open_pull(
            &feature(),
            &daniel(),
            vec![],
            None,
            Some(register),
            Some(&Seeded::new(1)),
            &AllowAll,
        )
    }

    /// What the writers have written: the entries of the log and the statements of the set.
    fn written(repository: &Repository) -> (usize, usize) {
        (
            repository.oplog().entries().unwrap().len(),
            repository.statement_set().hashes().unwrap().len(),
        )
    }

    #[test]
    fn a_retired_key_signs_no_new_statement_and_nothing_is_written() {
        let dir = TempDir::new();
        let (repository, mut register) = enforcing(&dir);
        let number = open_signed(&repository, &register).unwrap().number();
        // Retired from far ahead: what the key signed before still counts, nothing new does.
        // Every writer makes its statement in one place (`make_statement`), so the approval
        // and the check stand for the opening and the closing; the key of the one owner of
        // this register cannot be retired.
        register
            .retire(&Identity::of(&mia()), &key(2), 1_000)
            .unwrap();
        let before = written(&repository);
        let error = repository
            .approve_pull(
                number,
                &mia(),
                Some(&register),
                Some(&Seeded::new(2)),
                &AllowAll,
            )
            .unwrap_err();
        assert!(matches!(error, PullError::KeyNotActive), "{error:?}");
        let error = repository
            .record_check(
                repository.refs().branch(&feature()).unwrap().unwrap(),
                None,
                crate::oplog::CheckResult::Pass,
                &mia(),
                Some(&register),
                Some(&Seeded::new(2)),
                &AllowAll,
            )
            .unwrap_err();
        assert!(
            matches!(error, crate::repository::CheckError::KeyNotActive),
            "{error:?}"
        );
        assert_eq!(written(&repository), before);
    }

    #[test]
    fn a_closing_is_signed_by_whoever_closes_and_refused_before_anything_is_written() {
        let dir = TempDir::new();
        let (repository, register) = enforcing(&dir);
        let number = open_signed(&repository, &register).unwrap().number();
        let before = written(&repository);
        let close = |actor: &Actor, signer: Option<&dyn Signer>| {
            repository.close_pull(number, actor, Some(&register), signer, &AllowAll)
        };
        assert!(matches!(
            close(&daniel(), None),
            Err(PullError::SignatureRequired)
        ));
        // The key of Mia does not close for Daniel, and Mia may not close at all.
        assert!(matches!(
            close(&daniel(), Some(&Seeded::new(2))),
            Err(PullError::Proof(_))
        ));
        assert!(matches!(
            close(&mia(), Some(&Seeded::new(2))),
            Err(PullError::NotAllowedToClose { .. })
        ));
        assert_eq!(written(&repository), before);
        let entry = close(&daniel(), Some(&Seeded::new(1))).unwrap();
        let statement = repository
            .statement_set()
            .get(&entry.statement().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(statement.key(), Some(&key(1)));
        let pulls = repository.pull_requests(None).unwrap();
        assert!(
            matches!(pulls[0].state(), PullState::Closed { entry: closed, .. } if *closed == entry.number())
        );
        let entries = repository.oplog().verify().unwrap();
        repository
            .verify_statements(&entries, Some(&register))
            .unwrap();
    }

    #[test]
    fn a_pull_request_that_ends_while_its_closing_is_signed_is_found_under_the_lock() {
        struct Closing<'a>(&'a Repository, u64);

        impl Signer for Closing<'_> {
            fn sign(&self, message: &[u8]) -> Result<crate::signature::Signature, SignError> {
                by_hand::close(self.0, &daniel(), self.1, Some(&Seeded::new(1)));
                Seeded::new(1).sign(message)
            }

            fn public_key(&self) -> Result<PublicKey, SignError> {
                Ok(key(1))
            }
        }

        let dir = TempDir::new();
        let (repository, register) = enforcing(&dir);
        let number = open_signed(&repository, &register).unwrap().number();
        let before = written(&repository);
        let error = repository
            .close_pull(
                number,
                &daniel(),
                Some(&register),
                Some(&Closing(&repository, number)),
                &AllowAll,
            )
            .unwrap_err();
        assert!(matches!(error, PullError::NotOpen { .. }), "{error:?}");
        // The only new entry and the only new statement are those of the other closing.
        assert_eq!(written(&repository), (before.0 + 1, before.1 + 1));
    }

    #[test]
    fn a_branch_that_moves_while_its_opening_is_signed_is_refused_and_nothing_is_written() {
        struct Moving<'a>(&'a Repository);

        impl Signer for Moving<'_> {
            fn sign(&self, message: &[u8]) -> Result<crate::signature::Signature, SignError> {
                push_commit(self.0, &daniel());
                Seeded::new(1).sign(message)
            }

            fn public_key(&self) -> Result<PublicKey, SignError> {
                Ok(key(1))
            }
        }

        let dir = TempDir::new();
        let (repository, register) = enforcing(&dir);
        let before = written(&repository);
        let error = repository
            .open_pull(
                &feature(),
                &daniel(),
                vec![],
                None,
                Some(&register),
                Some(&Moving(&repository)),
                &AllowAll,
            )
            .unwrap_err();
        assert!(
            matches!(&error, PullError::BranchMoved { branch } if *branch == feature()),
            "{error:?}"
        );
        // The only new entry is the commit that moved the branch; no statement was put.
        assert_eq!(written(&repository), (before.0 + 1, before.1));
        // A new try opens on the new head.
        let opened = open_signed(&repository, &register).unwrap();
        let head = repository.refs().branch(&feature()).unwrap().unwrap();
        assert_eq!(opened.pull().unwrap().commit(), Some(&head));
    }

    #[test]
    fn a_lost_statement_never_opens_a_pull_request_again() {
        let dir = TempDir::new();
        let (repository, _) = repository(&dir);
        let number = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
            .unwrap()
            .number();
        let closed = repository
            .close_pull(number, &daniel(), None, None, &AllowAll)
            .unwrap();
        let path = repository
            .statement_set()
            .path_of(&closed.statement().unwrap());
        // The closing can only hold the change back, so it stands from the lines of its entry
        // where its statement is gone or is no statement; `op verify` reports the damage.
        for damage in [None, Some("not a statement")] {
            match damage {
                None => std::fs::remove_file(&path).unwrap(),
                Some(text) => std::fs::write(&path, text).unwrap(),
            }
            let pulls = repository.pull_requests(None).unwrap();
            assert!(
                matches!(pulls[0].state(), PullState::Closed { entry, .. } if *entry == closed.number()),
                "{damage:?}: {pulls:?}"
            );
            assert!(matches!(
                repository.approve_pull(number, &mia(), None, None, &AllowAll),
                Err(PullError::NotOpen { .. })
            ));
            let entries = repository.oplog().verify().unwrap();
            assert!(
                repository.verify_statements(&entries, None).is_err(),
                "{damage:?}"
            );
        }
        // The ended pull request is final also while its opening is lost: the log still names
        // it, so the same opening is refused and nothing is written.
        let lost = repository
            .statement_set()
            .path_of(&repository.pull_requests(None).unwrap()[0].id());
        let kept = std::fs::read(&lost).unwrap();
        std::fs::remove_file(&lost).unwrap();
        let before = written(&repository);
        let error = repository
            .open_pull(&feature(), &daniel(), vec![], None, None, None, &AllowAll)
            .unwrap_err();
        assert!(
            matches!(error, PullError::SameOpening { number: n } if n == number),
            "{error:?}"
        );
        assert_eq!(written(&repository), before);
        std::fs::write(&lost, kept).unwrap();
        // An opening can let a change through, so it counts only with its statement.
        let opening = repository.oplog().entries().unwrap();
        let opening = opening
            .iter()
            .find(|entry| entry.number() == number)
            .unwrap();
        std::fs::remove_file(
            repository
                .statement_set()
                .path_of(&opening.statement().unwrap()),
        )
        .unwrap();
        assert!(repository.pull_requests(None).unwrap().is_empty());
    }

    #[test]
    fn op_verify_refuses_a_retired_key_on_a_check_and_counts_what_is_unsigned() {
        let dir = TempDir::new();
        let (repository, mut register) = enforcing(&dir);
        let number = open_signed(&repository, &register).unwrap().number();
        let head = repository.refs().branch(&feature()).unwrap().unwrap();
        // An approval without a signature is no damage: it does not count, and it is counted.
        by_hand::approve(&repository, &mia(), number, head, None);
        let entries = repository.oplog().verify().unwrap();
        let verified = repository
            .verify_statements(&entries, Some(&register))
            .unwrap();
        assert_eq!((verified.statements, verified.unsigned), (2, 1));
        assert_eq!(
            repository
                .verify_statements(&entries, None)
                .unwrap()
                .unsigned,
            0,
            "the open mode asks for no signature"
        );
        // A check of a key that was retired before its entry is damage: only an approval of a
        // key that no longer counts is void and passes.
        let check = by_hand::check(
            &repository,
            &ana(),
            head,
            None,
            crate::oplog::CheckResult::Pass,
            Some(&Seeded::new(3)),
        );
        let approval = by_hand::approve(&repository, &ana(), number, head, Some(&Seeded::new(3)));
        register
            .retire(&Identity::of(&ana()), &key(3), check.number())
            .unwrap();
        assert!(approval.number() > check.number());
        let entries = repository.oplog().verify().unwrap();
        let error = repository
            .verify_statements(&entries, Some(&register))
            .unwrap_err();
        assert!(
            matches!(
                error,
                StatementCheckError::Proof {
                    number,
                    error: ProofError::KeyNoLongerCounts { .. }
                } if number == check.number()
            ),
            "{error:?}"
        );
        // Neither counts: the check for the rule table, the approval for the recount.
        assert!(repository
            .counted_checks(
                number,
                &head,
                &head,
                crate::policy::RiskLevel::Zero,
                Some(&register)
            )
            .unwrap()
            .is_empty());
        assert_eq!(
            repository
                .recount_approvals(number, Some(&register))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn an_opening_counts_in_an_enforcing_repository_only_with_a_signature_of_its_opener() {
        let dir = TempDir::new();
        let (repository, register) = enforcing(&dir);
        let head = repository.refs().branch(&feature()).unwrap().unwrap();
        // Written past the writer: an opening without a signature, as a repository that left
        // the open mode holds one, and an opening of Daniel that the key of Mia signed.
        let unsigned = by_hand::open(&repository, &daniel(), &feature(), head, None, None);
        let foreign = by_hand::open(
            &repository,
            &daniel(),
            &feature(),
            head,
            None,
            Some(&Seeded::new(2)),
        );
        // The open mode asks no register and knows both; an enforcing repository knows none.
        let numbers = |register: Option<&Register>| -> Vec<u64> {
            repository
                .pull_requests(register)
                .unwrap()
                .iter()
                .map(PullRequest::number)
                .collect()
        };
        assert_eq!(numbers(None), [unsigned.number(), foreign.number()]);
        assert!(numbers(Some(&register)).is_empty());
        // So nothing can be approved, counted, or merged for them.
        for number in [unsigned.number(), foreign.number()] {
            assert!(matches!(
                repository.approve_pull(
                    number,
                    &mia(),
                    Some(&register),
                    Some(&Seeded::new(2)),
                    &AllowAll
                ),
                Err(PullError::NoSuchPull { .. })
            ));
            assert!(matches!(
                repository.recount_approvals(number, Some(&register)),
                Err(PullError::NoSuchPull { .. })
            ));
            assert!(matches!(
                repository.record_pull_merge(number, &daniel(), head, Some(&register), &AllowAll),
                Err(PullError::NoSuchPull { .. })
            ));
        }
        // The branch is free for an opening of its write owner, which counts.
        push_commit(&repository, &daniel());
        let signed = open_signed(&repository, &register).unwrap();
        assert_eq!(numbers(Some(&register)), [signed.number()]);
        // `op verify` calls the foreign key damage and counts the opening without a signature.
        let entries = repository.oplog().verify().unwrap();
        let error = repository
            .verify_statements(&entries, Some(&register))
            .unwrap_err();
        assert!(
            matches!(
                error,
                StatementCheckError::Proof { number, error: ProofError::KeyNotOfActor }
                    if number == foreign.number()
            ),
            "{error:?}"
        );
    }

    #[test]
    fn an_opening_stays_when_its_key_is_retired_later_and_goes_when_it_was_retired_before() {
        let dir = TempDir::new();
        let (repository, mut register) = enforcing(&dir);
        // Ana opens by hand: the register holds one owner, whose key cannot be retired.
        let head = repository.refs().branch(&feature()).unwrap().unwrap();
        let opened = by_hand::open(
            &repository,
            &ana(),
            &feature(),
            head,
            None,
            Some(&Seeded::new(3)),
        );
        let count = |register: &Register| repository.pull_requests(Some(register)).unwrap().len();
        assert_eq!(count(&register), 1);
        let mut later = register.clone();
        later
            .retire(&Identity::of(&ana()), &key(3), opened.number() + 1)
            .unwrap();
        assert_eq!(count(&later), 1, "the opening stands before the retirement");
        let mut revoked = register.clone();
        register
            .retire(&Identity::of(&ana()), &key(3), opened.number())
            .unwrap();
        assert_eq!(
            count(&register),
            0,
            "the key signed nothing from this entry on"
        );
        // A revoked key counts for an opening as for every kind but an approval: before the
        // bound of its revocation.
        revoked
            .revoke(&Identity::of(&ana()), &key(3), opened.number() + 1)
            .unwrap();
        assert_eq!(
            count(&revoked),
            1,
            "the opening stands before the revocation"
        );
    }

    #[test]
    fn a_closing_ends_its_pull_request_whatever_its_signature() {
        let dir = TempDir::new();
        let (repository, register) = enforcing(&dir);
        let number = open_signed(&repository, &register).unwrap().number();
        // A closing can only hold a change back, so it stays whoever wrote it: here without a
        // signature, written past the writer. `op verify` counts it.
        let closed = by_hand::close(&repository, &mia(), number, None);
        let pulls = repository.pull_requests(Some(&register)).unwrap();
        assert!(
            matches!(pulls[0].state(), PullState::Closed { entry, .. } if *entry == closed.number()),
            "{pulls:?}"
        );
        let entries = repository.oplog().verify().unwrap();
        let verified = repository
            .verify_statements(&entries, Some(&register))
            .unwrap();
        assert_eq!(verified.unsigned, 1);
    }

    #[test]
    fn a_forged_signature_of_a_revoked_key_is_damage_and_not_a_void_approval() {
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
                Some(&Seeded::new(1)),
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
        // The statement now holds a signature of the key of Mia over something else.
        let forged = signature::sign(&Seeded::new(2), PayloadKind::PullApproval, b"other").unwrap();
        let path = repository
            .statement_set()
            .path_of(&entry.statement().unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        let signature_line = text.lines().last().unwrap();
        let changed = text.replace(
            signature_line,
            &format!("signature {}", register::signature_to_line(&forged)),
        );
        std::fs::write(&path, changed).unwrap();
        register
            .revoke(&Identity::of(&mia()), &key(2), 100)
            .unwrap();
        // The signature is checked before the state of the key, so a key that no longer counts
        // cannot hide a forged statement: the approval is not listed, and `op verify` reports
        // the file.
        let recount = repository
            .recount_approvals(number, Some(&register))
            .unwrap();
        assert_eq!((recount.count(), recount.void().len()), (0, 0));
        let entries = repository.oplog().verify().unwrap();
        let error = repository
            .verify_statements(&entries, Some(&register))
            .unwrap_err();
        assert!(
            matches!(error, StatementCheckError::Set(SetError::Malformed { .. })),
            "{error:?}"
        );
    }
}
