//! The tests of the gate on a hub (STEMMA-303). A clone reaches the hub through
//! [`MemoryTransport`]: it submits the statements of a pull request, sends the ref update that
//! moves `main` from `T` to `M` with the opening as its evidence, and reads the receipt or the
//! refusal. The other side is [`Hub`], a bare repository with its record and its statements in
//! memory, which answers with [`Repository::accept_ref_update`] and
//! [`Repository::accept_statement`].

use std::cell::{Cell, RefCell};
use std::io::{self, Read};

use crate::change::ChangeId;
use crate::index::Index;
use crate::namespace::Namespace;
use crate::object::{Blob, Commit, CommitId, FileMode};
use crate::oplog::{CheckResult, OperationName};
use crate::policy::{Policy, RiskLevel};
use crate::protocol::held::HeldStatement;
use crate::protocol::record::tests::Seeded;
use crate::protocol::record::{
    read_record, HubAddress, Move, RecordEntry, RefUpdate, RefUpdatePayload, StatementHash,
};
use crate::protocol::refusal::{GateRefusal, LevelSource, RefOperation, RefusalReason, Refused};
use crate::protocol::statement::{Approval, Body, Checked, Opening, Statement};
use crate::protocol::{ErrorMessage, QualifiedName, RepositoryId};
use crate::register::{self, Identity, Register, Role};
use crate::repository::statements::by_hand;
use crate::repository::{
    AcceptContext, GateError, InitOptions, RecordDestination, Repository, StatementDestination,
};
use crate::signature::{self, PayloadKind, Signer};
use crate::store::ObjectStore;
use crate::testutil::TempDir;
use crate::transport::{Answer, MemoryTransport, OtherSide, Refusal, Transport};
use crate::{policy, Actor, AllowAll, Authorizer, Decision};

const ADDRESS: &str = "https://hub.example";
const REPOSITORY: &str = "00112233445566778899aabbccddeeff";
const START: i64 = 1_000_000;

/// The policy of these tests: `docs/` and `tests/` are level 0, every other path level 2, and
/// the register and the policy level 3 by the floor of the core.
const POLICY: &str = "stemma-policy 1\nlevel 0 docs/\nlevel 0 tests/\n";

/// A test with two assertions, which a change weakens to one.
const TEST: &str = "#[test]\nfn holds() {\n    assert!(one());\n    assert!(two());\n}\n";
const WEAKER: &str = "#[test]\nfn holds() {\n    assert!(one());\n}\n";

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

/// The key of the agent `bot` of Daniel.
fn bot() -> Seeded {
    Seeded::new(6)
}

/// The key of the agent `cat` of Daniel.
fn cat() -> Seeded {
    Seeded::new(7)
}

fn human(id: &str) -> Identity {
    Identity::human(id).unwrap()
}

fn agent(id: &str) -> Identity {
    Identity::of(&Actor::agent(id, "-", "-", "daniel").unwrap())
}

fn repository_id() -> RepositoryId {
    REPOSITORY.parse().unwrap()
}

fn address() -> HubAddress {
    HubAddress::try_new(ADDRESS).unwrap()
}

fn name(text: &str) -> QualifiedName {
    text.parse().unwrap()
}

/// The register of these tests: Daniel owns it, Mia and Ana are members, and Daniel sponsors
/// the agents `bot` and `cat`.
fn register_of() -> Register {
    let mut register = Register::with_owner(human("daniel"), daniel().public()).unwrap();
    register
        .add_human(human("mia"), Role::Member, mia().public())
        .unwrap();
    register
        .add_human(human("ana"), Role::Member, ana().public())
        .unwrap();
    for (id, key) in [("bot", bot()), ("cat", cat())] {
        let payload = register::sponsorship_payload(&agent(id), Role::Member, &human("daniel"));
        let cosign = signature::sign(&daniel(), PayloadKind::Sponsorship, &payload).unwrap();
        register
            .add_agent(
                agent(id),
                Role::Member,
                human("daniel"),
                key.public(),
                cosign,
            )
            .unwrap();
    }
    register
}

/// Refuses one operation and allows every other.
struct Deny(&'static str);

impl Authorizer for Deny {
    fn authorize(&self, _actor: &Actor, operation: &OperationName, _target: &str) -> Decision {
        if operation.as_str() == self.0 {
            Decision::Deny
        } else {
            Decision::Allow
        }
    }
}

/// A destination in memory for the record and the statements.
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

/// The other side: a bare repository, its record, its statements, and the clock of the hub.
struct Hub {
    _dir: TempDir,
    repository: Repository,
    record: RefCell<Memory>,
    statements: RefCell<Memory>,
    time: Cell<i64>,
    /// The namespace that the context names, whose policy counts beside the policy of `T`.
    namespace: RefCell<Option<Namespace>>,
    /// Whether the context names no destination of the statements.
    no_statements: Cell<bool>,
    /// An operation that one more authorizer denies.
    denied: Cell<Option<&'static str>>,
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
            namespace: RefCell::default(),
            no_statements: Cell::new(false),
            denied: Cell::new(None),
        }
    }

    /// Stores a commit of `author` with these files and parents, as an upload does.
    fn commit(&self, author: &str, files: &[(&str, &str)], parents: Vec<CommitId>) -> CommitId {
        commit_in(self.repository.objects(), author, files, parents)
    }

    fn entries(&self) -> Vec<RecordEntry> {
        read_record(&self.record.borrow().0).unwrap()
    }

    fn main(&self) -> Option<CommitId> {
        self.repository
            .refs()
            .branch(&crate::refs::BranchName::try_new("main").unwrap())
            .unwrap()
    }
}

/// Stores a commit of `author` with these files and parents.
fn commit_in(
    store: &dyn ObjectStore,
    author: &str,
    files: &[(&str, &str)],
    parents: Vec<CommitId>,
) -> CommitId {
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

impl OtherSide for Hub {
    fn refs(&self) -> Result<crate::protocol::RefList, Refusal> {
        Err(Refusal::Failed("these tests list no references".to_owned()))
    }

    fn fetch(
        &self,
        _request: &crate::protocol::FetchRequest,
        _stream: &mut dyn io::Write,
    ) -> Result<(), Refusal> {
        Err(Refusal::Failed("these tests fetch nothing".to_owned()))
    }

    fn ref_update(&self, update: &RefUpdate) -> Result<RecordEntry, Refusal> {
        let hub = address();
        let signer = hub_key();
        let mut record = self.record.borrow_mut();
        let mut statements = self.statements.borrow_mut();
        let namespace = self.namespace.borrow();
        let mut context = AcceptContext::new(
            &hub,
            repository_id(),
            &mut *record,
            self.time.get(),
            &signer,
        );
        if let Some(namespace) = namespace.as_ref() {
            context = context.with_namespace(namespace);
        }
        if !self.no_statements.get() {
            context = context.with_statements(&mut *statements);
        }
        let also: Box<dyn Authorizer> = match self.denied.get() {
            Some(operation) => Box::new(Deny(operation)),
            None => Box::new(AllowAll),
        };
        self.repository
            .accept_ref_update_with(update, context, &*also)
            .map_err(|error| match error.answer() {
                Some(message) => Refusal::Error(message),
                None => Refusal::Failed(error.to_string()),
            })
    }

    fn submit_statement(&self, statement: &Statement) -> Result<HeldStatement, Refusal> {
        let hub = address();
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
        self.repository
            .accept_statement(statement, context, &mut *statements)
            .map_err(|error| match error.answer() {
                Some(message) => Refusal::Error(message),
                None => Refusal::Failed(error.to_string()),
            })
    }
}

type Wire = MemoryTransport<Hub>;

fn read_all(mut body: crate::transport::Body<'_>) -> Vec<u8> {
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).unwrap();
    bytes
}

/// The move of `reference` to `new` with `evidence`, from the state that the record leaves it
/// in.
fn to(
    wire: &Wire,
    reference: &str,
    new: Option<CommitId>,
    evidence: Option<StatementHash>,
) -> Move {
    let reference = name(reference);
    let last = wire.side().entries().into_iter().rev().find_map(|entry| {
        entry
            .update()
            .payload()
            .moves()
            .iter()
            .find(|moved| moved.name() == &reference)
            .map(|moved| (entry.hash(), moved.new_commit()))
    });
    let (previous, old) = last.map_or((None, None), |(hash, old)| (Some(hash), old));
    Move::try_new(reference, previous, old, new, evidence).unwrap()
}

/// Sends a ref update of `signer` as a clone does.
fn push(wire: &mut Wire, signer: &Seeded, moves: Vec<Move>) -> Result<RecordEntry, ErrorMessage> {
    let update = RefUpdatePayload::try_new(address(), repository_id(), moves, signer.public())
        .unwrap()
        .sign(signer)
        .unwrap();
    match wire.ref_update(&mut update.encode().as_slice()).unwrap() {
        Answer::Message(body) => {
            let receipt = RecordEntry::parse(&read_all(body)).unwrap();
            receipt.check_receipt(&update, &hub_key().public()).unwrap();
            Ok(receipt)
        }
        Answer::Error(body) => Err(ErrorMessage::parse(&read_all(body)).unwrap()),
    }
}

/// Submits a statement as a clone does and returns its hash.
fn submit(wire: &mut Wire, body: Body, signer: &Seeded) -> StatementHash {
    let statement = Statement::sign(body, signer).unwrap();
    match wire
        .submit_statement(&mut statement.encode().as_slice())
        .unwrap()
    {
        Answer::Message(_) => statement.hash(),
        Answer::Error(body) => panic!("{:?}", ErrorMessage::parse(&read_all(body))),
    }
}

/// The verdict of the gate in an answer: `None` for a receipt.
fn verdict(answer: Result<RecordEntry, ErrorMessage>) -> Option<RefusalReason> {
    match answer {
        Ok(_) => None,
        Err(ErrorMessage::Refused(Refused::References(refused))) => {
            let [refused] = refused.references() else {
                panic!("{refused:?}");
            };
            assert_eq!(refused.name(), &name("heads/main"));
            Some(refused.reason().clone())
        }
        Err(other) => panic!("{other:?}"),
    }
}

/// The gate refusal in an answer.
fn refused(answer: Result<RecordEntry, ErrorMessage>) -> GateRefusal {
    match verdict(answer) {
        Some(RefusalReason::Verdict(refusal)) => refusal,
        other => panic!("{other:?}"),
    }
}

/// The files of the root of [`gated_with`], with `register` and `policy`.
fn base_files<'a>(register: &'a str, policy: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        (register::FILE, register),
        (policy::FILE_NAME, policy),
        ("tests/t.rs", TEST),
        ("src/lib.rs", "pub fn one() {}\n"),
    ]
}

/// A hub after the first push of Daniel, with a pull request of Mia open on `feature`.
struct Gated {
    wire: Wire,
    /// The tip of `main`.
    root: CommitId,
    /// The head of `feature`, a commit of Mia on the root.
    head: CommitId,
    /// The opening of the pull request.
    pull: StatementHash,
}

/// `main` at a root with the register, `policy`, and the files `tests/t.rs` and `src/lib.rs`;
/// `feature` at a commit of Mia that adds or changes `change`, opened by her.
fn gated_with(policy: &str, change: &[(&str, &str)]) -> Gated {
    gated_by(policy, change, &mia())
}

/// [`gated_with`], with the head pushed to `feature` and opened by `writer`.
fn gated_by(policy: &str, change: &[(&str, &str)], writer: &Seeded) -> Gated {
    let hub = Hub::new();
    let register = register_of().encode();
    let base = [
        (register::FILE, register.as_str()),
        (policy::FILE_NAME, policy),
        ("tests/t.rs", TEST),
        ("src/lib.rs", "pub fn one() {}\n"),
    ];
    let root = hub.commit("daniel", &base, vec![]);
    let mut files: Vec<(&str, &str)> = base
        .iter()
        .filter(|(path, _)| change.iter().all(|(changed, _)| changed != path))
        .copied()
        .collect();
    files.extend_from_slice(change);
    let head = hub.commit("mia", &files, vec![root]);
    let mut wire = MemoryTransport::new(hub);
    let moved = to(&wire, "heads/main", Some(root), None);
    push(&mut wire, &daniel(), vec![moved]).unwrap();
    let moved = to(&wire, "heads/feature", Some(head), None);
    push(&mut wire, writer, vec![moved]).unwrap();
    let opening = Opening::try_new(
        Some(repository_id()),
        crate::refs::BranchName::try_new("feature").unwrap(),
        head,
        Vec::new(),
        None,
    )
    .unwrap();
    let pull = submit(&mut wire, Body::Opening(opening), writer);
    Gated {
        wire,
        root,
        head,
        pull,
    }
}

/// [`gated_with`] for a change of `docs/a.md`, which is level 0.
fn gated() -> Gated {
    gated_with(POLICY, &[("docs/a.md", "a\n")])
}

impl Gated {
    fn approve(&mut self, head: CommitId, signer: &Seeded) {
        let approval = Approval::new(Some(repository_id()), self.pull, head);
        submit(&mut self.wire, Body::Approval(approval), signer);
    }

    /// Approvals that every level asks for: `bot`, `cat`, and Daniel, a human owner.
    fn approve_all(&mut self) {
        for signer in [bot(), cat(), daniel()] {
            self.approve(self.head, &signer);
        }
    }

    /// A result of Ana: `commit` merged onto `base`, or run on itself.
    fn check(&mut self, commit: CommitId, base: Option<CommitId>, result: CheckResult) {
        let checked = Checked::try_new(Some(repository_id()), commit, base, result).unwrap();
        submit(&mut self.wire, Body::Check(checked), &ana());
    }

    fn tip(&self) -> CommitId {
        self.wire.side().main().unwrap()
    }

    /// The merge `M` of the head onto the tip: the tree of the head, which descends from the
    /// tip, and the single parent `T`.
    fn merge(&self) -> CommitId {
        let store = self.wire.side().repository.objects();
        let tree = store.get_commit(&self.head).unwrap().unwrap().tree();
        let commit = Commit::try_new(
            tree,
            vec![self.tip()],
            ChangeId::generate().unwrap(),
            Actor::human("daniel").unwrap(),
            2,
            "Merge pull request",
        )
        .unwrap();
        store.put_commit(&commit).unwrap()
    }

    /// Lands a change of Mia that replaces the register with `register` through the gate: level
    /// 3, which Daniel approves and Ana checks, declared as a repair of `repairs`. Returns the
    /// new tip.
    fn land_register(&mut self, register: &Register, repairs: Option<CommitId>) -> CommitId {
        let text = register.encode();
        self.land_files("keys", &base_files(&text, POLICY), &[daniel()], repairs)
    }

    /// Lands a change of Mia on `branch` with `files` through the gate: approved by
    /// `reviewers`, checked by Ana, declared as a repair of `repairs`. Returns the new tip.
    fn land_files(
        &mut self,
        branch: &str,
        files: &[(&str, &str)],
        reviewers: &[Seeded],
        repairs: Option<CommitId>,
    ) -> CommitId {
        let tip = self.tip();
        let keys = self.wire.side().commit("mia", files, vec![tip]);
        let moved = to(&self.wire, &format!("heads/{branch}"), Some(keys), None);
        push(&mut self.wire, &mia(), vec![moved]).unwrap();
        let opening = Opening::try_new(
            Some(repository_id()),
            crate::refs::BranchName::try_new(branch).unwrap(),
            keys,
            Vec::new(),
            repairs,
        )
        .unwrap();
        let pull = submit(&mut self.wire, Body::Opening(opening), &mia());
        for reviewer in reviewers {
            let approval = Approval::new(Some(repository_id()), pull, keys);
            submit(&mut self.wire, Body::Approval(approval), reviewer);
        }
        self.check(keys, Some(tip), CheckResult::Pass);
        let store = self.wire.side().repository.objects();
        let tree = store.get_commit(&keys).unwrap().unwrap().tree();
        let landed = Commit::try_new(
            tree,
            vec![tip],
            ChangeId::generate().unwrap(),
            Actor::human("daniel").unwrap(),
            2,
            "Merge",
        )
        .unwrap();
        let landed = store.put_commit(&landed).unwrap();
        let moved = to(&self.wire, "heads/main", Some(landed), Some(pull));
        push(&mut self.wire, &daniel(), vec![moved]).unwrap();
        landed
    }

    /// The merge of the change of `docs/a.md` onto the tip `tip`, whose register is `register`.
    fn docs_onto(&self, tip: CommitId, register: &Register) -> CommitId {
        let text = register.encode();
        let mut files = base_files(&text, POLICY);
        files.push(("docs/a.md", "a\n"));
        self.wire.side().commit("daniel", &files, vec![tip])
    }

    /// Sends the move of `main` to `merged` with the pull request as its evidence.
    fn land(&mut self, merged: CommitId) -> Result<RecordEntry, ErrorMessage> {
        let moved = to(&self.wire, "heads/main", Some(merged), Some(self.pull));
        push(&mut self.wire, &daniel(), vec![moved])
    }
}

#[test]
fn a_merge_that_the_gate_allows_moves_main_and_its_entry_names_the_pull_request() {
    let mut g = gated();
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();

    let receipt = g.land(merged).unwrap();

    assert_eq!(g.tip(), merged);
    let [moved] = receipt.update().payload().moves() else {
        panic!("one move");
    };
    assert_eq!(moved.evidence(), Some(g.pull));
    // The verdict rests on the policy of `T`, and the merge is a fast-forward of `main`.
    assert_eq!(receipt.policies(), [g.root]);
    assert_eq!(receipt.is_fast_forward(&name("heads/main")), Some(true));
    // "Merged" follows from the entry: the opening is final now.
    let again = to(&g.wire, "heads/main", Some(g.merge()), Some(g.pull));
    assert_eq!(
        refused(push(&mut g.wire, &daniel(), vec![again])),
        GateRefusal::NotOpen
    );
}

#[test]
fn a_merge_with_a_second_parent_is_refused() {
    let mut g = gated();
    g.approve_all();
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let store = g.wire.side().repository.objects();
    let tree = store.get_commit(&g.head).unwrap().unwrap().tree();
    let two = Commit::try_new(
        tree,
        vec![g.root, g.head],
        ChangeId::generate().unwrap(),
        Actor::human("daniel").unwrap(),
        2,
        "Merge",
    )
    .unwrap();
    let two = store.put_commit(&two).unwrap();
    // A commit with the tree of the merge and no parent is no merge onto `T` either.
    let orphan = Commit::try_new(
        tree,
        Vec::new(),
        ChangeId::generate().unwrap(),
        Actor::human("daniel").unwrap(),
        2,
        "Merge",
    )
    .unwrap();
    let orphan = store.put_commit(&orphan).unwrap();
    for merged in [two, orphan] {
        assert_eq!(refused(g.land(merged)), GateRefusal::Parents);
    }
    assert_eq!(g.tip(), g.root);
}

#[test]
fn a_tree_that_is_not_the_clean_merge_is_refused() {
    let mut g = gated();
    g.approve_all();
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let register = register_of().encode();
    // `M` smuggles a file in that the head does not hold.
    let merged = g.wire.side().commit(
        "daniel",
        &[
            (register::FILE, register.as_str()),
            (policy::FILE_NAME, POLICY),
            ("tests/t.rs", TEST),
            ("src/lib.rs", "pub fn one() {}\n"),
            ("docs/a.md", "a\n"),
            ("src/more.rs", "smuggled\n"),
        ],
        vec![g.root],
    );

    assert_eq!(refused(g.land(merged)), GateRefusal::NotTheMerge);
    assert_eq!(g.tip(), g.root);
}

#[test]
fn a_merge_that_breaks_the_register_or_the_policy_is_refused() {
    // Every later update reads the register of the tip and every later move of `main` its
    // policy: a merge that the gate allows but that breaks one of them would lock the hub.
    for change in [
        (register::FILE, "not a register\n"),
        (policy::FILE_NAME, "not a policy\n"),
    ] {
        let mut g = gated_with(POLICY, &[change]);
        g.approve_all();
        g.check(g.head, Some(g.root), CheckResult::Pass);
        let merged = g.merge();
        assert!(
            matches!(refused(g.land(merged)), GateRefusal::Unusable(_)),
            "{}",
            change.0
        );
        assert_eq!(g.tip(), g.root);
    }
}

#[test]
fn a_check_of_an_agent_of_the_opener_does_not_count_at_level_2() {
    // Daniel pushed and opened the head; `bot` is his agent. Its approval counts as one of two
    // agents (ADR-004), but its run is not independent of the change at level 2 (ADR-019).
    let mut g = gated_by(POLICY, &[("src/lib.rs", "pub fn two() {}\n")], &daniel());
    for signer in [bot(), cat(), ana()] {
        g.approve(g.head, &signer);
    }
    let checked = Checked::try_new(
        Some(repository_id()),
        g.head,
        Some(g.root),
        CheckResult::Pass,
    )
    .unwrap();
    submit(&mut g.wire, Body::Check(checked), &bot());
    let merged = g.merge();
    assert_eq!(
        refused(g.land(merged)),
        GateRefusal::NotChecked {
            level: RiskLevel::Two,
            source: LevelSource::Paths,
        }
    );
    g.check(g.head, Some(g.root), CheckResult::Pass);
    assert_eq!(verdict(g.land(merged)), None);
}

#[test]
fn a_head_on_main_already_lands_no_change_and_no_other_tree() {
    let mut g = gated();
    // A pull request whose head is the tip of `main` itself.
    let moved = to(&g.wire, "heads/same", Some(g.root), None);
    push(&mut g.wire, &mia(), vec![moved]).unwrap();
    let opening = Opening::try_new(
        Some(repository_id()),
        crate::refs::BranchName::try_new("same").unwrap(),
        g.root,
        Vec::new(),
        None,
    )
    .unwrap();
    g.pull = submit(&mut g.wire, Body::Opening(opening), &mia());
    let register = register_of().encode();
    let mut files = base_files(&register, POLICY);
    let same = g.wire.side().commit("daniel", &files, vec![g.root]);
    assert!(matches!(refused(g.land(same)), GateRefusal::Level(_)));
    files.push(("src/more.rs", "smuggled\n"));
    let other = g.wire.side().commit("daniel", &files, vec![g.root]);
    assert_eq!(refused(g.land(other)), GateRefusal::NotTheMerge);
    assert_eq!(g.tip(), g.root);
}

#[test]
fn a_head_that_conflicts_with_the_tip_is_refused() {
    let mut g = gated_with(POLICY, &[("src/lib.rs", "pub fn two() {}\n")]);
    // Another change of `src/lib.rs` lands first.
    let register = register_of().encode();
    let mut files = base_files(&register, POLICY);
    files.retain(|(path, _)| *path != "src/lib.rs");
    files.push(("src/lib.rs", "pub fn three() {}\n"));
    g.land_files("other", &files, &[bot(), cat(), daniel()], None);
    g.approve_all();
    g.check(g.head, Some(g.tip()), CheckResult::Pass);
    let tip = g.tip();
    let merged = g.merge();
    assert_eq!(refused(g.land(merged)), GateRefusal::Conflict);
    assert_eq!(g.tip(), tip);
}

#[test]
fn a_pull_request_whose_branch_was_deleted_is_refused() {
    let mut g = gated();
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();
    let moved = to(&g.wire, "heads/feature", None, None);
    push(&mut g.wire, &mia(), vec![moved]).unwrap();
    assert_eq!(
        refused(g.land(merged)),
        GateRefusal::BranchMoved { head: None }
    );
}

#[test]
fn a_merge_that_records_a_conflict_or_is_not_stored_is_refused() {
    let mut g = gated();
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let store = g.wire.side().repository.objects();
    let tree = |commit: &CommitId| store.get_commit(commit).unwrap().unwrap().tree();
    let conflict = crate::object::Commit2::try_new(
        vec![tree(&g.head), tree(&g.root), tree(&g.head)],
        vec![g.root],
        ChangeId::generate().unwrap(),
        Actor::human("daniel").unwrap(),
        2,
        "Merge",
    )
    .unwrap();
    let conflict = CommitId::new(
        store
            .put_raw(crate::object::Kind::Commit2, &conflict.encode())
            .unwrap(),
    );
    // `main` never holds a commit that records an open conflict.
    assert_eq!(refused(g.land(conflict)), GateRefusal::NotTheMerge);
    let absent = CommitId::new(
        format!("b3:{}", "7".repeat(64))
            .parse::<crate::object::ObjectId>()
            .unwrap(),
    );
    assert!(matches!(
        verdict(g.land(absent)),
        Some(RefusalReason::Incomplete(_))
    ));
    assert_eq!(g.tip(), g.root);
}

#[test]
fn a_merge_without_the_approvals_of_its_level_is_refused_with_the_count() {
    let mut g = gated();
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();
    assert_eq!(
        refused(g.land(merged)),
        GateRefusal::NotApproved {
            level: RiskLevel::Zero,
            source: LevelSource::Paths,
            agents: 0,
            agents_needed: 1,
            humans: 0,
            humans_needed: 0,
        }
    );
    // An approval of a human does not stand in for the approval of an agent.
    g.approve(g.head, &daniel());
    assert!(matches!(
        refused(g.land(merged)),
        GateRefusal::NotApproved { agents: 0, .. }
    ));
    g.approve(g.head, &bot());
    assert_eq!(verdict(g.land(merged)), None);
}

#[test]
fn without_a_result_that_counts_the_merge_is_refused() {
    let mut g = gated();
    g.approve_all();
    let merged = g.merge();
    // A failing run counts for nothing, and neither does a run of another commit.
    g.check(g.head, Some(g.root), CheckResult::Fail);
    g.check(g.root, None, CheckResult::Pass);
    assert_eq!(
        refused(g.land(merged)),
        GateRefusal::NotChecked {
            level: RiskLevel::Zero,
            source: LevelSource::Paths,
        }
    );
}

#[test]
fn an_approval_of_a_key_that_was_revoked_before_the_merge_does_not_count() {
    let mut g = gated();
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    // A second pull request revokes the key of `bot` and lands through the gate.
    let mut revoked = register_of();
    revoked.revoke(&agent("bot"), &bot().public(), 1).unwrap();
    let landed = g.land_register(&revoked, None);

    // The approval of `bot` came before the revocation and is void all the same.
    let merged = g.docs_onto(landed, &revoked);
    assert!(matches!(
        refused(g.land(merged)),
        GateRefusal::NotApproved {
            agents: 0,
            agents_needed: 1,
            ..
        }
    ));
    // The approval of an agent whose key holds counts.
    g.approve(g.head, &cat());
    assert_eq!(verdict(g.land(merged)), None);
}

#[test]
fn a_statement_before_the_retirement_of_its_key_counts() {
    let mut g = gated();
    g.approve(g.head, &bot());
    // The only result of the change is a run of Ana, whose key is retired after it.
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let mut retired = register_of();
    retired
        .add_key(&human("ana"), Seeded::new(8).public())
        .unwrap();
    retired.retire(&human("ana"), &ana().public(), 1).unwrap();
    let landed = g.land_register(&retired, None);

    let merged = g.docs_onto(landed, &retired);
    assert_eq!(verdict(g.land(merged)), None);
}

#[test]
fn a_register_of_the_record_that_cannot_be_read_fails_the_hub() {
    let mut g = gated();
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let mut retired = register_of();
    retired
        .add_key(&human("ana"), Seeded::new(8).public())
        .unwrap();
    retired.retire(&human("ana"), &ana().public(), 1).unwrap();
    let landed = g.land_register(&retired, None);
    let merged = g.docs_onto(landed, &retired);
    // The register of the root, which tells how long the retired key of Ana counted, is lost.
    let store = g.wire.side().repository.objects();
    let blob = store
        .put_blob(&Blob::new(register_of().encode().into_bytes()))
        .unwrap();
    std::fs::remove_file(store.object_path(&blob.object_id())).unwrap();

    let moved = to(&g.wire, "heads/main", Some(merged), Some(g.pull));
    let update =
        RefUpdatePayload::try_new(address(), repository_id(), vec![moved], daniel().public())
            .unwrap()
            .sign(&daniel())
            .unwrap();
    // A failure of the hub, not a refusal that reads as final.
    assert!(g.wire.ref_update(&mut update.encode().as_slice()).is_err());
    assert_eq!(g.tip(), landed);
}

#[test]
fn a_failing_run_before_the_revocation_of_its_key_keeps_the_tip_red() {
    let mut g = gated_with(POLICY, &[("src/lib.rs", "pub fn two() {}\n")]);
    g.approve_all();
    let mut revoked = register_of();
    revoked
        .add_key(&human("ana"), Seeded::new(8).public())
        .unwrap();
    revoked.revoke(&human("ana"), &ana().public(), 1).unwrap();
    // Ana's run fails on the root, then her key is revoked by a change that repairs the root.
    g.check(g.root, None, CheckResult::Fail);
    let landed = g.land_register(&revoked, Some(g.root));
    let text = revoked.encode();
    let mut files = base_files(&text, POLICY);
    files.retain(|(path, _)| *path != "src/lib.rs");
    files.push(("src/lib.rs", "pub fn two() {}\n"));
    let merged = g.wire.side().commit("daniel", &files, vec![landed]);
    let checked = Checked::try_new(
        Some(repository_id()),
        g.head,
        Some(landed),
        CheckResult::Pass,
    )
    .unwrap();
    // A run of Daniel, who neither opened nor wrote the change, counts at level 2.
    submit(&mut g.wire, Body::Check(checked), &daniel());

    assert_eq!(refused(g.land(merged)), GateRefusal::RedTip { red: g.root });
}

#[test]
fn a_weakened_test_raises_the_level_of_its_path() {
    let mut g = gated_with(POLICY, &[("tests/t.rs", WEAKER)]);
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();

    // `tests/` is level 0, and the lost assertion makes the change level 2: two agents and a
    // human.
    assert_eq!(
        refused(g.land(merged)),
        GateRefusal::NotApproved {
            level: RiskLevel::Two,
            source: LevelSource::Weakened,
            agents: 1,
            agents_needed: 2,
            humans: 0,
            humans_needed: 1,
        }
    );
    // At level 2 the approval of a member counts as the approval of a human.
    g.approve(g.head, &cat());
    g.approve(g.head, &ana());
    assert_eq!(verdict(g.land(merged)), None);
}

#[test]
fn a_merge_that_lowers_the_policy_is_judged_by_the_old_one() {
    // The policy of `T` asks for an agent at level 3; the change drops that line. A change of the
    // policy file is level 3 by the floor of the core.
    let strict = "stemma-policy 1\nlevel 0 docs/\nlevel 0 tests/\napprovals 3 1\n";
    let lowered = "stemma-policy 1\nlevel 0 docs/\nlevel 0 tests/\nlevel 0 src/\n";
    let mut g = gated_with(strict, &[(policy::FILE_NAME, lowered)]);
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();

    // At level 3 the human must be an owner: the approval of Ana, a member, does not count.
    g.approve(g.head, &ana());
    assert_eq!(
        refused(g.land(merged)),
        GateRefusal::NotApproved {
            level: RiskLevel::Three,
            source: LevelSource::Paths,
            agents: 0,
            agents_needed: 1,
            humans: 0,
            humans_needed: 1,
        }
    );
    // With the owner, the agent that the old policy asks for is still missing.
    g.approve(g.head, &daniel());
    assert!(matches!(
        refused(g.land(merged)),
        GateRefusal::NotApproved {
            agents: 0,
            agents_needed: 1,
            humans: 1,
            ..
        }
    ));
    g.approve(g.head, &bot());
    assert_eq!(verdict(g.land(merged)), None);
}

/// A namespace whose policy is `policy`, at a commit of the namespace repository whose register
/// Daniel owns.
fn namespace_of(policy: &str) -> Namespace {
    let policy = Policy::parse(policy.as_bytes()).unwrap();
    let register = Register::with_owner(human("daniel"), daniel().public()).unwrap();
    let commit = commit_in(
        Hub::new().repository.objects(),
        "daniel",
        &[(policy::NAMESPACE_FILE_NAME, policy.encode().as_str())],
        vec![],
    );
    Namespace::new(commit, policy, &register)
}

#[test]
fn the_namespace_policy_of_the_context_counts_beside_the_policy_of_the_tip() {
    let mut g = gated();
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();
    // A namespace policy that raises `docs/` to level 2.
    *g.wire.side().namespace.borrow_mut() = Some(namespace_of("stemma-policy 1\nlevel 2 docs/\n"));
    assert!(matches!(
        refused(g.land(merged)),
        GateRefusal::NotApproved {
            level: RiskLevel::Two,
            ..
        }
    ));
    // A namespace policy with the stricter bound of the unverified window.
    *g.wire.side().namespace.borrow_mut() = Some(namespace_of(
        "stemma-policy 1\nlevel 0 docs/\nlevel 0 tests/\nmax-unverified 1\n",
    ));
    assert_eq!(
        refused(g.land(merged)),
        GateRefusal::Unverified { count: 1, max: 1 }
    );
    // A namespace policy that names `docs/` at level 0 and asks for nothing more: the receipt
    // names the commit of `T` and the commit of the namespace policy.
    let namespace = namespace_of("stemma-policy 1\nlevel 0 docs/\nlevel 0 tests/\n");
    let commit = namespace.commit();
    *g.wire.side().namespace.borrow_mut() = Some(namespace);
    let receipt = g.land(merged).unwrap();
    assert_eq!(receipt.policies(), [g.root, commit]);
}

// The namespace policy and the owners that it demands (ADR-027, STEMMA-311).
mod namespace;

#[test]
fn the_authorizer_is_asked_for_main_before_the_gate() {
    let mut g = gated();
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();
    g.wire.side().denied.set(Some("push"));
    assert_eq!(
        verdict(g.land(merged)),
        Some(RefusalReason::Denied {
            operation: RefOperation::Push
        })
    );
    assert_eq!(g.tip(), g.root);
}

#[test]
fn a_hub_without_statements_or_with_unknown_evidence_holds_no_pull_request() {
    let mut g = gated();
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();
    g.wire.side().no_statements.set(true);
    assert_eq!(refused(g.land(merged)), GateRefusal::NoPull);
    g.wire.side().no_statements.set(false);
    // Evidence that names a held statement which is no opening.
    let approval = Approval::new(Some(repository_id()), g.pull, g.head);
    let approval = Statement::sign(Body::Approval(approval), &bot())
        .unwrap()
        .hash();
    let moved = to(&g.wire, "heads/main", Some(merged), Some(approval));
    assert_eq!(
        refused(push(&mut g.wire, &daniel(), vec![moved])),
        GateRefusal::NoPull
    );
    assert_eq!(verdict(g.land(merged)), None);
}

#[test]
fn a_branch_that_moved_after_the_opening_is_refused() {
    let mut g = gated();
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();
    let next = g
        .wire
        .side()
        .commit("mia", &[("docs/a.md", "b\n")], vec![g.head]);
    let moved = to(&g.wire, "heads/feature", Some(next), None);
    push(&mut g.wire, &mia(), vec![moved]).unwrap();
    assert_eq!(
        refused(g.land(merged)),
        GateRefusal::BranchMoved { head: Some(next) }
    );
}

#[test]
fn a_red_tip_pauses_level_2_and_a_change_of_level_0_lands() {
    let mut g = gated_with(POLICY, &[("src/lib.rs", "pub fn two() {}\n")]);
    g.approve_all();
    g.check(g.head, Some(g.root), CheckResult::Pass);
    g.check(g.root, None, CheckResult::Fail);
    let merged = g.merge();

    assert_eq!(refused(g.land(merged)), GateRefusal::RedTip { red: g.root });

    let mut zero = gated();
    zero.approve(zero.head, &bot());
    zero.check(zero.head, Some(zero.root), CheckResult::Pass);
    zero.check(zero.root, None, CheckResult::Fail);
    let merged = zero.merge();
    assert_eq!(verdict(zero.land(merged)), None);
}

#[test]
fn the_unverified_window_stops_every_merge() {
    let mut g = gated_with(
        "stemma-policy 1\nlevel 0 docs/\nmax-unverified 1\n",
        &[("docs/a.md", "a\n")],
    );
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();

    assert_eq!(
        refused(g.land(merged)),
        GateRefusal::Unverified { count: 1, max: 1 }
    );
    // A run on the tip covers it.
    g.check(g.root, None, CheckResult::Pass);
    assert_eq!(verdict(g.land(merged)), None);
}

#[test]
fn a_reservation_of_another_change_holds_the_merge_back_until_it_ends() {
    let mut g = gated_with(
        "stemma-policy 1\nlevel 0 docs/\nreservation 1\n",
        &[("docs/a.md", "a\n")],
    );
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    // Ana opens a change of level 2, and its one run lost to a move of `main`: it named
    // another base than the tip.
    let other = g.wire.side().commit(
        "ana",
        &[
            (register::FILE, register_of().encode().as_str()),
            (
                policy::FILE_NAME,
                "stemma-policy 1\nlevel 0 docs/\nreservation 1\n",
            ),
            ("tests/t.rs", TEST),
            ("src/lib.rs", "pub fn two() {}\n"),
        ],
        vec![g.root],
    );
    let moved = to(&g.wire, "heads/other", Some(other), None);
    push(&mut g.wire, &ana(), vec![moved]).unwrap();
    let opening = Opening::try_new(
        Some(repository_id()),
        crate::refs::BranchName::try_new("other").unwrap(),
        other,
        Vec::new(),
        None,
    )
    .unwrap();
    let holder = submit(&mut g.wire, Body::Opening(opening), &ana());
    g.wire.side().time.set(START + 100);
    let checked = Checked::try_new(
        Some(repository_id()),
        other,
        Some(g.head),
        CheckResult::Pass,
    )
    .unwrap();
    submit(&mut g.wire, Body::Check(checked), &mia());
    g.wire.side().time.set(START + 150);
    let merged = g.merge();

    // The run took 100 seconds from the opening, so the reservation lasts 200 seconds.
    assert_eq!(
        refused(g.land(merged)),
        GateRefusal::Reserved {
            pull: holder,
            until: START + 300,
        }
    );
    g.wire.side().time.set(START + 300);
    assert_eq!(verdict(g.land(merged)), None);
}

#[test]
fn the_local_gate_and_the_hub_reach_the_same_verdict_on_the_same_inputs() {
    // The same commits, the same register and policy, and the same statements, once in a clone
    // and once on a hub: first without an approval, then with one of `bot`.
    let mut g = gated();
    g.check(g.head, Some(g.root), CheckResult::Pass);

    let dir = TempDir::new();
    let clone = Repository::init(
        dir.path(),
        &InitOptions::default(),
        &Actor::human("daniel").unwrap(),
        &AllowAll,
    )
    .unwrap();
    let register = register_of().encode();
    let files = [
        (register::FILE, register.as_str()),
        (policy::FILE_NAME, POLICY),
        ("tests/t.rs", TEST),
        ("src/lib.rs", "pub fn one() {}\n"),
    ];
    let root = commit_in(clone.objects(), "daniel", &files, vec![]);
    let mut changed = files.to_vec();
    changed.push(("docs/a.md", "a\n"));
    let head = commit_in(clone.objects(), "mia", &changed, vec![root]);
    let operation = crate::oplog::OperationName::try_new("branch").unwrap();
    let daniel_actor = Actor::human("daniel").unwrap();
    for (branch, commit) in [("main", root), ("feature", head)] {
        let branch = crate::refs::BranchName::try_new(branch).unwrap();
        clone
            .update_branch(&operation, &branch, None, commit, &daniel_actor, &AllowAll)
            .unwrap();
    }
    let number = by_hand::open(
        &clone,
        &Actor::human("mia").unwrap(),
        &crate::refs::BranchName::try_new("feature").unwrap(),
        head,
        None,
        Some(&mia() as &dyn Signer),
    )
    .number();
    by_hand::check(
        &clone,
        &Actor::human("ana").unwrap(),
        head,
        Some(root),
        CheckResult::Pass,
        Some(&ana() as &dyn Signer),
    );
    let mode = register::Mode::Enforcing(register_of());
    let local = || clone.merge_gate_by_policy(number, &mode, &daniel_actor, &AllowAll);

    // Without an approval: the same refusal with the same facts.
    let merged = g.merge();
    let on_hub = refused(g.land(merged));
    let GateRefusal::NotApproved {
        level,
        source,
        agents,
        agents_needed,
        humans,
        humans_needed,
    } = on_hub
    else {
        panic!("{on_hub:?}");
    };
    match local().unwrap_err() {
        GateError::NotApproved {
            level: local_level,
            source: local_source,
            agents: local_agents,
            agents_needed: local_needed,
            humans: local_humans,
            humans_needed: local_humans_needed,
            ..
        } => assert_eq!(
            (
                local_level,
                local_source,
                local_agents,
                local_needed,
                local_humans,
                local_humans_needed
            ),
            (level, source, agents, agents_needed, humans, humans_needed)
        ),
        other => panic!("{other}"),
    }

    // With the approval of `bot`: both let the change land.
    g.approve(g.head, &bot());
    let bot_actor = Actor::agent("bot", "model", "session", "daniel").unwrap();
    by_hand::approve(
        &clone,
        &bot_actor,
        number,
        head,
        Some(&bot() as &dyn Signer),
    );
    assert_eq!(verdict(g.land(merged)), None);
    let landed = local().unwrap();
    assert_eq!(landed.tip, root);
    // The merge of the clone has the tree that the hub accepted.
    let tree_of = |store: &dyn ObjectStore, commit: CommitId| {
        store.get_commit(&commit).unwrap().unwrap().tree()
    };
    assert_eq!(
        tree_of(clone.objects(), landed.commit),
        tree_of(g.wire.side().repository.objects(), merged)
    );
}
