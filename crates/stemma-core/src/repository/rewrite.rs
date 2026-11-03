//! Rewriting the working-copy commit onto a new base (ADR-010).
//!
//! The working-copy commit `W` stands for the directory. When the branch it follows gets a new
//! tip, as after a merge, a rebase, or a `switch` that carries changes, `W` has to follow: the new
//! working-copy commit `W'` has the new tip as its parent, and its content is the three-way merge
//! of `W` over the old parent `P` of `W`, with the new tip `T` as the other side. As a sum of trees
//! that is `T + (W - P)`, so a conflict in the carry is data of `W'` (ADR-005), and a `W` that
//! carries an open conflict goes through the same algebra: its terms cancel against the new
//! ones or the sum grows.
//!
//! [`Repository::rewrite_working_copy`] is the mechanism, in the order of ADR-013: a snapshot,
//! the plan of the files, the entry, the files, the pointer. It moves the working-copy commit and
//! nothing else. The commands that also move a branch or HEAD (merge, rebase) are to write one
//! entry for all moves; they need a variant of the steps below that takes part in their entry,
//! which this module does not offer yet.
//!
//! A `W` that holds no change is not carried. It is replaced by an empty commit on the new tip with
//! a change ID of its own, as `switch` starts one, because the change ID tells a review that a
//! change survived a rewrite, and an empty working copy has no change to follow.
//!
//! The directory is written from the tree of `W'` if it has one tree. If `W'` records a conflict,
//! every path that is decided holds its file, and every path that is not decided holds the text of
//! [`marker_text`], so that the directory shows the competing values (ADR-005). A directory
//! term of a path that is a conflict of types counts as absent in this text; the commit keeps the
//! trees.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io;

use super::checkout::CheckoutError;
use super::snapshot::{read_any_commit, read_commit, SnapshotError};
use super::{now, Repository, UpdateError, WriteLock};
use crate::change::{ChangeId, GenerateError};
use crate::conflict::{marker_text, AnyCommit, FromSumError, Merge};
use crate::merge::{merge_trees, MergeError};
use crate::object::{Blob, CommitId, EntryMode, FileMode, Kind, ObjectId, Tree, TreeEntry, TreeId};
use crate::oplog::{Change, Entry, Moved, OpLogError, OperationName, TAKE_OVER};
use crate::refs::{Logged, RefError};
use crate::store::{ObjectStore, StoreError};
use crate::{Actor, Authorizer};

/// The operation that the log records for a rewrite of the working-copy commit.
const OPERATION: &str = "rewrite";

/// How a rewrite of the working-copy commit ended.
#[derive(Debug)]
#[must_use]
pub enum Rewritten {
    /// The working-copy commit has the new base as its parent already. Nothing but the snapshot
    /// was written, and no entry of a rewrite.
    Unchanged {
        /// The entry of the snapshot, if the directory held unsaved changes.
        snapshot: Option<Box<Entry>>,
    },
    /// The working-copy commit was replaced.
    Done {
        /// The entry of the snapshot, if the directory held unsaved changes.
        snapshot: Option<Box<Entry>>,
        /// The entry of the rewrite.
        entry: Box<Entry>,
        /// The working-copy commit that was replaced.
        previous: CommitId,
        /// The new working-copy commit, a child of the new base.
        working_copy: CommitId,
        /// The paths that are an open conflict in the new working-copy commit, sorted. Empty if
        /// the carry was clean, and for an empty working-copy commit unless the new base records
        /// a conflict itself.
        conflicts: Vec<String>,
    },
}

/// A new working-copy commit, written to the store and not yet named by a pointer.
struct Carried {
    id: CommitId,
    /// The tree that the directory is to hold: the tree of the commit, or, for a conflict, the
    /// tree with the text of the conflicts in place of the conflicted files.
    display: TreeId,
    conflicts: Vec<String>,
}

impl Repository {
    /// Replaces the working-copy commit by one on the commit `onto`, which carries the changes of
    /// the old one (see the module documentation), and brings the directory to its tree.
    ///
    /// The steps are: the authorizer is asked for `rewrite` (the target is the commit); the
    /// lock of the references is taken and held to the move (ADR-014); the tracked files are saved
    /// in a snapshot (a second question, `snapshot`); the new commit is made and stored; the files
    /// are planned from the values the lock sees, the entry is written and flushed, the directory
    /// is brought to the new tree, and the pointer moves (ADR-013). The entry holds one move, the
    /// one of the working-copy commit. HEAD and the branches stay where they are.
    ///
    /// If the working-copy commit holds no change, it is replaced by an empty commit with a new
    /// change ID. Otherwise the new commit keeps the change ID, the author, the time, and the
    /// message, and its tree is the merge described in the module documentation. A conflict is no
    /// failure: the commit records it, [`Rewritten::Done`] lists its paths, and the directory
    /// holds the marker text of each.
    ///
    /// If the newest entry of the log (a `take-over` does not count) is a rewrite onto the same
    /// commit that did not take effect (the entry was written, the files are written or partly
    /// written, and the pointer did not move), the call is its repeat: it takes no snapshot, as
    /// one would save the new files as changes of the old working-copy commit, and it uses the
    /// commit of that entry again. It checks the owner of the working copy instead (ADR-010). If
    /// that rewrite was onto another commit, the call is refused with
    /// [`RewriteError::StoppedOnOtherBase`] for the same reason.
    ///
    /// A working-copy commit that records a conflict is refused by the snapshot
    /// ([`SnapshotError::Conflicted`]): what a command does with an open conflict is not decided
    /// yet. The merge of such a commit is the same as for any other and is tested through the
    /// step that makes the commit. The same holds for the result: after a rewrite that ends in a
    /// conflict, every command that takes a snapshot, `op restore` and a second rewrite among
    /// them, is refused with that error until the snapshot handles a conflict (STEMMA-19).
    ///
    /// # Errors
    ///
    /// Before anything is written: [`RewriteError::Update`] for a refusal of the question
    /// `rewrite`, [`RewriteError::Refs`] for a lock that another command holds or a purge that is
    /// pending, [`RewriteError::NoSuchCommit`], [`RewriteError::Store`], and, when the call looks
    /// for a stopped rewrite, [`RewriteError::Log`], [`RewriteError::Read`], and
    /// [`RewriteError::StoppedOnOtherBase`].
    ///
    /// [`RewriteError::Snapshot`] is a refusal of the snapshot or, for a repeat, of the owner
    /// check; a take-over that the repeat made may have written its entry before it fails. A
    /// failure of the snapshot to move the pointer is [`RewriteError::Update`] with the entry, and
    /// [`RewriteError::SnapshotNotDurable`] says that the snapshot was made and its directory not
    /// flushed; the rewrite did not start then.
    ///
    /// After the snapshot, [`RewriteError::Read`], [`RewriteError::Merge`],
    /// [`RewriteError::Invalid`], [`RewriteError::Store`], [`RewriteError::ChangeId`],
    /// [`RewriteError::Parents`], [`RewriteError::SameChange`], and [`RewriteError::Checkout`] stand for the preparation of the
    /// new commit and the plan of the files. They wrote nothing but objects and, if the snapshot
    /// wrote an entry, that entry; the error is then wrapped in [`RewriteError::AfterSnapshot`].
    /// Then [`RewriteError::CheckoutFailed`], [`RewriteError::MoveFailed`], and
    /// [`RewriteError::NotDurable`] say how far the rewrite came.
    pub fn rewrite_working_copy(
        &self,
        onto: CommitId,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<Rewritten, RewriteError> {
        let operation =
            OperationName::try_new(OPERATION).expect("`rewrite` is a valid operation name");
        self.refs_store()
            .refuse_pending_purge()
            .map_err(RewriteError::Refs)?;
        let allowed = self
            .authorize(actor, authorizer, &operation, &onto.to_string())
            .map_err(RewriteError::Update)?;
        let onto = self
            .objects()
            .resolve_commit(&onto.object_id())
            .map_err(|error| RewriteError::Store(Box::new(error)))?
            .ok_or(RewriteError::NoSuchCommit(onto.object_id()))?;
        let lock = self
            .refs_store()
            .lock_writer(&allowed)
            .map_err(RewriteError::Refs)?;
        // A rewrite that stopped after the files is repeated without a snapshot (see above).
        let stopped = self.stopped_rewrite(onto)?;
        let (snapshot, repeated) = match stopped {
            Some(next) => {
                self.own_for_rewrite_repeat(&lock, actor, authorizer)?;
                (None, Some(next))
            }
            None => match self.snapshot_locked(&lock, actor, authorizer) {
                Ok(entry) => (entry.map(Box::new), None),
                // The snapshot was made and the pointer moved; only the flush is not known.
                // Nothing else was written, and the repeat finds the files saved.
                Err(SnapshotError::Update(UpdateError::NotDurable { entry, source })) => {
                    return Err(RewriteError::SnapshotNotDurable {
                        snapshot: entry,
                        source,
                    });
                }
                // The entry of a snapshot that failed to move the pointer stays reachable.
                Err(SnapshotError::Update(error)) => return Err(RewriteError::Update(error)),
                Err(error) => return Err(RewriteError::Snapshot(error)),
            },
        };
        #[cfg(test)]
        super::snapshot::pause::reached(super::snapshot::pause::Stage::Moves);
        let wrap = |error: RewriteError| match &snapshot {
            Some(saved) => RewriteError::AfterSnapshot {
                snapshot: saved.clone(),
                source: Box::new(error),
            },
            None => error,
        };
        let current = self
            .refs()
            .working_copy()
            .map_err(|error| wrap(RewriteError::Refs(error)))?;
        let carried = match repeated {
            Some(next) => self.carried(next).map(Some),
            None => read_any_commit(self.objects(), current)
                .map_err(RewriteError::Read)
                .and_then(|working| {
                    if working.parents() == [onto] {
                        Ok(None)
                    } else {
                        self.carry(&working, onto, actor).map(Some)
                    }
                }),
        };
        let carried = match carried.map_err(wrap)? {
            Some(carried) => carried,
            None => return Ok(Rewritten::Unchanged { snapshot }),
        };
        let logged = self.refs_store().update_working_copy_after(
            &lock,
            current,
            carried.id,
            self.objects(),
            || self.write_rewrite(&lock, &operation, current, &carried, actor),
        );
        let made = |entry: Entry| Rewritten::Done {
            snapshot: snapshot.clone(),
            entry: Box::new(entry),
            previous: current,
            working_copy: carried.id,
            conflicts: carried.conflicts.clone(),
        };
        match logged {
            Err(error) => Err(wrap(RewriteError::Refs(error))),
            Ok(Logged::Moved(entry)) => Ok(made(entry)),
            Ok(Logged::Unchanged) => Ok(Rewritten::Unchanged { snapshot }),
            Ok(Logged::StepFailed(error)) => Err(wrap(error)),
            Ok(Logged::MoveFailed(entry, source)) => Err(wrap(RewriteError::MoveFailed {
                entry: Box::new(entry),
                source,
            })),
            Ok(Logged::PartlyMoved(..)) => unreachable!("the rewrite moves one pointer"),
            Ok(Logged::NotFlushed(entry, source)) => Err(wrap(RewriteError::NotDurable {
                entry: Box::new(entry),
                source,
            })),
        }
    }

    /// The step of the move, under the lock: plan the files, write the entry, bring the directory
    /// to the new tree (ADR-013).
    fn write_rewrite(
        &self,
        lock: &WriteLock,
        operation: &OperationName,
        current: CommitId,
        carried: &Carried,
        actor: &Actor,
    ) -> Result<Entry, RewriteError> {
        let from = read_commit(self.objects(), current)
            .map_err(RewriteError::Read)?
            .tree();
        let plan = self
            .plan_checkout(Some(from), carried.display)
            .map_err(RewriteError::Checkout)?;
        let change = Change::try_new(
            now(),
            actor.clone(),
            operation.clone(),
            None,
            Some(Moved::from_to(Some(current), Some(carried.id))),
            vec![],
        )
        .map_err(|error| RewriteError::Update(UpdateError::Log(OpLogError::Entry(error))))?;
        let entry = self
            .append(lock, &change)
            .map_err(|error| RewriteError::Update(UpdateError::Log(error)))?;
        self.apply_checkout(&plan)
            .map_err(|source| RewriteError::CheckoutFailed {
                entry: Box::new(entry.clone()),
                source,
            })?;
        Ok(entry)
    }

    /// Makes the commit that replaces `working` as a child of `onto`, and stores it. Writes
    /// objects and nothing else.
    ///
    /// The tree is `onto + (working - parent)` as a sum, merged by [`merge_trees`]. A working-copy
    /// commit without a parent has the empty tree as its base. One that holds no change (its
    /// sum equals that of its base) is replaced by an empty commit with a change ID of its own.
    fn carry(
        &self,
        working: &AnyCommit,
        onto: CommitId,
        actor: &Actor,
    ) -> Result<Carried, RewriteError> {
        let objects = self.objects();
        let base = match working.parents() {
            [] => Merge::resolved(
                objects
                    .put_tree(&Tree::try_new(vec![]).map_err(|e| RewriteError::Merge(e.into()))?)
                    .map_err(|error| RewriteError::Store(Box::new(error)))?,
            ),
            [parent] => read_any_commit(objects, *parent)
                .map_err(RewriteError::Read)?
                .tree_sum(),
            _ => {
                return Err(RewriteError::Parents {
                    working: working.id(),
                })
            }
        };
        let target = read_any_commit(objects, onto).map_err(RewriteError::Read)?;
        if target.change() == working.change() {
            return Err(RewriteError::SameChange { onto });
        }
        let new_base = target.tree_sum();
        let own = working.tree_sum();
        let commit = if own.is_equivalent(&base) {
            AnyCommit::from_sum(
                &new_base,
                vec![onto],
                ChangeId::generate().map_err(RewriteError::ChangeId)?,
                actor.clone(),
                now(),
                "",
            )
        } else {
            // `onto + working - base`: the removed terms of the base are added, its added terms
            // removed.
            let mut adds: Vec<TreeId> = new_base.adds().copied().collect();
            adds.extend(own.adds().copied());
            adds.extend(base.removes().copied());
            let mut removes: Vec<TreeId> = new_base.removes().copied().collect();
            removes.extend(own.removes().copied());
            removes.extend(base.adds().copied());
            let merged = merge_trees(objects, &Merge::from_parts(adds, removes))
                .map_err(RewriteError::Merge)?;
            working.reparent(&merged, vec![onto])
        }
        .map_err(RewriteError::Invalid)?;
        let id = match &commit {
            AnyCommit::Commit(commit) => objects.put_commit(commit),
            AnyCommit::Commit2(commit) => objects
                .put_raw(Kind::Commit2, &commit.encode())
                .map(CommitId::new),
        }
        .map_err(|error| RewriteError::Store(Box::new(error)))?;
        self.carried(id)
    }

    /// The directory tree of a stored working-copy commit, with the paths that are a conflict.
    fn carried(&self, id: CommitId) -> Result<Carried, RewriteError> {
        let commit = read_any_commit(self.objects(), id).map_err(RewriteError::Read)?;
        let mut display = Display {
            store: self.objects(),
            conflicts: Vec::new(),
        };
        let tree = display
            .tree("", &commit.tree_sum())
            .map_err(RewriteError::Merge)?;
        let mut conflicts = display.conflicts;
        conflicts.sort();
        Ok(Carried {
            id,
            display: tree,
            conflicts,
        })
    }

    /// The working-copy commit of the newest entry, if that entry is a rewrite that did not
    /// finish: the pointer still has its old value. The files were written then (a stop after the
    /// files, or in their middle), and the pointer did not move. The caller holds the lock of
    /// the references, so no writer came between the entry and its move.
    ///
    /// If the commit of the entry has `onto` as its only parent, this is the repeat and the
    /// result is that commit. If it has another parent, the call is refused with
    /// [`RewriteError::StoppedOnOtherBase`].
    fn stopped_rewrite(&self, onto: CommitId) -> Result<Option<CommitId>, RewriteError> {
        let entries = self.oplog().entries().map_err(RewriteError::Log)?;
        // A `take-over` names no pointer: the repeat of another actor writes one before its own
        // entry, and must not hide a stopped rewrite from a second repeat.
        let Some(change) = entries
            .iter()
            .rev()
            .filter_map(Entry::change)
            .find(|change| change.operation().as_str() != TAKE_OVER)
        else {
            return Ok(None);
        };
        if change.operation().as_str() != OPERATION {
            return Ok(None);
        }
        let Some(moved) = change.working_copy() else {
            return Ok(None);
        };
        let (Some(&old), Some(&next)) = (moved.before(), moved.after()) else {
            return Ok(None);
        };
        let current = self.refs().working_copy().map_err(RewriteError::Refs)?;
        if current != old {
            return Ok(None);
        }
        // The commit is in the store, as the entry was written after it: a failure to read it
        // is damage and not "no repeat", which would take the snapshot that the repeat avoids.
        let commit = read_any_commit(self.objects(), next).map_err(RewriteError::Read)?;
        if commit.parents() == [onto] {
            Ok(Some(next))
        } else {
            // A snapshot would save the files of the stopped rewrite as changes of the old
            // working-copy commit, and the new rewrite would carry them onto another base.
            Err(RewriteError::StoppedOnOtherBase { stopped: next })
        }
    }

    /// The owner check of the snapshot that a repeat does not take: another actor does not finish
    /// the rewrite of the owner without `--take-over`, and an agent never takes over (ADR-010).
    fn own_for_rewrite_repeat(
        &self,
        lock: &WriteLock,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<(), RewriteError> {
        let (_, owner) = self
            .refs_store()
            .working_copy_and_owner()
            .map_err(RewriteError::Refs)?;
        if self
            .check_owner(actor, owner.as_ref())
            .map_err(RewriteError::Snapshot)?
        {
            let operation = self
                .ask_take_over(actor, authorizer)
                .map_err(RewriteError::Snapshot)?;
            self.take_over_locked(lock, &operation, actor)
                .map_err(RewriteError::Snapshot)?;
        }
        Ok(())
    }
}

/// One value of a name in one term of a sum of trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    mode: EntryMode,
    id: ObjectId,
}

/// Builds the tree that the directory holds for a sum of trees.
struct Display<'a> {
    store: &'a dyn ObjectStore,
    conflicts: Vec<String>,
}

impl Display<'_> {
    /// The tree of a sum: the tree itself if the sum is resolved, and otherwise a tree in which a
    /// path that is decided holds its value and a path that is not holds the marker text.
    fn tree(&mut self, prefix: &str, sum: &Merge<TreeId>) -> Result<TreeId, MergeError> {
        let sum = sum.simplify();
        if let Some(tree) = sum.resolution() {
            return Ok(tree);
        }
        let empty = self.store.put_tree(&Tree::try_new(vec![])?)?;
        let trees: Vec<Tree> = sum
            .terms()
            .iter()
            .map(|id| {
                self.store.get_tree(id)?.ok_or(MergeError::Missing {
                    id: id.object_id(),
                    kind: Kind::Tree,
                })
            })
            .collect::<Result<_, _>>()?;
        let names: BTreeSet<&str> = trees
            .iter()
            .flat_map(|tree| tree.entries().iter().map(TreeEntry::name))
            .collect();
        let mut entries = Vec::new();
        for name in names {
            let slots = Merge::try_from_terms(
                trees
                    .iter()
                    .map(|tree| {
                        tree.entries()
                            .binary_search_by(|entry| entry.name().cmp(name))
                            .ok()
                            .map(|at| {
                                let entry = &tree.entries()[at];
                                Slot {
                                    mode: entry.mode(),
                                    id: entry.id(),
                                }
                            })
                    })
                    .collect(),
            )
            .expect("the trees are the terms of a sum")
            .simplify();
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            if let Some(decided) = slots.resolution() {
                if let Some(slot) = decided {
                    entries.push(entry_of(name, slot)?);
                }
            } else if slots.terms().iter().flatten().all(is_directory) {
                let below = slots.map(|term| term.map_or(empty, |slot| TreeId::new(slot.id)));
                let tree = self.tree(&path, &below)?;
                if tree != empty {
                    entries.push(TreeEntry::directory(name, tree)?);
                }
            } else {
                entries.extend(self.conflicted_file(name, path, &slots)?);
            }
        }
        Ok(self.store.put_tree(&Tree::try_new(entries)?)?)
    }

    /// The file that stands for a path that is not decided: the marker text of its terms. The
    /// mode is that of the first term that is a file, and a regular file if none is, since a
    /// link with this text would point nowhere.
    fn conflicted_file(
        &mut self,
        name: &str,
        path: String,
        slots: &Merge<Option<Slot>>,
    ) -> Result<Option<TreeEntry>, MergeError> {
        let mut contents: Vec<Option<Vec<u8>>> = Vec::with_capacity(slots.terms().len());
        for term in slots.terms() {
            contents.push(match term {
                Some(slot) if !is_directory(slot) => Some(
                    self.store
                        .get_expecting(&slot.id, Kind::Blob)?
                        .map(crate::store::RawObject::into_body)
                        .ok_or(MergeError::Missing {
                            id: slot.id,
                            kind: Kind::Blob,
                        })?,
                ),
                _ => None,
            });
        }
        let sum = Merge::try_from_terms(contents.iter().map(Option::as_deref).collect())
            .expect("the terms of a sum");
        // Directories count as absent, so the terms may cancel to one value in this text: that
        // value is what the path shows. If that value is "absent", the path holds no file; it
        // stays in the list of conflicts, as the sum still records it.
        let text = match marker_text(&sum) {
            Some(text) => text,
            None => match sum.resolution() {
                Some(Some(value)) => value.to_vec(),
                _ => {
                    self.conflicts.push(path);
                    return Ok(None);
                }
            },
        };
        let mode = slots
            .terms()
            .iter()
            .flatten()
            .find_map(|slot| match slot.mode {
                EntryMode::File => Some(FileMode::File),
                EntryMode::Executable => Some(FileMode::Executable),
                EntryMode::Symlink | EntryMode::Directory => None,
            })
            .unwrap_or(FileMode::File);
        let blob = self.store.put_blob(&Blob::new(text))?;
        self.conflicts.push(path);
        Ok(Some(TreeEntry::file(name, mode, blob)?))
    }
}

fn is_directory(slot: &Slot) -> bool {
    slot.mode == EntryMode::Directory
}

/// The tree entry for a value of a name.
fn entry_of(name: &str, slot: Slot) -> Result<TreeEntry, crate::object::TreeError> {
    match FileMode::from_entry_mode(slot.mode) {
        Some(mode) => TreeEntry::file(name, mode, crate::object::BlobId::new(slot.id)),
        None => TreeEntry::directory(name, TreeId::new(slot.id)),
    }
}

/// Why [`Repository::rewrite_working_copy`] fails. [`CheckoutFailed`](Self::CheckoutFailed),
/// [`MoveFailed`](Self::MoveFailed), and [`NotDurable`](Self::NotDurable) wrote the entry of the
/// rewrite; the other variants did not. [`SnapshotNotDurable`](Self::SnapshotNotDurable) and
/// [`AfterSnapshot`](Self::AfterSnapshot) name the entry of a snapshot that came first.
#[derive(Debug)]
#[non_exhaustive]
pub enum RewriteError {
    /// The authorizer refused `rewrite`, or the entry could not be written (see
    /// [`UpdateError::Log`]). No pointer moved and no file was written.
    Update(UpdateError),
    /// The references could not be read, the lock is held, or a purge is pending.
    Refs(RefError),
    /// The log could not be read.
    Log(OpLogError),
    /// A commit could not be looked up or stored (this includes a failure to write one).
    Store(Box<StoreError>),
    /// The snapshot failed, or the owner of the working copy is not the actor.
    Snapshot(SnapshotError),
    /// A commit or a tree could not be read.
    Read(SnapshotError),
    /// The merge of the changes failed: an object is missing or damaged, or the storage failed.
    Merge(MergeError),
    /// The sum of trees of the new commit is not valid for a commit.
    Invalid(FromSumError),
    /// A file is in the way, or the files cannot be planned. Nothing was written.
    Checkout(CheckoutError),
    /// The operating system gave no random bytes for a change ID.
    ChangeId(GenerateError),
    /// The store has no commit with the ID.
    NoSuchCommit(ObjectId),
    /// The working-copy commit has more than one parent, and its base is not decided yet.
    Parents {
        /// The working-copy commit.
        working: CommitId,
    },
    /// The newest entry is a rewrite that did not finish, and it is onto another commit. Taking
    /// a snapshot would save its files as changes of the working-copy commit, so nothing was
    /// done: the same rewrite run again finishes it. Nothing was written.
    StoppedOnOtherBase {
        /// The working-copy commit that the stopped rewrite made.
        stopped: CommitId,
    },
    /// The new base has the change ID of the working-copy commit, so the result would be a
    /// second commit of one change. Nothing was written but objects.
    SameChange {
        /// The commit that was to be the new base.
        onto: CommitId,
    },
    /// The files could not be brought to the new tree after the entry was written. No pointer
    /// moved, and the files done so far stay; the same rewrite run again finishes it.
    CheckoutFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: CheckoutError,
    },
    /// The entry was written and the files are at the new tree, but the pointer did not move.
    MoveFailed {
        /// The entry that was written.
        entry: Box<Entry>,
        /// Why.
        source: RefError,
    },
    /// The rewrite was made, but the directory could not be flushed to disk, so the move may not
    /// survive a crash.
    NotDurable {
        /// The entry that was written.
        entry: Box<Entry>,
        /// The error of the flush.
        source: RefError,
    },
    /// The snapshot moved the working-copy commit and its directory could not be flushed. The
    /// rewrite did not start.
    SnapshotNotDurable {
        /// The entry of the snapshot.
        snapshot: Box<Entry>,
        /// The error of the flush.
        source: RefError,
    },
    /// The snapshot wrote an entry, and a later step failed.
    AfterSnapshot {
        /// The entry of the snapshot.
        snapshot: Box<Entry>,
        /// The failure.
        source: Box<RewriteError>,
    },
}

impl RewriteError {
    /// Returns the error of the file system, if one is inside.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Update(error) => error.io_error(),
            Self::Log(error) => error.io_error(),
            Self::Refs(error)
            | Self::MoveFailed { source: error, .. }
            | Self::NotDurable { source: error, .. }
            | Self::SnapshotNotDurable { source: error, .. } => error.io_error(),
            Self::Store(error) => error.io_error(),
            Self::Merge(error) => error.store_error().and_then(StoreError::io_error),
            Self::Snapshot(error) | Self::Read(error) => error.io_error(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.io_error(),
            Self::AfterSnapshot { source, .. } => source.io_error(),
            Self::Invalid(_)
            | Self::ChangeId(_)
            | Self::NoSuchCommit(_)
            | Self::Parents { .. }
            | Self::StoppedOnOtherBase { .. }
            | Self::SameChange { .. } => None,
        }
    }

    /// The entry of the rewrite, if the error leaves one in the log.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Update(error) => error.entry(),
            Self::CheckoutFailed { entry, .. }
            | Self::MoveFailed { entry, .. }
            | Self::NotDurable { entry, .. } => Some(entry),
            Self::AfterSnapshot { source, .. } => source.entry(),
            _ => None,
        }
    }

    /// The entry of the snapshot that came first, if it wrote one and a later step failed.
    pub fn snapshot(&self) -> Option<&Entry> {
        match self {
            Self::AfterSnapshot { snapshot, .. } | Self::SnapshotNotDurable { snapshot, .. } => {
                Some(snapshot)
            }
            _ => None,
        }
    }

    /// Whether the rewrite took effect and only the flush failed: the result is to be reported
    /// and the rewrite is not to be repeated.
    pub fn made(&self) -> bool {
        match self {
            Self::NotDurable { .. } => true,
            Self::AfterSnapshot { source, .. } => source.made(),
            _ => false,
        }
    }

    /// Whether a pointer moved although the call failed and the move is not known to be durable:
    /// the snapshot or the rewrite moved the working-copy commit and the flush failed. A snapshot
    /// that moved it durably before a later step failed is not counted; [`snapshot`](Self::snapshot)
    /// names it.
    pub fn pointer_moved(&self) -> bool {
        match self {
            Self::NotDurable { .. } | Self::SnapshotNotDurable { .. } => true,
            Self::AfterSnapshot { source, .. } => source.pointer_moved(),
            _ => false,
        }
    }
}

impl fmt::Display for RewriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Update(error) => write!(f, "could not rewrite the working-copy commit: {error}"),
            Self::Refs(error) => write!(
                f,
                "could not rewrite the working-copy commit, nothing of it was written: {error}"
            ),
            Self::Log(error) => write!(f, "could not read the operation log: {error}"),
            Self::Store(error) => write!(f, "could not look up or store a commit: {error}"),
            Self::Snapshot(error) if error.stands_alone() => error.fmt(f),
            Self::Snapshot(error) => write!(
                f,
                "could not save the working directory or check its owner first, so the rewrite \
                 did not start: {error}"
            ),
            Self::Read(error) => write!(f, "could not read a commit or a tree: {error}"),
            Self::Merge(error) => write!(f, "could not merge the changes: {error}"),
            Self::Invalid(error) => write!(f, "the new working-copy commit is not valid: {error}"),
            Self::Checkout(error) => write!(
                f,
                "could not rewrite the working-copy commit, nothing of it was written: {error}"
            ),
            Self::ChangeId(error) => write!(f, "could not make a change ID: {error}"),
            Self::NoSuchCommit(id) => write!(f, "the store has no commit {id}"),
            Self::Parents { working } => write!(
                f,
                "the working-copy commit {working} has more than one parent, and its base is \
                 not decided yet"
            ),
            Self::StoppedOnOtherBase { stopped } => write!(
                f,
                "the newest rewrite (working-copy commit {stopped}) did not finish and is onto \
                 another commit; run that rewrite again before any other command, as a snapshot \
                 would save its files as changes"
            ),
            Self::SameChange { onto } => write!(
                f,
                "commit {onto} has the change ID of the working-copy commit, so a rewrite onto it \
                 would make two commits of one change"
            ),
            Self::CheckoutFailed { entry, source } => write!(
                f,
                "could not write files; entry {} names a rewrite that did not happen and no \
                 pointer moved, and the files done so far stay: {source}; run the same \
                 rewrite again before any other command, as a snapshot saves only the files \
                 that the working-copy commit tracks and would miss the files that only the \
                 new tree has",
                entry.number()
            ),
            Self::MoveFailed { entry, source } => write!(
                f,
                "entry {} was written and the files are at the new tree, but the pointer did \
                 not move: {source}; run the same rewrite again before any other command, as a \
                 snapshot saves only the files that the working-copy commit tracks and would \
                 miss the files that only the new tree has",
                entry.number()
            ),
            Self::NotDurable { entry, source } => write!(
                f,
                "the rewrite was made as entry {}, but the directory could not be flushed, so \
                 it may not survive a crash: {source}",
                entry.number()
            ),
            Self::SnapshotNotDurable { snapshot, source } => write!(
                f,
                "unsaved changes were saved first as entry {}, but the directory could not be \
                 flushed to disk and the saved state may not survive a crash, so the rewrite did \
                 not start: {source}",
                snapshot.number()
            ),
            Self::AfterSnapshot { snapshot, source } => write!(
                f,
                "{source}; unsaved changes were saved first as entry {}",
                snapshot.number()
            ),
        }
    }
}

impl Error for RewriteError {
    /// The cause of the cause: `Display` includes the message of the wrapped error, so the chain
    /// leaves it out and continues behind it, as the other errors of the crate do.
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Update(error) => error.source(),
            Self::Log(error) => error.source(),
            Self::Refs(error)
            | Self::MoveFailed { source: error, .. }
            | Self::NotDurable { source: error, .. }
            | Self::SnapshotNotDurable { source: error, .. } => error.source(),
            Self::Store(error) => error.source(),
            Self::Merge(error) => error.source(),
            Self::Snapshot(error) | Self::Read(error) => error.source(),
            Self::Checkout(error) | Self::CheckoutFailed { source: error, .. } => error.source(),
            Self::Invalid(error) => error.source(),
            Self::ChangeId(error) => error.source(),
            Self::AfterSnapshot { source, .. } => source.source(),
            Self::NoSuchCommit(_)
            | Self::Parents { .. }
            | Self::StoppedOnOtherBase { .. }
            | Self::SameChange { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::rc::Rc;

    use super::*;
    use crate::fault;
    use crate::refs::BranchName;
    use crate::repository::{InitOptions, SwitchTarget};
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

    fn repository(dir: &TempDir) -> Repository {
        Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap()
    }

    fn write(dir: &TempDir, path: &str, content: &str) {
        fs::write(dir.path().join(path), content).unwrap();
    }

    fn read(dir: &TempDir, path: &str) -> String {
        fs::read_to_string(dir.path().join(path)).unwrap()
    }

    fn name(text: &str) -> BranchName {
        BranchName::try_new(text).unwrap()
    }

    fn track(repository: &Repository, path: &str) {
        repository
            .track(&[path.to_owned()], &actor(), &AllowAll)
            .unwrap();
    }

    fn commit(repository: &Repository, message: &str) {
        repository
            .commit(message, &actor(), &AllowAll)
            .map(drop)
            .unwrap();
    }

    fn switch(repository: &Repository, target: &SwitchTarget) {
        repository
            .switch(target, &actor(), &AllowAll)
            .map(drop)
            .unwrap();
    }

    fn create(branch: &str) -> SwitchTarget {
        SwitchTarget::Create {
            name: name(branch),
            start: None,
        }
    }

    fn existing(branch: &str) -> SwitchTarget {
        SwitchTarget::Existing(name(branch))
    }

    fn tip(repository: &Repository, branch: &str) -> CommitId {
        repository.refs().branch(&name(branch)).unwrap().unwrap()
    }

    fn working(repository: &Repository) -> AnyCommit {
        let id = repository.refs().working_copy().unwrap();
        read_any_commit(repository.objects(), id).unwrap()
    }

    fn files(repository: &Repository, commit: &AnyCommit) -> Vec<(String, String)> {
        let AnyCommit::Commit(commit) = commit else {
            panic!("a commit without a conflict expected");
        };
        let tree = repository
            .objects()
            .get_tree(&commit.tree())
            .unwrap()
            .unwrap();
        tree.entries()
            .iter()
            .map(|entry| {
                let blob = repository
                    .objects()
                    .get_expecting(&entry.id(), Kind::Blob)
                    .unwrap()
                    .unwrap();
                (
                    entry.name().to_owned(),
                    String::from_utf8(blob.into_body()).unwrap(),
                )
            })
            .collect()
    }

    const BASE: &str = "one\ntwo\nthree\n";

    /// `main` holds `a.txt` with `BASE`; `feature` changes its first line to `ONE`; `other` adds
    /// `c.txt`; `later` follows `feature` and adds `d.txt`. The repository is on `main` with an
    /// empty working-copy commit, and the directory holds the files of `main`.
    fn history(dir: &TempDir, repository: &Repository) {
        write(dir, "a.txt", BASE);
        track(repository, "a.txt");
        commit(repository, "first");
        switch(repository, &create("feature"));
        write(dir, "a.txt", "ONE\ntwo\nthree\n");
        commit(repository, "feature edit");
        switch(repository, &existing("main"));
        switch(repository, &create("other"));
        write(dir, "c.txt", "c\n");
        track(repository, "c.txt");
        commit(repository, "other adds c");
        switch(repository, &existing("feature"));
        switch(repository, &create("later"));
        write(dir, "d.txt", "d\n");
        track(repository, "d.txt");
        commit(repository, "later adds d");
        switch(repository, &existing("main"));
    }

    fn rewrite(repository: &Repository, onto: CommitId) -> Result<Rewritten, RewriteError> {
        repository.rewrite_working_copy(onto, &actor(), &AllowAll)
    }

    fn done(
        result: Result<Rewritten, RewriteError>,
    ) -> (Option<Entry>, Entry, CommitId, Vec<String>) {
        match result {
            Ok(Rewritten::Done {
                snapshot,
                entry,
                working_copy,
                conflicts,
                ..
            }) => (
                snapshot.map(|entry| *entry),
                *entry,
                working_copy,
                conflicts,
            ),
            other => panic!("expected a rewrite, got {other:?}"),
        }
    }

    fn entries(repository: &Repository) -> usize {
        repository.oplog().entries().unwrap().len()
    }

    #[test]
    fn a_clean_carry_moves_the_changes_onto_the_new_base_and_keeps_the_change_id() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let before = working(&repository);
        let count = entries(&repository);

        let (saved, entry, next, conflicts) = done(rewrite(&repository, feature));

        assert!(saved.is_none());
        assert!(conflicts.is_empty());
        assert_eq!(entries(&repository), count + 1, "one entry for the rewrite");
        assert_eq!(entry.change().unwrap().operation().as_str(), "rewrite");
        assert_eq!(repository.refs().working_copy().unwrap(), next);
        let after = working(&repository);
        assert_eq!(after.parents(), [feature]);
        assert_eq!(after.change(), before.change(), "the same change");
        assert_eq!(after.author(), before.author());
        assert_eq!(after.timestamp(), before.timestamp());
        assert_eq!(
            files(&repository, &after),
            [
                ("a.txt".to_owned(), "ONE\ntwo\nthree\n".to_owned()),
                ("b.txt".to_owned(), "b\n".to_owned())
            ]
        );
        assert_eq!(read(&dir, "a.txt"), "ONE\ntwo\nthree\n");
        assert_eq!(read(&dir, "b.txt"), "b\n");
        assert_eq!(
            repository.refs().head().unwrap(),
            crate::refs::Head::Branch(name("main")),
            "HEAD stays"
        );
        // The directory is the working-copy commit: nothing to save, and a repeat has no work.
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
        assert!(matches!(
            rewrite(&repository, feature),
            Ok(Rewritten::Unchanged { snapshot: None })
        ));
        assert_eq!(entries(&repository), count + 1);
    }

    #[test]
    fn unsaved_changes_are_saved_first_and_carried() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let other = tip(&repository, "other");
        write(&dir, "a.txt", "one\ntwo\nTHREE\n");

        let (saved, _, _, conflicts) = done(rewrite(&repository, other));

        assert!(saved.is_some(), "the edit was saved first");
        assert!(conflicts.is_empty());
        assert_eq!(read(&dir, "a.txt"), "one\ntwo\nTHREE\n");
        assert_eq!(read(&dir, "c.txt"), "c\n");
    }

    #[test]
    fn a_carry_with_a_conflict_records_it_and_shows_the_markers() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        write(&dir, "a.txt", "uno\ntwo\nthree\n");
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let before = working(&repository);

        let (_, _, next, conflicts) = done(rewrite(&repository, feature));

        assert_eq!(conflicts, ["a.txt"]);
        let after = working(&repository);
        let AnyCommit::Commit2(_) = &after else {
            panic!("the carry conflicts, so the commit is a commit2");
        };
        assert_eq!(after.id(), next);
        assert_eq!(after.parents(), [feature]);
        assert_eq!(after.change(), before.change());
        assert_eq!(after.tree_sum().sides(), 2);
        let text = read(&dir, "a.txt");
        let expected = "<<<<<<< conflict\n+++++++ side 1\nONE\ntwo\nthree\n------- base 1\n\
                        one\ntwo\nthree\n+++++++ side 2\nuno\ntwo\nthree\n>>>>>>>\n";
        assert_eq!(text, expected);
        assert_eq!(read(&dir, "b.txt"), "b\n", "a decided path holds its file");
    }

    #[test]
    fn a_working_copy_with_an_open_conflict_goes_through_the_same_algebra() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        let later = tip(&repository, "later");
        write(&dir, "a.txt", "uno\ntwo\nthree\n");
        let _ = done(rewrite(&repository, feature));
        let conflicted = working(&repository);
        assert!(conflicted.is_conflicted());

        // `later` follows `feature`, so it keeps the side of the conflict that `feature` is.
        let carried = repository.carry(&conflicted, later, &actor()).unwrap();

        assert_eq!(carried.conflicts, ["a.txt"], "the conflict is carried");
        let commit = read_any_commit(repository.objects(), carried.id).unwrap();
        assert!(commit.is_conflicted());
        assert_eq!(commit.parents(), [later]);
        assert_eq!(commit.change(), conflicted.change());
        // The sum has not grown: the new base only added another path.
        assert_eq!(commit.tree_sum().sides(), 2);
        let tree = repository
            .objects()
            .get_tree(&carried.display)
            .unwrap()
            .unwrap();
        let names: Vec<&str> = tree.entries().iter().map(TreeEntry::name).collect();
        assert_eq!(names, ["a.txt", "d.txt"]);
    }

    #[test]
    fn a_conflict_that_a_new_base_resolves_becomes_a_commit_again() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        write(&dir, "a.txt", "uno\ntwo\nthree\n");
        let _ = done(rewrite(&repository, feature));
        let conflicted = working(&repository);
        // Back onto `main`, the change of `feature` is taken out again, and the sum cancels to
        // the change of the working copy.
        let main = tip(&repository, "main");

        let carried = repository.carry(&conflicted, main, &actor()).unwrap();

        assert!(carried.conflicts.is_empty());
        let commit = read_any_commit(repository.objects(), carried.id).unwrap();
        assert!(!commit.is_conflicted(), "the terms cancelled");
        assert_eq!(files(&repository, &commit)[0].1, "uno\ntwo\nthree\n");
        assert_eq!(commit.change(), conflicted.change());
    }

    #[test]
    fn a_working_copy_without_changes_is_replaced_by_an_empty_commit_with_a_new_change_id() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        let before = working(&repository);

        let (saved, _, _, conflicts) = done(rewrite(&repository, feature));

        assert!(saved.is_none());
        assert!(conflicts.is_empty());
        let after = working(&repository);
        assert_eq!(after.parents(), [feature]);
        assert_ne!(after.change(), before.change(), "a new change");
        assert_eq!(after.message(), "");
        assert_eq!(
            after.tree_sum(),
            read_any_commit(repository.objects(), feature)
                .unwrap()
                .tree_sum(),
            "an empty commit has the tree of its parent"
        );
        assert_eq!(read(&dir, "a.txt"), "ONE\ntwo\nthree\n");
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    #[test]
    fn a_refused_question_or_a_missing_commit_writes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        let count = entries(&repository);

        let error = repository
            .rewrite_working_copy(feature, &actor(), &DenyAll)
            .unwrap_err();
        assert!(matches!(error, RewriteError::Update(_)), "{error}");

        let absent = CommitId::new(crate::object::Blob::new("absent").id().object_id());
        let error = rewrite(&repository, absent).unwrap_err();
        assert!(matches!(error, RewriteError::NoSuchCommit(_)), "{error}");
        assert_eq!(entries(&repository), count);
    }

    #[test]
    fn a_stop_after_the_files_is_finished_by_the_same_call() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let before = repository.refs().working_copy().unwrap();
        // The pointer file cannot be replaced once the entry and the files are written.
        let metadata = repository.metadata_dir();
        let denied = Rc::new(RefCell::new(None));
        let arm = Rc::clone(&denied);
        super::super::snapshot::pause::at_next(
            super::super::snapshot::pause::Stage::Moves,
            move || {
                *arm.borrow_mut() = Some(fault::deny_writes_in(&metadata));
            },
        );

        let error = rewrite(&repository, feature).unwrap_err();
        drop(denied);
        let _ = fs::remove_file(repository.metadata_dir().join("refs.lock"));

        assert!(matches!(error, RewriteError::MoveFailed { .. }), "{error}");
        assert!(!error.pointer_moved());
        let message = error.to_string();
        assert!(message.contains("run the same rewrite again"), "{message}");
        let stopped = error.entry().unwrap().number();
        let planned = error
            .entry()
            .and_then(|entry| entry.change())
            .and_then(Change::working_copy)
            .and_then(Moved::after)
            .copied()
            .unwrap();
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(
            read(&dir, "a.txt"),
            "ONE\ntwo\nthree\n",
            "the files are done"
        );

        // The repeat takes no snapshot, which would save the new files as changes of the old
        // working-copy commit, and it uses the commit of the stopped entry again.
        let (saved, entry, next, _) = done(rewrite(&repository, feature));

        assert!(saved.is_none());
        assert!(entry.number() > stopped);
        assert_eq!(next, planned);
        assert_eq!(repository.refs().working_copy().unwrap(), next);
        assert_eq!(read(&dir, "b.txt"), "b\n");
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    #[test]
    fn a_rewrite_is_restored_like_any_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let before = repository.refs().working_copy().unwrap();
        let (_, entry, _, _) = done(rewrite(&repository, feature));

        repository
            .restore(entry.number(), false, &actor(), &AllowAll)
            .unwrap();

        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(read(&dir, "a.txt"), BASE);
        assert_eq!(read(&dir, "b.txt"), "b\n");
    }

    #[test]
    fn unsaved_changes_are_saved_even_when_the_parent_is_the_new_base_already() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        let _ = done(rewrite(&repository, feature));
        write(&dir, "a.txt", "ONE\ntwo\nTHREE\n");

        let result = rewrite(&repository, feature);

        let Ok(Rewritten::Unchanged { snapshot: Some(_) }) = result else {
            panic!("expected an unchanged parent with a snapshot, got {result:?}");
        };
        assert_eq!(read(&dir, "a.txt"), "ONE\ntwo\nTHREE\n");
        assert!(repository.snapshot(&actor(), &AllowAll).unwrap().is_none());
    }

    #[test]
    fn a_conflict_below_a_directory_is_reported_with_its_full_path() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        fs::create_dir(dir.path().join("dir")).unwrap();
        write(&dir, "dir/a.txt", BASE);
        write(&dir, "dir/s.txt", "sibling\n");
        track(&repository, "dir");
        commit(&repository, "first");
        switch(&repository, &create("feature"));
        write(&dir, "dir/a.txt", "ONE\ntwo\nthree\n");
        commit(&repository, "feature edit");
        switch(&repository, &existing("main"));
        let feature = tip(&repository, "feature");
        write(&dir, "dir/a.txt", "uno\ntwo\nthree\n");
        write(&dir, "dir/s.txt", "changed sibling\n");

        let (_, _, _, conflicts) = done(rewrite(&repository, feature));

        assert_eq!(conflicts, ["dir/a.txt"]);
        assert!(read(&dir, "dir/a.txt").starts_with("<<<<<<< conflict\n"));
        assert_eq!(read(&dir, "dir/s.txt"), "changed sibling\n");
    }

    #[test]
    fn a_stopped_rewrite_blocks_a_rewrite_onto_another_base() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        let other = tip(&repository, "other");
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let metadata = repository.metadata_dir();
        let denied = Rc::new(RefCell::new(None));
        let arm = Rc::clone(&denied);
        super::super::snapshot::pause::at_next(
            super::super::snapshot::pause::Stage::Moves,
            move || {
                *arm.borrow_mut() = Some(fault::deny_writes_in(&metadata));
            },
        );
        let error = rewrite(&repository, feature).unwrap_err();
        drop(denied);
        let _ = fs::remove_file(repository.metadata_dir().join("refs.lock"));
        let stopped = error.entry().unwrap().number();
        assert!(matches!(error, RewriteError::MoveFailed { .. }), "{error}");

        // Another base is refused: a snapshot would save what the stopped rewrite wrote as
        // changes, and the new rewrite would carry them. Nothing is written.
        let count = entries(&repository);
        let error = rewrite(&repository, other).unwrap_err();
        assert!(
            matches!(error, RewriteError::StoppedOnOtherBase { .. }),
            "{error}"
        );
        assert_eq!(entries(&repository), count);
        assert!(error.entry().is_none() && error.snapshot().is_none());

        // The same base finishes it.
        let (saved, entry, _, _) = done(rewrite(&repository, feature));
        assert!(saved.is_none(), "the repeat takes no snapshot");
        assert!(entry.number() > stopped);
    }

    #[test]
    fn edits_in_different_places_of_one_file_merge_without_a_conflict() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        write(&dir, "a.txt", "one\ntwo\nTHREE\n");

        let (_, _, next, conflicts) = done(rewrite(&repository, feature));

        assert!(conflicts.is_empty());
        assert_eq!(read(&dir, "a.txt"), "ONE\ntwo\nTHREE\n");
        assert!(matches!(
            read_any_commit(repository.objects(), next).unwrap(),
            AnyCommit::Commit(_)
        ));
    }

    #[test]
    fn a_deletion_is_carried_onto_the_new_base() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let other = tip(&repository, "other");
        fs::remove_file(dir.path().join("a.txt")).unwrap();

        let (_, _, next, conflicts) = done(rewrite(&repository, other));

        assert!(conflicts.is_empty());
        assert!(!dir.path().join("a.txt").exists());
        assert_eq!(read(&dir, "c.txt"), "c\n");
        let names: Vec<String> = files(&repository, &working(&repository))
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["c.txt"]);
        assert_eq!(repository.refs().working_copy().unwrap(), next);
    }

    #[test]
    fn a_rewrite_onto_a_commit_of_the_same_change_is_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let itself = repository.refs().working_copy().unwrap();
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let count = entries(&repository);

        let error = rewrite(&repository, itself).unwrap_err();

        let inner = match &error {
            RewriteError::AfterSnapshot { source, .. } => source,
            other => other,
        };
        assert!(matches!(inner, RewriteError::SameChange { .. }), "{error}");
        // Only the snapshot of the unsaved file was written.
        assert!(entries(&repository) <= count + 1);
    }

    #[test]
    fn another_actor_does_not_finish_a_stopped_rewrite() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        write(&dir, "b.txt", "b\n");
        track(&repository, "b.txt");
        let metadata = repository.metadata_dir();
        let denied = Rc::new(RefCell::new(None));
        let arm = Rc::clone(&denied);
        super::super::snapshot::pause::at_next(
            super::super::snapshot::pause::Stage::Moves,
            move || {
                *arm.borrow_mut() = Some(fault::deny_writes_in(&metadata));
            },
        );
        let _ = rewrite(&repository, feature).unwrap_err();
        drop(denied);
        let _ = fs::remove_file(repository.metadata_dir().join("refs.lock"));
        let before = repository.refs().working_copy().unwrap();
        let count = entries(&repository);
        let mara = Actor::human("mara").unwrap();

        let error = repository
            .rewrite_working_copy(feature, &mara, &AllowAll)
            .unwrap_err();

        assert!(
            matches!(
                error,
                RewriteError::Snapshot(SnapshotError::NotOwner { .. })
            ),
            "{error}"
        );
        assert_eq!(repository.refs().working_copy().unwrap(), before);
        assert_eq!(entries(&repository), count);
    }

    #[test]
    fn a_rewrite_that_ends_in_a_conflict_is_refused_by_the_snapshot_until_it_handles_one() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let feature = tip(&repository, "feature");
        let main = tip(&repository, "main");
        write(&dir, "a.txt", "uno\ntwo\nthree\n");
        let (_, entry, conflicted, _) = done(rewrite(&repository, feature));
        let count = entries(&repository);

        let error = rewrite(&repository, main).unwrap_err();
        assert!(
            matches!(
                error,
                RewriteError::Snapshot(SnapshotError::Conflicted { id }) if id == conflicted
            ),
            "{error}"
        );
        assert!(matches!(
            repository.restore(entry.number(), false, &actor(), &AllowAll),
            Err(crate::repository::RestoreError::Snapshot(
                SnapshotError::Conflicted { .. }
            ))
        ));
        assert_eq!(entries(&repository), count, "both wrote nothing");
        assert_eq!(repository.refs().working_copy().unwrap(), conflicted);
    }
}
