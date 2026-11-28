//! The tests of the anchors in a clone (ADR-026, STEMMA-300). The hub is played by a test key
//! that seals the held anchors; its side has tests of its own (`accept/anchor/tests.rs`).

use super::*;
use crate::allowed::Allowed;
use crate::failure::Classify;
use crate::index::Index;
use crate::object::{Blob, Commit, FileMode};
use crate::oplog::{Change, Moved, OperationName};
use crate::protocol::anchor::{AnchorLines, AnchorState};
use crate::protocol::record::tests::Seeded;
use crate::protocol::{RecordHash, RepositoryId};
use crate::refs::{BranchName, RefKind};
use crate::register::{self, Bound};
use crate::remote::{self, RemoteAddress, RemoteConfig, RemoteRef, RemoteRefName};
use crate::repository::InitOptions;
use crate::store::ObjectStore;
use crate::testutil::TempDir;
use crate::{Actor, AllowAll};

const SECRET: &str = "key=hunter2";

fn actor() -> Actor {
    Actor::human("daniel").unwrap()
}

fn signer() -> Seeded {
    Seeded::new(1)
}

fn hub() -> Seeded {
    Seeded::new(2)
}

fn origin() -> RemoteName {
    RemoteName::try_new("origin").unwrap()
}

fn repository_id() -> RepositoryId {
    "00112233445566778899aabbccddeeff".parse().unwrap()
}

fn address() -> HubAddress {
    HubAddress::try_new("https://hub.example").unwrap()
}

/// A repository with the remote `origin`, whose configuration pinned the key of [`hub`].
fn repository(dir: &TempDir) -> Repository {
    let repository =
        Repository::init(dir.path(), &InitOptions::default(), &actor(), &AllowAll).unwrap();
    let mut config = RemoteConfig::new(
        RemoteAddress::try_new("https://hub.example/stemma").unwrap(),
        repository_id(),
    );
    config.hub_key = Some(hub().public());
    let metadata = repository.metadata_dir();
    remote::write_config(&metadata, &metadata.join("tmp"), &origin(), &config).unwrap();
    repository
}

fn write(dir: &TempDir, path: &str, content: &str) {
    let path = dir.path().join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn commit(dir: &TempDir, repository: &Repository, path: &str, text: &str) -> CommitId {
    write(dir, path, text);
    repository
        .track(&[path.to_owned()], &actor(), &AllowAll)
        .unwrap();
    repository.commit(text, &actor(), &AllowAll).unwrap().commit
}

/// What the hub answers to `anchor`: the held anchor in `state`, after the record entry
/// `record`, sealed by `key`.
fn held_by(
    key: &Seeded,
    anchor: Anchor,
    number: u64,
    record: u64,
    state: AnchorState,
) -> HeldAnchor {
    let lines = AnchorLines {
        number,
        time: 1_000,
        register: CommitId::new(
            "b3:1111111111111111111111111111111111111111111111111111111111111111"
                .parse()
                .unwrap(),
        ),
        record,
        actor: Identity::of(&actor()),
        state,
    };
    HeldAnchor::seal(anchor, lines, key).unwrap()
}

/// Anchors the log and keeps the confirmation, as a client does with the answer of the hub.
fn confirm(repository: &Repository, number: u64) -> HeldAnchor {
    let anchor = repository.anchor(&origin(), address(), &signer()).unwrap();
    let held = held_by(&hub(), anchor, number, number, AnchorState::Confirmed);
    repository.keep_confirmation(&origin(), &held).unwrap();
    held
}

fn format_of(repository: &Repository) -> String {
    fs::read_to_string(repository.metadata_dir().join("format")).unwrap()
}

#[test]
fn init_writes_the_id_of_the_log_and_an_older_repository_gets_one_when_first_needed() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    let path = repository.metadata_dir().join(LOG_ID_FILE);
    let text = fs::read_to_string(&path).unwrap();
    let log = repository.log_id().unwrap();
    assert_eq!(text, format!("stemma-log-id 1\n{log}\n"));
    assert_eq!(repository.log_id().unwrap(), log);
    assert_eq!(format_of(&repository), "stemma-repository 22\n");

    // A repository of version 18 has no ID; it gets one, and the format rises.
    fs::remove_file(&path).unwrap();
    fs::write(
        repository.metadata_dir().join("format"),
        "stemma-repository 18\n",
    )
    .unwrap();
    let new = repository.log_id().unwrap();
    assert_ne!(new, log);
    assert_eq!(repository.log_id().unwrap(), new);
    assert_eq!(format_of(&repository), "stemma-repository 22\n");

    // Another clone has another ID.
    let other = TempDir::new();
    assert_ne!(self::repository(&other).log_id().unwrap(), new);

    // From version 19 on a missing file is damage: the rise to 19 writes it.
    fs::remove_file(&path).unwrap();
    let error = repository.log_id().unwrap_err();
    assert!(
        matches!(error, LogAnchorError::LogIdMissing { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("was removed"), "{error}");
    assert_eq!(error.kind(), crate::failure::FailureKind::Damaged);
    // Any rise across 19, such as the next entry, gives a repository of 18 its ID.
    fs::write(
        repository.metadata_dir().join("format"),
        "stemma-repository 18\n",
    )
    .unwrap();
    commit(&dir, &repository, "a.txt", "one\n");
    assert!(path.exists());
    assert_eq!(format_of(&repository), "stemma-repository 22\n");

    // A file that is not in canonical form is damage, not a reason for a new ID.
    fs::write(&path, format!("stemma-log-id 1\n{}\n", "A".repeat(32))).unwrap();
    assert!(matches!(
        repository.log_id(),
        Err(LogAnchorError::LogIdDamaged { .. })
    )); // So is a file over the limit, which is not read as a failure to read.
    fs::write(&path, "x".repeat(70 * 1024)).unwrap();
    let error = repository.log_id().unwrap_err();
    assert!(
        matches!(error, LogAnchorError::LogIdDamaged { .. }),
        "{error}"
    );
    assert_eq!(error.kind(), crate::failure::FailureKind::Damaged);
}

#[test]
fn an_anchor_names_the_tip_and_the_last_confirmation_and_the_confirmation_is_kept() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    commit(&dir, &repository, "a.txt", "one\n");
    let entries = repository.oplog().entries().unwrap();
    let last = entries.last().unwrap();
    let first = repository.anchor(&origin(), address(), &signer()).unwrap();
    let payload = first.payload();
    assert_eq!(payload.log(), repository.log_id().unwrap());
    assert_eq!(payload.repository(), repository_id());
    assert_eq!(payload.tip().number, last.number());
    assert_eq!(Some(payload.tip().hash), last.hash());
    assert_eq!(payload.confirmed(), None);
    assert_eq!(repository.last_confirmation(&origin()).unwrap(), None);

    let held = held_by(&hub(), first, 1, 1, AnchorState::Confirmed);
    repository.keep_confirmation(&origin(), &held).unwrap();
    assert_eq!(
        repository.last_confirmation(&origin()).unwrap(),
        Some(held.clone())
    );

    commit(&dir, &repository, "a.txt", "two\n");
    let second = repository.anchor(&origin(), address(), &signer()).unwrap();
    assert_eq!(second.payload().confirmed(), Some(held.tip()));
    assert!(second.payload().tip().number > held.tip().number);
}

#[test]
fn a_confirmation_that_does_not_hold_is_not_kept() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    commit(&dir, &repository, "a.txt", "one\n");
    let anchor = repository.anchor(&origin(), address(), &signer()).unwrap();
    let contradicts = held_by(&hub(), anchor.clone(), 1, 1, AnchorState::Contradicts);
    assert!(matches!(
        repository.keep_confirmation(&origin(), &contradicts),
        Err(LogAnchorError::NotConfirmed)
    ));
    let foreign = held_by(
        &Seeded::new(9),
        anchor.clone(),
        1,
        1,
        AnchorState::Confirmed,
    );
    assert!(matches!(
        repository.keep_confirmation(&origin(), &foreign),
        Err(LogAnchorError::Confirmation(AnchorFinding::HubKey {
            position: 1
        }))
    ));
    // The anchor of another log.
    let other = TempDir::new();
    let elsewhere = self::repository(&other);
    commit(&other, &elsewhere, "a.txt", "one\n");
    let theirs = elsewhere.anchor(&origin(), address(), &signer()).unwrap();
    let theirs = held_by(&hub(), theirs, 1, 1, AnchorState::Confirmed);
    assert!(matches!(
        repository.keep_confirmation(&origin(), &theirs),
        Err(LogAnchorError::Confirmation(AnchorFinding::Other {
            position: 1
        }))
    ));
    // Another repository, and a remote without a pinned key.
    let foreign_repository = AnchorPayload::try_new(
        address(),
        "ffffffffffffffffffffffffffffffff".parse().unwrap(),
        repository.log_id().unwrap(),
        anchor.payload().tip(),
        None,
        signer().public(),
    )
    .unwrap()
    .sign(&signer())
    .unwrap();
    let foreign_repository = held_by(&hub(), foreign_repository, 1, 1, AnchorState::Confirmed);
    assert!(matches!(
        repository.keep_confirmation(&origin(), &foreign_repository),
        Err(LogAnchorError::Confirmation(AnchorFinding::Other {
            position: 1
        }))
    ));
    let unpinned = RemoteName::try_new("unpinned").unwrap();
    let metadata = repository.metadata_dir();
    remote::write_config(
        &metadata,
        &metadata.join("tmp"),
        &unpinned,
        &RemoteConfig::new(
            RemoteAddress::try_new("https://hub.example/stemma").unwrap(),
            repository_id(),
        ),
    )
    .unwrap();
    let ours = held_by(&hub(), anchor.clone(), 1, 1, AnchorState::Confirmed);
    assert!(matches!(
        repository.keep_confirmation(&unpinned, &ours),
        Err(LogAnchorError::HubKey(_))
    ));
    // The same confirmation twice is kept twice without harm.
    repository.keep_confirmation(&origin(), &ours).unwrap();
    repository.keep_confirmation(&origin(), &ours).unwrap();
    // An earlier entry than the kept one.
    let early = held_by(&hub(), anchor, 1, 1, AnchorState::Confirmed);
    commit(&dir, &repository, "a.txt", "two\n");
    confirm(&repository, 2);
    assert!(matches!(
        repository.keep_confirmation(&origin(), &early),
        Err(LogAnchorError::Older { .. })
    ));
    assert!(
        repository
            .last_confirmation(&origin())
            .unwrap()
            .unwrap()
            .tip()
            .number
            > 1
    );
}

#[test]
fn an_anchor_after_a_purge_still_checks() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    commit(&dir, &repository, "a.txt", "one\n");
    commit(&dir, &repository, "config/key", SECRET);
    commit(&dir, &repository, "a.txt", "two\n");
    let confirmed = confirm(&repository, 1);
    let secret = Blob::new(SECRET).id().object_id();
    repository.apply_purge(secret, &actor(), &AllowAll).unwrap();
    let entries = repository.oplog().entries().unwrap();
    let stubbed = entries
        .iter()
        .find(|entry| entry.number() == confirmed.tip().number)
        .unwrap();
    assert!(
        matches!(stubbed.record(), Record::Stub(_)),
        "the confirmed entry is a stub now: {stubbed:?}"
    );
    // The stub keeps the hash, so the anchor holds and the next one names it.
    let check = repository
        .verify_anchors(&origin(), std::slice::from_ref(&confirmed))
        .unwrap();
    assert_eq!(check.confirmed, 1);
    assert_eq!(check.last, Some(confirmed.tip()));
    let next = repository.anchor(&origin(), address(), &signer()).unwrap();
    assert_eq!(next.payload().confirmed(), Some(confirmed.tip()));
}

/// Cuts the log after the entry `keep`, and writes the tip file again, as whoever can write
/// `.stemma` can: the chain of what is left holds.
fn cut(repository: &Repository, keep: u64) {
    let entries = repository.oplog().entries().unwrap();
    let dir = repository.metadata_dir().join("oplog");
    for entry in entries.iter().filter(|entry| entry.number() > keep) {
        fs::remove_file(dir.join(format!("{:012}", entry.number()))).unwrap();
    }
    let last = entries.iter().find(|entry| entry.number() == keep).unwrap();
    fs::write(
        repository.metadata_dir().join("oplog-tip"),
        format!(
            "stemma-oplog-tip 2\nnumber {keep}\nhash {}\n",
            last.hash().unwrap()
        ),
    )
    .unwrap();
}

#[test]
fn a_cut_log_is_found() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    commit(&dir, &repository, "a.txt", "one\n");
    let first = repository.oplog().entries().unwrap().len() as u64;
    commit(&dir, &repository, "a.txt", "two\n");
    let confirmed = confirm(&repository, 1);
    cut(&repository, first);
    assert!(repository.oplog().verify().is_ok(), "the chain still holds");
    let error = repository
        .verify_anchors(&origin(), std::slice::from_ref(&confirmed))
        .unwrap_err();
    assert!(
        matches!(
            &error,
            LogAnchorError::Confirmation(AnchorFinding::Cut { anchor, last })
                if *anchor == confirmed.tip() && *last == Some(first)
        ),
        "{error}"
    );
    assert!(error.to_string().contains("its tail was cut"), "{error}");
    // The client finds it before it signs the next anchor.
    assert!(matches!(
        repository.anchor(&origin(), address(), &signer()),
        Err(LogAnchorError::Diverged(AnchorFinding::Cut { .. }))
    ));
}

#[test]
fn a_rewritten_entry_and_the_contradictions_of_the_hub_are_found() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    commit(&dir, &repository, "a.txt", "one\n");
    let first = repository.oplog().entries().unwrap().len() as u64;
    commit(&dir, &repository, "a.txt", "two\n");
    let confirmed = confirm(&repository, 1);
    // A restored backup: the log is cut and grows again with other entries.
    cut(&repository, first);
    commit(&dir, &repository, "a.txt", "three\n");
    commit(&dir, &repository, "a.txt", "four\n");
    let error = repository
        .verify_anchors(&origin(), std::slice::from_ref(&confirmed))
        .unwrap_err();
    assert!(
        matches!(
            error,
            LogAnchorError::Confirmation(AnchorFinding::Rewritten { .. })
        ),
        "{error}"
    );
    // What the hub held as contradicting is shown, and blocks nothing.
    let restored = TempDir::new();
    let fresh = self::repository(&restored);
    commit(&restored, &fresh, "a.txt", "one\n");
    let tip = confirm(&fresh, 1);
    // The same number with another hash, as a restored backup of another clone sends it.
    let contradicting = AnchorPayload::try_new(
        address(),
        repository_id(),
        fresh.log_id().unwrap(),
        Point {
            number: tip.tip().number,
            hash: format!("b3:{}", "77".repeat(32)).parse().unwrap(),
        },
        None,
        signer().public(),
    )
    .unwrap()
    .sign(&signer())
    .unwrap();
    let held = [
        tip,
        held_by(&hub(), contradicting, 2, 1, AnchorState::Contradicts),
    ];
    let check = fresh.verify_anchors(&origin(), &held).unwrap();
    assert_eq!(check.confirmed, 1);
    assert_eq!(check.contradictions.len(), 1);
}

/// Stores a commit whose register is `register` in the store of `repository`.
fn register_commit(repository: &Repository, register: &Register) -> CommitId {
    let store = repository.objects();
    let mut index = Index::new();
    let blob = store.put_blob(&Blob::new(register.encode())).unwrap();
    index.insert(register::FILE, FileMode::File, blob).unwrap();
    let commit = Commit::try_new(
        index.write_tree(store).unwrap(),
        Vec::new(),
        crate::change::ChangeId::generate().unwrap(),
        actor(),
        1,
        "register",
    )
    .unwrap();
    store.put_commit(&commit).unwrap()
}

/// Writes a `fetch` entry that brings `commit` as `origin/main`.
fn fetch_entry(repository: &Repository, commit: CommitId) -> u64 {
    let operation = OperationName::try_new(FETCH).unwrap();
    let allowed = Allowed::ask(&AllowAll, &actor(), &operation, "origin").unwrap();
    let lock = repository.refs_store().lock_writer(&allowed).unwrap();
    let name = RemoteRefName {
        remote: origin(),
        kind: RefKind::Branch,
        name: BranchName::try_new("main").unwrap(),
    };
    let value = RemoteRef {
        commit,
        entry: format!("b3:{}", "a1".repeat(32))
            .parse::<RecordHash>()
            .unwrap(),
    };
    let change = Change::try_new_with_remotes(
        1,
        actor(),
        operation,
        vec![(name, Moved::from_to(None, Some(value)))],
    )
    .unwrap();
    repository.append(&lock, &change).unwrap().number()
}

#[test]
fn a_bound_of_another_log_counts_from_the_fetch_entry_that_brought_it() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    let mia = Identity::human("mia").unwrap();
    let key = Seeded::new(3).public();
    let mut register = Register::with_owner(Identity::of(&actor()), signer().public()).unwrap();
    register
        .add_human(mia.clone(), register::Role::Member, key.clone())
        .unwrap();
    // Another clone revoked the key of Mia from its entry 40.
    let other = LogId::from_bytes([0xab; 16]);
    let mut revoked = register.clone();
    revoked
        .revoke(&mia, &key, Bound::in_log(40, other))
        .unwrap();
    let before = repository.oplog().entries().unwrap().len() as u64;
    let brought = fetch_entry(&repository, register_commit(&repository, &revoked));
    assert_eq!(brought, before + 1);
    let entries = repository.oplog().entries().unwrap();
    let local = repository.register_for_log(&revoked, &entries).unwrap();
    let state = local.entry(&mia).unwrap().keys()[0].state;
    assert_eq!(state, KeyState::Revoked { from: brought });

    // A bound of this log keeps its number.
    let own = repository.log_id().unwrap();
    let mut here = register.clone();
    here.revoke(&mia, &key, Bound::in_log(40, own)).unwrap();
    assert_eq!(repository.register_for_log(&here, &entries).unwrap(), here);

    // A bound that no `fetch` entry brought lets no entry of this log stand before it.
    let mut unknown = register;
    unknown
        .revoke(&mia, &key, Bound::in_log(40, LogId::from_bytes([0xcd; 16])))
        .unwrap();
    let local = repository.register_for_log(&unknown, &entries).unwrap();
    let state = local.entry(&mia).unwrap().keys()[0].state;
    assert_eq!(state, KeyState::Revoked { from: 1 });
}

#[test]
fn a_retirement_that_a_later_fetch_turns_into_a_revocation_counts_from_the_first_fetch() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    let mia = Identity::human("mia").unwrap();
    let key = Seeded::new(3).public();
    let mut register = Register::with_owner(Identity::of(&actor()), signer().public()).unwrap();
    register
        .add_human(mia.clone(), register::Role::Member, key.clone())
        .unwrap();
    let other = LogId::from_bytes([0xab; 16]);
    let mut retired = register;
    retired
        .retire(&mia, &key, Bound::in_log(30, other))
        .unwrap();
    let first = fetch_entry(&repository, register_commit(&repository, &retired));
    let mut revoked = retired;
    revoked
        .revoke(&mia, &key, Bound::in_log(20, other))
        .unwrap();
    let second = fetch_entry(&repository, register_commit(&repository, &revoked));
    assert!(second > first);
    let entries = repository.oplog().entries().unwrap();
    let local = repository.register_for_log(&revoked, &entries).unwrap();
    // The key stopped counting in this log with the first fetch, not with the second.
    assert_eq!(
        local.entry(&mia).unwrap().keys()[0].state,
        KeyState::Revoked { from: first }
    );
}

#[test]
fn a_kept_confirmation_that_was_changed_is_damage() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    commit(&dir, &repository, "a.txt", "one\n");
    let held = confirm(&repository, 1);
    let path = repository.metadata_dir().join(ANCHORS_DIR).join("origin");
    let text = fs::read_to_string(&path).unwrap();
    let number = held.tip().number;
    let changed = text.replacen(
        &format!("tip {number} "),
        &format!("tip {} ", number + 1),
        1,
    );
    assert_ne!(changed, text);
    for content in [changed, "x".repeat(70 * 1024)] {
        fs::write(&path, content).unwrap();
        let error = repository.last_confirmation(&origin()).unwrap_err();
        assert!(
            matches!(error, LogAnchorError::ConfirmationDamaged { .. }),
            "{error}"
        );
        assert_eq!(error.kind(), crate::failure::FailureKind::Damaged);
        assert!(matches!(
            repository.anchor(&origin(), address(), &signer()),
            Err(LogAnchorError::ConfirmationDamaged { .. })
        ));
    }
}

#[test]
fn a_kept_confirmation_of_another_hub_key_confirms_nothing() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    commit(&dir, &repository, "a.txt", "one\n");
    confirm(&repository, 1);
    // The remote now pins another key of the hub, as after a rotation.
    let metadata = repository.metadata_dir();
    let mut config = repository.remote(&origin()).unwrap().unwrap();
    config.hub_key = Some(Seeded::new(9).public());
    remote::write_config(&metadata, &metadata.join("tmp"), &origin(), &config).unwrap();
    assert!(matches!(
        repository.anchor(&origin(), address(), &signer()),
        Err(LogAnchorError::Confirmation(AnchorFinding::HubKey {
            position: 1
        }))
    ));
}

#[test]
fn a_confirmation_of_an_entry_that_the_log_does_not_hold_is_not_kept() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    commit(&dir, &repository, "a.txt", "one\n");
    let last = repository
        .oplog()
        .entries()
        .unwrap()
        .last()
        .unwrap()
        .number();
    let of = |tip: Point| {
        let anchor = AnchorPayload::try_new(
            address(),
            repository_id(),
            repository.log_id().unwrap(),
            tip,
            None,
            signer().public(),
        )
        .unwrap()
        .sign(&signer())
        .unwrap();
        held_by(&hub(), anchor, 1, 1, AnchorState::Confirmed)
    };
    let other = format!("b3:{}", "77".repeat(32)).parse().unwrap();
    let rewritten = of(Point {
        number: last,
        hash: other,
    });
    assert!(matches!(
        repository.keep_confirmation(&origin(), &rewritten),
        Err(LogAnchorError::Confirmation(
            AnchorFinding::Rewritten { .. }
        ))
    ));
    let beyond = of(Point {
        number: last + 5,
        hash: other,
    });
    assert!(matches!(
        repository.keep_confirmation(&origin(), &beyond),
        Err(LogAnchorError::Confirmation(AnchorFinding::Cut { .. }))
    ));
    assert_eq!(repository.last_confirmation(&origin()).unwrap(), None);
}

#[test]
fn a_list_out_of_order_or_without_the_kept_confirmation_is_found() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    commit(&dir, &repository, "a.txt", "one\n");
    let first = confirm(&repository, 1);
    commit(&dir, &repository, "a.txt", "two\n");
    let second = confirm(&repository, 2);
    let both = [first.clone(), second.clone()];
    assert_eq!(
        repository
            .verify_anchors(&origin(), &both)
            .unwrap()
            .confirmed,
        2
    );
    // An anchor left out, or one held twice.
    for list in [
        vec![second.clone()],
        vec![first.clone(), first.clone()],
        vec![second.clone(), first.clone()],
    ] {
        assert!(matches!(
            repository.verify_anchors(&origin(), &list),
            Err(LogAnchorError::Confirmation(AnchorFinding::Order { .. }))
        ));
    }
    // A list without the confirmation that the clone keeps, the last one in it.
    for list in [vec![], vec![first]] {
        let error = repository.verify_anchors(&origin(), &list).unwrap_err();
        assert!(
            matches!(
                &error,
                LogAnchorError::Confirmation(AnchorFinding::Withheld { kept })
                    if *kept == second.tip()
            ),
            "{error}"
        );
    }
}

#[test]
fn a_fetched_commit_that_the_store_does_not_hold_is_an_error() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    let mia = Identity::human("mia").unwrap();
    let key = Seeded::new(3).public();
    let mut register = Register::with_owner(Identity::of(&actor()), signer().public()).unwrap();
    register
        .add_human(mia.clone(), register::Role::Member, key.clone())
        .unwrap();
    register
        .revoke(&mia, &key, Bound::in_log(40, LogId::from_bytes([0xab; 16])))
        .unwrap();
    let missing = CommitId::new(format!("b3:{}", "42".repeat(32)).parse().unwrap());
    fetch_entry(&repository, missing);
    let entries = repository.oplog().entries().unwrap();
    assert!(matches!(
        repository.register_for_log(&register, &entries),
        Err(LogAnchorError::Register(_))
    ));
}

#[test]
fn a_missing_id_of_the_log_is_no_reason_to_read_every_bound_as_foreign() {
    let dir = TempDir::new();
    let repository = repository(&dir);
    let mia = Identity::human("mia").unwrap();
    let key = Seeded::new(3).public();
    let mut register = Register::with_owner(Identity::of(&actor()), signer().public()).unwrap();
    register
        .add_human(mia.clone(), register::Role::Member, key.clone())
        .unwrap();
    let own = repository.log_id().unwrap();
    register.revoke(&mia, &key, Bound::in_log(40, own)).unwrap();
    let entries = repository.oplog().entries().unwrap();
    fs::remove_file(repository.metadata_dir().join(LOG_ID_FILE)).unwrap();
    assert!(matches!(
        repository.register_for_log(&register, &entries),
        Err(LogAnchorError::LogIdMissing { .. })
    ));
    // A repository from before version 19 has no ID yet, and reads every bound with a log as
    // one of another log.
    fs::write(
        repository.metadata_dir().join("format"),
        "stemma-repository 18\n",
    )
    .unwrap();
    let local = repository.register_for_log(&register, &entries).unwrap();
    assert_eq!(
        local.entry(&mia).unwrap().keys()[0].state,
        KeyState::Revoked { from: 1 }
    );
}
