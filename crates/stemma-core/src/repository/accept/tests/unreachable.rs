//! The tests of the removal of unreachable objects on a hub (ADR-007, STEMMA-292): what a
//! refused upload leaves, what a reference or the record keeps, and what the removal refuses.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use super::*;
use crate::object::{Commit2, Kind, ObjectId, TreeId};
use crate::refs::encode_branch;
use crate::repository::{RemovedObjects, UnreachableError};

/// The grace period of these tests.
const GRACE: Duration = Duration::from_secs(60 * 60);

/// A time at which every file of a test is older than the grace period.
fn later() -> SystemTime {
    SystemTime::now() + 2 * GRACE
}

/// Calls the removal as the hub does, with the record of the hub and the clock at `now`.
fn remove_at(wire: &Wire, now: SystemTime) -> Result<RemovedObjects, UnreachableError> {
    let hub = wire.side();
    let mut record = hub.record.borrow_mut();
    hub.repository
        .remove_unreachable_objects_at(&mut *record, GRACE, now)
}

/// The objects in the store of the hub.
fn stored(wire: &Wire) -> BTreeSet<ObjectId> {
    wire.side()
        .repository
        .objects()
        .ids()
        .unwrap()
        .into_iter()
        .collect()
}

/// Uploads the objects of another store into the store of the hub, as a push does before its
/// ref update. Returns the objects that the hub did not have.
fn upload(wire: &Wire, from: &Hub) -> BTreeSet<ObjectId> {
    let before = stored(wire);
    let objects = wire.side().repository.objects();
    for id in from.repository.objects().ids().unwrap() {
        let object = from.repository.objects().get(&id).unwrap().unwrap();
        objects.put_raw(object.kind(), object.body()).unwrap();
    }
    stored(wire).difference(&before).copied().collect()
}

/// The commit, the tree, and the blob of a commit with one file.
fn parts(hub: &Hub, commit: CommitId, text: &str) -> BTreeSet<ObjectId> {
    let tree = hub
        .repository
        .objects()
        .get_commit(&commit)
        .unwrap()
        .unwrap()
        .tree();
    [
        commit.object_id(),
        tree.object_id(),
        Blob::new(text).id().object_id(),
    ]
    .into()
}

#[test]
fn an_object_of_a_refused_upload_is_removed_after_the_grace_period_and_kept_before_it() {
    let (mut wire, root) = started();
    // The objects of a push arrive, and its ref update is refused: they lie unnamed.
    let clone = Hub::new();
    let feature = clone.commit(&[("b.txt", "two\n")], vec![root]);
    let uploaded = upload(&wire, &clone);
    assert_eq!(uploaded, parts(&clone, feature, "two\n"));
    let moved = to(&mut wire, "heads/feature", Some(feature));
    assert!(matches!(
        push(&mut wire, &stranger(), vec![moved]),
        Err(ErrorMessage::Refused(Refused::Key { .. }))
    ));

    // Within the grace period they stay.
    let kept = remove_at(&wire, SystemTime::now()).unwrap();
    assert_eq!(kept.removed(), []);
    assert_eq!(kept.young(), uploaded.len());
    assert!(uploaded.is_subset(&stored(&wire)));

    // After it they go, and nothing else does.
    let before = stored(&wire);
    let removed = remove_at(&wire, later()).unwrap();
    assert_eq!(
        removed.removed().iter().copied().collect::<BTreeSet<_>>(),
        uploaded
    );
    assert_eq!(removed.young(), 0);
    assert_eq!(
        stored(&wire),
        before.difference(&uploaded).copied().collect()
    );
    assert_eq!(wire.side().reference("heads/main"), Some(root));
}

#[test]
fn a_ref_update_that_names_removed_objects_reports_them_as_missing() {
    let (mut wire, root) = started();
    let clone = Hub::new();
    let feature = clone.commit(&[("b.txt", "two\n")], vec![root]);
    upload(&wire, &clone);
    let removed = remove_at(&wire, later()).unwrap();
    assert_eq!(removed.removed().len(), 3);

    // The ref update came after the grace period: the hub answers that the commit is
    // incomplete, and the record and the references are as they were.
    let before = record_of(&wire);
    let moved = to(&mut wire, "heads/feature", Some(feature));
    let reasons = refused(push(&mut wire, &mia(), vec![moved]));
    assert!(
        matches!(&reasons[..], [(name, RefusalReason::Incomplete(_))] if name == "heads/feature"),
        "{reasons:?}"
    );
    unchanged(&wire, &before, &[("heads/feature", None)]);

    // The client uploads again, and the same update is accepted; its objects are reachable now
    // and stay.
    upload(&wire, &clone);
    let moved = to(&mut wire, "heads/feature", Some(feature));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    assert_eq!(wire.side().reference("heads/feature"), Some(feature));
    let before = stored(&wire);
    let after = remove_at(&wire, later()).unwrap();
    assert_eq!(after.removed(), []);
    assert_eq!(stored(&wire), before);
}

#[test]
fn nothing_that_a_branch_or_a_tag_reaches_is_removed() {
    let (mut wire, root) = started();
    let hub = wire.side();
    // Nested trees, a merge with two parents, a tag on an older commit, and a branch that is
    // deleted again: the record names its commit, and the record is no root.
    let first = hub.commit(&[("d/e/f.txt", "deep\n"), ("top.txt", "top\n")], vec![root]);
    let merge = hub.commit(&[("d/e/f.txt", "deeper\n")], vec![first, root]);
    let gone = hub.commit(&[("g.txt", "gone\n")], vec![root]);
    let gone_parts = parts(hub, gone, "gone\n");
    let moves = vec![
        to(&mut wire, "heads/feature", Some(merge)),
        to(&mut wire, "tags/v1", Some(first)),
        to(&mut wire, "heads/gone", Some(gone)),
    ];
    push(&mut wire, &mia(), moves).unwrap();
    let deleted = to(&mut wire, "heads/gone", None);
    push(&mut wire, &mia(), vec![deleted]).unwrap();

    let before = stored(&wire);
    let removed = remove_at(&wire, later()).unwrap();
    let removed: BTreeSet<ObjectId> = removed.removed().iter().copied().collect();
    // The deleted branch takes its commit, its tree, and its blob along, and nothing else.
    assert_eq!(removed, gone_parts);
    assert_eq!(
        stored(&wire),
        before.difference(&removed).copied().collect()
    );
    // A second run finds nothing to remove.
    assert_eq!(remove_at(&wire, later()).unwrap().removed(), []);
}

#[test]
fn a_commit_that_only_a_tag_reaches_is_kept() {
    let (mut wire, root) = started();
    let hub = wire.side();
    let tagged = hub.commit(&[("t.txt", "tagged\n")], vec![root]);
    let tagged_parts = parts(hub, tagged, "tagged\n");
    let moves = vec![
        to(&mut wire, "heads/release", Some(tagged)),
        to(&mut wire, "tags/v1", Some(tagged)),
    ];
    push(&mut wire, &mia(), moves).unwrap();
    // The branch goes; the tag alone names the commit now.
    let deleted = to(&mut wire, "heads/release", None);
    push(&mut wire, &mia(), vec![deleted]).unwrap();

    let before = stored(&wire);
    let removed = remove_at(&wire, later()).unwrap();
    assert_eq!(removed.removed(), []);
    assert!(tagged_parts.is_subset(&stored(&wire)));
    assert_eq!(stored(&wire), before);
}

/// Sets the time of modification of the files of `ids` to before the grace period.
fn age(wire: &Wire, ids: &BTreeSet<ObjectId>) {
    let objects = wire.side().repository.objects();
    for id in ids {
        fs::File::open(objects.object_path(id))
            .unwrap()
            .set_modified(SystemTime::now() - 2 * GRACE)
            .unwrap();
    }
}

#[test]
fn what_a_young_commit_reaches_is_kept_however_old_its_file() {
    let (mut wire, root) = started();
    // A branch once named an old commit and was deleted: the commit lies unnamed, and its file
    // is old.
    let old = wire.side().commit(&[("o.txt", "old\n")], vec![root]);
    let old_parts = parts(wire.side(), old, "old\n");
    let moved = to(&mut wire, "heads/y", Some(old));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    let deleted = to(&mut wire, "heads/y", None);
    push(&mut wire, &mia(), vec![deleted]).unwrap();
    age(&wire, &old_parts);
    // A new commit on top of it comes in a push: the upload does not write the old one again.
    // Its branch is deleted within the grace period, and the record names it.
    let young = wire
        .side()
        .commit(&[("o.txt", "old\n"), ("n.txt", "new\n")], vec![old]);
    let moved = to(&mut wire, "heads/x", Some(young));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    let deleted = to(&mut wire, "heads/x", None);
    push(&mut wire, &mia(), vec![deleted]).unwrap();

    let before = stored(&wire);
    let kept = remove_at(&wire, SystemTime::now()).unwrap();
    assert_eq!(kept.removed(), []);
    assert_eq!(stored(&wire), before);
    // The young commit has its history, so the walk of the gate over the record still reads.
    let hub = wire.side();
    let record = Record::read(&mut *hub.record.borrow_mut(), repository_id()).unwrap();
    hub.repository.writers(root, root, &record).unwrap();

    // After the grace period both go.
    let removed = remove_at(&wire, later()).unwrap();
    assert!(old_parts.is_subset(&removed.removed().iter().copied().collect()));
    assert!(!stored(&wire).contains(&young.object_id()));
}

#[test]
fn a_young_commit_whose_parent_never_arrived_does_not_stop_the_removal() {
    let (wire, root) = started();
    // An upload stopped half way: the commit is there, its parent is not.
    let clone = Hub::new();
    let parent = clone.commit(&[("p.txt", "parent\n")], vec![root]);
    let child = clone.commit(&[("c.txt", "child\n")], vec![parent]);
    let objects = wire.side().repository.objects();
    for id in parts(&clone, child, "child\n") {
        let object = clone.repository.objects().get(&id).unwrap().unwrap();
        objects.put_raw(object.kind(), object.body()).unwrap();
    }

    let before = stored(&wire);
    let kept = remove_at(&wire, SystemTime::now()).unwrap();
    assert_eq!(kept.removed(), []);
    assert_eq!(kept.young(), 3);
    assert_eq!(stored(&wire), before);
}

#[test]
fn the_moves_of_an_update_that_stopped_after_its_entry_are_kept() {
    let (mut wire, root) = started();
    let feature = wire.side().commit(&[("b.txt", "two\n")], vec![root]);
    let moved = to(&mut wire, "heads/feature", Some(feature));
    push(&mut wire, &mia(), vec![moved]).unwrap();
    // The entry is written and the branch did not move: the process ended in between.
    let repository = &wire.side().repository;
    fs::remove_file(repository.metadata_dir().join("refs/heads/feature")).unwrap();
    assert_eq!(wire.side().reference("heads/feature"), None);

    let before = stored(&wire);
    assert_eq!(remove_at(&wire, later()).unwrap().removed(), []);
    assert_eq!(stored(&wire), before);

    // The next accepted update completes the one that stopped, with its objects in place.
    let other = to(&mut wire, "heads/other", Some(root));
    push(&mut wire, &mia(), vec![other]).unwrap();
    assert_eq!(wire.side().reference("heads/feature"), Some(feature));
}

#[test]
fn a_repository_with_a_working_copy_is_refused() {
    let dir = TempDir::new();
    let actor = Actor::human("daniel").unwrap();
    let repository = Repository::init(
        dir.path(),
        &crate::repository::InitOptions::default(),
        &actor,
        &AllowAll,
    )
    .unwrap();
    let unnamed = repository.objects().put_blob(&Blob::new("x\n")).unwrap();
    let mut record = MemoryRecord::default();
    let error = repository
        .remove_unreachable_objects_at(&mut record, GRACE, later())
        .unwrap_err();
    assert!(matches!(error, UnreachableError::NotBare { .. }), "{error}");
    assert_eq!(error.kind(), FailureKind::Other);
    assert!(repository.objects().contains(&unnamed.object_id()).unwrap());
}

#[test]
fn a_held_lock_refuses_the_removal_before_it_removes_anything() {
    let (wire, _) = started();
    let clone = Hub::new();
    clone.commit(&[("b.txt", "two\n")], vec![]);
    let uploaded = upload(&wire, &clone);
    let lock = wire.side().repository.refs_store().lock_held().unwrap();
    let error = remove_at(&wire, later()).unwrap_err();
    assert!(
        matches!(&error, UnreachableError::Refs(source) if matches!(**source, RefError::Locked { .. })),
        "{error}"
    );
    assert_eq!(error.kind(), FailureKind::Locked);
    drop(lock);
    assert!(uploaded.is_subset(&stored(&wire)));
}

#[test]
fn a_reachable_object_that_is_missing_or_a_damaged_record_stops_the_removal() {
    let (wire, root) = started();
    let clone = Hub::new();
    clone.commit(&[("b.txt", "two\n")], vec![]);
    let uploaded = upload(&wire, &clone);

    // The tree of `main` is gone: what it named cannot be told from what is unreachable.
    let repository = &wire.side().repository;
    let tree = repository
        .objects()
        .get_commit(&root)
        .unwrap()
        .unwrap()
        .tree();
    assert!(repository.objects().remove(&tree.object_id()).unwrap());
    let error = remove_at(&wire, later()).unwrap_err();
    assert!(
        matches!(error, UnreachableError::Missing { id, kind: Kind::Tree, needed_by: Some(by) }
            if id == tree.object_id() && by == root.object_id()),
        "{error}"
    );
    assert_eq!(error.kind(), FailureKind::Damaged);
    assert!(
        error.to_string().ends_with("nothing was removed"),
        "{error}"
    );
    assert!(uploaded.is_subset(&stored(&wire)));

    // A record that does not read stops it as well.
    let (wire, _) = started();
    let uploaded = upload(&wire, &clone);
    wire.side()
        .record
        .borrow_mut()
        .bytes
        .extend_from_slice(b"garbage\n");
    let error = remove_at(&wire, later()).unwrap_err();
    assert!(matches!(error, UnreachableError::Record(_)), "{error}");
    assert_eq!(error.kind(), FailureKind::Damaged);
    wire.side().record.borrow_mut().fail_read = true;
    let error = remove_at(&wire, later()).unwrap_err();
    assert!(matches!(error, UnreachableError::RecordIo(_)), "{error}");
    assert!(error.io_error().is_some());
    assert!(uploaded.is_subset(&stored(&wire)));
}

/// A tree with these files, each with its mode, stored in the store of the hub.
fn tree_with(hub: &Hub, files: &[(&str, FileMode, &str)]) -> TreeId {
    let store = hub.repository.objects();
    let mut index = Index::new();
    for (path, mode, text) in files {
        let blob = store.put_blob(&Blob::new(*text)).unwrap();
        index.insert(*path, *mode, blob).unwrap();
    }
    index.write_tree(store).unwrap()
}

#[test]
fn the_history_below_a_tip_and_every_side_of_a_conflict_are_kept() {
    let (mut wire, root) = started();
    let hub = wire.side();
    // A chain whose middle commit only its child names, with an executable and a link.
    let middle = hub.commit(&[("m.txt", "middle\n")], vec![root]);
    let modes = tree_with(
        hub,
        &[
            ("run.sh", FileMode::Executable, "#!/bin/sh\n"),
            ("link", FileMode::Symlink, "run.sh"),
        ],
    );
    let tip = Commit::try_new(
        modes,
        vec![middle],
        ChangeId::generate().unwrap(),
        Actor::human("daniel").unwrap(),
        2,
        "the tip",
    )
    .unwrap();
    let tip = hub.repository.objects().put_commit(&tip).unwrap();
    // A commit that records an open conflict: three trees, each with a file of its own.
    let sides: Vec<TreeId> = ["side one\n", "base\n", "side two\n"]
        .iter()
        .map(|text| tree_with(hub, &[("c.txt", FileMode::File, text)]))
        .collect();
    let conflict = Commit2::try_new(
        sides,
        vec![middle],
        ChangeId::generate().unwrap(),
        Actor::human("daniel").unwrap(),
        3,
        "a conflict",
    )
    .unwrap();
    let conflict = CommitId::new(
        hub.repository
            .objects()
            .put_raw(Kind::Commit2, &conflict.encode())
            .unwrap(),
    );
    let moves = vec![
        to(&mut wire, "heads/chain", Some(tip)),
        to(&mut wire, "heads/conflict", Some(conflict)),
    ];
    push(&mut wire, &mia(), moves).unwrap();

    let before = stored(&wire);
    let removed = remove_at(&wire, later()).unwrap();
    assert_eq!(removed.removed(), []);
    assert_eq!(stored(&wire), before);
    // Everything in the store is reached: the walk counts each object once.
    assert_eq!(removed.reachable(), before.len());
}

#[test]
fn a_file_with_a_time_after_the_clock_is_young() {
    let (wire, _) = started();
    let clone = Hub::new();
    clone.commit(&[("b.txt", "two\n")], vec![]);
    let uploaded = upload(&wire, &clone);
    // The clock of the system stands before the time of the files.
    let earlier = SystemTime::now() - 2 * GRACE;
    let kept = remove_at(&wire, earlier).unwrap();
    assert_eq!(kept.removed(), []);
    assert_eq!(kept.young(), uploaded.len());
    assert!(uploaded.is_subset(&stored(&wire)));
}

#[test]
fn a_reference_to_an_object_of_another_kind_or_to_no_object_is_damage() {
    let (wire, _) = started();
    let clone = Hub::new();
    clone.commit(&[("b.txt", "two\n")], vec![]);
    let uploaded = upload(&wire, &clone);
    let repository = &wire.side().repository;
    let branch = repository.metadata_dir().join("refs/heads/broken");

    // A branch that names a blob.
    let blob = repository
        .objects()
        .put_blob(&Blob::new("a blob\n"))
        .unwrap();
    fs::write(&branch, encode_branch(&CommitId::new(blob.object_id()))).unwrap();
    let error = remove_at(&wire, later()).unwrap_err();
    assert!(
        matches!(
            error,
            UnreachableError::Read {
                needed_by: None,
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(error.kind(), FailureKind::Damaged);
    assert!(
        error.to_string().ends_with("; nothing was removed"),
        "{error}"
    );

    // A branch that names a commit of no upload.
    let absent = Hub::new().commit(&[("c.txt", "three\n")], vec![]);
    fs::write(&branch, encode_branch(&absent)).unwrap();
    let error = remove_at(&wire, later()).unwrap_err();
    assert!(
        matches!(error, UnreachableError::Missing { id, kind: Kind::Commit, needed_by: None }
            if id == absent.object_id()),
        "{error}"
    );
    assert_eq!(error.kind(), FailureKind::Damaged);
    assert_eq!(
        error.to_string(),
        format!("the commit {absent} of a root is missing; nothing was removed")
    );
    assert!(uploaded.is_subset(&stored(&wire)));
}
