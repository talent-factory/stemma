//! The tests of the anchors on a hub (ADR-026, STEMMA-300): a bare repository after a first
//! push, its record and the list of the anchors of one log in memory.

use std::cell::RefCell;

use super::*;
use crate::change::ChangeId;
use crate::index::Index;
use crate::object::{Blob, Commit, CommitId, FileMode};
use crate::oplog::EntryHash;
use crate::protocol::anchor::{AnchorLines, AnchorPayload, AnchorState, Point};
use crate::protocol::record::tests::Seeded;
use crate::protocol::record::{HubAddress, Move, RecordEntry, RefUpdate, RefUpdatePayload};
use crate::protocol::RepositoryId;
use crate::register::{self, Role};
use crate::repository::RecordDestination;
use crate::store::ObjectStore;
use crate::testutil::TempDir;
use crate::{Actor, AllowAll};

const ADDRESS: &str = "https://hub.example";
const REPOSITORY: &str = "00112233445566778899aabbccddeeff";
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

fn address() -> HubAddress {
    HubAddress::try_new(ADDRESS).unwrap()
}

fn repository_id() -> RepositoryId {
    REPOSITORY.parse().unwrap()
}

fn log() -> LogId {
    LogId::from_bytes([0x5a; 16])
}

fn hash(byte: u8) -> EntryHash {
    format!("b3:{}", format!("{byte:02x}").repeat(32))
        .parse()
        .unwrap()
}

fn point(number: u64, byte: u8) -> Point {
    Point {
        number,
        hash: hash(byte),
    }
}

/// A list or a record in memory, which can be told to fail.
#[derive(Default)]
struct Memory {
    bytes: Vec<u8>,
    fail_append: bool,
}

impl RecordDestination for Memory {
    fn read(&mut self) -> io::Result<Vec<u8>> {
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

impl AnchorDestination for Memory {
    fn read(&mut self) -> io::Result<Vec<u8>> {
        Ok(self.bytes.clone())
    }

    fn append(&mut self, held: &[u8]) -> io::Result<()> {
        if self.fail_append {
            return Err(io::Error::other("the disk is full"));
        }
        self.bytes.extend_from_slice(held);
        Ok(())
    }
}

/// A hub after the first push of Daniel: `main` at a commit whose register holds Daniel as the
/// owner and Mia as a member.
struct Hub {
    _dir: TempDir,
    repository: Repository,
    record: RefCell<Memory>,
    anchors: RefCell<Memory>,
    time: std::cell::Cell<i64>,
}

impl Hub {
    fn new() -> Self {
        let dir = TempDir::new();
        let owner = Actor::human("operator").unwrap();
        let repository = Repository::init_bare(dir.path().join("hub"), &owner, &AllowAll).unwrap();
        let hub = Self {
            _dir: dir,
            repository,
            record: RefCell::default(),
            anchors: RefCell::default(),
            time: std::cell::Cell::new(START),
        };
        let root = hub.commit(&[(register::FILE, &register_text())]);
        let main = "heads/main".parse().unwrap();
        hub.push(vec![
            Move::try_new(main, None, None, Some(root), None).unwrap()
        ])
        .unwrap();
        hub
    }

    fn commit(&self, files: &[(&str, &str)]) -> CommitId {
        let store = self.repository.objects();
        let mut index = Index::new();
        for (path, text) in files {
            let blob = store.put_blob(&Blob::new(*text)).unwrap();
            index.insert(*path, FileMode::File, blob).unwrap();
        }
        let commit = Commit::try_new(
            index.write_tree(store).unwrap(),
            Vec::new(),
            ChangeId::generate().unwrap(),
            Actor::human("daniel").unwrap(),
            1,
            "a commit",
        )
        .unwrap();
        store.put_commit(&commit).unwrap()
    }

    fn push(&self, moves: Vec<Move>) -> Result<RecordEntry, crate::repository::AcceptError> {
        let update: RefUpdate =
            RefUpdatePayload::try_new(address(), repository_id(), moves, daniel().public())
                .unwrap()
                .sign(&daniel())
                .unwrap();
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
        self.repository.accept_ref_update(&update, context)
    }

    /// Sends an anchor to the hub as it answers the request.
    fn send(&self, anchor: &Anchor) -> Result<HeldAnchor, AnchorError> {
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
            .accept_anchor(anchor, context, &mut *self.anchors.borrow_mut())
    }

    fn held(&self) -> Vec<HeldAnchor> {
        anchors_in(
            &mut *self.anchors.borrow_mut(),
            log(),
            repository_id(),
            ReadAccess::Granted,
        )
        .unwrap()
        .unwrap()
    }
}

fn register_text() -> String {
    let mut register =
        Register::with_owner(Identity::human("daniel").unwrap(), daniel().public()).unwrap();
    register
        .add_human(
            Identity::human("mia").unwrap(),
            Role::Member,
            mia().public(),
        )
        .unwrap();
    register.encode()
}

fn anchor_for(signer: &Seeded, hub: &str, tip: Point, confirmed: Option<Point>) -> Anchor {
    AnchorPayload::try_new(
        HubAddress::try_new(hub).unwrap(),
        repository_id(),
        log(),
        tip,
        confirmed,
        signer.public(),
    )
    .unwrap()
    .sign(signer)
    .unwrap()
}

fn anchor(tip: Point, confirmed: Option<Point>) -> Anchor {
    anchor_for(&mia(), ADDRESS, tip, confirmed)
}

#[test]
fn an_anchor_is_confirmed_with_the_time_and_the_number_of_the_last_record_entry() {
    let hub = Hub::new();
    let first = hub.send(&anchor(point(3, 0x03), None)).unwrap();
    assert!(first.is_confirmed());
    assert_eq!(first.number(), 1);
    assert_eq!(first.record(), 1);
    assert_eq!(first.time(), START);
    assert_eq!(first.lines().actor, Identity::human("mia").unwrap());
    assert_eq!(first.hub_key(), hub_key().public());
    hub.time.set(START + 10);
    let second = hub
        .send(&anchor(point(7, 0x07), Some(point(3, 0x03))))
        .unwrap();
    assert!(second.is_confirmed());
    assert_eq!(second.number(), 2);
    assert_eq!(hub.held(), vec![first.clone(), second.clone()]);

    // An older confirmed anchor sent again, as anyone who reads the list can, is answered with
    // its confirmation and is no contradiction.
    assert_eq!(hub.send(first.anchor()).unwrap(), first);
    assert_eq!(hub.held().len(), 2);

    // The same entry again is the same anchor: its confirmation is returned, nothing is added.
    let again = hub
        .send(&anchor(point(7, 0x07), Some(point(3, 0x03))))
        .unwrap();
    assert_eq!(again, second);
    assert_eq!(hub.held().len(), 2);
}

#[test]
fn a_restored_backup_yields_a_recorded_contradiction_and_blocks_no_ref_update() {
    let hub = Hub::new();
    let confirmed = hub.send(&anchor(point(9, 0x09), None)).unwrap();
    // The clone restored a backup: its log is at entry 5 again.
    let error = hub.send(&anchor(point(5, 0x05), None)).unwrap_err();
    assert!(
        matches!(
            &error,
            AnchorError::Refused(AnchorRefusal::Contradicts { held }) if *held == confirmed.tip()
        ),
        "{error}"
    );
    assert_eq!(
        error.answer(),
        Some(ErrorMessage::AnchorRefused(AnchorRefusal::Contradicts {
            held: confirmed.tip()
        }))
    );
    // The same number with another hash contradicts it as well.
    assert!(matches!(
        hub.send(&anchor(point(9, 0x99), None)),
        Err(AnchorError::Refused(AnchorRefusal::Contradicts { .. }))
    ));
    // The same contradicting anchor again is answered alike and not kept twice.
    assert!(matches!(
        hub.send(&anchor(point(5, 0x05), None)),
        Err(AnchorError::Refused(AnchorRefusal::Contradicts { .. }))
    ));
    let held = hub.held();
    assert_eq!(held.len(), 3);
    assert_eq!(held[1].state(), AnchorState::Contradicts);
    assert_eq!(held[1].tip(), point(5, 0x05));
    assert_eq!(held[2].state(), AnchorState::Contradicts);
    // Every contradicting anchor carries the signature of the actor as evidence.
    assert_eq!(held[1].anchor().payload().key(), &mia().public());

    // A ref update goes on: it counts by its proofs.
    let commit = hub.commit(&[("b.txt", "two\n")]);
    let branch = "heads/feature".parse().unwrap();
    hub.push(vec![
        Move::try_new(branch, None, None, Some(commit), None).unwrap()
    ])
    .unwrap();
    // And a later entry of the log is confirmed against the confirmed anchor.
    let later = hub.send(&anchor(point(12, 0x12), None)).unwrap();
    assert!(later.is_confirmed());
    assert_eq!(later.record(), 2);
}

#[test]
fn an_anchor_for_another_hub_or_repository_or_of_a_stranger_is_refused_and_not_kept() {
    let hub = Hub::new();
    assert!(matches!(
        hub.send(&anchor_for(
            &mia(),
            "https://mirror.example",
            point(1, 1),
            None
        )),
        Err(AnchorError::Refused(AnchorRefusal::Hub { .. }))
    ));
    let other = AnchorPayload::try_new(
        address(),
        "ffffffffffffffffffffffffffffffff".parse().unwrap(),
        log(),
        point(1, 1),
        None,
        mia().public(),
    )
    .unwrap()
    .sign(&mia())
    .unwrap();
    assert!(matches!(
        hub.send(&other),
        Err(AnchorError::Refused(AnchorRefusal::Repository { .. }))
    ));
    assert!(matches!(
        hub.send(&anchor_for(&Seeded::new(9), ADDRESS, point(1, 1), None)),
        Err(AnchorError::Refused(AnchorRefusal::Key { .. }))
    ));
    assert!(hub.held().is_empty());
}

#[test]
fn only_a_bare_repository_with_main_holds_anchors() {
    let dir = TempDir::new();
    let clone = Repository::init(
        dir.path().join("clone"),
        &crate::repository::InitOptions::default(),
        &Actor::human("daniel").unwrap(),
        &AllowAll,
    )
    .unwrap();
    let hub = address();
    let signer = hub_key();
    let mut record = Memory::default();
    let mut anchors = Memory::default();
    let context = AcceptContext::new(&hub, repository_id(), &mut record, START, &signer);
    assert!(matches!(
        clone.accept_anchor(&anchor(point(1, 1), None), context, &mut anchors),
        Err(AnchorError::NotBare { .. })
    ));
    let empty = Repository::init_bare(
        dir.path().join("hub"),
        &Actor::human("op").unwrap(),
        &AllowAll,
    )
    .unwrap();
    let context = AcceptContext::new(&hub, repository_id(), &mut record, START, &signer);
    assert!(matches!(
        empty.accept_anchor(&anchor(point(1, 1), None), context, &mut anchors),
        Err(AnchorError::Refused(AnchorRefusal::NoMain))
    ));
}

#[test]
fn a_damaged_list_a_clock_that_steps_back_and_a_failing_append_are_failures_of_the_hub() {
    let hub = Hub::new();
    hub.send(&anchor(point(3, 0x03), None)).unwrap();
    hub.time.set(START - 1);
    assert!(matches!(
        hub.send(&anchor(point(4, 0x04), None)),
        Err(AnchorError::Time { .. })
    ));
    hub.time.set(START);
    hub.anchors.borrow_mut().fail_append = true;
    let error = hub.send(&anchor(point(4, 0x04), None)).unwrap_err();
    assert!(error.io_error().is_some(), "{error}");
    assert_eq!(error.answer(), None);
    hub.anchors.borrow_mut().fail_append = false;
    assert_eq!(hub.held().len(), 1);

    // A list that holds the anchor of another log is damage.
    let foreign = anchors_in(
        &mut *hub.anchors.borrow_mut(),
        LogId::from_bytes([0x11; 16]),
        repository_id(),
        ReadAccess::Granted,
    );
    assert!(matches!(foreign, Err(AnchorError::Order { position: 1 })));
    hub.anchors.borrow_mut().bytes.pop();
    assert!(matches!(
        hub.send(&anchor(point(4, 0x04), None)),
        Err(AnchorError::Anchors(_))
    ));
    // A list whose numbers do not follow one another is damage.
    let one = HeldAnchor::seal(
        anchor(point(3, 0x03), None),
        AnchorLines {
            number: 1,
            time: START,
            register: hub
                .repository
                .refs()
                .branch(&"main".parse().unwrap())
                .unwrap()
                .unwrap(),
            record: 1,
            actor: Identity::human("mia").unwrap(),
            state: AnchorState::Confirmed,
        },
        &hub_key(),
    )
    .unwrap();
    let renumbered = HeldAnchor::seal(
        one.anchor().clone(),
        AnchorLines {
            number: 3,
            ..one.lines().clone()
        },
        &hub_key(),
    )
    .unwrap();
    let mut bytes = one.encode();
    bytes.extend_from_slice(&renumbered.encode());
    let mut gap = Memory {
        bytes,
        ..Memory::default()
    };
    assert!(matches!(
        anchors_in(&mut gap, log(), repository_id(), ReadAccess::Granted),
        Err(AnchorError::Order { position: 2 })
    ));
    // A reader who may not read the repository gets nothing.
    assert!(anchors_in(
        &mut *hub.anchors.borrow_mut(),
        log(),
        repository_id(),
        ReadAccess::Denied
    )
    .unwrap()
    .is_none());
}
