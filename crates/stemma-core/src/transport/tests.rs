//! The tests of the transport: the messages of the protocol travel between two repositories
//! through [`MemoryTransport`], a request that the reader refuses is answered with the error of the
//! protocol, and a side that cannot answer is a failure of the transport.
//!
//! The other side of these tests is [`Hub`], which answers from a bare repository with the
//! least that the messages need, so that they test the transport alone. The answering operation
//! of the core ([`Answering`](crate::serve::Answering)) has its own tests.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};

use super::*;
use crate::change::ChangeId;
use crate::index::Index;
use crate::object::{Blob, Commit, CommitId, EntryMode, FileMode, Kind, ObjectId, Tree, TreeId};
use crate::protocol::examples::{self, Message};
use crate::protocol::{
    FormerKeys, ObjectStreamReader, ObjectStreamWriter, ParseError, QualifiedName, RecordHash,
    Reference, RepositoryId, Spoken, StreamObject,
};
use crate::refs::{encode_branch, RefKind};
use crate::repository::{InitOptions, Repository};
use crate::signature::PublicKey;
use crate::store::ObjectStore;
use crate::testutil::TempDir;
use crate::{Actor, AllowAll};

/// An Ed25519 public key, made for these tests; nobody holds its private half.
const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIjwWEZ4603dDnTO8ilPzaYzYG5PnbKb6TxOzauTQ9du";

/// The longest body of an object that the receiving side of these tests accepts.
const MAX_OBJECT: u64 = 1 << 20;

fn daniel() -> Actor {
    Actor::human("daniel").unwrap()
}

fn repository_id() -> RepositoryId {
    "00112233445566778899aabbccddeeff".parse().unwrap()
}

/// A bare repository does not hold a record of ref updates yet (ADR-025), so every reference of
/// these tests carries this value as its record entry.
fn record() -> RecordHash {
    format!("b3:{}", "a1".repeat(32)).parse().unwrap()
}

/// A bare repository in the directory `hub` below `dir`.
fn bare(dir: &TempDir) -> Repository {
    Repository::init_bare(dir.path().join("hub"), &daniel(), &AllowAll).unwrap()
}

/// A repository with a working copy in the directory `clone` below `dir`.
fn clone(dir: &TempDir) -> Repository {
    let path = dir.path().join("clone");
    fs::create_dir(&path).unwrap();
    Repository::init(&path, &InitOptions::default(), &daniel(), &AllowAll).unwrap()
}

/// Stores a commit with these files and parents in the object store of `repository`.
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

/// Writes a branch file by hand: nothing in the crate moves a reference of a bare repository
/// before the accepting operation is built (ADR-025).
fn set_branch(repository: &Repository, branch: &str, commit: &CommitId) {
    let path = repository.metadata_dir().join("refs/heads").join(branch);
    fs::write(path, encode_branch(commit)).unwrap();
}

/// The other side of these tests: a bare repository that answers with its branches and with the
/// objects of its store. It stands in for the answering operation of the core, which refuses
/// what this one answers: a `want` that no reference reaches.
struct Hub<'a> {
    repository: &'a Repository,
}

impl Hub<'_> {
    fn body(&self, id: ObjectId) -> Vec<u8> {
        let object = self.repository.objects().get(&id).unwrap().unwrap();
        object.body().to_vec()
    }

    /// Writes a tree after the trees and blobs below it, each object once.
    fn send_tree(
        &self,
        tree: TreeId,
        sent: &mut BTreeSet<ObjectId>,
        stream: &mut ObjectStreamWriter<&mut dyn Write>,
    ) {
        if !sent.insert(tree.object_id()) {
            return;
        }
        let body = self.body(tree.object_id());
        for entry in Tree::parse(&body).unwrap().entries() {
            if entry.mode() == EntryMode::Directory {
                self.send_tree(TreeId::new(entry.id()), sent, stream);
            } else if sent.insert(entry.id()) {
                stream.write(Kind::Blob, &self.body(entry.id())).unwrap();
            }
        }
        stream.write(Kind::Tree, &body).unwrap();
    }

    /// Writes a commit in the order of the format (ADR-023): its ancestors first, then its
    /// blobs and trees, then the commit. A commit that the client has ends the walk.
    fn send_commit(
        &self,
        commit: CommitId,
        have: &BTreeSet<CommitId>,
        sent: &mut BTreeSet<ObjectId>,
        stream: &mut ObjectStreamWriter<&mut dyn Write>,
    ) {
        if have.contains(&commit) || !sent.insert(commit.object_id()) {
            return;
        }
        let body = self.body(commit.object_id());
        let parsed = Commit::parse(&body).unwrap();
        for parent in parsed.parents() {
            self.send_commit(*parent, have, sent, stream);
        }
        self.send_tree(parsed.tree(), sent, stream);
        stream.write(Kind::Commit, &body).unwrap();
    }
}

impl OtherSide for Hub<'_> {
    fn refs(&self) -> Result<RefList, Refusal> {
        let references = self
            .repository
            .refs()
            .branches()
            .map_err(|error| Refusal::Failed(error.to_string()))?
            .into_iter()
            .map(|(name, commit)| {
                Reference::new(QualifiedName::new(RefKind::Branch, name), commit, record())
            })
            .collect();
        let key = PublicKey::from_openssh(KEY).unwrap();
        Ok(RefList::try_new(repository_id(), key, FormerKeys::none(), references).unwrap())
    }

    fn fetch(&self, request: &FetchRequest, stream: &mut dyn Write) -> Result<(), Refusal> {
        let mut stream = ObjectStreamWriter::new(stream).unwrap();
        let mut sent = BTreeSet::new();
        for commit in request.want() {
            self.send_commit(*commit, request.have(), &mut sent, &mut stream);
        }
        Ok(())
    }
}

/// A side that answers the same refusal to every request and counts how often it is asked.
struct Refusing {
    refusal: fn() -> Refusal,
    asked: Cell<u32>,
}

impl Refusing {
    fn new(refusal: fn() -> Refusal) -> Self {
        Self {
            refusal,
            asked: Cell::new(0),
        }
    }

    fn refuse<T>(&self) -> Result<T, Refusal> {
        self.asked.set(self.asked.get() + 1);
        Err((self.refusal)())
    }
}

impl OtherSide for Refusing {
    fn refs(&self) -> Result<RefList, Refusal> {
        self.refuse()
    }

    fn fetch(&self, _request: &FetchRequest, _stream: &mut dyn Write) -> Result<(), Refusal> {
        self.refuse()
    }
}

/// A side that answers one list of references and the bytes of one stream, whatever is asked.
struct Fixed {
    refs: RefList,
    stream: &'static [u8],
}

impl OtherSide for Fixed {
    fn refs(&self) -> Result<RefList, Refusal> {
        Ok(self.refs.clone())
    }

    fn fetch(&self, _request: &FetchRequest, stream: &mut dyn Write) -> Result<(), Refusal> {
        stream
            .write_all(self.stream)
            .map_err(|error| Refusal::Failed(error.to_string()))
    }
}

/// A reader of a request that fails.
struct Broken;

impl Read for Broken {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("the reader failed"))
    }
}

/// The bytes of the message that was asked for. Panics for an error in its place.
fn message(answer: Answer<'_>) -> Vec<u8> {
    let Answer::Message(mut body) = answer else {
        panic!("expected the message, found {answer:?}");
    };
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).unwrap();
    bytes
}

/// The error that was answered. Panics for the message that was asked for.
fn error(answer: Answer<'_>) -> ErrorMessage {
    let Answer::Error(mut body) = answer else {
        panic!("expected an error, found {answer:?}");
    };
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).unwrap();
    ErrorMessage::parse(&bytes).unwrap()
}

/// The receiving side of a fetch in these tests: sends the request, reads the stream with the
/// reader of the protocol, and stores every object in `clone`. Returns the IDs in the order of
/// the stream.
fn fetch_into(
    clone: &Repository,
    transport: &mut dyn Transport,
    request: &FetchRequest,
) -> Vec<ObjectId> {
    let Answer::Message(body) = transport.fetch(&mut request.encode().as_slice()).unwrap() else {
        panic!("the fetch was answered with an error");
    };
    let mut reader = ObjectStreamReader::new(body, MAX_OBJECT).unwrap();
    let mut received = Vec::new();
    while let Some(object) = reader.next_object().unwrap() {
        let stored = store(clone, &object);
        assert_eq!(stored, object.id());
        received.push(stored);
    }
    received
}

fn store(repository: &Repository, object: &StreamObject) -> ObjectId {
    let objects = repository.objects();
    match object.kind {
        Kind::Blob => objects
            .put_blob(&Blob::new(object.body.clone()))
            .unwrap()
            .object_id(),
        Kind::Tree => objects
            .put_tree(&Tree::parse(&object.body).unwrap())
            .unwrap()
            .object_id(),
        Kind::Commit => objects
            .put_commit(&Commit::parse(&object.body).unwrap())
            .unwrap()
            .object_id(),
        kind => panic!("these tests send no {}", kind.name()),
    }
}

#[test]
fn the_list_of_references_travels_from_a_bare_repository_to_a_clone() {
    let dir = TempDir::new();
    let hub = bare(&dir);
    let first = commit_of(&hub, &[("a.txt", "one\n")], Vec::new());
    let second = commit_of(&hub, &[("a.txt", "two\n")], vec![first]);
    set_branch(&hub, "main", &second);
    set_branch(&hub, "topic", &first);
    let mut transport = MemoryTransport::new(Hub { repository: &hub });

    let bytes = message(transport.refs().unwrap());
    let list = RefList::parse(&bytes).unwrap();

    assert_eq!(list.repository(), repository_id());
    let listed: Vec<(String, CommitId)> = list
        .references()
        .iter()
        .map(|reference| (reference.name().to_string(), reference.commit()))
        .collect();
    assert_eq!(
        listed,
        [
            ("heads/main".to_owned(), second),
            ("heads/topic".to_owned(), first)
        ]
    );
    // What travelled is the one encoding of the message.
    assert_eq!(bytes, list.encode());
}

#[test]
fn a_fetch_carries_the_objects_of_a_bare_repository_into_a_clone() {
    let dir = TempDir::new();
    let hub = bare(&dir);
    let first = commit_of(&hub, &[("a.txt", "one\n")], Vec::new());
    let second = commit_of(&hub, &[("a.txt", "one\n"), ("b.txt", "two\n")], vec![first]);
    set_branch(&hub, "main", &second);
    let clone = clone(&dir);
    let mut transport = MemoryTransport::new(Hub { repository: &hub });

    let list = RefList::parse(&message(transport.refs().unwrap())).unwrap();
    let tip = list.references()[0].commit();
    assert_eq!(tip, second);
    assert!(clone.objects().get(&tip.object_id()).unwrap().is_none());

    let request = FetchRequest::try_new([tip], []).unwrap();
    let received = fetch_into(&clone, &mut transport, &request);

    // Two commits, two trees, and the two blobs: `a.txt` is the same blob in both commits.
    assert_eq!(received.len(), 6);
    assert_eq!(received.last(), Some(&second.object_id()));
    // The ancestor comes before the commit that names it (ADR-023).
    let position = |id: ObjectId| received.iter().position(|found| *found == id).unwrap();
    assert!(position(first.object_id()) < position(second.object_id()));
    // The clone holds what the bare repository holds, under the same IDs.
    for id in &received {
        assert_eq!(
            clone.objects().get(id).unwrap(),
            hub.objects().get(id).unwrap()
        );
    }
}

#[test]
fn a_fetch_with_a_commit_that_the_clone_has_carries_only_what_is_new() {
    let dir = TempDir::new();
    let hub = bare(&dir);
    let first = commit_of(&hub, &[("a.txt", "one\n")], Vec::new());
    let second = commit_of(&hub, &[("a.txt", "one\n"), ("b.txt", "two\n")], vec![first]);
    let clone = clone(&dir);
    let mut transport = MemoryTransport::new(Hub { repository: &hub });
    fetch_into(
        &clone,
        &mut transport,
        &FetchRequest::try_new([first], []).unwrap(),
    );

    let request = FetchRequest::try_new([second], [first]).unwrap();
    let received = fetch_into(&clone, &mut transport, &request);

    assert!(!received.contains(&first.object_id()));
    assert_eq!(received.last(), Some(&second.object_id()));
    assert!(clone.objects().get(&second.object_id()).unwrap().is_some());
}

#[test]
fn the_example_messages_travel_byte_for_byte() {
    let example = |name: &str| {
        examples::ALL
            .iter()
            .find(|example| example.name == name)
            .unwrap()
    };
    for (refs, fetch, objects) in [
        ("refs", "fetch", "objects"),
        ("refs-empty", "clone", "objects-empty"),
    ] {
        let (refs, fetch, objects) = (example(refs), example(fetch), example(objects));
        assert_eq!(
            (refs.message, fetch.message, objects.message),
            (
                Message::RefList,
                Message::FetchRequest,
                Message::ObjectStream
            )
        );
        let mut transport = MemoryTransport::new(Fixed {
            refs: RefList::parse(refs.bytes).unwrap(),
            stream: objects.bytes,
        });

        assert_eq!(message(transport.refs().unwrap()), refs.bytes);
        let mut request = fetch.bytes;
        assert_eq!(
            message(transport.fetch(&mut request).unwrap()),
            objects.bytes
        );
    }
}

#[test]
fn a_request_that_the_reader_refuses_is_answered_with_malformed_and_the_side_is_not_asked() {
    let mut transport = MemoryTransport::new(Refusing::new(|| panic!("the side was asked")));
    // A fetch request wants at least one commit.
    let mut request: &[u8] = b"stemma-protocol 1\n";

    let answered = error(transport.fetch(&mut request).unwrap());

    let refused = FetchRequest::parse(b"stemma-protocol 1\n").unwrap_err();
    assert_eq!(answered, ErrorMessage::from(&refused));
    assert!(matches!(answered, ErrorMessage::Malformed(_)), "{answered}");
    assert_eq!(transport.side().asked.get(), 0);
}

#[test]
fn a_request_of_another_protocol_version_is_answered_with_the_versions_spoken() {
    let mut transport = MemoryTransport::new(Refusing::new(|| panic!("the side was asked")));
    let want = format!("b3:{}", "11".repeat(32));
    let request = format!("stemma-protocol 2\nwant {want}\n");
    assert!(matches!(
        FetchRequest::parse(request.as_bytes()),
        Err(ParseError::UnsupportedVersion { found: 2 })
    ));

    let answered = error(transport.fetch(&mut request.as_bytes()).unwrap());

    assert_eq!(answered, ErrorMessage::UnsupportedVersion(Spoken::here()));
    assert_eq!(transport.side().asked.get(), 0);
}

#[test]
fn an_error_of_the_other_side_travels_in_place_of_the_message() {
    let refusal = || Refusal::from(ErrorMessage::UnsupportedVersion(Spoken::here()));
    let mut transport = MemoryTransport::new(Refusing::new(refusal));
    let request = examples::ALL
        .iter()
        .find(|example| example.message == Message::FetchRequest)
        .unwrap();

    let expected = ErrorMessage::UnsupportedVersion(Spoken::here());
    assert_eq!(error(transport.refs().unwrap()), expected);
    let mut bytes = request.bytes;
    assert_eq!(error(transport.fetch(&mut bytes).unwrap()), expected);
    assert_eq!(transport.side().asked.get(), 2);
}

#[test]
fn a_side_that_fails_before_it_answers_is_a_failure_of_the_transport() {
    let refusal = || Refusal::Failed("the store could not be read".to_owned());
    let mut transport = MemoryTransport::new(Refusing::new(refusal));
    let request = examples::ALL
        .iter()
        .find(|example| example.message == Message::FetchRequest)
        .unwrap();

    let mut bytes = request.bytes;
    for failure in [
        transport.refs().unwrap_err(),
        transport.fetch(&mut bytes).unwrap_err(),
    ] {
        assert!(
            matches!(&failure, TransportError::NoAnswer { detail }
                if detail == "the store could not be read"),
            "{failure}"
        );
        assert!(failure.io_error().is_none());
        assert_eq!(
            failure.to_string(),
            "the other side gave no answer of the protocol: the store could not be read"
        );
    }
}

#[test]
fn a_request_that_cannot_be_read_from_its_reader_is_an_error_of_the_request() {
    let mut transport = MemoryTransport::new(Refusing::new(|| panic!("the side was asked")));

    let failure = transport.fetch(&mut Broken).unwrap_err();

    assert!(matches!(failure, TransportError::Request(_)), "{failure}");
    assert!(failure.io_error().is_some());
    assert_eq!(
        failure.to_string(),
        "the request could not be read: the reader failed"
    );
    let carrier = TransportError::Carrier(io::Error::other("no route"));
    assert!(carrier.io_error().is_some());
    assert_eq!(carrier.to_string(), "the transport failed: no route");
    assert_eq!(transport.side().asked.get(), 0);
}

#[test]
fn the_debug_form_of_an_answer_names_the_case() {
    let mut transport = MemoryTransport::new(Refusing::new(|| {
        Refusal::from(ErrorMessage::UnsupportedVersion(Spoken::here()))
    }));
    assert_eq!(
        format!("{:?}", transport.refs().unwrap()),
        "Answer::Error(..)"
    );
    let empty = examples::ALL
        .iter()
        .find(|example| example.name == "refs-empty")
        .unwrap();
    let mut transport = MemoryTransport::new(Fixed {
        refs: RefList::parse(empty.bytes).unwrap(),
        stream: b"",
    });
    assert_eq!(
        format!("{:?}", transport.refs().unwrap()),
        "Answer::Message(..)"
    );
}

/// A side that writes the beginning of a stream and then refuses.
struct HalfWritten {
    refusal: fn() -> Refusal,
}

impl OtherSide for HalfWritten {
    fn refs(&self) -> Result<RefList, Refusal> {
        Err((self.refusal)())
    }

    fn fetch(&self, _request: &FetchRequest, stream: &mut dyn Write) -> Result<(), Refusal> {
        stream.write_all(b"stemma-protocol 1\nblob 3").unwrap();
        Err((self.refusal)())
    }
}

#[test]
fn what_a_side_wrote_before_it_refused_does_not_travel() {
    let request = examples::ALL
        .iter()
        .find(|example| example.message == Message::FetchRequest)
        .unwrap();
    let expected = ErrorMessage::UnsupportedVersion(Spoken::here());

    let mut transport = MemoryTransport::new(HalfWritten {
        refusal: || Refusal::from(ErrorMessage::UnsupportedVersion(Spoken::here())),
    });
    let mut bytes = request.bytes;
    let Answer::Error(mut body) = transport.fetch(&mut bytes).unwrap() else {
        panic!("expected an error");
    };
    let mut answered = Vec::new();
    body.read_to_end(&mut answered).unwrap();
    assert_eq!(answered, expected.encode());

    let mut transport = MemoryTransport::new(HalfWritten {
        refusal: || Refusal::Failed("the store could not be read".to_owned()),
    });
    let mut bytes = request.bytes;
    let failure = transport.fetch(&mut bytes).unwrap_err();
    assert!(
        matches!(failure, TransportError::NoAnswer { .. }),
        "{failure}"
    );
}

#[test]
fn the_source_of_a_transport_error_is_the_cause_of_its_cause() {
    use std::error::Error as _;

    use crate::testutil::DiskError;

    // The message prints the error of the carrier, so the source is what lies below it.
    for failure in [
        TransportError::Request(io::Error::other(DiskError)),
        TransportError::Carrier(io::Error::other(DiskError)),
    ] {
        let below = DiskError.source().map(ToString::to_string);
        assert_eq!(failure.source().map(ToString::to_string), below);
        assert!(failure.to_string().ends_with(&DiskError.to_string()));
    }
    let failure = TransportError::NoAnswer {
        detail: "a page".to_owned(),
    };
    assert!(failure.source().is_none());
}
