//! The namespace policy on a hub (ADR-027, STEMMA-311): it counts beside the policy of `T` and
//! never lowers it, and the owners that it demands stand in the register of every new commit of
//! `main`.

use super::*;

/// The key of the teacher, the owner of the namespace.
fn teacher() -> Seeded {
    Seeded::new(8)
}

/// A namespace of the teacher whose policy is `policy`.
fn course(policy: &str) -> Namespace {
    let policy = Policy::parse(policy.as_bytes()).unwrap();
    let register = Register::with_owner(human("teacher"), teacher().public()).unwrap();
    let commit = commit_in(
        Hub::new().repository.objects(),
        "teacher",
        &[(policy::NAMESPACE_FILE_NAME, policy.encode().as_str())],
        vec![],
    );
    Namespace::new(commit, policy, &register).unwrap()
}

const DEMANDING: &str = "stemma-policy 2\ndemand-namespace-owners\nlevel 0 docs/\nlevel 0 tests/\n";

/// The register of the tests with the teacher as a further owner.
fn with_teacher() -> Register {
    let mut register = register_of();
    register
        .add_human(human("teacher"), Role::Owner, teacher().public())
        .unwrap();
    register
}

#[test]
fn a_namespace_policy_never_lowers_the_level_of_the_policy_of_the_tip() {
    // The policy of `T` gives `src/` level 2; the namespace policy gives it level 0.
    let mut g = gated_with(POLICY, &[("src/lib.rs", "pub fn two() {}\n")]);
    *g.wire.side().namespace.borrow_mut() = Some(course(
        "stemma-policy 2\nlevel 0 src/\nlevel 0 docs/\nlevel 0 tests/\n",
    ));
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();
    assert!(matches!(
        refused(g.land(merged)),
        GateRefusal::NotApproved {
            level: RiskLevel::Two,
            ..
        }
    ));
    // With the approvals of level 2 it lands.
    g.approve(g.head, &cat());
    g.approve(g.head, &daniel());
    assert_eq!(verdict(g.land(merged)), None);
}

#[test]
fn a_change_of_the_namespace_policy_file_in_a_repository_is_level_3() {
    let mut g = gated_with(
        POLICY,
        &[(policy::NAMESPACE_FILE_NAME, "stemma-policy 1\n")],
    );
    g.approve(g.head, &bot());
    g.approve(g.head, &cat());
    g.approve(g.head, &ana());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();
    assert!(matches!(
        refused(g.land(merged)),
        GateRefusal::NotApproved {
            level: RiskLevel::Three,
            ..
        }
    ));
}

#[test]
fn an_update_of_main_whose_register_lacks_a_demanded_owner_is_refused() {
    let mut g = gated();
    *g.wire.side().namespace.borrow_mut() = Some(course(DEMANDING));
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();
    assert_eq!(
        verdict(g.land(merged)),
        Some(RefusalReason::NamespaceOwner(human("teacher")))
    );
    assert_eq!(g.tip(), g.root);

    // The register is brought into line through the gate, at level 3: the merge holds the
    // teacher, so the demand holds for it already.
    let tip = g.land_register(&with_teacher(), None);
    assert_eq!(g.tip(), tip);
}

#[test]
fn a_first_push_whose_register_lacks_a_demanded_owner_is_refused() {
    let hub = Hub::new();
    *hub.namespace.borrow_mut() = Some(course(DEMANDING));
    let without = register_of().encode();
    let root = hub.commit("daniel", &base_files(&without, POLICY), vec![]);
    let with = with_teacher().encode();
    let held = hub.commit("daniel", &base_files(&with, POLICY), vec![]);
    let mut wire = MemoryTransport::new(hub);

    let moved = to(&wire, "heads/main", Some(root), None);
    assert_eq!(
        verdict(push(&mut wire, &daniel(), vec![moved])),
        Some(RefusalReason::NamespaceOwner(human("teacher")))
    );
    assert_eq!(wire.side().main(), None);

    let moved = to(&wire, "heads/main", Some(held), None);
    let receipt = push(&mut wire, &daniel(), vec![moved]).unwrap();
    // The verdict rests on the namespace policy, so the receipt names its commit.
    let commit = wire.side().namespace.borrow().as_ref().unwrap().commit();
    assert_eq!(receipt.policies(), [commit]);
    assert_eq!(wire.side().main(), Some(held));
}

#[test]
fn a_demanded_owner_counts_with_its_key_not_by_its_name() {
    let hub = Hub::new();
    *hub.namespace.borrow_mut() = Some(course(DEMANDING));
    // A register that names the teacher as an owner with a key of its own choice.
    let mut register = register_of();
    register
        .add_human(human("teacher"), Role::Owner, Seeded::new(9).public())
        .unwrap();
    let text = register.encode();
    let root = hub.commit("daniel", &base_files(&text, POLICY), vec![]);
    let mut wire = MemoryTransport::new(hub);
    let moved = to(&wire, "heads/main", Some(root), None);
    assert_eq!(
        verdict(push(&mut wire, &daniel(), vec![moved])),
        Some(RefusalReason::NamespaceOwner(human("teacher")))
    );
}

#[test]
fn a_namespace_policy_without_the_demand_lets_any_register_through() {
    let hub = Hub::new();
    *hub.namespace.borrow_mut() = Some(course("stemma-policy 2\nlevel 0 docs/\n"));
    let text = register_of().encode();
    let root = hub.commit("daniel", &base_files(&text, POLICY), vec![]);
    let mut wire = MemoryTransport::new(hub);
    let moved = to(&wire, "heads/main", Some(root), None);
    assert!(push(&mut wire, &daniel(), vec![moved]).is_ok());
}

/// A check of `writer` for the head of `g` on its root.
fn check_by(g: &mut Gated, writer: &Seeded) {
    let checked = Checked::try_new(
        Some(repository_id()),
        g.head,
        Some(g.root),
        CheckResult::Pass,
    )
    .unwrap();
    submit(&mut g.wire, Body::Check(checked), writer);
}

#[test]
fn a_level_3_change_lands_in_a_repository_with_a_single_human_owner() {
    // The register of the tests has one human owner, Daniel, as a course has one teacher. The
    // agent `bot` of Daniel writes and opens the change of the namespace policy, Daniel approves
    // it, and the agent `cat` of Daniel records the check: the rule of phase 4 holds unchanged.
    let change = [(
        policy::NAMESPACE_FILE_NAME,
        "stemma-policy 2\nlevel 3 grading/\n",
    )];
    let mut g = gated_by(POLICY, &change, &bot());
    g.approve(g.head, &daniel());
    check_by(&mut g, &cat());
    let merged = g.merge();
    assert_eq!(verdict(g.land(merged)), None);
    assert_eq!(g.tip(), merged);
}

#[test]
fn a_level_3_change_that_the_single_owner_writes_needs_another_owner() {
    // Daniel writes and opens the change himself: his approval is a self-approval, and no
    // other owner can give the one that level 3 asks for. A check of one of his agents does not
    // count either, as their sponsor opened the change; Ana, a member, records it.
    let change = [(
        policy::NAMESPACE_FILE_NAME,
        "stemma-policy 2\nlevel 3 grading/\n",
    )];
    let mut g = gated_by(POLICY, &change, &daniel());
    for signer in [bot(), cat()] {
        g.approve(g.head, &signer);
    }
    // The hub does not even hold the approval of Daniel.
    let approval = Approval::new(Some(repository_id()), g.pull, g.head);
    let statement = Statement::sign(Body::Approval(approval), &daniel()).unwrap();
    let refused_approval = matches!(
        g.wire
            .submit_statement(&mut statement.encode().as_slice())
            .unwrap(),
        Answer::Error(_)
    );
    assert!(refused_approval);
    check_by(&mut g, &ana());
    let merged = g.merge();
    assert!(matches!(
        refused(g.land(merged)),
        GateRefusal::NotApproved {
            level: RiskLevel::Three,
            humans: 0,
            humans_needed: 1,
            ..
        }
    ));
}

#[test]
fn the_local_gate_and_the_accepting_operation_agree_on_the_level() {
    use crate::protocol::record::HubKeys;
    use crate::protocol::{FormerKeys, RecordHash};
    use crate::remote::{RemoteAddress, RemoteName};
    use crate::repository::RemoteSetup;
    use crate::serve::{Answering, ListFacts};

    // The policy of `T` gives `docs/` level 0, the namespace policy level 2.
    let namespace = course("stemma-policy 2\nlevel 2 docs/\nlevel 0 tests/\n");
    let mut g = gated();
    *g.wire.side().namespace.borrow_mut() = Some(namespace.clone());
    g.approve(g.head, &bot());
    g.check(g.head, Some(g.root), CheckResult::Pass);
    let merged = g.merge();
    let on_hub = match refused(g.land(merged)) {
        GateRefusal::NotApproved { level, .. } => level,
        other => panic!("{other:?}"),
    };
    assert_eq!(on_hub, RiskLevel::Two);

    // A clone fetches from the same hub, whose list names the namespace policy.
    let dir = TempDir::new();
    let keys = HubKeys::try_new(hub_key().public(), FormerKeys::none()).unwrap();
    let entry: RecordHash = format!("b3:{}", "a1".repeat(32)).parse().unwrap();
    let facts =
        ListFacts::new(repository_id(), &keys, move |_| Some(entry)).with_namespace(&namespace);
    let hub = &g.wire.side().repository;
    let mut transport = MemoryTransport::new(Answering::new(hub, facts));
    let origin = RemoteName::try_new("origin").unwrap();
    let (clone, _) = Repository::clone_remote(
        dir.path().join("clone"),
        &InitOptions::default(),
        &origin,
        &RemoteSetup::new(RemoteAddress::try_new("https://hub.example/course/work").unwrap()),
        &mut transport,
        &Actor::human("daniel").unwrap(),
        &AllowAll,
    )
    .unwrap();

    // The local gate combines the policy of `T` with the kept namespace policies, and the level
    // of the same change is the level of the hub.
    let mut policies = vec![clone.policy_at(&g.root, true).unwrap()];
    policies.extend(clone.kept_namespace_policies().unwrap());
    // The merge was refused, so the clone has its tree as the tree of the head.
    let tree = clone.objects().get_commit(&g.head).unwrap().unwrap().tree();
    let (locally, _) = clone.level_by(&policies, &g.root, tree).unwrap();
    assert_eq!(locally, on_hub);
    // Without the namespace policy the clone would have judged level 0.
    let (alone, _) = clone.level_by(&policies[..1], &g.root, tree).unwrap();
    assert_eq!(alone, RiskLevel::Zero);
}
