//! The answering side of a fetch (ADR-023): the operations that answer the list of references
//! and a fetch request from a repository.
//!
//! Both run on every repository, on a bare one in a hub and on one with a working copy alike
//! (ADR-022). They write nothing but the stream: they take no lock, keep no state between two
//! requests, and leave no file.
//!
//! - **The list of references.** [`Repository::answer_refs`] lists the branches and the tags of
//!   the store. What a list names beyond them (the ID of the repository, the keys of the hub,
//!   and the record entry of each name) is not kept by a store and comes from the caller
//!   ([`ListFacts`]).
//! - **The fetch.** [`Repository::answer_fetch`] writes the object stream for a request. A
//!   `want` must be reachable from a branch or a tag of the repository, the references that the
//!   list names; a remote reference of a clone names nothing that is served. A `have` counts
//!   if one of these references reaches it too. Every other one is ignored: a commit that the
//!   store does not hold, and one that it holds unnamed, whose history may have gaps.
//! - **What is sent.** The commits that a `want` reaches and that no known `have` reaches, and
//!   for each of them the trees and the blobs that no tree of a parent holds with the same
//!   ID at the same path. An object is sent once per stream. An object that the receiver has by
//!   another way, such as a blob that returns to an older content, is sent again: the answer may
//!   hold too much and never too little (ADR-023).
//! - **The order.** Ancestors come first, and the blobs and the trees of a commit come before
//!   the commit, a tree after what it names. So every beginning of the stream that ends with a
//!   commit is a complete state for a receiver that has the `have` commits, and a transfer that
//!   was cut off resumes with the commits that arrived as further `have` lines.
//! - **The refusal.** A `want` that no branch and no tag reaches is refused before the first
//!   byte of the stream is written ([`AnswerError::UnknownWant`]), in the same words for a
//!   commit that the store lacks and for one that it holds unnamed, as the objects of a refused
//!   push are.
//!
//! The walk over the commits reads every commit that it sends, and every commit below the known
//! `have` commits, once, also for a fetch that brings one new commit: only the whole of that
//! history says which commits a `have` reaches. A `want` or a `have` that is no tip of a
//! reference costs the history below the references on top, until it is found, and all of it
//! if it is not found: a `want` that is refused, and a `have` that the store holds unnamed. A
//! `have` that the store does not hold costs one lookup. Writing the stream then reads every
//! commit that is sent once more, its parents, its new trees and blobs, and the trees of the
//! parents at the
//! changed paths.
//!
//! [`Answering`] joins the two operations to the trait [`OtherSide`], so that a
//! [`MemoryTransport`](crate::transport::MemoryTransport) answers from a repository.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::io::{self, Write};

use crate::conflict::AnyCommit;
use crate::history::{read_commit, HistoryError};
use crate::object::{CommitId, EntryMode, Kind, ObjectId, Tree, TreeEntry, TreeId};
use crate::protocol::record::{Chain, HubKeys};
use crate::protocol::{
    ErrorMessage, FetchRequest, ObjectStreamWriter, QualifiedName, RecordHash, RefList, Reference,
    RepositoryId,
};
use crate::refs::{RefError, RefKind};
use crate::repository::Repository;
use crate::store::{FsObjectStore, ObjectStore, ReadError, StoreError};
use crate::transport::{OtherSide, Refusal};

/// The hash of the last record entry of a name, or `None` for a name that the record does not
/// list.
type EntryOf<'a> = Box<dyn Fn(&QualifiedName) -> Option<RecordHash> + Send + Sync + 'a>;

/// What a list of references names beyond the references of a store (ADR-023): the ID of the
/// repository (ADR-027), the keys of the hub (ADR-026), and for each name the hash of its last
/// record entry (ADR-025). A hub knows the ID and the entries from its record and holds its
/// keys itself; a store holds none of them.
///
/// The value borrows what it names, so a hub builds one per request.
pub struct ListFacts<'a> {
    repository: RepositoryId,
    keys: &'a HubKeys,
    entry_of: EntryOf<'a>,
}

impl<'a> ListFacts<'a> {
    /// The facts of a repository: its ID, the keys of the hub, and the hash of the last record
    /// entry of a name, or `None` for a name that the record does not list.
    pub fn new(
        repository: RepositoryId,
        keys: &'a HubKeys,
        entry_of: impl Fn(&QualifiedName) -> Option<RecordHash> + Send + Sync + 'a,
    ) -> Self {
        Self {
            repository,
            keys,
            entry_of: Box::new(entry_of),
        }
    }

    /// The facts as the record of a repository leaves them: the ID and the entries of `chain`,
    /// with the keys of the hub. The chain must have been read from its first entry: one that
    /// began in the middle ([`Chain::after`]) knows no entry of a name that was listed before,
    /// and the list then fails for that name.
    pub fn of_chain(chain: &'a Chain, keys: &'a HubKeys) -> Self {
        Self::new(chain.repository(), keys, |name| chain.entry_of(name))
    }
}

impl fmt::Debug for ListFacts<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The entries are a function, so only the values are shown.
        f.debug_struct("ListFacts")
            .field("repository", &self.repository)
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

/// Why the list of references cannot be answered. Both are failures of this side and no message
/// of the protocol.
#[derive(Debug)]
#[non_exhaustive]
pub enum ListError {
    /// The references of the repository could not be read.
    Refs(Box<RefError>),
    /// A reference of the store has no record entry in the facts of the caller, so the list
    /// cannot name it.
    NoRecordEntry(QualifiedName),
}

impl ListError {
    /// Returns the error of the store inside the error of the references, if the store could
    /// not answer.
    pub fn store_error(&self) -> Option<&StoreError> {
        match self {
            Self::Refs(source) => source.store_error(),
            Self::NoRecordEntry(_) => None,
        }
    }

    /// Returns the error of the file system if that is what failed while the references were
    /// read (see the crate documentation).
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Refs(source) => source.io_error(),
            Self::NoRecordEntry(_) => None,
        }
    }
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refs(source) => write!(f, "{source}"),
            Self::NoRecordEntry(name) => {
                write!(f, "the reference {name} has no entry in the record")
            }
        }
    }
}

impl Error for ListError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message includes that of the cause, so the source is the cause of the cause (see
        // the crate documentation).
        match self {
            Self::Refs(source) => source.source(),
            Self::NoRecordEntry(_) => None,
        }
    }
}

impl From<RefError> for ListError {
    fn from(source: RefError) -> Self {
        Self::Refs(Box::new(source))
    }
}

/// What an answer to a fetch request carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Sent {
    /// The number of objects in the stream.
    pub objects: u64,
    /// The number of commits among them, of the kind `commit` or `commit2`.
    pub commits: u64,
    /// The bytes of the bodies of the objects.
    pub bytes: u64,
}

/// Why a fetch request is not answered with its object stream. One of them is a refusal that
/// the other side hears as an error message ([`answer`](Self::answer)); every other one is a
/// failure of this side. The errors inside are boxed, because the unboxed ones are too large
/// for clippy's `result_large_err` lint.
#[derive(Debug)]
#[non_exhaustive]
pub enum AnswerError {
    /// A fetch request wants a commit that no branch and no tag of the repository reaches
    /// (ADR-023). Carries the first such commit in the order of the IDs. Nothing was written.
    UnknownWant(CommitId),
    /// The references of the repository could not be read.
    Refs(Box<RefError>),
    /// The history below the references or below a wanted commit cannot be read: a commit is
    /// missing, is refused by its reader, or the store could not return it, because the storage
    /// failed, the file is damaged, or the object is no commit
    /// ([`store_error`](Self::store_error)).
    History(Box<HistoryError>),
    /// A tree or a blob that a commit of the answer names is not in the store.
    Missing {
        /// The object that is absent.
        id: ObjectId,
        /// The kind that the object must have.
        expected: Kind,
        /// The commit or the tree that names it.
        needed_by: ObjectId,
    },
    /// A tree that a commit of the answer names is intact as an object, and the strict reader
    /// refuses its body (ADR-005) or returns a value with another ID. The source is never
    /// [`ReadError::Store`]: an error of the store is [`Store`](Self::Store).
    Unreadable {
        /// The commit or the tree that names it.
        needed_by: ObjectId,
        /// The refusal of the reader, which names the tree.
        source: Box<ReadError>,
    },
    /// The store could not answer: it could not say whether it holds a `have`, or it could not
    /// return a tree or a blob of the answer, because the storage failed, the file is damaged,
    /// or the object has another kind than the commit or the tree that names it claims. The
    /// same failure for a commit is [`History`](Self::History).
    Store(Box<StoreError>),
    /// The stream could not be written: its first line, or an object after it. The other side
    /// may hold a part of it, cut off anywhere.
    Io(io::Error),
}

impl AnswerError {
    /// The error message that answers the request, if the error is a refusal:
    /// [`ErrorMessage::UnknownWant`] for [`UnknownWant`](Self::UnknownWant). `None` for a
    /// failure of this side, which is no message of the protocol: a carrier reports it in its
    /// own words.
    pub fn answer(&self) -> Option<ErrorMessage> {
        match self {
            Self::UnknownWant(commit) => Some(ErrorMessage::UnknownWant(*commit)),
            _ => None,
        }
    }

    /// Returns the error of the store if the store could not answer, also inside the error of
    /// the references or of the history. `None` for every other error; the source of
    /// [`Unreadable`](Self::Unreadable) is never one of the store.
    pub fn store_error(&self) -> Option<&StoreError> {
        match self {
            Self::Refs(source) => source.store_error(),
            Self::History(source) => source.store_error(),
            Self::Store(source) => Some(source),
            Self::UnknownWant(_) | Self::Missing { .. } | Self::Unreadable { .. } | Self::Io(_) => {
                None
            }
        }
    }

    /// Returns the error of the file system if that is what failed, in the store or while the
    /// references were read (see the crate documentation). `None` for [`Io`](Self::Io): the
    /// error of the writer of the stream is that of the carrier, so that a connection that
    /// broke is not taken for a failing disk. A caller that wants it matches on the variant.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Io(_) => None,
            Self::Refs(source) => source.io_error(),
            _ => self.store_error().and_then(StoreError::io_error),
        }
    }
}

impl fmt::Display for AnswerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownWant(commit) => write!(
                f,
                "no reference of this repository reaches the wanted commit {commit}"
            ),
            Self::Refs(source) => write!(f, "{source}"),
            Self::History(source) => write!(f, "{source}"),
            Self::Missing {
                id,
                expected,
                needed_by,
            } => write!(
                f,
                "the {} {id} is missing, which {needed_by} names",
                expected.name()
            ),
            Self::Unreadable { needed_by, source } => {
                write!(f, "{source} ({needed_by} names the object)")
            }
            Self::Store(source) => write!(f, "{source}"),
            Self::Io(source) => write!(f, "cannot write the object stream: {source}"),
        }
    }
}

impl Error for AnswerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // The message includes that of the cause, so the source is the cause of the cause (see
        // the crate documentation).
        match self {
            Self::UnknownWant(_) | Self::Missing { .. } => None,
            Self::Refs(source) => source.source(),
            Self::History(source) => source.source(),
            Self::Unreadable { source, .. } => source.source(),
            Self::Store(source) => source.source(),
            Self::Io(source) => source.source(),
        }
    }
}

impl From<RefError> for AnswerError {
    fn from(source: RefError) -> Self {
        Self::Refs(Box::new(source))
    }
}

impl From<HistoryError> for AnswerError {
    fn from(source: HistoryError) -> Self {
        Self::History(Box::new(source))
    }
}

impl From<StoreError> for AnswerError {
    fn from(source: StoreError) -> Self {
        Self::Store(Box::new(source))
    }
}

/// The commits that the references name which a list of references names: the branches and the
/// tags. The remote references of a clone are not among them.
fn served_tips(repository: &Repository) -> Result<BTreeSet<CommitId>, RefError> {
    let refs = repository.refs();
    let mut tips = BTreeSet::new();
    tips.extend(refs.branches()?.into_iter().map(|(_, commit)| commit));
    tips.extend(refs.tags()?.into_iter().map(|(_, commit)| commit));
    Ok(tips)
}

/// The commit graph as far as an answer has read it: the parents of every commit that was read.
struct Graph<'a> {
    store: &'a FsObjectStore,
    parents: HashMap<CommitId, Vec<CommitId>>,
}

impl<'a> Graph<'a> {
    fn new(store: &'a FsObjectStore) -> Self {
        Self {
            store,
            parents: HashMap::new(),
        }
    }

    /// The parents of a commit, read once. `child` is the commit that names it, for the error.
    fn parents(
        &mut self,
        id: CommitId,
        child: Option<CommitId>,
    ) -> Result<&[CommitId], HistoryError> {
        if !self.parents.contains_key(&id) {
            let commit = read_commit(self.store, id, child)?;
            self.parents.insert(id, commit.parents().to_vec());
        }
        Ok(&self.parents[&id])
    }

    /// The commits of `starts` and everything below them. Every one of them is read.
    fn closure(
        &mut self,
        starts: impl IntoIterator<Item = CommitId>,
    ) -> Result<HashSet<CommitId>, HistoryError> {
        let mut seen = HashSet::new();
        let mut pending: Vec<(CommitId, Option<CommitId>)> = Vec::new();
        for start in starts {
            if seen.insert(start) {
                pending.push((start, None));
            }
        }
        while let Some((id, child)) = pending.pop() {
            for &parent in self.parents(id, child)? {
                if seen.insert(parent) {
                    pending.push((parent, Some(id)));
                }
            }
        }
        Ok(seen)
    }

    /// The commits of `want` and below them that are not in `had`, in the order of the stream:
    /// a commit after its parents. The walk does not go below a commit of `had`, which holds
    /// everything below its commits.
    fn commits_to_send(
        &mut self,
        want: &BTreeSet<CommitId>,
        had: &HashSet<CommitId>,
    ) -> Result<Vec<CommitId>, HistoryError> {
        let mut order = Vec::new();
        let mut listed: HashSet<CommitId> = HashSet::new();
        // A commit to look at with the commit that names it, or a commit whose parents are all
        // listed.
        let mut steps: Vec<(CommitId, Option<CommitId>, bool)> = want
            .iter()
            .rev()
            .map(|&commit| (commit, None, false))
            .collect();
        while let Some((id, child, parents_listed)) = steps.pop() {
            if parents_listed {
                order.push(id);
                continue;
            }
            if had.contains(&id) || !listed.insert(id) {
                continue;
            }
            steps.push((id, child, true));
            let parents = self.parents(id, child)?;
            steps.extend(
                parents
                    .iter()
                    .rev()
                    .map(|&parent| (parent, Some(id), false)),
            );
        }
        Ok(order)
    }
}

/// Which commits the served references reach, as far as the history below them has been read:
/// it is read only until a commit is found, so a tip costs nothing and the answer "no" costs
/// all of it.
struct Reach {
    reached: HashSet<CommitId>,
    /// The reached commits whose parents are not read yet, each with the commit that names it.
    frontier: Vec<(CommitId, Option<CommitId>)>,
}

impl Reach {
    fn new(tips: &BTreeSet<CommitId>) -> Self {
        Self {
            reached: tips.iter().copied().collect(),
            frontier: tips.iter().map(|&tip| (tip, None)).collect(),
        }
    }

    /// Whether a served reference reaches the commit.
    fn reaches(&mut self, graph: &mut Graph<'_>, commit: CommitId) -> Result<bool, HistoryError> {
        while !self.reached.contains(&commit) {
            let Some((next, child)) = self.frontier.pop() else {
                return Ok(false);
            };
            for &parent in graph.parents(next, child)? {
                if self.reached.insert(parent) {
                    self.frontier.push((parent, Some(next)));
                }
            }
        }
        Ok(true)
    }
}

/// The kind of a commit as it is stored.
fn kind_of(commit: &AnyCommit) -> Kind {
    match commit {
        AnyCommit::Commit(_) => Kind::Commit,
        AnyCommit::Commit2(_) => Kind::Commit2,
    }
}

/// A step of the walk over the trees of a commit: a tree to look at, with the trees of the
/// parents at its path and the object that names it, or a tree whose entries are written.
enum TreeStep {
    Enter(TreeId, Vec<TreeId>, ObjectId),
    Exit(TreeId, Tree),
}

/// The writer of an answer: the stream, and what it has written so far.
struct Sender<'a, W: Write> {
    store: &'a FsObjectStore,
    stream: ObjectStreamWriter<W>,
    /// The trees and the blobs that the stream holds already.
    written: HashSet<ObjectId>,
    sent: Sent,
}

impl<W: Write> Sender<'_, W> {
    fn write(&mut self, kind: Kind, body: &[u8]) -> Result<(), AnswerError> {
        self.stream.write(kind, body).map_err(AnswerError::Io)?;
        self.sent.objects += 1;
        self.sent.bytes += body.len() as u64;
        Ok(())
    }

    /// Writes a commit after the trees and the blobs that it adds to its parents.
    fn send_commit(&mut self, id: CommitId) -> Result<(), AnswerError> {
        // The commit was read when the graph was walked; a purge may have removed it since.
        let commit = read_commit(self.store, id, None)?;
        // The trees of the parents only save work: a parent that cannot be read saves none,
        // and the walk then sends what it would have left out.
        let mut bases = Vec::new();
        for parent in commit.parents() {
            if let Ok(Some(parent)) = self.store.get_any_commit(parent) {
                bases.extend(parent.tree_sum().terms().iter().copied());
            }
        }
        for &tree in commit.tree_sum().terms() {
            self.send_tree(tree, bases.clone(), id.object_id())?;
        }
        self.write(kind_of(&commit), &commit.encode())?;
        self.sent.commits += 1;
        Ok(())
    }

    /// Writes a tree after the trees and the blobs below it, leaving out every entry that a
    /// tree of `bases`, the trees of the parents at this path, holds with the same ID, and
    /// every object that the stream holds already. The walk keeps its own stack, so the depth
    /// of a tree costs no stack of the program.
    fn send_tree(
        &mut self,
        root: TreeId,
        bases: Vec<TreeId>,
        needed_by: ObjectId,
    ) -> Result<(), AnswerError> {
        let mut steps = vec![TreeStep::Enter(root, bases, needed_by)];
        while let Some(step) = steps.pop() {
            let (tree_id, bases, needed_by) = match step {
                TreeStep::Enter(tree_id, bases, needed_by) => (tree_id, bases, needed_by),
                TreeStep::Exit(tree_id, tree) => {
                    // Everything below the tree is written, so it counts as written from here
                    // on. A second entry of the same tree comes after this exit and is skipped.
                    self.written.insert(tree_id.object_id());
                    self.write(Kind::Tree, &tree.encode())?;
                    continue;
                }
            };
            let object = tree_id.object_id();
            if bases.contains(&tree_id) || self.written.contains(&object) {
                continue;
            }
            let tree = match self.store.get_tree(&tree_id) {
                Ok(Some(tree)) => tree,
                Ok(None) => {
                    return Err(AnswerError::Missing {
                        id: object,
                        expected: Kind::Tree,
                        needed_by,
                    })
                }
                Err(ReadError::Store(source)) => return Err(AnswerError::Store(source)),
                Err(source) => {
                    return Err(AnswerError::Unreadable {
                        needed_by,
                        source: Box::new(source),
                    })
                }
            };
            // As for the parents of the commit: a tree of a parent that cannot be read saves no
            // work, and what it would have pruned is sent.
            let bases: Vec<Tree> = bases
                .iter()
                .filter_map(|base| self.store.get_tree(base).ok().flatten())
                .collect();
            let bases: Vec<HashMap<&str, &TreeEntry>> = bases
                .iter()
                .map(|base| {
                    base.entries()
                        .iter()
                        .map(|entry| (entry.name(), entry))
                        .collect()
                })
                .collect();
            let mut below_trees = Vec::new();
            for entry in tree.entries() {
                let is_directory = entry.mode() == EntryMode::Directory;
                // The entries of the parents with this name and of the same sort, as the
                // connectivity check of the receiver compares them.
                let below: Vec<ObjectId> = bases
                    .iter()
                    .filter_map(|base| base.get(entry.name()))
                    .filter(|found| (found.mode() == EntryMode::Directory) == is_directory)
                    .map(|found| found.id())
                    .collect();
                if below.contains(&entry.id()) {
                    continue;
                }
                if is_directory {
                    let below = below.into_iter().map(TreeId::new).collect();
                    below_trees.push(TreeStep::Enter(TreeId::new(entry.id()), below, object));
                } else if self.written.insert(entry.id()) {
                    self.send_blob(entry.id(), object)?;
                }
            }
            steps.push(TreeStep::Exit(tree_id, tree));
            steps.extend(below_trees);
        }
        Ok(())
    }

    fn send_blob(&mut self, id: ObjectId, needed_by: ObjectId) -> Result<(), AnswerError> {
        let Some(blob) = self.store.get_expecting(&id, Kind::Blob)? else {
            return Err(AnswerError::Missing {
                id,
                expected: Kind::Blob,
                needed_by,
            });
        };
        self.write(Kind::Blob, blob.body())
    }
}

impl Repository {
    /// Answers the list of references (ADR-023): the branches and the tags of this repository,
    /// each with its commit and with the record entry that `facts` names for it.
    ///
    /// The references are read one after the other without a lock, so the list is no snapshot
    /// across all of them (ADR-025). The call works on a bare repository and on one with a
    /// working copy alike, and it does not compare a reference with the record: which of the
    /// two is ahead while a ref update runs is the matter of the accepting operation.
    ///
    /// # Errors
    ///
    /// [`ListError::Refs`] if the references cannot be read, and [`ListError::NoRecordEntry`]
    /// for the first reference that `facts` names no entry for, the branches before the tags.
    pub fn answer_refs(&self, facts: &ListFacts<'_>) -> Result<RefList, ListError> {
        let refs = self.refs();
        let branches = refs.branches()?.into_iter().map(|at| (RefKind::Branch, at));
        let tags = refs.tags()?.into_iter().map(|at| (RefKind::Tag, at));
        let mut references = Vec::new();
        for (kind, (name, commit)) in branches.chain(tags) {
            let name = QualifiedName::new(kind, name);
            let Some(entry) = (facts.entry_of)(&name) else {
                return Err(ListError::NoRecordEntry(name));
            };
            references.push(Reference::new(name, commit, entry));
        }
        // The keys of a hub hold former keys that their current key signed, and a store lists a
        // name once per kind.
        Ok(RefList::try_new(
            facts.repository,
            facts.keys.current().clone(),
            facts.keys.former_keys().clone(),
            references,
        )
        .expect("the keys of a hub and the references of a store make a list"))
    }

    /// Answers a fetch request (ADR-023): writes to `stream` the object stream of everything
    /// that the `want` commits reach and the `have` commits do not, in the order of the format.
    /// See the [module](crate::serve) for what is sent and for the cost of the walk.
    ///
    /// Every `want` must be reachable from a branch or a tag of this repository. A `have` is a
    /// claim of the other side that it holds the commit with everything below it (ADR-015). It
    /// counts if a branch or a tag of this repository reaches it; one that the store does not
    /// hold, or holds unnamed, is ignored.
    ///
    /// The call takes no lock and keeps no state, and it works on a bare repository and on one
    /// with a working copy alike. `stream` is written as the walk goes and is not flushed; a
    /// caller that writes to a file or a socket wraps it in a buffer.
    ///
    /// A purge that runs beside the call may remove an object that the answer sends. A commit
    /// that is gone when the graph is walked fails the call before the first byte; a commit, a
    /// tree, or a blob that is gone when it is written cuts the stream off. A parent or a tree
    /// of a parent that is gone, or that cannot be read for any other reason, fails nothing and
    /// is not reported: the answer then holds more.
    ///
    /// # Errors
    ///
    /// Before the first byte is written: [`AnswerError::UnknownWant`] for a `want` that no
    /// reference reaches, [`AnswerError::Refs`] if the references cannot be read,
    /// [`AnswerError::History`] if a commit below the references is missing or cannot be read,
    /// and [`AnswerError::Store`] if the store cannot say whether it holds a `have`. After it,
    /// with a stream that is cut off: [`AnswerError::Missing`],
    /// [`AnswerError::Unreadable`], [`AnswerError::Store`], and [`AnswerError::History`] for an
    /// object that the store does not return. [`AnswerError::Io`] if `stream` fails, at its
    /// first line or later.
    pub fn answer_fetch<W: Write>(
        &self,
        request: &FetchRequest,
        stream: W,
    ) -> Result<Sent, AnswerError> {
        let tips = served_tips(self)?;
        let mut graph = Graph::new(self.objects());
        let mut reach = Reach::new(&tips);
        for &commit in request.want() {
            if !reach.reaches(&mut graph, commit)? {
                return Err(AnswerError::UnknownWant(commit));
            }
        }
        let mut known = Vec::new();
        for &commit in request.have() {
            // The lookup spares the walk below the references for a commit that only the other
            // side has, which is the usual `have` that is not known here.
            if self.objects().contains(&commit.object_id())? && reach.reaches(&mut graph, commit)? {
                known.push(commit);
            }
        }
        let had = graph.closure(known)?;
        let commits = graph.commits_to_send(request.want(), &had)?;
        drop((graph, reach, had));
        let mut sender = Sender {
            store: self.objects(),
            stream: ObjectStreamWriter::new(stream).map_err(AnswerError::Io)?,
            written: HashSet::new(),
            sent: Sent::default(),
        };
        for commit in commits {
            sender.send_commit(commit)?;
        }
        Ok(sender.sent)
    }
}

/// A repository as the other side of a [`MemoryTransport`](crate::transport::MemoryTransport):
/// it answers the list of references and a fetch request through
/// [`Repository::answer_refs`] and [`Repository::answer_fetch`] (ADR-022).
///
/// A refusal travels as its error message ([`AnswerError::answer`]), and every other error as
/// a side that failed, with its message as the detail. What such a call had written of a stream
/// is discarded by the transport in memory, so its caller sees no stream that is cut off. A
/// carrier over a network answers a failure in its own words and does not pass the message on.
#[derive(Debug)]
pub struct Answering<'a> {
    repository: &'a Repository,
    facts: ListFacts<'a>,
}

impl<'a> Answering<'a> {
    /// The other side that answers from `repository`, with the facts of its list of references.
    pub fn new(repository: &'a Repository, facts: ListFacts<'a>) -> Self {
        Self { repository, facts }
    }
}

impl OtherSide for Answering<'_> {
    fn refs(&self) -> Result<RefList, Refusal> {
        self.repository
            .answer_refs(&self.facts)
            .map_err(|error| Refusal::Failed(error.to_string()))
    }

    fn fetch(&self, request: &FetchRequest, stream: &mut dyn Write) -> Result<(), Refusal> {
        self.repository
            .answer_fetch(request, stream)
            .map(|_| ())
            .map_err(|error| match error.answer() {
                Some(message) => Refusal::Error(message),
                None => Refusal::Failed(error.to_string()),
            })
    }
}

#[cfg(test)]
mod tests;
