//! The purge itself: `stemma purge BLOB --apply`, `--resume`, and `--abort` (ADR-014).
//!
//! A purge takes a snapshot, takes the lock of the references, plans again under it, moves the
//! repository to format 4, and writes the journal. Then it writes the new trees and commits,
//! checks the pointers, writes the `purge` entry, moves the pointers, replaces the entries by
//! stubs, deletes the delete set, and removes the journal. Each step after the journal can be
//! repeated, so `--resume` runs them again from the journal. Until the `purge` entry is written,
//! nothing is lost, and `--abort` removes the new objects and the journal; after it, the purge can
//! only go forward.

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use super::journal::{self, Journal, JournalError};
use super::{refuse_agent, Planner, PurgeError, PurgePlan};
use crate::object::{BlobId, CommitId, ObjectId};
use crate::oplog::{Entry, EntryError, OpLogError, Purge, Record};
use crate::refs::{Logged, PurgeLock, RefError};
use crate::repository::{
    now, raise_format, Repository, SnapshotError, UpdateError, CHAIN_FORMAT_VERSION,
};
use crate::store::{ObjectStore, StoreError};
use crate::{Actor, Authorizer};

/// What a purge did, or what `--resume` finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Purged {
    snapshot: Option<Entry>,
    plan: PurgePlan,
    entries: Vec<Entry>,
}

impl Purged {
    /// The entry of the snapshot that the purge took first, if the directory had changed. A
    /// resumed purge takes none.
    pub fn snapshot(&self) -> Option<&Entry> {
        self.snapshot.as_ref()
    }

    /// What the purge did: the plan that it carried out.
    pub fn plan(&self) -> &PurgePlan {
        &self.plan
    }

    /// The `purge` entries of the log; more than one if the list of stubs did not fit into one,
    /// or if `--resume` wrote the rest of a list that stopped.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

/// What `--abort` undid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aborted {
    plan: PurgePlan,
    removed: Vec<ObjectId>,
}

impl Aborted {
    /// The plan of the purge that was undone.
    pub fn plan(&self) -> &PurgePlan {
        &self.plan
    }

    /// The objects that the journal names as new and that the abort removed. One that the purge
    /// had not written yet, or that an earlier abort removed, is not among them.
    pub fn removed(&self) -> &[ObjectId] {
        &self.removed
    }
}

/// The step of a purge at which it stopped (ADR-014).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PurgeStep {
    /// Writing or reading the journal, or taking the lock for it.
    Journal,
    /// Writing the new trees and commits.
    Objects,
    /// Checking the pointers, writing the `purge` entry, and moving the pointers.
    Entry,
    /// Replacing the entries by stubs.
    Stubs,
    /// Deleting the delete set.
    Delete,
    /// Removing the journal.
    Finish,
    /// Removing the new objects for `--abort`.
    Abort,
}

impl fmt::Display for PurgeStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Journal => "the journal",
            Self::Objects => "writing the new objects",
            Self::Entry => "the entry and the move of the pointers",
            Self::Stubs => "replacing the entries by stubs",
            Self::Delete => "deleting the old objects",
            Self::Finish => "removing the journal",
            Self::Abort => "removing the new objects",
        })
    }
}

/// The step after which a test stops a purge, as a crash would: the lock file stays behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stop {
    Journal,
    Objects,
    Entry,
    Moves,
    Stubs,
    Delete,
}

/// What failed in a step of a purge.
#[derive(Debug)]
#[non_exhaustive]
pub enum PurgeFailure {
    /// The references failed, or a pointer is at neither of its values
    /// ([`RefError::PurgeStale`]).
    Refs(RefError),
    /// The object store failed.
    Store(Box<StoreError>),
    /// The operation log failed.
    Log(OpLogError),
    /// The `purge` entry cannot be built.
    Entry(EntryError),
    /// The journal failed.
    Journal(JournalError),
    /// The new objects cannot be made again from the old ones, which `--resume` does.
    Plan(Box<PurgeError>),
    /// The rewrite of a commit gives another ID than the journal names: the store has changed.
    Mismatch {
        /// The old commit.
        old: CommitId,
        /// The new one that the journal names.
        expected: CommitId,
        /// The new one that the rewrite gives.
        found: CommitId,
    },
    /// A new commit is missing although the `purge` entry is written.
    Missing(CommitId),
}

impl fmt::Display for PurgeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refs(error) => error.fmt(f),
            Self::Store(error) => error.fmt(f),
            Self::Log(error) => error.fmt(f),
            Self::Entry(error) => error.fmt(f),
            Self::Journal(error) => error.fmt(f),
            Self::Plan(error) => error.reason(f),
            Self::Mismatch {
                old,
                expected,
                found,
            } => write!(
                f,
                "the journal names {expected} as the new version of {old}, but the rewrite gives \
                 {found}"
            ),
            Self::Missing(commit) => write!(
                f,
                "the new commit {commit} is missing, although the entry of the purge is written; \
                 a resume cannot make it again, so it has to be restored first"
            ),
        }
    }
}

impl Error for PurgeFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Refs(error) => error.source(),
            Self::Store(error) => error.source(),
            Self::Log(error) => error.source(),
            Self::Entry(error) => error.source(),
            Self::Journal(error) => error.source(),
            Self::Plan(error) => error.source(),
            Self::Mismatch { .. } | Self::Missing(_) => None,
        }
    }
}

/// Why [`Repository::apply_purge`], [`Repository::resume_purge`], or
/// [`Repository::abort_purge`] fails. [`pending`](Self::pending) says whether the journal stays,
/// so that `--resume` (or, before the `purge` entry, `--abort`) has to come next.
#[derive(Debug)]
#[non_exhaustive]
pub enum ApplyPurgeError {
    /// The purge was refused or could not be planned. Nothing of the purge was written. No
    /// journal exists.
    Plan(PurgeError),
    /// The snapshot before the purge failed. Nothing of the purge was written. No journal exists.
    Snapshot(SnapshotError),
    /// The purge could not start under the lock: the lock could not be taken, the log or the
    /// store could not be read, or the format file or the journal could not be written. No
    /// journal exists; the format can be 3 already.
    Start(PurgeFailure),
    /// The purge failed after the snapshot that it took first had saved the working directory.
    /// The snapshot stays, as any snapshot does; `source` says what failed, and whether a journal
    /// exists.
    AfterSnapshot {
        /// The entry of the snapshot.
        snapshot: Box<Entry>,
        /// What failed after it.
        source: Box<ApplyPurgeError>,
    },
    /// A purge has not finished: its journal exists. Only `--resume` or `--abort` may run.
    Pending {
        /// The journal.
        path: PathBuf,
    },
    /// `--resume` or `--abort` found a journal that cannot be read. It stays, and keeps every
    /// writer out; neither mode can go on before it is repaired by hand.
    Unreadable(Box<JournalError>),
    /// `--resume` or `--abort` found no journal: no purge is pending.
    NotPending {
        /// The path of the journal.
        path: PathBuf,
    },
    /// The purge stopped at a step. The journal stays; `--resume` finishes the purge, and
    /// `--abort` undoes it as long as the `purge` entry is not written.
    Stopped {
        /// The step.
        step: PurgeStep,
        /// The numbers of the `purge` entries that are written; empty if none is known to be,
        /// so that `--abort` can still undo the purge.
        entries: Vec<u64>,
        /// What failed.
        source: Box<PurgeFailure>,
    },
    /// The purge is finished, or undone by `--abort`, and its journal is removed, but the
    /// directory could not be flushed, so the removal may not survive a crash. No journal
    /// exists now; after a crash, `--resume` or `--abort` finds it again and finishes.
    NotDurable {
        /// The numbers of the `purge` entries; empty for an abort, as a purge writes at least one.
        entries: Vec<u64>,
        /// The error of the flush.
        source: Box<JournalError>,
    },
    /// `--abort` was refused, because the `purge` entry is written: that is the point of no
    /// return, and only `--resume` can finish the purge.
    PastNoReturn {
        /// The numbers of the `purge` entries.
        entries: Vec<u64>,
    },
}

impl ApplyPurgeError {
    /// Whether the journal of a purge exists after the failure, so that `--resume` or `--abort`
    /// has to come before any other command that writes.
    pub fn pending(&self) -> bool {
        match self {
            Self::Pending { .. }
            | Self::Unreadable(_)
            | Self::Stopped { .. }
            | Self::PastNoReturn { .. } => true,
            Self::AfterSnapshot { source, .. } => source.pending(),
            Self::Plan(_)
            | Self::Snapshot(_)
            | Self::Start(_)
            | Self::NotPending { .. }
            | Self::NotDurable { .. } => false,
        }
    }

    /// Whether the call made something that stays although it failed: a journal, or a purge or
    /// an abort that is not known to be flushed. A command ends with code 3 then.
    pub fn made(&self) -> bool {
        match self {
            Self::AfterSnapshot { source, .. } => source.made(),
            Self::NotDurable { .. } => true,
            other => other.pending(),
        }
    }
}

impl fmt::Display for ApplyPurgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plan(error) => error.fmt(f),
            Self::Snapshot(error) => write!(
                f,
                "could not save the working directory before the purge, nothing was purged: \
                 {error}"
            ),
            Self::Start(source) => {
                write!(f, "could not start the purge, nothing was purged: {source}")
            }
            Self::Pending { path } => RefError::PurgePending { path: path.clone() }.fmt(f),
            Self::Unreadable(source) => write!(
                f,
                "the journal of a purge cannot be read: {source}; neither `stemma purge --resume` \
                 nor `stemma purge --abort` can go on before it is repaired, and no other command \
                 writes while it exists"
            ),
            Self::NotPending { path } => write!(
                f,
                "no purge is pending: there is no journal `{}`",
                path.display()
            ),
            Self::AfterSnapshot { snapshot, source } => write!(
                f,
                "{source} (the working copy was saved first as entry {})",
                snapshot.number()
            ),
            Self::Stopped {
                step,
                entries,
                source,
            } if entries.is_empty() => write!(
                f,
                "the purge stopped at {step}: {source}; its journal stays, `stemma purge --resume` \
                 finishes it, and `stemma purge --abort` undoes it as long as its entry is not \
                 written"
            ),
            Self::Stopped {
                step,
                entries,
                source,
            } => write!(
                f,
                "the purge stopped at {step} after its entry {} was written: {source}; its \
                 journal stays, and only `stemma purge --resume` finishes it",
                numbers(entries)
            ),
            Self::NotDurable { entries, source } if entries.is_empty() => write!(
                f,
                "the purge was undone and its journal removed, but flushing failed: {source}; \
                 after a crash, run `stemma purge --abort` again"
            ),
            Self::NotDurable { entries, source } => write!(
                f,
                "the purge was made as entry {} and its journal removed, but flushing failed: \
                 {source}; after a crash, run `stemma purge --resume`",
                numbers(entries)
            ),
            Self::PastNoReturn { entries } => write!(
                f,
                "the purge cannot be undone, as its entry {} is written; `stemma purge --resume` \
                 finishes it",
                numbers(entries)
            ),
        }
    }
}

/// The error of a journal that could not be removed at the end of a purge (`entries` are its
/// `purge` entries) or of an abort (none): [`ApplyPurgeError::NotDurable`] if it is gone but not
/// known to be flushed, as the work is done, and a stop at [`PurgeStep::Finish`] if it stays.
fn not_removed(entries: &[u64], error: JournalError) -> ApplyPurgeError {
    if error.made() {
        ApplyPurgeError::NotDurable {
            entries: entries.to_vec(),
            source: Box::new(error),
        }
    } else {
        ApplyPurgeError::Stopped {
            step: PurgeStep::Finish,
            entries: entries.to_vec(),
            source: Box::new(PurgeFailure::Journal(error)),
        }
    }
}

/// The numbers of entries, for a message: `7` or `7, 8`.
fn numbers(entries: &[u64]) -> String {
    entries
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl Error for ApplyPurgeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Plan(error) => error.source(),
            Self::Snapshot(error) => error.source(),
            Self::Start(source) => source.source(),
            Self::Stopped { source, .. } => source.source(),
            Self::AfterSnapshot { source, .. } => source.source(),
            Self::NotDurable { source, .. } => source.source(),
            Self::Unreadable(source) => source.source(),
            Self::Pending { .. } | Self::NotPending { .. } | Self::PastNoReturn { .. } => None,
        }
    }
}

/// The error of a step that runs between the checks of the pointers and their moves.
enum EntryStep {
    /// The entry could not be written; carries the numbers of the `purge` entries that are in
    /// the log nonetheless.
    Failed(Vec<u64>, Box<PurgeFailure>),
    /// A test stopped the purge after the entry.
    Halted,
}

impl Repository {
    /// Purges the blob: removes it from the history and from the operation log (ADR-014).
    ///
    /// The actor must be a human, and the authorizer is asked for `purge` with the blob, as for
    /// [`plan_purge`](Self::plan_purge). Then the purge takes a snapshot of the working directory,
    /// takes the lock of the references and keeps it to the end, and plans again under it, so its
    /// plan can differ from one printed before. It moves the repository to format 4 and writes
    /// the journal `.stemma/purge-pending`, and from then on every other command that writes
    /// refuses. It writes the new trees and commits, checks that every pointer is at its value
    /// before the purge, writes the `purge` entry, and moves the pointers. Last, it replaces the
    /// entries that named a rewritten commit by stubs, deletes the old commits, the old trees, and
    /// the blob, and removes the journal.
    ///
    /// # Errors
    ///
    /// Returns [`ApplyPurgeError::Plan`] for the errors of [`plan_purge`](Self::plan_purge),
    /// [`ApplyPurgeError::Pending`] if a purge has not finished, [`ApplyPurgeError::Snapshot`] if
    /// the snapshot fails (with [`RefError::Locked`] while another command holds the lock; nothing
    /// is written then), [`ApplyPurgeError::Start`] if the purge cannot start under the lock, and
    /// [`ApplyPurgeError::Stopped`] if a step after the journal fails; then the journal stays.
    pub fn apply_purge(
        &self,
        blob: ObjectId,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Purged, ApplyPurgeError> {
        self.apply_purge_until(blob, actor, authorizer, None)
            .map(|purged| purged.expect("only a test stops a purge"))
    }

    /// Finishes a purge that stopped, from its journal (ADR-014). The lock that the purge left
    /// behind is taken for its own and removed. Each step is repeated: a new object that exists is
    /// left alone, a pointer at its new value is skipped, a `purge` entry that is written is found
    /// and not written again, a stub is replaced by itself, and a deleted object is gone.
    ///
    /// # Errors
    ///
    /// Returns [`ApplyPurgeError::Plan`] with [`PurgeError::Agent`] or [`PurgeError::Update`] if
    /// the actor is an agent or the authorizer refuses, [`ApplyPurgeError::NotPending`] if there is
    /// no journal, and [`ApplyPurgeError::Stopped`] if a step fails again, among them a pointer
    /// that is at neither of its values ([`RefError::PurgeStale`]).
    pub fn resume_purge(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Purged, ApplyPurgeError> {
        self.resume_purge_until(actor, authorizer, None)
            .map(|purged| purged.expect("only a test stops a purge"))
    }

    /// Undoes a purge that stopped before its `purge` entry (ADR-014): removes the objects that
    /// the journal names as new, then the journal. The lock that the purge left behind is taken
    /// for its own and removed. The format stays at 3, as a format is never lowered.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`resume_purge`](Self::resume_purge) before a step, and
    /// [`ApplyPurgeError::PastNoReturn`] if the `purge` entry is written.
    pub fn abort_purge(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Aborted, ApplyPurgeError> {
        let (journal, lock) = self.take_over(actor, authorizer)?;
        let written = self.written_purge(&journal).map_err(stopped(
            PurgeStep::Abort,
            &[],
            PurgeFailure::Log,
        ))?;
        if !written.is_empty() {
            return Err(ApplyPurgeError::PastNoReturn {
                entries: written.iter().map(Entry::number).collect(),
            });
        }
        let mut removed = Vec::new();
        for id in &journal.new {
            if self
                .objects()
                .remove(id)
                .map_err(stopped(PurgeStep::Abort, &[], store))?
            {
                removed.push(*id);
            }
        }
        self.remove_journal(&[])?;
        drop(lock);
        Ok(Aborted {
            plan: journal.plan,
            removed,
        })
    }

    /// [`apply_purge`](Self::apply_purge), which a test can stop at a step. Returns `None` if it
    /// stopped.
    pub(super) fn apply_purge_until(
        &self,
        blob: ObjectId,
        actor: &Actor,
        authorizer: &dyn Authorizer,
        stop: Option<Stop>,
    ) -> Result<Option<Purged>, ApplyPurgeError> {
        let blob = self
            .check_purge(blob, actor, authorizer)
            .map_err(ApplyPurgeError::Plan)?;
        let pending = |error| match error {
            RefError::PurgePending { path } => ApplyPurgeError::Pending { path },
            other => ApplyPurgeError::Start(PurgeFailure::Refs(other)),
        };
        self.refs_store().refuse_pending_purge().map_err(pending)?;
        // A purge that wrote its journal since the check above is refused by the snapshot.
        let snapshot = self
            .snapshot(actor, authorizer)
            .map_err(|error| match error {
                SnapshotError::Update(UpdateError::Refs(RefError::PurgePending { path })) => {
                    ApplyPurgeError::Pending { path }
                }
                other => ApplyPurgeError::Snapshot(other),
            })?;
        let purged = self.start_purge(blob, actor, stop, pending);
        match (purged, snapshot) {
            (Ok(purged), snapshot) => Ok(purged.map(|purged| Purged { snapshot, ..purged })),
            (Err(error), Some(snapshot)) => Err(ApplyPurgeError::AfterSnapshot {
                snapshot: Box::new(snapshot),
                source: Box::new(error),
            }),
            (Err(error), None) => Err(error),
        }
    }

    /// The purge after its snapshot: the lock, the plan, the format, the journal, and the steps.
    fn start_purge(
        &self,
        blob: BlobId,
        actor: &Actor,
        stop: Option<Stop>,
        pending: impl Fn(RefError) -> ApplyPurgeError,
    ) -> Result<Option<Purged>, ApplyPurgeError> {
        let lock = self.refs_store().lock_purge().map_err(pending)?;
        let (plan, planner) = self.plan_blob(blob).map_err(ApplyPurgeError::Plan)?;
        let start = |source| ApplyPurgeError::Start(source);
        let last = self
            .oplog()
            .numbers()
            .map_err(|error| start(PurgeFailure::Log(error)))?
            .into_iter()
            .max()
            .unwrap_or(0);
        let mut new = Vec::new();
        for object in planner.new_objects(&plan.rewrites) {
            let id = object.id();
            if !self
                .objects()
                .contains(&id)
                .map_err(|error| start(store(error)))?
            {
                new.push(id);
            }
        }
        new.sort();
        new.dedup();
        let dir = self.metadata_dir();
        raise_format(&dir, CHAIN_FORMAT_VERSION).map_err(|error| start(store(error)))?;
        let journal = Journal { plan, last, new };
        journal::write(&dir, &journal).map_err(|error| {
            if error.made() {
                ApplyPurgeError::Stopped {
                    step: PurgeStep::Journal,
                    entries: Vec::new(),
                    source: Box::new(PurgeFailure::Journal(error)),
                }
            } else {
                start(PurgeFailure::Journal(error))
            }
        })?;
        if stop == Some(Stop::Journal) {
            lock.leak();
            return Ok(None);
        }
        Ok(self
            .carry_out(&journal, actor, lock, stop)?
            .map(|entries| Purged {
                snapshot: None,
                plan: journal.plan,
                entries,
            }))
    }

    /// [`resume_purge`](Self::resume_purge), which a test can stop at a step.
    pub(super) fn resume_purge_until(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
        stop: Option<Stop>,
    ) -> Result<Option<Purged>, ApplyPurgeError> {
        let (journal, lock) = self.take_over(actor, authorizer)?;
        Ok(self
            .carry_out(&journal, actor, lock, stop)?
            .map(|entries| Purged {
                snapshot: None,
                plan: journal.plan,
                entries,
            }))
    }

    /// Refuses an agent, reads the journal, asks the authorizer for `purge` with its blob, takes
    /// the lock over, and reads the journal again under it.
    fn take_over(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<(Journal, PurgeLock), ApplyPurgeError> {
        refuse_agent(actor).map_err(ApplyPurgeError::Plan)?;
        let dir = self.metadata_dir();
        let read = || match journal::read(&dir) {
            Ok(Some(journal)) => Ok(journal),
            Ok(None) => Err(ApplyPurgeError::NotPending {
                path: journal::path(&dir),
            }),
            Err(error) => Err(ApplyPurgeError::Unreadable(Box::new(error))),
        };
        let journal = read()?;
        self.authorize_purge(actor, authorizer, &journal.plan.blob.to_string())
            .map_err(ApplyPurgeError::Plan)?;
        let lock = self.refs_store().take_over_lock().map_err(stopped(
            PurgeStep::Journal,
            &[],
            PurgeFailure::Refs,
        ))?;
        Ok((read()?, lock))
    }

    /// The `purge` entries that the purge of the journal has written: those after the last entry
    /// that the journal recorded. The lock keeps every other writer out, so no other entry can
    /// come after it.
    fn written_purge(&self, journal: &Journal) -> Result<Vec<Entry>, OpLogError> {
        Ok(self
            .oplog()
            .entries()?
            .into_iter()
            .filter(|entry| {
                entry.number() > journal.last && matches!(entry.record(), Record::Purge(_))
            })
            .collect())
    }

    /// The steps from the new objects on (5 to 8 of ADR-014), with the lock that the purge holds.
    /// Every step can be repeated, so the same code finishes a purge that stopped. Returns the
    /// `purge` entries, or `None` if a test stopped it.
    fn carry_out(
        &self,
        journal: &Journal,
        actor: &Actor,
        lock: PurgeLock,
        stop: Option<Stop>,
    ) -> Result<Option<Vec<Entry>>, ApplyPurgeError> {
        let plan = &journal.plan;
        let mut written = self.written_purge(journal).map_err(stopped(
            PurgeStep::Objects,
            &[],
            PurgeFailure::Log,
        ))?;
        // The numbers of the `purge` entries that are written, for an error: once there is one,
        // only `--resume` can finish the purge.
        let numbers = |written: &[Entry]| written.iter().map(Entry::number).collect::<Vec<_>>();

        // Step 5: the new objects. Nothing is deleted before the entry, so the old ones are there
        // to make them again from.
        if written.is_empty() {
            self.write_new_objects(journal).map_err(stopped(
                PurgeStep::Objects,
                &[],
                |failure| failure,
            ))?;
        } else {
            for (_, new) in &plan.rewrites {
                let present = self.objects().contains(&new.object_id()).map_err(stopped(
                    PurgeStep::Objects,
                    &numbers(&written),
                    store,
                ))?;
                if !present {
                    return Err(ApplyPurgeError::Stopped {
                        step: PurgeStep::Objects,
                        entries: numbers(&written),
                        source: Box::new(PurgeFailure::Missing(*new)),
                    });
                }
            }
        }
        if stop == Some(Stop::Objects) {
            lock.leak();
            return Ok(None);
        }

        // Step 6: the pointers are checked, then the entry is written, then they move. A list of
        // stubs that stopped after a part is finished with the rest, without the count.
        let listed = |number: u64| {
            written.iter().any(|entry| match entry.record() {
                Record::Purge(purge) => purge.lists(number),
                _ => false,
            })
        };
        let missing: Vec<u64> = plan
            .entries
            .iter()
            .copied()
            .filter(|number| !listed(*number))
            .collect();
        let commits = if written.is_empty() {
            plan.rewrites.len() as u64
        } else {
            0
        };
        let step = || {
            let mut entries = Vec::new();
            if written.is_empty() || !missing.is_empty() {
                let purge =
                    Purge::new(now(), actor.clone(), commits, &missing).map_err(|error| {
                        EntryStep::Failed(Vec::new(), Box::new(PurgeFailure::Entry(error)))
                    })?;
                entries = self.oplog().append_purge(purge).map_err(|error| {
                    // An entry that is in the log although the call failed is written.
                    let made = match &error {
                        OpLogError::NotFlushed { entry, .. } => vec![entry.number()],
                        OpLogError::PurgeIncomplete { written, .. }
                        | OpLogError::PurgeNotFlushed { written, .. } => numbers(written),
                        // Every other error leaves no entry behind (see `FsOpLog::append`).
                        OpLogError::Io { .. }
                        | OpLogError::Corrupt { .. }
                        | OpLogError::WrongNumber { .. }
                        | OpLogError::Gap { .. }
                        | OpLogError::TipAhead { .. }
                        | OpLogError::BadTip { .. }
                        | OpLogError::TipMismatch { .. }
                        | OpLogError::ChainBroken { .. }
                        | OpLogError::UnchainedAfterChain { .. }
                        | OpLogError::TipMissing { .. }
                        | OpLogError::TipBehind { .. }
                        | OpLogError::Predecessor { .. }
                        | OpLogError::UnexpectedFile { .. }
                        | OpLogError::Missing { .. }
                        | OpLogError::Full
                        | OpLogError::Contended { .. }
                        | OpLogError::EntryTooLarge { .. }
                        | OpLogError::Entry(_)
                        | OpLogError::UnlistedStub { .. }
                        | OpLogError::NotListed { .. }
                        | OpLogError::NotAChange { .. } => Vec::new(),
                    };
                    EntryStep::Failed(made, Box::new(PurgeFailure::Log(error)))
                })?;
            }
            if stop == Some(Stop::Entry) {
                return Err(EntryStep::Halted);
            }
            Ok(entries)
        };
        let outcome = self
            .refs_store()
            .purge_moves(&lock, &journal.moves(), self.objects(), step)
            .map_err(stopped(
                PurgeStep::Entry,
                &numbers(&written),
                PurgeFailure::Refs,
            ))?;
        let entry_failed = |entries: Vec<u64>, source| ApplyPurgeError::Stopped {
            step: PurgeStep::Entry,
            entries,
            source,
        };
        match outcome {
            Logged::Moved(entries) => written.extend(entries),
            Logged::Unchanged => unreachable!("a purge runs its step even if no pointer moves"),
            Logged::StepFailed(EntryStep::Failed(made, source)) => {
                let mut entries = numbers(&written);
                entries.extend(made);
                return Err(entry_failed(entries, source));
            }
            Logged::StepFailed(EntryStep::Halted) => {
                lock.leak();
                return Ok(None);
            }
            Logged::MoveFailed(done, error)
            | Logged::NotFlushed(done, error)
            | Logged::PartlyMoved(done, error) => {
                written.extend(done);
                return Err(entry_failed(
                    numbers(&written),
                    Box::new(PurgeFailure::Refs(error)),
                ));
            }
        }
        if stop == Some(Stop::Moves) {
            lock.leak();
            return Ok(None);
        }
        let entries = numbers(&written);

        // Step 7: the stubs.
        for number in &plan.entries {
            self.oplog().replace_with_stub(*number).map_err(stopped(
                PurgeStep::Stubs,
                &entries,
                PurgeFailure::Log,
            ))?;
        }
        if stop == Some(Stop::Stubs) {
            lock.leak();
            return Ok(None);
        }

        // Step 8: the delete set, the commits first, then the trees, then the blob, so that no
        // object that is left names one that is gone.
        let deleted = plan
            .commits
            .iter()
            .map(|commit| commit.object_id())
            .chain(plan.trees.iter().map(|tree| tree.object_id()))
            .chain([plan.blob.object_id()]);
        for id in deleted {
            self.objects()
                .remove(&id)
                .map_err(stopped(PurgeStep::Delete, &entries, store))?;
        }
        if stop == Some(Stop::Delete) {
            lock.leak();
            return Ok(None);
        }
        self.remove_journal(&entries)?;
        drop(lock);
        Ok(Some(written))
    }

    /// Removes the journal: the last step of a purge (`entries` are its `purge` entries) and of
    /// an abort (no entries). The error is that of [`not_removed`].
    fn remove_journal(&self, entries: &[u64]) -> Result<(), ApplyPurgeError> {
        journal::remove(&self.metadata_dir()).map_err(|error| not_removed(entries, error))
    }

    /// Makes the new trees and commits again from the old ones, checks that they are the ones
    /// that the journal names, and writes them.
    fn write_new_objects(&self, journal: &Journal) -> Result<(), PurgeFailure> {
        let plan = &journal.plan;
        let mut planner = Planner::new(self.objects(), plan.blob);
        for (old, expected) in &plan.rewrites {
            planner
                .commit(*old)
                .map_err(|error| PurgeFailure::Plan(Box::new(error)))?;
            let found = planner.commits[old];
            if found != *expected {
                return Err(PurgeFailure::Mismatch {
                    old: *old,
                    expected: *expected,
                    found,
                });
            }
        }
        for object in planner.new_objects(&plan.rewrites) {
            object.put(self.objects()).map_err(store)?;
        }
        Ok(())
    }
}

fn store(error: StoreError) -> PurgeFailure {
    PurgeFailure::Store(Box::new(error))
}

/// Maps the error of a step to [`ApplyPurgeError::Stopped`], with the numbers of the `purge`
/// entries that are written.
fn stopped<E>(
    step: PurgeStep,
    entries: &[u64],
    failure: impl FnOnce(E) -> PurgeFailure,
) -> impl FnOnce(E) -> ApplyPurgeError {
    let entries = entries.to_vec();
    move |error| ApplyPurgeError::Stopped {
        step,
        entries,
        source: Box::new(failure(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::io;

    use super::*;
    use crate::object::{Blob, Kind, Tree};
    use crate::oplog::{check_stubs, OperationName};
    use crate::refs::{encode_branch, BranchName, Head};
    use crate::repository::snapshot::pause::Stage;
    use crate::repository::{InitOptions, RestoreError, Restored, UpdateError};
    use crate::testutil::TempDir;
    use crate::{AllowAll, Decision};

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &str, _target: &str) -> Decision {
            Decision::Deny
        }
    }

    const SECRET: &str = "key=hunter2";

    fn actor() -> Actor {
        Actor::human("daniel").unwrap()
    }

    fn agent() -> Actor {
        Actor::agent("bot", "model", "session", "daniel").unwrap()
    }

    fn secret() -> ObjectId {
        Blob::new(SECRET).id().object_id()
    }

    fn repository(dir: &TempDir) -> Repository {
        Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap()
    }

    fn write(dir: &TempDir, path: &str, content: &str) {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn track(repository: &Repository, paths: &[&str]) {
        let paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();
        repository.track(&paths, &actor(), &AllowAll).unwrap();
    }

    fn commit(repository: &Repository, message: &str) -> CommitId {
        repository
            .commit(message, &actor(), &AllowAll)
            .unwrap()
            .commit
    }

    /// `a.txt` committed as "first", the secret in `config/key` committed as "second", and a
    /// change of `a.txt` committed as "third".
    fn history(dir: &TempDir, repository: &Repository) -> [CommitId; 3] {
        write(dir, "a.txt", "one");
        track(repository, &["a.txt"]);
        let first = commit(repository, "first");
        write(dir, "config/key", SECRET);
        track(repository, &["config/key"]);
        let second = commit(repository, "second");
        write(dir, "a.txt", "two");
        let third = commit(repository, "third");
        [first, second, third]
    }

    fn ids(repository: &Repository) -> BTreeSet<ObjectId> {
        repository.objects().ids().unwrap().into_iter().collect()
    }

    fn apply(repository: &Repository) -> Purged {
        repository
            .apply_purge(secret(), &actor(), &AllowAll)
            .unwrap()
    }

    fn stop_at(repository: &Repository, stop: Stop) {
        let stopped = repository
            .apply_purge_until(secret(), &actor(), &AllowAll, Some(stop))
            .unwrap();
        assert!(stopped.is_none());
    }

    fn format(dir: &TempDir) -> String {
        fs::read_to_string(dir.path().join(".stemma").join("format")).unwrap()
    }

    /// Sets the format back to version 2, as a repository from before the purge and the chain
    /// has it, so that a test sees whether something moves it.
    fn old_format(dir: &TempDir) {
        fs::write(dir.path().join(".stemma/format"), "stemma-repository 2\n").unwrap();
    }

    fn journal_exists(dir: &TempDir) -> bool {
        dir.path().join(".stemma").join("purge-pending").exists()
    }

    fn lock_exists(dir: &TempDir) -> bool {
        dir.path().join(".stemma").join("refs.lock").exists()
    }

    /// What every finished purge leaves: no tree in the store names the blob, and the blob, the
    /// old commits, and the old trees are gone; every entry that is kept names only commits that
    /// exist; the entries of the plan are stubs; the numbering has no gap; the pointers have the
    /// values after the purge; the journal and the lock are gone, and the format is 3.
    fn assert_purged(dir: &TempDir, repository: &Repository, purged: &Purged) {
        let store = repository.objects();
        let plan = purged.plan();
        assert!(!store.contains(&secret()).unwrap(), "the blob is gone");
        for commit in plan.deleted_commits() {
            assert!(!store.contains(&commit.object_id()).unwrap());
        }
        for tree in plan.deleted_trees() {
            assert!(!store.contains(&tree.object_id()).unwrap());
        }
        for id in ids(repository) {
            if store.kind_of(&id).unwrap() == Some(Kind::Tree) {
                let raw = store.get(&id).unwrap().unwrap();
                let tree = Tree::parse(raw.body()).unwrap();
                assert!(tree.entries().iter().all(|entry| entry.id() != secret()));
            }
        }
        let entries = repository.oplog().entries().unwrap();
        check_stubs(&entries).unwrap();
        let numbers: Vec<u64> = entries.iter().map(Entry::number).collect();
        let expected: Vec<u64> = (1..=entries.len() as u64).collect();
        assert_eq!(numbers, expected, "no gap");
        for entry in &entries {
            for commit in entry.commits() {
                assert!(store.contains(&commit.object_id()).unwrap(), "{commit}");
            }
        }
        for number in plan.entries() {
            let entry = repository.oplog().read(*number).unwrap();
            assert!(matches!(entry.record(), Record::Stub(_)), "{number}");
        }
        let purges: Vec<&Entry> = entries
            .iter()
            .filter(|entry| matches!(entry.record(), Record::Purge(_)))
            .collect();
        assert!(!purges.is_empty());
        if let Some(moved) = plan.working_copy() {
            assert_eq!(
                repository.refs().working_copy().unwrap(),
                *moved.after().unwrap()
            );
        }
        for (name, moved) in plan.branches() {
            assert_eq!(
                repository.refs().branch(name).unwrap(),
                moved.after().copied()
            );
        }
        assert!(!journal_exists(dir));
        assert!(!lock_exists(dir));
        assert_eq!(format(dir), "stemma-repository 4\n");
    }

    #[test]
    fn a_purge_removes_the_blob_from_the_history_and_the_log() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, third] = history(&dir, &repository);
        let before = repository.oplog().entries().unwrap().len() as u64;
        let purged = apply(&repository);
        assert_purged(&dir, &repository, &purged);
        assert_eq!(purged.snapshot(), None, "nothing changed since the commit");
        assert!(repository.objects().contains(&first.object_id()).unwrap());
        assert!(!repository.objects().contains(&second.object_id()).unwrap());
        assert!(!repository.objects().contains(&third.object_id()).unwrap());
        let [entry] = purged.entries() else {
            panic!("one entry, got {:?}", purged.entries());
        };
        assert_eq!(entry.number(), before + 1);
        let Record::Purge(purge) = entry.record() else {
            panic!("a purge entry");
        };
        assert_eq!(purge.commits(), purged.plan().rewrites().len() as u64);
        assert!(purged.plan().entries().iter().all(|n| purge.lists(*n)));
        // The new commits keep the messages and the file outside the secret.
        let main = BranchName::new("main").unwrap();
        let tip = repository.refs().branch(&main).unwrap().unwrap();
        let tip = super::super::super::snapshot::read_commit(repository.objects(), tip).unwrap();
        assert_eq!(tip.message(), "third");
        // The directory is not touched: the file stays, untracked.
        assert!(dir.path().join("config").join("key").exists());
    }

    #[test]
    fn a_stub_and_the_purge_entry_refuse_a_restore() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let purged = apply(&repository);
        let stub = purged.plan().entries()[0];
        let error = repository
            .restore(stub, true, &actor(), &AllowAll)
            .unwrap_err();
        assert!(matches!(error, RestoreError::Purged { .. }), "{error:?}");
        let entry = purged.entries()[0].number();
        let error = repository
            .restore(entry, true, &actor(), &AllowAll)
            .unwrap_err();
        assert!(matches!(error, RestoreError::IsPurge { .. }), "{error:?}");
    }

    #[test]
    fn a_blob_in_no_commit_is_deleted_with_an_entry_of_its_own() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        repository.objects().put_blob(&Blob::new(SECRET)).unwrap();
        let purged = apply(&repository);
        assert_purged(&dir, &repository, &purged);
        let Record::Purge(purge) = purged.entries()[0].record() else {
            panic!("a purge entry");
        };
        assert_eq!(purge.commits(), 0);
        assert!(purge.stubs().is_empty());
    }

    #[test]
    fn an_agent_is_refused_and_nothing_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        old_format(&dir);
        let before = (ids(&repository), repository.oplog().numbers().unwrap());
        let error = repository
            .apply_purge(secret(), &agent(), &AllowAll)
            .unwrap_err();
        assert!(
            matches!(error, ApplyPurgeError::Plan(PurgeError::Agent)),
            "{error:?}"
        );
        assert!(!error.pending());
        assert_eq!(
            (ids(&repository), repository.oplog().numbers().unwrap()),
            before
        );
        assert_eq!(format(&dir), "stemma-repository 2\n");
    }

    #[test]
    fn a_refusal_of_the_authorizer_writes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let before = ids(&repository);
        let error = repository
            .apply_purge(secret(), &actor(), &DenyAll)
            .unwrap_err();
        assert!(
            matches!(error, ApplyPurgeError::Plan(PurgeError::Update(_))),
            "{error:?}"
        );
        assert_eq!(ids(&repository), before);
    }

    #[test]
    fn a_change_that_holds_the_secret_is_saved_by_the_snapshot_and_purged() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, &["a.txt"]);
        commit(&repository, "first");
        // The secret is written into a tracked file and never saved by hand.
        write(&dir, "a.txt", SECRET);
        repository.objects().put_blob(&Blob::new(SECRET)).unwrap();
        let purged = apply(&repository);
        assert!(purged.snapshot().is_some(), "the snapshot saved the change");
        assert!(purged
            .plan()
            .entries()
            .contains(&purged.snapshot().unwrap().number()));
        assert_purged(&dir, &repository, &purged);
    }

    #[test]
    fn a_crash_after_each_step_is_finished_by_resume() {
        for stop in [
            Stop::Journal,
            Stop::Objects,
            Stop::Entry,
            Stop::Moves,
            Stop::Stubs,
            Stop::Delete,
        ] {
            let dir = TempDir::new();
            let repository = repository(&dir);
            history(&dir, &repository);
            stop_at(&repository, stop);
            assert!(journal_exists(&dir), "{stop:?}");
            assert!(lock_exists(&dir), "{stop:?}: the crash left the lock");
            let purged = repository.resume_purge(&actor(), &AllowAll).unwrap();
            assert_purged(&dir, &repository, &purged);
            let purges = repository
                .oplog()
                .entries()
                .unwrap()
                .into_iter()
                .filter(|entry| matches!(entry.record(), Record::Purge(_)))
                .count();
            assert_eq!(purges, 1, "{stop:?}: the entry is not written twice");
            // A second resume finds nothing to do.
            let error = repository.resume_purge(&actor(), &AllowAll).unwrap_err();
            assert!(
                matches!(error, ApplyPurgeError::NotPending { .. }),
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_resume_that_stops_again_is_finished_by_the_next() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        stop_at(&repository, Stop::Journal);
        let resumed = repository
            .resume_purge_until(&actor(), &AllowAll, Some(Stop::Entry))
            .unwrap();
        assert!(resumed.is_none());
        let purged = repository.resume_purge(&actor(), &AllowAll).unwrap();
        assert_purged(&dir, &repository, &purged);
    }

    #[test]
    fn every_writer_refuses_while_a_purge_is_pending() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, ..] = history(&dir, &repository);
        stop_at(&repository, Stop::Objects);
        let before = ids(&repository);
        write(&dir, "new.txt", "new");
        write(&dir, "a.txt", "changed");
        let pending = |error: &dyn std::fmt::Display| {
            assert!(
                error.to_string().contains("stemma purge --resume"),
                "{error}"
            );
        };
        let error = repository
            .track(&["new.txt".to_owned()], &actor(), &AllowAll)
            .unwrap_err();
        pending(&error);
        pending(&repository.snapshot(&actor(), &AllowAll).unwrap_err());
        pending(&repository.status(&actor(), &AllowAll).unwrap_err());
        pending(&repository.commit("more", &actor(), &AllowAll).unwrap_err());
        let error = repository
            .restore(1, true, &actor(), &AllowAll)
            .unwrap_err();
        pending(&error);
        // A move of a pointer is refused under the lock as well, also by the lock of the crash.
        let operation = OperationName::new("checkout").unwrap();
        let error = repository
            .set_head(&operation, &Head::Detached(first), &actor(), &AllowAll)
            .unwrap_err();
        assert!(
            matches!(error, UpdateError::Refs(RefError::PurgePending { .. })),
            "{error:?}"
        );
        assert_eq!(ids(&repository), before, "no writer wrote an object");
        // A second purge is refused too.
        let error = repository
            .apply_purge(secret(), &actor(), &AllowAll)
            .unwrap_err();
        assert!(
            matches!(error, ApplyPurgeError::Pending { .. }),
            "{error:?}"
        );
        assert!(error.pending());
    }

    #[test]
    fn an_abort_before_the_entry_leaves_the_repository_as_it_was() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let before = (
            ids(&repository),
            repository.oplog().entries().unwrap(),
            repository.refs().working_copy().unwrap(),
            repository.refs().branches().unwrap(),
        );
        stop_at(&repository, Stop::Objects);
        assert_ne!(ids(&repository), before.0, "the new objects are written");
        let aborted = repository.abort_purge(&actor(), &AllowAll).unwrap();
        assert!(!aborted.removed().is_empty());
        let after = (
            ids(&repository),
            repository.oplog().entries().unwrap(),
            repository.refs().working_copy().unwrap(),
            repository.refs().branches().unwrap(),
        );
        assert_eq!(after, before);
        assert!(!journal_exists(&dir));
        assert!(!lock_exists(&dir));
        assert_eq!(
            format(&dir),
            "stemma-repository 4\n",
            "a format is never lowered"
        );
        // The repository can be written again, and purged.
        write(&dir, "a.txt", "three");
        commit(&repository, "fourth");
        assert_purged(&dir, &repository, &apply(&repository));
    }

    #[test]
    fn an_abort_keeps_an_object_that_the_store_had_before() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        // The tree of "first" is the tree that the rewrite of "second" makes.
        let [first, ..] = history(&dir, &repository);
        let tree = super::super::super::snapshot::read_commit(repository.objects(), first)
            .unwrap()
            .tree();
        stop_at(&repository, Stop::Objects);
        let journal = journal::read(&repository.metadata_dir()).unwrap().unwrap();
        assert!(!journal.new.contains(&tree.object_id()));
        repository.abort_purge(&actor(), &AllowAll).unwrap();
        assert!(repository.objects().contains(&tree.object_id()).unwrap());
    }

    #[test]
    fn an_abort_after_the_entry_is_refused_and_resume_finishes() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        stop_at(&repository, Stop::Entry);
        let error = repository.abort_purge(&actor(), &AllowAll).unwrap_err();
        assert!(
            matches!(error, ApplyPurgeError::PastNoReturn { .. }),
            "{error:?}"
        );
        assert!(error.pending());
        assert!(error.to_string().contains("cannot be undone"));
        let purged = repository.resume_purge(&actor(), &AllowAll).unwrap();
        assert_purged(&dir, &repository, &purged);
    }

    #[test]
    fn a_pointer_at_neither_value_stops_the_resume_and_is_named() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, ..] = history(&dir, &repository);
        stop_at(&repository, Stop::Objects);
        // A foreign change after the lock was removed by hand.
        let branch = dir.path().join(".stemma/refs/heads/main");
        fs::write(&branch, encode_branch(&first)).unwrap();
        let entries = repository.oplog().numbers().unwrap();
        let error = repository.resume_purge(&actor(), &AllowAll).unwrap_err();
        match &error {
            ApplyPurgeError::Stopped {
                step: PurgeStep::Entry,
                entries,
                source,
            } => {
                let PurgeFailure::Refs(RefError::PurgeStale { pointer }) = &**source else {
                    panic!("expected a stale pointer, got {source:?}");
                };
                assert_eq!(pointer, "main");
                assert!(entries.is_empty(), "no entry, so the abort is offered");
            }
            other => panic!("expected a stale pointer, got {other:?}"),
        }
        assert!(error.to_string().contains("`main` is at neither"));
        assert_eq!(repository.oplog().numbers().unwrap(), entries, "no entry");
        // Nothing moved, so the purge can still be undone.
        repository.abort_purge(&actor(), &AllowAll).unwrap();
        let main = BranchName::new("main").unwrap();
        assert_eq!(repository.refs().branch(&main).unwrap(), Some(first));
    }

    #[test]
    fn resume_and_abort_without_a_journal_are_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let error = repository.resume_purge(&actor(), &AllowAll).unwrap_err();
        assert!(
            matches!(error, ApplyPurgeError::NotPending { .. }),
            "{error:?}"
        );
        assert!(!error.pending());
        let error = repository.abort_purge(&actor(), &AllowAll).unwrap_err();
        assert!(
            matches!(error, ApplyPurgeError::NotPending { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn an_agent_may_not_resume_or_abort() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        stop_at(&repository, Stop::Journal);
        for error in [
            repository.resume_purge(&agent(), &AllowAll).unwrap_err(),
            repository
                .abort_purge(&agent(), &AllowAll)
                .map(|_| ())
                .unwrap_err(),
        ] {
            assert!(
                matches!(error, ApplyPurgeError::Plan(PurgeError::Agent)),
                "{error:?}"
            );
        }
        assert!(journal_exists(&dir));
    }

    #[test]
    fn a_damaged_journal_stops_the_resume() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        stop_at(&repository, Stop::Journal);
        fs::write(journal::path(&repository.metadata_dir()), "junk").unwrap();
        let error = repository.resume_purge(&actor(), &AllowAll).unwrap_err();
        assert!(
            matches!(&error, ApplyPurgeError::Unreadable(source)
                if matches!(**source, JournalError::Malformed { .. })),
            "{error:?}"
        );
        assert!(error.pending() && error.made());
        assert!(
            !error.to_string().contains("finishes it"),
            "neither mode can go on: {error}"
        );
        let error = repository.abort_purge(&actor(), &AllowAll).unwrap_err();
        assert!(matches!(error, ApplyPurgeError::Unreadable(_)), "{error:?}");
    }

    #[test]
    fn a_second_purge_purges_another_secret_over_the_stubs_of_the_first() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        apply(&repository);
        write(&dir, "other", "token=swordfish");
        track(&repository, &["other"]);
        commit(&repository, "fourth");
        let other = Blob::new("token=swordfish").id().object_id();
        let purged = repository.apply_purge(other, &actor(), &AllowAll).unwrap();
        assert!(!repository.objects().contains(&other).unwrap());
        assert!(!purged.plan().rewrites().is_empty());
        // `assert_purged` checks the numbering and that every kept entry names existing commits;
        // the entry of the first purge is kept and is no stub.
        assert_purged(&dir, &repository, &purged);
        let purges = repository
            .oplog()
            .entries()
            .unwrap()
            .into_iter()
            .filter(|entry| matches!(entry.record(), Record::Purge(_)))
            .count();
        assert_eq!(purges, 2);
    }

    #[test]
    fn a_detached_head_on_a_rewritten_commit_moves() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [_, second, _] = history(&dir, &repository);
        let operation = OperationName::new("checkout").unwrap();
        repository
            .set_head(&operation, &Head::Detached(second), &actor(), &AllowAll)
            .unwrap();
        let purged = apply(&repository);
        let moved = purged.plan().head().unwrap();
        assert_eq!(repository.refs().head().unwrap(), *moved.after().unwrap());
        assert_purged(&dir, &repository, &purged);
    }

    #[test]
    fn a_purge_entry_that_stopped_after_a_part_is_finished_with_the_rest() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        stop_at(&repository, Stop::Objects);
        let journal = journal::read(&repository.metadata_dir()).unwrap().unwrap();
        let stubs = journal.plan.entries().to_vec();
        assert!(stubs.len() > 1, "{stubs:?}");
        // The first part of a split entry: the count and some of the stubs.
        let first = Purge::new(
            0,
            actor(),
            journal.plan.rewrites().len() as u64,
            &stubs[..1],
        )
        .unwrap();
        repository.oplog().append_purge(first).unwrap();
        let purged = repository.resume_purge(&actor(), &AllowAll).unwrap();
        assert_purged(&dir, &repository, &purged);
        let parts: Vec<Purge> = purged
            .entries()
            .iter()
            .map(|entry| match entry.record() {
                Record::Purge(purge) => purge.clone(),
                other => panic!("a purge entry, got {other:?}"),
            })
            .collect();
        assert_eq!(parts.len(), 2, "the first part and the rest");
        assert_eq!(parts[0].commits(), journal.plan.rewrites().len() as u64);
        assert_eq!(parts[1].commits(), 0, "the count is not written twice");
        assert!(stubs[1..].iter().all(|number| parts[1].lists(*number)));
        assert!(!parts[1].lists(stubs[0]));
    }

    #[test]
    fn a_working_copy_at_neither_value_stops_the_resume_and_is_named() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, ..] = history(&dir, &repository);
        stop_at(&repository, Stop::Objects);
        fs::write(
            dir.path().join(".stemma/working-copy"),
            encode_branch(&first),
        )
        .unwrap();
        let error = repository.resume_purge(&actor(), &AllowAll).unwrap_err();
        assert!(
            matches!(
                &error,
                ApplyPurgeError::Stopped { source, .. }
                    if matches!(&**source, PurgeFailure::Refs(RefError::PurgeStale { pointer })
                        if pointer == "working-copy")
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_detached_head_at_neither_value_stops_the_resume_and_is_named() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, _] = history(&dir, &repository);
        let operation = OperationName::new("checkout").unwrap();
        repository
            .set_head(&operation, &Head::Detached(second), &actor(), &AllowAll)
            .unwrap();
        stop_at(&repository, Stop::Objects);
        fs::write(
            dir.path().join(".stemma/HEAD"),
            Head::Detached(first).encode(),
        )
        .unwrap();
        let error = repository.resume_purge(&actor(), &AllowAll).unwrap_err();
        assert!(
            matches!(
                &error,
                ApplyPurgeError::Stopped { source, .. }
                    if matches!(&**source, PurgeFailure::Refs(RefError::PurgeStale { pointer })
                        if pointer == "HEAD")
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_held_lock_stops_the_purge_before_the_format_and_the_journal() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        old_format(&dir);
        // Another process changes the references. The snapshot of the purge takes the lock
        // first, before its first object, and is refused.
        fs::write(dir.path().join(".stemma/refs.lock"), "other").unwrap();
        let before = ids(&repository);
        let error = repository
            .apply_purge(secret(), &actor(), &AllowAll)
            .unwrap_err();
        assert!(
            matches!(
                error,
                ApplyPurgeError::Snapshot(SnapshotError::Update(UpdateError::Refs(
                    RefError::Locked { .. }
                )))
            ),
            "{error:?}"
        );
        assert!(!error.pending());
        assert!(!error.made());
        assert!(!journal_exists(&dir));
        assert_eq!(format(&dir), "stemma-repository 2\n");
        assert_eq!(ids(&repository), before, "no new object");
    }

    #[test]
    fn a_failure_after_the_snapshot_names_its_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        write(&dir, "a.txt", "changed");
        // A damaged object that only the plan under the lock looks at.
        let damaged = Blob::new("never stored").id().object_id();
        let path = repository.objects().object_path(&damaged);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "junk").unwrap();
        let error = repository
            .apply_purge(secret(), &actor(), &AllowAll)
            .unwrap_err();
        let ApplyPurgeError::AfterSnapshot { snapshot, source } = &error else {
            panic!("expected a failure after the snapshot, got {error:?}");
        };
        assert!(matches!(**source, ApplyPurgeError::Plan(_)), "{source:?}");
        assert!(!error.pending());
        assert!(error.to_string().ends_with(&format!(
            "(the working copy was saved first as entry {})",
            snapshot.number()
        )));
        assert!(!journal_exists(&dir));

        // What stays after the snapshot decides the code: a journal, or a purge not flushed.
        let after = |source: ApplyPurgeError| ApplyPurgeError::AfterSnapshot {
            snapshot: snapshot.clone(),
            source: Box::new(source),
        };
        let stopped = after(ApplyPurgeError::Stopped {
            step: PurgeStep::Objects,
            entries: Vec::new(),
            source: Box::new(PurgeFailure::Refs(RefError::Locked {
                path: dir.path().join(".stemma/refs.lock"),
            })),
        });
        assert!(stopped.pending() && stopped.made());
        let not_durable = after(not_removed(&[7], not_flushed(&dir)));
        assert!(!not_durable.pending() && not_durable.made());
    }

    fn not_flushed(dir: &TempDir) -> JournalError {
        JournalError::NotFlushed {
            path: dir.path().join(".stemma"),
            source: io::Error::other("no flush"),
        }
    }

    #[test]
    fn a_journal_that_is_gone_but_not_flushed_names_the_mode_that_finishes_it() {
        let dir = TempDir::new();
        let purged = not_removed(&[7], not_flushed(&dir));
        assert!(
            matches!(purged, ApplyPurgeError::NotDurable { .. }),
            "{purged:?}"
        );
        let message = purged.to_string();
        assert!(message.contains("entry 7"), "{message}");
        assert!(
            message.contains("--resume") && !message.contains("--abort"),
            "{message}"
        );
        let aborted = not_removed(&[], not_flushed(&dir));
        let message = aborted.to_string();
        assert!(
            message.contains("--abort") && !message.contains("--resume"),
            "{message}"
        );

        // A journal that could not be removed stays: the purge stopped at its end.
        let stays = not_removed(
            &[7],
            JournalError::Io {
                operation: "remove the journal",
                path: dir.path().join(".stemma/purge-pending"),
                source: io::Error::other("denied"),
            },
        );
        assert!(
            matches!(
                &stays,
                ApplyPurgeError::Stopped { step: PurgeStep::Finish, entries, .. }
                    if entries == &[7]
            ),
            "{stays:?}"
        );
        assert!(stays.pending());
    }

    #[test]
    fn a_new_commit_that_is_missing_after_the_entry_stops_the_resume() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        stop_at(&repository, Stop::Entry);
        let journal = journal::read(&repository.metadata_dir()).unwrap().unwrap();
        let (_, new) = journal.plan.rewrites()[0];
        repository.objects().remove(&new.object_id()).unwrap();
        let error = repository.resume_purge(&actor(), &AllowAll).unwrap_err();
        match &error {
            ApplyPurgeError::Stopped {
                step: PurgeStep::Objects,
                entries,
                source,
            } => {
                assert!(
                    matches!(**source, PurgeFailure::Missing(missing) if missing == new),
                    "{source:?}"
                );
                assert_eq!(entries.len(), 1, "the entry is named");
            }
            other => panic!("expected a missing commit, got {other:?}"),
        }
        assert!(error.to_string().contains("only `stemma purge --resume`"));
        assert!(!error.to_string().contains("--abort"));
    }

    #[test]
    fn an_abort_names_only_the_objects_that_it_removed() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        // Before the new objects: the journal names them, but none is written yet.
        stop_at(&repository, Stop::Journal);
        let aborted = repository.abort_purge(&actor(), &AllowAll).unwrap();
        assert!(aborted.removed().is_empty(), "{:?}", aborted.removed());
    }

    /// The objects that the pointers reach, through parents and tree entries, are all in the
    /// store: no pointer names a commit with a dangling reference.
    fn assert_reachable_objects_exist(repository: &Repository) {
        let store = repository.objects();
        let refs = repository.refs();
        let mut todo: Vec<ObjectId> = vec![refs.working_copy().unwrap().object_id()];
        todo.extend(
            refs.branches()
                .unwrap()
                .iter()
                .map(|(_, id)| id.object_id()),
        );
        if let Head::Detached(commit) = refs.head().unwrap() {
            todo.push(commit.object_id());
        }
        let mut seen = BTreeSet::new();
        while let Some(id) = todo.pop() {
            if !seen.insert(id) {
                continue;
            }
            let raw = store.get(&id).unwrap();
            let raw = raw.unwrap_or_else(|| panic!("{id} is reachable but missing"));
            match store.kind_of(&id).unwrap() {
                Some(Kind::Commit) => {
                    let commit = crate::object::Commit::parse(raw.body()).unwrap();
                    todo.push(commit.tree().object_id());
                    todo.extend(commit.parents().iter().map(|parent| parent.object_id()));
                }
                Some(Kind::Tree) => {
                    let tree = Tree::parse(raw.body()).unwrap();
                    todo.extend(tree.entries().iter().map(|entry| entry.id()));
                }
                _ => {}
            }
        }
    }

    /// Runs a purge of the secret when this thread next reaches `stage`, while the writer holds
    /// the lock, as a second process in the same repository would, and returns its error.
    fn purge_at_the_stop_point(
        dir: &TempDir,
        stage: Stage,
    ) -> std::rc::Rc<std::cell::RefCell<Option<String>>> {
        let result = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (path, seen) = (dir.path().to_path_buf(), result.clone());
        crate::repository::snapshot::pause::at_next(stage, move || {
            let other = Repository::open(&path).unwrap();
            let error = other
                .apply_purge(secret(), &actor(), &AllowAll)
                .unwrap_err();
            assert!(
                matches!(
                    &error,
                    ApplyPurgeError::Snapshot(SnapshotError::Update(UpdateError::Refs(
                        RefError::Locked { .. }
                    )))
                ),
                "{error:?}"
            );
            assert!(!error.pending());
            assert!(!journal_exists_at(&path), "the purge wrote no journal");
            *seen.borrow_mut() = Some(error.to_string());
        });
        result
    }

    fn journal_exists_at(path: &std::path::Path) -> bool {
        path.join(".stemma").join("purge-pending").exists()
    }

    #[test]
    fn a_purge_cannot_overtake_a_snapshot_that_saved_the_secret_again() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        // The writer saves the secret again (the blob exists, so its save is no change) and
        // another file that changed, so that it moves the working-copy commit.
        write(&dir, "a.txt", "changed again");
        let refused = purge_at_the_stop_point(&dir, Stage::Saved);
        let entry = repository.snapshot(&actor(), &AllowAll).unwrap();
        assert!(
            entry.is_some(),
            "the snapshot moved the working-copy commit"
        );
        assert!(
            refused.borrow().is_some(),
            "the purge ran at the stop point"
        );
        // The writer moved the repository to the format of the chain; the purge left it alone.
        assert_eq!(format(&dir), "stemma-repository 4\n");
        assert!(!lock_exists(&dir));
        assert!(repository.objects().contains(&secret()).unwrap());
        assert_reachable_objects_exist(&repository);
        // Run again afterwards, the purge sees the snapshot and removes the secret from it too.
        let purged = apply(&repository);
        assert_purged(&dir, &repository, &purged);
        assert_reachable_objects_exist(&repository);
    }

    #[test]
    fn a_purge_cannot_overtake_a_commit_between_its_snapshot_and_its_moves() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        write(&dir, "a.txt", "fourth");
        let refused = purge_at_the_stop_point(&dir, Stage::Moves);
        let committed = repository.commit("fourth", &actor(), &AllowAll).unwrap();
        assert!(
            refused.borrow().is_some(),
            "the purge ran at the stop point"
        );
        assert!(!lock_exists(&dir));
        let refs = repository.refs();
        let main = BranchName::new("main").unwrap();
        assert_eq!(refs.branch(&main).unwrap(), Some(committed.commit));
        assert_eq!(refs.working_copy().unwrap(), committed.working_copy);
        assert_reachable_objects_exist(&repository);
        let purged = apply(&repository);
        assert_purged(&dir, &repository, &purged);
    }

    #[test]
    fn a_purge_cannot_overtake_a_restore_between_its_snapshot_and_its_moves() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let last = repository
            .oplog()
            .numbers()
            .unwrap()
            .into_iter()
            .max()
            .unwrap();
        write(&dir, "a.txt", "unsaved");
        let refused = purge_at_the_stop_point(&dir, Stage::Moves);
        let restored = repository.restore(last, true, &actor(), &AllowAll).unwrap();
        assert!(matches!(restored, Restored::Done { .. }), "{restored:?}");
        assert!(
            refused.borrow().is_some(),
            "the purge ran at the stop point"
        );
        assert!(!lock_exists(&dir));
        assert_reachable_objects_exist(&repository);
        let purged = apply(&repository);
        assert_purged(&dir, &repository, &purged);
    }

    #[test]
    fn a_writer_after_a_stopped_purge_writes_no_object_and_the_abort_is_clean() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        stop_at(&repository, Stop::Objects);
        let before = ids(&repository);
        write(&dir, "a.txt", "changed");
        let error = repository.snapshot(&actor(), &AllowAll).unwrap_err();
        assert!(
            matches!(
                error,
                SnapshotError::Update(UpdateError::Refs(RefError::PurgePending { .. }))
            ),
            "{error:?}"
        );
        assert_eq!(ids(&repository), before, "the writer wrote no object");
        repository.abort_purge(&actor(), &AllowAll).unwrap();
        assert!(!journal_exists(&dir));
        assert!(!lock_exists(&dir));
        assert_reachable_objects_exist(&repository);
        repository.snapshot(&actor(), &AllowAll).unwrap();
        assert_reachable_objects_exist(&repository);
    }

    #[test]
    fn a_snapshot_holds_the_lock_from_its_objects_to_its_move_and_releases_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        write(&dir, "a.txt", "changed");
        let path = dir.path().to_path_buf();
        crate::repository::snapshot::pause::at_next(Stage::Saved, move || {
            assert!(path.join(".stemma").join("refs.lock").exists());
        });
        repository.snapshot(&actor(), &AllowAll).unwrap();
        assert!(!lock_exists(&dir));
        // A snapshot that fails after its lock releases it too.
        fs::remove_file(dir.path().join("a.txt")).unwrap();
        let error = repository
            .track(&["a.txt".to_owned()], &actor(), &AllowAll)
            .unwrap_err();
        assert!(error.to_string().contains("a.txt"), "{error}");
        assert!(!lock_exists(&dir));
    }
}
