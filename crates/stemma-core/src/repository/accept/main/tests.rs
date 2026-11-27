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
use crate::object::{Blob, Commit, CommitId, FileMode};
use crate::oplog::CheckResult;
use crate::policy::RiskLevel;
use crate::protocol::held::HeldStatement;
use crate::protocol::record::tests::Seeded;
use crate::protocol::record::{
    read_record, HubAddress, Move, RecordEntry, RefUpdate, RefUpdatePayload, StatementHash,
};
use crate::protocol::refusal::{GateRefusal, LevelSource, RefusalReason, Refused};
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
use crate::{policy, Actor, AllowAll};

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
        let context = AcceptContext::new(
            &hub,
            repository_id(),
            &mut *record,
            self.time.get(),
            &signer,
        )
        .with_statements(&mut *statements);
        self.repository
            .accept_ref_update(update, context)
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
    push(&mut wire, &mia(), vec![moved]).unwrap();
    let opening = Opening::try_new(
        Some(repository_id()),
        crate::refs::BranchName::try_new("feature").unwrap(),
        head,
        Vec::new(),
        None,
    )
    .unwrap();
    let pull = submit(&mut wire, Body::Opening(opening), &mia());
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
    // The opener's own approval does not count, and neither does a human one for an agent.
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
    // A second pull request of Mia revokes the key of `bot` and lands through the gate: level 3,
    // which an owner approves.
    let mut revoked = register_of();
    revoked.revoke(&agent("bot"), &bot().public(), 1).unwrap();
    let revoked = revoked.encode();
    let keys = g.wire.side().commit(
        "mia",
        &[
            (register::FILE, revoked.as_str()),
            (policy::FILE_NAME, POLICY),
            ("tests/t.rs", TEST),
            ("src/lib.rs", "pub fn one() {}\n"),
        ],
        vec![g.root],
    );
    let moved = to(&g.wire, "heads/keys", Some(keys), None);
    push(&mut g.wire, &mia(), vec![moved]).unwrap();
    let opening = Opening::try_new(
        Some(repository_id()),
        crate::refs::BranchName::try_new("keys").unwrap(),
        keys,
        Vec::new(),
        None,
    )
    .unwrap();
    let keys_pull = submit(&mut g.wire, Body::Opening(opening), &mia());
    let approval = Approval::new(Some(repository_id()), keys_pull, keys);
    submit(&mut g.wire, Body::Approval(approval), &daniel());
    g.check(keys, Some(g.root), CheckResult::Pass);
    let tree = g
        .wire
        .side()
        .repository
        .objects()
        .get_commit(&keys)
        .unwrap()
        .unwrap()
        .tree();
    let landed = Commit::try_new(
        tree,
        vec![g.root],
        ChangeId::generate().unwrap(),
        Actor::human("daniel").unwrap(),
        2,
        "Merge",
    )
    .unwrap();
    let landed = g
        .wire
        .side()
        .repository
        .objects()
        .put_commit(&landed)
        .unwrap();
    let moved = to(&g.wire, "heads/main", Some(landed), Some(keys_pull));
    push(&mut g.wire, &daniel(), vec![moved]).unwrap();

    // The approval of `bot` came before the revocation and is void all the same.
    let merged = g.wire.side().commit(
        "daniel",
        &[
            (register::FILE, revoked.as_str()),
            (policy::FILE_NAME, POLICY),
            ("tests/t.rs", TEST),
            ("src/lib.rs", "pub fn one() {}\n"),
            ("docs/a.md", "a\n"),
        ],
        vec![landed],
    );
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
    g.approve(g.head, &cat());
    g.approve(g.head, &daniel());
    assert_eq!(verdict(g.land(merged)), None);
}

#[test]
fn a_merge_that_lowers_the_policy_is_judged_by_the_old_one() {
    // The change makes every path level 0 and edits `src/lib.rs`, which is level 2 by the
    // policy of `T`; the policy file itself is level 3.
    let lowered = "stemma-policy 1\nlevel 0 docs/\nlevel 0 tests/\nlevel 0 src/\n";
    let mut g = gated_with(
        POLICY,
        &[
            (policy::FILE_NAME, lowered),
            ("src/lib.rs", "pub fn two() {}\n"),
        ],
    );
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();

    assert_eq!(
        refused(g.land(merged)),
        GateRefusal::NotApproved {
            level: RiskLevel::Three,
            source: LevelSource::Paths,
            agents: 1,
            agents_needed: 0,
            humans: 0,
            humans_needed: 1,
        }
    );
    g.approve(g.head, &daniel());
    assert_eq!(verdict(g.land(merged)), None);
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
}
