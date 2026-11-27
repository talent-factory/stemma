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
        let mut held = self.held.borrow_mut();
        if let Some(found) = held.iter().find(|held| held.statement() == statement) {
            return Ok(found.clone());
        }
        let lines = HubLines {
            number: held.len() as u64 + 1,
            time: 1_000 + held.len() as i64,
            register: CommitId::new(ObjectId::from_digest(blake3::hash(b"main"))),
            record: 1,
            actor: Identity::human("daniel").unwrap(),
        };
        let sealed = HeldStatement::seal(statement.clone(), lines, &self.signer).unwrap();
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
    let facts = ListFacts::new(repository_id(), keys(), |_| Some(record()));
    Side {
        answering: Answering::new(hub, facts),
        held: RefCell::new(Vec::new()),
        refuse: RefCell::new(None),
        fail_after: Cell::new(None),
        signer: hub_key(),
        submissions: Cell::new(0),
    }
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
    assert!(matches!(error, SubmitError::Transport(_)), "{error}");
    assert!(error.is_unreachable());
    let side = self::side(&setup.hub);
    side.fail_after.set(Some(1));
    let mut transport = MemoryTransport::new(side);
    let error = clone
        .submit_statements(&origin(), &hashes, &mut transport, &daniel(), &AllowAll)
        .unwrap_err();
    assert!(error.is_unreachable());
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
