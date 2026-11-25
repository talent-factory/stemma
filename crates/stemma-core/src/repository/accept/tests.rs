//! The tests of the accepting operation (ADR-025). A clone reaches the hub through
//! [`MemoryTransport`]: it reads the list of references, signs a ref update, sends it, and reads
//! the receipt or the error. The other side is [`Hub`], a bare repository with its record in
//! memory, which answers a ref update with [`Repository::accept_ref_update`].

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::{self, Write};

use super::*;
use crate::change::ChangeId;
use crate::failure::{Classify, FailureKind};
use crate::fault;
use crate::index::Index;
use crate::object::{Blob, Commit, FileMode};
use crate::oplog::OperationName;
use crate::policy;
use crate::protocol::record::tests::Seeded;
use crate::protocol::record::{RecordProblem, RefUpdatePayload, StaleReference};
use crate::protocol::{FetchRequest, ParseError, RecordHash, RefList, Reference};
use crate::register::{self, Role};
use crate::signature::{SignError, Signature};
use crate::store::ObjectStore;
use crate::testutil::TempDir;
use crate::transport::{Answer, MemoryTransport, OtherSide, Refusal, Transport, TransportError};
use crate::{Actor, Decision};

const ADDRESS: &str = "https://hub.example";
const REPOSITORY: &str = "00112233445566778899aabbccddeeff";

/// The lease that the policy of these tests names, in seconds.
const LEASE: i64 = 30 * 60;

/// The policy of these tests.
const POLICY: &str = "stemma-policy 1\nlease 30\n";

/// The time of the hub at the start of a test.
const START: i64 = 1_000_000;

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

fn human(id: &str) -> Identity {
    Identity::human(id).unwrap()
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

/// The register of these tests: Daniel owns it, Mia and Ana are members.
fn register_text() -> String {
    let mut register = Register::with_owner(human("daniel"), daniel().public()).unwrap();
    register
        .add_human(human("mia"), Role::Member, mia().public())
        .unwrap();
    register
        .add_human(human("ana"), Role::Member, ana().public())
        .unwrap();
    register.encode()
}

/// The record of a hub in memory, which can be told to fail.
#[derive(Default)]
struct MemoryRecord {
    bytes: Vec<u8>,
    fail_read: bool,
    fail_append: bool,
}

impl RecordDestination for MemoryRecord {
    fn read(&mut self) -> io::Result<Vec<u8>> {
        if self.fail_read {
            return Err(io::Error::other("the record cannot be read"));
        }
        Ok(self.bytes.clone())
    }

    fn append(&mut self, entry: &[u8]) -> io::Result<()> {
        if self.fail_append {
            return Err(io::Error::other("the disk is full"));
        }
        self.bytes.extend_from_slice(entry);
        Ok(())
    }
}

/// A signer that fails.
struct Broken;

impl Signer for Broken {
    fn sign(&self, _: &[u8]) -> Result<Signature, SignError> {
        Err(SignError::Unreadable {
            reason: "broken".into(),
        })
    }
}

/// Refuses one operation and allows every other.
struct Deny(&'static str);

impl Authorizer for Deny {
    fn authorize(&self, _actor: &Actor, operation: &OperationName, _target: &str) -> Decision {
        if operation.to_string() == self.0 {
            Decision::Deny
        } else {
            Decision::Allow
        }
    }
}

/// The other side of these tests: a bare repository, its record, and the clock of the hub.
struct Hub {
    _dir: TempDir,
    repository: Repository,
    record: RefCell<MemoryRecord>,
    time: Cell<i64>,
}

impl Hub {
    fn new() -> Self {
        let dir = TempDir::new();
        let owner = Actor::human("operator").unwrap();
        let repository = Repository::init_bare(dir.path().join("hub"), &owner, &AllowAll).unwrap();
        Self {
            _dir: dir,
            repository,
            record: RefCell::default(),
            time: Cell::new(START),
        }
    }

    /// Calls the accepting operation as the hub does, with one more authorizer.
    fn accept_with(
        &self,
        update: &RefUpdate,
        also: &dyn Authorizer,
    ) -> Result<RecordEntry, AcceptError> {
        let hub = address();
        let signer = hub_key();
        let mut record = self.record.borrow_mut();
        let context = AcceptContext::new(
            &hub,
            repository_id(),
            &mut *record,
            self.time.get(),
            &signer,
        );
        self.repository
            .accept_ref_update_with(update, context, also)
    }

    fn accept(&self, update: &RefUpdate) -> Result<RecordEntry, AcceptError> {
        self.accept_with(update, &AllowAll)
    }

    /// The entries of the record, read and checked as a reader of the record does.
    fn entries(&self) -> Vec<RecordEntry> {
        read_record(&self.record.borrow().bytes).unwrap()
    }

    fn chain(&self) -> Chain {
        let mut chain = Chain::new(repository_id());
        for entry in self.entries() {
            chain.accept(&entry).unwrap();
        }
        chain
    }

    /// The commit of a reference in the store of the hub.
    fn reference(&self, reference: &str) -> Option<CommitId> {
        let name = name(reference);
        let refs = self.repository.refs();
        match name.kind() {
            RefKind::Branch => refs.branch(name.name()).unwrap(),
            RefKind::Tag => refs.tag(name.name()).unwrap(),
        }
    }

    /// Stores a commit with these files and parents, as an upload does.
    fn commit(&self, files: &[(&str, &str)], parents: Vec<CommitId>) -> CommitId {
        let store = self.repository.objects();
        let mut index = Index::new();
        for (path, text) in files {
            let blob = store.put_blob(&Blob::new(*text)).unwrap();
            index.insert(*path, FileMode::File, blob).unwrap();
        }
        let commit = Commit::try_new(
            index.write_tree(store).unwrap(),
            parents,
            ChangeId::generate().unwrap(),
            Actor::human("daniel").unwrap(),
            1,
            "a commit",
        )
        .unwrap();
        store.put_commit(&commit).unwrap()
    }

    /// A commit that holds the register and the policy of these tests, and one more file.
    fn governed(&self, text: &str, parents: Vec<CommitId>) -> CommitId {
        self.governed_by(&register_text(), text, parents)
    }

    /// A commit that holds this register and the policy of these tests, and one more file.
    fn governed_by(&self, register: &str, text: &str, parents: Vec<CommitId>) -> CommitId {
        self.commit(
            &[
                (register::FILE, register),
                (policy::FILE_NAME, POLICY),
                ("a.txt", text),
            ],
            parents,
        )
    }
}

impl OtherSide for Hub {
    fn refs(&self) -> Result<RefList, Refusal> {
        let chain = self.chain();
        let refs = self.repository.refs();
        let branches = refs
            .branches()
            .unwrap()
            .into_iter()
            .map(|reference| (RefKind::Branch, reference));
        let tags = refs
            .tags()
            .unwrap()
            .into_iter()
            .map(|reference| (RefKind::Tag, reference));
        let references = branches
            .chain(tags)
            .map(|(kind, (reference, commit))| {
                let name = QualifiedName::new(kind, reference);
                let entry = chain.entry_of(&name).unwrap();
                Reference::new(name, commit, entry)
            })
            .collect();
        Ok(RefList::try_new(repository_id(), hub_key().public(), Vec::new(), references).unwrap())
    }

    fn fetch(&self, _request: &FetchRequest, _stream: &mut dyn Write) -> Result<(), Refusal> {
        Err(Refusal::Failed("these tests fetch nothing".to_owned()))
    }

    fn ref_update(&self, update: &RefUpdate) -> Result<RecordEntry, Refusal> {
        self.accept(update).map_err(|error| match error.answer() {
            Some(message) => Refusal::Error(message),
            None => Refusal::Failed(error.to_string()),
        })
    }
}

type Wire = MemoryTransport<Hub>;

fn wire() -> Wire {
    MemoryTransport::new(Hub::new())
}

fn read_all(mut body: crate::transport::Body<'_>) -> Vec<u8> {
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).unwrap();
    bytes
}

/// The state of a reference in the list of references of the hub: the hash of its entry and its
/// commit, or nothing for a reference that does not exist.
fn listed(wire: &mut Wire, reference: &str) -> (Option<RecordHash>, Option<CommitId>) {
    let Answer::Message(body) = wire.refs().unwrap() else {
        panic!("the list of references was refused");
    };
    let list = RefList::parse(&read_all(body)).unwrap();
    list.references()
        .iter()
        .find(|listed| listed.name() == &name(reference))
        .map_or((None, None), |listed| {
            (Some(listed.entry()), Some(listed.commit()))
        })
}

/// A move of a reference from its state in the list of references of the hub.
fn to(wire: &mut Wire, reference: &str, new: Option<CommitId>) -> Move {
    let (previous, old) = listed(wire, reference);
    Move::try_new(name(reference), previous, old, new, None).unwrap()
}

/// A move of a reference that does not exist, whose name the record listed last in `previous`.
fn again(reference: &str, previous: &RecordEntry, new: CommitId) -> Move {
    Move::try_new(
        name(reference),
        Some(previous.hash()),
        None,
        Some(new),
        None,
    )
    .unwrap()
}

fn signed_for(signer: &Seeded, hub: &str, repository: &str, moves: Vec<Move>) -> RefUpdate {
    RefUpdatePayload::try_new(
        HubAddress::try_new(hub).unwrap(),
        repository.parse().unwrap(),
        moves,
        signer.public(),
    )
    .unwrap()
    .sign(signer)
    .unwrap()
}

fn signed(signer: &Seeded, moves: Vec<Move>) -> RefUpdate {
    signed_for(signer, ADDRESS, REPOSITORY, moves)
}

/// Sends an update as a clone does: the receipt, checked against what was sent and the key of
/// the hub, or the error that the hub answered.
fn send(wire: &mut Wire, update: &RefUpdate) -> Result<RecordEntry, ErrorMessage> {
    match wire.ref_update(&mut update.encode().as_slice()).unwrap() {
        Answer::Message(body) => {
            let receipt = RecordEntry::parse(&read_all(body)).unwrap();
            receipt.check_receipt(update, &hub_key().public()).unwrap();
            Ok(receipt)
        }
        Answer::Error(body) => Err(ErrorMessage::parse(&read_all(body)).unwrap()),
    }
}

fn push(wire: &mut Wire, signer: &Seeded, moves: Vec<Move>) -> Result<RecordEntry, ErrorMessage> {
    send(wire, &signed(signer, moves))
}

/// The refused references of an answer, each with its reason.
fn refused(answer: Result<RecordEntry, ErrorMessage>) -> Vec<(String, RefusalReason)> {
    match answer {
        Err(ErrorMessage::Refused(Refused::References(refused))) => refused
            .references()
            .iter()
            .map(|refused| (refused.name().to_string(), refused.reason().clone()))
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// The first push of Daniel: `main` at a commit with the register and the policy.
fn first_push(wire: &mut Wire) -> (CommitId, RecordEntry) {
    let root = wire.side().governed("one\n", vec![]);
    let moved = to(wire, "heads/main", Some(root));
    let receipt = push(wire, &daniel(), vec![moved]).unwrap();
    (root, receipt)
}

/// A hub after the first push, with the tip of `main`.
fn started() -> (Wire, CommitId) {
    let mut wire = wire();
    let (root, _) = first_push(&mut wire);
    (wire, root)
}

/// Checks that an attempt left the record and the references as they were.
fn unchanged(wire: &Wire, record: &[u8], references: &[(&str, Option<CommitId>)]) {
    assert_eq!(wire.side().record.borrow().bytes, record);
    for (reference, commit) in references {
        assert_eq!(wire.side().reference(reference), *commit, "{reference}");
    }
}

fn record_of(wire: &Wire) -> Vec<u8> {
    wire.side().record.borrow().bytes.clone()
}

#[test]
fn the_first_push_sets_main_by_the_register_that_it_brings() {
    let mut wire = wire();
    let (root, receipt) = first_push(&mut wire);

    assert_eq!(receipt.number(), 1);
    assert_eq!(receipt.time(), START);
    assert_eq!(receipt.previous(), None);
    // The register of the first push is the one of the pushed commit (ADR-024).
    assert_eq!(receipt.register(), root);
    assert_eq!(receipt.policies(), []);
    assert_eq!(receipt.actor(), &human("daniel"));
    assert_eq!(receipt.is_fast_forward(&name("heads/main")), Some(false));
    assert_eq!(wire.side().reference("heads/main"), Some(root));
    // The record holds the receipt, and the list of references names it.
    assert_eq!(wire.side().entries(), std::slice::from_ref(&receipt));
    assert_eq!(
        listed(&mut wire, "heads/main"),
        (Some(receipt.hash()), Some(root))
    );
    // A reader of the record finds the actor in the register that the entry names.
    let keys = crate::protocol::record::HubKeys::try_new(hub_key().public(), Vec::new()).unwrap();
    let unchecked = Chain::new(repository_id())
        .verify(
            &wire.side().entries(),
            &keys,
            wire.side().repository.objects(),
        )
        .unwrap();
    assert_eq!(unchecked, []);
}

#[test]
fn the_first_push_may_bring_further_references() {
    let mut wire = wire();
    let root = wire.side().governed("one\n", vec![]);
    let topic = wire.side().commit(&[("b.txt", "two\n")], vec![root]);
    let moves = vec![
        to(&mut wire, "heads/main", Some(root)),
        to(&mut wire, "heads/topic", Some(topic)),
        to(&mut wire, "tags/v1", Some(root)),
    ];
    let receipt = push(&mut wire, &daniel(), moves).unwrap();
    assert_eq!(receipt.register(), root);
    assert_eq!(wire.side().reference("heads/topic"), Some(topic));
    assert_eq!(wire.side().reference("tags/v1"), Some(root));
}

#[test]
fn the_first_push_is_refused_unless_its_register_holds_the_key_as_an_owner() {
    let mut wire = wire();
    let root = wire.side().governed("one\n", vec![]);
    let moved = to(&mut wire, "heads/main", Some(root));
    // A member of the pushed register, and a key that it does not hold.
    for signer in [mia(), stranger()] {
        assert_eq!(
            refused(push(&mut wire, &signer, vec![moved.clone()])),
            [("heads/main".to_owned(), RefusalReason::NotOwner)]
        );
    }
    unchanged(&wire, &[], &[("heads/main", None)]);

    // A commit without a register: a hub holds enforcing repositories only.
    let bare = wire.side().commit(&[("a.txt", "one\n")], vec![]);
    let moved = to(&mut wire, "heads/main", Some(bare));
    assert_eq!(
        refused(push(&mut wire, &daniel(), vec![moved])),
        [("heads/main".to_owned(), RefusalReason::NoRegister)]
    );

    // A register that its reader refuses.
    let broken = wire
        .side()
        .commit(&[(register::FILE, "stemma-register 9\n")], vec![]);
    let moved = to(&mut wire, "heads/main", Some(broken));
    let reasons = refused(push(&mut wire, &daniel(), vec![moved]));
    assert!(
        matches!(&reasons[..], [(main, RefusalReason::BadRegister(_))] if main == "heads/main"),
        "{reasons:?}"
    );

    // A commit that records an open conflict, which `main` never holds.
    let (conflicted, _) = crate::testutil::conflicted_commit(&wire.side().repository);
    let moved = to(&mut wire, "heads/main", Some(conflicted));
    let conflict = Reason::try_new("the commit records an open conflict").unwrap();
    assert_eq!(
        refused(push(&mut wire, &daniel(), vec![moved])),
        [(
            "heads/main".to_owned(),
            RefusalReason::BadRegister(conflict)
        )]
    );
    unchanged(&wire, &[], &[("heads/main", None)]);
}

#[test]
fn an_empty_repository_takes_no_update_that_does_not_set_main() {
    let mut wire = wire();
    let root = wire.side().governed("one\n", vec![]);
    let moved = to(&mut wire, "heads/topic", Some(root));
    assert_eq!(
        push(&mut wire, &daniel(), vec![moved]),
        Err(ErrorMessage::Refused(Refused::NoMain))
    );
    unchanged(&wire, &[], &[("heads/topic", None)]);
}

#[test]
fn main_moves_through_the_gate_only_and_cannot_lose_its_register() {
    let (mut wire, root) = started();
    let before = record_of(&wire);
    // A descendant, also for the owner: the gate is not part of this operation.
    let next = wire.side().governed("two\n", vec![root]);
    let moved = to(&mut wire, "heads/main", Some(next));
    assert_eq!(
        refused(push(&mut wire, &daniel(), vec![moved])),
        [("heads/main".to_owned(), RefusalReason::Gate)]
    );
    // A commit that removes the register, and the deletion of `main`.
    let stripped = wire.side().commit(&[("a.txt", "one\n")], vec![root]);
    for new in [Some(stripped), None] {
        let moved = to(&mut wire, "heads/main", new);
        assert_eq!(
            refused(push(&mut wire, &daniel(), vec![moved])),
            [("heads/main".to_owned(), RefusalReason::Gate)]
        );
    }
    unchanged(&wire, &before, &[("heads/main", Some(root))]);
}

#[test]
fn another_actor_is_refused_before_the_lease_ends_and_takes_the_branch_after_it() {
    let (mut wire, root) = started();
    let one = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let two = wire.side().commit(&[("b.txt", "two\n")], vec![one]);
    let three = wire.side().commit(&[("b.txt", "three\n")], vec![two]);
    let moved = to(&mut wire, "heads/feature", Some(one));
    let created = push(&mut wire, &mia(), vec![moved]).unwrap();
    assert_eq!(created.actor(), &human("mia"));
    // The verdict rested on no policy: the name was free.
    assert_eq!(created.policies(), []);
    let before = record_of(&wire);

    // One second before the end of the lease, which runs from the time of the entry.
    wire.side().time.set(START + LEASE - 1);
    let moved = to(&mut wire, "heads/feature", Some(two));
    let leased = RefusalReason::Leased {
        holder: human("mia"),
        expires: START + LEASE,
    };
    assert_eq!(
        refused(push(&mut wire, &ana(), vec![moved.clone()])),
        [("heads/feature".to_owned(), leased.clone())]
    );
    // The deletion is a move like every other.
    let deletion = to(&mut wire, "heads/feature", None);
    assert_eq!(
        refused(push(&mut wire, &ana(), vec![deletion])),
        [("heads/feature".to_owned(), leased)]
    );
    unchanged(&wire, &before, &[("heads/feature", Some(one))]);

    // At the end of the lease Ana takes the branch, and the entry names the policy that set
    // the duration.
    wire.side().time.set(START + LEASE);
    let taken = push(&mut wire, &ana(), vec![moved]).unwrap();
    assert_eq!(taken.actor(), &human("ana"));
    assert_eq!(taken.policies(), [root]);
    assert_eq!(taken.register(), root);
    assert_eq!(wire.side().reference("heads/feature"), Some(two));

    // Now Ana is the write owner, and Mia waits for the lease that began with Ana's entry.
    let moved = to(&mut wire, "heads/feature", Some(three));
    assert_eq!(
        refused(push(&mut wire, &mia(), vec![moved.clone()])),
        [(
            "heads/feature".to_owned(),
            RefusalReason::Leased {
                holder: human("ana"),
                expires: START + 2 * LEASE,
            }
        )]
    );
    // The write owner moves at any time, and every move renews the lease.
    let moved_on = push(&mut wire, &ana(), vec![moved]).unwrap();
    assert_eq!(moved_on.policies(), []);
    assert_eq!(wire.side().entries().len(), 4);
}

#[test]
fn the_write_owner_moves_a_branch_without_a_fast_forward() {
    let (mut wire, root) = started();
    let one = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let two = wire.side().commit(&[("b.txt", "two\n")], vec![one]);
    let rewritten = wire.side().commit(&[("b.txt", "other\n")], vec![root]);
    let feature = name("heads/feature");

    let moved = to(&mut wire, "heads/feature", Some(one));
    let created = push(&mut wire, &mia(), vec![moved]).unwrap();
    assert_eq!(created.is_fast_forward(&feature), Some(false));
    let moved = to(&mut wire, "heads/feature", Some(two));
    let forward = push(&mut wire, &mia(), vec![moved]).unwrap();
    assert_eq!(forward.is_fast_forward(&feature), Some(true));
    // A commit that does not descend from the old one, as a rebase leaves it.
    let moved = to(&mut wire, "heads/feature", Some(rewritten));
    let rebased = push(&mut wire, &mia(), vec![moved]).unwrap();
    assert_eq!(rebased.is_fast_forward(&feature), Some(false));
    assert_eq!(wire.side().reference("heads/feature"), Some(rewritten));
    // Back to an ancestor is no fast-forward either.
    let moved = to(&mut wire, "heads/feature", Some(root));
    let back = push(&mut wire, &mia(), vec![moved]).unwrap();
    assert_eq!(back.is_fast_forward(&feature), Some(false));
}

#[test]
fn a_deleted_branch_has_no_write_owner() {
    let (mut wire, root) = started();
    let one = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let moved = to(&mut wire, "heads/feature", Some(one));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    let deletion = to(&mut wire, "heads/feature", None);
    let deleted = push(&mut wire, &mia(), vec![deletion]).unwrap();
    assert_eq!(wire.side().reference("heads/feature"), None);
    // Ana creates the name at once, inside what was the lease of Mia.
    let created = push(
        &mut wire,
        &ana(),
        vec![again("heads/feature", &deleted, root)],
    )
    .unwrap();
    assert_eq!(created.actor(), &human("ana"));
    assert_eq!(wire.side().reference("heads/feature"), Some(root));
}

#[test]
fn a_tag_is_set_deleted_and_set_again_and_its_chain_survives() {
    let (mut wire, root) = started();
    let next = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let tag = name("tags/v1");

    let moved = to(&mut wire, "tags/v1", Some(root));
    let set = push(&mut wire, &mia(), vec![moved]).unwrap();
    assert_eq!(wire.side().reference("tags/v1"), Some(root));
    // A tag has no write owner: another member deletes it.
    let moved = to(&mut wire, "tags/v1", None);
    let deleted = push(&mut wire, &ana(), vec![moved]).unwrap();
    assert_eq!(wire.side().reference("tags/v1"), None);
    assert_eq!(listed(&mut wire, "tags/v1"), (None, None));
    let before = record_of(&wire);

    // The name is known to the record: an update that takes it for new is stale, and the
    // answer names the entry that deleted it.
    let moved = to(&mut wire, "tags/v1", Some(next));
    let stale = Stale::try_new(vec![StaleReference {
        name: tag.clone(),
        entry: Some(deleted.hash()),
        commit: None,
    }])
    .unwrap();
    assert_eq!(
        push(&mut wire, &mia(), vec![moved]),
        Err(ErrorMessage::Stale(stale))
    );
    // So is the first update of the tag when it is sent again (A, B, A).
    let replayed = Move::try_new(tag.clone(), None, None, Some(root), None).unwrap();
    assert!(matches!(
        push(&mut wire, &mia(), vec![replayed]),
        Err(ErrorMessage::Stale(_))
    ));
    unchanged(&wire, &before, &[("tags/v1", None)]);

    let again_set = push(&mut wire, &mia(), vec![again("tags/v1", &deleted, next)]).unwrap();
    assert_eq!(wire.side().reference("tags/v1"), Some(next));
    // The record shows that the tag was deleted and set again.
    let chain: Vec<Option<RecordHash>> = wire
        .side()
        .entries()
        .iter()
        .filter(|entry| entry.is_fast_forward(&tag).is_some())
        .map(|entry| entry.update().payload().moves()[0].previous())
        .collect();
    assert_eq!(chain, [None, Some(set.hash()), Some(deleted.hash())]);
    assert_eq!(wire.side().chain().entry_of(&tag), Some(again_set.hash()));
}

#[test]
fn an_update_with_one_refused_reference_moves_nothing() {
    let (mut wire, root) = started();
    let one = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let two = wire.side().commit(&[("b.txt", "two\n")], vec![one]);
    let moved = to(&mut wire, "heads/feature", Some(one));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    let before = record_of(&wire);
    let state = [
        ("heads/feature", Some(one)),
        ("heads/other", None),
        ("tags/v1", None),
    ];

    // Ana may create a branch and set a tag, and may not move the branch of Mia.
    let moves = vec![
        to(&mut wire, "heads/other", Some(one)),
        to(&mut wire, "heads/feature", Some(two)),
        to(&mut wire, "tags/v1", Some(root)),
    ];
    let reasons = refused(push(&mut wire, &ana(), moves));
    assert!(
        matches!(&reasons[..], [(feature, RefusalReason::Leased { .. })] if feature == "heads/feature"),
        "{reasons:?}"
    );
    unchanged(&wire, &before, &state);

    // Every refused reference is named with its reason.
    let moves = vec![
        to(&mut wire, "heads/main", Some(one)),
        to(&mut wire, "heads/feature", Some(two)),
        to(&mut wire, "heads/other", Some(one)),
    ];
    let reasons = refused(push(&mut wire, &ana(), moves));
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert!(matches!(reasons[0].1, RefusalReason::Leased { .. }));
    assert_eq!(reasons[1], ("heads/main".to_owned(), RefusalReason::Gate));
    unchanged(&wire, &before, &state);
}

#[test]
fn a_stale_update_names_every_stale_reference_with_its_state_and_moves_nothing() {
    let (mut wire, root) = started();
    let one = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let two = wire.side().commit(&[("b.txt", "two\n")], vec![one]);
    let moved = to(&mut wire, "heads/feature", Some(one));
    let created = push(&mut wire, &mia(), vec![moved]).unwrap();
    // Two updates are prepared on the same state, and the first one wins.
    let prepared = vec![
        to(&mut wire, "heads/feature", Some(two)),
        to(&mut wire, "tags/v1", Some(one)),
        to(&mut wire, "heads/other", Some(one)),
    ];
    let moved = to(&mut wire, "heads/feature", Some(two));
    let tagged = to(&mut wire, "tags/v1", Some(root));
    let winner = push(&mut wire, &mia(), vec![moved, tagged]).unwrap();
    assert_ne!(winner.hash(), created.hash());
    let before = record_of(&wire);

    let stale = Stale::try_new(vec![
        StaleReference {
            name: name("heads/feature"),
            entry: Some(winner.hash()),
            commit: Some(two),
        },
        StaleReference {
            name: name("tags/v1"),
            entry: Some(winner.hash()),
            commit: Some(root),
        },
    ])
    .unwrap();
    // The third reference of the update is not stale and is not named.
    assert_eq!(
        push(&mut wire, &mia(), prepared),
        Err(ErrorMessage::Stale(stale))
    );
    unchanged(
        &wire,
        &before,
        &[("heads/feature", Some(two)), ("heads/other", None)],
    );
}

#[test]
fn a_stop_between_the_entry_and_the_move_is_completed_from_the_entry() {
    let (mut wire, root) = started();
    let one = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let update = signed(&mia(), vec![to(&mut wire, "heads/feature", Some(one))]);

    // The entry is written, and the move fails.
    let heads = wire.side().repository.path().join("refs/heads");
    let denied = fault::deny_writes_in(&heads);
    let error = wire.side().accept(&update).unwrap_err();
    drop(denied);
    let AcceptError::NotCompleted { receipt, .. } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(error.receipt(), Some(&**receipt));
    assert_eq!(error.kind(), FailureKind::Other);
    assert_eq!(error.answer(), None);
    assert_eq!(wire.side().entries().last(), Some(&**receipt));
    assert_eq!(wire.side().reference("heads/feature"), None);

    // The next call for the repository completes the move before it judges its own update,
    // here one that is stale because it was prepared before.
    let late = Move::try_new(name("heads/feature"), None, None, Some(root), None).unwrap();
    assert!(matches!(
        push(&mut wire, &ana(), vec![late]),
        Err(ErrorMessage::Stale(_))
    ));
    assert_eq!(wire.side().reference("heads/feature"), Some(one));
    assert_eq!(wire.side().entries().len(), 2);
    // The update that stopped is accepted, so sending it again is stale as well.
    assert!(matches!(
        send(&mut wire, &update),
        Err(ErrorMessage::Stale(_))
    ));
    assert_eq!(
        listed(&mut wire, "heads/feature"),
        (Some(receipt.hash()), Some(one))
    );
}

#[test]
fn a_stop_between_two_moves_is_completed_and_an_accepted_update_follows() {
    let (mut wire, root) = started();
    let one = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let two = wire.side().commit(&[("b.txt", "two\n")], vec![one]);
    let update = signed(
        &mia(),
        vec![
            to(&mut wire, "heads/feature", Some(one)),
            to(&mut wire, "tags/v1", Some(one)),
        ],
    );
    // The branch moves, and the tag does not.
    let tags = wire.side().repository.path().join("refs/tags");
    fs::create_dir_all(&tags).unwrap();
    let denied = fault::deny_writes_in(&tags);
    let error = wire.side().accept(&update).unwrap_err();
    drop(denied);
    assert!(
        matches!(error, AcceptError::NotCompleted { .. }),
        "{error:?}"
    );
    assert_eq!(wire.side().reference("heads/feature"), Some(one));
    assert_eq!(wire.side().reference("tags/v1"), None);

    // The record says where the references are, so the next update builds on the entry.
    let stopped = wire.side().entries().pop().unwrap();
    let next = Move::try_new(
        name("heads/feature"),
        Some(stopped.hash()),
        Some(one),
        Some(two),
        None,
    )
    .unwrap();
    let receipt = push(&mut wire, &mia(), vec![next]).unwrap();
    assert_eq!(receipt.number(), 3);
    assert_eq!(wire.side().reference("heads/feature"), Some(two));
    assert_eq!(wire.side().reference("tags/v1"), Some(one));
}

#[test]
fn an_update_for_another_hub_or_another_repository_is_refused() {
    let (mut wire, root) = started();
    let before = record_of(&wire);
    let moved = to(&mut wire, "heads/feature", Some(root));
    let elsewhere = signed_for(
        &mia(),
        "https://other.example",
        REPOSITORY,
        vec![moved.clone()],
    );
    assert_eq!(
        send(&mut wire, &elsewhere),
        Err(ErrorMessage::Refused(Refused::Hub { called: address() }))
    );
    let other = "ffeeddccbbaa99887766554433221100";
    let foreign = signed_for(&mia(), ADDRESS, other, vec![moved]);
    assert_eq!(
        send(&mut wire, &foreign),
        Err(ErrorMessage::Refused(Refused::Repository {
            id: repository_id()
        }))
    );
    unchanged(&wire, &before, &[("heads/feature", None)]);
}

#[test]
fn a_key_that_the_register_of_main_does_not_hold_is_refused() {
    let (mut wire, root) = started();
    let before = record_of(&wire);
    let moved = to(&mut wire, "heads/feature", Some(root));
    // The register that counts is the one at the tip of `main`, not one that the update brings.
    assert_eq!(
        push(&mut wire, &stranger(), vec![moved]),
        Err(ErrorMessage::Refused(Refused::Key { register: root }))
    );
    unchanged(&wire, &before, &[("heads/feature", None)]);
}

#[test]
fn the_authorizer_is_asked_for_push_tag_and_untag() {
    let (mut wire, root) = started();
    let tagged = to(&mut wire, "tags/v1", Some(root));
    push(&mut wire, &mia(), vec![tagged]).unwrap();
    let before = record_of(&wire);
    let denied = |operation: &str| RefusalReason::Denied {
        operation: RefOperation::ALL
            .into_iter()
            .find(|found| found.name() == operation)
            .unwrap(),
    };
    let cases = [
        ("push", to(&mut wire, "heads/feature", Some(root))),
        ("tag", to(&mut wire, "tags/v2", Some(root))),
        ("untag", to(&mut wire, "tags/v1", None)),
    ];
    for (operation, moved) in cases {
        let reference = moved.name().clone();
        let update = signed(&mia(), vec![moved]);
        let error = wire
            .side()
            .accept_with(&update, &Deny(operation))
            .unwrap_err();
        let Some(ErrorMessage::Refused(Refused::References(refused))) = error.answer() else {
            panic!("{error:?}");
        };
        assert_eq!(refused.reason_of(&reference), Some(&denied(operation)));
    }
    unchanged(
        &wire,
        &before,
        &[("heads/feature", None), ("tags/v1", Some(root))],
    );
}

#[test]
fn a_commit_whose_objects_are_not_there_is_refused_as_incomplete() {
    let (mut wire, root) = started();
    let before = record_of(&wire);
    // A commit of another store: its ID is known, and none of its objects arrived.
    let elsewhere = Hub::new();
    let absent = elsewhere.commit(&[("c.txt", "three\n")], vec![]);
    let moves = vec![
        to(&mut wire, "heads/feature", Some(absent)),
        to(&mut wire, "heads/other", Some(root)),
    ];
    let reasons = refused(push(&mut wire, &mia(), moves));
    assert!(
        matches!(&reasons[..], [(feature, RefusalReason::Incomplete(_))] if feature == "heads/feature"),
        "{reasons:?}"
    );
    unchanged(
        &wire,
        &before,
        &[("heads/feature", None), ("heads/other", None)],
    );

    // The same for the commit of a first push, whose register cannot be read.
    let mut empty = self::wire();
    let moved = to(&mut empty, "heads/main", Some(absent));
    let reasons = refused(push(&mut empty, &daniel(), vec![moved]));
    assert!(
        matches!(&reasons[..], [(main, RefusalReason::Incomplete(_))] if main == "heads/main"),
        "{reasons:?}"
    );
}

#[test]
fn a_name_that_is_reserved_or_held_by_another_reference_is_refused() {
    let (mut wire, root) = started();
    let moved = to(&mut wire, "heads/feature", Some(root));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    let before = record_of(&wire);

    let conflict = |with: &str| RefusalReason::Conflict(BranchName::try_new(with).unwrap());
    let moves = vec![
        // A reserved prefix (ADR-008).
        to(&mut wire, "heads/tags/v1", Some(root)),
        // A name below a branch that exists, and one that differs from it in case only.
        to(&mut wire, "heads/feature/x", Some(root)),
        to(&mut wire, "heads/Feature", Some(root)),
        // Two new names of one update, one below the other, and two that differ in case.
        to(&mut wire, "heads/new", Some(root)),
        to(&mut wire, "heads/new/x", Some(root)),
        to(&mut wire, "tags/R1", Some(root)),
        to(&mut wire, "tags/r1", Some(root)),
        // A tag may carry the name of a branch.
        to(&mut wire, "tags/feature", Some(root)),
    ];
    assert_eq!(
        refused(push(&mut wire, &mia(), moves)),
        [
            ("heads/Feature".to_owned(), conflict("feature")),
            ("heads/feature/x".to_owned(), conflict("feature")),
            ("heads/new/x".to_owned(), conflict("new")),
            ("heads/tags/v1".to_owned(), RefusalReason::Reserved),
            ("tags/r1".to_owned(), conflict("R1")),
        ]
    );
    unchanged(
        &wire,
        &before,
        &[("heads/new", None), ("tags/feature", None)],
    );
}

#[test]
fn only_a_bare_repository_accepts_a_ref_update() {
    let dir = TempDir::new();
    let actor = Actor::human("daniel").unwrap();
    let repository = Repository::init(
        dir.path(),
        &crate::repository::InitOptions::default(),
        &actor,
        &AllowAll,
    )
    .unwrap();
    let commit = CommitId::new(format!("b3:{}", "11".repeat(32)).parse().unwrap());
    let moved = Move::try_new(name("heads/main"), None, None, Some(commit), None).unwrap();
    let update = signed(&daniel(), vec![moved]);
    let (hub, signer) = (address(), hub_key());
    let mut record = MemoryRecord::default();
    let context = AcceptContext {
        hub: &hub,
        repository: repository_id(),
        record: &mut record,
        time: START,
        signer: &signer,
    };
    assert_eq!(
        format!("{context:?}"),
        format!(
            "AcceptContext {{ hub: HubAddress({ADDRESS:?}), repository: RepositoryId({REPOSITORY}), \
             time: {START}, .. }}"
        )
    );
    let error = repository.accept_ref_update(&update, context).unwrap_err();
    assert!(matches!(error, AcceptError::NotBare { .. }), "{error:?}");
    assert!(error.to_string().contains("has a working copy"), "{error}");
    assert_eq!(error.kind(), FailureKind::Other);
    assert!(record.bytes.is_empty());
}

#[test]
fn a_reference_that_was_moved_past_the_record_stops_the_update() {
    let (mut wire, root) = started();
    let one = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let moved = to(&mut wire, "heads/feature", Some(one));
    push(&mut wire, &mia(), vec![moved.clone()]).unwrap();
    let next = to(&mut wire, "heads/feature", Some(root));
    let before = record_of(&wire);
    // Someone writes the file of the branch by hand.
    let path = wire.side().repository.path().join("refs/heads/feature");
    fs::write(&path, crate::refs::encode_branch(&root)).unwrap();

    let error = wire.side().accept(&signed(&mia(), vec![next])).unwrap_err();
    let AcceptError::Diverged {
        name: reference,
        recorded,
        found,
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(reference, &name("heads/feature"));
    assert_eq!((*recorded, *found), (Some(one), Some(root)));
    assert_eq!(error.kind(), FailureKind::Damaged);
    assert_eq!(error.answer(), None);
    assert_eq!(record_of(&wire), before);
    // The same for a reference that the last entry does not name, and for one that is gone.
    fs::write(&path, crate::refs::encode_branch(&one)).unwrap();
    let tagged = signed(&mia(), vec![to(&mut wire, "tags/v1", Some(root))]);
    wire.side().accept(&tagged).unwrap();
    fs::write(&path, crate::refs::encode_branch(&root)).unwrap();
    let update = signed(&mia(), vec![to(&mut wire, "tags/v2", Some(root))]);
    let error = wire.side().accept(&update).unwrap_err();
    assert!(
        matches!(&error, AcceptError::Diverged { name: moved, .. } if moved == &name("heads/feature")),
        "{error:?}"
    );
    fs::remove_file(&path).unwrap();
    let error = wire.side().accept(&update).unwrap_err();
    assert!(
        matches!(&error, AcceptError::Diverged { found: None, .. }),
        "{error:?}"
    );
    // A failure of the hub is no answer of the protocol.
    let again = signed(&mia(), vec![moved]);
    assert!(matches!(
        wire.ref_update(&mut again.encode().as_slice()),
        Err(TransportError::NoAnswer { .. })
    ));
}

#[test]
fn a_held_lock_is_reported_and_nothing_is_written() {
    let (mut wire, root) = started();
    let before = record_of(&wire);
    let update = signed(&mia(), vec![to(&mut wire, "heads/feature", Some(root))]);
    let lock = wire
        .side()
        .repository
        .refs_store()
        .lock_writer(&Allowed::granted())
        .unwrap();
    let error = wire.side().accept(&update).unwrap_err();
    assert!(
        matches!(&error, AcceptError::Refs(source) if matches!(**source, RefError::Locked { .. })),
        "{error:?}"
    );
    assert_eq!(error.kind(), FailureKind::Locked);
    drop(lock);
    unchanged(&wire, &before, &[("heads/feature", None)]);
    // The update is as good as before.
    send(&mut wire, &update).unwrap();
}

#[test]
fn a_record_that_cannot_be_used_stops_the_update() {
    let (mut wire, root) = started();
    let update = signed(&mia(), vec![to(&mut wire, "heads/feature", Some(root))]);
    let good = record_of(&wire);
    let set = |change: &dyn Fn(&mut MemoryRecord)| {
        let mut record = MemoryRecord {
            bytes: good.clone(),
            ..MemoryRecord::default()
        };
        change(&mut record);
        *wire.side().record.borrow_mut() = record;
    };

    // The destination cannot be read.
    set(&|record| record.fail_read = true);
    let error = wire.side().accept(&update).unwrap_err();
    assert!(
        matches!(
            error,
            AcceptError::RecordIo {
                step: RecordStep::Read,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(error.io_error().is_some());
    assert_eq!(error.kind(), FailureKind::Other);

    // An entry is cut off.
    set(&|record| record.bytes.truncate(good.len() - 10));
    let error = wire.side().accept(&update).unwrap_err();
    assert!(matches!(error, AcceptError::Record(_)), "{error:?}");
    assert_eq!(error.kind(), FailureKind::Damaged);

    // The record lost its entries, so the references are ahead of it.
    set(&|record| record.bytes.clear());
    let error = wire.side().accept(&update).unwrap_err();
    assert!(
        matches!(
            &error,
            AcceptError::Diverged { name: main, recorded: None, found: Some(found) }
                if main == &name("heads/main") && *found == root
        ),
        "{error:?}"
    );
    assert_eq!(error.kind(), FailureKind::Damaged);

    // The entry is doubled: the second one does not follow the first.
    set(&|record| record.bytes.extend_from_slice(&good));
    let error = wire.side().accept(&update).unwrap_err();
    assert!(matches!(error, AcceptError::Chain(_)), "{error:?}");
    assert_eq!(error.kind(), FailureKind::Damaged);

    // The entry cannot be appended: nothing moves, and nothing is recorded.
    set(&|record| record.fail_append = true);
    let error = wire.side().accept(&update).unwrap_err();
    assert!(
        matches!(
            error,
            AcceptError::RecordIo {
                step: RecordStep::Append,
                ..
            }
        ),
        "{error:?}"
    );
    unchanged(&wire, &good, &[("heads/feature", None)]);

    set(&|_| {});
    send(&mut wire, &update).unwrap();
}

#[test]
fn a_signer_of_the_hub_that_fails_leaves_nothing_behind() {
    let (mut wire, root) = started();
    let before = record_of(&wire);
    let update = signed(&mia(), vec![to(&mut wire, "heads/feature", Some(root))]);
    let hub = address();
    let error = {
        let mut record = wire.side().record.borrow_mut();
        let context = AcceptContext {
            hub: &hub,
            repository: repository_id(),
            record: &mut *record,
            time: START,
            signer: &Broken,
        };
        wire.side()
            .repository
            .accept_ref_update(&update, context)
            .unwrap_err()
    };
    assert!(matches!(error, AcceptError::Write(_)), "{error:?}");
    assert_eq!(error.kind(), FailureKind::Other);
    unchanged(&wire, &before, &[("heads/feature", None)]);
}

#[test]
fn an_update_that_was_changed_on_the_way_is_answered_as_malformed() {
    let (mut wire, root) = started();
    let before = record_of(&wire);
    let other = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let update = signed(&mia(), vec![to(&mut wire, "heads/feature", Some(root))]);
    let text = String::from_utf8(update.encode()).unwrap();
    let changed = text.replace(&root.to_string(), &other.to_string());
    assert_ne!(changed, text);
    let Answer::Error(body) = wire.ref_update(&mut changed.as_bytes()).unwrap() else {
        panic!("a changed update was accepted");
    };
    let expected = ParseError::Record {
        line: 7,
        reason: RecordProblem::ActorSignature,
    };
    assert_eq!(
        ErrorMessage::parse(&read_all(body)).unwrap(),
        ErrorMessage::from(&expected)
    );
    unchanged(&wire, &before, &[("heads/feature", None)]);
}

#[test]
fn a_side_that_does_not_answer_ref_updates_fails() {
    struct Reader;
    impl OtherSide for Reader {
        fn refs(&self) -> Result<RefList, Refusal> {
            Err(Refusal::Failed("no list".to_owned()))
        }
        fn fetch(&self, _: &FetchRequest, _: &mut dyn Write) -> Result<(), Refusal> {
            Err(Refusal::Failed("no stream".to_owned()))
        }
    }
    let commit = CommitId::new(format!("b3:{}", "11".repeat(32)).parse().unwrap());
    let moved = Move::try_new(name("heads/main"), None, None, Some(commit), None).unwrap();
    let update = signed(&daniel(), vec![moved]);
    let mut wire = MemoryTransport::new(Reader);
    let error = wire
        .ref_update(&mut update.encode().as_slice())
        .unwrap_err();
    assert!(
        matches!(&error, TransportError::NoAnswer { detail } if detail.contains("no ref update")),
        "{error:?}"
    );
}

#[test]
fn the_names_that_stand_in_each_others_way_are_compared_without_case() {
    let branch = |text: &str| BranchName::try_new(text).unwrap();
    assert!(in_the_way(&branch("a"), &branch("a")));
    assert!(in_the_way(&branch("a"), &branch("A")));
    assert!(in_the_way(&branch("a"), &branch("A/b")));
    assert!(in_the_way(&branch("a/b/c"), &branch("a/B")));
    assert!(!in_the_way(&branch("a"), &branch("ab")));
    assert!(!in_the_way(&branch("a/b"), &branch("a/c")));
    assert!(!in_the_way(&branch("a/b"), &branch("b")));
}

#[test]
fn every_failure_has_a_message() {
    let (mut wire, root) = started();
    let update = signed(&mia(), vec![to(&mut wire, "heads/feature", Some(root))]);
    let heads = wire.side().repository.path().join("refs/heads");
    let denied = fault::deny_writes_in(&heads);
    let error = wire.side().accept(&update).unwrap_err();
    drop(denied);
    assert!(
        error
            .to_string()
            .starts_with("the record entry 2 is written, but a reference did not move: "),
        "{error}"
    );
    let diverged = AcceptError::Diverged {
        name: name("heads/feature"),
        recorded: None,
        found: Some(root),
    };
    assert_eq!(
        diverged.to_string(),
        format!(
            "the reference heads/feature is at {root}, and the record of ref updates leaves it \
             at no commit"
        )
    );
    let io = AcceptError::RecordIo {
        step: RecordStep::Read,
        source: io::Error::other("gone"),
    };
    assert_eq!(
        io.to_string(),
        "cannot read the record of ref updates: gone"
    );
}

#[test]
fn a_first_push_that_stopped_is_completed_by_the_update_of_another_sender() {
    let mut wire = wire();
    let root = wire.side().governed("one\n", vec![]);
    let first = signed(&daniel(), vec![to(&mut wire, "heads/main", Some(root))]);
    let heads = wire.side().repository.path().join("refs/heads");
    fs::create_dir_all(&heads).unwrap();
    let denied = fault::deny_writes_in(&heads);
    let error = wire.side().accept(&first).unwrap_err();
    drop(denied);
    assert!(
        matches!(error, AcceptError::NotCompleted { .. }),
        "{error:?}"
    );
    assert_eq!(wire.side().reference("heads/main"), None);

    // The repository has no `main` yet, and the record names it: Mia is judged by the register
    // of that commit, and her update completes the first push before it is accepted.
    let moved = Move::try_new(name("heads/topic"), None, None, Some(root), None).unwrap();
    let receipt = push(&mut wire, &mia(), vec![moved]).unwrap();
    assert_eq!(receipt.number(), 2);
    assert_eq!(receipt.register(), root);
    assert_eq!(wire.side().reference("heads/main"), Some(root));
    assert_eq!(wire.side().reference("heads/topic"), Some(root));
}

#[test]
fn a_stopped_deletion_is_completed_and_a_completion_that_fails_writes_nothing() {
    let (mut wire, root) = started();
    let moved = to(&mut wire, "heads/feature", Some(root));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    let deletion = signed(&mia(), vec![to(&mut wire, "heads/feature", None)]);
    let heads = wire.side().repository.path().join("refs/heads");
    let denied = fault::deny_writes_in(&heads);
    let error = wire.side().accept(&deletion).unwrap_err();
    assert!(
        matches!(error, AcceptError::NotCompleted { .. }),
        "{error:?}"
    );
    assert_eq!(wire.side().reference("heads/feature"), Some(root));
    let before = record_of(&wire);

    // While the storage still fails, the next call cannot complete the deletion. It reports
    // the references and appends nothing.
    let next = signed(&ana(), vec![to(&mut wire, "tags/v1", Some(root))]);
    let error = wire.side().accept(&next).unwrap_err();
    assert!(matches!(error, AcceptError::Refs(_)), "{error:?}");
    assert_eq!(error.answer(), None);
    unchanged(
        &wire,
        &before,
        &[("heads/feature", Some(root)), ("tags/v1", None)],
    );
    drop(denied);

    send(&mut wire, &next).unwrap();
    assert_eq!(wire.side().reference("heads/feature"), None);
    assert_eq!(listed(&mut wire, "heads/feature"), (None, None));
    assert_eq!(wire.side().reference("tags/v1"), Some(root));
}

#[test]
fn a_name_that_was_taken_behind_a_stopped_update_stops_the_next_one() {
    let (mut wire, root) = started();
    let update = signed(&mia(), vec![to(&mut wire, "heads/feature", Some(root))]);
    let heads = wire.side().repository.path().join("refs/heads");
    let denied = fault::deny_writes_in(&heads);
    wire.side().accept(&update).unwrap_err();
    drop(denied);
    let before = record_of(&wire);
    // Someone writes a branch below the name that the entry sets.
    fs::create_dir(heads.join("feature")).unwrap();
    fs::write(heads.join("feature/x"), crate::refs::encode_branch(&root)).unwrap();

    let tagged = Move::try_new(name("tags/v1"), None, None, Some(root), None).unwrap();
    let next = signed(&ana(), vec![tagged]);
    let error = wire.side().accept(&next).unwrap_err();
    assert!(
        matches!(
            &error,
            AcceptError::Diverged { name: taken, recorded: Some(_), found: None }
                if taken == &name("heads/feature")
        ),
        "{error:?}"
    );
    assert_eq!(error.kind(), FailureKind::Damaged);
    unchanged(&wire, &before, &[("tags/v1", None)]);
}

#[test]
fn a_new_name_below_a_branch_that_the_same_update_deletes_is_refused() {
    let (mut wire, root) = started();
    let moved = to(&mut wire, "heads/feature", Some(root));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    let before = record_of(&wire);
    let moves = vec![
        to(&mut wire, "heads/feature", None),
        to(&mut wire, "heads/feature/x", Some(root)),
    ];
    assert_eq!(
        refused(push(&mut wire, &mia(), moves)),
        [(
            "heads/feature/x".to_owned(),
            RefusalReason::Conflict(BranchName::try_new("feature").unwrap())
        )]
    );
    unchanged(&wire, &before, &[("heads/feature", Some(root))]);
}

#[test]
fn an_empty_directory_that_holds_a_name_is_cleared() {
    let (mut wire, root) = started();
    let leftover = wire.side().repository.path().join("refs/heads/feature");
    fs::create_dir(&leftover).unwrap();
    let moved = to(&mut wire, "heads/feature", Some(root));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    assert!(leftover.is_file());
    assert_eq!(wire.side().reference("heads/feature"), Some(root));
}

#[test]
fn a_branch_of_another_actor_waits_for_a_policy_that_can_be_used() {
    let mut wire = wire();
    // The first push needs a register and no policy.
    let root = wire
        .side()
        .commit(&[(register::FILE, &register_text())], vec![]);
    let one = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let two = wire.side().commit(&[("b.txt", "two\n")], vec![one]);
    let moved = to(&mut wire, "heads/main", Some(root));
    push(&mut wire, &daniel(), vec![moved]).unwrap();
    let moved = to(&mut wire, "heads/feature", Some(root));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    let before = record_of(&wire);

    // Without a policy no lease has a duration, so the branch of Mia is never free for Ana.
    for time in [START, START + 100 * LEASE] {
        wire.side().time.set(time);
        let moved = to(&mut wire, "heads/feature", Some(one));
        let reasons = refused(push(&mut wire, &ana(), vec![moved]));
        assert!(
            matches!(&reasons[..], [(feature, RefusalReason::Policy(_))] if feature == "heads/feature"),
            "{reasons:?}"
        );
    }
    unchanged(&wire, &before, &[("heads/feature", Some(root))]);
    // The write owner reads no policy.
    let moved = to(&mut wire, "heads/feature", Some(two));
    push(&mut wire, &mia(), vec![moved]).unwrap();
}

#[test]
fn a_policy_that_the_store_cannot_return_is_a_failure_of_the_hub() {
    let (mut wire, root) = started();
    let moved = to(&mut wire, "heads/feature", Some(root));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    let before = record_of(&wire);
    let one = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let update = signed(&ana(), vec![to(&mut wire, "heads/feature", Some(one))]);
    // The blob of the policy is lost.
    let store = wire.side().repository.objects();
    let blob = store.put_blob(&Blob::new(POLICY)).unwrap();
    fs::remove_file(store.object_path(&blob.object_id())).unwrap();

    let error = wire.side().accept(&update).unwrap_err();
    assert!(
        matches!(error, AcceptError::Policy { damaged: true, .. }),
        "{error:?}"
    );
    assert_eq!(error.answer(), None);
    // An object that the history names is gone: the store is damaged.
    assert_eq!(error.kind(), FailureKind::Damaged);
    unchanged(&wire, &before, &[("heads/feature", Some(root))]);
}

#[test]
fn an_agent_is_a_write_owner_by_its_identity() {
    let mut wire = wire();
    let bot = Seeded::new(6);
    let agent = Identity::of(&Actor::agent("bot", "m", "s", "mia").unwrap());
    let mut register = Register::parse(register_text().as_bytes()).unwrap();
    let cosign = crate::signature::sign(
        &mia(),
        crate::signature::PayloadKind::Sponsorship,
        &register::sponsorship_payload(&agent, Role::Member, &human("mia")),
    )
    .unwrap();
    register
        .add_agent(
            agent.clone(),
            Role::Member,
            human("mia"),
            bot.public(),
            cosign,
        )
        .unwrap();
    let root = wire.side().governed_by(&register.encode(), "one\n", vec![]);
    let one = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let moved = to(&mut wire, "heads/main", Some(root));
    push(&mut wire, &daniel(), vec![moved]).unwrap();

    let moved = to(&mut wire, "heads/feature", Some(root));
    let created = push(&mut wire, &bot, vec![moved]).unwrap();
    assert_eq!(created.actor(), &agent);
    // The agent is the write owner whatever its model and its session are, and its sponsor is
    // another identity.
    let moved = to(&mut wire, "heads/feature", Some(one));
    assert_eq!(
        refused(push(&mut wire, &mia(), vec![moved.clone()])),
        [(
            "heads/feature".to_owned(),
            RefusalReason::Leased {
                holder: agent,
                expires: START + LEASE
            }
        )]
    );
    push(&mut wire, &bot, vec![moved]).unwrap();
    assert_eq!(wire.side().reference("heads/feature"), Some(one));
}

#[test]
fn nothing_of_the_repository_is_read_before_the_lock() {
    let (mut wire, root) = started();
    let good = record_of(&wire);
    // A stale update of a key that the register does not hold is stale: the sender is judged
    // under the lock, after the chains.
    let late = Move::try_new(name("heads/main"), None, None, Some(root), None).unwrap();
    assert!(matches!(
        push(&mut wire, &stranger(), vec![late]),
        Err(ErrorMessage::Stale(_))
    ));
    // Beside a writer the answer is the lock, whatever the record and the sender are.
    let update = signed(&stranger(), vec![to(&mut wire, "heads/topic", Some(root))]);
    wire.side().record.borrow_mut().bytes = b"stemma-protocol 1\nhub".to_vec();
    let lock = wire
        .side()
        .repository
        .refs_store()
        .lock_writer(&Allowed::granted())
        .unwrap();
    let error = wire.side().accept(&update).unwrap_err();
    assert_eq!(error.kind(), FailureKind::Locked, "{error:?}");
    assert_eq!(error.receipt(), None);
    drop(lock);
    // Under the lock the record is read first.
    let error = wire.side().accept(&update).unwrap_err();
    assert!(matches!(error, AcceptError::Record(_)), "{error:?}");
    wire.side().record.borrow_mut().bytes = good;
    assert_eq!(
        send(&mut wire, &update),
        Err(ErrorMessage::Refused(Refused::Key { register: root }))
    );
}

#[test]
fn the_time_of_the_hub_does_not_step_back() {
    let (mut wire, root) = started();
    let before = record_of(&wire);
    let update = signed(&mia(), vec![to(&mut wire, "heads/feature", Some(root))]);
    wire.side().time.set(START - 1);
    let error = wire.side().accept(&update).unwrap_err();
    assert!(
        matches!(
            &error,
            AcceptError::Write(source) if matches!(
                **source,
                WriteError::Chain(ChainError::Time { last: START, found })
                    if found == START - 1
            )
        ),
        "{error:?}"
    );
    assert_eq!(error.answer(), None);
    unchanged(&wire, &before, &[("heads/feature", None)]);
    // The same second is taken.
    wire.side().time.set(START);
    send(&mut wire, &update).unwrap();
}

/// A hub after the first push whose record ends with an entry that this operation would not
/// write: the update of `signer` for `heads/feature`, recorded as an update of `actor`, whose
/// move did not happen.
fn with_forged_last_entry(signer: &Seeded, actor: &str) -> (Wire, CommitId) {
    let (mut wire, root) = started();
    let moved = to(&mut wire, "heads/feature", Some(root));
    let forged = signed(signer, vec![moved]);
    let acceptance = Acceptance {
        time: START,
        register: root,
        policies: Vec::new(),
        fast_forward: BTreeSet::new(),
        actor: human(actor),
    };
    let mut bytes = Vec::new();
    wire.side()
        .chain()
        .append(&forged, acceptance, &hub_key(), &mut bytes)
        .unwrap();
    wire.side()
        .record
        .borrow_mut()
        .bytes
        .extend_from_slice(&bytes);
    (wire, root)
}

#[test]
fn an_entry_whose_moves_its_register_does_not_allow_is_not_completed() {
    // A key that the register does not hold, and a key of Mia recorded as an update of Ana.
    for (signer, actor) in [(stranger(), "mia"), (mia(), "ana")] {
        let (wire, root) = with_forged_last_entry(&signer, actor);
        let before = record_of(&wire);
        let tagged = Move::try_new(name("tags/v1"), None, None, Some(root), None).unwrap();
        let error = wire
            .side()
            .accept(&signed(&ana(), vec![tagged]))
            .unwrap_err();
        assert!(
            matches!(error, AcceptError::LastEntry { number: 2 }),
            "{actor}: {error:?}"
        );
        assert_eq!(error.kind(), FailureKind::Damaged);
        assert!(error.to_string().contains("record entry 2"), "{error}");
        unchanged(
            &wire,
            &before,
            &[("heads/feature", None), ("tags/v1", None)],
        );
    }
    // The same entry with its true actor is completed.
    let (mut wire, root) = with_forged_last_entry(&mia(), "mia");
    let tagged = Move::try_new(name("tags/v1"), None, None, Some(root), None).unwrap();
    send(&mut wire, &signed(&ana(), vec![tagged])).unwrap();
    assert_eq!(wire.side().reference("heads/feature"), Some(root));
}

#[test]
fn a_policy_that_the_storage_cannot_read_now_is_no_damage() {
    let (mut wire, root) = started();
    let moved = to(&mut wire, "heads/feature", Some(root));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    let before = record_of(&wire);
    let one = wire.side().commit(&[("b.txt", "one\n")], vec![root]);
    let update = signed(&ana(), vec![to(&mut wire, "heads/feature", Some(one))]);
    let store = wire.side().repository.objects();
    let blob = store.put_blob(&Blob::new(POLICY)).unwrap();
    let denied = fault::deny_reads_of(&store.object_path(&blob.object_id()));
    let error = wire.side().accept(&update).unwrap_err();
    drop(denied);
    assert!(
        matches!(error, AcceptError::Policy { damaged: false, .. }),
        "{error:?}"
    );
    assert_eq!(error.kind(), FailureKind::Other);
    assert_eq!(error.answer(), None);
    unchanged(&wire, &before, &[("heads/feature", Some(root))]);
    // Once the storage reads again, the same update is judged.
    wire.side().time.set(START + LEASE);
    send(&mut wire, &update).unwrap();
}
