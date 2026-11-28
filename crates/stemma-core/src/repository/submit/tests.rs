//! Tests of the submission of statements and of the reading of the held statements
//! (STEMMA-312), through the transport in memory. The other side is [`Side`]: the answering side
//! of a bare repository for the list of references, and a hub in memory for the statements,
//! which holds what it is sent, signs its lines with a key of its own, and answers a statement
//! that it holds already with the lines that it holds it with, as `accept_statement` does. Its
//! rules are not those of a hub: the tests of `accept_statement` judge them.

use std::cell::{Cell, RefCell};
use std::fs;

use super::*;
use crate::change::ChangeId;
use crate::index::Index;
use crate::object::{Blob, Commit, FileMode, ObjectId};
use crate::oplog::CheckResult;
use crate::protocol::held::HubLines;
use crate::protocol::record::tests::Seeded;
use crate::protocol::record::HubKeys;
use crate::protocol::{FormerKeys, RecordHash};
use crate::refs::{encode_branch, BranchName};
use crate::register::Identity;
use crate::remote::RemoteAddress;
use crate::repository::{InitOptions, RemoteSetup};
use crate::serve::{Answering, ListFacts};
use crate::store::ObjectStore;
use crate::testutil::TempDir;
use crate::transport::{MemoryTransport, OtherSide, Refusal};
use crate::AllowAll;

fn daniel() -> Actor {
    Actor::human("daniel").unwrap()
}

fn mia() -> Actor {
    Actor::human("mia").unwrap()
}

fn origin() -> RemoteName {
    RemoteName::try_new("origin").unwrap()
}

fn topic() -> BranchName {
    BranchName::try_new("topic").unwrap()
}

fn hub_key() -> Seeded {
    Seeded::new(2)
}

fn other_key() -> Seeded {
    Seeded::new(7)
}

fn repository_id() -> RepositoryId {
    "11".repeat(16).parse().unwrap()
}

fn other_id() -> RepositoryId {
    "22".repeat(16).parse().unwrap()
}

/// The keys of the hub, leaked so that the facts can borrow them.
fn keys() -> &'static HubKeys {
    Box::leak(Box::new(
        HubKeys::try_new(hub_key().public(), FormerKeys::none()).unwrap(),
    ))
}

fn record() -> RecordHash {
    format!("b3:{}", "a1".repeat(32)).parse().unwrap()
}

/// The other side: the list of references of a bare repository, and the statements in memory.
struct Side<'a> {
    answering: Answering<'a>,
    held: RefCell<Vec<HeldStatement>>,
    /// The reason that the next submission is refused with, once.
    refuse: RefCell<Option<StatementRefusal>>,
    /// The submissions after which the side fails, for the one after them.
    fail_after: Cell<Option<usize>>,
    /// The key that the side signs its lines with.
    signer: Seeded,
    /// The record entry that the lines of a new held statement name.
    record: Cell<u64>,
    /// The statement that the side holds in place of the next one it is sent, once.
    answer_other: RefCell<Option<Statement>>,
    submissions: Cell<usize>,
}

impl OtherSide for Side<'_> {
    fn refs(&self) -> Result<crate::protocol::RefList, Refusal> {
        self.answering.refs()
    }

    fn fetch(
        &self,
        request: &crate::protocol::FetchRequest,
        stream: &mut dyn std::io::Write,
    ) -> Result<(), Refusal> {
        self.answering.fetch(request, stream)
    }

    fn submit_statement(&self, statement: &Statement) -> Result<HeldStatement, Refusal> {
        let count = self.submissions.get();
        if self.fail_after.get() == Some(count) {
            return Err(Refusal::Failed("the hub stopped".to_owned()));
        }
        self.submissions.set(count + 1);
        if let Some(reason) = self.refuse.borrow_mut().take() {
            return Err(Refusal::Error(ErrorMessage::StatementRefused(reason)));
        }
        let statement = self
            .answer_other
            .borrow_mut()
            .take()
            .unwrap_or_else(|| statement.clone());
        let mut held = self.held.borrow_mut();
        if let Some(found) = held.iter().find(|held| *held.statement() == statement) {
            return Ok(found.clone());
        }
        let lines = HubLines {
            number: held.len() as u64 + 1,
            time: 1_000 + held.len() as i64,
            register: CommitId::new(ObjectId::from_digest(blake3::hash(b"main"))),
            record: self.record.get(),
            actor: Identity::human("daniel").unwrap(),
        };
        let sealed = HeldStatement::seal(statement, lines, &self.signer).unwrap();
        held.push(sealed.clone());
        Ok(sealed)
    }

    fn statements(&self, range: &Range) -> Result<Vec<HeldStatement>, Refusal> {
        Ok(self
            .held
            .borrow()
            .iter()
            .filter(|held| range.contains(held.number()))
            .cloned()
            .collect())
    }
}

/// A hub with one commit on `main`, and a clone of it with the branch `topic` at that commit.
struct Setup {
    _dir: TempDir,
    hub: Repository,
    clone: Repository,
    commit: CommitId,
}

impl Setup {
    fn new() -> Self {
        let dir = TempDir::new();
        let hub = Repository::init_bare(dir.path().join("hub"), &daniel(), &AllowAll).unwrap();
        let objects = hub.objects();
        let mut index = Index::new();
        let blob = objects.put_blob(&Blob::new("hello\n")).unwrap();
        index.insert("file.txt", FileMode::File, blob).unwrap();
        let commit = Commit::try_new(
            index.write_tree(objects).unwrap(),
            Vec::new(),
            ChangeId::generate().unwrap(),
            daniel(),
            1,
            "first",
        )
        .unwrap();
        let commit = objects.put_commit(&commit).unwrap();
        let path = hub.metadata_dir().join("refs").join("heads/main");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, encode_branch(&commit)).unwrap();
        let (clone, _) = {
            let side = side(&hub);
            Repository::clone_remote(
                dir.path().join("clone"),
                &InitOptions::default(),
                &origin(),
                &RemoteSetup::new(
                    RemoteAddress::try_new("https://hub.example/ns/project").unwrap(),
                ),
                &mut MemoryTransport::new(side),
                &daniel(),
                &AllowAll,
            )
            .unwrap()
        };
        clone
            .create_branch(
                &OperationName::try_new("branch").unwrap(),
                &topic(),
                commit.object_id(),
                &daniel(),
                &AllowAll,
            )
            .unwrap();
        Self {
            _dir: dir,
            hub,
            clone,
            commit,
        }
    }

    /// Opens a pull request for `topic` for the repository of the hub, and returns its number.
    fn open(&self) -> u64 {
        self.clone
            .open_pull_for(
                repository_id(),
                &topic(),
                &daniel(),
                Vec::new(),
                None,
                None,
                None,
                &AllowAll,
            )
            .unwrap()
            .number()
    }
}

fn side(hub: &Repository) -> Side<'_> {
    side_with(hub, keys(), hub_key())
}

/// A side with these keys in its list, which signs with `signer`.
fn side_with<'a>(hub: &'a Repository, keys: &'static HubKeys, signer: Seeded) -> Side<'a> {
    side_of(hub, repository_id(), keys, signer)
}

/// A side whose list names the repository `id` and these keys, which signs with `signer`.
fn side_of<'a>(
    hub: &'a Repository,
    id: RepositoryId,
    keys: &'static HubKeys,
    signer: Seeded,
) -> Side<'a> {
    let facts = ListFacts::new(id, keys, |_| Some(record()));
    Side {
        answering: Answering::new(hub, facts),
        held: RefCell::new(Vec::new()),
        refuse: RefCell::new(None),
        fail_after: Cell::new(None),
        signer,
        record: Cell::new(1),
        answer_other: RefCell::new(None),
        submissions: Cell::new(0),
    }
}

/// The hashes of the statements of the pull request `number`.
fn hashes_of(clone: &Repository, number: u64) -> Vec<StatementHash> {
    clone
        .statements_of_pull(number, None)
        .unwrap()
        .iter()
        .map(Statement::hash)
        .collect()
}

/// A side whose hub rotated to [`other_key`], with [`hub_key`] as the former key of the record
/// entries `first` to `last`, which signs with the former key at `record`; the clone pins the
/// new key.
fn rotated<'a>(setup: &'a Setup, first: u64, last: u64, record: u64) -> MemoryTransport<Side<'a>> {
    let new = other_key();
    let former = FormerKeys::sign(
        repository_id(),
        &new.public(),
        vec![crate::protocol::FormerKey::try_new(first, last, hub_key().public()).unwrap()],
        &new,
    )
    .unwrap();
    let keys: &'static HubKeys =
        Box::leak(Box::new(HubKeys::try_new(new.public(), former).unwrap()));
    let side = side_with(&setup.hub, keys, hub_key());
    side.record.set(record);
    let mut transport = MemoryTransport::new(side);
    let list = read_ref_list(&mut transport).unwrap();
    setup
        .clone
        .pin_hub_key_again(
            &origin(),
            &list,
            &new.public().fingerprint(),
            &daniel(),
            &AllowAll,
        )
        .unwrap();
    transport
}

fn kinds(clone: &Repository, hashes: &[StatementHash]) -> Vec<StatementKind> {
    hashes
        .iter()
        .map(|hash| clone.statement_set().get(hash).unwrap().unwrap().kind())
        .collect()
}

#[test]
fn an_opening_for_a_hub_names_its_repository_and_the_later_statements_follow_it() {
    let setup = Setup::new();
    let number = setup.open();
    let clone = &setup.clone;
    clone
        .approve_pull(number, &mia(), None, None, &AllowAll)
        .unwrap();
    clone
        .close_pull(number, &daniel(), None, None, &AllowAll)
        .unwrap();
    let pulls = clone.pull_requests(None).unwrap();
    assert_eq!(pulls[0].repository(), Some(repository_id()));
    let hashes = clone
        .statements_of_pull(number, None)
        .unwrap()
        .iter()
        .map(Statement::hash)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds(clone, &hashes),
        [
            StatementKind::Opening,
            StatementKind::Approval,
            StatementKind::Closing
        ]
    );
    assert_eq!(hashes[0], pulls[0].id());
    for hash in &hashes {
        let statement = clone.statement_set().get(hash).unwrap().unwrap();
        assert_eq!(statement.body().repository(), Some(repository_id()));
    }
}

#[test]
fn a_pull_request_of_this_clone_only_is_not_sent() {
    let setup = Setup::new();
    let entry = setup
        .clone
        .open_pull(&topic(), &daniel(), Vec::new(), None, None, None, &AllowAll)
        .unwrap();
    // Its statements name no hub, and no hub accepts them.
    let hashes = setup
        .clone
        .statements_of_pull(entry.number(), None)
        .unwrap()
        .iter()
        .map(Statement::hash)
        .collect::<Vec<_>>();
    assert_eq!(hashes.len(), 1);
    let error = setup
        .clone
        .submit_statements(
            &origin(),
            &hashes,
            &mut MemoryTransport::new(side(&setup.hub)),
            &daniel(),
            &AllowAll,
        )
        .unwrap_err();
    assert!(
        matches!(error, SubmitError::OtherRepository { named: None, .. }),
        "{error}"
    );
    let error = setup.clone.statements_of_pull(99, None).unwrap_err();
    assert!(matches!(error, SubmitError::NoSuchPull(99)));
}

#[test]
fn the_checks_of_a_head_belong_to_its_pull_request() {
    let setup = Setup::new();
    let number = setup.open();
    let clone = &setup.clone;
    clone
        .record_check_for(
            repository_id(),
            setup.commit,
            None,
            CheckResult::Pass,
            &mia(),
            None,
            None,
            &AllowAll,
        )
        .unwrap();
    // A check that counts in this clone only is not sent to the hub.
    clone
        .record_check(
            setup.commit,
            None,
            CheckResult::Fail,
            &mia(),
            None,
            None,
            &AllowAll,
        )
        .unwrap();
    let hashes = clone
        .statements_of_pull(number, None)
        .unwrap()
        .iter()
        .map(Statement::hash)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds(clone, &hashes),
        [StatementKind::Opening, StatementKind::Check]
    );
}

#[test]
fn a_submission_is_held_and_a_repeated_one_gets_the_same_lines() {
    let setup = Setup::new();
    let number = setup.open();
    let clone = &setup.clone;
    clone
        .approve_pull(number, &mia(), None, None, &AllowAll)
        .unwrap();
    let hashes = clone
        .statements_of_pull(number, None)
        .unwrap()
        .iter()
        .map(Statement::hash)
        .collect::<Vec<_>>();
    let mut transport = MemoryTransport::new(side(&setup.hub));
    let first = clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap();
    let numbers = |submitted: &[Submission]| -> Vec<u64> {
        submitted
            .iter()
            .map(|submission| match &submission.outcome {
                Outcome::Held(held) => held.number(),
                Outcome::Refused(reason) => panic!("{reason}"),
            })
            .collect()
    };
    assert_eq!(numbers(&first), [1, 2]);
    assert_eq!(first[0].hash, hashes[0]);
    assert_eq!(first[1].kind, StatementKind::Approval);
    let again = clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap();
    assert_eq!(numbers(&again), [1, 2]);
    // The reading gives the same statements with the same lines.
    let held = clone
        .held_statements(&origin(), &mut transport, &daniel(), &AllowAll)
        .unwrap();
    assert_eq!(held.len(), 2);
    assert_eq!(held[0].hash(), hashes[0]);
}

#[test]
fn a_refusal_is_an_answer_and_the_next_statement_is_sent() {
    let setup = Setup::new();
    let number = setup.open();
    let clone = &setup.clone;
    clone
        .approve_pull(number, &mia(), None, None, &AllowAll)
        .unwrap();
    let hashes = clone
        .statements_of_pull(number, None)
        .unwrap()
        .iter()
        .map(Statement::hash)
        .collect::<Vec<_>>();
    let side = side(&setup.hub);
    *side.refuse.borrow_mut() = Some(StatementRefusal::NotWriteOwner);
    let mut transport = MemoryTransport::new(side);
    let submitted = clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap();
    assert_eq!(
        submitted[0].outcome,
        Outcome::Refused(StatementRefusal::NotWriteOwner)
    );
    assert!(matches!(submitted[1].outcome, Outcome::Held(_)));
}

#[test]
fn a_hub_that_stops_keeps_the_answers_that_came_before() {
    let setup = Setup::new();
    let number = setup.open();
    let clone = &setup.clone;
    clone
        .approve_pull(number, &mia(), None, None, &AllowAll)
        .unwrap();
    let hashes = clone
        .statements_of_pull(number, None)
        .unwrap()
        .iter()
        .map(Statement::hash)
        .collect::<Vec<_>>();
    let side = side(&setup.hub);
    side.fail_after.set(Some(0));
    let mut transport = MemoryTransport::new(side);
    let error = clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap_err();
    // A side that fails before it answers gives no answer of the protocol, which is no
    // unreachable hub: the status of the answer says more about it.
    assert!(matches!(error, SubmitError::Transport(_)), "{error}");
    assert!(!error.is_unreachable());
    let side = self::side(&setup.hub);
    side.fail_after.set(Some(1));
    let mut transport = MemoryTransport::new(side);
    let error = clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap_err();
    assert!(
        matches!(&error, SubmitError::After { source, .. } if matches!(**source, SubmitError::Transport(_))),
        "{error}"
    );
    assert_eq!(error.submitted().len(), 1);
    assert!(error
        .to_string()
        .contains("after the hub had answered 1 statement"));
}

#[test]
fn a_statement_for_another_repository_or_none_is_not_sent() {
    let setup = Setup::new();
    let clone = &setup.clone;
    let other = clone
        .record_check_for(
            other_id(),
            setup.commit,
            None,
            CheckResult::Pass,
            &mia(),
            None,
            None,
            &AllowAll,
        )
        .unwrap();
    let local = clone
        .record_check(
            setup.commit,
            None,
            CheckResult::Fail,
            &mia(),
            None,
            None,
            &AllowAll,
        )
        .unwrap();
    for entry in [other, local] {
        let hash = entry.statement().unwrap();
        let mut transport = MemoryTransport::new(side(&setup.hub));
        let error = clone
            .submit_statements(&origin(), &[hash], &mut transport, &daniel(), &AllowAll)
            .unwrap_err();
        assert!(
            matches!(error, SubmitError::OtherRepository { hash: h, .. } if h == hash),
            "{error}"
        );
        assert_eq!(transport.side().submissions.get(), 0);
    }
    let error = clone
        .submit_statements(
            &RemoteName::try_new("nowhere").unwrap(),
            &[],
            &mut MemoryTransport::new(side(&setup.hub)),
            &daniel(),
            &AllowAll,
        )
        .unwrap_err();
    assert!(matches!(error, SubmitError::NoSuchRemote(_)), "{error}");
}

#[test]
fn a_held_statement_signed_by_another_key_is_refused() {
    let setup = Setup::new();
    let number = setup.open();
    let clone = &setup.clone;
    let hashes = clone
        .statements_of_pull(number, None)
        .unwrap()
        .iter()
        .map(Statement::hash)
        .collect::<Vec<_>>();
    let mut side = side(&setup.hub);
    side.signer = other_key();
    let mut transport = MemoryTransport::new(side);
    let error = clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap_err();
    assert!(
        matches!(error, SubmitError::WrongHubKey(h) if h == hashes[0]),
        "{error}"
    );
    // The reading checks the same: the side holds the statement now, signed by the other key.
    let error = clone
        .held_statements(&origin(), &mut transport, &daniel(), &AllowAll)
        .unwrap_err();
    assert!(matches!(error, SubmitError::WrongHubKey(_)), "{error}");
}

#[test]
fn the_authorizer_is_asked_before_anything_is_sent() {
    struct Deny;
    impl Authorizer for Deny {
        fn authorize(&self, _: &Actor, _: &OperationName, _: &str) -> Decision {
            Decision::Deny
        }
    }
    let setup = Setup::new();
    let number = setup.open();
    let hashes = setup
        .clone
        .statements_of_pull(number, None)
        .unwrap()
        .iter()
        .map(Statement::hash)
        .collect::<Vec<_>>();
    let mut transport = MemoryTransport::new(side(&setup.hub));
    let error = setup
        .clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &Deny)
        .unwrap_err();
    assert!(
        matches!(&error, SubmitError::Denied { operation, target }
            if operation == SUBMIT && target == "remote origin"),
        "{error}"
    );
    let error = setup
        .clone
        .held_statements(&origin(), &mut transport, &daniel(), &Deny)
        .unwrap_err();
    assert!(matches!(error, SubmitError::Denied { .. }), "{error}");
    assert_eq!(transport.side().submissions.get(), 0);
}

#[test]
fn a_repeated_opening_of_a_pull_request_that_ended_is_no_refusal_that_counts() {
    let setup = Setup::new();
    let number = setup.open();
    let clone = &setup.clone;
    clone
        .close_pull(number, &daniel(), None, None, &AllowAll)
        .unwrap();
    let hashes: Vec<_> = clone
        .statements_of_pull(number, None)
        .unwrap()
        .iter()
        .map(Statement::hash)
        .collect();
    let side = side(&setup.hub);
    // The hub answers the opening of a pull request that ended with `not-open`, as
    // `accept_statement` does for the same opening again.
    *side.refuse.borrow_mut() = Some(StatementRefusal::NotOpen);
    let mut transport = MemoryTransport::new(side);
    let submitted = clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap();
    assert_eq!(
        submitted[0].outcome,
        Outcome::Refused(StatementRefusal::NotOpen)
    );
    assert!(!submitted[0].is_refused());
    assert!(!submitted[1].is_refused());
    // `not-open` for a closing is a refusal that counts.
    let closing = Submission {
        outcome: Outcome::Refused(StatementRefusal::NotOpen),
        ..submitted[1].clone()
    };
    assert!(closing.is_refused());
}

#[test]
fn an_answer_longer_than_its_bound_names_the_bound() {
    let error = SubmitError::TooLarge { bound: 7 };
    assert!(error.to_string().contains("7 bytes"), "{error}");
}

#[test]
fn after_a_rotation_a_statement_held_before_it_counts_with_the_former_key() {
    let setup = Setup::new();
    let number = setup.open();
    let clone = &setup.clone;
    let hashes: Vec<_> = clone
        .statements_of_pull(number, None)
        .unwrap()
        .iter()
        .map(Statement::hash)
        .collect();
    // The old key holds the opening, at record 1.
    let old = MemoryTransport::new(side(&setup.hub));
    let mut old = old;
    clone
        .submit_statements(&origin(), &hashes, &mut old, &daniel(), &AllowAll)
        .unwrap();
    let held = old.side().held.borrow().clone();
    // The hub rotates: the new key is current, and the old one signed the record entry 1.
    let new = other_key();
    let former = FormerKeys::sign(
        repository_id(),
        &new.public(),
        vec![crate::protocol::FormerKey::try_new(1, 1, hub_key().public()).unwrap()],
        &new,
    )
    .unwrap();
    let rotated: &'static HubKeys =
        Box::leak(Box::new(HubKeys::try_new(new.public(), former).unwrap()));
    let side = side_with(&setup.hub, rotated, other_key());
    *side.held.borrow_mut() = held;
    let mut transport = MemoryTransport::new(side);
    let list = read_ref_list(&mut transport).unwrap();
    clone
        .pin_hub_key_again(
            &origin(),
            &list,
            &new.public().fingerprint(),
            &daniel(),
            &AllowAll,
        )
        .unwrap();
    // The repeated opening comes back with the signature of the old key, which counts for it.
    let submitted = clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap();
    assert!(matches!(submitted[0].outcome, Outcome::Held(_)));
    let read = clone
        .held_statements(&origin(), &mut transport, &daniel(), &AllowAll)
        .unwrap();
    assert_eq!(read.len(), 1);
}

#[test]
fn a_former_key_does_not_count_outside_its_range() {
    let setup = Setup::new();
    let number = setup.open();
    let clone = &setup.clone;
    let hashes: Vec<_> = clone
        .statements_of_pull(number, None)
        .unwrap()
        .iter()
        .map(Statement::hash)
        .collect();
    // The old key signed only the record entries 5 to 6, and the side signs lines of record 1
    // with it: a stolen former key cannot make held statements of another time.
    let new = other_key();
    let former = FormerKeys::sign(
        repository_id(),
        &new.public(),
        vec![crate::protocol::FormerKey::try_new(5, 6, hub_key().public()).unwrap()],
        &new,
    )
    .unwrap();
    let rotated: &'static HubKeys =
        Box::leak(Box::new(HubKeys::try_new(new.public(), former).unwrap()));
    let mut transport = MemoryTransport::new(side_with(&setup.hub, rotated, hub_key()));
    let list = read_ref_list(&mut transport).unwrap();
    clone
        .pin_hub_key_again(
            &origin(),
            &list,
            &new.public().fingerprint(),
            &daniel(),
            &AllowAll,
        )
        .unwrap();
    let error = clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap_err();
    assert!(matches!(error, SubmitError::WrongHubKey(_)), "{error}");
}

#[test]
fn a_former_key_counts_for_the_record_entry_just_before_its_range_and_no_earlier() {
    // A held statement names the last record entry before it, and the key that signed it may
    // already be the key of the next entry: the former key of 2 to 3 counts for record 1.
    let setup = Setup::new();
    let hashes = hashes_of(&setup.clone, setup.open());
    let mut transport = rotated(&setup, 2, 3, 1);
    let submitted = setup
        .clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap();
    assert!(matches!(submitted[0].outcome, Outcome::Held(_)));

    // The former key of 3 to 4 does not count for record 1.
    let setup = Setup::new();
    let hashes = hashes_of(&setup.clone, setup.open());
    let mut transport = rotated(&setup, 3, 4, 1);
    let error = setup
        .clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap_err();
    assert!(matches!(error, SubmitError::WrongHubKey(_)), "{error}");
}

#[test]
fn a_hub_that_holds_another_statement_than_the_one_it_was_sent_is_refused() {
    let setup = Setup::new();
    let number = setup.open();
    let clone = &setup.clone;
    clone
        .approve_pull(number, &mia(), None, None, &AllowAll)
        .unwrap();
    let hashes = hashes_of(clone, number);
    let approval = clone.statement_set().get(&hashes[1]).unwrap().unwrap();
    let side = side(&setup.hub);
    *side.answer_other.borrow_mut() = Some(approval.clone());
    let mut transport = MemoryTransport::new(side);
    let error = clone
        .submit_statements(
            &origin(),
            &hashes[..1],
            &mut transport,
            &daniel(),
            &AllowAll,
        )
        .unwrap_err();
    assert!(
        matches!(error, SubmitError::NotSubmitted(hash) if hash == hashes[0]),
        "{error}"
    );
}

#[test]
fn a_list_of_another_repository_or_key_refuses_before_anything_is_sent() {
    let setup = Setup::new();
    let hashes = hashes_of(&setup.clone, setup.open());
    let mut transport = MemoryTransport::new(side_of(&setup.hub, other_id(), keys(), hub_key()));
    let error = setup
        .clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap_err();
    assert!(matches!(error, SubmitError::HubKey(_)), "{error}");
    assert_eq!(transport.side().submissions.get(), 0);

    let other: &'static HubKeys = Box::leak(Box::new(
        HubKeys::try_new(other_key().public(), FormerKeys::none()).unwrap(),
    ));
    let mut transport = MemoryTransport::new(side_with(&setup.hub, other, other_key()));
    let error = setup
        .clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap_err();
    assert!(matches!(error, SubmitError::HubKey(_)), "{error}");
    assert_eq!(transport.side().submissions.get(), 0);
    let error = setup
        .clone
        .held_statements(&origin(), &mut transport, &daniel(), &AllowAll)
        .unwrap_err();
    assert!(matches!(error, SubmitError::HubKey(_)), "{error}");
}

#[test]
fn a_held_statement_of_another_repository_is_refused_when_read() {
    let setup = Setup::new();
    let clone = &setup.clone;
    let other = clone
        .open_pull_for(
            other_id(),
            &topic(),
            &daniel(),
            Vec::new(),
            None,
            None,
            None,
            &AllowAll,
        )
        .unwrap()
        .number();
    let hash = hashes_of(clone, other)[0];
    let statement = clone.statement_set().get(&hash).unwrap().unwrap();
    let lines = HubLines {
        number: 1,
        time: 1_000,
        register: CommitId::new(ObjectId::from_digest(blake3::hash(b"main"))),
        record: 1,
        actor: Identity::human("daniel").unwrap(),
    };
    let side = side(&setup.hub);
    side.held
        .borrow_mut()
        .push(HeldStatement::seal(statement, lines, &hub_key()).unwrap());
    let mut transport = MemoryTransport::new(side);
    let error = clone
        .held_statements(&origin(), &mut transport, &daniel(), &AllowAll)
        .unwrap_err();
    assert!(
        matches!(error, SubmitError::NotOfRemote(found) if found == hash),
        "{error}"
    );
}

#[test]
fn only_a_failure_of_the_carrier_or_of_a_body_is_an_unreachable_hub() {
    let carrier = || TransportError::Carrier(io::Error::other("refused"));
    let no_answer = || TransportError::NoAnswer {
        detail: "503, with a body of the media type `text/html`".to_owned(),
    };
    assert!(SubmitError::Transport(carrier()).is_unreachable());
    assert!(SubmitError::Body(io::Error::other("broke off")).is_unreachable());
    assert!(SubmitError::List(Box::new(FetchError::Transport(carrier()))).is_unreachable());
    assert!(!SubmitError::Transport(no_answer()).is_unreachable());
    assert!(!SubmitError::List(Box::new(FetchError::Transport(no_answer()))).is_unreachable());
    assert!(!SubmitError::NotSubmitted(record_hash()).is_unreachable());
    let after = SubmitError::After {
        submitted: Vec::new(),
        source: Box::new(SubmitError::Transport(carrier())),
    };
    assert!(after.is_unreachable());
}

fn record_hash() -> StatementHash {
    format!("b3:{}", "b2".repeat(32)).parse().unwrap()
}
