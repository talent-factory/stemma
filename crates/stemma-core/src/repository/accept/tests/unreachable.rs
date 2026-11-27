//! The tests of the removal of unreachable objects on a hub (ADR-007, STEMMA-292): what a
//! refused upload leaves, what a reference or the record keeps, and what the removal refuses.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use super::*;
use crate::object::{Kind, ObjectId};
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

#[test]
fn a_branch_file_that_names_a_commit_of_no_upload_is_reported() {
    // A reference that names an absent commit is damage: the removal stops before it
    // removes anything.
    let (wire, _) = started();
    let clone = Hub::new();
    let absent = clone.commit(&[("c.txt", "three\n")], vec![]);
    let repository = &wire.side().repository;
    fs::write(
        repository.metadata_dir().join("refs/heads/broken"),
        encode_branch(&absent),
    )
    .unwrap();
    let error = remove_at(&wire, later()).unwrap_err();
    assert!(
        matches!(error, UnreachableError::Missing { id, kind: Kind::Commit, needed_by: None }
            if id == absent.object_id()),
        "{error}"
    );
}
