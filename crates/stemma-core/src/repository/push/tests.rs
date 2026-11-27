//! Tests of the push and of the merge with a remote (STEMMA-309). A clone reaches the hub through
//! [`MemoryTransport`]; the other side is [`Hub`], a bare repository with its record, its
//! statements, and its anchors in memory, which answers every request of a push through the
//! operations of the core: the list of references from its record, the fetch, the upload, the
//! ref update, the statements, and the anchor. Knobs on the hub break a request on purpose.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::io::{self, BufRead, Write};

use super::*;
use crate::change::ChangeId;
use crate::index::Index;
use crate::object::{Blob, Commit, FileMode};
use crate::oplog::{CheckResult, PullEntry, Record};
use crate::protocol::anchor::Anchor;
use crate::protocol::held::HeldStatement;
use crate::protocol::record::tests::Seeded;
use crate::protocol::record::{read_record, Chain, HubKeys, StaleReference};
use crate::protocol::refusal::{GateRefusal, RefusalReason, Refused};
use crate::protocol::statement::{Approval, Body, Checked, Opening, Statement};
use crate::protocol::{FetchRequest, FormerKeys, RefList, RepositoryId};
use crate::receive::Bounds;
use crate::register::{self, Identity, Mode, Register, Role};
use crate::remote::RemoteAddress;
use crate::repository::statements::by_hand;
use crate::repository::{
    AcceptContext, AnchorDestination, GateError, InitOptions, RecordDestination, RemoteSetup,
    StatementDestination,
};
use crate::serve::ListFacts;
use crate::signature::{self, PayloadKind};
use crate::store::ObjectStore;
use crate::testutil::TempDir;
use crate::transport::{MemoryTransport, OtherSide, Refusal};
use crate::{policy, AllowAll};
use std::io::Read;

const ADDRESS: &str = "https://hub.example";
const REPOSITORY: &str = "00112233445566778899aabbccddeeff";
const START: i64 = 1_000_000;

/// The policy of these tests: `docs/` is level 0, every other path level 2.
const POLICY: &str = "stemma-policy 1\nlevel 0 docs/\n";

fn daniel() -> Seeded {
    Seeded::new(1)
}

fn hub_key() -> Seeded {
    Seeded::new(2)
}

fn mia() -> Seeded {
    Seeded::new(3)
}

fn ana() -> Seeded {
    Seeded::new(4)
}

/// A key that no register of these tests holds.
fn stranger() -> Seeded {
    Seeded::new(5)
}

fn bot() -> Seeded {
    Seeded::new(6)
}

fn cat() -> Seeded {
    Seeded::new(7)
}

fn human(id: &str) -> Actor {
    Actor::human(id).unwrap()
}

fn bot_actor() -> Actor {
    Actor::agent("bot", "model", "session", "daniel").unwrap()
}

fn agent(id: &str) -> Identity {
    Identity::of(&Actor::agent(id, "-", "-", "daniel").unwrap())
}

fn origin() -> RemoteName {
    RemoteName::try_new("origin").unwrap()
}

fn address() -> HubAddress {
    HubAddress::try_new(ADDRESS).unwrap()
}

fn repository_id() -> RepositoryId {
    REPOSITORY.parse().unwrap()
}

fn name(text: &str) -> QualifiedName {
    text.parse().unwrap()
}

fn branch(text: &str) -> BranchName {
    BranchName::try_new(text).unwrap()
}

/// The register of these tests: Daniel owns it, Mia and Ana are members, and Daniel sponsors
/// the agents `bot` and `cat`.
fn register_of() -> Register {
    let mut register =
        Register::with_owner(Identity::human("daniel").unwrap(), daniel().public()).unwrap();
    for (id, key) in [("mia", mia()), ("ana", ana())] {
        register
            .add_human(Identity::human(id).unwrap(), Role::Member, key.public())
            .unwrap();
    }
    for (id, key) in [("bot", bot()), ("cat", cat())] {
        let sponsor = Identity::human("daniel").unwrap();
        let payload = register::sponsorship_payload(&agent(id), Role::Member, &sponsor);
        let cosign = signature::sign(&daniel(), PayloadKind::Sponsorship, &payload).unwrap();
        register
            .add_agent(agent(id), Role::Member, sponsor, key.public(), cosign)
            .unwrap();
    }
    register
}

/// A destination in memory for the record, the statements, and the anchors.
#[derive(Default)]
struct Memory(Vec<u8>);

impl RecordDestination for Memory {
    fn read(&mut self) -> io::Result<Vec<u8>> {
        Ok(self.0.clone())
    }

    fn append(&mut self, entry: &[u8]) -> io::Result<()> {
        self.0.extend_from_slice(entry);
        Ok(())
    }
}

impl StatementDestination for Memory {
    fn read(&mut self) -> io::Result<Vec<u8>> {
        Ok(self.0.clone())
    }

    fn append(&mut self, held: &[u8]) -> io::Result<()> {
        self.0.extend_from_slice(held);
        Ok(())
    }
}

impl AnchorDestination for Memory {
    fn read(&mut self) -> io::Result<Vec<u8>> {
        Ok(self.0.clone())
    }

    fn append(&mut self, held: &[u8]) -> io::Result<()> {
        self.0.extend_from_slice(held);
        Ok(())
    }
}

type Hook = Box<dyn FnOnce(&Hub)>;

/// The other side: a bare repository with its record, its statements, its anchors, its clock,
/// and the knobs that break a request.
struct Hub {
    _dir: TempDir,
    repository: Repository,
    record: RefCell<Memory>,
    statements: RefCell<Memory>,
    anchors: RefCell<Memory>,
    time: Cell<i64>,
    /// Every upload request that reached the hub, in its order, also one that failed.
    uploads: RefCell<Vec<Vec<u8>>>,
    /// The numbers of the upload requests, from 0, that fail as a broken connection does: the
    /// hub reports them, and [`After`] turns them into a failure of the carrier.
    broken: RefCell<BTreeSet<usize>>,
    /// How many of the next uploads the hub confirms without storing their objects.
    lose: Cell<u32>,
    /// The ref updates that reached the hub.
    updates: Cell<u32>,
    /// Whether every ref update is answered as stale.
    always_stale: Cell<bool>,
    /// Whether the hub signs its receipts with another key than the pinned one.
    rogue: Cell<bool>,
    /// A step that runs before the next ref update is accepted: another writer.
    before_update: RefCell<Option<Hook>>,
    /// Whether the hub fails every anchor.
    no_anchors: Cell<bool>,
}

impl Hub {
    fn new() -> Self {
        let dir = TempDir::new();
        let repository =
            Repository::init_bare(dir.path().join("hub"), &human("operator"), &AllowAll).unwrap();
        Self {
            _dir: dir,
            repository,
            record: RefCell::default(),
            statements: RefCell::default(),
            anchors: RefCell::default(),
            time: Cell::new(START),
            uploads: RefCell::default(),
            broken: RefCell::default(),
            lose: Cell::new(0),
            updates: Cell::new(0),
            always_stale: Cell::new(false),
            rogue: Cell::new(false),
            before_update: RefCell::new(None),
            no_anchors: Cell::new(false),
        }
    }

    /// The clock of the hub, one second further for every call.
    fn tick(&self) -> i64 {
        self.time.set(self.time.get() + 1);
        self.time.get()
    }

    fn entries(&self) -> Vec<RecordEntry> {
        read_record(&self.record.borrow().0).unwrap()
    }

    fn chain(&self) -> Chain {
        let mut chain = Chain::new(repository_id());
        for entry in self.entries() {
            chain.accept(&entry).unwrap();
        }
        chain
    }

    fn branch(&self, name: &str) -> Option<CommitId> {
        self.repository.refs().branch(&branch(name)).unwrap()
    }

    /// Accepts a ref update as the hub does, past the transport: another writer.
    fn accept(&self, update: &RefUpdate) -> Result<RecordEntry, AcceptError> {
        let hub = address();
        let signer = if self.rogue.get() {
            stranger()
        } else {
            hub_key()
        };
        let mut record = self.record.borrow_mut();
        let mut statements = self.statements.borrow_mut();
        let context = AcceptContext::new(&hub, repository_id(), &mut *record, self.tick(), &signer)
            .with_statements(&mut *statements);
        self.repository.accept_ref_update(update, context)
    }

    /// Submits a statement as the hub accepts it.
    fn submit(&self, statement: &Statement) {
        let hub = address();
        let signer = hub_key();
        let mut record = self.record.borrow_mut();
        let mut statements = self.statements.borrow_mut();
        let context = AcceptContext::new(&hub, repository_id(), &mut *record, self.tick(), &signer);
        self.repository
            .accept_statement(statement, context, &mut *statements)
            .unwrap();
    }

    /// The move of `reference` to `new` from the state that the record leaves it in.
    fn move_of(&self, reference: &str, new: CommitId, evidence: Option<StatementHash>) -> Move {
        let chain = self.chain();
        let reference = name(reference);
        Move::try_new(
            reference.clone(),
            chain.entry_of(&reference),
            chain.commit_of(&reference),
            Some(new),
            evidence,
        )
        .unwrap()
    }
}

use crate::repository::AcceptError;

fn refused(error: impl std::fmt::Display) -> Refusal {
    Refusal::Failed(error.to_string())
}

fn answer_of<E: std::fmt::Display>(error: E, message: Option<ErrorMessage>) -> Refusal {
    match message {
        Some(message) => Refusal::Error(message),
        None => refused(error),
    }
}

impl OtherSide for Hub {
    fn refs(&self) -> Result<RefList, Refusal> {
        let chain = self.chain();
        let keys = HubKeys::try_new(hub_key().public(), FormerKeys::none()).unwrap();
        let facts = ListFacts::of_chain(&chain, &keys);
        self.repository.answer_refs(&facts).map_err(refused)
    }

    fn fetch(&self, request: &FetchRequest, stream: &mut dyn Write) -> Result<(), Refusal> {
        self.repository
            .answer_fetch(request, stream)
            .map(|_| ())
            .map_err(|error| {
                let message = error.answer();
                answer_of(error, message)
            })
    }

    fn upload(&self, stream: &mut dyn BufRead) -> Result<(), Refusal> {
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        let number = self.uploads.borrow().len();
        self.uploads.borrow_mut().push(bytes.clone());
        if self.broken.borrow().contains(&number) {
            return Err(Refusal::Failed(BROKEN.to_owned()));
        }
        if self.lose.get() > 0 {
            self.lose.set(self.lose.get() - 1);
            return Ok(());
        }
        self.repository
            .receive_objects(bytes.as_slice(), Bounds::default())
            .map(|_| ())
            .map_err(refused)
    }

    fn ref_update(&self, update: &RefUpdate) -> Result<RecordEntry, Refusal> {
        self.updates.set(self.updates.get() + 1);
        let hook = self.before_update.borrow_mut().take();
        if let Some(hook) = hook {
            hook(self);
        }
        if self.always_stale.get() {
            let chain = self.chain();
            let references = update
                .payload()
                .moves()
                .iter()
                .map(|moved| StaleReference {
                    name: moved.name().clone(),
                    entry: chain.entry_of(moved.name()),
                    commit: chain.commit_of(moved.name()),
                })
                .collect();
            let stale = Stale::try_new(references).unwrap();
            return Err(Refusal::Error(ErrorMessage::Stale(stale)));
        }
        self.accept(update).map_err(|error| {
            let message = error.answer();
            answer_of(error, message)
        })
    }

    fn submit_statement(&self, statement: &Statement) -> Result<HeldStatement, Refusal> {
        let hub = address();
        let signer = hub_key();
        let mut record = self.record.borrow_mut();
        let mut statements = self.statements.borrow_mut();
        let context = AcceptContext::new(&hub, repository_id(), &mut *record, self.tick(), &signer);
        self.repository
            .accept_statement(statement, context, &mut *statements)
            .map_err(|error| {
                let message = error.answer();
                answer_of(error, message)
            })
    }

    fn anchor(&self, anchor: &Anchor) -> Result<HeldAnchor, Refusal> {
        if self.no_anchors.get() {
            return Err(Refusal::Failed("the anchors are down".to_owned()));
        }
        let hub = address();
        let signer = hub_key();
        let mut record = self.record.borrow_mut();
        let mut anchors = self.anchors.borrow_mut();
        let context = AcceptContext::new(&hub, repository_id(), &mut *record, self.tick(), &signer);
        self.repository
            .accept_anchor(anchor, context, &mut *anchors)
            .map_err(|error| {
                let message = error.answer();
                answer_of(error, message)
            })
    }
}

type Wire = MemoryTransport<Hub>;

/// What the hub reports for an upload request that [`Hub::broken`] names.
const BROKEN: &str = "the connection broke";

/// What [`After`] runs after the hub answered a ref update.
type Step<'a> = Box<dyn FnOnce(&mut Wire) + 'a>;

/// A transport to the hub that runs a step after the hub answered the ref update and before the
/// clone reads the answer, and that may refuse to carry an upload: what another process of the
/// clone, or a transport that knows no upload, does.
struct After<'a> {
    wire: &'a mut Wire,
    step: Option<Step<'a>>,
    carries_upload: bool,
}

impl<'a> After<'a> {
    fn new(wire: &'a mut Wire, step: impl FnOnce(&mut Wire) + 'a) -> Self {
        Self {
            wire,
            step: Some(Box::new(step)),
            carries_upload: true,
        }
    }
}

impl Transport for After<'_> {
    fn refs(&mut self) -> Result<Answer<'_>, TransportError> {
        self.wire.refs()
    }

    fn fetch(&mut self, request: &mut dyn io::Read) -> Result<Answer<'_>, TransportError> {
        self.wire.fetch(request)
    }

    fn ref_update(&mut self, request: &mut dyn io::Read) -> Result<Answer<'_>, TransportError> {
        let read = |mut body: crate::transport::Body<'_>| {
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes).unwrap();
            bytes
        };
        let (message, bytes) = match self.wire.ref_update(request)? {
            Answer::Message(body) => (true, read(body)),
            Answer::Error(body) => (false, read(body)),
        };
        if let Some(step) = self.step.take() {
            step(self.wire);
        }
        let body: crate::transport::Body<'_> = Box::new(io::Cursor::new(bytes));
        Ok(if message {
            Answer::Message(body)
        } else {
            Answer::Error(body)
        })
    }

    fn challenge(&mut self) -> Result<Answer<'_>, TransportError> {
        self.wire.challenge()
    }

    fn answer_challenge(
        &mut self,
        request: &mut dyn io::Read,
    ) -> Result<crate::transport::SignedIn<'_>, TransportError> {
        self.wire.answer_challenge(request)
    }

    fn upload(&mut self, request: &mut dyn io::Read) -> Result<Answer<'_>, TransportError> {
        if self.carries_upload {
            // A connection that broke is a failure of the carrier, which the transport in
            // memory has no other way to show.
            match self.wire.upload(request) {
                Err(TransportError::NoAnswer { detail }) if detail == BROKEN => Err(
                    TransportError::Carrier(io::Error::new(io::ErrorKind::ConnectionReset, BROKEN)),
                ),
                other => other,
            }
        } else {
            Err(TransportError::Carrier(io::Error::new(
                io::ErrorKind::Unsupported,
                "this transport does not carry an upload",
            )))
        }
    }

    fn anchor(&mut self, request: &mut dyn io::Read) -> Result<Answer<'_>, TransportError> {
        self.wire.anchor(request)
    }
}

/// A hub without a reference, and a clone of it with the remote `origin`.
struct Fixture {
    _dir: TempDir,
    wire: Wire,
    clone: Repository,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new();
        let mut wire = MemoryTransport::new(Hub::new());
        let address = RemoteAddress::try_new("https://hub.example/talent-factory/stemma").unwrap();
        let (clone, _) = Repository::clone_remote(
            dir.path().join("clone"),
            &InitOptions::default(),
            &origin(),
            &RemoteSetup::new(address),
            &mut wire,
            &human("daniel"),
            &AllowAll,
        )
        .unwrap();
        Self {
            _dir: dir,
            wire,
            clone,
        }
    }

    fn hub(&self) -> &Hub {
        self.wire.side()
    }

    /// Stores a commit of `author` with these files and parents in the clone.
    fn commit(&self, author: &str, files: &[(&str, &str)], parents: Vec<CommitId>) -> CommitId {
        let store = self.clone.objects();
        let mut index = Index::new();
        for (path, text) in files {
            let blob = store.put_blob(&Blob::new(*text)).unwrap();
            index.insert(*path, FileMode::File, blob).unwrap();
        }
        let commit = Commit::try_new(
            index.write_tree(store).unwrap(),
            parents,
            ChangeId::generate().unwrap(),
            human(author),
            1,
            "a commit",
        )
        .unwrap();
        store.put_commit(&commit).unwrap()
    }

    /// Sets a local branch of the clone.
    fn set(&self, name: &str, commit: CommitId) {
        let current = self.clone.refs().branch(&branch(name)).unwrap();
        let operation = OperationName::try_new("branch").unwrap();
        self.clone
            .update_branch(
                &operation,
                &branch(name),
                current,
                commit,
                &human("daniel"),
                &AllowAll,
            )
            .unwrap();
    }

    /// The root of `main`: the register, the policy, and one file.
    fn root(&self) -> CommitId {
        let register = register_of().encode();
        self.commit(
            "daniel",
            &[
                (register::FILE, register.as_str()),
                (policy::FILE_NAME, POLICY),
                ("src/lib.rs", "pub fn one() {}\n"),
            ],
            vec![],
        )
    }

    /// The files of the root with `more` beside them.
    fn files_with<'a>(
        &self,
        register: &'a str,
        more: &[(&'a str, &'a str)],
    ) -> Vec<(&'a str, &'a str)> {
        let mut files = vec![
            (register::FILE, register),
            (policy::FILE_NAME, POLICY),
            ("src/lib.rs", "pub fn one() {}\n"),
        ];
        files.extend_from_slice(more);
        files
    }

    fn push_by(
        &mut self,
        signer: &Seeded,
        targets: &[PushTarget],
        piece: u64,
    ) -> Result<Option<Pushed>, PushError> {
        let mut wire = After::new(&mut self.wire, |_: &mut Wire| {});
        self.clone.push_with(
            &origin(),
            &address(),
            targets,
            &mut wire,
            signer,
            &human("daniel"),
            &AllowAll,
            piece,
        )
    }

    fn push(&mut self, reference: &str, commit: CommitId) -> Result<Option<Pushed>, PushError> {
        let target = PushTarget::new(name(reference), Some(commit));
        self.push_by(&daniel(), &[target], PIECE_BYTES)
    }

    fn remote(&self, reference: &str) -> Option<RemoteRef> {
        self.clone
            .refs()
            .remote_ref(&remote_ref_name(&origin(), &name(reference)))
            .unwrap()
    }

    fn local(&self, name: &str) -> Option<CommitId> {
        self.clone.refs().branch(&branch(name)).unwrap()
    }

    /// The entries `push` of the log of the clone.
    fn push_entries(&self) -> Vec<Entry> {
        self.clone
            .oplog()
            .entries()
            .unwrap()
            .into_iter()
            .filter(|entry| {
                entry
                    .change()
                    .is_some_and(|c| c.operation().as_str() == PUSH)
            })
            .collect()
    }
}

/// A line of `count` commits on `base`, the last one first.
fn line(f: &Fixture, base: CommitId, count: usize) -> Vec<CommitId> {
    let register = register_of().encode();
    let mut commits = vec![base];
    for n in 0..count {
        let text = format!("commit {n}\n{}", "x".repeat(4000));
        let path = format!("src/{n}.rs");
        let files = f.files_with(&register, &[(path.as_str(), text.as_str())]);
        let parent = *commits.last().unwrap();
        commits.push(f.commit("daniel", &files, vec![parent]));
    }
    commits.remove(0);
    commits.reverse();
    commits
}

// The push.

#[test]
fn a_first_push_moves_the_remote_reference_after_the_receipt_and_writes_one_entry() {
    let mut f = Fixture::new();
    let root = f.root();

    let pushed = f.push("heads/main", root).unwrap().unwrap();

    // The hub moved, and the clone followed the receipt.
    assert_eq!(f.hub().branch("main"), Some(root));
    let receipt = &pushed.receipt;
    assert_eq!(receipt.number(), 1);
    assert_eq!(
        f.remote("heads/main"),
        Some(RemoteRef {
            commit: root,
            entry: receipt.hash()
        })
    );
    // One entry `push` with the remote reference and the receipt.
    let entry = pushed.entry.unwrap();
    let change = entry.change().unwrap();
    assert_eq!(change.operation().as_str(), PUSH);
    assert!(change.branches().is_empty());
    let [(moved_name, moved)] = change.remotes() else {
        panic!("{:?}", change.remotes());
    };
    assert_eq!(moved_name, &remote_ref_name(&origin(), &name("heads/main")));
    assert_eq!(moved.before(), None);
    assert_eq!(
        change.receipt(),
        Some(&Receipt {
            number: 1,
            hash: receipt.hash()
        })
    );
    // The entry reads back from the log as it was written, and the format knows it.
    assert_eq!(f.clone.oplog().read(entry.number()).unwrap(), entry);
    assert_eq!(
        std::fs::read_to_string(f.clone.metadata_dir().join("format")).unwrap(),
        "stemma-repository 21\n"
    );
    // The upload was one piece, and the anchor of the entry is confirmed and kept.
    assert_eq!(pushed.uploaded.passes, 1);
    assert_eq!(pushed.uploaded.pieces, 1);
    assert_eq!(pushed.uploaded.sent.commits, 1);
    let held = pushed.anchor.unwrap();
    assert!(held.is_confirmed());
    assert_eq!(held.tip().number, entry.number());
    assert_eq!(f.clone.last_confirmation(&origin()).unwrap(), Some(held));
}

#[test]
fn a_push_without_new_objects_sends_no_upload() {
    let mut f = Fixture::new();
    let root = f.root();
    f.push("heads/main", root).unwrap().unwrap();
    let uploads = f.hub().uploads.borrow().len();

    let pushed = f.push("heads/feature", root).unwrap().unwrap();

    assert_eq!(f.hub().uploads.borrow().len(), uploads);
    assert_eq!(pushed.uploaded, Uploaded::default());
    assert_eq!(f.hub().branch("feature"), Some(root));
    assert_eq!(f.remote("heads/feature").unwrap().commit, root);
}

#[test]
fn a_push_of_what_the_remote_references_hold_sends_nothing() {
    let mut f = Fixture::new();
    let root = f.root();
    f.push("heads/main", root).unwrap().unwrap();
    let (updates, entries) = (
        f.hub().updates.get(),
        f.clone.oplog().entries().unwrap().len(),
    );

    assert!(f.push("heads/main", root).unwrap().is_none());

    assert_eq!(f.hub().updates.get(), updates);
    assert_eq!(f.clone.oplog().entries().unwrap().len(), entries);
}

#[test]
fn a_push_uploads_only_what_the_remote_references_do_not_reach_in_pieces_at_commits() {
    let mut f = Fixture::new();
    let root = f.root();
    f.push("heads/main", root).unwrap().unwrap();
    let commits = line(&f, root, 3);
    let before = f.hub().uploads.borrow().len();

    // A bound of one byte makes every commit a piece of its own.
    let target = PushTarget::new(name("heads/work"), Some(commits[0]));
    let pushed = f.push_by(&daniel(), &[target], 1).unwrap().unwrap();

    assert_eq!(pushed.uploaded.pieces, 3);
    assert_eq!(pushed.uploaded.sent.commits, 3);
    assert_eq!(f.hub().uploads.borrow().len(), before + 3);
    // The root is on the hub already: no piece carries it.
    assert_eq!(f.hub().branch("work"), Some(commits[0]));
}

#[test]
fn an_interrupted_upload_goes_on_at_the_first_piece_that_was_not_confirmed() {
    let mut f = Fixture::new();
    let root = f.root();
    f.push("heads/main", root).unwrap().unwrap();
    let commits = line(&f, root, 3);
    let before = f.hub().uploads.borrow().len();
    // The second piece breaks once.
    f.hub().broken.borrow_mut().insert(before + 1);

    let target = PushTarget::new(name("heads/work"), Some(commits[0]));
    let pushed = f.push_by(&daniel(), &[target], 1).unwrap().unwrap();

    let uploads = f.hub().uploads.borrow();
    let sent = &uploads[before..];
    assert_eq!(sent.len(), 4);
    // The first piece went once; the second went again after it broke.
    assert_eq!(sent[1], sent[2]);
    assert_ne!(sent[0], sent[1]);
    assert_eq!(sent.iter().filter(|piece| **piece == sent[0]).count(), 1);
    assert_eq!(pushed.uploaded.pieces, 3);
    assert_eq!(pushed.uploaded.repeated, 1);
    assert_eq!(f.hub().branch("work"), Some(commits[0]));
}

#[test]
fn an_upload_that_keeps_failing_moves_nothing_and_the_next_push_completes() {
    let mut f = Fixture::new();
    let root = f.root();
    f.push("heads/main", root).unwrap().unwrap();
    let commits = line(&f, root, 3);
    let before = f.hub().uploads.borrow().len();
    let entries = f.clone.oplog().entries().unwrap().len();
    f.hub()
        .broken
        .borrow_mut()
        .extend([before + 1, before + 2, before + 3]);
    let target = PushTarget::new(name("heads/work"), Some(commits[0]));

    let error = f
        .push_by(&daniel(), std::slice::from_ref(&target), 1)
        .unwrap_err();

    assert!(
        matches!(error, PushError::Upload { confirmed: 1, .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("1 confirmed pieces"), "{error}");
    assert_eq!(f.hub().updates.get(), 1);
    assert_eq!(f.hub().branch("work"), None);
    assert_eq!(f.remote("heads/work"), None);
    assert_eq!(f.clone.oplog().entries().unwrap().len(), entries);

    // The next push sends the upload again, and it passes.
    f.hub().broken.borrow_mut().clear();
    f.push_by(&daniel(), &[target], 1).unwrap().unwrap();
    assert_eq!(f.hub().branch("work"), Some(commits[0]));
    assert_eq!(f.remote("heads/work").unwrap().commit, commits[0]);
}

#[test]
fn a_refusal_for_want_of_objects_uploads_once_more_and_then_gives_up() {
    let mut f = Fixture::new();
    let root = f.root();
    f.push("heads/main", root).unwrap().unwrap();
    let commits = line(&f, root, 1);
    // The hub confirms the first upload and loses its objects.
    f.hub().lose.set(1);

    let pushed = f.push("heads/work", commits[0]).unwrap().unwrap();

    assert_eq!(pushed.uploaded.passes, 2);
    assert_eq!(f.hub().branch("work"), Some(commits[0]));

    // A hub that loses every upload is refused after the second, and nothing moves.
    let more = line(&f, commits[0], 1);
    f.hub().lose.set(2);
    let error = f.push("heads/work", more[0]).unwrap_err();
    let PushError::Refused(ErrorMessage::Refused(refused)) = &error else {
        panic!("{error:?}");
    };
    assert!(refused.wants_upload());
    assert_eq!(f.hub().branch("work"), Some(commits[0]));
    assert_eq!(f.remote("heads/work").unwrap().commit, commits[0]);
}

#[test]
fn a_refused_push_moves_nothing() {
    let mut f = Fixture::new();
    let root = f.root();

    let target = PushTarget::new(name("heads/main"), Some(root));
    let error = f.push_by(&stranger(), &[target], PIECE_BYTES).unwrap_err();

    assert!(
        matches!(error, PushError::Refused(ErrorMessage::Refused(_))),
        "{error:?}"
    );
    assert!(error.receipt().is_none());
    assert_eq!(f.hub().branch("main"), None);
    assert_eq!(f.remote("heads/main"), None);
    assert!(f.push_entries().is_empty());
}

#[test]
fn a_stale_push_moves_nothing_names_the_state_on_the_hub_and_is_not_repeated() {
    let mut f = Fixture::new();
    let root = f.root();
    f.push("heads/main", root).unwrap().unwrap();
    // Another writer sets `feature` on the hub, which the clone has not fetched.
    let other = line(&f, root, 1)[0];
    let stream = {
        let tips = [other].into_iter().collect();
        let had = [root].into_iter().collect();
        let mut upload = f.clone.upload_of(&tips, &had).unwrap();
        upload.next_piece(PIECE_BYTES).unwrap().unwrap().bytes
    };
    f.hub()
        .repository
        .receive_objects(stream.as_slice(), Bounds::default())
        .unwrap();
    let moved = f.hub().move_of("heads/feature", other, None);
    let update = RefUpdatePayload::try_new(address(), repository_id(), vec![moved], mia().public())
        .unwrap()
        .sign(&mia())
        .unwrap();
    let theirs = f.hub().accept(&update).unwrap();
    let updates = f.hub().updates.get();

    let error = f.push("heads/feature", root).unwrap_err();

    let PushError::Stale(stale) = &error else {
        panic!("{error:?}");
    };
    assert_eq!(
        stale.references(),
        [StaleReference {
            name: name("heads/feature"),
            entry: Some(theirs.hash()),
            commit: Some(other),
        }]
    );
    // One update, and nothing moved on either side.
    assert_eq!(f.hub().updates.get(), updates + 1);
    assert_eq!(f.hub().branch("feature"), Some(other));
    assert_eq!(f.remote("heads/feature"), None);
}

#[test]
fn a_receipt_of_another_key_moves_nothing_in_the_clone() {
    let mut f = Fixture::new();
    let root = f.root();
    f.hub().rogue.set(true);

    let error = f.push("heads/main", root).unwrap_err();

    assert!(
        matches!(error, PushError::Receipt(ReceiptError::OtherKey)),
        "{error:?}"
    );
    assert_eq!(f.remote("heads/main"), None);
    assert!(f.push_entries().is_empty());
}

#[test]
fn a_push_is_refused_without_a_remote_or_by_the_authorizer() {
    let mut f = Fixture::new();
    let root = f.root();
    let target = PushTarget::new(name("heads/main"), Some(root));
    let other = RemoteName::try_new("other").unwrap();
    let error = f
        .clone
        .push(
            &other,
            &address(),
            std::slice::from_ref(&target),
            &mut f.wire,
            &daniel(),
            &human("daniel"),
            &AllowAll,
        )
        .unwrap_err();
    assert!(matches!(error, PushError::NoSuchRemote(_)), "{error:?}");

    struct DenyPush;
    impl Authorizer for DenyPush {
        fn authorize(&self, _: &Actor, operation: &OperationName, _: &str) -> crate::Decision {
            if operation.as_str() == PUSH {
                crate::Decision::Deny
            } else {
                crate::Decision::Allow
            }
        }
    }
    let error = f
        .clone
        .push(
            &origin(),
            &address(),
            &[target.clone(), target],
            &mut f.wire,
            &daniel(),
            &human("daniel"),
            &DenyPush,
        )
        .unwrap_err();
    assert!(
        matches!(error, PushError::Update(UpdateError::Denied { .. })),
        "{error:?}"
    );
    assert_eq!(f.hub().updates.get(), 0);
}

// The entry `push`.

#[test]
fn the_entry_push_names_a_receipt_and_no_other_entry_names_one() {
    let receipt = Receipt {
        number: 3,
        hash: "b3:0000000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap(),
    };
    let commit = CommitId::new(
        "b3:1111111111111111111111111111111111111111111111111111111111111111"
            .parse()
            .unwrap(),
    );
    let remote = vec![(
        remote_ref_name(&origin(), &name("heads/main")),
        Moved::from_to(
            None,
            Some(RemoteRef {
                commit,
                entry: receipt.hash,
            }),
        ),
    )];
    // A push names a pointer that it moved.
    assert_eq!(
        Change::try_new_push(1, human("daniel"), vec![], vec![], receipt),
        Err(crate::oplog::EntryError::NoChange)
    );
    let zero = Receipt {
        number: 0,
        ..receipt
    };
    assert_eq!(
        Change::try_new_push(1, human("daniel"), vec![], remote.clone(), zero),
        Err(crate::oplog::EntryError::PushReceipt)
    );
    // A change of the operation `push` without a receipt is refused.
    let operation = OperationName::try_new(PUSH).unwrap();
    assert_eq!(
        Change::try_new_with_remotes(1, human("daniel"), operation, remote.clone()),
        Err(crate::oplog::EntryError::PushReceipt)
    );
    let change = Change::try_new_push(1, human("daniel"), vec![], remote, receipt).unwrap();
    assert_eq!(change.receipt(), Some(&receipt));
}

#[test]
fn the_reader_refuses_a_receipt_out_of_place() {
    let mut f = Fixture::new();
    let root = f.root();
    let entry = f.push("heads/main", root).unwrap().unwrap().entry.unwrap();
    let path = f
        .clone
        .metadata_dir()
        .join("oplog")
        .join(format!("{:012}", entry.number()));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.ends_with(&format!(
        "receipt 1 {}\n",
        entry.change().unwrap().receipt().unwrap().hash
    )));
    crate::oplog::Entry::parse(text.as_bytes()).unwrap();
    let receipt_line = text.lines().last().unwrap().to_owned();
    for broken in [
        // A line after the receipt.
        format!(
            "{text}receipt 1 {}\n",
            receipt_line.split(' ').nth(2).unwrap()
        ),
        // No receipt.
        text.replace(&format!("{receipt_line}\n"), ""),
        // A receipt with 0 and one that is no number.
        text.replace("receipt 1 ", "receipt 0 "),
        text.replace("receipt 1 ", "receipt 01 "),
        text.replace("receipt 1 ", "receipt one "),
        // A receipt of another operation.
        text.replace("operation push", "operation fetch"),
    ] {
        assert!(
            crate::oplog::Entry::parse(broken.as_bytes()).is_err(),
            "{broken}"
        );
    }
    assert!(matches!(
        f.clone.oplog().read(entry.number()).unwrap().record(),
        Record::Change(_)
    ));
}

// The merge with a remote.

/// A clone whose `main` and `feature` are on the hub, with a pull request of Mia open on
/// `feature`, checked by Ana, in the clone and on the hub alike.
struct Merging {
    f: Fixture,
    root: CommitId,
    head: CommitId,
    number: u64,
    opening: StatementHash,
}

impl Merging {
    fn new() -> Self {
        let mut f = Fixture::new();
        let register = register_of().encode();
        let root = f.root();
        let files = f.files_with(&register, &[("docs/a.md", "a\n")]);
        let head = f.commit("mia", &files, vec![root]);
        f.set("main", root);
        f.set("feature", head);
        f.push("heads/main", root).unwrap().unwrap();
        let target = PushTarget::new(name("heads/feature"), Some(head));
        f.push_by(&mia(), &[target], PIECE_BYTES).unwrap().unwrap();

        let opening =
            Opening::try_new(Some(repository_id()), branch("feature"), head, vec![], None).unwrap();
        let statement = Statement::sign(Body::Opening(opening), &mia()).unwrap();
        let entry =
            PullEntry::open(now(), human("mia"), branch("feature"), head, vec![], None).unwrap();
        let number = by_hand::pull_naming(&f.clone, &human("mia"), entry, &statement).number();
        f.hub().submit(&statement);
        let checked =
            Checked::try_new(Some(repository_id()), head, Some(root), CheckResult::Pass).unwrap();
        let check = Statement::sign(Body::Check(checked), &ana()).unwrap();
        by_hand::check_naming(
            &f.clone,
            &human("ana"),
            now(),
            (head, Some(root), CheckResult::Pass),
            &check,
        );
        f.hub().submit(&check);
        Self {
            f,
            root,
            head,
            number,
            opening: statement.hash(),
        }
    }

    /// The approval of `bot` in the clone, and on the hub if `on_hub`.
    fn approve(&self, on_hub: bool) {
        let approval = Approval::new(Some(repository_id()), self.opening, self.head);
        let statement = Statement::sign(Body::Approval(approval), &bot()).unwrap();
        let entry = PullEntry::approve(now(), bot_actor(), self.number, self.head).unwrap();
        by_hand::pull_naming(&self.f.clone, &bot_actor(), entry, &statement);
        if on_hub {
            self.f.hub().submit(&statement);
        }
    }

    fn merge(&mut self) -> Result<RemoteMerge, RemoteMergeError> {
        let mode = Mode::Enforcing(register_of());
        self.f.clone.merge_gate_remote(
            self.number,
            &mode,
            &origin(),
            &address(),
            &mut self.f.wire,
            &daniel(),
            &human("daniel"),
            &AllowAll,
        )
    }

    fn pr_merges(&self) -> usize {
        self.f
            .clone
            .oplog()
            .entries()
            .unwrap()
            .iter()
            .filter(|entry| matches!(entry.record(), Record::Pull(pull) if pull.kind() == crate::oplog::PullKind::Merge))
            .count()
    }
}

#[test]
fn a_merge_that_the_hub_accepts_moves_main_only_after_the_receipt() {
    let mut m = Merging::new();
    m.approve(true);

    let merged = m.merge().unwrap();

    assert_eq!(merged.attempts, 1);
    assert!(!merged.repaired);
    assert_eq!(merged.tip, m.root);
    // The hub accepted `M` with the opening as evidence, and the clone followed the receipt.
    let receipt = merged.receipt.unwrap();
    let [moved] = receipt.update().payload().moves() else {
        panic!("one move");
    };
    assert_eq!(moved.evidence(), Some(m.opening));
    assert_eq!(m.f.hub().branch("main"), Some(merged.commit));
    assert_eq!(m.f.local("main"), Some(merged.commit));
    assert_eq!(
        m.f.remote("heads/main"),
        Some(RemoteRef {
            commit: merged.commit,
            entry: receipt.hash()
        })
    );
    // One entry `push` moved both, then `pr-merge`.
    let pushed = merged.pushed.unwrap();
    let change = pushed.change().unwrap();
    assert_eq!(change.operation().as_str(), PUSH);
    assert_eq!(
        change.branches(),
        [(
            branch("main"),
            Moved::from_to(Some(m.root), Some(merged.commit))
        )]
    );
    assert_eq!(change.remotes().len(), 1);
    assert_eq!(change.receipt().unwrap().number, receipt.number());
    assert!(merged.merged.number() > pushed.number());
    assert_eq!(m.pr_merges(), 1);
    assert!(merged.anchor.unwrap().unwrap().is_confirmed());
    // `M` has the single parent `T`.
    let commit =
        m.f.clone
            .objects()
            .get_commit(&merged.commit)
            .unwrap()
            .unwrap();
    assert_eq!(commit.parents(), [m.root]);
}

#[test]
fn a_merge_that_the_hub_refuses_leaves_main_and_the_pull_request_as_they_were() {
    let mut m = Merging::new();
    // The clone counts the approval of `bot`; the hub does not hold it.
    m.approve(false);
    let pushes = m.f.push_entries().len();

    let error = m.merge().unwrap_err();

    let RemoteMergeError::Push(push) = &error else {
        panic!("{error:?}");
    };
    let PushError::Refused(ErrorMessage::Refused(Refused::References(refused))) = &**push else {
        panic!("{push:?}");
    };
    assert!(matches!(
        refused.references()[0].reason(),
        RefusalReason::Verdict(GateRefusal::NotApproved { .. })
    ));
    assert_eq!(m.f.hub().branch("main"), Some(m.root));
    assert_eq!(m.f.local("main"), Some(m.root));
    assert_eq!(m.f.remote("heads/main").unwrap().commit, m.root);
    assert_eq!(m.f.push_entries().len(), pushes);
    assert_eq!(m.pr_merges(), 0);
    // The pull request is still open: once the hub holds the approval, the merge lands.
    let approval = Approval::new(Some(repository_id()), m.opening, m.head);
    m.f.hub()
        .submit(&Statement::sign(Body::Approval(approval), &bot()).unwrap());
    let merged = m.merge().unwrap();
    assert_eq!(m.f.local("main"), Some(merged.commit));
}

#[test]
fn a_local_refusal_sends_nothing() {
    let mut m = Merging::new();
    // Without an approval in the clone the local verdict refuses.
    let updates = m.f.hub().updates.get();

    let error = m.merge().unwrap_err();

    assert!(
        matches!(error, RemoteMergeError::Gate(GateError::NotApproved { .. })),
        "{error:?}"
    );
    assert_eq!(m.f.hub().updates.get(), updates);
    assert_eq!(m.f.local("main"), Some(m.root));
}

#[test]
fn a_merge_that_loses_to_another_writer_repeats_on_the_new_tip() {
    let mut m = Merging::new();
    m.approve(true);
    // Another pull request, of Ana on `other`, lands on the hub between the fetch and the update
    // of the first attempt.
    let register = register_of().encode();
    let files = m.f.files_with(&register, &[("docs/b.md", "b\n")]);
    let theirs = m.f.commit("ana", &files, vec![m.root]);
    let target = PushTarget::new(name("heads/other"), Some(theirs));
    m.f.push_by(&ana(), &[target], PIECE_BYTES)
        .unwrap()
        .unwrap();
    let opening =
        Opening::try_new(Some(repository_id()), branch("other"), theirs, vec![], None).unwrap();
    let opening = Statement::sign(Body::Opening(opening), &ana()).unwrap();
    m.f.hub().submit(&opening);
    let checked = Checked::try_new(
        Some(repository_id()),
        theirs,
        Some(m.root),
        CheckResult::Pass,
    )
    .unwrap();
    m.f.hub()
        .submit(&Statement::sign(Body::Check(checked), &mia()).unwrap());
    let approval = Approval::new(Some(repository_id()), opening.hash(), theirs);
    m.f.hub()
        .submit(&Statement::sign(Body::Approval(approval), &cat()).unwrap());
    let root = m.root;
    let hash = opening.hash();
    *m.f.hub().before_update.borrow_mut() = Some(Box::new(move |hub: &Hub| {
        let store = hub.repository.objects();
        let tree = store.get_commit(&theirs).unwrap().unwrap().tree();
        let commit = Commit::try_new(
            tree,
            vec![root],
            ChangeId::generate().unwrap(),
            human("daniel"),
            2,
            "Merge pull request",
        )
        .unwrap();
        let landed = store.put_commit(&commit).unwrap();
        let moved = hub.move_of("heads/main", landed, Some(hash));
        let update =
            RefUpdatePayload::try_new(address(), repository_id(), vec![moved], daniel().public())
                .unwrap()
                .sign(&daniel())
                .unwrap();
        hub.accept(&update).unwrap();
    }));
    let updates = m.f.hub().updates.get();

    let merged = m.merge().unwrap();

    assert_eq!(merged.attempts, 2);
    assert_eq!(m.f.hub().updates.get(), updates + 2);
    // The second attempt merged onto the tip of the other writer, and signed again.
    let tip = m.f.hub().entries()[m.f.hub().entries().len() - 2]
        .update()
        .payload()
        .moves()[0]
        .new_commit()
        .unwrap();
    assert_eq!(merged.tip, tip);
    assert_eq!(m.f.hub().branch("main"), Some(merged.commit));
    assert_eq!(m.f.local("main"), Some(merged.commit));
    let commit =
        m.f.clone
            .objects()
            .get_commit(&merged.commit)
            .unwrap()
            .unwrap();
    assert_eq!(commit.parents(), [tip]);
    assert_eq!(m.pr_merges(), 1);
}

#[test]
fn the_loop_ends_after_its_bound_when_main_never_stands_still() {
    let mut m = Merging::new();
    m.approve(true);
    m.f.hub().always_stale.set(true);
    let updates = m.f.hub().updates.get();

    let error = m.merge().unwrap_err();

    assert!(
        matches!(error, RemoteMergeError::Contended { attempts, .. } if attempts == MAX),
        "{error:?}"
    );
    assert_eq!(m.f.hub().updates.get(), updates + MAX);
    assert_eq!(m.f.local("main"), Some(m.root));
    assert_eq!(m.f.remote("heads/main").unwrap().commit, m.root);
    assert_eq!(m.pr_merges(), 0);
}

const MAX: u32 = crate::repository::MAX_ATTEMPTS;

#[test]
fn a_merge_whose_receipt_was_not_recorded_is_recorded_by_the_next_call() {
    let mut m = Merging::new();
    m.approve(true);
    // The hub accepted the merge, and the clone stopped before its entries: the update is sent
    // past the clone, as a call that stopped after the receipt leaves it.
    let tree =
        m.f.clone
            .objects()
            .get_commit(&m.head)
            .unwrap()
            .unwrap()
            .tree();
    let commit = Commit::try_new(
        tree,
        vec![m.root],
        ChangeId::generate().unwrap(),
        human("daniel"),
        2,
        "Merge pull request",
    )
    .unwrap();
    let landed = m.f.hub().repository.objects().put_commit(&commit).unwrap();
    let moved = m.f.hub().move_of("heads/main", landed, Some(m.opening));
    let update =
        RefUpdatePayload::try_new(address(), repository_id(), vec![moved], daniel().public())
            .unwrap()
            .sign(&daniel())
            .unwrap();
    m.f.hub().accept(&update).unwrap();
    let updates = m.f.hub().updates.get();

    let merged = m.merge().unwrap();

    assert!(merged.repaired);
    assert_eq!(m.f.hub().updates.get(), updates);
    assert_eq!(m.f.remote("heads/main").unwrap().commit, landed);
    // The local `main` followed the hub with an entry of the gate, and no anchor was sent.
    assert_eq!(m.f.local("main"), Some(landed));
    let moved = merged.pushed.unwrap();
    assert_eq!(
        moved.change().unwrap().branches(),
        [(branch("main"), Moved::from_to(Some(m.root), Some(landed)))]
    );
    assert!(merged.anchor.is_none());
    assert_eq!(m.pr_merges(), 1);
}

#[test]
fn a_local_main_beside_the_hub_is_refused_before_anything_is_sent() {
    let mut m = Merging::new();
    m.approve(true);
    let register = register_of().encode();
    let files = m.f.files_with(&register, &[("src/local.rs", "local\n")]);
    let local = m.f.commit("daniel", &files, vec![m.root]);
    m.f.set("main", local);
    let updates = m.f.hub().updates.get();

    let error = m.merge().unwrap_err();

    assert!(
        matches!(error, RemoteMergeError::Diverged { local: l, tip } if l == local && tip == m.root),
        "{error:?}"
    );
    assert_eq!(m.f.hub().updates.get(), updates);
    assert_eq!(m.f.local("main"), Some(local));
}

#[test]
fn a_remote_reference_that_a_fetch_moved_after_the_receipt_keeps_its_state() {
    let mut f = Fixture::new();
    let root = f.root();
    f.push("heads/main", root).unwrap().unwrap();
    let commits = line(&f, root, 1);
    let clone = &f.clone;
    let mut after = After::new(&mut f.wire, |wire: &mut Wire| {
        clone
            .fetch(&origin(), wire, &human("daniel"), &AllowAll)
            .unwrap();
    });

    let target = PushTarget::new(name("heads/work"), Some(commits[0]));
    let pushed = clone
        .push_with(
            &origin(),
            &address(),
            &[target],
            &mut after,
            &daniel(),
            &human("daniel"),
            &AllowAll,
            PIECE_BYTES,
        )
        .unwrap()
        .unwrap();
    drop(after);

    // The fetch brought the state of the receipt already: no entry `push` moves it again.
    assert!(pushed.entry.is_none());
    assert_eq!(
        f.remote("heads/work"),
        Some(RemoteRef {
            commit: commits[0],
            entry: pushed.receipt.hash()
        })
    );
}

#[test]
fn a_merge_moves_the_local_main_even_when_a_fetch_moved_its_remote_reference() {
    let mut m = Merging::new();
    m.approve(true);
    let mode = Mode::Enforcing(register_of());
    let clone = &m.f.clone;
    let mut after = After::new(&mut m.f.wire, |wire: &mut Wire| {
        clone
            .fetch(&origin(), wire, &human("daniel"), &AllowAll)
            .unwrap();
    });

    let merged = clone
        .merge_gate_remote(
            m.number,
            &mode,
            &origin(),
            &address(),
            &mut after,
            &daniel(),
            &human("daniel"),
            &AllowAll,
        )
        .unwrap();
    drop(after);

    assert_eq!(m.f.local("main"), Some(merged.commit));
    assert_eq!(m.f.remote("heads/main").unwrap().commit, merged.commit);
    let pushed = merged.pushed.unwrap();
    let change = pushed.change().unwrap();
    assert!(change.remotes().is_empty());
    assert_eq!(
        change.branches(),
        [(
            branch("main"),
            Moved::from_to(Some(m.root), Some(merged.commit))
        )]
    );
    assert_eq!(m.pr_merges(), 1);
}

#[test]
fn a_push_that_the_clone_cannot_record_carries_the_receipt() {
    let mut f = Fixture::new();
    let root = f.root();
    let config = f.clone.metadata_dir().join("remotes").join("origin");
    // The remote is changed while the hub accepts: as a `remote pin` beside the push does.
    let mut after = After::new(&mut f.wire, |_: &mut Wire| {
        let text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(&config, text.replace("sign-in no", "sign-in yes")).unwrap();
    });

    let target = PushTarget::new(name("heads/main"), Some(root));
    let error = f
        .clone
        .push_with(
            &origin(),
            &address(),
            &[target],
            &mut after,
            &daniel(),
            &human("daniel"),
            &AllowAll,
            PIECE_BYTES,
        )
        .unwrap_err();
    drop(after);

    let PushError::Unrecorded { receipt, source } = &error else {
        panic!("{error:?}");
    };
    assert!(
        matches!(**source, PushError::RemoteChanged(_)),
        "{source:?}"
    );
    assert_eq!(
        error.receipt().map(RecordEntry::number),
        Some(receipt.number())
    );
    assert_eq!(f.hub().branch("main"), Some(root));
    assert_eq!(f.remote("heads/main"), None);
    assert!(f.push_entries().is_empty());
    // A fetch brings the remote reference along.
    f.clone
        .fetch(&origin(), &mut f.wire, &human("daniel"), &AllowAll)
        .unwrap();
    assert_eq!(f.remote("heads/main").unwrap().commit, root);
}

#[test]
fn a_transport_without_an_upload_fails_at_once_and_an_anchor_failure_leaves_the_push() {
    let mut f = Fixture::new();
    let root = f.root();
    let mut after = After::new(&mut f.wire, |_: &mut Wire| {});
    after.carries_upload = false;
    let target = PushTarget::new(name("heads/main"), Some(root));
    let error = f
        .clone
        .push_with(
            &origin(),
            &address(),
            std::slice::from_ref(&target),
            &mut after,
            &daniel(),
            &human("daniel"),
            &AllowAll,
            PIECE_BYTES,
        )
        .unwrap_err();
    drop(after);
    // One attempt, and no advice to push again.
    assert!(
        matches!(error, PushError::Upload { confirmed: 0, .. }),
        "{error:?}"
    );
    assert!(!error.to_string().contains("push again"), "{error}");
    assert_eq!(f.hub().updates.get(), 0);

    f.hub().no_anchors.set(true);
    let pushed = f.push("heads/main", root).unwrap().unwrap();
    assert!(pushed.entry.is_some());
    assert!(
        matches!(pushed.anchor, Err(AnchorFailure::Transport(_))),
        "{:?}",
        pushed.anchor
    );
    assert_eq!(f.remote("heads/main").unwrap().commit, root);
}

#[test]
fn a_push_deletes_a_reference_on_the_hub_and_its_remote_reference() {
    let mut f = Fixture::new();
    let root = f.root();
    f.push("heads/main", root).unwrap().unwrap();
    f.push("heads/feature", root).unwrap().unwrap();

    let target = PushTarget::new(name("heads/feature"), None);
    let pushed = f
        .push_by(&daniel(), &[target], PIECE_BYTES)
        .unwrap()
        .unwrap();

    assert_eq!(f.hub().branch("feature"), None);
    assert_eq!(f.remote("heads/feature"), None);
    let change = pushed.entry.unwrap();
    let [(_, moved)] = change.change().unwrap().remotes() else {
        panic!("one remote reference");
    };
    assert_eq!(moved.after(), None);
}
