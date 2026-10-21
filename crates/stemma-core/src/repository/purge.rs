//! The plan of a purge: removing a secret from the history and the operation log (ADR-014).
//!
//! A purge takes the ID of a blob. Every commit whose tree holds the blob, at any depth, is
//! rewritten with a tree without it, and every commit after it with the new parents; the change ID
//! stays. The entries of the log that name a rewritten commit are replaced by stubs, and the
//! objects that the rewrite replaced are deleted, unless something that is kept still names them.
//!
//! This module makes the plan and writes nothing. The steps that write (the new objects, the
//! `purge` entry, the stubs, the deletion, the journal) come with the slices that follow.

use std::collections::{hash_map, HashMap, HashSet};
use std::error::Error;
use std::fmt;

use super::snapshot::{read_commit, read_object, SnapshotError};
use super::{Repository, UpdateError};
use crate::object::{
    BlobId, Commit, CommitError, CommitId, FileMode, Kind, ObjectId, Tree, TreeEntry, TreeId,
};
use crate::oplog::{Entry, Moved, OpLogError, OperationName};
use crate::refs::{BranchName, Head, RefError};
use crate::store::{FsObjectStore, ObjectStore, StoreError};
use crate::{Actor, ActorRef, Authorizer};

/// The operation that the authorizer is asked for.
const OPERATION: &str = "purge";

/// What a purge would do: the commits it rewrites, the pointers it moves, the entries it replaces
/// by stubs, and the objects it deletes.
///
/// The plan is made from the pointers and the log as they are now. A real purge saves the
/// directory in a snapshot first and plans again, so its plan can differ (ADR-014).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgePlan {
    blob: BlobId,
    rewrites: Vec<(CommitId, CommitId)>,
    head: Option<Moved<Head>>,
    working_copy: Option<Moved<CommitId>>,
    branches: Vec<(BranchName, Moved<CommitId>)>,
    entries: Vec<u64>,
    commits: Vec<CommitId>,
    trees: Vec<TreeId>,
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

    /// The move of HEAD, if it names a rewritten commit.
    pub fn head(&self) -> Option<&Moved<Head>> {
        self.head.as_ref()
    }

    /// The move of the working-copy commit, if it is rewritten.
    pub fn working_copy(&self) -> Option<&Moved<CommitId>> {
        self.working_copy.as_ref()
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

    /// The commits that are deleted: the old versions of the rewritten commits, and the commits
    /// in the store that hold the blob and that nothing kept reaches. Sorted by ID.
    pub fn deleted_commits(&self) -> &[CommitId] {
        &self.commits
    }

    /// The trees that are deleted: those that hold the blob, at any depth. Sorted by ID.
    pub fn deleted_trees(&self) -> &[TreeId] {
        &self.trees
    }
}

impl Repository {
    /// Plans the purge of a blob and writes nothing (ADR-014).
    ///
    /// The actor must be a human: an agent is refused before anything else, whatever the
    /// authorizer says, because a purge removes history that cannot be brought back. Then the
    /// authorizer is asked for `purge`, with the blob as the target. The plan starts from the
    /// pointers (HEAD, the working-copy commit, the branches) and from every commit that an entry
    /// of the log names, and follows the parents. A tree that becomes empty leaves its parent
    /// tree, as a snapshot never makes an empty directory; the tree of a commit can become empty.
    /// Last, the store is scanned for trees and commits that hold the blob and that no pointer
    /// and no entry reaches, such as those a crash left behind, and they join the delete set.
    ///
    /// # Errors
    ///
    /// [`PurgeError::Agent`] for an agent, [`PurgeError::Update`] for a refusal of the authorizer,
    /// [`PurgeError::NoSuchBlob`] if the store has no blob with the ID, [`PurgeError::Refs`] and
    /// [`PurgeError::Log`] if the pointers or the log cannot be read, [`PurgeError::Read`] for a
    /// commit or a tree that is missing or cannot be read, [`PurgeError::Store`] if the store
    /// cannot be listed or an object in it is damaged, and [`PurgeError::DuplicateParent`] if a
    /// rewrite makes two parents of a commit the same.
    pub fn plan_purge(
        &self,
        blob: ObjectId,
        actor: &Actor,
        authorizer: &dyn Authorizer,
    ) -> Result<PurgePlan, PurgeError> {
        if let ActorRef::Agent { .. } = actor.view() {
            return Err(PurgeError::Agent);
        }
        let operation = OperationName::new(OPERATION).expect("`purge` is a valid operation name");
        self.authorize(actor, authorizer, &operation, &blob.to_string())
            .map_err(PurgeError::Update)?;
        let blob = match self.objects().resolve_blob(&blob) {
            Ok(Some(found)) => found,
            Ok(None) => {
                return Err(PurgeError::NoSuchBlob {
                    id: blob,
                    found: None,
                })
            }
            Err(StoreError::KindMismatch { found, .. }) => {
                return Err(PurgeError::NoSuchBlob {
                    id: blob,
                    found: Some(found),
                })
            }
            Err(error) => return Err(PurgeError::Store(error)),
        };

        let refs = self.refs();
        let head = refs.head().map_err(PurgeError::Refs)?;
        let working_copy = refs.working_copy().map_err(PurgeError::Refs)?;
        let branches = refs.branches().map_err(PurgeError::Refs)?;
        let log = self.oplog().entries().map_err(PurgeError::Log)?;

        let mut planner = Planner::new(self.objects(), blob);
        let mut roots = vec![working_copy];
        roots.extend(branches.iter().map(|(_, commit)| *commit));
        if let Head::Detached(commit) = &head {
            roots.push(*commit);
        }
        roots.extend(log.iter().flat_map(|entry| entry.change().commits()));
        for root in &roots {
            planner.commit(*root)?;
        }
        let rewrites = planner.rewrites.clone();
        let rewritten: HashSet<CommitId> = rewrites.iter().map(|(old, _)| *old).collect();
        let new = |commit: CommitId| planner.commits[&commit];

        let working_copy_move = rewritten
            .contains(&working_copy)
            .then(|| Moved::from_to(Some(working_copy), Some(new(working_copy))));
        let head_move = match &head {
            Head::Detached(commit) if rewritten.contains(commit) => Some(Moved::from_to(
                Some(head.clone()),
                Some(Head::Detached(new(*commit))),
            )),
            _ => None,
        };
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
                .change()
                .commits()
                .iter()
                .any(|commit| rewritten.contains(commit))
        });

        // The roots that stay: the pointers after the purge, and the entries that are not
        // replaced. What they reach is not deleted.
        let mut kept_roots = vec![new(working_copy)];
        kept_roots.extend(branches.iter().map(|(_, commit)| new(*commit)));
        if let Head::Detached(commit) = &head {
            kept_roots.push(new(*commit));
        }
        kept_roots.extend(kept.iter().flat_map(|entry| entry.change().commits()));

        planner.scan()?;
        let reached = planner.reached(&kept_roots)?;
        let mut commits: Vec<CommitId> = planner
            .rewrites
            .iter()
            .map(|(old, _)| *old)
            .filter(|old| !reached.contains(&old.object_id()))
            .collect();
        commits.sort();
        let mut trees: Vec<TreeId> = planner
            .trees
            .iter()
            .filter(|(old, new)| new.is_some() && !reached.contains(&old.object_id()))
            .map(|(old, _)| *old)
            .collect();
        trees.sort();

        Ok(PurgePlan {
            blob,
            rewrites,
            head: head_move,
            working_copy: working_copy_move,
            branches: branch_moves,
            entries: replaced.iter().map(|entry| entry.number()).collect(),
            commits,
            trees,
        })
    }
}

/// Rewrites commits and trees in memory, with a memo, so that each object is read and rewritten
/// once.
struct Planner<'a> {
    objects: &'a FsObjectStore,
    blob: BlobId,
    /// Every tree that was looked at, with its new version if it holds the blob.
    trees: HashMap<TreeId, Option<Tree>>,
    /// The new trees, by their IDs, for the walk of what is kept.
    new_trees: HashMap<TreeId, Tree>,
    /// Every commit that was looked at, with the ID of its new version (the same ID if it does
    /// not change).
    commits: HashMap<CommitId, CommitId>,
    /// The new commits, by their IDs, for the walk of what is kept.
    new_commits: HashMap<CommitId, Commit>,
    /// The commits that change, parents first.
    rewrites: Vec<(CommitId, CommitId)>,
}

impl<'a> Planner<'a> {
    fn new(objects: &'a FsObjectStore, blob: BlobId) -> Self {
        Self {
            objects,
            blob,
            trees: HashMap::new(),
            new_trees: HashMap::new(),
            commits: HashMap::new(),
            new_commits: HashMap::new(),
            rewrites: Vec::new(),
        }
    }

    /// Rewrites the commit and its ancestors. The history can be long, so the walk keeps its own
    /// stack instead of recursing: a commit is done once all its parents are done.
    fn commit(&mut self, root: CommitId) -> Result<(), PurgeError> {
        let mut pending = vec![root];
        let mut read: HashMap<CommitId, Commit> = HashMap::new();
        while let Some(&id) = pending.last() {
            if self.commits.contains_key(&id) {
                pending.pop();
                continue;
            }
            let commit = match read.entry(id) {
                hash_map::Entry::Occupied(known) => known.into_mut(),
                hash_map::Entry::Vacant(slot) => {
                    slot.insert(read_commit(self.objects, id).map_err(PurgeError::Read)?)
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
            let parents: Vec<CommitId> = commit
                .parents()
                .iter()
                .map(|parent| self.commits[parent])
                .collect();
            let tree = self.tree(commit.tree())?;
            if tree.is_none() && parents == commit.parents() {
                self.commits.insert(id, id);
                continue;
            }
            let rewritten = commit
                .reparent(tree.unwrap_or(commit.tree()), parents)
                .map_err(|error| match error {
                    CommitError::DuplicateParent(parent) => {
                        PurgeError::DuplicateParent { commit: id, parent }
                    }
                })?;
            let new = rewritten.id();
            self.commits.insert(id, new);
            self.new_commits.insert(new, rewritten);
            self.rewrites.push((id, new));
        }
        Ok(())
    }

    /// Returns the ID of the new version of the tree, or `None` if it does not hold the blob. A
    /// tree is as deep as the directories, so the recursion is bounded by the paths.
    fn tree(&mut self, id: TreeId) -> Result<Option<TreeId>, PurgeError> {
        if let Some(known) = self.trees.get(&id) {
            return Ok(known.as_ref().map(Tree::id));
        }
        let raw =
            read_object(self.objects, id.object_id(), Kind::Tree).map_err(PurgeError::Read)?;
        let tree = Tree::parse(raw.body())
            .map_err(|source| PurgeError::Read(SnapshotError::Tree { id, source }))?;
        let mut changed = false;
        let mut entries = Vec::with_capacity(tree.entries().len());
        for entry in tree.entries() {
            if FileMode::from_entry_mode(entry.mode()).is_some() {
                if entry.id() == self.blob.object_id() {
                    changed = true;
                } else {
                    entries.push(entry.clone());
                }
                continue;
            }
            match self.tree(TreeId::new(entry.id()))? {
                None => entries.push(entry.clone()),
                Some(new) => {
                    changed = true;
                    if !self.new_trees[&new].entries().is_empty() {
                        entries.push(
                            TreeEntry::directory(entry.name(), new)
                                .expect("the name is that of a valid entry"),
                        );
                    }
                }
            }
        }
        let new = changed.then(|| Tree::new(entries).expect("the entries of a valid tree"));
        let new_id = new.as_ref().map(Tree::id);
        if let Some(tree) = &new {
            self.new_trees.insert(tree.id(), tree.clone());
        }
        self.trees.insert(id, new);
        Ok(new_id)
    }

    /// Looks at every tree and commit in the store that the walk from the roots did not reach, so
    /// that one that holds the blob is rewritten in memory too, and its old version joins the
    /// delete set. A damaged object is an error: it may name the blob.
    fn scan(&mut self) -> Result<(), PurgeError> {
        for id in self.objects.ids().map_err(PurgeError::Store)? {
            let known = self.trees.contains_key(&TreeId::new(id))
                || self.commits.contains_key(&CommitId::new(id))
                || id == self.blob.object_id();
            if known {
                continue;
            }
            // An object that is gone since the listing names nothing any more.
            let Some(raw) = self.objects.get(&id).map_err(PurgeError::Store)? else {
                continue;
            };
            match raw.kind() {
                Kind::Tree => {
                    self.tree(TreeId::new(id))?;
                }
                Kind::Commit => self.commit(CommitId::new(id))?,
                _ => {}
            }
        }
        Ok(())
    }

    /// The commits and trees that the roots reach after the purge, through the parents and the
    /// trees, with the new objects taken from memory.
    fn reached(&self, roots: &[CommitId]) -> Result<HashSet<ObjectId>, PurgeError> {
        let mut seen = HashSet::new();
        let mut commits: Vec<CommitId> = roots.to_vec();
        let mut trees = Vec::new();
        while let Some(id) = commits.pop() {
            if !seen.insert(id.object_id()) {
                continue;
            }
            let commit = match self.new_commits.get(&id) {
                Some(commit) => commit.clone(),
                None => read_commit(self.objects, id).map_err(PurgeError::Read)?,
            };
            trees.push(commit.tree());
            commits.extend(commit.parents());
        }
        while let Some(id) = trees.pop() {
            if !seen.insert(id.object_id()) {
                continue;
            }
            let tree = match self.new_trees.get(&id) {
                Some(tree) => tree.clone(),
                None => {
                    let raw = read_object(self.objects, id.object_id(), Kind::Tree)
                        .map_err(PurgeError::Read)?;
                    Tree::parse(raw.body())
                        .map_err(|source| PurgeError::Read(SnapshotError::Tree { id, source }))?
                }
            };
            for entry in tree.entries() {
                if FileMode::from_entry_mode(entry.mode()).is_none() {
                    trees.push(TreeId::new(entry.id()));
                }
            }
        }
        Ok(seen)
    }
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
    /// The pointers cannot be read.
    Refs(RefError),
    /// The operation log cannot be read.
    Log(OpLogError),
    /// A commit or a tree is missing, or it cannot be read.
    Read(SnapshotError),
    /// The store cannot be listed, or an object in it is damaged, so it is not known whether it
    /// names the blob.
    Store(StoreError),
    /// Two parents of a commit become the same commit, because they differed only in the blob.
    /// A commit with two parents is a merge, which comes with phase 3.
    DuplicateParent {
        /// The commit.
        commit: CommitId,
        /// The new parent that it would have twice.
        parent: CommitId,
    },
}

impl fmt::Display for PurgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent => write!(
                f,
                "an agent may not purge, as a purge removes history for good; a human has to run \
                 it, for example the sponsor of the agent"
            ),
            Self::Update(error) => write!(f, "could not purge: {error}"),
            Self::NoSuchBlob { id, found: None } => {
                write!(f, "the object store has no blob {id}")
            }
            Self::NoSuchBlob {
                id,
                found: Some(kind),
            } => write!(f, "{id} is a {}, not a blob", kind.name()),
            Self::Refs(error) => write!(f, "could not read the references: {error}"),
            Self::Log(error) => write!(f, "could not read the operation log: {error}"),
            Self::Read(error) => write!(f, "could not read the history: {error}"),
            Self::Store(error) => write!(
                f,
                "could not look at every object, so it is not known whether one names the blob: \
                 {error}"
            ),
            Self::DuplicateParent { commit, parent } => write!(
                f,
                "commit {commit} would have the parent {parent} twice after the purge, which a \
                 commit cannot have"
            ),
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
            Self::Store(error) => error.source(),
            Self::Agent | Self::NoSuchBlob { .. } | Self::DuplicateParent { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::object::Blob;
    use crate::repository::InitOptions;
    use crate::testutil::TempDir;
    use crate::{AllowAll, Decision};

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn authorize(&self, _actor: &Actor, _operation: &str, _target: &str) -> Decision {
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

    fn secret() -> ObjectId {
        Blob::new("key=hunter2").id().object_id()
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
        planner.new_commits[&new].clone()
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
        write(dir, "config/key", "key=hunter2");
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
            plan.working_copy(),
            Some(&Moved::from_to(Some(working_copy), Some(new(working_copy))))
        );
        let main = BranchName::new("main").unwrap();
        assert_eq!(
            plan.branches(),
            [(main, Moved::from_to(Some(third), Some(new(third))))]
        );
        assert_eq!(plan.head(), None, "HEAD names the branch");
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
        let tree = planner.new_commits[&planner.commits[&second]].tree();
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
    fn the_entries_that_name_a_rewritten_commit_are_replaced_and_no_other() {
        let dir = TempDir::new();
        let repository = repository(&dir);
        let [_, second, _] = history(&dir, &repository);
        let plan = plan(&repository);
        let rewritten: HashSet<CommitId> = plan.rewrites().iter().map(|(old, _)| *old).collect();
        for entry in repository.oplog().entries().unwrap() {
            let names = entry
                .change()
                .commits()
                .iter()
                .any(|commit| rewritten.contains(commit));
            assert_eq!(plan.entries().contains(&entry.number()), names);
        }
        assert!(plan.entries().windows(2).all(|pair| pair[0] < pair[1]));
        assert!(rewritten.contains(&second));
        assert!(!plan.entries().contains(&1), "the entry of init is kept");
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
        let tree = store.put_tree(&Tree::new(vec![entry]).unwrap()).unwrap();
        let orphan = Commit::new(
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
        assert_eq!(plan.working_copy(), None);
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
}
