//! Naming the working-copy commit (ADR-010).
//!
//! [`Repository::commit`] gives the working-copy commit its message, moves the branch that HEAD
//! names to it, and starts a new, empty working-copy commit on top. Both pointers move in one
//! operation, so the log has one entry for them (ADR-011).

use std::error::Error;
use std::fmt;
use std::io;

use super::snapshot::read_commit;
use super::{now, Repository, SnapshotError, UpdateError};
use crate::change::{ChangeId, GenerateError};
use crate::index::Index;
use crate::object::{Commit, CommitId, TreeId};
use crate::oplog::{outcomes, Change, Current, Entry, Moved, OpLogError, OperationName, Outcome};
use crate::refs::{BranchMove, BranchName, Head, Logged, RefError, WorkingCopyMove, WriteLock};
use crate::store::{ObjectStore, StoreError};
use crate::{Actor, Authorizer};

/// The operation that the log records for a commit.
const OPERATION: &str = "commit";

/// What [`Repository::commit`] did.
#[derive(Debug)]
#[must_use]
#[non_exhaustive]
pub struct Committed {
    /// The commit that got the message and that the branch names now. It is the working-copy
    /// commit as it was, with the same change ID.
    pub commit: CommitId,
    /// The new working-copy commit: a child of `commit`, with a new change ID. Its tree is that of
    /// `commit`, so it is empty, except when `resumed` is set and a change was made since the
    /// stop: it then has the tree of the snapshot of that change.
    pub working_copy: CommitId,
    /// The branch that moved to `commit`.
    pub branch: BranchName,
    /// The entry of the log for the moves.
    pub entry: Entry,
    /// The call finished an earlier commit that had stopped between its two moves, and the
    /// message of the call was not used: `commit` carries the message of the earlier call.
    pub resumed: bool,
    /// The moves happened, but a directory could not be flushed, so they may not survive a crash.
    /// The caller reports it after it has shown the result, and does not repeat the commit.
    pub late: Option<RefError>,
}

impl Repository {
    /// Names the working-copy commit and starts a new one (ADR-010).
    ///
    /// The message must have a character that is not white space. HEAD must name a branch; it is
    /// read for that, and the authorizer is asked for the operation `commit` on the branch before
    /// anything else is read or written. The tracked files are then saved first, as
    /// [`snapshot`](Self::snapshot) does (and the authorizer is asked for that operation too), so
    /// that a change since the last command is part of the commit. That is an operation of its
    /// own and has its own entry. Then the working-copy commit gets `message`, with its parents and
    /// its change ID, by the author `actor` at the time now. The branch that HEAD names moves to
    /// it, and a new working-copy commit takes its place: a child of the named commit, with the
    /// same tree, an empty message, and a change ID of its own. The same tree is what makes it
    /// empty, since the tracked set is the tree (ADR-010).
    ///
    /// The lock of the references is taken after the authorizer agreed and before the snapshot (a
    /// lock that is held, or the journal of a purge, is a [`CommitError::Update`] before anything
    /// is written), and held to the last move, so that no purge comes in between and deletes an
    /// object that the commit names (ADR-014). The branch moves first and the working-copy commit
    /// second, with one entry. If a stop or a failure comes between the two, the branch has the
    /// named commit and the working-copy commit is still the old one. The next call finds that
    /// state (the tip of the branch has the parents and the change ID of the working-copy commit,
    /// and a message, which the working-copy commit has not, and the newest entry of a commit
    /// moved the branch there but not the working-copy commit) and finishes the first commit: it
    /// starts the new working-copy commit and moves only that pointer, with an entry of its own.
    /// That commit takes the tree of the working-copy commit that the call finds, so a change
    /// since the stop, saved by the snapshot of this call, is kept in it and is not part of the
    /// finished commit.
    /// The result says [`resumed`](Committed::resumed), and the message of the second call is not
    /// used. A working-copy commit that `op restore` put back to an older version of itself looks
    /// the same in the commits, but its entry took effect; it is [`CommitError::BranchMoved`].
    ///
    /// # Errors
    ///
    /// Returns [`CommitError::EmptyMessage`] and [`CommitError::DetachedHead`] (HEAD names a
    /// commit and not a branch) before anything is written, and [`CommitError::NothingToCommit`]
    /// if the tree is that of the parent (or empty, for a first commit) and
    /// [`CommitError::BranchMoved`] if the branch is not at the parent of the working-copy commit,
    /// so that the commit would drop what the branch has; these two leave only the snapshot. Every
    /// failure after a snapshot that wrote an entry is [`CommitError::AfterSnapshot`], which names
    /// that entry and wraps the failure; [`CommitError::failure`] returns the failure itself.
    /// [`CommitError::Update`] covers a refusal of the authorizer for `commit`, a pointer that
    /// another process moved first (`UpdateError::Refs` with [`RefError::Stale`] or
    /// [`RefError::StaleWorkingCopy`]: repeat the commit), and a failure of the log or the
    /// references; [`UpdateError::entry`] and [`UpdateError::pointer_moved`] say what was done. A
    /// refusal for the snapshot and every other failure of it is [`CommitError::Snapshot`].
    /// [`CommitError::Refs`] covers reading HEAD and the branch, [`CommitError::Tip`] reading the
    /// commit at the tip of the branch, [`CommitError::Store`] storing the new commits,
    /// [`CommitError::ChangeId`] the random source, and [`CommitError::Log`] reading the log to
    /// tell a stopped commit from a restored working copy.
    pub fn commit(
        &self,
        message: &str,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Committed, CommitError> {
        if message.trim().is_empty() {
            return Err(CommitError::EmptyMessage);
        }
        let operation = OperationName::new(OPERATION).expect("`commit` is a valid operation name");
        let name = match self.refs().head().map_err(CommitError::Refs)? {
            Head::Branch(name) => name,
            Head::Detached(commit) => return Err(CommitError::DetachedHead(commit)),
        };
        let allowed = self.authorize(actor, authorizer, &operation, name.as_str())?;
        // One lock from the first object of the snapshot to the last move, so that no purge
        // comes in between and deletes an object that the commit names (ADR-014).
        let lock = self
            .refs_store()
            .lock_writer(&allowed)
            .map_err(|error| CommitError::Update(UpdateError::Refs(error)))?;
        let saved = self.snapshot_locked(&lock, actor, authorizer)?;
        #[cfg(test)]
        super::snapshot::pause::reached(super::snapshot::pause::Stage::Moves);
        // A failure after the snapshot names its entry: the commit was not made, but the snapshot
        // was, and the working-copy commit moved to it.
        self.commit_locked(&lock, &operation, name, message, actor)
            .map_err(|error| match saved {
                Some(snapshot) => CommitError::AfterSnapshot {
                    snapshot: Box::new(snapshot),
                    source: Box::new(error),
                },
                None => error,
            })
    }

    /// The part of [`commit`](Self::commit) after the snapshot, under `lock`.
    fn commit_locked(
        &self,
        lock: &WriteLock,
        operation: &OperationName,
        name: BranchName,
        message: &str,
        actor: &Actor,
    ) -> Result<Committed, CommitError> {
        let current = self.refs().working_copy().map_err(CommitError::Refs)?;
        let working = read_commit(self.objects(), current)?;
        let tip = self.refs().branch(&name).map_err(CommitError::Refs)?;
        if working.parents() != tip.as_slice() {
            let Some(stopped) = tip else {
                return Err(CommitError::BranchMoved {
                    branch: name,
                    parents: working.parents().to_vec(),
                    tip,
                });
            };
            let named = read_commit(self.objects(), stopped).map_err(CommitError::Tip)?;
            let stopped_here = named.parents() == working.parents()
                && named.change() == working.change()
                && !named.message().is_empty()
                && self.stopped_by_log(&name, stopped, current)?;
            if stopped_here {
                return self.finish(lock, operation, name, &named, &working, actor);
            }
            return Err(CommitError::BranchMoved {
                branch: name,
                parents: working.parents().to_vec(),
                tip,
            });
        }
        let before = match tip {
            Some(parent) => read_commit(self.objects(), parent)
                .map_err(CommitError::Tip)?
                .tree(),
            None => Index::new()
                .write_tree(self.objects())
                .map_err(SnapshotError::WriteTree)?,
        };
        if working.tree() == before {
            return Err(CommitError::NothingToCommit);
        }

        let named = working.rewrite(working.tree(), actor.clone(), now(), message);
        let named_id = self.put(&named)?;
        let (_, next_id) = self.start_after(named.tree(), named_id, actor)?;

        let outcome = self
            .refs
            .update_branch_and_working_copy_after(
                lock,
                &BranchMove {
                    name: &name,
                    expected: tip,
                    new: named_id,
                },
                &WorkingCopyMove {
                    expected: current,
                    new: next_id,
                },
                &self.objects,
                || {
                    let change = Change::new(
                        now(),
                        actor.clone(),
                        operation.clone(),
                        None,
                        Some(Moved::from_to(Some(current), Some(next_id))),
                        vec![(name.clone(), Moved::from_to(tip, Some(named_id)))],
                    )
                    .map_err(OpLogError::Entry)?;
                    self.append(lock, &change)
                },
            )
            .map_err(UpdateError::Refs)?;
        let (entry, late) = match outcome {
            Logged::Moved(entry) => (entry, None),
            Logged::NotFlushed(entry, source) => (entry, Some(source)),
            other => return Err(CommitError::Update(into_error(other))),
        };
        Ok(Committed {
            commit: named_id,
            working_copy: next_id,
            branch: name,
            entry,
            resumed: false,
            late,
        })
    }

    /// Finishes a commit that stopped after the move of the branch: `named` is at the tip of the
    /// branch, and `working` is the working-copy commit. `working` is either the commit that `named`
    /// was made from or a snapshot of a change since the stop. The new working-copy commit takes
    /// the tree of `working`, so that the snapshot is kept.
    fn finish(
        &self,
        lock: &WriteLock,
        operation: &OperationName,
        branch: BranchName,
        named: &Commit,
        working: &Commit,
        actor: &Actor,
    ) -> Result<Committed, CommitError> {
        let named_id = named.id();
        let current = working.id();
        let (_, next_id) = self.start_after(working.tree(), named_id, actor)?;
        let (entry, late) = match self.move_working_copy(lock, operation, current, next_id, actor) {
            Ok(Some(entry)) => (entry, None),
            Ok(None) => unreachable!("the working-copy commit changes: its parents differ"),
            Err(UpdateError::NotDurable { entry, source }) => (*entry, Some(source)),
            Err(error) => return Err(CommitError::Update(error)),
        };
        Ok(Committed {
            commit: named_id,
            working_copy: next_id,
            branch,
            entry,
            resumed: true,
            late,
        })
    }

    /// Whether the log says that a commit stopped between its two moves: the newest entry of a
    /// `commit` moved `branch` to `tip` and the working-copy commit, and [`outcomes`] reads it as
    /// not taken. The commits alone cannot tell such a stop from a working-copy commit that
    /// `op restore` put back to an older version of itself (ADR-013). The next move of the
    /// working-copy commit tells them apart: after a stop it starts from the old value of the
    /// entry (a snapshot of a change since the stop, or nothing yet), after a restore from its
    /// new value, so the entry took effect. An outcome that the log does not decide (behind a
    /// stub) is no stop either, so that the message of the call is never dropped on a guess.
    ///
    /// The caller holds the lock of the references, so no writer is between its entry and its
    /// moves, and the pointers are settled. The entry of a commit moves only `branch` and the
    /// working-copy commit, so `now` holds only those, and HEAD, which [`Current`] needs. Other
    /// branches are left out, so only the outcome at `position` is meaningful.
    fn stopped_by_log(
        &self,
        branch: &BranchName,
        tip: CommitId,
        current: CommitId,
    ) -> Result<bool, CommitError> {
        let entries = self.oplog().entries().map_err(CommitError::Log)?;
        let Some((position, change)) = entries.iter().enumerate().rev().find_map(|(at, entry)| {
            entry
                .change()
                .filter(|change| change.operation().as_str() == OPERATION)
                .map(|change| (at, change))
        }) else {
            return Ok(false);
        };
        let moved_branch = change
            .branches()
            .iter()
            .any(|(name, moved)| name == branch && moved.after() == Some(&tip));
        if !moved_branch || change.working_copy().is_none() {
            return Ok(false);
        }
        let now = Current::new(Head::Branch(branch.clone()), [(branch.clone(), tip)], true)
            .with_working_copy(Some(current));
        Ok(outcomes(&entries, Some(&now))[position] == Outcome::NotTaken)
    }

    /// Makes and stores the working-copy commit that follows `named_id`: the given tree, an empty
    /// message, and a change ID of its own.
    fn start_after(
        &self,
        tree: TreeId,
        named_id: CommitId,
        actor: &Actor,
    ) -> Result<(Commit, CommitId), CommitError> {
        let next = Commit::new(
            tree,
            vec![named_id],
            ChangeId::generate().map_err(CommitError::ChangeId)?,
            actor.clone(),
            now(),
            "",
        )
        .expect("a single parent is not repeated");
        let id = self.put(&next)?;
        Ok((next, id))
    }

    fn put(&self, commit: &Commit) -> Result<CommitId, CommitError> {
        self.objects()
            .put_commit(commit)
            .map_err(|error| CommitError::Store(Box::new(error)))
    }
}

/// The error of an outcome that is no success.
fn into_error(outcome: Logged<Entry, OpLogError>) -> UpdateError {
    match outcome {
        Logged::StepFailed(error) => UpdateError::Log(error),
        Logged::MoveFailed(entry, source) => UpdateError::MoveFailed {
            entry: Box::new(entry),
            source,
        },
        Logged::PartlyMoved(entry, source) => UpdateError::PartlyMoved {
            entry: Box::new(entry),
            source,
        },
        Logged::NotFlushed(entry, source) => UpdateError::NotDurable {
            entry: Box::new(entry),
            source,
        },
        Logged::Unchanged => unreachable!("a commit moves both pointers"),
        Logged::Moved(_) => unreachable!("a move that took place is no error"),
    }
}

/// Reasons why [`Repository::commit`] fails.
#[derive(Debug)]
#[non_exhaustive]
pub enum CommitError {
    /// The message is empty or has only white space. There is no editor in phase 1.
    EmptyMessage,
    /// HEAD names a commit and not a branch, so there is no branch to move.
    DetachedHead(CommitId),
    /// The tree of the working-copy commit is that of its parent (or empty, if there is none).
    NothingToCommit,
    /// The branch is not at the parent of the working-copy commit, so a commit on top of that parent
    /// would leave out what the branch has. A rebase (phase 3) is the way to bring the two
    /// together.
    BranchMoved {
        /// The branch that HEAD names.
        branch: BranchName,
        /// The parents of the working-copy commit.
        parents: Vec<CommitId>,
        /// The commit of the branch, or `None` if it does not exist.
        tip: Option<CommitId>,
    },
    /// The snapshot failed (among others a refusal of the authorizer for it).
    Snapshot(SnapshotError),
    /// The commit at the tip of the branch, or the tree before the first commit, could not be
    /// read.
    Tip(SnapshotError),
    /// A new commit could not be stored.
    Store(Box<StoreError>),
    /// HEAD, the working-copy commit, or the branch could not be read.
    Refs(RefError),
    /// The authorizer refused `commit`, a pointer was moved by another process, or the log or the
    /// references failed. [`UpdateError::entry`] and [`UpdateError::pointer_moved`] say what was
    /// done.
    Update(UpdateError),
    /// The operating system gave no random bytes for the change ID of the new working-copy commit.
    ChangeId(GenerateError),
    /// The log could not be read to tell a commit that stopped between its moves from a
    /// working-copy commit that was put back to an older version of itself. The commit was not
    /// made; at most the snapshot was written, and the call can be repeated.
    Log(OpLogError),
    /// The snapshot that comes first wrote an entry and moved the working-copy commit to it, and a
    /// later step failed with `source`. The commit was not made, unless `source` says that a
    /// pointer of it moved.
    AfterSnapshot {
        /// The entry of the snapshot.
        snapshot: Box<Entry>,
        /// The failure.
        source: Box<CommitError>,
    },
}

impl CommitError {
    /// The entry that the failure left in the log, if one: see [`UpdateError::entry`].
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Update(error) => error.entry(),
            Self::AfterSnapshot { source, .. } => source.entry(),
            _ => None,
        }
    }

    /// The entry of the snapshot that came first, if it wrote one and a later step failed.
    pub fn snapshot(&self) -> Option<&Entry> {
        match self {
            Self::AfterSnapshot { snapshot, .. } => Some(snapshot),
            _ => None,
        }
    }

    /// The failure itself, without the snapshot that came before it.
    pub fn failure(&self) -> &CommitError {
        match self {
            Self::AfterSnapshot { source, .. } => source.failure(),
            other => other,
        }
    }

    /// Whether a pointer moved although the call failed: see [`UpdateError::pointer_moved`]. If so
    /// the commit must not be repeated; the next call finishes it where it stopped.
    pub fn pointer_moved(&self) -> bool {
        match self {
            Self::Update(error) => error.pointer_moved(),
            Self::AfterSnapshot { source, .. } => source.pointer_moved(),
            _ => false,
        }
    }

    /// Returns the error of the file system, if one inside. The chain [`Error::source`] does not
    /// lead to it (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Snapshot(error) | Self::Tip(error) => error.io_error(),
            Self::Store(error) => error.io_error(),
            Self::Refs(error) => error.io_error(),
            Self::Update(error) => error.io_error(),
            Self::Log(error) => error.io_error(),
            Self::AfterSnapshot { source, .. } => source.io_error(),
            _ => None,
        }
    }
}

impl From<SnapshotError> for CommitError {
    fn from(error: SnapshotError) -> Self {
        Self::Snapshot(error)
    }
}

impl From<UpdateError> for CommitError {
    fn from(error: UpdateError) -> Self {
        Self::Update(error)
    }
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyMessage => f.write_str("the message of a commit must not be empty"),
            Self::DetachedHead(commit) => write!(
                f,
                "HEAD names the commit {commit} and not a branch, so there is no branch to commit to"
            ),
            Self::NothingToCommit => {
                f.write_str("there is nothing to commit: no tracked file changed")
            }
            Self::BranchMoved {
                branch,
                parents,
                tip,
            } => {
                let show = |commits: &[CommitId]| {
                    if commits.is_empty() {
                        "no commit".to_owned()
                    } else {
                        commits
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                };
                write!(
                    f,
                    "the branch `{branch}` is at {}, but the working-copy commit is based on {}; \
                     committing would leave out what the branch has",
                    show(tip.as_slice()),
                    show(parents)
                )
            }
            Self::Snapshot(error) => error.fmt(f),
            Self::Tip(error) => write!(f, "could not read the tip of the branch: {error}"),
            Self::Store(error) => write!(f, "could not store the new commits: {error}"),
            Self::Refs(error) => write!(f, "could not read the references: {error}"),
            Self::Update(error) => error.fmt(f),
            Self::ChangeId(error) => write!(f, "could not make a change ID: {error}"),
            Self::Log(error) => write!(
                f,
                "could not read the operation log, so the commit was not made: {error}"
            ),
            Self::AfterSnapshot { snapshot, source } => write!(
                f,
                "{source}; the working copy was saved first as entry {}",
                snapshot.number()
            ),
        }
    }
}

impl Error for CommitError {
    /// The cause of the cause: `Display` includes the message of the wrapped error, so the chain
    /// leaves it out and continues behind it, as the other errors of the crate do.
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Snapshot(error) | Self::Tip(error) => error.source(),
            Self::Store(error) => error.source(),
            Self::Refs(error) => error.source(),
            Self::Update(error) => error.source(),
            Self::ChangeId(error) => error.source(),
            Self::Log(error) => error.source(),
            Self::AfterSnapshot { source, .. } => source.source(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::object::Tree;
    use crate::oplog::OpLogError;
    use crate::repository::InitOptions;
    use crate::testutil::TempDir;
    use crate::{AllowAll, Decision};

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &OperationName, _target: &str) -> Decision {
            Decision::Deny
        }
    }

    fn actor() -> Actor {
        Actor::human("daniel").unwrap()
    }

    fn other_actor() -> Actor {
        Actor::human("alice").unwrap()
    }

    fn repository(dir: &TempDir) -> Repository {
        Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap()
    }

    fn write(dir: &TempDir, path: &str, content: &str) {
        fs::write(dir.path().join(path), content).unwrap();
    }

    fn track(repository: &Repository, path: &str) {
        repository
            .track(&[path.to_owned()], &actor(), &AllowAll)
            .unwrap();
    }

    fn commit(repository: &Repository, message: &str) -> Result<Committed, CommitError> {
        repository.commit(message, &other_actor(), &AllowAll)
    }

    fn read(repository: &Repository, id: CommitId) -> Commit {
        read_commit(repository.objects(), id).unwrap()
    }

    fn main() -> BranchName {
        BranchName::new("main").unwrap()
    }

    #[test]
    fn a_first_commit_creates_the_branch_and_starts_a_new_working_copy() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let before = repository.refs().working_copy().unwrap();
        let change = read(&repository, before).change();
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");

        let done = commit(&repository, "Add a").unwrap();

        assert!(done.late.is_none());
        assert_eq!(done.branch, main());
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(done.commit)
        );
        assert_eq!(repository.refs().working_copy().unwrap(), done.working_copy);
        let named = read(&repository, done.commit);
        assert_eq!(named.message(), "Add a");
        assert_eq!(
            named.change(),
            change,
            "the named commit keeps the change ID"
        );
        assert_eq!(named.author(), &other_actor());
        assert!(named.parents().is_empty());
        let next = read(&repository, done.working_copy);
        assert_eq!(next.parents(), [done.commit]);
        assert_eq!(next.tree(), named.tree(), "the new working copy is empty");
        assert_eq!(next.message(), "");
        assert_ne!(next.change(), change);
    }

    #[test]
    fn two_commits_have_different_change_ids_and_form_a_chain() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let first = commit(&repository, "one").unwrap();
        write(&dir, "a.txt", "two");

        let second = commit(&repository, "two").unwrap();

        let (one, two) = (
            read(&repository, first.commit),
            read(&repository, second.commit),
        );
        assert_ne!(one.change(), two.change());
        assert_eq!(two.parents(), [first.commit]);
        assert_ne!(one.tree(), two.tree());
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(second.commit)
        );
    }

    #[test]
    fn a_change_since_the_last_command_is_part_of_the_commit() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        write(&dir, "a.txt", "changed afterwards");

        let done = commit(&repository, "Add a").unwrap();

        let tree = read(&repository, done.commit).tree();
        let files = super::super::snapshot::tracked_files(repository.objects(), tree).unwrap();
        assert_eq!(files.len(), 1);
        let blob = Tree::parse(
            repository
                .objects()
                .get_expecting(&tree.object_id(), crate::object::Kind::Tree)
                .unwrap()
                .unwrap()
                .body(),
        )
        .unwrap();
        let entry = &blob.entries()[0];
        let content = repository
            .objects()
            .get_expecting(&entry.id(), crate::object::Kind::Blob)
            .unwrap()
            .unwrap();
        assert_eq!(content.body(), b"changed afterwards");
        let again = repository.snapshot(&actor(), &AllowAll).unwrap();
        assert!(again.is_none(), "the working directory equals the commit");
    }

    #[test]
    fn a_commit_writes_one_entry_for_both_pointers() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let entries = repository.oplog().entries().unwrap().len();

        let done = commit(&repository, "Add a").unwrap();

        let all = repository.oplog().entries().unwrap();
        assert_eq!(all.len(), entries + 1);
        let last = all.last().unwrap();
        assert_eq!(last, &done.entry);
        let change = last.change().unwrap();
        assert_eq!(change.operation().as_str(), "commit");
        assert_eq!(change.actor(), &other_actor());
        assert_eq!(
            change.working_copy().unwrap().after(),
            Some(&done.working_copy)
        );
        let (name, moved) = change.branches().first().unwrap();
        assert_eq!(name, &main());
        assert_eq!(moved.before(), None);
        assert_eq!(moved.after(), Some(&done.commit));
    }

    #[test]
    fn a_change_since_the_last_command_is_saved_by_its_own_snapshot_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        write(&dir, "a.txt", "two");
        let entries = repository.oplog().entries().unwrap().len();

        let _ = commit(&repository, "Add a").unwrap();

        let all = repository.oplog().entries().unwrap();
        let names: Vec<&str> = all[entries..]
            .iter()
            .map(|entry| entry.change().unwrap().operation().as_str())
            .collect();
        assert_eq!(names, ["snapshot", "commit"]);
    }

    #[test]
    fn nothing_to_commit_in_a_new_repository_and_after_a_commit() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        assert!(matches!(
            commit(&repository, "nothing"),
            Err(CommitError::NothingToCommit)
        ));
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let _ = commit(&repository, "Add a").unwrap();
        let entries = repository.oplog().entries().unwrap().len();

        let error = commit(&repository, "again").unwrap_err();

        assert!(matches!(error, CommitError::NothingToCommit), "{error}");
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[test]
    fn an_empty_message_is_refused_before_anything_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let entries = repository.oplog().entries().unwrap().len();

        for message in ["", "  \n\t"] {
            let error = commit(&repository, message).unwrap_err();
            assert!(matches!(error, CommitError::EmptyMessage), "{error}");
        }

        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[test]
    fn a_detached_head_is_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let first = commit(&repository, "one").unwrap();
        let operation = OperationName::new("checkout").unwrap();
        repository
            .set_head(
                &operation,
                &Head::Detached(first.commit),
                &actor(),
                &AllowAll,
            )
            .unwrap();
        write(&dir, "a.txt", "two");

        let error = commit(&repository, "two").unwrap_err();

        assert!(
            matches!(error, CommitError::DetachedHead(id) if id == first.commit),
            "{error}"
        );
    }

    #[test]
    fn a_branch_that_moved_is_not_overwritten() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let first = commit(&repository, "one").unwrap();
        // Someone moves the branch to the working-copy commit, past the parent it is based on.
        let operation = OperationName::new("branch").unwrap();
        repository
            .update_branch(
                &operation,
                &main(),
                Some(first.commit),
                first.working_copy,
                &actor(),
                &AllowAll,
            )
            .unwrap();
        write(&dir, "a.txt", "two");

        let error = commit(&repository, "two").unwrap_err();

        let CommitError::BranchMoved { tip, parents, .. } = error.failure() else {
            panic!("{error}");
        };
        assert_eq!(tip, &Some(first.working_copy));
        assert_eq!(parents, &[first.commit]);
        assert!(error.to_string().contains("main"), "{error}");
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(first.working_copy)
        );
    }

    #[test]
    fn a_refusal_by_the_authorizer_changes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let working_copy = repository.refs().working_copy().unwrap();
        let entries = repository.oplog().entries().unwrap().len();

        let error = repository
            .commit("Add a", &other_actor(), &DenyAll)
            .unwrap_err();

        assert!(
            matches!(&error, CommitError::Update(UpdateError::Denied { operation, target })
                if operation == "commit" && target == "main"),
            "{error}"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), working_copy);
        assert_eq!(repository.refs().branch(&main()).unwrap(), None);
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert!(
            !dir.path().join(".stemma/refs.lock").exists(),
            "a refusal takes no lock"
        );
    }

    #[test]
    fn a_locked_repository_is_reported_and_nothing_is_written() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let entries = repository.oplog().entries().unwrap().len();
        fs::write(repository.metadata_dir().join("refs.lock"), "someone").unwrap();

        let error = commit(&repository, "Add a").unwrap_err();

        assert!(
            matches!(
                error,
                CommitError::Update(UpdateError::Refs(RefError::Locked { .. }))
            ),
            "{error}"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert_eq!(repository.refs().branch(&main()).unwrap(), None);
    }

    #[test]
    fn a_pointer_that_another_process_moved_first_writes_no_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let working_copy = repository.refs().working_copy().unwrap();
        let first = commit(&repository, "one").unwrap();
        let entries = repository.oplog().entries().unwrap().len();
        // The moves use the working-copy commit that the caller saw before the first commit.
        let stale = repository.refs.update_branch_and_working_copy_after(
            &repository
                .refs
                .lock_writer(&crate::Allowed::granted())
                .unwrap(),
            &BranchMove {
                name: &main(),
                expected: Some(first.commit),
                new: first.working_copy,
            },
            &WorkingCopyMove {
                expected: working_copy,
                new: first.working_copy,
            },
            repository.objects(),
            || Ok::<_, OpLogError>(()),
        );

        assert!(
            matches!(stale, Err(RefError::StaleWorkingCopy { .. })),
            "the working-copy commit is checked"
        );
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(first.commit)
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    /// Whether permissions have an effect: root may write into a directory without the permission.
    #[cfg(unix)]
    fn permissions_hold(dir: &std::path::Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        let probe = dir.join("probe");
        fs::create_dir(&probe).unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o555)).unwrap();
        let held = fs::write(probe.join("file"), b"").is_err();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&probe).unwrap();
        held
    }

    /// A repository that has one file with a first commit not made yet, and the commits and
    /// moves that a `commit` would make, for the tests that stop it half way.
    struct Pending {
        named: CommitId,
        next: CommitId,
        working: CommitId,
    }

    fn pending(repository: &Repository, dir: &TempDir) -> Pending {
        write(dir, "a.txt", "one");
        track(repository, "a.txt");
        let working = repository.refs().working_copy().unwrap();
        let old = read(repository, working);
        let named = old.rewrite(old.tree(), actor(), now(), "stopped");
        let named = repository.put(&named).unwrap();
        let (_, next) = repository
            .start_after(read(repository, named).tree(), named, &actor())
            .unwrap();
        Pending {
            named,
            next,
            working,
        }
    }

    fn moves(
        repository: &Repository,
        pending: &Pending,
    ) -> Result<Logged<Entry, OpLogError>, RefError> {
        moves_with(repository, pending, || {})
    }

    /// Makes the moves of a commit; `after_entry` runs when the entry is written and nothing has
    /// moved yet.
    fn moves_with(
        repository: &Repository,
        pending: &Pending,
        after_entry: impl FnOnce(),
    ) -> Result<Logged<Entry, OpLogError>, RefError> {
        let operation = OperationName::new("commit").unwrap();
        repository.refs.update_branch_and_working_copy_after(
            &repository
                .refs
                .lock_writer(&crate::Allowed::granted())
                .unwrap(),
            &BranchMove {
                name: &main(),
                expected: None,
                new: pending.named,
            },
            &WorkingCopyMove {
                expected: pending.working,
                new: pending.next,
            },
            repository.objects(),
            || {
                let change = Change::new(
                    now(),
                    actor(),
                    operation.clone(),
                    None,
                    Some(Moved::from_to(Some(pending.working), Some(pending.next))),
                    vec![(main(), Moved::from_to(None, Some(pending.named)))],
                )
                .unwrap();
                let entry = repository.oplog.append(&change)?;
                after_entry();
                Ok(entry)
            },
        )
    }

    /// A commit that stopped between its moves: the entry is written and the branch has moved, but
    /// the working-copy file could not be replaced. `None` where permissions do not hold.
    #[cfg(unix)]
    fn stopped(repository: &Repository, dir: &TempDir) -> Option<Pending> {
        use std::os::unix::fs::PermissionsExt;
        if !permissions_hold(dir.path()) {
            return None;
        }
        let pending = pending(repository, dir);
        let metadata = repository.metadata_dir();
        // The branch can move, and the working-copy file cannot be replaced: the metadata
        // directory is read-only once the entry is written.
        let outcome = moves_with(repository, &pending, || {
            fs::set_permissions(&metadata, fs::Permissions::from_mode(0o555)).unwrap();
        })
        .unwrap();
        fs::set_permissions(&metadata, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_file(metadata.join("refs.lock"));
        let Logged::PartlyMoved(entry, source) = outcome else {
            panic!("the second move should fail");
        };
        let error = into_error(Logged::PartlyMoved(entry.clone(), source));
        assert!(error.pointer_moved() && error.entry() == Some(&entry));
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(pending.named)
        );
        assert_eq!(repository.refs().working_copy().unwrap(), pending.working);
        Some(pending)
    }

    #[cfg(unix)]
    #[test]
    fn a_stop_between_the_moves_is_finished_by_the_next_commit() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let Some(pending) = stopped(&repository, &dir) else {
            return;
        };

        let done = commit(&repository, "ignored").unwrap();

        assert!(done.resumed && done.late.is_none());
        assert_eq!(done.commit, pending.named);
        assert_eq!(read(&repository, done.commit).message(), "stopped");
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(pending.named)
        );
        assert_eq!(repository.refs().working_copy().unwrap(), done.working_copy);
        let next = read(&repository, done.working_copy);
        assert_eq!(next.parents(), [pending.named]);
        assert_ne!(next.change(), read(&repository, pending.named).change());
        assert_eq!(next.tree(), read(&repository, pending.named).tree());
        // The repository works again.
        write(&dir, "a.txt", "two");
        let after = commit(&repository, "after").unwrap();
        assert!(!after.resumed);
        assert_eq!(read(&repository, after.commit).parents(), [pending.named]);
    }

    #[cfg(unix)]
    #[test]
    fn a_change_since_a_stop_is_saved_and_the_stop_is_still_finished() {
        // The snapshot of the change gives the working-copy commit a new ID; its move starts from
        // the old value of the entry, so the log still reads the entry as not taken.
        let dir = TempDir::new();
        let repository = repository(&dir);
        let Some(pending) = stopped(&repository, &dir) else {
            return;
        };
        write(&dir, "a.txt", "two");

        let done = commit(&repository, "ignored").unwrap();

        assert!(done.resumed);
        assert_eq!(done.commit, pending.named);
        assert_eq!(read(&repository, done.commit).message(), "stopped");
        let next = read(&repository, done.working_copy);
        assert_eq!(next.parents(), [pending.named]);
        // The change since the stop is kept: the new working-copy commit has the tree of the
        // snapshot, not the tree of the named commit, so it matches the files and a snapshot
        // finds nothing to save.
        assert_ne!(next.tree(), read(&repository, pending.named).tree());
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
        // The next commit records the change.
        let after = commit(&repository, "after").unwrap();
        assert!(!after.resumed);
        assert_eq!(read(&repository, after.commit).tree(), next.tree());
    }

    #[cfg(unix)]
    #[test]
    fn a_stop_behind_a_stub_is_not_finished() {
        // The stub took the move that showed the stop: the log does not decide it any more.
        let dir = TempDir::new();
        let repository = repository(&dir);
        let Some(pending) = stopped(&repository, &dir) else {
            return;
        };
        write(&dir, "a.txt", "two");
        let snapshot = repository
            .snapshot(&actor(), &AllowAll)
            .unwrap()
            .unwrap()
            .number();
        let log = repository.oplog();
        log.append_purge(crate::oplog::Purge::new(1, actor(), 1, &[snapshot]).unwrap())
            .unwrap();
        log.replace_with_stub(snapshot).unwrap();
        let working = repository.refs().working_copy().unwrap();

        let error = commit(&repository, "mine").unwrap_err();

        assert!(
            matches!(&error, CommitError::BranchMoved { tip, .. } if *tip == Some(pending.named)),
            "{error}"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), working);
    }

    #[cfg(unix)]
    #[test]
    fn a_tip_that_the_newest_commit_did_not_set_is_no_stop() {
        // Another commit of the same shape at the tip, set by another operation: the newest entry
        // of a commit moved the branch elsewhere, so this tip is not the commit that stopped.
        let dir = TempDir::new();
        let repository = repository(&dir);
        let Some(pending) = stopped(&repository, &dir) else {
            return;
        };
        let old = read(&repository, pending.working);
        let other = repository
            .put(&old.rewrite(old.tree(), actor(), now(), "other"))
            .unwrap();
        repository
            .update_branch(
                &OperationName::new("branch").unwrap(),
                &main(),
                Some(pending.named),
                other,
                &actor(),
                &AllowAll,
            )
            .unwrap();
        let entries = repository.oplog().entries().unwrap().len();

        let error = commit(&repository, "mine").unwrap_err();

        assert!(
            matches!(&error, CommitError::BranchMoved { tip, .. } if *tip == Some(other)),
            "{error}"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert_eq!(repository.refs().working_copy().unwrap(), pending.working);
    }

    #[test]
    fn a_working_copy_moved_back_after_a_whole_commit_is_no_stop() {
        // The pointers are exactly those of a stop, but a later entry moved the working-copy
        // commit back from the new value of the commit entry: the entry took effect.
        let dir = TempDir::new();
        let repository = repository(&dir);
        let pending = pending(&repository, &dir);
        assert!(matches!(
            moves(&repository, &pending).unwrap(),
            Logged::Moved(_)
        ));
        repository
            .move_working_copy(
                &repository
                    .refs
                    .lock_writer(&crate::Allowed::granted())
                    .unwrap(),
                &OperationName::new("restore").unwrap(),
                pending.next,
                pending.working,
                &actor(),
            )
            .unwrap();
        let entries = repository.oplog().entries().unwrap().len();

        let error = commit(&repository, "mine").unwrap_err();

        assert!(
            matches!(&error, CommitError::BranchMoved { tip, .. } if *tip == Some(pending.named)),
            "{error}"
        );
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert_eq!(repository.refs().working_copy().unwrap(), pending.working);
    }

    #[test]
    fn a_working_copy_that_a_restore_put_back_is_no_commit_that_stopped() {
        // The restored working-copy commit has the parents and the change ID of the tip, and the
        // tip has a message: the commits look like a stop between the moves, the log does not.
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let first = commit(&repository, "one").unwrap();
        write(&dir, "a.txt", "two");
        let snapshot = repository.snapshot(&actor(), &AllowAll).unwrap().unwrap();
        let second = commit(&repository, "two").unwrap();
        repository
            .restore(snapshot.number(), false, &actor(), &AllowAll)
            .unwrap();
        let restored = repository.refs().working_copy().unwrap();
        assert_eq!(read(&repository, restored).parents(), [first.commit]);
        assert_eq!(
            read(&repository, restored).change(),
            read(&repository, second.commit).change()
        );
        let entries = repository.oplog().entries().unwrap().len();

        let error = commit(&repository, "three").unwrap_err();

        let CommitError::BranchMoved { tip, parents, .. } = &error else {
            panic!("{error}");
        };
        assert_eq!(tip, &Some(second.commit));
        assert_eq!(parents, &[first.commit]);
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert_eq!(repository.refs().working_copy().unwrap(), restored);
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(second.commit)
        );
        // The way to commit on the old state: a restore of the commit itself moves the branch back
        // to the parent, but brings the files of that commit; a second restore of the earlier
        // entry brings the old files again and leaves the branch.
        repository
            .restore(second.entry.number(), false, &actor(), &AllowAll)
            .unwrap();
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(first.commit)
        );
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "two");
        repository
            .restore(snapshot.number(), false, &actor(), &AllowAll)
            .unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "one");
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(first.commit)
        );
        write(&dir, "a.txt", "three");
        let third = commit(&repository, "three").unwrap();
        assert!(!third.resumed);
        assert_eq!(read(&repository, third.commit).message(), "three");
        assert_eq!(read(&repository, third.commit).parents(), [first.commit]);
    }

    #[test]
    fn a_stop_that_the_log_does_not_record_is_not_finished() {
        // The pointers of a stopped commit, set without the entry of a commit: nothing in the log
        // says that a commit stopped, so the call does not take the message of the tip.
        let dir = TempDir::new();
        let repository = repository(&dir);
        let pending = pending(&repository, &dir);
        let operation = OperationName::new("branch").unwrap();
        repository
            .update_branch(
                &operation,
                &main(),
                None,
                pending.named,
                &actor(),
                &AllowAll,
            )
            .unwrap();

        let error = commit(&repository, "mine").unwrap_err();

        assert!(
            matches!(&error, CommitError::BranchMoved { tip, .. } if *tip == Some(pending.named)),
            "{error}"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), pending.working);
    }

    #[cfg(unix)]
    #[test]
    fn a_failure_of_the_first_move_leaves_both_pointers_and_tries_no_second() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let repository = repository(&dir);
        if !permissions_hold(dir.path()) {
            return;
        }
        let pending = pending(&repository, &dir);
        let heads = repository.metadata_dir().join("refs");
        fs::create_dir_all(heads.join("heads")).unwrap();
        fs::set_permissions(heads.join("heads"), fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = moves(&repository, &pending).unwrap();

        fs::set_permissions(heads.join("heads"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            outcome,
            Logged::MoveFailed(_, RefError::Io { .. })
        ));
        assert_eq!(repository.refs().branch(&main()).unwrap(), None);
        assert_eq!(repository.refs().working_copy().unwrap(), pending.working);
        assert!(!repository.metadata_dir().join("refs.lock").exists());
    }

    #[test]
    fn a_name_in_the_way_is_a_conflict_and_writes_no_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let pending = pending(&repository, &dir);
        let other = OperationName::new("branch").unwrap();
        let deeper = BranchName::new("main/x").unwrap();
        repository
            .update_branch(&other, &deeper, None, pending.named, &actor(), &AllowAll)
            .unwrap();
        let entries = repository.oplog().entries().unwrap().len();

        let error = moves(&repository, &pending).unwrap_err();

        assert!(matches!(error, RefError::Conflict { .. }), "{error}");
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert_eq!(repository.refs().working_copy().unwrap(), pending.working);
    }

    #[test]
    fn an_empty_directory_where_the_branch_goes_is_removed() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let pending = pending(&repository, &dir);
        fs::create_dir_all(repository.metadata_dir().join("refs/heads/main")).unwrap();

        let outcome = moves(&repository, &pending).unwrap();

        assert!(matches!(outcome, Logged::Moved(_)));
        assert_eq!(
            repository.refs().branch(&main()).unwrap(),
            Some(pending.named)
        );
    }

    #[test]
    fn a_branch_that_is_not_where_it_was_expected_is_stale_and_writes_no_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let pending = pending(&repository, &dir);
        let other = OperationName::new("branch").unwrap();
        repository
            .update_branch(&other, &main(), None, pending.working, &actor(), &AllowAll)
            .unwrap();
        let entries = repository.oplog().entries().unwrap().len();

        let error = moves(&repository, &pending).unwrap_err();

        assert!(matches!(error, RefError::Stale { .. }), "{error}");
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
        assert_eq!(repository.refs().working_copy().unwrap(), pending.working);
    }

    #[test]
    fn a_commit_that_the_store_lacks_is_refused_before_the_step() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let mut pending = pending(&repository, &dir);
        let entries = repository.oplog().entries().unwrap().len();
        pending.next = pending.named;
        let other = Commit::new(
            read(&repository, pending.named).tree(),
            vec![],
            ChangeId::generate().unwrap(),
            actor(),
            1,
            "never stored",
        )
        .unwrap();
        pending.named = other.id();

        let error = moves(&repository, &pending).unwrap_err();

        assert!(matches!(error, RefError::MissingCommit(_)), "{error}");
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }

    #[test]
    fn two_commits_at_once_make_exactly_one() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let entries = repository.oplog().entries().unwrap().len();

        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|n| {
                    let repository = repository.clone();
                    scope.spawn(move || {
                        repository.commit(&format!("commit {n}"), &other_actor(), &AllowAll)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let made = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(made, 1, "{results:?}");
        for result in results.iter().filter_map(|r| r.as_ref().err()) {
            assert!(
                matches!(
                    result,
                    CommitError::NothingToCommit
                        | CommitError::Update(UpdateError::Refs(
                            RefError::Locked { .. }
                                | RefError::Stale { .. }
                                | RefError::StaleWorkingCopy { .. }
                        ))
                        | CommitError::Snapshot(_)
                ),
                "{result}"
            );
        }
        let names: Vec<String> = repository.oplog().entries().unwrap()[entries..]
            .iter()
            .map(|entry| entry.change().unwrap().operation().as_str().to_owned())
            .filter(|name| name == "commit")
            .collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn the_second_commit_records_where_each_pointer_came_from() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let first = commit(&repository, "one").unwrap();
        write(&dir, "a.txt", "two");

        let second = commit(&repository, "two").unwrap();

        let change = second.entry.change().unwrap();
        // The snapshot before the commit moved the working-copy commit, so the commit starts from
        // that one.
        let (name, moved) = change.branches().first().unwrap();
        assert_eq!(name, &main());
        assert_eq!(moved.before(), Some(&first.commit));
        assert_eq!(moved.after(), Some(&second.commit));
        let working = change.working_copy().unwrap();
        assert_ne!(working.before(), Some(&first.working_copy));
        assert_eq!(working.after(), Some(&second.working_copy));
        let log = repository.oplog().entries().unwrap();
        let snapshot = &log[log.len() - 2];
        assert_eq!(snapshot.change().unwrap().operation().as_str(), "snapshot");
        assert_eq!(
            snapshot.change().unwrap().working_copy().unwrap().after(),
            working.before()
        );
    }

    #[test]
    fn the_authorizer_is_asked_for_the_commit_and_then_for_the_snapshot() {
        use std::sync::Mutex;
        struct Recording(Mutex<Vec<(String, String)>>);
        impl Authorizer for Recording {
            fn authorize(
                &self,
                _actor: &Actor,
                operation: &OperationName,
                target: &str,
            ) -> Decision {
                self.0
                    .lock()
                    .unwrap()
                    .push((operation.to_string(), target.to_owned()));
                Decision::Allow
            }
        }
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let recording = Recording(Mutex::new(Vec::new()));

        let _ = repository
            .commit("Add a", &other_actor(), &recording)
            .unwrap();

        let asked = recording.0.into_inner().unwrap();
        assert_eq!(
            asked,
            [
                ("commit".to_owned(), "main".to_owned()),
                ("snapshot".to_owned(), "working-copy".to_owned())
            ]
        );
    }

    #[test]
    fn a_refusal_for_the_snapshot_comes_as_a_snapshot_error() {
        struct OnlyCommit;
        impl Authorizer for OnlyCommit {
            fn authorize(
                &self,
                _actor: &Actor,
                operation: &OperationName,
                _target: &str,
            ) -> Decision {
                if operation.as_str() == "commit" {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
        }
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");

        let error = repository
            .commit("Add a", &other_actor(), &OnlyCommit)
            .unwrap_err();

        assert!(
            matches!(
                &error,
                CommitError::Snapshot(SnapshotError::Update(UpdateError::Denied { .. }))
            ),
            "{error}"
        );
        assert_eq!(repository.refs().branch(&main()).unwrap(), None);
    }

    #[test]
    fn a_refusal_after_the_snapshot_leaves_only_the_snapshot_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let first = commit(&repository, "one").unwrap();
        let entries = repository.oplog().entries().unwrap().len();
        // The working directory is dirty and the branch is gone from under the working copy.
        write(&dir, "a.txt", "two");
        let other = OperationName::new("branch").unwrap();
        repository
            .update_branch(
                &other,
                &main(),
                Some(first.commit),
                first.working_copy,
                &actor(),
                &AllowAll,
            )
            .unwrap();
        let after_move = repository.oplog().entries().unwrap().len();
        assert_eq!(after_move, entries + 1);

        let error = commit(&repository, "two").unwrap_err();

        assert!(
            matches!(error.failure(), CommitError::BranchMoved { .. }),
            "{error}"
        );
        let log = repository.oplog().entries().unwrap();
        let names: Vec<&str> = log[after_move..]
            .iter()
            .map(|entry| entry.change().unwrap().operation().as_str())
            .collect();
        assert_eq!(names, ["snapshot"]);
        // The error names the entry of that snapshot, and nothing of the commit moved.
        assert_eq!(error.snapshot(), Some(&log[after_move]));
        assert!(!error.pointer_moved() && error.entry().is_none());
        assert!(
            error.to_string().ends_with(&format!(
                "; the working copy was saved first as entry {}",
                log[after_move].number()
            )),
            "{error}"
        );
    }

    #[test]
    fn deleting_every_tracked_file_is_a_change_to_commit() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let _ = commit(&repository, "one").unwrap();
        fs::remove_file(dir.path().join("a.txt")).unwrap();

        let done = commit(&repository, "remove a").unwrap();

        let tree = read(&repository, done.commit).tree();
        let files = super::super::snapshot::tracked_files(repository.objects(), tree).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn an_unreadable_tip_is_named_as_such() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, "a.txt");
        let first = commit(&repository, "one").unwrap();
        write(&dir, "a.txt", "two");
        let hex = first.commit.to_string();
        let path = dir.path().join(".stemma/objects");
        let found = walkdir_find(&path, &hex[hex.len() - 8..]);
        fs::remove_file(found).unwrap();

        let error = commit(&repository, "two").unwrap_err();

        assert!(matches!(error.failure(), CommitError::Tip(_)), "{error}");
        assert!(error.snapshot().is_some(), "{error}");
        assert!(error.to_string().contains("tip of the branch"), "{error}");
    }

    /// The file below `dir` whose name ends with `suffix`.
    fn walkdir_find(dir: &std::path::Path, suffix: &str) -> std::path::PathBuf {
        fn search(dir: &std::path::Path, suffix: &str) -> Option<std::path::PathBuf> {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if let Some(found) = search(&path, suffix) {
                        return Some(found);
                    }
                } else if path.to_string_lossy().ends_with(suffix) {
                    return Some(path);
                }
            }
            None
        }
        search(dir, suffix).unwrap_or_else(|| panic!("no file ends in {suffix}"))
    }

    #[test]
    fn the_error_exposes_the_log_and_the_cause_of_the_cause_only() {
        let entry = {
            let dir = TempDir::new();
            repository(&dir).oplog().entries().unwrap().remove(0)
        };
        let partly = CommitError::Update(into_error(Logged::PartlyMoved(
            entry.clone(),
            RefError::Locked {
                path: "x".into(),
                holder: crate::refs::LockHolder::Unknown,
            },
        )));
        assert!(partly.pointer_moved());
        assert_eq!(partly.entry(), Some(&entry));
        assert!(partly.to_string().contains("do not repeat"), "{partly}");
        // The wrapped error is in `Display`, so the chain does not repeat it.
        assert!(partly.source().is_none());
        let log = CommitError::Update(into_error(Logged::StepFailed(OpLogError::Full)));
        assert!(!log.pointer_moved() && log.entry().is_none());
        assert!(!CommitError::NothingToCommit.pointer_moved());
        // A log that cannot be read to tell a stop: nothing moved, and the cause is reachable.
        let unreadable = CommitError::Log(OpLogError::Io {
            operation: "list the entries",
            path: "oplog".into(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        });
        assert!(!unreadable.pointer_moved() && unreadable.entry().is_none());
        assert!(
            unreadable
                .to_string()
                .starts_with("could not read the operation log, so the commit was not made: "),
            "{unreadable}"
        );
        assert_eq!(
            unreadable.io_error().map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied)
        );
        assert!(unreadable.source().is_none());
    }

    #[test]
    fn rejections_by_precondition_leave_the_log_as_it_was() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let entries = repository.oplog().entries().unwrap().len();
        let _ = commit(&repository, " ");
        assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    }
}
