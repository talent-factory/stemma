//! The tests of the accepting operation for a statement (ADR-025). A clone reaches the hub
//! through [`MemoryTransport`]: it submits a signed statement and reads the held statement or
//! the error, and it reads the statements and the record by a range. The other side is [`Hub`],
//! a bare repository with its record and its statements in memory, which answers with
//! [`Repository::accept_statement`], [`held_in`], and [`record_in`]. The references that the
//! statements speak about are set by ref updates through [`Repository::accept_ref_update`].

use std::cell::{Cell, RefCell};
use std::io::Read;

use super::*;
use crate::change::ChangeId;
use crate::index::Index;
use crate::object::{Blob, Commit, FileMode};
use crate::oplog::CheckResult;
use crate::policy;
use crate::protocol::held::HeldProblem;
use crate::protocol::record::tests::Seeded;
use crate::protocol::record::{HubAddress, Move, RefUpdatePayload};
use crate::protocol::statement::{Approval, Checked, Closing, Opening, OwnerOverride};
use crate::protocol::{ParseError, RepositoryId, VERSION};
use crate::register::{self, Role};
use crate::store::ObjectStore;
use crate::testutil::TempDir;
use crate::transport::{Answer, MemoryTransport, OtherSide, Refusal, Transport};
use crate::{Actor, Decision};

const ADDRESS: &str = "https://hub.example";
const REPOSITORY: &str = "00112233445566778899aabbccddeeff";
const POLICY: &str = "stemma-policy 1\nlease 30\n";
const LEASE: i64 = 30 * 60;
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

fn stranger() -> Seeded {
    Seeded::new(5)
}

fn human(id: &str) -> Identity {
    Identity::human(id).unwrap()
}

fn repository_id() -> RepositoryId {
    REPOSITORY.parse().unwrap()
}

fn branch(name: &str) -> BranchName {
    BranchName::try_new(name).unwrap()
}

fn name(text: &str) -> QualifiedName {
    text.parse().unwrap()
}

/// The register of these tests: Daniel owns it, Mia and Ana are members.
fn register_of() -> Register {
    let mut register = Register::with_owner(human("daniel"), daniel().public()).unwrap();
    register
        .add_human(human("mia"), Role::Member, mia().public())
        .unwrap();
    register
        .add_human(human("ana"), Role::Member, ana().public())
        .unwrap();
    register
}

/// A destination in memory, which can be told to fail, for the record and the statements.
#[derive(Default)]
struct Memory {
    bytes: Vec<u8>,
    fail_read: bool,
    fail_append: bool,
}

impl Memory {
    fn read_bytes(&self) -> io::Result<Vec<u8>> {
        if self.fail_read {
            return Err(io::Error::other("the destination cannot be read"));
        }
        Ok(self.bytes.clone())
    }
}

impl RecordDestination for Memory {
    fn read(&mut self) -> io::Result<Vec<u8>> {
        self.read_bytes()
    }

    fn append(&mut self, entry: &[u8]) -> io::Result<()> {
        self.bytes.extend_from_slice(entry);
        Ok(())
    }
}

impl StatementDestination for Memory {
    fn read(&mut self) -> io::Result<Vec<u8>> {
        self.read_bytes()
    }

    fn append(&mut self, held: &[u8]) -> io::Result<()> {
        if self.fail_append {
            return Err(io::Error::other("the disk is full"));
        }
        self.bytes.extend_from_slice(held);
        Ok(())
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

/// The other side of these tests: a bare repository, its record, its statements, the clock of
/// the hub, and whether the requester may read.
struct Hub {
    _dir: TempDir,
    repository: Repository,
    record: RefCell<Memory>,
    statements: RefCell<Memory>,
    time: Cell<i64>,
    access: Cell<ReadAccess>,
    also: Cell<Option<&'static str>>,
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
            statements: RefCell::default(),
            time: Cell::new(START),
            access: Cell::new(ReadAccess::Granted),
            also: Cell::new(None),
        }
    }

    fn accept(&self, statement: &Statement) -> Result<HeldStatement, StatementError> {
        let hub = HubAddress::try_new(ADDRESS).unwrap();
        let signer = hub_key();
        let mut record = self.record.borrow_mut();
        let mut statements = self.statements.borrow_mut();
        let context = AcceptContext::new(
            &hub,
            repository_id(),
            &mut *record,
            self.time.get(),
            &signer,
        );
        let also: Box<dyn Authorizer> = match self.also.get() {
            Some(denied) => Box::new(Deny(denied)),
            None => Box::new(AllowAll),
        };
        self.repository
            .accept_statement_with(statement, context, &mut *statements, &*also)
    }

    /// Moves references by a ref update of `signer`, as a push does.
    fn push(&self, signer: &Seeded, moves: Vec<(&str, Option<CommitId>)>) -> RecordEntry {
        let record = read_record(&self.record.borrow().bytes).unwrap();
        let moves = moves
            .into_iter()
            .map(|(reference, new)| {
                let reference = name(reference);
                let last = record.iter().rev().find_map(|entry| {
                    entry
                        .update()
                        .payload()
                        .moves()
                        .iter()
                        .find(|moved| moved.name() == &reference)
                        .map(|moved| (entry.hash(), moved.new_commit()))
                });
                let (previous, old) = last.map_or((None, None), |(hash, old)| (Some(hash), old));
                Move::try_new(reference, previous, old, new, None).unwrap()
            })
            .collect();
        let update = RefUpdatePayload::try_new(
            HubAddress::try_new(ADDRESS).unwrap(),
            repository_id(),
            moves,
            signer.public(),
        )
        .unwrap()
        .sign(signer)
        .unwrap();
        let hub = HubAddress::try_new(ADDRESS).unwrap();
        let key = hub_key();
        let mut record = self.record.borrow_mut();
        let context =
            AcceptContext::new(&hub, repository_id(), &mut *record, self.time.get(), &key);
        self.repository.accept_ref_update(&update, context).unwrap()
    }

    /// Stores a commit of `author` with these files and parents, as an upload does.
    fn commit(&self, author: &str, files: &[(&str, &str)], parents: Vec<CommitId>) -> CommitId {
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
            Actor::human(author).unwrap(),
            1,
            "a commit",
        )
        .unwrap();
        store.put_commit(&commit).unwrap()
    }

    /// A commit of `author` that holds this register and the policy, and one more file.
    fn governed(&self, register: &Register, text: &str, parents: Vec<CommitId>) -> CommitId {
        self.commit(
            "daniel",
            &[
                (register::FILE, &register.encode()),
                (policy::FILE_NAME, POLICY),
                ("a.txt", text),
            ],
            parents,
        )
    }

    fn held(&self) -> Vec<u8> {
        self.statements.borrow().bytes.clone()
    }
}

impl OtherSide for Hub {
    fn refs(&self) -> Result<crate::protocol::RefList, Refusal> {
        Err(Refusal::Failed("these tests list no references".to_owned()))
    }

    fn fetch(
        &self,
        _request: &crate::protocol::FetchRequest,
        _stream: &mut dyn std::io::Write,
    ) -> Result<(), Refusal> {
        Err(Refusal::Failed("these tests fetch nothing".to_owned()))
    }

    fn submit_statement(&self, statement: &Statement) -> Result<HeldStatement, Refusal> {
        self.accept(statement)
            .map_err(|error| match error.answer() {
                Some(message) => Refusal::Error(message),
                None => Refusal::Failed(error.to_string()),
            })
    }

    fn statements(&self, range: &Range) -> Result<Vec<HeldStatement>, Refusal> {
        held_in(&mut *self.statements.borrow_mut(), range, self.access.get()).map_err(answered)
    }

    fn record(&self, range: &Range) -> Result<Vec<RecordEntry>, Refusal> {
        record_in(&mut *self.record.borrow_mut(), range, self.access.get()).map_err(answered)
    }
}

fn answered(error: RangeError) -> Refusal {
    match error.answer() {
        Some(message) => Refusal::Error(message),
        None => Refusal::Failed(error.to_string()),
    }
}

type Wire = MemoryTransport<Hub>;

fn read_all(mut body: crate::transport::Body<'_>) -> Vec<u8> {
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).unwrap();
    bytes
}

/// Submits a statement as a clone does: the held statement, checked against what was sent and
/// the key of the hub, or the error that the hub answered.
fn submit(wire: &mut Wire, statement: &Statement) -> Result<HeldStatement, ErrorMessage> {
    match wire
        .submit_statement(&mut statement.encode().as_slice())
        .unwrap()
    {
        Answer::Message(body) => {
            let held = HeldStatement::parse(&read_all(body)).unwrap();
            assert_eq!(held.statement(), statement);
            assert_eq!(held.hub_key(), hub_key().public());
            Ok(held)
        }
        Answer::Error(body) => Err(ErrorMessage::parse(&read_all(body)).unwrap()),
    }
}

/// The reason of a refused statement.
fn refused(wire: &mut Wire, statement: &Statement) -> StatementRefusal {
    let before = wire.side().held();
    match submit(wire, statement) {
        Err(ErrorMessage::StatementRefused(refusal)) => {
            assert_eq!(wire.side().held(), before, "a refusal wrote a statement");
            refusal
        }
        other => panic!("{other:?}"),
    }
}

fn read_range(wire: &mut Wire, range: Range, record: bool) -> Result<Vec<u8>, ErrorMessage> {
    let mut request = range.encode();
    let answer = if record {
        wire.record(&mut request.as_slice())
    } else {
        wire.statements(&mut request.as_slice())
    };
    request.clear();
    match answer.unwrap() {
        Answer::Message(body) => Ok(read_all(body)),
        Answer::Error(body) => Err(ErrorMessage::parse(&read_all(body)).unwrap()),
    }
}

fn signed(body: Body, signer: &Seeded) -> Statement {
    Statement::sign(body, signer).unwrap()
}

fn opening(branch_name: &str, head: CommitId, reviewers: &[&str], signer: &Seeded) -> Statement {
    let reviewers = reviewers.iter().map(|id| human(id)).collect();
    let opening = Opening::try_new(
        Some(repository_id()),
        branch(branch_name),
        head,
        reviewers,
        None,
    )
    .unwrap();
    signed(Body::Opening(opening), signer)
}

fn approval(pull: StatementHash, head: CommitId, signer: &Seeded) -> Statement {
    let approval = Approval::new(Some(repository_id()), pull, head);
    signed(Body::Approval(approval), signer)
}

fn check(commit: CommitId, result: CheckResult, signer: &Seeded) -> Statement {
    let checked = Checked::try_new(Some(repository_id()), commit, None, result).unwrap();
    signed(Body::Check(checked), signer)
}

fn closing(pull: StatementHash, signer: &Seeded) -> Statement {
    signed(
        Body::Closing(Closing::new(Some(repository_id()), pull)),
        signer,
    )
}

/// A hub after the first push of Daniel and the branch `feature` of Mia, whose commit Ana
/// wrote: `main` at the root, `feature` at the commit on it.
struct Started {
    wire: Wire,
    root: CommitId,
    feature: CommitId,
}

fn started() -> Started {
    let hub = Hub::new();
    let root = hub.governed(&register_of(), "one\n", vec![]);
    hub.push(&daniel(), vec![("heads/main", Some(root))]);
    let feature = hub.commit(
        "ana",
        &[
            (register::FILE, &register_of().encode()),
            (policy::FILE_NAME, POLICY),
            ("a.txt", "two\n"),
        ],
        vec![root],
    );
    hub.push(&mia(), vec![("heads/feature", Some(feature))]);
    Started {
        wire: MemoryTransport::new(hub),
        root,
        feature,
    }
}

/// [`started`] with the pull request of Mia open on `feature`, Daniel as its reviewer.
fn opened() -> (Started, HeldStatement) {
    let mut started = started();
    let open = opening("feature", started.feature, &["daniel"], &mia());
    let held = submit(&mut started.wire, &open).unwrap();
    (started, held)
}

#[test]
fn an_opening_is_held_with_the_lines_of_the_hub_and_read_back_by_a_range() {
    let (mut started, held) = opened();
    assert_eq!(held.number(), 1);
    assert_eq!(held.time(), START);
    assert_eq!(held.register(), started.root);
    assert_eq!(held.record(), 2);
    assert_eq!(held.actor(), &human("mia"));

    let bytes = read_range(&mut started.wire, Range::all(), false).unwrap();
    assert_eq!(read_held(&bytes).unwrap(), std::slice::from_ref(&held));
    let bytes = read_range(&mut started.wire, Range::try_new(2, None).unwrap(), false).unwrap();
    assert!(bytes.is_empty());
    let bytes = read_range(&mut started.wire, Range::try_new(2, Some(2)).unwrap(), true).unwrap();
    let entries = read_record(&bytes).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].number(), 2);
    assert_eq!(entries[0].actor(), &human("mia"));
}

#[test]
fn the_same_statement_is_answered_with_its_lines_and_not_held_twice() {
    let (mut started, held) = opened();
    let before = started.wire.side().held();
    started.wire.side().time.set(START + 60);
    let open = opening("feature", started.feature, &["daniel"], &mia());
    assert_eq!(submit(&mut started.wire, &open).unwrap(), held);
    assert_eq!(started.wire.side().held(), before);
}

#[test]
fn the_form_is_judged_before_the_lock() {
    let mut started = started();
    let feature = started.feature;
    let other = "ffeeddccbbaa99887766554433221100".parse().unwrap();
    let body = Opening::try_new(Some(other), branch("feature"), feature, Vec::new(), None).unwrap();
    assert_eq!(
        refused(&mut started.wire, &signed(Body::Opening(body), &mia())),
        StatementRefusal::Repository {
            id: repository_id()
        }
    );
    let local = Opening::try_new(None, branch("feature"), feature, Vec::new(), None).unwrap();
    assert_eq!(
        refused(&mut started.wire, &signed(Body::Opening(local), &mia())),
        StatementRefusal::Repository {
            id: repository_id()
        }
    );
    let unsigned = Opening::try_new(
        Some(repository_id()),
        branch("feature"),
        feature,
        Vec::new(),
        None,
    )
    .unwrap();
    assert_eq!(
        refused(
            &mut started.wire,
            &Statement::unsigned(Body::Opening(unsigned))
        ),
        StatementRefusal::Unsigned
    );
    let over = OwnerOverride::try_new(
        Some(repository_id()),
        branch("feature"),
        human("mia"),
        Some(feature),
        "Mia is away",
    )
    .unwrap();
    assert_eq!(
        refused(&mut started.wire, &signed(Body::Override(over), &daniel())),
        StatementRefusal::Kind(crate::protocol::statement::StatementKind::Override)
    );
}

#[test]
fn the_key_must_be_an_active_key_of_the_register_at_the_tip_of_main() {
    let mut started = started();
    let open = opening("feature", started.feature, &[], &stranger());
    assert_eq!(
        refused(&mut started.wire, &open),
        StatementRefusal::Key {
            register: started.root
        }
    );
}

#[test]
fn a_repository_without_main_refuses_every_statement() {
    let hub = Hub::new();
    let commit = hub.commit("daniel", &[("a.txt", "one\n")], vec![]);
    let mut wire = MemoryTransport::new(hub);
    assert_eq!(
        refused(&mut wire, &check(commit, CheckResult::Pass, &daniel())),
        StatementRefusal::NoMain
    );
}

#[test]
fn a_revoked_key_signs_nothing_new_and_what_it_signed_before_lies_before_the_entry() {
    let mut started = started();
    let open = opening("feature", started.feature, &["daniel"], &mia());
    let before = submit(&mut started.wire, &open).unwrap();
    // A later entry of the record: a tag. The statement names the entry before it.
    let hub = started.wire.side();
    let entry = hub.push(&daniel(), vec![("tags/v1", Some(started.root))]);
    assert!(before.lies_before(entry.number()));
    let approved = approval(before.hash(), started.feature, &daniel());
    let after = submit(&mut started.wire, &approved).unwrap();
    assert!(!after.lies_before(entry.number()));
    assert_eq!(after.record(), entry.number());

    // A hub whose register at the tip of `main` revoked the key of Mia refuses her statements.
    let hub = Hub::new();
    let mut revoked = register_of();
    revoked.revoke(&human("mia"), &mia().public(), 1).unwrap();
    let root = hub.governed(&revoked, "one\n", vec![]);
    hub.push(&daniel(), vec![("heads/main", Some(root))]);
    let mut wire = MemoryTransport::new(hub);
    assert_eq!(
        refused(&mut wire, &check(root, CheckResult::Pass, &mia())),
        StatementRefusal::Key { register: root }
    );
    // Daniel's key still counts.
    submit(&mut wire, &check(root, CheckResult::Pass, &daniel())).unwrap();
}

#[test]
fn an_opening_needs_the_branch_at_its_head_on_the_hub() {
    let mut started = started();
    let at_root = opening("feature", started.root, &[], &mia());
    assert_eq!(
        refused(&mut started.wire, &at_root),
        StatementRefusal::Head(Some(started.feature))
    );
    let missing = opening("missing", started.root, &[], &mia());
    assert_eq!(
        refused(&mut started.wire, &missing),
        StatementRefusal::Head(None)
    );
}

#[test]
fn only_the_write_owner_opens_and_only_until_the_lease_ends() {
    let mut started = started();
    let by_ana = opening("feature", started.feature, &[], &ana());
    assert_eq!(
        refused(&mut started.wire, &by_ana),
        StatementRefusal::NotWriteOwner
    );
    started.wire.side().time.set(START + LEASE);
    let by_mia = opening("feature", started.feature, &[], &mia());
    assert_eq!(
        refused(&mut started.wire, &by_mia),
        StatementRefusal::NotWriteOwner
    );
}

#[test]
fn the_reviewers_are_in_the_register_and_are_not_the_opener() {
    let mut started = started();
    let feature = started.feature;
    assert_eq!(
        refused(
            &mut started.wire,
            &opening("feature", feature, &["mia"], &mia())
        ),
        StatementRefusal::SelfReview
    );
    assert_eq!(
        refused(
            &mut started.wire,
            &opening("feature", feature, &["zoe"], &mia())
        ),
        StatementRefusal::UnknownReviewer(human("zoe"))
    );
}

#[test]
fn a_branch_has_one_open_pull_request() {
    let (mut started, held) = opened();
    let again = opening("feature", started.feature, &[], &mia());
    assert_eq!(
        refused(&mut started.wire, &again),
        StatementRefusal::AlreadyOpen(held.hash())
    );
}

#[test]
fn a_repaired_commit_is_reached_by_a_reference() {
    let mut started = started();
    let unnamed = started
        .wire
        .side()
        .commit("daniel", &[("b.txt", "x\n")], vec![]);
    let body = Opening::try_new(
        Some(repository_id()),
        branch("feature"),
        started.feature,
        Vec::new(),
        Some(unnamed),
    )
    .unwrap();
    assert_eq!(
        refused(&mut started.wire, &signed(Body::Opening(body), &mia())),
        StatementRefusal::Unreachable(unnamed)
    );
}

#[test]
fn an_approval_is_for_the_current_head_of_an_open_pull_request_and_never_ones_own() {
    let (mut started, held) = opened();
    let pull = held.hash();
    let feature = started.feature;
    // Mia opened it, and Ana wrote its commit.
    assert_eq!(
        refused(&mut started.wire, &approval(pull, feature, &mia())),
        StatementRefusal::SelfApproval
    );
    assert_eq!(
        refused(&mut started.wire, &approval(pull, feature, &ana())),
        StatementRefusal::SelfApproval
    );
    assert_eq!(
        refused(&mut started.wire, &approval(pull, started.root, &daniel())),
        StatementRefusal::Head(Some(feature))
    );
    let unknown = opening("other", feature, &[], &mia()).hash();
    assert_eq!(
        refused(&mut started.wire, &approval(unknown, feature, &daniel())),
        StatementRefusal::NoPull
    );
    let held = submit(&mut started.wire, &approval(pull, feature, &daniel())).unwrap();
    assert_eq!(held.number(), 2);
    assert_eq!(held.actor(), &human("daniel"));
}

#[test]
fn whoever_first_brought_a_commit_under_a_reference_is_a_writer_of_it() {
    let hub = Hub::new();
    let root = hub.governed(&register_of(), "one\n", vec![]);
    hub.push(&daniel(), vec![("heads/main", Some(root))]);
    // A commit under the name of someone that is not in the register, which Daniel tags first.
    let commit = hub.commit("zoe", &[("a.txt", "two\n")], vec![root]);
    hub.push(&daniel(), vec![("tags/look", Some(commit))]);
    hub.push(&mia(), vec![("heads/feature", Some(commit))]);
    let mut wire = MemoryTransport::new(hub);
    let held = submit(&mut wire, &opening("feature", commit, &[], &mia())).unwrap();
    assert_eq!(
        refused(&mut wire, &approval(held.hash(), commit, &daniel())),
        StatementRefusal::SelfApproval
    );
    submit(&mut wire, &approval(held.hash(), commit, &ana())).unwrap();
}

#[test]
fn a_check_names_reachable_commits_and_counts_or_not_at_the_merge() {
    let mut started = started();
    // Ana wrote the commit, so her check would not count for it; it is accepted all the same.
    let held = submit(
        &mut started.wire,
        &check(started.feature, CheckResult::Fail, &ana()),
    )
    .unwrap();
    assert_eq!(held.actor(), &human("ana"));
    let unnamed = started
        .wire
        .side()
        .commit("daniel", &[("b.txt", "x\n")], vec![]);
    assert_eq!(
        refused(
            &mut started.wire,
            &check(unnamed, CheckResult::Pass, &daniel())
        ),
        StatementRefusal::Unreachable(unnamed)
    );
    let with_base = Checked::try_new(
        Some(repository_id()),
        started.feature,
        Some(unnamed),
        CheckResult::Pass,
    )
    .unwrap();
    assert_eq!(
        refused(
            &mut started.wire,
            &signed(Body::Check(with_base), &daniel())
        ),
        StatementRefusal::Unreachable(unnamed)
    );
}

#[test]
fn the_opener_or_an_owner_closes_and_an_ended_opening_is_final() {
    let (mut started, held) = opened();
    let pull = held.hash();
    assert_eq!(
        refused(&mut started.wire, &closing(pull, &ana())),
        StatementRefusal::NotCloser
    );
    submit(&mut started.wire, &closing(pull, &daniel())).unwrap();
    assert_eq!(
        refused(&mut started.wire, &closing(pull, &mia())),
        StatementRefusal::NotOpen
    );
    assert_eq!(
        refused(
            &mut started.wire,
            &approval(pull, started.feature, &daniel())
        ),
        StatementRefusal::NotOpen
    );
    // The same opening is the statement that the hub holds, and the pull request stays closed.
    let open = opening("feature", started.feature, &["daniel"], &mia());
    assert_eq!(submit(&mut started.wire, &open).unwrap(), held);
    assert_eq!(
        refused(
            &mut started.wire,
            &approval(pull, started.feature, &daniel())
        ),
        StatementRefusal::NotOpen
    );
    // The opener closes too.
    let other = opening("feature", started.feature, &[], &mia());
    let other = submit(&mut started.wire, &other).unwrap();
    submit(&mut started.wire, &closing(other.hash(), &mia())).unwrap();
}

#[test]
fn the_authorizer_is_asked() {
    let mut started = started();
    started.wire.side().also.set(Some(PR_OPEN));
    let open = opening("feature", started.feature, &[], &mia());
    assert_eq!(
        refused(&mut started.wire, &open),
        StatementRefusal::Denied(OperationName::try_new(PR_OPEN).unwrap())
    );
}

#[test]
fn the_reading_requests_answer_only_who_may_read() {
    let (mut started, _) = opened();
    started.wire.side().access.set(ReadAccess::Denied);
    for record in [false, true] {
        assert_eq!(
            read_range(&mut started.wire, Range::all(), record),
            Err(ErrorMessage::NotReadable)
        );
    }
    // A request that its reader refuses is answered as malformed, and the hub is not asked.
    started.wire.side().access.set(ReadAccess::Granted);
    let answer = started
        .wire
        .statements(&mut format!("stemma-protocol {VERSION}\nrange 0 -\n").as_bytes())
        .unwrap();
    let Answer::Error(body) = answer else {
        panic!("a malformed range was answered");
    };
    assert!(matches!(
        ErrorMessage::parse(&read_all(body)).unwrap(),
        ErrorMessage::Malformed(_)
    ));
    assert!(
        Range::parse(b"stemma-protocol 1\nrange 5 4\n").is_err_and(|error| matches!(
            error,
            ParseError::Held {
                reason: HeldProblem::Range,
                ..
            }
        ))
    );
}

#[test]
fn a_reader_without_the_lock_leaves_out_a_statement_that_is_being_written() {
    let (started, held) = opened();
    let mut memory = Memory {
        bytes: started.wire.side().held(),
        ..Memory::default()
    };
    memory
        .bytes
        .extend_from_slice(b"stemma-protocol 1\nstatement pr-cl");
    assert_eq!(
        held_in(&mut memory, &Range::all(), ReadAccess::Granted).unwrap(),
        [held]
    );
    memory.fail_read = true;
    assert!(matches!(
        held_in(&mut memory, &Range::all(), ReadAccess::Granted),
        Err(RangeError::Io(_))
    ));
    assert!(matches!(
        held_in(&mut memory, &Range::all(), ReadAccess::Denied),
        Err(RangeError::NotReadable)
    ));
}

#[test]
fn the_failures_of_the_hub_are_no_answer() {
    let (started, held) = opened();
    let hub = started.wire.side();
    let approved = approval(held.hash(), started.feature, &daniel());
    // The clock stepped back.
    hub.time.set(START - 1);
    assert!(matches!(
        hub.accept(&approved),
        Err(StatementError::Time { last: START, .. })
    ));
    hub.time.set(START);
    // The held statements cannot be read.
    hub.statements.borrow_mut().fail_read = true;
    let error = hub.accept(&approved).unwrap_err();
    assert!(matches!(error, StatementError::StatementsIo { .. }));
    assert!(error.answer().is_none());
    assert!(error.io_error().is_some());
    hub.statements.borrow_mut().fail_read = false;
    // A held statement that carries another number than its place.
    let wrong = HeldStatement::seal(
        held.statement().clone(),
        HubLines {
            number: 2,
            ..held.lines().clone()
        },
        &hub_key(),
    )
    .unwrap();
    let saved = hub.held();
    hub.statements.borrow_mut().bytes = wrong.encode();
    assert!(matches!(
        hub.accept(&approved),
        Err(StatementError::Order {
            position: 1,
            number: 2
        })
    ));
    hub.statements.borrow_mut().bytes = saved;
    // The record cannot be read: a failure that the ref update shares.
    hub.record.borrow_mut().fail_read = true;
    assert!(matches!(hub.accept(&approved), Err(StatementError::Hub(_))));
}

#[test]
fn a_repository_with_a_working_copy_accepts_no_statement() {
    let dir = TempDir::new();
    let actor = Actor::human("daniel").unwrap();
    let clone = Repository::init(
        dir.path(),
        &crate::repository::InitOptions::default(),
        &actor,
        &AllowAll,
    )
    .unwrap();
    let hub = HubAddress::try_new(ADDRESS).unwrap();
    let key = hub_key();
    let mut record = Memory::default();
    let mut statements = Memory::default();
    let context = AcceptContext::new(&hub, repository_id(), &mut record, START, &key);
    let statement = check(
        CommitId::new(format!("b3:{}", "11".repeat(32)).parse().unwrap()),
        CheckResult::Pass,
        &daniel(),
    );
    assert!(matches!(
        clone.accept_statement(&statement, context, &mut statements),
        Err(StatementError::NotBare { .. })
    ));
}

#[test]
fn a_fetch_brings_the_statements_of_the_open_pull_requests() {
    let (mut started, held) = opened();
    let feature = started.feature;
    let approved = submit(
        &mut started.wire,
        &approval(held.hash(), feature, &daniel()),
    )
    .unwrap();
    let checked = submit(
        &mut started.wire,
        &check(feature, CheckResult::Pass, &daniel()),
    )
    .unwrap();
    let rooted = submit(
        &mut started.wire,
        &check(started.root, CheckResult::Pass, &daniel()),
    )
    .unwrap();
    // A second branch whose pull request is closed.
    let hub = started.wire.side();
    hub.push(&ana(), vec![("heads/done", Some(started.root))]);
    let done = submit(
        &mut started.wire,
        &opening("done", started.root, &[], &ana()),
    )
    .unwrap();
    let closed = submit(&mut started.wire, &closing(done.hash(), &ana())).unwrap();

    let all = read_held(&read_range(&mut started.wire, Range::all(), false).unwrap()).unwrap();
    let record = read_record(&read_range(&mut started.wire, Range::all(), true).unwrap()).unwrap();
    assert_eq!(all.len(), 6);
    let selected = statements_of_open_pulls(&all, &record, &register_of());
    assert_eq!(selected, [held, approved, checked]);
    assert!(!selected.contains(&rooted) && !selected.contains(&closed));

    // The clone keeps them in its set, for the entry of the fetch, if the pinned key signed
    // them.
    let dir = TempDir::new();
    let actor = Actor::human("daniel").unwrap();
    let clone = Repository::init(
        dir.path(),
        &crate::repository::InitOptions::default(),
        &actor,
        &AllowAll,
    )
    .unwrap();
    let operation = OperationName::try_new("fetch").unwrap();
    let allowed = Allowed::ask(&AllowAll, &actor, &operation, "hub").unwrap();
    let lock = clone.refs_store().lock_writer(&allowed).unwrap();
    assert!(matches!(
        clone.keep_received_statements(&lock, &selected, &stranger().public()),
        Err(ReceivedError::OtherKey { number: 1 })
    ));
    let hashes = clone
        .keep_received_statements(&lock, &selected, &hub_key().public())
        .unwrap();
    let set = clone.statement_set();
    for (hash, held) in hashes.iter().zip(&selected) {
        assert_eq!(set.get(hash).unwrap().as_ref(), Some(held.statement()));
    }
}

#[test]
fn a_policy_that_cannot_be_used_refuses_an_opening_where_a_lease_applies() {
    let hub = Hub::new();
    let root = hub.commit(
        "daniel",
        &[
            (register::FILE, &register_of().encode()),
            ("a.txt", "one\n"),
        ],
        vec![],
    );
    hub.push(&daniel(), vec![("heads/main", Some(root))]);
    let feature = hub.commit("ana", &[("a.txt", "two\n")], vec![root]);
    hub.push(&mia(), vec![("heads/feature", Some(feature))]);
    let mut wire = MemoryTransport::new(hub);
    assert!(matches!(
        refused(&mut wire, &opening("feature", feature, &[], &mia())),
        StatementRefusal::Policy(_)
    ));
}

#[test]
fn the_lease_counts_to_its_last_second() {
    let mut started = started();
    started.wire.side().time.set(START + LEASE - 1);
    submit(
        &mut started.wire,
        &opening("feature", started.feature, &[], &mia()),
    )
    .unwrap();
}

#[test]
fn a_closing_of_an_unknown_pull_request_is_refused() {
    let mut started = started();
    let unknown = opening("other", started.feature, &[], &mia()).hash();
    assert_eq!(
        refused(&mut started.wire, &closing(unknown, &daniel())),
        StatementRefusal::NoPull
    );
}

#[test]
fn the_authorizer_is_asked_for_the_operation_of_every_kind() {
    let (mut started, held) = opened();
    let feature = started.feature;
    let cases = [
        (PR_APPROVE, approval(held.hash(), feature, &daniel())),
        (PR_CLOSE, closing(held.hash(), &daniel())),
        (CHECK, check(feature, CheckResult::Pass, &daniel())),
    ];
    for (operation, statement) in cases {
        started.wire.side().also.set(Some(operation));
        assert_eq!(
            refused(&mut started.wire, &statement),
            StatementRefusal::Denied(OperationName::try_new(operation).unwrap())
        );
    }
}

#[test]
fn a_failed_append_holds_nothing_and_is_no_answer() {
    let (started, held) = opened();
    let hub = started.wire.side();
    let before = hub.held();
    hub.statements.borrow_mut().fail_append = true;
    let error = hub
        .accept(&approval(held.hash(), started.feature, &daniel()))
        .unwrap_err();
    assert!(matches!(
        error,
        StatementError::StatementsIo {
            step: RecordStep::Append,
            ..
        }
    ));
    assert!(error.answer().is_none());
    assert_eq!(hub.held(), before);
}

#[test]
fn the_time_of_the_last_record_entry_bounds_the_time_of_a_statement() {
    let started = started();
    let hub = started.wire.side();
    hub.time.set(START + 100);
    hub.push(&daniel(), vec![("tags/v1", Some(started.root))]);
    hub.time.set(START + 50);
    assert!(matches!(
        hub.accept(&check(started.root, CheckResult::Pass, &daniel())),
        Err(StatementError::Time {
            last: 1_000_100,
            ..
        })
    ));
}

#[test]
fn damaged_numbers_of_the_held_statements_are_reported_by_the_reader_of_a_range() {
    let (started, held) = opened();
    let wrong = HeldStatement::seal(
        held.statement().clone(),
        HubLines {
            number: 3,
            ..held.lines().clone()
        },
        &hub_key(),
    )
    .unwrap();
    let mut memory = Memory {
        bytes: wrong.encode(),
        ..Memory::default()
    };
    drop(started);
    assert!(matches!(
        held_in(&mut memory, &Range::all(), ReadAccess::Granted),
        Err(RangeError::Order {
            position: 1,
            number: 3
        })
    ));
}

#[test]
fn a_step_of_an_identity_that_the_register_no_longer_holds_still_counts() {
    let (mut started, held) = opened();
    // Ana opens and closes a pull request of her own.
    started
        .wire
        .side()
        .push(&ana(), vec![("heads/done", Some(started.root))]);
    let done = submit(
        &mut started.wire,
        &opening("done", started.root, &[], &ana()),
    )
    .unwrap();
    submit(&mut started.wire, &closing(done.hash(), &ana())).unwrap();
    let all = read_held(&read_range(&mut started.wire, Range::all(), false).unwrap()).unwrap();
    let record = read_record(&read_range(&mut started.wire, Range::all(), true).unwrap()).unwrap();
    // A register without Ana and Mia: her closing still ends her pull request, and Mia's
    // opening is still open.
    let without = Register::with_owner(human("daniel"), daniel().public()).unwrap();
    assert_eq!(statements_of_open_pulls(&all, &record, &without), [held]);
}

#[test]
fn a_merge_in_the_record_ends_its_pull_request() {
    use crate::protocol::record::{Acceptance, Chain};
    let (mut started, held) = opened();
    let all = read_held(&read_range(&mut started.wire, Range::all(), false).unwrap()).unwrap();
    let mut record =
        read_record(&read_range(&mut started.wire, Range::all(), true).unwrap()).unwrap();
    // The hub moves no `main` past the first push yet, so the entry of the merge is written
    // with the writer of the record, as the gate on a hub will write it.
    let mut chain = Chain::new(repository_id());
    for entry in &record {
        chain.accept(entry).unwrap();
    }
    let main = name("heads/main");
    let merged = Move::try_new(
        main.clone(),
        chain.entry_of(&main),
        Some(started.root),
        Some(started.feature),
        Some(held.hash()),
    )
    .unwrap();
    let update = RefUpdatePayload::try_new(
        HubAddress::try_new(ADDRESS).unwrap(),
        repository_id(),
        vec![merged],
        daniel().public(),
    )
    .unwrap()
    .sign(&daniel())
    .unwrap();
    let acceptance = Acceptance {
        time: START,
        register: started.root,
        policies: Vec::new(),
        fast_forward: std::collections::BTreeSet::new(),
        actor: human("daniel"),
    };
    let entry = chain
        .append(&update, acceptance, &hub_key(), &mut Vec::new())
        .unwrap();
    record.push(entry);
    assert_eq!(statements_of_open_pulls(&all, &record, &register_of()), []);
}
