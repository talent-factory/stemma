//! The plan of a purge: removing a secret from the history and the operation log (ADR-014).
//!
//! A purge takes the ID of a blob. Every commit whose tree holds the blob, at any depth, is
//! rewritten with a tree without it (for a `commit2`, every tree of its list, in the form that its
//! sum has afterwards), and every commit after it with the new parents; the change ID stays.
//! The entries of the log that name a rewritten commit are replaced by stubs. The objects
//! that the rewrite replaced are deleted, and so are leftovers in the store that hold the blob or
//! come after a commit that does. The plan fails if anything that is kept still reaches them.
//!
//! This module makes the plan and writes nothing (STEMMA-46). The log knows the stubs and the
//! `purge` entry (STEMMA-47); [`apply`] carries the plan out, with the journal of [`journal`], and
//! finishes or undoes a purge that stopped (STEMMA-48).

mod apply;
mod journal;
#[cfg(any(fuzzing, test))]
pub(crate) use journal::fuzz as fuzz_journal;

pub use apply::{Aborted, ApplyPurgeError, PurgeFailure, PurgeStep, Purged};
pub use journal::JournalError;

use std::collections::{hash_map, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;

use super::conflicted;
use super::snapshot::{read_any_commit, read_object, SnapshotError};
use super::{Repository, UpdateError};
use crate::conflict::{AnyCommit, FromSumError, Merge};
use crate::object::{
    BlobId, Commit2Error, CommitError, CommitId, FileMode, Kind, ObjectId, Tree, TreeEntry, TreeId,
};
use crate::oplog::{Entry, Moved, OpLogError, OperationName};
use crate::refs::{BranchName, Head, RefError};
use crate::store::{Batch, FsObjectStore, ObjectStore, StoreError};
use crate::{Actor, ActorRef, Allowed, Authorizer};

/// The operation that the authorizer is asked for.
const OPERATION: &str = "purge";

/// What a purge would do: the commits it rewrites, the pointers it moves, the entries it replaces
/// by stubs, and the objects it deletes.
///
/// The plan is made from the pointers and the log as they are now. The purge itself
/// ([`Repository::apply_purge`]) saves the directory in a snapshot first and plans again under the
/// lock, so its plan can differ (ADR-014).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgePlan {
    blob: BlobId,
    rewrites: Vec<(CommitId, CommitId)>,
    heads: Vec<(String, Moved<Head>)>,
    working_copies: Vec<(String, Moved<CommitId>)>,
    branches: Vec<(BranchName, Moved<CommitId>)>,
    entries: Vec<u64>,
    commits: Vec<CommitId>,
    trees: Vec<TreeId>,
    blobs: Vec<BlobId>,
}

impl PurgePlan {
    /// The blob that is purged.
    pub fn blob(&self) -> BlobId {
        self.blob
    }

    /// The commits that are rewritten, each with the ID of its new version, parents before their
    /// children. The new commits keep the change ID, the author, the time, and the message.
    pub fn rewrites(&self) -> &[(CommitId, CommitId)] {
        &self.rewrites
    }

    /// The moves of HEAD of the worktrees whose HEAD names a rewritten commit, each with the name
    /// of its worktree (`main` first, then the linked ones by name).
    pub fn heads(&self) -> &[(String, Moved<Head>)] {
        &self.heads
    }

    /// The moves of the working-copy commits that are rewritten, each with the name of its
    /// worktree (`main` first, then the linked ones by name).
    pub fn working_copies(&self) -> &[(String, Moved<CommitId>)] {
        &self.working_copies
    }

    /// The move of HEAD of the worktree `worktree`, if it names a rewritten commit.
    pub fn head(&self, worktree: &str) -> Option<&Moved<Head>> {
        self.heads
            .iter()
            .find(|(name, _)| name == worktree)
            .map(|(_, moved)| moved)
    }

    /// The move of the working-copy commit of the worktree `worktree`, if it is rewritten.
    pub fn working_copy(&self, worktree: &str) -> Option<&Moved<CommitId>> {
        self.working_copies
            .iter()
            .find(|(name, _)| name == worktree)
            .map(|(_, moved)| moved)
    }

    /// The branches that move, sorted by name.
    pub fn branches(&self) -> &[(BranchName, Moved<CommitId>)] {
        &self.branches
    }

    /// The numbers of the entries of the log that are replaced by stubs, in ascending order: every
    /// entry that names a rewritten commit as an old or a new value.
    pub fn entries(&self) -> &[u64] {
        &self.entries
    }

    /// The commits that are deleted, sorted by ID: the old versions of the rewritten commits, and
    /// the commits in the store that no pointer and no entry reaches and that hold the blob or
    /// come after a commit that does. Such a leftover is deleted and not rewritten, as nothing
    /// names it.
    pub fn deleted_commits(&self) -> &[CommitId] {
        &self.commits
    }

    /// The trees that are deleted, sorted by ID: every tree that holds the blob, at any depth, in
    /// a commit or left in the store. Nothing that is kept reaches them; the plan checks it.
    pub fn deleted_trees(&self) -> &[TreeId] {
        &self.trees
    }

    /// The blobs that are deleted besides the purged one, sorted by ID: the texts with markers
    /// that the working-copy view of a rewritten conflict stored. They hold the content of every
    /// side, so a copy of the secret can be among them (STEMMA-195). The trees that name them
    /// are among [`deleted_trees`](Self::deleted_trees). A view is derived data, so the next
    /// command that needs one stores it again.
    pub fn deleted_blobs(&self) -> &[BlobId] {
        &self.blobs
    }
}

impl Repository {
    /// Plans the purge of a blob and writes nothing (ADR-014).
    ///
    /// The actor must be a human: an agent is refused before anything else, whatever the
    /// authorizer says, because a purge removes history that cannot be brought back. Then the
    /// authorizer is asked for `purge`, with the blob as the target. The plan starts from the
    /// pointers (HEAD, the working-copy commit, the branches, the tags) and from every commit
    /// that an entry of the log names, and follows the parents. A tree that becomes empty leaves its parent
    /// tree, as a snapshot never makes an empty directory; the tree of a commit can become empty.
    /// Last, the store is scanned for trees and commits that no pointer and no entry reaches,
    /// such as those a crash left behind. One that holds the blob, or a commit that comes after
    /// one, joins the delete set. The plan then checks that nothing that is kept holds the blob or
    /// reaches what it deletes.
    ///
    /// # Errors
    ///
    /// [`PurgeError::Agent`] for an agent, [`PurgeError::Update`] for a refusal of the authorizer,
    /// [`PurgeError::NoSuchBlob`] if the store has no blob with the ID,
    /// [`PurgeError::BlobUnreadable`] if its file cannot be read or is damaged,
    /// [`PurgeError::Refs`] and [`PurgeError::Log`] if the pointers or the log cannot be read,
    /// [`PurgeError::Read`] for a
    /// commit or a tree that is missing or cannot be read, [`PurgeError::Store`] if the store
    /// cannot be listed or an object in it is damaged, [`PurgeError::DuplicateParent`] if a
    /// rewrite makes two parents of a commit the same, [`PurgeError::Leftover`] for one of these
    /// failures in an object that only the scan found, [`PurgeError::TooManyEntries`] if the
    /// plan would replace more entries of the log than a journal lists, and
    /// [`PurgeError::Unsound`] if the check fails, and [`PurgeError::TaggedCommit`] if a tag
    /// points to a commit that the purge would rewrite, because a tag never moves (ADR-008).
    pub fn plan_purge(
        &self,
        blob: ObjectId,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<PurgePlan, PurgeError> {
        let (blob, _) = self.check_purge(blob, actor, authorizer)?;
        self.plan_blob(blob).map(|(plan, _)| plan)
    }

    /// Refuses an agent, asks the authorizer, and finds the blob: what comes before a plan. The
    /// proof of the authorizer goes with the blob, for a purge that goes on to write.
    fn check_purge(
        &self,
        blob: ObjectId,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<(BlobId, Allowed), PurgeError> {
        refuse_agent(actor)?;
        let allowed = self.authorize_purge(actor, authorizer, &blob.to_string())?;
        match self.objects().resolve_blob(&blob) {
            Ok(Some(found)) => Ok((found, allowed)),
            Ok(None) => Err(PurgeError::NoSuchBlob {
                id: blob,
                found: None,
            }),
            Err(StoreError::KindMismatch { found, .. }) => Err(PurgeError::NoSuchBlob {
                id: blob,
                found: Some(found),
            }),
            Err(source) => Err(PurgeError::BlobUnreadable {
                id: blob,
                source: Box::new(source),
            }),
        }
    }

    /// Asks the authorizer for `purge`, with the blob as the target.
    fn authorize_purge(
        &self,
        actor: &Actor,
        authorizer: &dyn Authorizer,
        target: &str,
    ) -> Result<Allowed, PurgeError> {
        let operation =
            OperationName::try_new(OPERATION).expect("`purge` is a valid operation name");
        self.authorize(actor, authorizer, &operation, target)
            .map_err(PurgeError::Update)
    }

    /// Plans the purge of a blob that is in the store, and returns the planner with the new
    /// objects in memory as well.
    fn plan_blob(&self, blob: BlobId) -> Result<(PurgePlan, Planner<'_>), PurgeError> {
        let refs = self.refs();
        // HEAD and the working-copy commit of every worktree are roots: a purge in one worktree
        // must not delete what another worktree needs, and it moves what it rewrites there too
        // (ADR-014).
        let worktrees = self.refs_store().all_pointers().map_err(PurgeError::Refs)?;
        let branches = refs.branches().map_err(PurgeError::Refs)?;
        let tags = refs.tags().map_err(PurgeError::Refs)?;
        let log = self.oplog().entries().map_err(PurgeError::Log)?;

        let mut planner = Planner::new(self.objects(), blob);
        let mut roots: Vec<CommitId> = worktrees.iter().map(|(_, _, commit)| *commit).collect();
        roots.extend(branches.iter().map(|(_, commit)| *commit));
        roots.extend(tags.iter().map(|(_, commit)| *commit));
        for (_, head, _) in &worktrees {
            if let Head::Detached(commit) = head {
                roots.push(*commit);
            }
        }
        roots.extend(log.iter().flat_map(|entry| entry.commits()));
        for root in &roots {
            planner.commit(*root)?;
        }
        let rewrites = planner.rewrites.clone();
        let rewritten: HashSet<CommitId> = rewrites.iter().map(|(old, _)| *old).collect();
        // A tag never moves (ADR-008), so a purge cannot rewrite the commit of a tag, and a tag
        // that is left would keep the secret reachable. The purge is refused until the tag is
        // dealt with (ADR-014).
        check_tags(&tags, &rewritten)?;
        let new = |commit: CommitId| planner.commits[&commit];

        let working_copy_moves: Vec<(String, Moved<CommitId>)> = worktrees
            .iter()
            .filter(|(_, _, commit)| rewritten.contains(commit))
            .map(|(name, _, commit)| {
                (
                    name.clone(),
                    Moved::from_to(Some(*commit), Some(new(*commit))),
                )
            })
            .collect();
        let head_moves: Vec<(String, Moved<Head>)> = worktrees
            .iter()
            .filter_map(|(name, head, _)| match head {
                Head::Detached(commit) if rewritten.contains(commit) => Some((
                    name.clone(),
                    Moved::from_to(Some(head.clone()), Some(Head::Detached(new(*commit)))),
                )),
                _ => None,
            })
            .collect();
        let branch_moves: Vec<(BranchName, Moved<CommitId>)> = branches
            .iter()
            .filter(|(_, commit)| rewritten.contains(commit))
            .map(|(name, commit)| {
                (
                    name.clone(),
                    Moved::from_to(Some(*commit), Some(new(*commit))),
                )
            })
            .collect();
        let (replaced, kept): (Vec<&Entry>, Vec<&Entry>) = log.iter().partition(|entry| {
            entry
                .commits()
                .iter()
                .any(|commit| rewritten.contains(commit))
        });

        // The roots that stay: the pointers after the purge, and the entries that are not
        // replaced.
        let mut kept_roots: Vec<CommitId> = worktrees
            .iter()
            .map(|(_, _, commit)| new(*commit))
            .collect();
        kept_roots.extend(branches.iter().map(|(_, commit)| new(*commit)));
        kept_roots.extend(tags.iter().map(|(_, commit)| new(*commit)));
        for (_, head, _) in &worktrees {
            if let Head::Detached(commit) = head {
                kept_roots.push(new(*commit));
            }
        }
        kept_roots.extend(kept.iter().flat_map(|entry| entry.commits()));

        planner.scan()?;
        let mut commits: Vec<CommitId> = planner.rewrites.iter().map(|(old, _)| *old).collect();
        commits.sort();
        let mut trees: Vec<TreeId> = planner
            .trees
            .iter()
            .filter(|(_, new)| new.is_some())
            .map(|(old, _)| *old)
            .collect();
        trees.sort();
        let reached = planner.check(&kept_roots, &commits, &trees)?;
        let views = planner.views(&reached, &trees, &rewrites)?;
        trees.extend(views.trees);
        trees.sort();
        trees.dedup();
        // A log with 2^24 entries is too large for a test, so no test notices if this goes.
        // Without it, a purge writes a journal that `--resume` and `--abort` cannot read.
        check_entries(replaced.len())?;

        let plan = PurgePlan {
            blob,
            rewrites,
            heads: head_moves,
            working_copies: working_copy_moves,
            branches: branch_moves,
            entries: replaced.iter().map(|entry| entry.number()).collect(),
            commits,
            trees,
            blobs: views.blobs,
        };
        Ok((plan, planner))
    }
}

/// Refuses a plan that replaces more entries of the log than the reader of the journal accepts,
/// so that no purge writes a journal that it cannot read back to resume or abort.
fn check_entries(count: usize) -> Result<(), PurgeError> {
    if count as u64 > journal::MAX_STUBS {
        return Err(PurgeError::TooManyEntries { count });
    }
    Ok(())
}

/// Refuses an agent: only a human may purge (ADR-014).
fn refuse_agent(actor: &Actor) -> Result<(), PurgeError> {
    match actor.view() {
        ActorRef::Agent { .. } => Err(PurgeError::Agent),
        _ => Ok(()),
    }
}

/// A tree or a commit that a purge writes.
#[derive(Debug, Clone)]
enum NewObject {
    Tree(Tree),
    Commit(AnyCommit),
}

impl NewObject {
    fn id(&self) -> ObjectId {
        match self {
            Self::Tree(tree) => tree.id().object_id(),
            Self::Commit(commit) => commit.id().object_id(),
        }
    }

    /// Stores the object; one that the store has already is left alone (ADR-007).
    fn put(&self, objects: &Batch<'_>) -> Result<(), StoreError> {
        match self {
            Self::Tree(tree) => objects.put_tree(tree).map(|_| ()),
            Self::Commit(AnyCommit::Commit(commit)) => objects.put_commit(commit).map(|_| ()),
            Self::Commit(AnyCommit::Commit2(commit)) => {
                objects.put_raw(Kind::Commit2, &commit.encode()).map(|_| ())
            }
        }
    }
}

/// Where a tree holds the blob: the names of its entries that are the blob, and the names and IDs
/// of its subtrees that hold it.
#[derive(Debug, Clone)]
struct Places {
    files: Vec<String>,
    directories: Vec<(String, TreeId)>,
}

/// Rewrites commits and trees in memory, with a memo, so that each object is read and rewritten
/// once. It keeps the links of every commit and tree it has seen, old and new, so that the walk of
/// what is kept does not read the history a second time.
struct Planner<'a> {
    objects: &'a FsObjectStore,
    blob: BlobId,
    /// Every tree that was looked at, with the ID of its new version if it holds the blob.
    trees: HashMap<TreeId, Option<TreeId>>,
    /// The subtrees of every tree, old and new, and whether it names the blob itself.
    tree_links: HashMap<TreeId, (Vec<TreeId>, bool)>,
    /// The IDs of the files of every tree, old and new, that the walk of what is kept needs to
    /// know which blobs are reached.
    tree_files: HashMap<TreeId, Vec<ObjectId>>,
    /// The new trees, by their IDs.
    new_trees: HashMap<TreeId, Tree>,
    /// Where each tree that holds the blob holds it, for [`paths`](Self::paths).
    places: HashMap<TreeId, Places>,
    /// Every commit that was looked at, with the ID of its new version (the same ID if it does
    /// not change).
    commits: HashMap<CommitId, CommitId>,
    /// The trees (one for a `commit`, the list for a `commit2`) and the parents of every commit,
    /// old and new.
    commit_links: HashMap<CommitId, (Vec<TreeId>, Vec<CommitId>)>,
    /// The new commits, by their IDs.
    new_commits: HashMap<CommitId, AnyCommit>,
    /// The commits that change, parents first.
    rewrites: Vec<(CommitId, CommitId)>,
}

impl<'a> Planner<'a> {
    fn new(objects: &'a FsObjectStore, blob: BlobId) -> Self {
        Self {
            objects,
            blob,
            trees: HashMap::new(),
            tree_links: HashMap::new(),
            tree_files: HashMap::new(),
            new_trees: HashMap::new(),
            places: HashMap::new(),
            commits: HashMap::new(),
            commit_links: HashMap::new(),
            new_commits: HashMap::new(),
            rewrites: Vec::new(),
        }
    }

    /// Rewrites the commit and its ancestors. The history can be long, so the walk keeps its own
    /// stack instead of recursing: a commit is done once all its parents are done.
    fn commit(&mut self, root: CommitId) -> Result<(), PurgeError> {
        let mut pending = vec![root];
        let mut read: HashMap<CommitId, AnyCommit> = HashMap::new();
        while let Some(&id) = pending.last() {
            if self.commits.contains_key(&id) {
                pending.pop();
                continue;
            }
            let commit = match read.entry(id) {
                hash_map::Entry::Occupied(known) => known.into_mut(),
                hash_map::Entry::Vacant(slot) => {
                    slot.insert(read_any_commit(self.objects, id).map_err(PurgeError::Read)?)
                }
            };
            let missing: Vec<CommitId> = commit
                .parents()
                .iter()
                .filter(|parent| !self.commits.contains_key(parent))
                .copied()
                .collect();
            if !missing.is_empty() {
                pending.extend(missing);
                continue;
            }
            pending.pop();
            let commit = read.remove(&id).expect("the commit was read above");
            // A `commit` has one tree and a `commit2` a list of them; every one of the list may
            // hold the blob.
            let sum = commit.tree_sum();
            self.commit_links
                .insert(id, (sum.terms().to_vec(), commit.parents().to_vec()));
            let parents: Vec<CommitId> = commit
                .parents()
                .iter()
                .map(|parent| self.commits[parent])
                .collect();
            let mut changed = false;
            let mut terms = Vec::with_capacity(sum.terms().len());
            for &tree in sum.terms() {
                let new = self.tree(tree)?;
                changed |= new.is_some();
                terms.push(new.unwrap_or(tree));
            }
            if !changed && parents == commit.parents() {
                self.commits.insert(id, id);
                continue;
            }
            let terms = Merge::try_from_terms(terms).expect("a rewrite keeps the number of trees");
            // The form of the result follows from the sum: a base and a side that the rewrite
            // makes equal cancel, so that a conflict can end up as a `commit`.
            let rewritten = commit
                .reparent(&terms, parents)
                .map_err(|error| match error {
                    FromSumError::Commit(CommitError::DuplicateParent(parent))
                    | FromSumError::Commit2(Commit2Error::DuplicateParent(parent)) => {
                        PurgeError::DuplicateParent { commit: id, parent }
                    }
                    source => PurgeError::Rewrite { commit: id, source },
                })?;
            let new = rewritten.id();
            self.commits.insert(id, new);
            self.commit_links.insert(
                new,
                (
                    rewritten.tree_sum().into_terms(),
                    rewritten.parents().to_vec(),
                ),
            );
            self.new_commits.insert(new, rewritten);
            self.rewrites.push((id, new));
        }
        Ok(())
    }

    /// Returns the ID of the new version of the tree, or `None` if it does not hold the blob. A
    /// tree is as deep as the directories, so the recursion is bounded by the paths.
    fn tree(&mut self, id: TreeId) -> Result<Option<TreeId>, PurgeError> {
        if let Some(known) = self.trees.get(&id) {
            return Ok(*known);
        }
        let tree = read_tree(self.objects, id)?;
        let mut changed = false;
        let mut holds_blob = false;
        let mut old_subtrees = Vec::new();
        let mut new_subtrees = Vec::new();
        let mut entries = Vec::with_capacity(tree.entries().len());
        let mut files = Vec::new();
        let mut directories = Vec::new();
        for entry in tree.entries() {
            if FileMode::from_entry_mode(entry.mode()).is_some() {
                if entry.id() == self.blob.object_id() {
                    changed = true;
                    holds_blob = true;
                    files.push(entry.name().to_owned());
                } else {
                    entries.push(entry.clone());
                }
                continue;
            }
            let subtree = TreeId::new(entry.id());
            old_subtrees.push(subtree);
            match self.tree(subtree)? {
                None => {
                    entries.push(entry.clone());
                    new_subtrees.push(subtree);
                }
                Some(new) => {
                    changed = true;
                    directories.push((entry.name().to_owned(), subtree));
                    if !self.new_trees[&new].entries().is_empty() {
                        entries.push(
                            TreeEntry::directory(entry.name(), new)
                                .expect("the name is that of a valid entry"),
                        );
                        new_subtrees.push(new);
                    }
                }
            }
        }
        self.tree_links.insert(id, (old_subtrees, holds_blob));
        self.tree_files.insert(
            id,
            tree.entries()
                .iter()
                .filter(|entry| FileMode::from_entry_mode(entry.mode()).is_some())
                .map(TreeEntry::id)
                .collect(),
        );
        if !changed {
            self.trees.insert(id, None);
            return Ok(None);
        }
        self.places.insert(id, Places { files, directories });
        let new = Tree::try_new(entries).expect("the entries of a valid tree");
        let new_id = new.id();
        self.tree_links.insert(new_id, (new_subtrees, false));
        self.tree_files.insert(
            new_id,
            new.entries()
                .iter()
                .filter(|entry| FileMode::from_entry_mode(entry.mode()).is_some())
                .map(TreeEntry::id)
                .collect(),
        );
        self.new_trees.insert(new_id, new);
        self.trees.insert(id, Some(new_id));
        Ok(Some(new_id))
    }

    /// The paths at which the commits that the plan rewrites held the blob, sorted and each
    /// once. A tree that only the store holds names no path, so it adds none.
    fn paths(&self) -> Vec<String> {
        let mut paths = BTreeSet::new();
        let mut seen = HashSet::new();
        let mut pending: Vec<(TreeId, String)> = self
            .rewrites
            .iter()
            .flat_map(|(old, _)| &self.commit_links[old].0)
            .map(|tree| (*tree, String::new()))
            .collect();
        let join = |prefix: &str, name: &str| {
            if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            }
        };
        while let Some((tree, prefix)) = pending.pop() {
            let Some(Places { files, directories }) = self.places.get(&tree) else {
                continue;
            };
            if !seen.insert((tree, prefix.clone())) {
                continue;
            }
            paths.extend(files.iter().map(|name| join(&prefix, name)));
            pending.extend(
                directories
                    .iter()
                    .map(|(name, subtree)| (*subtree, join(&prefix, name))),
            );
        }
        paths.into_iter().collect()
    }

    /// Looks at every tree and commit in the store that the walk from the roots did not reach, so
    /// that one that holds the blob, or that comes after a commit that does, joins the delete set.
    /// Only the header of an object is read to learn its kind, so a blob is not read. A damaged
    /// tree or commit is an error, as it may name the blob, unless the damage turned its header
    /// into that of a blob: then it is taken for one (see the limits in ADR-014).
    fn scan(&mut self) -> Result<(), PurgeError> {
        for id in self.objects.ids().map_err(store)? {
            let known = self.trees.contains_key(&TreeId::new(id))
                || self.commits.contains_key(&CommitId::new(id))
                || id == self.blob.object_id();
            if known {
                continue;
            }
            let leftover = |source| PurgeError::Leftover {
                id,
                source: Box::new(source),
            };
            // An object that is gone since the listing names nothing any more.
            let Some(kind) = self.objects.kind_of(&id).map_err(|e| leftover(store(e)))? else {
                continue;
            };
            match kind {
                Kind::Blob => {}
                Kind::Tree => {
                    self.tree(TreeId::new(id)).map_err(leftover)?;
                }
                // Every kind is matched, so that a kind added later does not compile until the
                // plan follows it: skipping it would leave a secret that it may reach.
                Kind::Commit | Kind::Commit2 => self.commit(CommitId::new(id)).map_err(leftover)?,
            }
        }
        Ok(())
    }

    /// Checks that nothing that is kept holds the blob or reaches what is deleted. By
    /// construction it cannot: every commit with the blob is rewritten, and so is every commit
    /// after it. The check makes a mistake in that reasoning an error instead of a secret that
    /// stays. A kept tree that holds the blob is found by the walk; the comparison with the delete
    /// set is a second net for links that disagree with the delete set, and does not fire today.
    /// On success it returns the set of objects that the kept roots reach (see `reached`).
    fn check(
        &mut self,
        kept: &[CommitId],
        commits: &[CommitId],
        trees: &[TreeId],
    ) -> Result<HashSet<ObjectId>, PurgeError> {
        let reached = self.reached(kept)?;
        let deleted = commits
            .iter()
            .map(|commit| commit.object_id())
            .chain(trees.iter().map(|tree| tree.object_id()));
        match deleted.into_iter().find(|id| reached.contains(id)) {
            Some(id) => Err(PurgeError::Unsound { id }),
            None => Ok(reached),
        }
    }

    /// The objects that the working-copy views of the rewritten conflicts stored (ADR-014): the
    /// text with markers of each conflicted path holds the content of every side, so a view of a
    /// commit that held the blob can hold a copy of it. The commits that are deleted are the
    /// ones that held it, and the views are computed from them, so the plan stays free of writes.
    /// An object that something kept reaches stays, as it is no longer only a view: a tracked
    /// file with the same bytes as a text with markers, for example. So does an object that a
    /// tree names which stays, stored or written by the purge, such as the stored view of a kept
    /// conflict commit with the same sides: deleting it would leave that tree dangling and make
    /// the next scan fail. A new tree that is only in memory (a rewritten leftover) does not
    /// count, because it is never written. `deleted` are the trees that the purge deletes anyway,
    /// and `written` are the rewrites that the purge writes (without the leftovers).
    fn views(
        &self,
        reached: &HashSet<ObjectId>,
        deleted: &[TreeId],
        written: &[(CommitId, CommitId)],
    ) -> Result<Views, PurgeError> {
        let mut blobs = BTreeSet::new();
        let mut trees = BTreeSet::new();
        for (old, _) in &self.rewrites {
            let terms = self.commit_links[old].0.clone();
            if terms.len() < 2 {
                continue;
            }
            let sum = Merge::try_from_terms(terms).expect("the trees of a commit are a sum");
            let found = conflicted::view_objects(self.objects, &sum).map_err(|source| {
                PurgeError::View {
                    commit: *old,
                    source: Box::new(source),
                }
            })?;
            blobs.extend(found.blobs);
            trees.extend(found.trees);
        }
        // The purged blob goes with the delete set already.
        blobs.remove(&self.blob);
        // Only what a command stored is deleted: the view of most commits was never shown.
        let stored = |id: ObjectId| self.objects.contains(&id).map_err(store);
        let mut views = Views {
            blobs: Vec::new(),
            trees: Vec::new(),
        };
        for blob in blobs {
            if !reached.contains(&blob.object_id()) && stored(blob.object_id())? {
                views.blobs.push(blob);
            }
        }
        for tree in trees {
            if !reached.contains(&tree.object_id()) && stored(tree.object_id())? {
                views.trees.push(tree);
            }
        }
        // A tree that stays holds what it names. Keeping a view tree makes it one of those, so
        // repeat until nothing more is kept.
        let deleted: HashSet<TreeId> = deleted.iter().copied().collect();
        let mut writes: HashSet<TreeId> = HashSet::new();
        let mut pending: Vec<TreeId> = written
            .iter()
            .flat_map(|(_, new)| self.new_commits[new].tree_sum().terms().to_vec())
            .collect();
        while let Some(id) = pending.pop() {
            if self.new_trees.contains_key(&id) && writes.insert(id) {
                pending.extend(&self.tree_links[&id].0);
            }
        }
        loop {
            let mut shielded: HashSet<ObjectId> = HashSet::new();
            for (id, (subtrees, _)) in &self.tree_links {
                // A new tree that is not written and not stored is only in memory (a rewritten
                // leftover).
                let unwritten = self.new_trees.contains_key(id)
                    && !writes.contains(id)
                    && !self.objects.contains(&id.object_id()).map_err(store)?;
                let goes = unwritten || deleted.contains(id) || views.trees.contains(id);
                if !goes {
                    shielded.extend(subtrees.iter().map(|tree| tree.object_id()));
                    shielded.extend(self.tree_files[id].iter().copied());
                }
            }
            let (blobs, trees) = (views.blobs.len(), views.trees.len());
            views
                .blobs
                .retain(|blob| !shielded.contains(&blob.object_id()));
            views
                .trees
                .retain(|tree| !shielded.contains(&tree.object_id()));
            if (blobs, trees) == (views.blobs.len(), views.trees.len()) {
                break;
            }
        }
        Ok(views)
    }

    /// The commits, the trees, and the files named by those trees that the roots reach after the
    /// purge, through the parents and the trees, from the links in memory. A root that the plan
    /// did not walk is read from the store.
    ///
    /// # Errors
    ///
    /// Returns [`PurgeError::Unsound`] if a tree that is reached names the blob.
    fn reached(&mut self, roots: &[CommitId]) -> Result<HashSet<ObjectId>, PurgeError> {
        let mut seen = HashSet::new();
        let mut commits: Vec<CommitId> = roots.to_vec();
        let mut trees: Vec<TreeId> = Vec::new();
        while let Some(id) = commits.pop() {
            if !seen.insert(id.object_id()) {
                continue;
            }
            if !self.commit_links.contains_key(&id) {
                self.commit(id)?;
            }
            let (listed, parents) = &self.commit_links[&id];
            trees.extend(listed);
            commits.extend(parents);
        }
        while let Some(id) = trees.pop() {
            if !seen.insert(id.object_id()) {
                continue;
            }
            if !self.tree_links.contains_key(&id) {
                self.tree(id)?;
            }
            let (subtrees, holds_blob) = &self.tree_links[&id];
            if *holds_blob {
                return Err(PurgeError::Unsound { id: id.object_id() });
            }
            trees.extend(subtrees);
            seen.extend(self.tree_files[&id].iter().copied());
        }
        Ok(seen)
    }

    /// The new trees and commits of the rewrites, in the order in which they are written: the
    /// subtrees before their tree, the tree before its commit, and the parents first. A leftover
    /// that the scan rewrote in memory is not among them, as it is deleted and not rewritten.
    fn new_objects(&self, rewrites: &[(CommitId, CommitId)]) -> Vec<NewObject> {
        let mut objects = Vec::new();
        let mut seen = HashSet::new();
        for (_, new) in rewrites {
            let commit = &self.new_commits[new];
            for tree in commit.tree_sum().terms() {
                self.push_tree(*tree, &mut seen, &mut objects);
            }
            objects.push(NewObject::Commit(commit.clone()));
        }
        objects
    }

    /// Adds a new tree after its new subtrees. A tree that is not new is in the store already.
    fn push_tree(&self, id: TreeId, seen: &mut HashSet<TreeId>, objects: &mut Vec<NewObject>) {
        let Some(tree) = self.new_trees.get(&id) else {
            return;
        };
        if !seen.insert(id) {
            return;
        }
        for subtree in &self.tree_links[&id].0 {
            self.push_tree(*subtree, seen, objects);
        }
        objects.push(NewObject::Tree(tree.clone()));
    }
}

/// The objects of the views of the rewritten conflicts that the purge deletes.
struct Views {
    blobs: Vec<BlobId>,
    trees: Vec<TreeId>,
}

/// Wraps an error of the store.
fn store(error: StoreError) -> PurgeError {
    PurgeError::Store(Box::new(error))
}

/// Reads and parses a tree, or fails if the store lacks it.
fn read_tree(objects: &FsObjectStore, id: TreeId) -> Result<Tree, PurgeError> {
    let raw = read_object(objects, id.object_id(), Kind::Tree).map_err(PurgeError::Read)?;
    Tree::parse(raw.body()).map_err(|source| PurgeError::Read(SnapshotError::Tree { id, source }))
}

/// Why [`Repository::plan_purge`] fails. A plan writes nothing, so no failure leaves a trace.
#[derive(Debug)]
#[non_exhaustive]
pub enum PurgeError {
    /// The actor is an agent. Only a human may purge (ADR-014); a human sponsor can run it.
    Agent,
    /// The authorizer refused `purge`.
    Update(UpdateError),
    /// The store has no blob with the ID: it is absent, or it is another kind of object.
    NoSuchBlob {
        /// The ID.
        id: ObjectId,
        /// The kind of the object, if the store has one with the ID.
        found: Option<Kind>,
    },
    /// The store has a file for the blob, but it cannot be read or is damaged, so it is not known
    /// that it is a blob.
    BlobUnreadable {
        /// The ID.
        id: ObjectId,
        /// Why.
        source: Box<StoreError>,
    },
    /// The pointers cannot be read.
    Refs(RefError),
    /// The operation log cannot be read.
    Log(OpLogError),
    /// A commit or a tree is missing, or it cannot be read.
    Read(SnapshotError),
    /// The store cannot be listed, or an object in it is damaged, so it is not known whether it
    /// names the blob.
    Store(Box<StoreError>),
    /// Two parents of a commit become the same commit, because they differed only in the blob.
    /// A commit with two parents is a merge, which comes with phase 3.
    DuplicateParent {
        /// The commit.
        commit: CommitId,
        /// The new parent that it would have twice.
        parent: CommitId,
    },
    /// The working-copy view of a commit that the purge rewrites cannot be computed, for example
    /// because a side of its conflict names a blob that is missing. Without the view, the plan
    /// does not know which copies of the content the view stored (STEMMA-195).
    View {
        /// The commit.
        commit: CommitId,
        /// Why the view cannot be made.
        source: Box<SnapshotError>,
    },
    /// A rewrite of a commit gives a sum of trees that no commit can hold. The form of a commit
    /// follows from its sum, so this means a defect of the rewrite, not a damaged repository.
    Rewrite {
        /// The commit.
        commit: CommitId,
        /// Why the sum is rejected.
        source: FromSumError,
    },
    /// A tree or a commit that nothing reaches, left in the store, cannot be planned. The scan
    /// does not skip it, as it may hold the blob.
    Leftover {
        /// The object that the scan found.
        id: ObjectId,
        /// Why it cannot be planned.
        source: Box<PurgeError>,
    },
    /// The plan would replace more entries of the log than the journal of a purge lists
    /// (2^24), so that the purge could not be resumed or aborted after a crash.
    TooManyEntries {
        /// The number of entries that the plan would replace.
        count: usize,
    },
    /// Tags point to commits that the purge would rewrite. A tag never moves (ADR-008), so the
    /// purge would leave the commits with the secret reachable.
    TaggedCommit {
        /// Every such tag with the commit it points to, which holds the blob or follows a commit
        /// that does. Never empty.
        tags: Vec<(BranchName, CommitId)>,
    },
    /// The plan would keep an object reachable that holds the blob or that it deletes. This is a
    /// mistake of the plan and not of the repository; it is an error so that the secret does not
    /// stay without a word.
    Unsound {
        /// The object.
        id: ObjectId,
    },
}

impl PurgeError {
    /// The reason, without the words that say that nothing was planned.
    fn reason(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent => write!(
                f,
                "an agent may not purge, as a purge removes history for good; a human has to run \
                 it, for example the sponsor of the agent"
            ),
            Self::Update(error) => write!(f, "{error}"),
            Self::NoSuchBlob { id, found: None } => {
                write!(f, "the object store has no blob {id}")
            }
            Self::NoSuchBlob {
                id,
                found: Some(kind),
            } => write!(f, "{id} is a {}, not a blob", kind.name()),
            Self::BlobUnreadable { id, source } => write!(
                f,
                "the blob {id} cannot be read, so it is not known that it is one: {source}"
            ),
            Self::Refs(error) => write!(f, "the references cannot be read: {error}"),
            Self::Log(error) => write!(f, "the operation log cannot be read: {error}"),
            Self::Read(error) => write!(f, "the history cannot be read: {error}"),
            Self::View { commit, source } => write!(
                f,
                "the view of the conflict in commit {commit} cannot be made, so it is not known \
                 whether it stored a copy of the blob: {source}"
            ),
            Self::Store(error) => write!(
                f,
                "not every object can be looked at, so it is not known whether one names the \
                 blob: {error}"
            ),
            Self::DuplicateParent { commit, parent } => write!(
                f,
                "commit {commit} would have the parent {parent} twice after the purge, which a \
                 commit cannot have"
            ),
            Self::Leftover { id, source } => {
                write!(
                    f,
                    "the object {id}, which nothing reaches and which was left in the store, \
                     cannot be planned: "
                )?;
                source.reason(f)
            }
            Self::TooManyEntries { count } => write!(
                f,
                "the purge would replace {count} entries of the operation log, more than the \
                 {} that its journal can list",
                journal::MAX_STUBS
            ),
            Self::Rewrite { commit, source } => write!(
                f,
                "the rewrite of the commit {commit} gives a sum of trees that no commit can hold: \
                 {source}"
            ),
            Self::TaggedCommit { tags } => {
                write!(f, "a tag never moves, so the purge cannot proceed while ")?;
                for (index, (tag, commit)) in tags.iter().enumerate() {
                    let separator = if index == 0 { "" } else { ", " };
                    write!(
                        f,
                        "{separator}the tag `{tag}` points to the commit {commit}"
                    )?;
                }
                write!(
                    f,
                    ", which the purge would rewrite; delete each tag with `stemma tag --delete \
                     <name>` (if a purge is pending, abort it first with `stemma purge \
                     --abort`), run the purge again, and then set the tag again at the commit \
                     that the purge leaves if it is still needed"
                )
            }
            Self::Unsound { id } => write!(
                f,
                "the plan would keep {id} reachable although it holds the blob or is deleted; \
                 this is a mistake of stemma"
            ),
        }
    }
}

impl fmt::Display for PurgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent => self.reason(f),
            _ => {
                write!(f, "could not plan the purge, nothing was written: ")?;
                self.reason(f)
            }
        }
    }
}

impl Error for PurgeError {
    /// The cause of the cause, as the other errors of the crate do: `Display` includes the message
    /// of the wrapped error.
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Update(error) => error.source(),
            Self::Refs(error) => error.source(),
            Self::Log(error) => error.source(),
            Self::Read(error) => error.source(),
            Self::View { source, .. } => source.source(),
            Self::Store(error) | Self::BlobUnreadable { source: error, .. } => error.source(),
            Self::Leftover { source, .. } => source.source(),
            Self::Rewrite { source, .. } => source.source(),
            Self::Agent
            | Self::NoSuchBlob { .. }
            | Self::DuplicateParent { .. }
            | Self::TooManyEntries { .. }
            | Self::TaggedCommit { .. }
            | Self::Unsound { .. } => None,
        }
    }
}

/// Refuses a purge that would rewrite the commit of a tag (ADR-008, ADR-014): lists every such
/// tag, so that one refusal names them all.
fn check_tags(
    tags: &[(BranchName, CommitId)],
    rewritten: &HashSet<CommitId>,
) -> Result<(), PurgeError> {
    let tagged: Vec<(BranchName, CommitId)> = tags
        .iter()
        .filter(|(_, commit)| rewritten.contains(commit))
        .cloned()
        .collect();
    if tagged.is_empty() {
        Ok(())
    } else {
        Err(PurgeError::TaggedCommit { tags: tagged })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::snapshot::read_commit;
    use super::*;
    use crate::object::{Blob, Commit};
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

    const SECRET: &str = "key=hunter2";

    fn secret() -> ObjectId {
        Blob::new(SECRET).id().object_id()
    }

    fn numbers(repository: &Repository) -> Vec<u64> {
        repository
            .oplog()
            .entries()
            .unwrap()
            .iter()
            .map(Entry::number)
            .collect()
    }

    /// The planner after it has walked the commit.
    fn planned(repository: &Repository, commit: CommitId) -> Planner<'_> {
        let mut planner = Planner::new(repository.objects(), BlobId::new(secret()));
        planner.commit(commit).unwrap();
        planner
    }

    /// The tree of the new version of a commit.
    fn new_tree(planner: &Planner<'_>, old: CommitId) -> TreeId {
        one_tree(&planner.new_commits[&planner.commits[&old]])
    }

    /// The tree of a commit that is not a conflict.
    fn one_tree(commit: &AnyCommit) -> TreeId {
        match commit {
            AnyCommit::Commit(commit) => commit.tree(),
            AnyCommit::Commit2(_) => panic!("a commit2 has no one tree"),
        }
    }

    /// The ID that a tree names under `name`.
    fn subtree(repository: &Repository, planner: &Planner<'_>, tree: TreeId, name: &str) -> TreeId {
        let tree = match planner.new_trees.get(&tree) {
            Some(tree) => tree.clone(),
            None => read_tree(repository.objects(), tree).unwrap(),
        };
        let entry = tree
            .entries()
            .iter()
            .find(|entry| entry.name() == name)
            .unwrap();
        TreeId::new(entry.id())
    }

    /// Writes a file at the path of an ID that the store does not have.
    fn junk(repository: &Repository, content: &[u8]) -> ObjectId {
        let id = Blob::new("never stored").id().object_id();
        let path = repository.objects().object_path(&id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
        id
    }

    fn plan(repository: &Repository) -> PurgePlan {
        repository
            .plan_purge(secret(), &actor(), &AllowAll)
            .unwrap()
    }

    /// The new version of a commit, as the plan makes it in memory.
    fn new_commit(repository: &Repository, old: CommitId) -> Commit {
        let blob = BlobId::new(secret());
        let mut planner = Planner::new(repository.objects(), blob);
        planner.commit(old).unwrap();
        let new = planner.commits[&old];
        match planner.new_commits[&new].clone() {
            AnyCommit::Commit(commit) => commit,
            AnyCommit::Commit2(_) => panic!("a commit2 has no one tree"),
        }
    }

    fn names(repository: &Repository, tree: TreeId, planner: &Planner<'_>) -> Vec<String> {
        let tree = match planner.new_trees.get(&tree) {
            Some(tree) => tree.clone(),
            None => {
                let raw = read_object(repository.objects(), tree.object_id(), Kind::Tree).unwrap();
                Tree::parse(raw.body()).unwrap()
            }
        };
        tree.entries()
            .iter()
            .map(|entry| entry.name().to_owned())
            .collect()
    }

    /// A repository with `a.txt` committed as "first", then the secret in `config/key` committed
    /// as "second", then a change of `a.txt` committed as "third". Returns the three commits.
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

    fn counts(repository: &Repository) -> (usize, usize) {
        (
            repository.objects().ids().unwrap().len(),
            repository.oplog().entries().unwrap().len(),
        )
    }

    /// A log with 2^24 entries is too large to build in a test, so the limit is checked alone.
    #[test]
    fn a_plan_that_replaces_more_entries_than_a_journal_lists_is_refused() {
        let most = journal::MAX_STUBS as usize;
        assert!(check_entries(most).is_ok());
        let error = check_entries(most + 1).unwrap_err();
        assert!(
            matches!(error, PurgeError::TooManyEntries { count } if count == most + 1),
            "{error:?}"
        );
        assert!(error.to_string().contains("16777217 entries"), "{error}");
    }

    #[test]
    fn an_agent_is_refused_before_the_authorizer_is_asked() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let agent = Actor::agent("bot", "model", "session", "daniel").unwrap();
        let error = repository
            .plan_purge(secret(), &agent, &DenyAll)
            .unwrap_err();
        assert!(matches!(error, PurgeError::Agent), "{error:?}");
        assert!(error.to_string().contains("a human has to run it"));
    }

    #[test]
    fn the_authorizer_is_asked_for_purge_with_the_blob() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let error = repository
            .plan_purge(secret(), &actor(), &DenyAll)
            .unwrap_err();
        match error {
            PurgeError::Update(UpdateError::Denied { operation, target }) => {
                assert_eq!(operation, "purge");
                assert_eq!(target, secret().to_string());
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_id_that_is_not_a_stored_blob_is_refused() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let error = repository
            .plan_purge(secret(), &actor(), &AllowAll)
            .unwrap_err();
        assert!(matches!(error, PurgeError::NoSuchBlob { id, found: None } if id == secret()));
        let working_copy = repository.refs().working_copy().unwrap().object_id();
        let error = repository
            .plan_purge(working_copy, &actor(), &AllowAll)
            .unwrap_err();
        assert!(
            matches!(
                error,
                PurgeError::NoSuchBlob {
                    found: Some(Kind::Commit),
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().ends_with("is a commit, not a blob"));
    }

    #[test]
    fn the_commits_with_the_blob_and_after_it_are_rewritten_and_the_pointers_move() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, third] = history(&dir, &repository);
        let working_copy = repository.refs().working_copy().unwrap();
        let plan = plan(&repository);

        let old: Vec<CommitId> = plan.rewrites().iter().map(|(old, _)| *old).collect();
        assert!(!old.contains(&first), "the first commit has no secret");
        let position = |commit| old.iter().position(|old| *old == commit).unwrap();
        assert!(position(second) < position(third), "parents come first");
        assert!(position(third) < position(working_copy));
        let new = |commit| {
            plan.rewrites()
                .iter()
                .find(|(old, _)| *old == commit)
                .unwrap()
                .1
        };
        assert_eq!(
            plan.working_copy("main"),
            Some(&Moved::from_to(Some(working_copy), Some(new(working_copy))))
        );
        let main = BranchName::try_new("main").unwrap();
        assert_eq!(
            plan.branches(),
            [(main, Moved::from_to(Some(third), Some(new(third))))]
        );
        assert_eq!(plan.head("main"), None, "HEAD names the branch");
    }

    /// Creates a tag directly in the store, without an entry in the operation log.
    fn tag(dir: &TempDir, repository: &Repository, name: &str, commit: CommitId) {
        use crate::allowed::Allowed;
        use crate::refs::{FsRefStore, RefStore};
        FsRefStore::new(dir.path().join(".stemma"))
            .create_tag(
                &BranchName::try_new(name).unwrap(),
                commit,
                repository.objects(),
                &Allowed::granted(),
            )
            .unwrap();
    }

    #[test]
    fn a_tag_on_a_commit_that_would_be_rewritten_refuses_the_purge() {
        for index in [1, 2] {
            let dir = TempDir::new();
            let repository = repository(&dir);
            let commits = history(&dir, &repository);
            tag(&dir, &repository, "v1", commits[index]);
            let error = repository
                .plan_purge(secret(), &actor(), &AllowAll)
                .unwrap_err();
            match &error {
                PurgeError::TaggedCommit { tags } => {
                    assert_eq!(
                        tags,
                        &[(BranchName::try_new("v1").unwrap(), commits[index])]
                    );
                }
                other => panic!("expected a tagged commit, got {other:?}"),
            }
            assert!(error.to_string().contains("a tag never moves"), "{error}");
            assert!(
                error.to_string().contains("stemma tag --delete <name>"),
                "{error}"
            );
        }
    }

    #[test]
    fn the_refusal_names_every_tag_that_stands_in_the_way() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, third] = history(&dir, &repository);
        tag(&dir, &repository, "v0", first);
        tag(&dir, &repository, "v1", second);
        tag(&dir, &repository, "v2", third);
        let PurgeError::TaggedCommit { tags } = repository
            .plan_purge(secret(), &actor(), &AllowAll)
            .unwrap_err()
        else {
            panic!("expected a tagged commit");
        };
        let names: Vec<&str> = tags.iter().map(|(tag, _)| tag.as_str()).collect();
        assert_eq!(names, ["v1", "v2"], "v0 is before the secret");
    }

    #[test]
    fn a_tag_on_a_commit_before_the_secret_does_not_stand_in_the_way() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, ..] = history(&dir, &repository);
        tag(&dir, &repository, "v0", first);
        let plan = plan(&repository);
        assert!(!plan.rewrites().iter().any(|(old, _)| *old == first));
    }

    #[test]
    fn a_new_commit_keeps_the_change_the_author_the_time_and_the_message() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, third] = history(&dir, &repository);
        let old = read_commit(repository.objects(), third).unwrap();
        let new = new_commit(&repository, third);
        assert_eq!(new.change(), old.change());
        assert_eq!(new.author(), old.author());
        assert_eq!(new.timestamp(), old.timestamp());
        assert_eq!(new.message(), "third");
        assert_eq!(new.parents(), [new_commit(&repository, second).id()]);
        assert_eq!(new_commit(&repository, second).parents(), [first]);
    }

    #[test]
    fn a_directory_that_holds_only_the_blob_leaves_the_tree() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [_, second, _] = history(&dir, &repository);
        let mut planner = Planner::new(repository.objects(), BlobId::new(secret()));
        planner.commit(second).unwrap();
        let tree = one_tree(&planner.new_commits[&planner.commits[&second]]);
        assert_eq!(names(&repository, tree, &planner), ["a.txt"]);

        // The same files saved by a snapshot make the same tree.
        let other = TempDir::new();
        let fresh =
            Repository::init(other.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
        write(&other, "a.txt", "one");
        track(&fresh, &["a.txt"]);
        let made = read_commit(fresh.objects(), commit(&fresh, "first")).unwrap();
        assert_eq!(made.tree(), tree);
    }

    #[test]
    fn the_entries_from_the_one_that_saved_the_secret_on_are_replaced() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, &["a.txt"]);
        commit(&repository, "first");
        let before = numbers(&repository).len() as u64;
        write(&dir, "config/key", SECRET);
        track(&repository, &["config/key"]);
        commit(&repository, "second");
        let after = numbers(&repository).len() as u64;
        // Every entry after the secret was saved moves the working copy, which descends from it.
        let expected: Vec<u64> = (before + 1..=after).collect();
        assert_eq!(plan(&repository).entries(), expected);
    }

    #[test]
    fn the_delete_set_holds_the_old_commits_and_trees_and_nothing_that_is_kept() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, third] = history(&dir, &repository);
        let plan = plan(&repository);
        let mut old: Vec<CommitId> = plan.rewrites().iter().map(|(old, _)| *old).collect();
        old.sort();
        assert_eq!(plan.deleted_commits(), old);
        assert!(plan.deleted_commits().contains(&second));
        assert!(plan.deleted_commits().contains(&third));
        assert!(!plan.deleted_commits().contains(&first));
        let first_tree = read_commit(repository.objects(), first).unwrap().tree();
        assert!(!plan.deleted_trees().contains(&first_tree));
        let second_tree = read_commit(repository.objects(), second).unwrap().tree();
        assert!(plan.deleted_trees().contains(&second_tree));
        assert_eq!(
            plan.deleted_trees().len(),
            3,
            "the roots of the second and the third commit, and the `config` tree they share"
        );
    }

    #[test]
    fn a_tree_and_a_commit_that_only_the_store_holds_join_the_delete_set() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let store = repository.objects();
        let blob = BlobId::new(secret());
        let entry = TreeEntry::file("leftover", FileMode::File, blob).unwrap();
        let tree = store
            .put_tree(&Tree::try_new(vec![entry]).unwrap())
            .unwrap();
        let orphan = Commit::try_new(
            tree,
            vec![],
            crate::change::ChangeId::generate().unwrap(),
            actor(),
            0,
            "left by a crash",
        )
        .unwrap();
        let orphan = store.put_commit(&orphan).unwrap();
        let plan = plan(&repository);
        assert!(plan.deleted_trees().contains(&tree));
        assert!(plan.deleted_commits().contains(&orphan));
        assert!(
            plan.rewrites().iter().all(|(old, _)| *old != orphan),
            "no pointer names it, so it is not rewritten"
        );
    }

    #[test]
    fn a_blob_in_no_commit_is_only_deleted() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        repository
            .objects()
            .put_blob(&Blob::new("key=hunter2"))
            .unwrap();
        let plan = plan(&repository);
        assert_eq!(plan.blob().object_id(), secret());
        assert!(plan.rewrites().is_empty());
        assert!(plan.entries().is_empty());
        assert!(plan.deleted_commits().is_empty());
        assert!(plan.deleted_trees().is_empty());
        assert_eq!(plan.working_copy("main"), None);
    }

    #[test]
    fn a_plan_writes_nothing() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let before = counts(&repository);
        plan(&repository);
        assert_eq!(counts(&repository), before);
    }

    #[test]
    fn a_directory_that_keeps_other_files_gets_a_new_tree_without_the_blob() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        write(&dir, "config/key", SECRET);
        write(&dir, "config/other", "other");
        track(&repository, &["a.txt", "config"]);
        let commit = commit(&repository, "first");
        let planner = planned(&repository, commit);
        let old_root = read_commit(repository.objects(), commit).unwrap().tree();
        let old_config = subtree(&repository, &planner, old_root, "config");
        let root = new_tree(&planner, commit);
        assert_eq!(names(&repository, root, &planner), ["a.txt", "config"]);
        let config = subtree(&repository, &planner, root, "config");
        assert_ne!(config, old_config);
        assert_eq!(names(&repository, config, &planner), ["other"]);
        let plan = plan(&repository);
        assert!(plan.deleted_trees().contains(&old_root));
        assert!(plan.deleted_trees().contains(&old_config));
    }

    #[test]
    fn a_secret_that_only_the_log_still_names_is_purged_from_the_log() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a.txt", "one");
        track(&repository, &["a.txt"]);
        commit(&repository, "first");
        write(&dir, "key", SECRET);
        track(&repository, &["key"]);
        let saved = *numbers(&repository).last().unwrap();
        let held = repository.refs().working_copy().unwrap();
        let restored = repository
            .restore(saved, false, &actor(), &AllowAll)
            .unwrap();
        let restored = restored.entry().unwrap().number();

        let plan = plan(&repository);
        assert_eq!(plan.entries(), [saved, restored]);
        assert!(plan.rewrites().iter().any(|(old, _)| *old == held));
        assert!(plan.deleted_commits().contains(&held));
        assert_eq!(
            plan.working_copy("main"),
            None,
            "the working copy holds no secret now"
        );
        assert!(plan.branches().is_empty());
    }

    #[test]
    fn the_walk_of_what_is_kept_fails_on_a_tree_that_holds_the_blob() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [_, second, _] = history(&dir, &repository);
        let mut planner = planned(&repository, second);
        let error = planner.reached(&[second]).unwrap_err();
        assert!(matches!(error, PurgeError::Unsound { .. }), "{error:?}");
        assert!(error.to_string().ends_with("this is a mistake of stemma"));
    }

    #[test]
    fn a_detached_head_on_a_rewritten_commit_moves_and_one_on_a_clean_commit_does_not() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, _] = history(&dir, &repository);
        let operation = OperationName::try_new("checkout").unwrap();
        let detach = |commit| {
            repository
                .set_head(
                    &operation,
                    &repository.refs().head().unwrap(),
                    &Head::Detached(commit),
                    &actor(),
                    &AllowAll,
                )
                .unwrap();
        };
        detach(second);
        let detached = plan(&repository);
        let new = detached
            .rewrites()
            .iter()
            .find(|(old, _)| *old == second)
            .unwrap()
            .1;
        assert_eq!(
            detached.head("main"),
            Some(&Moved::from_to(
                Some(Head::Detached(second)),
                Some(Head::Detached(new))
            ))
        );
        detach(first);
        assert_eq!(plan(&repository).head("main"), None);
    }

    #[test]
    fn a_damaged_object_in_the_store_stops_the_plan() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        junk(&repository, b"junk");
        let error = repository
            .plan_purge(secret(), &actor(), &AllowAll)
            .unwrap_err();
        match &error {
            PurgeError::Leftover { source, .. } => {
                assert!(matches!(**source, PurgeError::Store(_)), "{source:?}");
            }
            other => panic!("expected a leftover, got {other:?}"),
        }
        assert!(error
            .to_string()
            .starts_with("could not plan the purge, nothing was written: the object b3:"));
        assert!(error
            .to_string()
            .contains("not every object can be looked at"));
    }

    #[test]
    fn a_damaged_secret_is_not_taken_for_a_missing_one() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let id = junk(&repository, b"junk");
        let error = repository.plan_purge(id, &actor(), &AllowAll).unwrap_err();
        assert!(
            matches!(error, PurgeError::BlobUnreadable { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_tree_of_a_commit_can_become_empty() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "key", SECRET);
        track(&repository, &["key"]);
        let commit = commit(&repository, "only");
        let planner = planned(&repository, commit);
        let tree = new_tree(&planner, commit);
        assert!(planner.new_trees[&tree].entries().is_empty());
        assert!(plan(&repository)
            .rewrites()
            .iter()
            .any(|(old, _)| *old == commit));
    }

    #[test]
    fn the_blob_is_removed_from_every_place_of_a_tree() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a/key", SECRET);
        write(&dir, "b/key", SECRET);
        write(&dir, "key.bak", SECRET);
        write(&dir, "c.txt", "kept");
        track(&repository, &["."]);
        let commit = commit(&repository, "first");
        let planner = planned(&repository, commit);
        assert_eq!(
            names(&repository, new_tree(&planner, commit), &planner),
            ["c.txt"]
        );
        let plan = plan(&repository);
        let old: HashSet<CommitId> = plan.rewrites().iter().map(|(old, _)| *old).collect();
        assert_eq!(old.len(), plan.rewrites().len(), "each commit once");
    }

    #[test]
    fn the_paths_name_every_place_of_the_blob_in_the_history_once() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        write(&dir, "a/b/key", SECRET);
        write(&dir, "key.bak", SECRET);
        write(&dir, "c.txt", "kept");
        track(&repository, &["."]);
        let first = commit(&repository, "first");
        // A later commit moves the secret to a new place and keeps the old ones.
        write(&dir, "d/key", SECRET);
        track(&repository, &["d/key"]);
        let second = commit(&repository, "second");
        let mut planner = Planner::new(repository.objects(), BlobId::new(secret()));
        planner.commit(second).unwrap();
        assert!(planner.rewrites.iter().any(|(old, _)| *old == first));
        assert_eq!(planner.paths(), ["a/b/key", "d/key", "key.bak"]);
    }

    #[test]
    fn a_blob_in_no_commit_has_no_path() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        repository.objects().put_blob(&Blob::new(SECRET)).unwrap();
        let mut planner = Planner::new(repository.objects(), BlobId::new(secret()));
        planner.scan().unwrap();
        assert!(planner.paths().is_empty());
    }

    #[test]
    fn a_leftover_merge_whose_parents_become_one_is_an_error_that_names_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let store = repository.objects();
        let file = |name: &str, content: &str| {
            TreeEntry::file(name, FileMode::File, Blob::new(content).id()).unwrap()
        };
        let with = store
            .put_tree(&Tree::try_new(vec![file("a.txt", "one"), file("key", SECRET)]).unwrap())
            .unwrap();
        let without = store
            .put_tree(&Tree::try_new(vec![file("a.txt", "one")]).unwrap())
            .unwrap();
        let change = crate::change::ChangeId::generate().unwrap();
        let parent = |tree| Commit::try_new(tree, vec![], change, actor(), 5, "p").unwrap();
        let one = store.put_commit(&parent(with)).unwrap();
        let two = store.put_commit(&parent(without)).unwrap();
        let merge = Commit::try_new(
            without,
            vec![one, two],
            crate::change::ChangeId::generate().unwrap(),
            actor(),
            6,
            "merge",
        )
        .unwrap();
        let merge = store.put_commit(&merge).unwrap();

        let error = repository
            .plan_purge(secret(), &actor(), &AllowAll)
            .unwrap_err();
        match &error {
            PurgeError::Leftover { id, source } => {
                assert_eq!(*id, merge.object_id());
                assert!(matches!(
                    **source,
                    PurgeError::DuplicateParent { commit, parent }
                        if commit == merge && parent == two
                ));
            }
            other => panic!("expected a leftover, got {other:?}"),
        }
        assert!(error.to_string().contains("which nothing reaches"));
    }

    #[test]
    fn a_leftover_after_a_rewritten_commit_is_deleted_and_a_clean_one_is_not() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, _] = history(&dir, &repository);
        let store = repository.objects();
        let clean_tree = read_commit(store, first).unwrap().tree();
        let make = |parents, message: &str| {
            let commit = Commit::try_new(
                clean_tree,
                parents,
                crate::change::ChangeId::generate().unwrap(),
                actor(),
                7,
                message,
            )
            .unwrap();
            store.put_commit(&commit).unwrap()
        };
        let after = make(vec![second], "after the secret");
        let clean = make(vec![first], "clean");
        let plan = plan(&repository);
        assert!(plan.deleted_commits().contains(&after));
        assert!(!plan.deleted_commits().contains(&clean));
        assert!(!plan.deleted_trees().contains(&clean_tree));
    }

    #[test]
    fn the_same_repository_gives_the_same_plan() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        assert_eq!(plan(&repository), plan(&repository));
    }

    #[test]
    fn the_plan_fails_if_a_kept_root_holds_the_blob() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [_, second, _] = history(&dir, &repository);
        let mut planner = planned(&repository, second);
        planner.scan().unwrap();
        let commits: Vec<CommitId> = planner.rewrites.iter().map(|(old, _)| *old).collect();
        // The old commit as a kept root, as a plan that forgot to map it to its new version would
        // pass it.
        let error = planner.check(&[second], &commits, &[]).unwrap_err();
        assert!(matches!(error, PurgeError::Unsound { .. }), "{error:?}");
        let new = planner.commits[&second];
        planner.check(&[new], &commits, &[]).unwrap();
    }

    #[test]
    fn the_file_mode_does_not_matter() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let store = repository.objects();
        let blob = BlobId::new(secret());
        let other = Blob::new("other target").id();
        let tree = Tree::try_new(vec![
            TreeEntry::file("deploy.sh", FileMode::Executable, blob).unwrap(),
            TreeEntry::file("link", FileMode::Symlink, blob).unwrap(),
            TreeEntry::file("kept", FileMode::Symlink, other).unwrap(),
        ])
        .unwrap();
        let tree = store.put_tree(&tree).unwrap();
        let mut planner = Planner::new(store, blob);
        let new = planner.tree(tree).unwrap().unwrap();
        assert_eq!(
            names(&repository, new, &planner),
            ["kept"],
            "an executable and a link with the secret leave, a link with another target stays"
        );
    }

    #[test]
    fn a_leftover_tree_with_the_blob_in_a_subtree_is_deleted() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let store = repository.objects();
        let blob = BlobId::new(secret());
        let inner =
            Tree::try_new(vec![TreeEntry::file("key", FileMode::File, blob).unwrap()]).unwrap();
        let inner = store.put_tree(&inner).unwrap();
        let outer = Tree::try_new(vec![TreeEntry::directory("deep", inner).unwrap()]).unwrap();
        let outer = store.put_tree(&outer).unwrap();
        let plan = plan(&repository);
        assert!(plan.deleted_trees().contains(&outer));
        assert!(plan.deleted_trees().contains(&inner));
    }

    #[test]
    fn a_leftover_whose_parent_is_missing_stops_the_plan_and_is_named() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, ..] = history(&dir, &repository);
        let store = repository.objects();
        let missing = CommitId::new(Blob::new("no such commit").id().object_id());
        let tree = read_commit(store, first).unwrap().tree();
        let orphan = Commit::try_new(
            tree,
            vec![missing],
            crate::change::ChangeId::generate().unwrap(),
            actor(),
            8,
            "dangling",
        )
        .unwrap();
        let orphan = store.put_commit(&orphan).unwrap();
        let error = repository
            .plan_purge(secret(), &actor(), &AllowAll)
            .unwrap_err();
        match &error {
            PurgeError::Leftover { id, source } => {
                assert_eq!(*id, orphan.object_id());
                assert!(matches!(**source, PurgeError::Read(_)), "{source:?}");
            }
            other => panic!("expected a leftover, got {other:?}"),
        }
    }

    /// A tree without the secret, which differs from the trees of `history`.
    fn other_tree(repository: &Repository) -> TreeId {
        let blob = repository.objects().put_blob(&Blob::new("other")).unwrap();
        let entry = TreeEntry::file("b.txt", FileMode::File, blob).unwrap();
        repository
            .objects()
            .put_tree(&Tree::try_new(vec![entry]).unwrap())
            .unwrap()
    }

    /// Stores a `commit2` with the trees and the parent.
    fn conflict(repository: &Repository, trees: Vec<TreeId>, parent: CommitId) -> CommitId {
        let commit = crate::object::Commit2::try_new(
            trees,
            vec![parent],
            crate::change::ChangeId::generate().unwrap(),
            actor(),
            8,
            "conflict",
        )
        .unwrap();
        let id = repository
            .objects()
            .put_raw(Kind::Commit2, &commit.encode())
            .unwrap();
        CommitId::new(id)
    }

    fn tree_of(repository: &Repository, commit: CommitId) -> TreeId {
        read_commit(repository.objects(), commit).unwrap().tree()
    }

    #[test]
    fn a_commit2_is_rewritten_in_each_tree_of_its_list_and_stays_a_commit2() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, third] = history(&dir, &repository);
        let (kept, with_secret, later) = (
            tree_of(&repository, first),
            tree_of(&repository, second),
            tree_of(&repository, third),
        );
        let other = other_tree(&repository);
        let old = conflict(&repository, vec![later, with_secret, other], first);
        let planner = planned(&repository, old);
        let new = planner.commits[&old];
        assert_ne!(new, old);
        let AnyCommit::Commit2(rewritten) = &planner.new_commits[&new] else {
            panic!("the sum keeps three terms, so it stays a commit2");
        };
        assert_eq!(
            rewritten.trees(),
            [
                planner.trees[&later].unwrap(),
                planner.trees[&with_secret].unwrap(),
                other
            ]
        );
        assert_eq!(rewritten.parents(), [first]);
        assert_eq!(rewritten.message(), "conflict");
        // The tree that the purge leaves of `second` is the tree of `first`, which holds no key.
        assert_eq!(planner.trees[&with_secret], Some(kept));
        assert_eq!(planner.trees[&kept], None);
        assert_eq!(planner.commit_links[&new].0, rewritten.trees());
    }

    #[test]
    fn a_commit2_whose_terms_cancel_after_the_rewrite_becomes_a_commit() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, third] = history(&dir, &repository);
        let (kept, with_secret, later) = (
            tree_of(&repository, first),
            tree_of(&repository, second),
            tree_of(&repository, third),
        );
        // The base loses the key and equals the second side, so the two cancel.
        let old = conflict(&repository, vec![later, with_secret, kept], first);
        let planner = planned(&repository, old);
        let new = planner.commits[&old];
        let AnyCommit::Commit(rewritten) = &planner.new_commits[&new] else {
            panic!("one tree is left, so the commit is a commit");
        };
        assert_eq!(rewritten.tree(), planner.trees[&later].unwrap());
        assert_eq!(rewritten.parents(), [first]);
        assert_eq!(rewritten.change(), {
            let AnyCommit::Commit2(inner) =
                crate::repository::snapshot::read_any_commit(repository.objects(), old).unwrap()
            else {
                panic!("the stored commit is a commit2");
            };
            inner.change()
        });
    }

    #[test]
    fn a_commit2_with_clean_trees_is_rewritten_when_its_parent_is_and_keeps_its_trees() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, _] = history(&dir, &repository);
        let other = other_tree(&repository);
        let kept = tree_of(&repository, first);
        let old = conflict(&repository, vec![kept, other, kept], second);
        let planner = planned(&repository, old);
        let new = planner.commits[&old];
        assert_ne!(new, old);
        let AnyCommit::Commit2(rewritten) = &planner.new_commits[&new] else {
            panic!("the trees are untouched, so the commit stays a commit2");
        };
        assert_eq!(rewritten.trees(), [kept, other, kept]);
        assert_eq!(rewritten.parents(), [planner.commits[&second]]);
        assert_eq!(rewritten.message(), "conflict");
    }

    #[test]
    fn a_leftover_commit2_whose_parents_become_one_is_an_error_that_names_it() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let store = repository.objects();
        let file = |name: &str, content: &str| {
            TreeEntry::file(name, FileMode::File, Blob::new(content).id()).unwrap()
        };
        let with = store
            .put_tree(&Tree::try_new(vec![file("a.txt", "one"), file("key", SECRET)]).unwrap())
            .unwrap();
        let without = store
            .put_tree(&Tree::try_new(vec![file("a.txt", "one")]).unwrap())
            .unwrap();
        let change = crate::change::ChangeId::generate().unwrap();
        let parent = |tree| Commit::try_new(tree, vec![], change, actor(), 5, "p").unwrap();
        let one = store.put_commit(&parent(with)).unwrap();
        let two = store.put_commit(&parent(without)).unwrap();
        let other = other_tree(&repository);
        let conflict = crate::object::Commit2::try_new(
            vec![without, other, without],
            vec![one, two],
            crate::change::ChangeId::generate().unwrap(),
            actor(),
            6,
            "conflict",
        )
        .unwrap();
        let conflict = CommitId::new(store.put_raw(Kind::Commit2, &conflict.encode()).unwrap());

        let error = repository
            .plan_purge(secret(), &actor(), &AllowAll)
            .unwrap_err();
        match &error {
            PurgeError::Leftover { id, source } => {
                assert_eq!(*id, conflict.object_id());
                assert!(matches!(
                    **source,
                    PurgeError::DuplicateParent { commit, parent }
                        if commit == conflict && parent == two
                ));
            }
            other => panic!("expected a leftover, got {other:?}"),
        }
    }

    #[test]
    fn a_leftover_commit2_that_holds_the_secret_is_deleted_with_its_kind() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, _] = history(&dir, &repository);
        let other = other_tree(&repository);
        let with_secret = tree_of(&repository, second);
        let kept = tree_of(&repository, first);
        let old = conflict(&repository, vec![with_secret, kept, other], first);
        let plan = plan(&repository);
        assert!(plan.deleted_commits().contains(&old), "{plan:?}");
    }

    #[test]
    fn a_leftover_commit2_that_follows_a_rewritten_commit_is_deleted() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, second, _] = history(&dir, &repository);
        let other = other_tree(&repository);
        let kept = tree_of(&repository, first);
        let old = conflict(&repository, vec![kept, other, kept], second);
        let plan = plan(&repository);
        assert!(plan.deleted_commits().contains(&old), "{plan:?}");
    }

    #[test]
    fn a_leftover_commit2_that_holds_no_secret_stays() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, ..] = history(&dir, &repository);
        let other = other_tree(&repository);
        let kept = tree_of(&repository, first);
        let old = conflict(&repository, vec![kept, other, kept], first);
        let plan = plan(&repository);
        assert!(!plan.deleted_commits().contains(&old), "{plan:?}");
        assert!(!plan.rewrites().iter().any(|(from, _)| *from == old));
    }

    /// Stores a `commit2` whose sides are `key` files with the secret and with other texts, shows
    /// its working-copy view (which stores the text with markers), and returns the commit, the
    /// tree of the view, and the blob with the markers.
    fn secret_conflict(repository: &Repository, parent: CommitId) -> (CommitId, TreeId, BlobId) {
        let store = repository.objects();
        let tree = |content: &str| {
            let blob = store.put_blob(&Blob::new(content)).unwrap();
            let entry = TreeEntry::file("key", FileMode::File, blob).unwrap();
            store
                .put_tree(&Tree::try_new(vec![entry]).unwrap())
                .unwrap()
        };
        let sum = Merge::try_from_terms(vec![tree(SECRET), tree("base"), tree("other")]).unwrap();
        let commit = conflict(repository, sum.terms().to_vec(), parent);
        let view = conflicted::view(store, &sum).unwrap().tree;
        let entries = read_tree(store, view).unwrap();
        let text = BlobId::new(entries.entries()[0].id());
        let stored = read_object(store, text.object_id(), Kind::Blob).unwrap();
        assert!(
            String::from_utf8(stored.into_body())
                .unwrap()
                .contains(SECRET),
            "the text with markers holds the secret"
        );
        (commit, view, text)
    }

    #[test]
    fn the_text_with_markers_of_a_rewritten_conflict_and_its_view_tree_are_deleted() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, ..] = history(&dir, &repository);
        let (_, view, text) = secret_conflict(&repository, first);
        let before = repository.objects().ids().unwrap();
        let plan = plan(&repository);
        assert_eq!(plan.deleted_blobs(), [text]);
        assert!(plan.deleted_trees().contains(&view));
        assert_eq!(
            repository.objects().ids().unwrap(),
            before,
            "a plan writes nothing"
        );
    }

    #[test]
    fn a_view_that_no_command_showed_is_not_listed() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, ..] = history(&dir, &repository);
        let store = repository.objects();
        let tree = |content: &str| {
            let blob = store.put_blob(&Blob::new(content)).unwrap();
            let entry = TreeEntry::file("key", FileMode::File, blob).unwrap();
            store
                .put_tree(&Tree::try_new(vec![entry]).unwrap())
                .unwrap()
        };
        let sides = vec![tree(SECRET), tree("base"), tree("other")];
        conflict(&repository, sides, first);
        let plan = plan(&repository);
        assert!(plan.deleted_blobs().is_empty());
    }

    #[test]
    fn a_view_that_a_commit_without_the_secret_stored_is_not_touched() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, ..] = history(&dir, &repository);
        let store = repository.objects();
        let tree = |content: &str| {
            let blob = store.put_blob(&Blob::new(content)).unwrap();
            let entry = TreeEntry::file("key", FileMode::File, blob).unwrap();
            store
                .put_tree(&Tree::try_new(vec![entry]).unwrap())
                .unwrap()
        };
        let sum = Merge::try_from_terms(vec![tree("one"), tree("base"), tree("two")]).unwrap();
        conflict(&repository, sum.terms().to_vec(), first);
        conflicted::view(store, &sum).unwrap();
        let plan = plan(&repository);
        assert!(plan.deleted_blobs().is_empty());
    }

    #[test]
    fn a_view_object_that_the_stored_view_of_a_kept_conflict_shares_stays() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, ..] = history(&dir, &repository);
        let store = repository.objects();
        // `dir/a.txt` conflicts in a kept commit and in its child, which also adds the secret, so
        // both views store the same subtree and the same text with markers.
        let side = |content: &str, with_secret: bool| {
            let blob = store.put_blob(&Blob::new(content)).unwrap();
            let entry = TreeEntry::file("a.txt", FileMode::File, blob).unwrap();
            let sub = store
                .put_tree(&Tree::try_new(vec![entry]).unwrap())
                .unwrap();
            let mut entries = vec![TreeEntry::directory("dir", sub).unwrap()];
            if with_secret {
                let secret = store.put_blob(&Blob::new(SECRET)).unwrap();
                entries.push(TreeEntry::file("key", FileMode::File, secret).unwrap());
            }
            store.put_tree(&Tree::try_new(entries).unwrap()).unwrap()
        };
        let sides = |with_secret| {
            Merge::try_from_terms(vec![
                side("x", with_secret),
                side("base", with_secret),
                side("y", with_secret),
            ])
            .unwrap()
        };
        let (kept, child) = (sides(false), sides(true));
        let parent = conflict(&repository, kept.terms().to_vec(), first);
        conflict(&repository, child.terms().to_vec(), parent);
        let shown = conflicted::view(store, &kept).unwrap().tree;
        conflicted::view(store, &child).unwrap();
        let shared = read_tree(store, shown).unwrap();
        let subtree = TreeId::new(shared.entries()[0].id());
        let text = read_tree(store, subtree).unwrap();
        let text = BlobId::new(text.entries()[0].id());
        let plan = plan(&repository);
        assert!(!plan.deleted_trees().contains(&subtree), "{plan:?}");
        assert!(!plan.deleted_blobs().contains(&text), "{plan:?}");
    }

    #[test]
    fn a_rewritten_leftover_does_not_shield_the_text_of_a_conflict() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, ..] = history(&dir, &repository);
        let (_, _, text) = secret_conflict(&repository, first);
        let store = repository.objects();
        let stored = read_object(store, text.object_id(), Kind::Blob).unwrap();
        let stored = String::from_utf8(stored.into_body()).unwrap();
        // No pointer names this commit, so the plan rewrites it in memory and never writes it. Its
        // new tree names the bytes of the text, but a tree that does not exist keeps nothing.
        let secret = store.put_blob(&Blob::new(SECRET)).unwrap();
        let copy = store.put_blob(&Blob::new(&*stored)).unwrap();
        let entries = vec![
            TreeEntry::file("copy.txt", FileMode::File, copy).unwrap(),
            TreeEntry::file("key", FileMode::File, secret).unwrap(),
        ];
        let tree = store.put_tree(&Tree::try_new(entries).unwrap()).unwrap();
        let orphan = Commit::try_new(
            tree,
            vec![],
            crate::change::ChangeId::generate().unwrap(),
            actor(),
            0,
            "left by a crash",
        )
        .unwrap();
        store.put_commit(&orphan).unwrap();
        assert_eq!(copy, text, "the same bytes are the same blob");
        let plan = plan(&repository);
        assert!(plan.deleted_blobs().contains(&text), "{plan:?}");
    }

    #[test]
    fn a_text_with_markers_that_a_kept_file_has_too_stays() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [first, ..] = history(&dir, &repository);
        let (_, _, text) = secret_conflict(&repository, first);
        let stored = read_object(repository.objects(), text.object_id(), Kind::Blob).unwrap();
        // A tracked file with the same bytes is no longer only a view: the commit keeps it.
        write(
            &dir,
            "copy.txt",
            &String::from_utf8(stored.into_body()).unwrap(),
        );
        track(&repository, &["copy.txt"]);
        commit(&repository, "copy");
        let plan = plan(&repository);
        assert!(
            plan.deleted_blobs().is_empty(),
            "{:?}",
            plan.deleted_blobs()
        );
    }

    #[test]
    fn a_second_purge_plans_over_stubs_and_lists_neither_a_stub_nor_a_purge_entry() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        history(&dir, &repository);
        let first = plan(&repository);
        let log = repository.oplog();
        let purge = log
            .append_purge(
                crate::oplog::Purge::try_new(
                    1,
                    actor(),
                    first.rewrites().len() as u64,
                    first.entries(),
                )
                .unwrap(),
            )
            .unwrap()[0]
            .number();
        for number in first.entries() {
            log.replace_with_stub(*number).unwrap();
        }
        // Another secret, saved after the purge.
        write(&dir, "other", "token=swordfish");
        track(&repository, &["other"]);
        let other = Blob::new("token=swordfish").id().object_id();
        let second = repository.plan_purge(other, &actor(), &AllowAll).unwrap();
        assert!(!second.entries().is_empty());
        assert!(!second.entries().contains(&purge));
        assert!(second
            .entries()
            .iter()
            .all(|number| !first.entries().contains(number)));
    }
}
