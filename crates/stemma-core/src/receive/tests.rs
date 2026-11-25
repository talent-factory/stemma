//! Tests of the receiving side of an object stream (ADR-023, STEMMA-289): the upload on a hub,
//! the connectivity check, the bounds, the directory of a receiving operation in `tmp/`, and the
//! journal of a running fetch.

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::*;
use crate::change::ChangeId;
use crate::failure::{Classify, FailureKind};
use crate::index::Index;
use crate::object::{Blob, BlobId, Commit, FileMode};
use crate::oplog::OperationName;
use crate::protocol::ObjectStreamWriter;
use crate::refs::{encode_branch, BareLacks};
use crate::repository::InitOptions;
use crate::testutil::{conflicted_commit, write_remote_ref, TempDir};
use crate::{Actor, AllowAll, Allowed};

fn daniel() -> Actor {
    Actor::human("daniel").unwrap()
}

/// A bare repository in the directory `hub` below `dir`.
fn hub(dir: &TempDir) -> Repository {
    Repository::init_bare(dir.path().join("hub"), &daniel(), &AllowAll).unwrap()
}

/// A repository with a working copy in the directory `name` below `dir`.
fn clone_in(dir: &TempDir, name: &str) -> Repository {
    let path = dir.path().join(name);
    Repository::init(&path, &InitOptions::default(), &daniel(), &AllowAll).unwrap()
}

/// Stores a commit with these files and parents in the object store of `repository`.
fn commit_of(repository: &Repository, files: &[(&str, &str)], parents: Vec<CommitId>) -> CommitId {
    let mut index = Index::new();
    for (path, text) in files {
        let blob = repository.objects().put_blob(&Blob::new(*text)).unwrap();
        index.insert(*path, FileMode::File, blob).unwrap();
    }
    let tree = index.write_tree(repository.objects()).unwrap();
    let commit = Commit::try_new(
        tree,
        parents,
        ChangeId::generate().unwrap(),
        daniel(),
        1,
        "a commit",
    )
    .unwrap();
    repository.objects().put_commit(&commit).unwrap()
}

/// The ID of the blob with this text.
fn blob_id(text: &str) -> ObjectId {
    Blob::new(text).id().object_id()
}

/// The objects of the commits in the order of a stream (ADR-023): for each commit its blobs and
/// trees, a tree after what it names, and then the commit. An object stands once, and one of
/// `sent` is left out, as a sender leaves out what the receiver has.
fn objects_of(
    sender: &Repository,
    commits: &[CommitId],
    sent: &mut HashSet<ObjectId>,
) -> Vec<ObjectId> {
    fn tree(
        store: &FsObjectStore,
        id: TreeId,
        sent: &mut HashSet<ObjectId>,
        out: &mut Vec<ObjectId>,
    ) {
        if !sent.insert(id.object_id()) {
            return;
        }
        for entry in store.get_tree(&id).unwrap().unwrap().entries() {
            if entry.mode() == EntryMode::Directory {
                tree(store, TreeId::new(entry.id()), sent, out);
            } else if sent.insert(entry.id()) {
                out.push(entry.id());
            }
        }
        out.push(id.object_id());
    }
    let store = sender.objects();
    let mut out = Vec::new();
    for commit in commits {
        if !sent.insert(commit.object_id()) {
            continue;
        }
        let read = store.get_any_commit(commit).unwrap().unwrap();
        for &id in read.tree_sum().terms() {
            tree(store, id, sent, &mut out);
        }
        out.push(commit.object_id());
    }
    out
}

/// A stream of these objects of `sender`, in this order.
fn stream_of(sender: &Repository, objects: &[ObjectId]) -> Vec<u8> {
    let mut writer = ObjectStreamWriter::new(Vec::new()).unwrap();
    for id in objects {
        let object = sender.objects().get(id).unwrap().unwrap();
        writer.write(object.kind(), object.body()).unwrap();
    }
    writer.finish()
}

/// A stream of everything below the commits.
fn stream_of_commits(sender: &Repository, commits: &[CommitId]) -> Vec<u8> {
    stream_of(sender, &objects_of(sender, commits, &mut HashSet::new()))
}

/// The names in `tmp/` of the repository, sorted.
fn tmp_names(repository: &Repository) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(repository.metadata_dir().join("tmp"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn set_branch(repository: &Repository, branch: &str, commit: &CommitId) {
    let path = repository.metadata_dir().join("refs/heads").join(branch);
    fs::write(path, encode_branch(commit)).unwrap();
}

fn journal_path(repository: &Repository) -> PathBuf {
    repository.metadata_dir().join(FETCH_JOURNAL)
}

/// Removes the file of an object, as an interrupted transfer never wrote it.
fn remove_object(repository: &Repository, id: &ObjectId) {
    assert!(repository.objects().remove(id).unwrap());
}

fn set(commits: &[CommitId]) -> BTreeSet<CommitId> {
    commits.iter().copied().collect()
}

#[test]
fn the_default_bounds_are_the_named_ones() {
    let bounds = Bounds::default();
    assert_eq!(bounds.max_object_len(), 1 << 30);
    assert_eq!(bounds.max_request_len(), 1 << 34);
    assert_eq!(bounds, Bounds::new(MAX_OBJECT_LEN, MAX_REQUEST_LEN));
}

#[test]
fn an_upload_stores_every_object_under_the_id_it_computes_and_is_idempotent() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let hub = hub(&dir);
    let first = commit_of(
        &sender,
        &[("a.txt", "one\n"), ("src/b.txt", "two\n")],
        vec![],
    );
    let second = commit_of(
        &sender,
        &[("a.txt", "three\n"), ("src/b.txt", "two\n")],
        vec![first],
    );
    let objects = objects_of(&sender, &[first, second], &mut HashSet::new());
    // Two blobs, the tree `src`, the root, and the commit; then one blob, the root, the commit.
    assert_eq!(objects.len(), 8);
    let stream = stream_of(&sender, &objects);

    for _ in 0..2 {
        let received = hub.receive_objects(&stream[..], Bounds::default()).unwrap();
        assert_eq!(received.objects, 8);
        assert_eq!(received.commits, 2);
        let bodies: usize = objects
            .iter()
            .map(|id| sender.objects().get(id).unwrap().unwrap().body().len())
            .sum();
        assert_eq!(received.bytes, bodies as u64);
        for id in &objects {
            let here = hub.objects().get(id).unwrap().unwrap();
            let there = sender.objects().get(id).unwrap().unwrap();
            assert_eq!((here.kind(), here.body()), (there.kind(), there.body()));
        }
        // The directory of the upload is gone, and nothing else lies in `tmp/`.
        assert!(tmp_names(&hub).is_empty(), "{:?}", tmp_names(&hub));
    }
    // The upload names nothing: the hub has no reference, and the check of the ref update that
    // follows finds everything.
    assert!(hub.refs().branches().unwrap().is_empty());
    hub.check_connectivity(&[second]).unwrap();
}

#[test]
fn an_empty_stream_stores_nothing_and_a_stream_without_its_first_line_is_refused() {
    let dir = TempDir::new();
    let hub = hub(&dir);
    let empty = ObjectStreamWriter::new(Vec::new()).unwrap().finish();
    assert_eq!(
        hub.receive_objects(&empty[..], Bounds::default()).unwrap(),
        Received::default()
    );

    let error = hub
        .receive_objects(&b""[..], Bounds::default())
        .unwrap_err();
    assert!(
        matches!(error, ReceiveError::Stream(StreamError::FirstLine)),
        "{error}"
    );
    assert!(!error.exceeds_bound());
    assert_eq!(error.kind(), FailureKind::Other);
    assert!(tmp_names(&hub).is_empty());
}

#[test]
fn a_damaged_object_shows_as_a_missing_one() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let hub = hub(&dir);
    let commit = commit_of(&sender, &[("a.txt", "one\n"), ("b.txt", "two\n")], vec![]);
    let objects = objects_of(&sender, &[commit], &mut HashSet::new());
    // The frame stays whole and one byte of a body changes: `one` becomes `onE`.
    let mut stream = stream_of(&sender, &objects);
    let at = stream
        .windows(4)
        .position(|bytes| bytes == b"one\n")
        .unwrap();
    stream[at + 2] = b'E';

    // The upload cannot know which object was meant: it stores the object that arrived.
    let received = hub.receive_objects(&stream[..], Bounds::default()).unwrap();
    assert_eq!(received.objects, 4);
    assert!(hub.objects().contains(&blob_id("onE\n")).unwrap());
    assert!(!hub.objects().contains(&blob_id("one\n")).unwrap());

    let error = hub.check_connectivity(&[commit]).unwrap_err();
    let tree = sender
        .objects()
        .get_commit(&commit)
        .unwrap()
        .unwrap()
        .tree();
    match &error {
        ConnectivityError::Missing {
            id,
            expected,
            needed_by,
        } => {
            assert_eq!(*id, blob_id("one\n"));
            assert_eq!(*expected, Kind::Blob);
            assert_eq!(*needed_by, Some(tree.object_id()));
        }
        other => panic!("not a missing object: {other:?}"),
    }
    assert!(error.is_incomplete());
    assert_eq!(error.kind(), FailureKind::Other);
    assert_eq!(
        error.to_string(),
        format!(
            "the blob {} is missing, which {tree} names",
            blob_id("one\n")
        )
    );

    // A fetch checks while it receives, and reports the same.
    let receiver = clone_in(&dir, "receiver");
    let mut fetch = receiver.begin_fetch().unwrap();
    let error = fetch.receive(&stream[..], Bounds::default()).unwrap_err();
    assert!(
        matches!(
            &error,
            ReceiveError::Connectivity(ConnectivityError::Missing { id, .. })
                if *id == blob_id("one\n")
        ),
        "{error}"
    );
    assert!(fetch.check(&set(&[commit])).is_err());
}

#[test]
fn a_damaged_tree_or_commit_shows_as_a_missing_one_too() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let first = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let second = commit_of(&sender, &[("a.txt", "two\n")], vec![first]);
    let objects = objects_of(&sender, &[first, second], &mut HashSet::new());
    let stream = stream_of(&sender, &objects);
    let tree = sender.objects().get_commit(&first).unwrap().unwrap().tree();

    // The name of the file in the first tree, and the message of the first commit.
    for (needle, missing, expected, needed_by) in [
        (
            &b"a.txt"[..],
            tree.object_id(),
            Kind::Tree,
            first.object_id(),
        ),
        (
            &b"a commit"[..],
            first.object_id(),
            Kind::Commit,
            second.object_id(),
        ),
    ] {
        let hub_dir = TempDir::new();
        let hub = hub(&hub_dir);
        let mut damaged = stream.clone();
        let at = damaged
            .windows(needle.len())
            .position(|bytes| bytes == needle)
            .unwrap();
        damaged[at] = b'z';
        hub.receive_objects(&damaged[..], Bounds::default())
            .unwrap();

        let error = hub.check_connectivity(&[second]).unwrap_err();
        assert!(
            matches!(
                &error,
                ConnectivityError::Missing { id, expected: kind, needed_by: by }
                    if *id == missing && *kind == expected && *by == Some(needed_by)
            ),
            "{error}"
        );
    }
}

#[test]
fn a_tip_that_is_missing_names_no_object_that_needs_it() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let hub = hub(&dir);
    let commit = commit_of(&sender, &[("a.txt", "one\n")], vec![]);

    let error = hub.check_connectivity(&[commit]).unwrap_err();
    assert!(
        matches!(
            &error,
            ConnectivityError::Missing { id, expected: Kind::Commit, needed_by: None }
                if *id == commit.object_id()
        ),
        "{error}"
    );
    assert_eq!(error.to_string(), format!("the commit {commit} is missing"));
    // No tips: nothing to check.
    hub.check_connectivity(&[]).unwrap();
}

#[test]
fn an_object_of_the_wrong_kind_and_a_body_that_a_strict_reader_refuses_are_named() {
    let dir = TempDir::new();
    let hub = hub(&dir);
    let store = hub.objects();
    let good = commit_of(&hub, &[("a.txt", "one\n")], vec![]);
    let good_tree = store.get_commit(&good).unwrap().unwrap().tree();
    let commit = |tree: TreeId, parents: Vec<CommitId>| {
        let commit = Commit::try_new(
            tree,
            parents,
            ChangeId::generate().unwrap(),
            daniel(),
            1,
            "next",
        )
        .unwrap();
        store.put_commit(&commit).unwrap()
    };

    // A tree that names a commit as a file.
    let entry = TreeEntry::file("a.txt", FileMode::File, BlobId::new(good.object_id())).unwrap();
    let wrong_blob = store
        .put_tree(&Tree::try_new(vec![entry]).unwrap())
        .unwrap();
    let error = hub
        .check_connectivity(&[commit(wrong_blob, vec![])])
        .unwrap_err();
    assert!(
        matches!(
            &error,
            ConnectivityError::WrongKind { id, expected: Kind::Blob, found: Kind::Commit, needed_by }
                if *id == good.object_id() && *needed_by == Some(wrong_blob.object_id())
        ),
        "{error}"
    );
    assert!(error.is_incomplete());

    // A tree that names a blob as a directory.
    let entry = TreeEntry::directory("src", TreeId::new(blob_id("one\n"))).unwrap();
    let wrong_tree = store
        .put_tree(&Tree::try_new(vec![entry]).unwrap())
        .unwrap();
    let error = hub
        .check_connectivity(&[commit(wrong_tree, vec![])])
        .unwrap_err();
    assert!(
        matches!(
            &error,
            ConnectivityError::WrongKind {
                expected: Kind::Tree,
                found: Kind::Blob,
                ..
            }
        ),
        "{error}"
    );

    // A parent that is a tree.
    let child = commit(good_tree, vec![CommitId::new(good_tree.object_id())]);
    let error = hub.check_connectivity(&[child]).unwrap_err();
    assert!(
        matches!(
            &error,
            ConnectivityError::WrongKind { expected: Kind::Commit, found: Kind::Tree, needed_by, .. }
                if *needed_by == Some(child.object_id())
        ),
        "{error}"
    );

    // A tree and a commit whose bodies no writer of the crate makes: the frames of a stream
    // are whole, so the upload stores them, and the strict readers refuse them.
    let mut writer = ObjectStreamWriter::new(Vec::new()).unwrap();
    writer.write(Kind::Tree, b"not a tree").unwrap();
    writer.write(Kind::Commit, b"not a commit").unwrap();
    hub.receive_objects(&writer.finish()[..], Bounds::default())
        .unwrap();
    let bad_tree = TreeId::new(crate::object::hash_encoding(Kind::Tree, b"not a tree"));
    let bad_commit = CommitId::new(crate::object::hash_encoding(Kind::Commit, b"not a commit"));

    let above = commit(bad_tree, vec![]);
    let error = hub.check_connectivity(&[above]).unwrap_err();
    assert!(
        matches!(
            &error,
            ConnectivityError::Unreadable { needed_by, source }
                if matches!(&**source, ReadError::Tree { id, .. } if *id == bad_tree)
                    && *needed_by == Some(above.object_id())
        ),
        "{error}"
    );
    assert!(error.is_incomplete());
    assert!(
        error
            .to_string()
            .contains(&format!(" ({above} names the object)")),
        "{error}"
    );
    let error = hub.check_connectivity(&[bad_commit]).unwrap_err();
    assert!(
        matches!(
            &error,
            ConnectivityError::Unreadable { needed_by: None, source }
                if matches!(&**source, ReadError::Commit { .. })
        ),
        "{error}"
    );
}

#[test]
fn the_check_does_not_presuppose_that_every_object_comes_from_the_sender() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let hub = hub(&dir);
    let files = [("a.txt", "one\n"), ("src/b.txt", "two\n")];
    let commit = commit_of(&sender, &files, vec![]);
    // The hub has the trees and the blobs by another way, with another commit above them.
    commit_of(&hub, &files, vec![]);
    assert!(!hub.objects().contains(&commit.object_id()).unwrap());

    // The stream carries the commit alone.
    let stream = stream_of(&sender, &[commit.object_id()]);
    let received = hub.receive_objects(&stream[..], Bounds::default()).unwrap();
    assert_eq!((received.objects, received.commits), (1, 1));
    hub.check_connectivity(&[commit]).unwrap();
}

#[test]
fn the_walk_covers_objects_that_were_in_the_store_before() {
    let dir = TempDir::new();
    let hub = hub(&dir);
    // What an interrupted transfer left: a commit with its trees, and one blob that never
    // arrived. Nothing of it comes with the stream that follows.
    let first = commit_of(&hub, &[("a.txt", "one\n"), ("src/b.txt", "two\n")], vec![]);
    let second = commit_of(
        &hub,
        &[("a.txt", "three\n"), ("src/b.txt", "two\n")],
        vec![first],
    );
    remove_object(&hub, &blob_id("two\n"));
    let empty = ObjectStreamWriter::new(Vec::new()).unwrap().finish();
    hub.receive_objects(&empty[..], Bounds::default()).unwrap();

    // The walk from the new tip goes through the parent, which no reference reaches, and finds
    // the gap there; the tree of the tip itself is not asked for the blob again.
    for tip in [first, second] {
        let error = hub.check_connectivity(&[tip]).unwrap_err();
        assert!(
            matches!(&error, ConnectivityError::Missing { id, .. } if *id == blob_id("two\n")),
            "{error}"
        );
    }
}

#[test]
fn the_walk_ends_at_a_commit_that_an_own_reference_reaches_and_compares_the_trees() {
    let dir = TempDir::new();
    let hub = hub(&dir);
    let root = commit_of(&hub, &[("a.txt", "one\n"), ("src/b.txt", "two\n")], vec![]);
    let first = commit_of(
        &hub,
        &[("a.txt", "one\n"), ("src/b.txt", "four\n")],
        vec![root],
    );
    remove_object(&hub, &blob_id("one\n"));
    let second = commit_of(
        &hub,
        &[("a.txt", "one\n"), ("src/b.txt", "five\n")],
        vec![first],
    );
    remove_object(&hub, &blob_id("one\n"));

    // No reference: the walk reads everything below the tip and misses the blob.
    let error = hub.check_connectivity(&[second]).unwrap_err();
    assert!(
        matches!(&error, ConnectivityError::Missing { id, .. } if *id == blob_id("one\n")),
        "{error}"
    );
    // A branch, a tag, and a remote reference each make their commit complete (ADR-015), so
    // the blob that the tip shares with it at the same path is not asked for. The reference
    // may name the parent of the tip or a commit further down.
    let set_tag = |commit: &CommitId| {
        let tags = hub.metadata_dir().join("refs/tags");
        fs::create_dir_all(&tags).unwrap();
        fs::write(tags.join("v1"), encode_branch(commit)).unwrap();
    };
    for name in ["branch", "tag", "remote", "below"] {
        match name {
            "branch" => set_branch(&hub, "main", &first),
            "tag" => set_tag(&first),
            "remote" => write_remote_ref(&hub.metadata_dir(), "origin/heads/main", first),
            _ => set_branch(&hub, "main", &root),
        }
        hub.check_connectivity(&[second])
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        // The tip of the reference is complete without a look.
        hub.check_connectivity(&[first, root]).unwrap();
        for path in [
            "refs/heads/main",
            "refs/tags/v1",
            "refs/remotes/origin/heads/main",
        ] {
            let _ = fs::remove_file(hub.metadata_dir().join(path));
        }
    }

    // What the tip changed is checked even below a reference.
    set_branch(&hub, "main", &first);
    remove_object(&hub, &blob_id("five\n"));
    let error = hub.check_connectivity(&[second]).unwrap_err();
    assert!(
        matches!(&error, ConnectivityError::Missing { id, .. } if *id == blob_id("five\n")),
        "{error}"
    );
}

#[test]
fn a_blob_that_moved_to_another_path_is_checked_and_not_taken_from_the_parent() {
    let dir = TempDir::new();
    let hub = hub(&dir);
    let first = commit_of(&hub, &[("a.txt", "one\n")], vec![]);
    set_branch(&hub, "main", &first);
    // The same blob under a new name: the trees are compared path by path, so it is looked at.
    let second = commit_of(&hub, &[("b.txt", "one\n")], vec![first]);
    hub.check_connectivity(&[second]).unwrap();
    remove_object(&hub, &blob_id("one\n"));
    let error = hub.check_connectivity(&[second]).unwrap_err();
    assert!(
        matches!(error, ConnectivityError::Missing { .. }),
        "{error}"
    );
}

#[test]
fn a_merge_and_a_commit_with_an_open_conflict_are_walked() {
    let dir = TempDir::new();
    let clone = clone_in(&dir, "clone");
    let base = commit_of(&clone, &[("a.txt", "one\n")], vec![]);
    let left = commit_of(
        &clone,
        &[("a.txt", "one\n"), ("l.txt", "left\n")],
        vec![base],
    );
    let right = commit_of(
        &clone,
        &[("a.txt", "one\n"), ("r.txt", "right\n")],
        vec![base],
    );
    let files = [
        ("a.txt", "one\n"),
        ("l.txt", "left\n"),
        ("r.txt", "right\n"),
    ];
    let merge = commit_of(&clone, &files, vec![left, right]);
    clone.check_connectivity(&[merge]).unwrap();

    // The base is reached through both parents and is checked once; a blob that only one side
    // has is found through it.
    remove_object(&clone, &blob_id("right\n"));
    let error = clone.check_connectivity(&[merge]).unwrap_err();
    assert!(
        matches!(&error, ConnectivityError::Missing { id, .. } if *id == blob_id("right\n")),
        "{error}"
    );

    // A `commit2` names a list of trees, and every one of them is walked.
    let (conflicted, _) = conflicted_commit(&clone);
    clone.check_connectivity(&[conflicted]).unwrap();
    let read = clone
        .objects()
        .get_any_commit(&conflicted)
        .unwrap()
        .unwrap();
    let last = *read.tree_sum().terms().last().unwrap();
    remove_object(&clone, &last.object_id());
    let error = clone.check_connectivity(&[conflicted]).unwrap_err();
    assert!(
        matches!(
            &error,
            ConnectivityError::Missing { id, expected: Kind::Tree, needed_by }
                if *id == last.object_id() && *needed_by == Some(conflicted.object_id())
        ),
        "{error}"
    );
}

#[test]
fn a_reference_whose_history_cannot_be_read_is_reported_as_damage_of_the_own_repository() {
    let dir = TempDir::new();
    let hub = hub(&dir);
    let first = commit_of(&hub, &[("a.txt", "one\n")], vec![]);
    let second = commit_of(&hub, &[("a.txt", "two\n")], vec![first]);
    let other = commit_of(&hub, &[("c.txt", "other\n")], vec![]);
    set_branch(&hub, "main", &second);
    remove_object(&hub, &first.object_id());

    // The other commit is not the tip of the branch, so the walk below the branch looks for it
    // and meets the gap.
    let error = hub.check_connectivity(&[other]).unwrap_err();
    assert!(
        matches!(
            &error,
            ConnectivityError::Own(source)
                if matches!(
                    &**source,
                    HistoryError::Missing { id, child } if *id == first && *child == Some(second)
                )
        ),
        "{error}"
    );
    assert!(!error.is_incomplete());
    assert_eq!(error.kind(), FailureKind::Damaged);
    assert!(
        error
            .to_string()
            .starts_with("cannot read the history below"),
        "{error}"
    );
}

#[test]
fn an_object_that_the_walk_does_not_reach_stays_and_is_no_error() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let receiver = clone_in(&dir, "receiver");
    let commit = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let stray = sender.objects().put_blob(&Blob::new("stray\n")).unwrap();
    let mut objects = objects_of(&sender, &[commit], &mut HashSet::new());
    // Before the commit and after it.
    objects.insert(0, stray.object_id());
    let other = sender.objects().put_blob(&Blob::new("other\n")).unwrap();
    objects.push(other.object_id());

    let mut fetch = receiver.begin_fetch().unwrap();
    let stream = stream_of(&sender, &objects);
    let received = fetch.receive(&stream[..], Bounds::default()).unwrap();
    assert_eq!((received.objects, received.commits), (5, 1));
    fetch.check(&set(&[commit])).unwrap();
    assert!(receiver.objects().contains(&stray.object_id()).unwrap());
    assert!(receiver.objects().contains(&other.object_id()).unwrap());
}

#[test]
fn an_object_above_the_bound_is_refused_before_its_body_is_stored() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let hub = hub(&dir);
    let small = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let large = commit_of(
        &sender,
        &[("a.txt", "one\n"), ("big.bin", &"x".repeat(4096))],
        vec![small],
    );
    let stream = stream_of_commits(&sender, &[small, large]);
    let bounds = Bounds::new(4095, MAX_REQUEST_LEN);

    let error = hub.receive_objects(&stream[..], bounds).unwrap_err();
    assert!(
        matches!(
            &error,
            ReceiveError::Stream(StreamError::TooLarge {
                length: 4096,
                limit: 4095,
                ..
            })
        ),
        "{error}"
    );
    assert!(error.exceeds_bound());
    assert_eq!(error.kind(), FailureKind::Other);
    // What came before the object is stored, unnamed; the object itself is not, and the
    // directory of the upload is gone.
    assert!(hub.objects().contains(&small.object_id()).unwrap());
    assert!(!hub.objects().contains(&blob_id(&"x".repeat(4096))).unwrap());
    assert!(!hub.objects().contains(&large.object_id()).unwrap());
    assert!(tmp_names(&hub).is_empty());
    // An object of exactly the bound passes.
    hub.receive_objects(&stream[..], Bounds::new(4096, MAX_REQUEST_LEN))
        .unwrap();
    hub.check_connectivity(&[large]).unwrap();
}

#[test]
fn a_stream_above_the_bound_of_a_request_is_refused_and_one_that_fills_it_passes() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let first = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let second = commit_of(&sender, &[("a.txt", "two\n")], vec![first]);
    let stream = stream_of_commits(&sender, &[first, second]);
    let length = stream.len() as u64;

    let exact = hub(&dir);
    exact
        .receive_objects(&stream[..], Bounds::new(MAX_OBJECT_LEN, length))
        .unwrap();
    exact.check_connectivity(&[second]).unwrap();

    // With one byte less the reader still sees every frame, a bound between two frames ends the
    // stream there, and a bound of 0 ends it inside the first line: each time the answer is
    // the bound, not a stream that ended or was cut off.
    let first_only = stream_of_commits(&sender, &[first]).len() as u64;
    for limit in [length - 1, first_only, 0] {
        let hub_dir = TempDir::new();
        let hub = hub(&hub_dir);
        let error = hub
            .receive_objects(&stream[..], Bounds::new(MAX_OBJECT_LEN, limit))
            .unwrap_err();
        assert!(
            matches!(&error, ReceiveError::RequestTooLarge { limit: found } if *found == limit),
            "{limit}: {error}"
        );
        assert!(error.exceeds_bound());
        assert_eq!(
            error.to_string(),
            format!("the object stream is longer than the limit of {limit} bytes")
        );
        assert!(tmp_names(&hub).is_empty());
    }
    // A stream that is cut off below the bound is cut off.
    let hub_dir = TempDir::new();
    let error = hub(&hub_dir)
        .receive_objects(&stream[..stream.len() - 1], Bounds::default())
        .unwrap_err();
    assert!(
        matches!(error, ReceiveError::Stream(StreamError::CutOff { .. })),
        "{error}"
    );
}

#[test]
fn a_cut_stream_leaves_a_state_that_the_next_fetch_resumes() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let receiver = clone_in(&dir, "receiver");
    let first = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let second = commit_of(&sender, &[("a.txt", "two\n")], vec![first]);
    let third = commit_of(&sender, &[("a.txt", "three\n")], vec![second]);
    let mut sent = HashSet::new();
    let before = objects_of(&sender, &[first, second], &mut sent);
    let rest = objects_of(&sender, &[third], &mut sent);
    let whole = stream_of(&sender, &[before.clone(), rest.clone()].concat());
    // The connection ends inside the frame of the third commit: its blob and its tree arrived.
    let cut = &whole[..whole.len() - 10];

    let mut fetch = receiver.begin_fetch().unwrap();
    assert!(fetch.have().is_empty());
    assert!(fetch.resumed().is_empty());
    let error = fetch.receive(cut, Bounds::default()).unwrap_err();
    assert!(
        matches!(
            error,
            ReceiveError::Stream(StreamError::CutOff { object: 9 })
        ),
        "{error}"
    );
    // The wanted commit did not arrive, so no reference moves and the fetch is given up.
    let error = fetch.check(&set(&[third])).unwrap_err();
    assert!(
        matches!(
            error,
            ConnectivityError::Missing {
                needed_by: None,
                ..
            }
        ),
        "{error}"
    );
    drop(fetch);

    // What arrived whole is stored, and the journal names the two complete commits.
    for id in before.iter().chain(&rest[..2]) {
        assert!(receiver.objects().contains(id).unwrap(), "{id}");
    }
    assert_eq!(
        fs::read_to_string(journal_path(&receiver)).unwrap(),
        format!("stemma-fetch 1\ncommit {first}\ncommit {second}\n")
    );
    assert!(tmp_names(&receiver).is_empty());

    // The next fetch checks them again and sends the newest as a further `have`; the remote
    // then sends what is not below it.
    let mut fetch = receiver.begin_fetch().unwrap();
    assert_eq!(fetch.resumed(), [first, second]);
    assert_eq!(fetch.have(), &set(&[second]));
    let received = fetch
        .receive(&stream_of(&sender, &rest)[..], Bounds::default())
        .unwrap();
    assert_eq!((received.objects, received.commits), (3, 1));
    fetch.check(&set(&[third])).unwrap();
    // The journal is read once no fetch holds it: on Windows the lock of the system keeps
    // every other handle from reading the file.
    drop(fetch);
    assert_eq!(
        fs::read_to_string(journal_path(&receiver)).unwrap(),
        format!("stemma-fetch 1\ncommit {first}\ncommit {second}\ncommit {third}\n")
    );
    let mut fetch = receiver.begin_fetch().unwrap();
    assert_eq!(fetch.resumed(), [first, second, third]);
    assert_eq!(fetch.have(), &set(&[third]));
    fetch.check(&set(&[third])).unwrap();
    // The caller moves its remote references here, and then the journal goes.
    fetch.finish().unwrap();
    assert!(!journal_path(&receiver).exists());
    assert!(receiver.begin_fetch().unwrap().resumed().is_empty());
}

#[test]
fn a_fetch_sends_the_commits_of_its_references_and_records_no_commit_twice() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let receiver = clone_in(&dir, "receiver");
    let first = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let second = commit_of(&sender, &[("a.txt", "two\n")], vec![first]);
    let side = commit_of(&sender, &[("s.txt", "side\n")], vec![]);
    let tagged = commit_of(&sender, &[("t.txt", "tag\n")], vec![]);
    let all = [first, second, side, tagged];
    receiver
        .receive_objects(
            &stream_of_commits(&sender, &[first, side, tagged])[..],
            Bounds::default(),
        )
        .unwrap();
    set_branch(&receiver, "work", &side);
    write_remote_ref(&receiver.metadata_dir(), "origin/heads/main", first);
    let tags = receiver.metadata_dir().join("refs/tags");
    fs::create_dir_all(&tags).unwrap();
    fs::write(tags.join("v1"), encode_branch(&tagged)).unwrap();

    let mut fetch = receiver.begin_fetch().unwrap();
    assert_eq!(fetch.have(), &set(&[first, side, tagged]));
    // The stream carries everything, also what the references reach: only the new commit is
    // recorded, and a second stream with it records nothing.
    for _ in 0..2 {
        fetch
            .receive(&stream_of_commits(&sender, &all)[..], Bounds::default())
            .unwrap();
    }
    fetch.check(&set(&all)).unwrap();
    drop(fetch);
    assert_eq!(
        fs::read_to_string(journal_path(&receiver)).unwrap(),
        format!("stemma-fetch 1\ncommit {second}\n")
    );

    // The remote reference moved, as a fetch that stopped before its last step leaves it: the
    // recorded commit is reached by a reference now and is no further `have`.
    write_remote_ref(&receiver.metadata_dir(), "origin/heads/main", second);
    let fetch = receiver.begin_fetch().unwrap();
    assert!(fetch.resumed().is_empty());
    assert_eq!(fetch.have(), &set(&[second, side, tagged]));
}

#[test]
fn a_recorded_commit_is_checked_again_and_one_that_lost_an_object_is_fetched_again() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let receiver = clone_in(&dir, "receiver");
    let first = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let second = commit_of(&sender, &[("a.txt", "two\n")], vec![first]);
    let stream = stream_of_commits(&sender, &[first, second]);
    let mut fetch = receiver.begin_fetch().unwrap();
    fetch.receive(&stream[..], Bounds::default()).unwrap();
    drop(fetch);

    // The journal is a hint: an object of the second commit is gone since, as after a purge.
    remove_object(&receiver, &blob_id("two\n"));
    let mut fetch = receiver.begin_fetch().unwrap();
    assert_eq!(fetch.resumed(), [first]);
    assert_eq!(fetch.have(), &set(&[first]));
    // It arrives again and is recorded again; the reader takes the second line as the first.
    fetch.receive(&stream[..], Bounds::default()).unwrap();
    drop(fetch);
    assert_eq!(
        fs::read_to_string(journal_path(&receiver)).unwrap(),
        format!("stemma-fetch 1\ncommit {first}\ncommit {second}\ncommit {second}\n")
    );
    let fetch = receiver.begin_fetch().unwrap();
    assert_eq!(fetch.resumed(), [first, second]);
    assert_eq!(fetch.have(), &set(&[second]));

    // A line that names a commit that never arrived, written by hand, is no further `have`.
    drop(fetch);
    let stranger = commit_of(&sender, &[("x.txt", "x\n")], vec![]);
    fs::write(
        journal_path(&receiver),
        format!("stemma-fetch 1\ncommit {stranger}\n"),
    )
    .unwrap();
    let fetch = receiver.begin_fetch().unwrap();
    assert!(fetch.resumed().is_empty());
    assert!(fetch.have().is_empty());
}

#[test]
fn a_last_line_of_the_journal_that_was_cut_off_is_removed() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let receiver = clone_in(&dir, "receiver");
    let first = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    receiver
        .receive_objects(&stream_of_commits(&sender, &[first])[..], Bounds::default())
        .unwrap();
    let whole = format!("stemma-fetch 1\ncommit {first}\n");

    // A process that ended while it wrote a line, and one that ended inside the first line.
    for (content, resumed, after) in [
        (format!("{whole}commit b3:12"), vec![first], whole.as_str()),
        (whole.clone(), vec![first], whole.as_str()),
        ("stemma-fe".to_owned(), vec![], "stemma-fetch 1\n"),
        (String::new(), vec![], "stemma-fetch 1\n"),
    ] {
        fs::write(journal_path(&receiver), &content).unwrap();
        let fetch = receiver.begin_fetch().unwrap();
        assert_eq!(fetch.resumed(), resumed, "{content:?}");
        drop(fetch);
        assert_eq!(fs::read_to_string(journal_path(&receiver)).unwrap(), after);
    }
}

#[test]
fn a_journal_that_no_fetch_writes_is_refused_with_its_line() {
    let dir = TempDir::new();
    let receiver = clone_in(&dir, "receiver");
    let id = commit_of(&receiver, &[("a.txt", "one\n")], vec![]);
    let upper = id.to_string().to_uppercase().replace("B3:", "b3:");
    for (content, line) in [
        ("stemma-fetch 2\n".to_owned(), 1),
        ("stemma-purge 1\n".to_owned(), 1),
        ("\n".to_owned(), 1),
        (format!("stemma-fetch 1\ncommit {id}\n\n"), 3),
        (format!("stemma-fetch 1\ncommit  {id}\n"), 2),
        (format!("stemma-fetch 1\ncommit {id} \n"), 2),
        (format!("stemma-fetch 1\ncommit {upper}\n"), 2),
        (format!("stemma-fetch 1\ncommit {id}\r\n"), 2),
        (format!("stemma-fetch 1\ncommit {id}\ntree {id}\n"), 3),
        // A last line without a line feed that is not the beginning of a line of a fetch.
        ("garbage".to_owned(), 1),
        ("stemma-fetch 12".to_owned(), 1),
        (format!("stemma-fetch 1\ncommit {id}\ntree"), 3),
        (format!("stemma-fetch 1\ncommit {id}0"), 2),
        ("stemma-fetch 1\ncommit B3".to_owned(), 2),
        ("stemma-fetch 1\ncommit \u{fffd}\n".to_owned(), 2),
    ] {
        fs::write(journal_path(&receiver), &content).unwrap();
        let error = receiver.begin_fetch().unwrap_err();
        assert!(
            matches!(
                &error,
                ReceiveError::Journal(FetchJournalError::Malformed { path, line: found })
                    if *found == line && *path == journal_path(&receiver)
            ),
            "{content:?}: {error}"
        );
        assert_eq!(error.kind(), FailureKind::Damaged);
        assert!(error.to_string().contains("Remove the file"), "{error}");
        // The refusal changes nothing.
        assert_eq!(
            fs::read_to_string(journal_path(&receiver)).unwrap(),
            content
        );
    }
}

#[test]
fn a_second_fetch_at_once_is_refused_and_a_bare_repository_has_no_fetch() {
    let dir = TempDir::new();
    let receiver = clone_in(&dir, "receiver");
    let fetch = receiver.begin_fetch().unwrap();
    let error = receiver.begin_fetch().unwrap_err();
    assert!(
        matches!(
            &error,
            ReceiveError::Journal(FetchJournalError::Running { path })
                if *path == journal_path(&receiver)
        ),
        "{error}"
    );
    assert_eq!(error.kind(), FailureKind::Locked);
    // The journal of the fetch that runs is untouched, and a fetch after it begins.
    drop(fetch);
    assert_eq!(
        fs::read_to_string(journal_path(&receiver)).unwrap(),
        "stemma-fetch 1\n"
    );
    receiver.begin_fetch().unwrap().finish().unwrap();
    assert!(!journal_path(&receiver).exists());

    let hub = hub(&dir);
    let error = hub.begin_fetch().unwrap_err();
    assert!(
        matches!(
            &error,
            ReceiveError::Refs(source)
                if matches!(
                    &**source,
                    RefError::Bare { lacks: BareLacks::OperationLog, operation: "fetch", .. }
                )
        ),
        "{error}"
    );
    assert!(!journal_path(&hub).exists());
}

/// Takes the lock of the references as a writer does, which empties `tmp/` of files.
fn take_the_lock(repository: &Repository) {
    let operation = OperationName::try_new("push").unwrap();
    let allowed = Allowed::ask(&AllowAll, &daniel(), &operation, "main").unwrap();
    drop(repository.refs_store().lock_writer(&allowed).unwrap());
}

#[test]
fn the_lock_holder_removes_no_file_under_a_running_upload() {
    let dir = TempDir::new();
    let hub = hub(&dir);
    let root = hub.metadata_dir();
    let running = ReceiveDir::create(&root).unwrap();
    let name = running
        .path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(is_receive_dir_name(&name), "{name}");
    assert_eq!(running.path.parent().unwrap(), root.join("tmp"));
    let batch = hub.objects().batch_in(running.path.clone());
    let id = batch.put_raw(Kind::Blob, b"in flight\n").unwrap();
    let inside = |path: &Path| fs::read_dir(path).unwrap().count();
    // The guard and the temporary file of the object.
    assert_eq!(inside(&running.path), 2);
    // A file directly in `tmp/`, as a writer under the lock leaves it when it is killed.
    fs::write(root.join("tmp/1-0-0123456789abcdef"), "left").unwrap();

    take_the_lock(&hub);
    assert_eq!(tmp_names(&hub), std::slice::from_ref(&name));
    assert_eq!(inside(&running.path), 2);
    // Neither does another receiving operation, nor the cleanup that a hub asks for.
    hub.remove_receive_leftovers();
    let empty = ObjectStreamWriter::new(Vec::new()).unwrap().finish();
    hub.receive_objects(&empty[..], Bounds::default()).unwrap();
    assert_eq!(tmp_names(&hub), [name]);
    assert_eq!(inside(&running.path), 2);

    // The upload ends: its object is in place, and its directory is gone.
    batch.finish().unwrap();
    assert!(hub.objects().get(&id).unwrap().is_some());
    drop(running);
    assert!(tmp_names(&hub).is_empty());
}

#[test]
fn a_crashed_upload_leaves_a_directory_that_is_removed_later() {
    let dir = TempDir::new();
    let hub = hub(&dir);
    let root = hub.metadata_dir();
    let leave = || {
        let crashed = ReceiveDir::create(&root).unwrap();
        let batch = hub.objects().batch_in(crashed.path.clone());
        batch.put_raw(Kind::Blob, b"lost\n").unwrap();
        // A process that is killed removes nothing and releases its locks.
        std::mem::forget(batch);
        crashed.crash()
    };

    // What the crash leaves is a directory, so the writer that takes the lock leaves it.
    let left = leave();
    take_the_lock(&hub);
    assert!(left.is_dir());
    assert_eq!(fs::read_dir(&left).unwrap().count(), 2);
    // The cleanup that a hub asks for at its start removes it.
    hub.remove_receive_leftovers();
    assert!(tmp_names(&hub).is_empty());

    // So does the next receiving operation, an upload or a fetch.
    let left = leave();
    let empty = ObjectStreamWriter::new(Vec::new()).unwrap().finish();
    hub.receive_objects(&empty[..], Bounds::default()).unwrap();
    assert!(!left.exists());
    assert!(tmp_names(&hub).is_empty());
    assert!(!hub.objects().contains(&blob_id("lost\n")).unwrap());

    // A directory that ended before it had its guard is removed while it is empty.
    let unguarded = root.join("tmp/receive-1-0-0123456789abcdef");
    fs::create_dir(&unguarded).unwrap();
    hub.remove_receive_leftovers();
    assert!(!unguarded.exists());
}

#[test]
fn the_cleanup_leaves_what_no_receiving_operation_made() {
    let dir = TempDir::new();
    let hub = hub(&dir);
    let tmp = hub.metadata_dir().join("tmp");
    // A directory of another name, a file with the name of a receiving operation, and a
    // directory with that name that holds something and has no guard: nothing says that its
    // operation has ended.
    fs::create_dir(tmp.join("receive-notes")).unwrap();
    fs::create_dir(tmp.join("other")).unwrap();
    fs::write(tmp.join("receive-1-1-0123456789abcdef"), "a file").unwrap();
    let unguarded = tmp.join("receive-1-2-0123456789abcdef");
    fs::create_dir(&unguarded).unwrap();
    fs::write(unguarded.join("1-3-0123456789abcdef"), "an object").unwrap();

    hub.remove_receive_leftovers();
    assert_eq!(
        tmp_names(&hub),
        [
            "other",
            "receive-1-1-0123456789abcdef",
            "receive-1-2-0123456789abcdef",
            "receive-notes"
        ]
    );
    assert!(unguarded.join("1-3-0123456789abcdef").exists());
}

#[test]
fn the_objects_of_a_stream_are_made_durable_in_batches_at_a_commit() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let receiver = clone_in(&dir, "receiver");
    // More objects than one batch holds before a commit, and more commits than batches.
    let mut commits = Vec::new();
    for round in 0..4 {
        let files: Vec<(String, String)> = (0..FLUSH_OBJECTS)
            .map(|file| (format!("f{file}.txt"), format!("{round} {file}\n")))
            .collect();
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        commits.push(commit_of(
            &sender,
            &files,
            commits.last().copied().into_iter().collect(),
        ));
        commits.push(commit_of(
            &sender,
            &[("only.txt", "x\n")],
            vec![*commits.last().unwrap()],
        ));
    }
    let stream = stream_of_commits(&sender, &commits);

    let mut fetch = receiver.begin_fetch().unwrap();
    fetch.receive(&stream[..], Bounds::default()).unwrap();
    fetch.check(&set(&commits)).unwrap();
    drop(fetch);
    let lines: String = commits
        .iter()
        .map(|commit| format!("commit {commit}\n"))
        .collect();
    assert_eq!(
        fs::read_to_string(journal_path(&receiver)).unwrap(),
        format!("stemma-fetch 1\n{lines}")
    );
    assert!(tmp_names(&receiver).is_empty());
}

/// A stream that calls `hook` once, when the reader has taken the first `at` bytes.
struct Hooked<'a, F: FnMut()> {
    bytes: &'a [u8],
    at: usize,
    position: usize,
    hook: Option<F>,
}

impl<F: FnMut()> Read for Hooked<'_, F> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl<F: FnMut()> BufRead for Hooked<'_, F> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.position >= self.at {
            if let Some(mut hook) = self.hook.take() {
                hook();
            }
            return Ok(&self.bytes[self.position..]);
        }
        Ok(&self.bytes[self.position..self.at])
    }

    fn consume(&mut self, count: usize) {
        self.position += count;
    }
}

#[test]
fn an_upload_keeps_its_temporary_files_while_another_writer_takes_the_lock() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    // The first commit fills a batch, so the objects of the second lie in a batch of their own.
    let first = commit_of(
        &sender,
        &[
            ("a.txt", "1\n"),
            ("b.txt", "2\n"),
            ("c.txt", "3\n"),
            ("d.txt", "4\n"),
        ],
        vec![],
    );
    let second = commit_of(&sender, &[("a.txt", "5\n")], vec![first]);
    let mut sent = HashSet::new();
    let before = stream_of(&sender, &objects_of(&sender, &[first], &mut sent)).len();
    let stream = stream_of_commits(&sender, &[first, second]);

    // Inside the frame of the first commit, and inside that of the second.
    for at in [before - 10, stream.len() - 10] {
        let hub_dir = TempDir::new();
        let hub = hub(&hub_dir);
        let mut seen = false;
        let hooked = Hooked {
            bytes: &stream,
            at,
            position: 0,
            hook: Some(|| {
                // A file directly in `tmp/` goes, as every file there does.
                let stray = hub.metadata_dir().join("tmp/1-0-0123456789abcdef");
                fs::write(&stray, "left").unwrap();
                take_the_lock(&hub);
                assert!(!stray.exists());
                hub.remove_receive_leftovers();
                let empty = ObjectStreamWriter::new(Vec::new()).unwrap().finish();
                hub.receive_objects(&empty[..], Bounds::default()).unwrap();
                // The directory of the upload is there, with its guard and the temporary
                // files of the objects that are not yet in place.
                let names = tmp_names(&hub);
                assert_eq!(names.len(), 1, "{names:?}");
                assert!(is_receive_dir_name(&names[0]), "{names:?}");
                let inside = fs::read_dir(hub.metadata_dir().join("tmp").join(&names[0]))
                    .unwrap()
                    .count();
                assert!(inside > 1, "{inside}");
                seen = true;
            }),
        };

        let received = hub.receive_objects(hooked, Bounds::default()).unwrap();
        assert!(seen);
        assert_eq!(received.commits, 2);
        hub.check_connectivity(&[second]).unwrap();
        assert!(tmp_names(&hub).is_empty());
    }
}

#[test]
fn an_entry_of_another_sort_under_the_same_name_is_not_taken_from_the_parent() {
    let dir = TempDir::new();
    let hub = hub(&dir);
    let store = hub.objects();
    let blob = store.put_blob(&Blob::new("one\n")).unwrap();
    let commit = |tree: TreeId, parents: Vec<CommitId>| {
        let commit = Commit::try_new(
            tree,
            parents,
            ChangeId::generate().unwrap(),
            daniel(),
            1,
            "next",
        )
        .unwrap();
        store.put_commit(&commit).unwrap()
    };
    let tree_of = |entry: TreeEntry| {
        store
            .put_tree(&Tree::try_new(vec![entry]).unwrap())
            .unwrap()
    };

    // The parent has the blob as the file `src`; the child names the same ID as a directory.
    let file = tree_of(TreeEntry::file("src", FileMode::File, blob).unwrap());
    let parent = commit(file, vec![]);
    set_branch(&hub, "main", &parent);
    let as_directory = tree_of(TreeEntry::directory("src", TreeId::new(blob.object_id())).unwrap());
    let error = hub
        .check_connectivity(&[commit(as_directory, vec![parent])])
        .unwrap_err();
    assert!(
        matches!(
            &error,
            ConnectivityError::WrongKind { id, expected: Kind::Tree, found: Kind::Blob, .. }
                if *id == blob.object_id()
        ),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "object {} is a blob, not a tree, which {as_directory} names",
            blob.object_id()
        )
    );

    // The other way round: the parent has a tree as the directory `src`, the child as a file.
    let directory = tree_of(TreeEntry::directory("src", file).unwrap());
    let parent = commit(directory, vec![]);
    set_branch(&hub, "main", &parent);
    let as_file =
        tree_of(TreeEntry::file("src", FileMode::File, BlobId::new(file.object_id())).unwrap());
    let error = hub
        .check_connectivity(&[commit(as_file, vec![parent])])
        .unwrap_err();
    assert!(
        matches!(
            &error,
            ConnectivityError::WrongKind { id, expected: Kind::Blob, found: Kind::Tree, .. }
                if *id == file.object_id()
        ),
        "{error}"
    );
}

#[test]
fn a_changed_directory_is_compared_with_the_directory_of_the_parent() {
    let dir = TempDir::new();
    let hub = hub(&dir);
    let first = commit_of(
        &hub,
        &[("src/keep.txt", "keep\n"), ("src/b.txt", "two\n")],
        vec![],
    );
    set_branch(&hub, "main", &first);
    let second = commit_of(
        &hub,
        &[("src/keep.txt", "keep\n"), ("src/b.txt", "three\n")],
        vec![first],
    );
    // The directory changed, the file in it did not: the parent, which is complete, has it at
    // the same path, so it is not asked for.
    remove_object(&hub, &blob_id("keep\n"));
    hub.check_connectivity(&[second]).unwrap();
}

#[test]
fn a_damaged_file_of_an_object_is_an_error_of_the_store_and_a_fetch_writes_it_anew() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let receiver = clone_in(&dir, "receiver");
    let commit = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let stream = stream_of_commits(&sender, &[commit]);
    let mut fetch = receiver.begin_fetch().unwrap();
    fetch.receive(&stream[..], Bounds::default()).unwrap();
    drop(fetch);
    let journal = format!("stemma-fetch 1\ncommit {commit}\n");
    assert_eq!(
        fs::read_to_string(journal_path(&receiver)).unwrap(),
        journal
    );
    let blob = receiver.objects().object_path(&blob_id("one\n"));

    // The storage fails: the check cannot be made, and a recorded commit stops the fetch.
    {
        let _denied = fault::deny_reads_of(&blob);
        let error = receiver.check_connectivity(&[commit]).unwrap_err();
        assert!(matches!(error, ConnectivityError::Store(_)), "{error}");
        assert!(!error.is_incomplete());
        assert_eq!(error.kind(), FailureKind::Other);
        assert!(error.store_error().is_some());
        assert_eq!(
            error.io_error().map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied)
        );
        let error = receiver.begin_fetch().unwrap_err();
        assert!(
            matches!(
                &error,
                ReceiveError::Connectivity(ConnectivityError::Store(_))
            ),
            "{error}"
        );
        assert!(error.store_error().is_some());
        assert!(error.io_error().is_some());
    }
    assert_eq!(
        fs::read_to_string(journal_path(&receiver)).unwrap(),
        journal
    );

    // The file of the blob is damaged: the check names the store, and it is damage.
    crate::testutil::damage(&blob, b"junk");
    let error = receiver.check_connectivity(&[commit]).unwrap_err();
    assert!(matches!(error, ConnectivityError::Store(_)), "{error}");
    assert!(!error.is_incomplete());
    assert_eq!(error.kind(), FailureKind::Damaged);
    assert!(error.io_error().is_none());
    // The fetch does not resume with the commit, and the transfer writes the file anew.
    let mut fetch = receiver.begin_fetch().unwrap();
    assert!(fetch.resumed().is_empty());
    assert!(fetch.have().is_empty());
    if cfg!(any(unix, windows)) {
        fetch.receive(&stream[..], Bounds::default()).unwrap();
        fetch.check(&set(&[commit])).unwrap();
    }
}

#[test]
fn a_recorded_id_that_names_no_commit_is_no_further_have() {
    let dir = TempDir::new();
    let receiver = clone_in(&dir, "receiver");
    let commit = commit_of(&receiver, &[("a.txt", "one\n")], vec![]);
    let tree = receiver
        .objects()
        .get_commit(&commit)
        .unwrap()
        .unwrap()
        .tree();
    let bad = receiver
        .objects()
        .put_raw(Kind::Commit, b"not a commit")
        .unwrap();

    // A tree, and a commit whose body the strict reader refuses, written by hand.
    for id in [tree.object_id(), bad] {
        fs::write(
            journal_path(&receiver),
            format!("stemma-fetch 1\ncommit {id}\n"),
        )
        .unwrap();
        let fetch = receiver.begin_fetch().unwrap();
        assert!(fetch.resumed().is_empty(), "{id}");
        assert!(fetch.have().is_empty(), "{id}");
    }
}

#[test]
fn a_commit_that_is_not_connected_ends_the_stream_after_the_commits_before_it_are_recorded() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let receiver = clone_in(&dir, "receiver");
    let good = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let bad = commit_of(&sender, &[("a.txt", "two\n")], vec![good]);
    // Both commits arrive in one batch, and the blob of the second is damaged.
    let mut stream = stream_of_commits(&sender, &[good, bad]);
    let at = stream
        .windows(4)
        .position(|bytes| bytes == b"two\n")
        .unwrap();
    stream[at] = b'T';

    let mut fetch = receiver.begin_fetch().unwrap();
    let error = fetch.receive(&stream[..], Bounds::default()).unwrap_err();
    assert!(
        matches!(
            &error,
            ReceiveError::Connectivity(ConnectivityError::Missing { id, .. })
                if *id == blob_id("two\n")
        ),
        "{error}"
    );
    assert!(error.store_error().is_none());
    drop(fetch);
    assert_eq!(
        fs::read_to_string(journal_path(&receiver)).unwrap(),
        format!("stemma-fetch 1\ncommit {good}\n")
    );
    assert_eq!(receiver.begin_fetch().unwrap().resumed(), [good]);
}

#[test]
fn a_journal_that_cannot_be_written_or_removed_is_reported_and_the_objects_stay() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let receiver = clone_in(&dir, "receiver");
    let commit = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let stream = stream_of_commits(&sender, &[commit]);

    // The handle of the journal cannot write.
    let mut fetch = receiver.begin_fetch().unwrap();
    fetch.journal.file = File::open(journal_path(&receiver)).unwrap();
    let error = fetch.receive(&stream[..], Bounds::default()).unwrap_err();
    // Nothing was written, and the cut after the failure failed on this handle too: the
    // journal remembers where the next write has to cut first.
    assert_eq!(fetch.journal.torn, Some("stemma-fetch 1\n".len() as u64));
    assert!(
        matches!(
            &error,
            ReceiveError::Journal(FetchJournalError::Io {
                operation: "write the journal of the fetch",
                ..
            })
        ),
        "{error}"
    );
    assert!(error.io_error().is_some());
    assert!(error.store_error().is_none());
    assert_eq!(error.kind(), FailureKind::Other);
    assert!(
        error
            .to_string()
            .starts_with("could not write the journal of the fetch `"),
        "{error}"
    );
    // The objects are stored, the commit is complete, and only its line is missing.
    fetch.check(&set(&[commit])).unwrap();
    drop(fetch);
    assert_eq!(
        fs::read_to_string(journal_path(&receiver)).unwrap(),
        "stemma-fetch 1\n"
    );

    // The journal cannot be removed: the fetch has happened, and the file stays for the next.
    let fetch = receiver.begin_fetch().unwrap();
    let error = {
        let _denied = fault::deny_writes_in(&receiver.metadata_dir());
        fetch.finish().unwrap_err()
    };
    assert!(
        matches!(
            &error,
            FetchJournalError::Io {
                operation: "remove the journal of the fetch",
                ..
            }
        ),
        "{error}"
    );
    assert!(error.io_error().is_some());
    assert!(journal_path(&receiver).exists());
    receiver.begin_fetch().unwrap().finish().unwrap();
    assert!(!journal_path(&receiver).exists());
}

#[test]
fn a_batch_is_durable_before_its_commits_are_reported() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let hub = hub(&dir);
    // Three objects per commit and four to a batch: a batch closes at every second commit.
    let mut commits: Vec<CommitId> = Vec::new();
    for round in 0..5 {
        let text = format!("{round}\n");
        let parents = commits.last().copied().into_iter().collect();
        commits.push(commit_of(&sender, &[("a.txt", &text)], parents));
    }
    let stream = stream_of_commits(&sender, &commits);

    let mut batches = Vec::new();
    receive_stream(
        hub.objects(),
        &hub.metadata_dir(),
        &stream[..],
        Bounds::default(),
        |reported| {
            for commit in reported {
                assert!(hub.objects().contains(&commit.object_id()).unwrap());
            }
            if !reported.is_empty() {
                batches.push(reported.to_vec());
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        batches,
        [&commits[..2], &commits[2..4], &commits[4..]].map(<[CommitId]>::to_vec)
    );
}

#[test]
fn a_journal_whose_name_is_gone_or_leads_elsewhere_is_not_the_open_file() {
    let dir = TempDir::new();
    let receiver = clone_in(&dir, "receiver");
    let path = journal_path(&receiver);
    // A second fetch opened the journal just before the first one ended and removed it.
    let fetch = receiver.begin_fetch().unwrap();
    let opened = OpenOptions::new().read(true).write(true).open(&path);
    // On Windows the lock of the running fetch does not keep the file from being opened.
    let opened = opened.unwrap();
    assert!(names_file(&path, &opened));
    fetch.finish().unwrap();
    assert!(!names_file(&path, &opened));
    // A third fetch made a new journal under the name: on Unix the file tells them apart.
    let third = receiver.begin_fetch().unwrap();
    assert_eq!(names_file(&path, &opened), !cfg!(unix));
    assert!(names_file(&path, &third.journal.file));
    // The handle that was opened too early is free to lock, and no fetch may use it.
    drop(opened);
    third.finish().unwrap();
}

#[test]
fn a_torn_line_that_could_not_be_cut_is_cut_before_the_next_line() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let receiver = clone_in(&dir, "receiver");
    let commit = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let mut fetch = receiver.begin_fetch().unwrap();
    // What a write leaves that failed in its middle and whose cut failed too.
    let end = fetch.journal.file.seek(SeekFrom::End(0)).unwrap();
    fetch.journal.file.write_all(b"commit b3:12").unwrap();
    fetch.journal.torn = Some(end);

    fetch
        .receive(
            &stream_of_commits(&sender, &[commit])[..],
            Bounds::default(),
        )
        .unwrap();
    assert_eq!(fetch.journal.torn, None);
    drop(fetch);
    assert_eq!(
        fs::read_to_string(journal_path(&receiver)).unwrap(),
        format!("stemma-fetch 1\ncommit {commit}\n")
    );
}

#[test]
fn the_beginning_of_a_line_of_a_fetch_is_a_cut_line_and_nothing_else_is() {
    let id = format!("b3:{}", "0123456789abcdef".repeat(4));
    // The first line, at every length up to its line feed.
    let header = "stemma-fetch 1\n";
    for end in 0..=header.len() {
        assert!(is_cut_line(&header.as_bytes()[..end], true), "{end}");
    }
    for tail in ["stemma-fetch 2", "stemma-fetch 1 ", "commit ", "x", "\n"] {
        assert!(!is_cut_line(tail.as_bytes(), true), "{tail:?}");
    }
    // A line `commit <id>`, at every length up to the whole ID without its line feed.
    let line = format!("commit {id}");
    for end in 0..=line.len() {
        assert!(is_cut_line(&line.as_bytes()[..end], false), "{end}");
    }
    for tail in [
        format!("commit {id}0"),
        format!("commit {id} "),
        "commit B3:".to_owned(),
        "commit b3:0g".to_owned(),
        "commit b3:0A".to_owned(),
        "commit  ".to_owned(),
        "commit b4".to_owned(),
        "commits".to_owned(),
        "tree".to_owned(),
        "stemma-fetch 1".to_owned(),
    ] {
        assert!(!is_cut_line(tail.as_bytes(), false), "{tail:?}");
    }
}

#[test]
fn a_whole_line_that_lacks_only_its_line_feed_is_cut_and_the_fetch_begins() {
    let dir = TempDir::new();
    let receiver = clone_in(&dir, "receiver");
    let commit = commit_of(&receiver, &[("a.txt", "one\n")], vec![]);
    for content in [
        format!("stemma-fetch 1\ncommit {commit}"),
        "stemma-fetch 1\ncommit ".to_owned(),
        "stemma-fetch 1\nc".to_owned(),
        "stemma-fetch 1".to_owned(),
    ] {
        fs::write(journal_path(&receiver), &content).unwrap();
        let fetch = receiver.begin_fetch().unwrap();
        assert!(fetch.resumed().is_empty(), "{content:?}");
        drop(fetch);
        assert_eq!(
            fs::read_to_string(journal_path(&receiver)).unwrap(),
            "stemma-fetch 1\n",
            "{content:?}"
        );
    }
}

#[test]
fn what_the_sender_delivered_goes_before_a_journal_that_cannot_be_written() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let receiver = clone_in(&dir, "receiver");
    let good = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let bad = commit_of(&sender, &[("a.txt", "two\n")], vec![good]);
    let after = commit_of(&sender, &[("a.txt", "three\n")], vec![bad]);
    let mut stream = stream_of_commits(&sender, &[good, bad, after]);
    let at = stream
        .windows(4)
        .position(|bytes| bytes == b"two\n")
        .unwrap();
    stream[at] = b'T';

    let mut fetch = receiver.begin_fetch().unwrap();
    fetch.journal.file = File::open(journal_path(&receiver)).unwrap();
    let error = fetch.receive(&stream[..], Bounds::default()).unwrap_err();
    assert!(
        matches!(
            &error,
            ReceiveError::Connectivity(ConnectivityError::Missing { id, .. })
                if *id == blob_id("two\n")
        ),
        "{error}"
    );
    // The check ended at the commit that is not connected: the one after it was not looked at.
    assert!(fetch.connectivity.verified.contains(&good));
    assert!(!fetch.connectivity.verified.contains(&after));
}

#[test]
fn a_reader_that_returns_another_id_is_a_defect_and_not_an_incomplete_upload() {
    let requested = blob_id("one\n");
    let parsed = blob_id("two\n");
    let mismatch = ConnectivityError::Unreadable {
        needed_by: None,
        source: Box::new(ReadError::IdMismatch { requested, parsed }),
    };
    assert!(!mismatch.is_incomplete());
    assert_eq!(mismatch.kind(), FailureKind::Damaged);
    assert!(mismatch.store_error().is_none());
    assert!(mismatch.io_error().is_none());
    // Without an object that names it, the message is that of the reader alone.
    assert_eq!(
        mismatch.to_string(),
        ReadError::IdMismatch { requested, parsed }.to_string()
    );

    // The check never builds this value, and its class is that of the store error inside.
    let path = PathBuf::from("objects/b3/00");
    let inside = ConnectivityError::Unreadable {
        needed_by: None,
        source: Box::new(ReadError::Store(Box::new(StoreError::Io {
            operation: "read the object file",
            path,
            source: io::Error::other("disk"),
        }))),
    };
    assert_eq!(inside.kind(), FailureKind::Other);
    assert!(inside.store_error().is_some());
    assert!(inside.io_error().is_some());
}

/// A carrier that fails once the reader has taken its bytes.
struct Broken<'a>(&'a [u8]);

impl Read for Broken<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for Broken<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.0.is_empty() {
            return Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset"));
        }
        Ok(self.0)
    }

    fn consume(&mut self, count: usize) {
        self.0 = &self.0[count..];
    }
}

#[test]
fn a_carrier_that_fails_is_an_error_of_the_stream_and_what_arrived_is_kept() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let receiver = clone_in(&dir, "receiver");
    let commit = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let stream = stream_of_commits(&sender, &[commit]);

    let mut fetch = receiver.begin_fetch().unwrap();
    let error = fetch
        .receive(Broken(&stream), Bounds::default())
        .unwrap_err();
    assert!(
        matches!(error, ReceiveError::Stream(StreamError::Io(_))),
        "{error}"
    );
    // A connection that broke is no failure of the file system of the repository.
    assert!(error.io_error().is_none());
    assert!(error.store_error().is_none());
    assert!(!error.exceeds_bound());
    assert_eq!(error.kind(), FailureKind::Other);
    drop(fetch);
    assert_eq!(
        fs::read_to_string(journal_path(&receiver)).unwrap(),
        format!("stemma-fetch 1\ncommit {commit}\n")
    );
    assert!(tmp_names(&receiver).is_empty());
}

#[test]
fn a_store_that_cannot_be_written_fails_the_upload_and_leaves_no_directory() {
    let dir = TempDir::new();
    let sender = clone_in(&dir, "sender");
    let hub = hub(&dir);
    let commit = commit_of(&sender, &[("a.txt", "one\n")], vec![]);
    let stream = stream_of_commits(&sender, &[commit]);

    let error = {
        let _denied = fault::deny_writes_in(&hub.metadata_dir().join("objects"));
        hub.receive_objects(&stream[..], Bounds::default())
            .unwrap_err()
    };
    assert!(matches!(error, ReceiveError::Store(_)), "{error}");
    assert!(error.store_error().is_some());
    assert_eq!(
        error.io_error().map(io::Error::kind),
        Some(io::ErrorKind::PermissionDenied)
    );
    assert_eq!(error.kind(), FailureKind::Other);
    assert!(!hub.objects().contains(&commit.object_id()).unwrap());
    assert!(tmp_names(&hub).is_empty(), "{:?}", tmp_names(&hub));
    // The upload is repeated and passes.
    hub.receive_objects(&stream[..], Bounds::default()).unwrap();
    hub.check_connectivity(&[commit]).unwrap();
}

#[test]
fn references_that_cannot_be_read_stop_a_fetch_before_it_makes_its_journal() {
    let dir = TempDir::new();
    let receiver = clone_in(&dir, "receiver");
    let commit = commit_of(&receiver, &[("a.txt", "one\n")], vec![]);
    let _denied = fault::deny_reads_of(&receiver.metadata_dir().join("refs/heads"));

    let error = receiver.begin_fetch().unwrap_err();
    assert!(matches!(error, ReceiveError::Refs(_)), "{error}");
    assert!(error.io_error().is_some());
    assert!(error.store_error().is_none());
    assert!(!journal_path(&receiver).exists());

    let error = receiver.check_connectivity(&[commit]).unwrap_err();
    assert!(matches!(error, ConnectivityError::Refs(_)), "{error}");
    assert!(!error.is_incomplete());
    assert!(error.io_error().is_some());
    assert!(error.store_error().is_none());
}
