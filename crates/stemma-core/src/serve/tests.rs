//! Tests of the answering side of a fetch (ADR-023, STEMMA-288): the list of references, the
//! stream of a clone and of a fetch, the refusal of a `want` that no reference reaches, and the
//! order of the stream, read back by the receiving side ([`receive`](crate::receive)).

use std::fs;
use std::io::{self, Read};

use super::*;
use crate::change::ChangeId;
use crate::index::Index;
use crate::object::{Blob, Commit, FileMode};
use crate::oplog::OperationName;
use crate::protocol::{ObjectStreamReader, StreamObject};
use crate::receive::{Bounds, ConnectivityError};
use crate::refs::encode_branch;
use crate::refs::RefError;
use crate::repository::InitOptions;
use crate::testutil::{conflicted_commit, damage, write_remote_ref, TempDir};
use crate::transport::{Answer, MemoryTransport, Transport, TransportError};
use crate::{Actor, AllowAll, Allowed};

/// An Ed25519 public key, made for these tests; nobody holds its private half.
const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIjwWEZ4603dDnTO8ilPzaYzYG5PnbKb6TxOzauTQ9du";

fn daniel() -> Actor {
    Actor::human("daniel").unwrap()
}

fn repository_id() -> RepositoryId {
    "00112233445566778899aabbccddeeff".parse().unwrap()
}

/// The hash of a record entry whose bytes are all `byte`.
fn record(byte: u8) -> RecordHash {
    format!("b3:{}", format!("{byte:02x}").repeat(32))
        .parse()
        .unwrap()
}

/// A commit ID whose bytes are all `byte`, which no store of these tests holds.
fn absent(byte: u8) -> CommitId {
    let id: ObjectId = format!("b3:{}", format!("{byte:02x}").repeat(32))
        .parse()
        .unwrap();
    CommitId::new(id)
}

/// The facts of a hub whose record names the same entry for every reference.
fn facts() -> ListFacts<'static> {
    let key = PublicKey::from_openssh(KEY).unwrap();
    ListFacts::new(repository_id(), key, Vec::new(), |_| Some(record(0xa1)))
}

/// A bare repository in the directory `name` below `dir`.
fn bare(dir: &TempDir, name: &str) -> Repository {
    Repository::init_bare(dir.path().join(name), &daniel(), &AllowAll).unwrap()
}

/// A repository with a working copy in the directory `name` below `dir`.
fn with_working_copy(dir: &TempDir, name: &str) -> Repository {
    let path = dir.path().join(name);
    Repository::init(&path, &InitOptions::default(), &daniel(), &AllowAll).unwrap()
}

/// Stores a commit with these files and parents in the object store of `repository`. A path
/// with a slash makes a directory.
fn commit_of(repository: &Repository, files: &[(&str, &str)], parents: Vec<CommitId>) -> CommitId {
    let mut index = Index::new();
    for (path, text) in files {
        let blob = repository.objects().put_blob(&Blob::new(*text)).unwrap();
        index.insert(*path, FileMode::File, blob).unwrap();
    }
    let commit = Commit::try_new(
        index.write_tree(repository.objects()).unwrap(),
        parents,
        ChangeId::generate().unwrap(),
        daniel(),
        1,
        "a commit",
    )
    .unwrap();
    repository.objects().put_commit(&commit).unwrap()
}

/// Writes the file of a reference by hand, `heads/<name>` or `tags/<name>`: nothing in the
/// crate moves a reference of a bare repository before the accepting operation is built.
fn set_reference(repository: &Repository, name: &str, commit: &CommitId) {
    let path = repository.metadata_dir().join("refs").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, encode_branch(commit)).unwrap();
}

fn request(want: &[CommitId], have: &[CommitId]) -> FetchRequest {
    FetchRequest::try_new(want.iter().copied(), have.iter().copied()).unwrap()
}

/// The stream that `repository` answers to the request.
fn answer(repository: &Repository, request: &FetchRequest) -> Vec<u8> {
    let mut stream = Vec::new();
    repository.answer_fetch(request, &mut stream).unwrap();
    stream
}

/// The objects of a stream, in its order.
fn objects(stream: &[u8]) -> Vec<StreamObject> {
    let mut reader = ObjectStreamReader::new(stream, u64::MAX).unwrap();
    let mut objects = Vec::new();
    while let Some(object) = reader.next_object().unwrap() {
        objects.push(object);
    }
    objects
}

fn ids(stream: &[u8]) -> Vec<ObjectId> {
    objects(stream).iter().map(StreamObject::id).collect()
}

/// The commits of a stream, in its order.
fn commits(stream: &[u8]) -> Vec<CommitId> {
    objects(stream)
        .iter()
        .filter(|object| matches!(object.kind, Kind::Commit | Kind::Commit2))
        .map(|object| CommitId::new(object.id()))
        .collect()
}

fn blob_id(text: &str) -> ObjectId {
    Blob::new(text).id().object_id()
}

/// The beginnings of a stream that end with a commit, each with that commit.
fn prefixes(stream: &[u8]) -> Vec<(Vec<u8>, CommitId)> {
    let objects = objects(stream);
    let mut prefixes = Vec::new();
    for (index, last) in objects.iter().enumerate() {
        if !matches!(last.kind, Kind::Commit | Kind::Commit2) {
            continue;
        }
        let mut writer = ObjectStreamWriter::new(Vec::new()).unwrap();
        for object in &objects[..=index] {
            writer.write(object.kind, &object.body).unwrap();
        }
        prefixes.push((writer.finish(), CommitId::new(last.id())));
    }
    prefixes
}

/// A history with a merge, a directory, a file that returns to an older content, and a
/// conflicted commit on top: `root`, then `left` and `right`, then `merge`, then `top`.
struct History {
    root: CommitId,
    left: CommitId,
    right: CommitId,
    merge: CommitId,
    top: CommitId,
}

fn history(repository: &Repository) -> History {
    let root = commit_of(
        repository,
        &[("a.txt", "one\n"), ("dir/b.txt", "b\n")],
        Vec::new(),
    );
    let left = commit_of(
        repository,
        &[("a.txt", "two\n"), ("dir/b.txt", "b\n")],
        vec![root],
    );
    let right = commit_of(
        repository,
        &[
            ("a.txt", "one\n"),
            ("dir/b.txt", "b\n"),
            ("dir/c.txt", "c\n"),
        ],
        vec![root],
    );
    let merge = commit_of(
        repository,
        &[
            ("a.txt", "two\n"),
            ("dir/b.txt", "b\n"),
            ("dir/c.txt", "c\n"),
        ],
        vec![left, right],
    );
    // The file returns to the content of the root.
    let top = commit_of(
        repository,
        &[
            ("a.txt", "one\n"),
            ("d.txt", "d\n"),
            ("dir/b.txt", "b\n"),
            ("dir/c.txt", "c\n"),
        ],
        vec![merge],
    );
    History {
        root,
        left,
        right,
        merge,
        top,
    }
}

#[test]
fn a_clone_gets_everything_below_the_wanted_commit() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    set_reference(&hub, "heads/main", &history.top);

    let mut stream = Vec::new();
    let sent = hub
        .answer_fetch(&request(&[history.top], &[]), &mut stream)
        .unwrap();

    // Ancestors first: the parents of the merge in the order in which it names them.
    assert_eq!(
        commits(&stream),
        [
            history.root,
            history.left,
            history.right,
            history.merge,
            history.top
        ]
    );
    let sent_ids = ids(&stream);
    assert_eq!(sent.objects, sent_ids.len() as u64);
    assert_eq!(sent.commits, 5);
    assert_eq!(
        sent.bytes,
        objects(&stream)
            .iter()
            .map(|object| object.body.len() as u64)
            .sum::<u64>()
    );
    // Every object stands once, also the blob to which `a.txt` returns.
    let mut unique = sent_ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), sent_ids.len());
    // The receiver finds the history complete.
    let clone = bare(&dir, "clone");
    clone
        .receive_objects(stream.as_slice(), Bounds::default())
        .unwrap();
    clone.check_connectivity(&[history.top]).unwrap();
}

#[test]
fn the_blobs_and_trees_of_a_commit_come_before_it_and_a_tree_after_what_it_names() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let only = commit_of(
        &hub,
        &[("a.txt", "one\n"), ("dir/b.txt", "b\n")],
        Vec::new(),
    );
    set_reference(&hub, "heads/main", &only);

    let stream = answer(&hub, &request(&[only], &[]));

    let sent = objects(&stream);
    let kinds: Vec<Kind> = sent.iter().map(|object| object.kind).collect();
    assert_eq!(kinds.last(), Some(&Kind::Commit));
    assert_eq!(sent.len(), 5);
    let position = |id: ObjectId| sent.iter().position(|object| object.id() == id).unwrap();
    let root = Commit::parse(&sent[4].body).unwrap().tree();
    let root_tree = Tree::parse(&sent[position(root.object_id())].body).unwrap();
    for entry in root_tree.entries() {
        assert!(position(entry.id()) < position(root.object_id()));
        if entry.mode() == EntryMode::Directory {
            assert!(position(blob_id("b\n")) < position(entry.id()));
        }
    }
}

#[test]
fn a_fetch_leaves_out_what_a_known_have_reaches_and_ignores_an_unknown_have() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    set_reference(&hub, "heads/main", &history.top);
    // A commit that the store holds unnamed counts as little as one that it lacks, although
    // it lies above the wanted commit.
    let apart = commit_of(&hub, &[("z.txt", "apart\n")], vec![history.top]);

    let stream = answer(
        &hub,
        &request(&[history.top], &[history.left, absent(0x77), apart]),
    );

    // The root lies below the known `have`, although the walk also reaches it around it.
    assert_eq!(
        commits(&stream),
        [history.right, history.merge, history.top]
    );
    let sent = ids(&stream);
    // `right` adds `c.txt` and the trees above it.
    assert!(sent.contains(&blob_id("c\n")));
    // What the `have` holds at the same path is not sent: `two` came with `left`.
    assert!(!sent.contains(&blob_id("two\n")));
    assert!(!sent.contains(&blob_id("b\n")));
    // `top` returns to a content that no tree of its parent holds at the path, so it is sent
    // again: too much, never too little.
    assert!(sent.contains(&blob_id("one\n")));

    // A receiver that has the `have` finds the wanted commit complete.
    let clone = bare(&dir, "clone");
    let first = answer(&hub, &request(&[history.left], &[]));
    clone
        .receive_objects(first.as_slice(), Bounds::default())
        .unwrap();
    set_reference(&clone, "heads/main", &history.left);
    clone
        .receive_objects(stream.as_slice(), Bounds::default())
        .unwrap();
    clone.check_connectivity(&[history.top]).unwrap();
}

#[test]
fn a_request_whose_wants_are_all_had_is_answered_with_an_empty_stream() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    set_reference(&hub, "heads/main", &history.top);

    // The other side has a commit above the wanted one, which a reference reaches.
    for have in [vec![history.top], vec![history.merge, history.top]] {
        let mut stream = Vec::new();
        let sent = hub
            .answer_fetch(&request(&[history.merge], &have), &mut stream)
            .unwrap();
        assert_eq!(sent, Sent::default());
        assert_eq!(stream, b"stemma-protocol 1\n");
    }
    // The same commit wanted and had is had; the other one is sent.
    let stream = answer(
        &hub,
        &request(&[history.merge, history.top], &[history.merge]),
    );
    assert_eq!(commits(&stream), [history.top]);
}

#[test]
fn two_wanted_commits_share_one_stream_and_each_object_stands_once() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    set_reference(&hub, "heads/left", &history.left);
    set_reference(&hub, "heads/right", &history.right);

    let stream = answer(&hub, &request(&[history.left, history.right], &[]));

    let mut sent = commits(&stream);
    assert_eq!(sent[0], history.root);
    sent.sort();
    let mut expected = vec![history.root, history.left, history.right];
    expected.sort();
    assert_eq!(sent, expected);
    let all = ids(&stream);
    assert_eq!(
        all.iter().filter(|&&id| id == blob_id("b\n")).count(),
        1,
        "{all:?}"
    );
}

#[test]
fn a_want_that_no_reference_reaches_is_refused_before_anything_is_written() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    set_reference(&hub, "heads/main", &history.merge);
    // The store holds `top`, as after an upload whose ref update was refused.
    let unnamed = history.top;
    let missing = absent(0x00);

    for (want, refused) in [
        (vec![unnamed], unnamed),
        (vec![missing], missing),
        // The first one in the order of the IDs is named, and a reachable one beside it does
        // not help.
        (vec![unnamed, missing, history.merge], missing.min(unnamed)),
        // Also one that comes after a commit that a reference reaches.
        (vec![history.merge, absent(0xff)], absent(0xff)),
    ] {
        let mut stream = Vec::new();
        let error = hub
            .answer_fetch(&request(&want, &[]), &mut stream)
            .unwrap_err();
        assert!(
            matches!(error, AnswerError::UnknownWant(commit) if commit == refused),
            "{error:?}"
        );
        assert!(stream.is_empty());
        assert_eq!(error.answer(), Some(ErrorMessage::UnknownWant(refused)));
        assert!(error.to_string().contains(&refused.to_string()));
        assert!(error.store_error().is_none() && error.io_error().is_none());
    }
    // A commit below a reference is served, and so is one that a tag alone reaches.
    assert_eq!(
        commits(&answer(&hub, &request(&[history.root], &[]))),
        [history.root]
    );
    let tagged = commit_of(&hub, &[("t.txt", "tag\n")], vec![unnamed]);
    set_reference(&hub, "tags/v1", &tagged);
    assert_eq!(
        commits(&answer(&hub, &request(&[unnamed], &[history.merge]))),
        [unnamed]
    );
}

#[test]
fn a_remote_reference_of_a_clone_names_nothing_that_is_served() {
    let dir = TempDir::new();
    let clone = with_working_copy(&dir, "clone");
    let fetched = commit_of(&clone, &[("a.txt", "one\n")], Vec::new());
    write_remote_ref(&clone.metadata_dir(), "origin/heads/main", fetched);

    let error = clone
        .answer_fetch(&request(&[fetched], &[]), io::sink())
        .unwrap_err();

    assert!(
        matches!(error, AnswerError::UnknownWant(commit) if commit == fetched),
        "{error:?}"
    );
    // The list names the same references as the fetch serves.
    let list = clone.answer_refs(&facts()).unwrap();
    assert!(list.references().is_empty());
}

#[test]
fn every_beginning_of_the_stream_that_ends_with_a_commit_is_a_complete_state() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    let (conflicted, _) = conflicted_commit(&hub);
    let top = commit_of(
        &hub,
        &[("a.txt", "three\n"), ("dir/b.txt", "b\n")],
        vec![history.top, conflicted],
    );
    set_reference(&hub, "heads/main", &top);

    // A clone: the receiver has nothing.
    let stream = answer(&hub, &request(&[top], &[]));
    let all = prefixes(&stream);
    assert_eq!(all.len(), 7);
    assert_eq!(all.last().unwrap().0, stream);
    for (index, (prefix, commit)) in all.iter().enumerate() {
        let receiver = bare(&dir, &format!("clone-{index}"));
        receiver
            .receive_objects(prefix.as_slice(), Bounds::default())
            .unwrap();
        receiver.check_connectivity(&[*commit]).unwrap();
        // The next commit is not there yet, so the check is not empty.
        if let Some((_, next)) = all.get(index + 1) {
            let error = receiver.check_connectivity(&[*next]).unwrap_err();
            assert!(
                matches!(error, ConnectivityError::Missing { .. }),
                "{error:?}"
            );
        }
    }

    // A fetch: the receiver has the `have` and what lies below it.
    let have = answer(&hub, &request(&[history.left], &[]));
    let stream = answer(&hub, &request(&[top], &[history.left]));
    let all = prefixes(&stream);
    assert_eq!(all.len(), 5);
    for (index, (prefix, commit)) in all.iter().enumerate() {
        let receiver = bare(&dir, &format!("fetch-{index}"));
        receiver
            .receive_objects(have.as_slice(), Bounds::default())
            .unwrap();
        set_reference(&receiver, "heads/main", &history.left);
        receiver
            .receive_objects(prefix.as_slice(), Bounds::default())
            .unwrap();
        receiver.check_connectivity(&[*commit]).unwrap();
    }
}

#[test]
fn a_fetch_that_was_cut_off_resumes_with_the_commits_that_arrived() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    set_reference(&hub, "heads/main", &history.top);
    let stream = answer(&hub, &request(&[history.top], &[]));
    let (arrived, last) = prefixes(&stream).swap_remove(2);
    assert_eq!(last, history.right);

    // The second request names what arrived: the last commit, and the one that it does not
    // reach.
    let rest = answer(
        &hub,
        &request(&[history.top], &[history.left, history.right]),
    );

    assert_eq!(commits(&rest), [history.merge, history.top]);
    // The trees of both parents of the merge count: it adds its root tree alone, and `top`
    // adds its root tree, `d.txt`, and the content to which `a.txt` returns.
    assert_eq!(ids(&rest).len(), 6);
    let clone = bare(&dir, "clone");
    for part in [&arrived, &rest] {
        clone
            .receive_objects(part.as_slice(), Bounds::default())
            .unwrap();
    }
    clone.check_connectivity(&[history.top]).unwrap();
}

#[test]
fn the_operations_work_with_and_without_a_working_copy_and_take_no_lock() {
    let dir = TempDir::new();
    for repository in [bare(&dir, "bare"), with_working_copy(&dir, "full")] {
        let history = history(&repository);
        set_reference(&repository, "heads/main", &history.top);
        set_reference(&repository, "heads/topic", &history.left);
        set_reference(&repository, "tags/v1", &history.root);
        // A writer holds the lock of the references while the answers are made.
        let operation = OperationName::try_new("push").unwrap();
        let allowed = Allowed::ask(&AllowAll, &daniel(), &operation, "main").unwrap();
        let _lock = repository.refs_store().lock_writer(&allowed).unwrap();

        let list = repository.answer_refs(&facts()).unwrap();
        let listed: Vec<(String, CommitId)> = list
            .references()
            .iter()
            .map(|reference| (reference.name().to_string(), reference.commit()))
            .collect();
        assert_eq!(
            listed,
            [
                ("heads/main".to_owned(), history.top),
                ("heads/topic".to_owned(), history.left),
                ("tags/v1".to_owned(), history.root),
            ]
        );
        let stream = answer(&repository, &request(&[history.top], &[history.left]));
        assert_eq!(
            commits(&stream),
            [history.right, history.merge, history.top]
        );
        // Nothing is left behind: no file in `tmp/`, and no journal.
        let tmp = repository.metadata_dir().join("tmp");
        assert_eq!(fs::read_dir(tmp).unwrap().count(), 0);
        assert!(!repository.metadata_dir().join("fetch-pending").exists());
    }
}

#[test]
fn the_list_names_the_facts_of_the_caller_and_refuses_a_reference_without_an_entry() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let commit = commit_of(&hub, &[("a.txt", "one\n")], Vec::new());
    let key = || PublicKey::from_openssh(KEY).unwrap();
    let former = |first, last| FormerKey::try_new(first, last, key()).unwrap();

    // A repository without a reference has a list without one.
    let list = hub.answer_refs(&facts()).unwrap();
    assert!(list.references().is_empty());
    assert_eq!(list.repository(), repository_id());
    assert_eq!(list.key(), &key());

    set_reference(&hub, "heads/main", &commit);
    set_reference(&hub, "tags/v1", &commit);
    let entries = |name: &QualifiedName| match name.kind() {
        RefKind::Branch => Some(record(0xb1)),
        RefKind::Tag => Some(record(0xb2)),
    };
    let named = ListFacts::new(repository_id(), key(), vec![former(1, 4)], entries);
    let list = hub.answer_refs(&named).unwrap();
    assert_eq!(list.former(), [former(1, 4)]);
    let listed: Vec<RecordHash> = list.references().iter().map(Reference::entry).collect();
    assert_eq!(listed, [record(0xb1), record(0xb2)]);
    assert_eq!(RefList::parse(&list.encode()).unwrap(), list);
    assert!(format!("{named:?}").contains("ListFacts"));

    // The record of a repository gives the ID and the entries; a name that it does not list
    // cannot stand in the list.
    let chain = Chain::new(repository_id());
    let error = hub
        .answer_refs(&ListFacts::of_chain(&chain, key(), Vec::new()))
        .unwrap_err();
    assert!(
        matches!(&error, AnswerError::NoRecordEntry(name) if name.to_string() == "heads/main"),
        "{error:?}"
    );
    assert!(error.to_string().contains("heads/main"));
    assert!(error.answer().is_none());

    let overlapping = ListFacts::new(
        repository_id(),
        key(),
        vec![former(1, 4), former(4, 9)],
        entries,
    );
    let error = hub.answer_refs(&overlapping).unwrap_err();
    assert!(
        matches!(error, AnswerError::List(RefListError::OverlappingRanges(4))),
        "{error:?}"
    );
}

#[test]
fn an_object_that_the_store_lacks_fails_the_answer_and_names_what_needs_it() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    set_reference(&hub, "heads/main", &history.top);
    let fetch = request(&[history.top], &[history.merge]);

    // A blob of the answer is gone, as after a purge that ran beside the fetch.
    assert!(hub.objects().remove(&blob_id("one\n")).unwrap());
    let error = hub.answer_fetch(&fetch, io::sink()).unwrap_err();
    let root_of = |commit: CommitId| {
        let commit = hub.objects().get_any_commit(&commit).unwrap().unwrap();
        commit.tree_sum().terms()[0]
    };
    assert!(
        matches!(
            error,
            AnswerError::Missing { id, expected: Kind::Blob, needed_by }
                if id == blob_id("one\n") && needed_by == root_of(history.top).object_id()
        ),
        "{error:?}"
    );
    assert!(error.answer().is_none());
    assert!(error.to_string().contains("is missing"));

    // A tree of the answer is gone.
    let tree = root_of(history.top);
    assert!(hub.objects().remove(&tree.object_id()).unwrap());
    let error = hub.answer_fetch(&fetch, io::sink()).unwrap_err();
    assert!(
        matches!(
            error,
            AnswerError::Missing { id, expected: Kind::Tree, needed_by }
                if id == tree.object_id() && needed_by == history.top.object_id()
        ),
        "{error:?}"
    );

    // A commit below the wanted one is gone: the walk fails before the stream begins.
    assert!(hub.objects().remove(&history.right.object_id()).unwrap());
    let mut stream = Vec::new();
    let error = hub
        .answer_fetch(&request(&[history.top], &[]), &mut stream)
        .unwrap_err();
    assert!(
        matches!(
            &error,
            AnswerError::History(source)
                if matches!(**source, HistoryError::Missing { id, .. } if id == history.right)
        ),
        "{error:?}"
    );
    assert!(stream.is_empty());
    assert!(error.store_error().is_none());
}

/// A writer that takes a number of bytes and then fails.
struct Full(usize);

impl Write for Full {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0 < bytes.len() {
            return Err(io::Error::other("the writer is full"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_writer_that_fails_is_reported_as_such() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    set_reference(&hub, "heads/main", &history.top);

    // The first line fails, and an object after it.
    for room in [0, 30] {
        let error = hub
            .answer_fetch(&request(&[history.top], &[]), Full(room))
            .unwrap_err();
        assert!(matches!(error, AnswerError::Io(_)), "{error:?}");
        assert!(error.io_error().is_some());
        assert!(error.to_string().contains("the writer is full"));
        assert!(error.answer().is_none());
    }
}

/// The message that was asked for. Panics for an error in its place.
fn message(answer: Answer<'_>) -> Vec<u8> {
    let Answer::Message(mut body) = answer else {
        panic!("expected the message, found {answer:?}");
    };
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).unwrap();
    bytes
}

#[test]
fn a_clone_fetches_from_a_repository_through_the_transport_in_memory() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    set_reference(&hub, "heads/main", &history.top);
    let clone = with_working_copy(&dir, "clone");
    let mut transport = MemoryTransport::new(Answering::new(&hub, facts()));

    let list = RefList::parse(&message(transport.refs().unwrap())).unwrap();
    let want: BTreeSet<CommitId> = list.references().iter().map(Reference::commit).collect();
    assert_eq!(want, BTreeSet::from([history.top]));
    let mut fetch = clone.begin_fetch().unwrap();
    let sent = FetchRequest::try_new(want.iter().copied(), fetch.have().iter().copied()).unwrap();
    let Answer::Message(body) = transport.fetch(&mut sent.encode().as_slice()).unwrap() else {
        panic!("the fetch was answered with an error");
    };
    let received = fetch.receive(body, Bounds::default()).unwrap();
    fetch.check(&want).unwrap();
    fetch.finish().unwrap();

    assert_eq!(received.commits, 5);
    clone.check_connectivity(&[history.top]).unwrap();
}

#[test]
fn a_refusal_travels_as_its_error_message_and_a_failure_as_no_answer() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    set_reference(&hub, "heads/main", &history.merge);
    let mut transport = MemoryTransport::new(Answering::new(&hub, facts()));

    let unnamed = request(&[history.top], &[]);
    let Answer::Error(mut body) = transport.fetch(&mut unnamed.encode().as_slice()).unwrap() else {
        panic!("the fetch was answered with a stream");
    };
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).unwrap();
    drop(body);
    assert_eq!(
        ErrorMessage::parse(&bytes).unwrap(),
        ErrorMessage::UnknownWant(history.top)
    );

    // A store that lacks an object is a failure of the side and no message of the protocol.
    assert!(hub.objects().remove(&history.root.object_id()).unwrap());
    let whole = request(&[history.merge], &[]);
    let error = transport.fetch(&mut whole.encode().as_slice()).unwrap_err();
    let TransportError::NoAnswer { detail } = &error else {
        panic!("{error:?}");
    };
    assert!(detail.contains(&history.root.to_string()), "{detail}");

    // So is a list that cannot be made.
    let no_entry = ListFacts::new(
        repository_id(),
        PublicKey::from_openssh(KEY).unwrap(),
        Vec::new(),
        |_| None,
    );
    let mut transport = MemoryTransport::new(Answering::new(&hub, no_entry));
    let error = transport.refs().unwrap_err();
    assert!(
        matches!(error, TransportError::NoAnswer { .. }),
        "{error:?}"
    );
}

/// Stores a commit with this tree, which the store need not hold as a tree.
fn commit_with_tree(repository: &Repository, tree: TreeId, parents: Vec<CommitId>) -> CommitId {
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

#[test]
fn a_commit_stands_after_its_parents_where_a_merge_names_an_ancestor_of_its_other_parent() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let root = commit_of(&hub, &[("a.txt", "0\n")], Vec::new());
    let a = commit_of(&hub, &[("a.txt", "1\n")], vec![root]);
    let b = commit_of(&hub, &[("a.txt", "2\n")], vec![a]);
    // The root is a parent of the merge and lies two commits below its other parent.
    let near_first = commit_of(&hub, &[("a.txt", "3\n")], vec![root, b]);
    let far_first = commit_of(&hub, &[("a.txt", "4\n")], vec![b, root]);
    set_reference(&hub, "heads/near", &near_first);
    set_reference(&hub, "heads/far", &far_first);

    for (index, merge) in [near_first, far_first].into_iter().enumerate() {
        let stream = answer(&hub, &request(&[merge], &[]));
        assert_eq!(commits(&stream), [root, a, b, merge]);
        for (prefix, commit) in prefixes(&stream) {
            let receiver = bare(&dir, &format!("clone-{index}-{commit}"));
            receiver
                .receive_objects(prefix.as_slice(), Bounds::default())
                .unwrap();
            receiver.check_connectivity(&[commit]).unwrap();
        }
        let stream = answer(&hub, &request(&[merge], &[a]));
        assert_eq!(commits(&stream), [b, merge]);
    }
}

#[test]
fn a_path_that_changes_between_a_file_and_a_directory_is_sent_whole() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let file = commit_of(&hub, &[("x", "file\n")], Vec::new());
    let directory = commit_of(&hub, &[("x/y.txt", "below\n")], vec![file]);
    let back = commit_of(&hub, &[("x", "again\n")], vec![directory]);
    set_reference(&hub, "heads/main", &back);
    let have = answer(&hub, &request(&[file], &[]));

    let stream = answer(&hub, &request(&[back], &[file]));

    assert_eq!(commits(&stream), [directory, back]);
    assert!(ids(&stream).contains(&blob_id("below\n")));
    assert!(ids(&stream).contains(&blob_id("again\n")));
    for (index, (prefix, commit)) in prefixes(&stream).into_iter().enumerate() {
        let receiver = bare(&dir, &format!("fetch-{index}"));
        receiver
            .receive_objects(have.as_slice(), Bounds::default())
            .unwrap();
        set_reference(&receiver, "heads/main", &file);
        receiver
            .receive_objects(prefix.as_slice(), Bounds::default())
            .unwrap();
        receiver.check_connectivity(&[commit]).unwrap();
    }
}

#[test]
fn a_tree_of_a_parent_that_is_gone_prunes_nothing_and_fails_nothing() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    set_reference(&hub, "heads/main", &history.top);
    let fetch = request(&[history.top], &[history.merge]);
    let pruned = ids(&answer(&hub, &fetch));
    assert!(!pruned.contains(&blob_id("b\n")) && !pruned.contains(&blob_id("c\n")));
    let merged = hub.objects().get_any_commit(&history.merge).unwrap();
    let base = merged.unwrap().tree_sum().terms()[0];

    assert!(hub.objects().remove(&base.object_id()).unwrap());
    let stream = answer(&hub, &fetch);

    // Without the tree of the parent the whole tree of the commit is sent.
    assert_eq!(commits(&stream), [history.top]);
    let sent = ids(&stream);
    assert!(sent.len() > pruned.len());
    assert!(sent.contains(&blob_id("b\n")) && sent.contains(&blob_id("c\n")));
}

#[test]
fn a_reference_that_cannot_be_read_fails_both_answers() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let commit = commit_of(&hub, &[("a.txt", "one\n")], Vec::new());
    set_reference(&hub, "tags/v1", &commit);
    fs::write(hub.metadata_dir().join("refs/heads/main"), "no commit\n").unwrap();

    let listed = hub.answer_refs(&facts()).unwrap_err();
    let mut stream = Vec::new();
    let fetched = hub
        .answer_fetch(&request(&[commit], &[]), &mut stream)
        .unwrap_err();

    for error in [&listed, &fetched] {
        assert!(
            matches!(error, AnswerError::Refs(source) if matches!(**source, RefError::Corrupt { .. })),
            "{error:?}"
        );
        assert!(error.answer().is_none());
    }
    assert!(stream.is_empty());
    let mut transport = MemoryTransport::new(Answering::new(&hub, facts()));
    let error = transport.refs().unwrap_err();
    assert!(
        matches!(error, TransportError::NoAnswer { .. }),
        "{error:?}"
    );
}

#[test]
fn a_damaged_object_fails_the_answer_as_an_error_of_the_store_or_of_the_reader() {
    let dir = TempDir::new();
    let hub = bare(&dir, "hub");
    let history = history(&hub);
    set_reference(&hub, "heads/main", &history.top);

    // The file of a blob of the answer is damaged: the stream is cut off before its commit.
    let path = hub.objects().object_path(&blob_id("d\n"));
    damage(&path, b"no object");
    let mut stream = Vec::new();
    let error = hub
        .answer_fetch(&request(&[history.top], &[history.merge]), &mut stream)
        .unwrap_err();
    assert!(
        matches!(&error, AnswerError::Store(source) if matches!(**source, StoreError::Corrupt { .. })),
        "{error:?}"
    );
    assert!(error.store_error().is_some() && error.answer().is_none());
    assert!(stream.starts_with(b"stemma-protocol 1\n"));

    // A directory entry names a blob: the store returns no tree for it.
    let blob = hub.objects().put_blob(&Blob::new("a blob\n")).unwrap();
    let entry = TreeEntry::directory("x", TreeId::new(blob.object_id())).unwrap();
    let tree = Tree::try_new(vec![entry]).unwrap();
    let tree = hub.objects().put_tree(&tree).unwrap();
    let wrong_kind = commit_with_tree(&hub, tree, Vec::new());
    set_reference(&hub, "heads/wrong-kind", &wrong_kind);
    let error = hub
        .answer_fetch(&request(&[wrong_kind], &[]), io::sink())
        .unwrap_err();
    assert!(
        matches!(
            &error,
            AnswerError::Store(source) if matches!(**source, StoreError::KindMismatch { .. })
        ),
        "{error:?}"
    );

    // A tree whose body the strict reader refuses is intact as an object.
    let body = hub.objects().put_raw(Kind::Tree, b"no tree").unwrap();
    let unreadable = commit_with_tree(&hub, TreeId::new(body), Vec::new());
    set_reference(&hub, "heads/unreadable", &unreadable);
    let error = hub
        .answer_fetch(&request(&[unreadable], &[]), io::sink())
        .unwrap_err();
    assert!(
        matches!(
            &error,
            AnswerError::Unreadable { needed_by, source }
                if *needed_by == unreadable.object_id()
                    && matches!(**source, ReadError::Tree { .. })
        ),
        "{error:?}"
    );
    assert!(error.store_error().is_none() && error.answer().is_none());
    assert!(error.to_string().contains(&unreadable.to_string()));
}

#[test]
fn a_commit_that_is_gone_below_a_reference_or_a_have_fails_before_the_stream_begins() {
    let dir = TempDir::new();
    // The walk below the references meets the gap while it looks for the wanted commit.
    let hub = bare(&dir, "below-reference");
    let first = history(&hub);
    set_reference(&hub, "heads/main", &first.top);
    assert!(hub.objects().remove(&first.right.object_id()).unwrap());
    let mut stream = Vec::new();
    let error = hub
        .answer_fetch(&request(&[first.root], &[]), &mut stream)
        .unwrap_err();
    assert!(
        matches!(
            &error,
            AnswerError::History(source)
                if matches!(**source, HistoryError::Missing { id, .. } if id == first.right)
        ),
        "{error:?}"
    );
    assert!(stream.is_empty());

    // The walk below a known `have` meets it.
    let hub = bare(&dir, "below-have");
    let second = history(&hub);
    set_reference(&hub, "heads/main", &second.top);
    assert!(hub.objects().remove(&second.root.object_id()).unwrap());
    let error = hub
        .answer_fetch(&request(&[second.top], &[second.left]), &mut stream)
        .unwrap_err();
    assert!(
        matches!(
            &error,
            AnswerError::History(source) if matches!(
                **source,
                HistoryError::Missing { id, child } if id == second.root && child == Some(second.left)
            )
        ),
        "{error:?}"
    );
    assert!(stream.is_empty());
}
