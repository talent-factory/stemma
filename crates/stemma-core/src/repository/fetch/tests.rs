//! Tests of the fetch and the clone (STEMMA-295), through the transport in memory and the
//! answering side of a bare repository ([`Answering`]).

use std::cell::RefCell;
use std::fs;
use std::io::Write;

use super::*;
use crate::change::ChangeId;
use crate::index::Index;
use crate::object::{Blob, Commit, FileMode};
use crate::oplog::{EntryMalformed, EntryParseError};
use crate::protocol::record::HubKeys;
use crate::protocol::RecordHash;
use crate::receive::FETCH_JOURNAL;
use crate::refs::{encode_branch, ConflictKind, RefKind};
use crate::remote::{RemoteAddress, RemoteConfig};
use crate::serve::{Answering, ListFacts};
use crate::signature::PublicKey;
use crate::store::ObjectStore;
use crate::testutil::TempDir;
use crate::transport::{MemoryTransport, OtherSide, Refusal};
use crate::AllowAll;
use crate::Decision;

/// An Ed25519 public key, made for these tests; nobody holds its private half.
const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIjwWEZ4603dDnTO8ilPzaYzYG5PnbKb6TxOzauTQ9du";

fn daniel() -> Actor {
    Actor::human("daniel").unwrap()
}

fn origin() -> RemoteName {
    RemoteName::try_new("origin").unwrap()
}

fn address() -> RemoteAddress {
    RemoteAddress::try_new("https://hub.example/talent-factory/stemma").unwrap()
}

fn repository_id(byte: u8) -> RepositoryId {
    format!("{byte:02x}").repeat(16).parse().unwrap()
}

/// The hash of a record entry whose bytes are all `byte`.
fn record(byte: u8) -> RecordHash {
    format!("b3:{}", format!("{byte:02x}").repeat(32))
        .parse()
        .unwrap()
}

/// The keys of a hub without a former key, leaked so that the facts can borrow them.
fn keys() -> &'static HubKeys {
    let key = PublicKey::from_openssh(KEY).unwrap();
    Box::leak(Box::new(
        HubKeys::try_new(key, crate::protocol::FormerKeys::none()).unwrap(),
    ))
}

/// A hub: a bare repository, and the record entry that its list names for every reference.
struct Hub {
    repository: Repository,
    id: RepositoryId,
    entry: RecordHash,
}

impl Hub {
    fn new(dir: &TempDir) -> Self {
        let repository =
            Repository::init_bare(dir.path().join("hub"), &daniel(), &AllowAll).unwrap();
        Self {
            repository,
            id: repository_id(0x11),
            entry: record(0xa1),
        }
    }

    /// A transport to the hub as it is now.
    fn transport(&self) -> MemoryTransport<Answering<'_>> {
        let entry = self.entry;
        let facts = ListFacts::new(self.id, keys(), move |_| Some(entry));
        MemoryTransport::new(Answering::new(&self.repository, facts))
    }

    /// Stores a commit with one file and these parents.
    fn commit(&self, text: &str, parents: Vec<CommitId>) -> CommitId {
        let objects = self.repository.objects();
        let mut index = Index::new();
        let blob = objects.put_blob(&Blob::new(text)).unwrap();
        index.insert("file.txt", FileMode::File, blob).unwrap();
        let commit = Commit::try_new(
            index.write_tree(objects).unwrap(),
            parents,
            ChangeId::generate().unwrap(),
            daniel(),
            1,
            text,
        )
        .unwrap();
        objects.put_commit(&commit).unwrap()
    }

    /// Sets `heads/<name>` or `tags/<name>` by hand, as the tests of the answering side do.
    fn set(&self, name: &str, commit: CommitId) {
        let path = self.repository.metadata_dir().join("refs").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, encode_branch(&commit)).unwrap();
    }

    fn remove(&self, name: &str) {
        fs::remove_file(self.repository.metadata_dir().join("refs").join(name)).unwrap();
    }

    /// A line of commits, each the child of the one before, the last one first in the result.
    fn line(&self, count: usize) -> Vec<CommitId> {
        let mut commits: Vec<CommitId> = Vec::new();
        for n in 0..count {
            let parents = commits.last().copied().into_iter().collect();
            commits.push(self.commit(&format!("commit {n}\n{}", "x".repeat(2000)), parents));
        }
        commits.reverse();
        commits
    }
}

fn clone(dir: &TempDir, hub: &Hub) -> (Repository, Fetched) {
    Repository::clone_remote(
        dir.path().join("clone"),
        &InitOptions::default(),
        &origin(),
        &RemoteSetup::new(address()),
        &mut hub.transport(),
        &daniel(),
        &AllowAll,
    )
    .unwrap()
}

fn remote_name(kind: RefKind, name: &str) -> RemoteRefName {
    RemoteRefName {
        remote: origin(),
        kind,
        name: name.parse().unwrap(),
    }
}

fn branch(name: &str) -> RemoteRefName {
    remote_name(RefKind::Branch, name)
}

fn value(commit: CommitId, entry: RecordHash) -> RemoteRef {
    RemoteRef { commit, entry }
}

/// The remote references of the clone, as name and commit.
fn remote_refs(repository: &Repository) -> Vec<(String, CommitId)> {
    repository
        .refs()
        .remote_refs()
        .unwrap()
        .into_iter()
        .map(|(name, reference)| (name.to_string(), reference.commit))
        .collect()
}

fn journal_exists(repository: &Repository) -> bool {
    repository.metadata_dir().join(FETCH_JOURNAL).exists()
}

#[test]
fn a_clone_holds_every_reference_of_the_hub_as_a_remote_reference() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(3);
    let feature = hub.commit("feature", vec![main[1]]);
    hub.set("heads/main", main[0]);
    hub.set("heads/feat/x", feature);
    hub.set("tags/v1", main[2]);

    let (repository, fetched) = clone(&dir, &hub);

    assert_eq!(
        remote_refs(&repository),
        vec![
            ("remotes/origin/heads/feat/x".to_owned(), feature),
            ("remotes/origin/heads/main".to_owned(), main[0]),
            ("remotes/origin/tags/v1".to_owned(), main[2]),
        ]
    );
    assert_eq!(
        repository.refs().remote_ref(&branch("main")).unwrap(),
        Some(value(main[0], hub.entry))
    );
    // The configuration holds the address and the ID of the list, and the key of the hub that
    // the list names is pinned (ADR-026, STEMMA-305).
    let config = repository.remote(&origin()).unwrap().unwrap();
    assert_eq!(config.address, address());
    assert_eq!(config.repository, hub.id);
    assert_eq!(config.hub_key, Some(PublicKey::from_openssh(KEY).unwrap()));
    // One entry `fetch` after the entry of `init`, with every reference as created.
    let entry = fetched.entry.unwrap();
    let change = entry.change().unwrap();
    assert_eq!(change.operation().as_str(), FETCH);
    assert_eq!(change.remotes().len(), 3);
    assert!(change
        .remotes()
        .iter()
        .all(|(_, moved)| moved.before().is_none()));
    assert_eq!(repository.oplog().entries().unwrap().len(), 2);
    // Four commits, each with a tree and a blob.
    assert_eq!(fetched.received.commits, 4);
    assert_eq!(fetched.received.objects, 12);
    // No local branch follows a remote reference.
    assert!(repository.refs().branches().unwrap().is_empty());
    assert!(!journal_exists(&repository));
    repository.check_connectivity(&main).unwrap();
    assert_eq!(
        fs::read_to_string(repository.metadata_dir().join("format")).unwrap(),
        "stemma-repository 20\n"
    );
}

#[test]
fn a_fetch_brings_what_is_new_and_moves_the_remote_reference() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(2);
    hub.set("heads/main", main[0]);
    let (repository, _) = clone(&dir, &hub);

    let next = hub.commit("next", vec![main[0]]);
    hub.set("heads/main", next);
    let fetched = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();

    // Only the new commit, its tree, and its blob: the clone sent `main` as `have`.
    assert_eq!(fetched.received.objects, 3);
    let entry = fetched.entry.unwrap();
    let [(name, moved)] = entry.change().unwrap().remotes() else {
        panic!("one remote reference moves: {entry:?}");
    };
    assert_eq!(*name, branch("main"));
    assert_eq!(moved.before(), Some(&value(main[0], hub.entry)));
    assert_eq!(moved.after(), Some(&value(next, hub.entry)));
    assert_eq!(
        remote_refs(&repository),
        vec![("remotes/origin/heads/main".to_owned(), next)]
    );
}

#[test]
fn a_fetch_without_news_sends_no_request_and_writes_no_entry() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    hub.set("heads/main", hub.line(1)[0]);
    let (repository, _) = clone(&dir, &hub);
    let entries = repository.oplog().entries().unwrap().len();

    let fetched = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();

    assert!(fetched.entry.is_none());
    assert_eq!(fetched.received, Received::default());
    assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    assert!(!journal_exists(&repository));
}

#[test]
fn a_new_record_entry_alone_moves_the_remote_reference() {
    let dir = TempDir::new();
    let mut hub = Hub::new(&dir);
    let main = hub.line(1)[0];
    hub.set("heads/main", main);
    let (repository, _) = clone(&dir, &hub);

    // The hub moved `main` away and back: the same commit under a later record entry.
    hub.entry = record(0xb2);
    let fetched = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();

    assert_eq!(fetched.received, Received::default());
    let entry = fetched.entry.unwrap();
    let [(_, moved)] = entry.change().unwrap().remotes() else {
        panic!("{entry:?}");
    };
    assert_eq!(moved.before(), Some(&value(main, record(0xa1))));
    assert_eq!(moved.after(), Some(&value(main, record(0xb2))));
}

#[test]
fn a_reference_that_the_list_no_longer_names_is_removed_and_a_restore_brings_it_back() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(2);
    let feature = hub.commit("feature", vec![main[0]]);
    hub.set("heads/main", main[0]);
    hub.set("heads/feat/x", feature);
    let (repository, _) = clone(&dir, &hub);

    hub.remove("heads/feat/x");
    let fetched = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();
    let entry = fetched.entry.unwrap();
    let [(name, moved)] = entry.change().unwrap().remotes() else {
        panic!("{entry:?}");
    };
    assert_eq!(*name, branch("feat/x"));
    assert_eq!(moved.before(), Some(&value(feature, hub.entry)));
    assert_eq!(moved.after(), None);
    assert_eq!(
        remote_refs(&repository),
        vec![("remotes/origin/heads/main".to_owned(), main[0])]
    );
    // The directory `feat` went with its last reference.
    let heads = repository.metadata_dir().join("refs/remotes/origin/heads");
    assert!(!heads.join("feat").exists());

    let plan = repository
        .plan_restore(entry.number(), false, &daniel(), &AllowAll)
        .unwrap()
        .unwrap();
    assert_eq!(plan.remotes().len(), 1);
    assert!(plan.branches().is_empty() && plan.checkout().is_none());
    let restored = repository
        .restore(entry.number(), false, &daniel(), &AllowAll)
        .unwrap();
    let restore = restored.entry().unwrap().change().unwrap();
    assert_eq!(restore.operation().as_str(), "restore");
    assert_eq!(
        restore.remotes()[0].1.after(),
        Some(&value(feature, hub.entry))
    );
    assert_eq!(
        repository.refs().remote_ref(&branch("feat/x")).unwrap(),
        Some(value(feature, hub.entry))
    );
    // The next fetch removes it again, as the list is whole.
    let again = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();
    assert_eq!(again.entry.unwrap().change().unwrap().remotes().len(), 1);
    assert_eq!(
        repository.refs().remote_ref(&branch("feat/x")).unwrap(),
        None
    );
}

#[test]
fn a_reference_gives_way_to_one_below_its_name() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(1)[0];
    hub.set("heads/a", main);
    let (repository, _) = clone(&dir, &hub);

    hub.remove("heads/a");
    hub.set("heads/a/b", main);
    let fetched = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();

    assert_eq!(fetched.entry.unwrap().change().unwrap().remotes().len(), 2);
    assert_eq!(
        remote_refs(&repository),
        vec![("remotes/origin/heads/a/b".to_owned(), main)]
    );
}

#[test]
fn the_entry_of_a_fetch_reads_back_as_it_was_written() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(2);
    hub.set("heads/main", main[0]);
    hub.set("tags/v1", main[1]);
    let (repository, fetched) = clone(&dir, &hub);
    let entry = fetched.entry.unwrap();
    let bytes = entry.encode();
    let text = String::from_utf8(bytes.clone()).unwrap();
    let line = format!(
        "remote remotes/origin/heads/main - - {} {}\n",
        main[0], hub.entry
    );
    assert!(text.contains(&line), "{text}");
    assert_eq!(Entry::parse(&bytes).unwrap(), entry);
    assert_eq!(repository.oplog().read(entry.number()).unwrap(), entry);

    // A commit without its record entry, and the lines out of order, are refused.
    let half = text.replace(
        &line,
        &format!("remote remotes/origin/heads/main - - {} -\n", main[0]),
    );
    assert!(matches!(
        Entry::parse(half.as_bytes()),
        Err(EntryParseError::Malformed {
            reason: EntryMalformed::HalfRemoteValue,
            ..
        })
    ));
    let tag_line = text
        .lines()
        .find(|line| line.starts_with("remote remotes/origin/tags/"))
        .unwrap()
        .to_owned();
    let swapped = text
        .replace(&line, "")
        .replace(&format!("{tag_line}\n"), &format!("{tag_line}\n{line}"));
    assert!(matches!(
        Entry::parse(swapped.as_bytes()),
        Err(EntryParseError::OutOfOrder { .. })
    ));
}

/// A side that answers the list of the hub and a fetch with what the hub answers, but cuts the
/// stream after `cut` bytes while `cut` is set, and records every request.
struct Cutting<'a> {
    hub: Answering<'a>,
    cut: RefCell<Option<usize>>,
    requests: RefCell<Vec<FetchRequest>>,
}

impl OtherSide for Cutting<'_> {
    fn refs(&self) -> Result<RefList, Refusal> {
        self.hub.refs()
    }

    fn fetch(&self, request: &FetchRequest, stream: &mut dyn Write) -> Result<(), Refusal> {
        self.requests.borrow_mut().push(request.clone());
        let mut whole = Vec::new();
        self.hub.fetch(request, &mut whole)?;
        let end = self
            .cut
            .borrow()
            .map_or(whole.len(), |cut| cut.min(whole.len()));
        stream.write_all(&whole[..end]).unwrap();
        Ok(())
    }
}

#[test]
fn an_interrupted_fetch_moves_nothing_and_the_next_one_resumes() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let first = hub.line(1)[0];
    hub.set("heads/main", first);
    let (repository, _) = clone(&dir, &hub);
    // Six new commits of about 2 KiB each on top.
    let mut tip = first;
    for n in 0..6 {
        tip = hub.commit(&format!("on top {n}\n{}", "y".repeat(2000)), vec![tip]);
    }
    hub.set("heads/main", tip);
    let entry = hub.entry;
    let side = Cutting {
        hub: Answering::new(
            &hub.repository,
            ListFacts::new(hub.id, keys(), move |_| Some(entry)),
        ),
        cut: RefCell::new(Some(9000)),
        requests: RefCell::new(Vec::new()),
    };
    let mut transport = MemoryTransport::new(side);
    let entries = repository.oplog().entries().unwrap().len();

    let error = repository
        .fetch(&origin(), &mut transport, &daniel(), &AllowAll)
        .unwrap_err();
    assert!(matches!(error, FetchError::Receive(_)), "{error}");
    assert!(error.entry().is_none());
    assert_eq!(
        remote_refs(&repository),
        vec![("remotes/origin/heads/main".to_owned(), first)]
    );
    assert_eq!(repository.oplog().entries().unwrap().len(), entries);
    assert!(journal_exists(&repository));

    *transport.side().cut.borrow_mut() = None;
    let fetched = repository
        .fetch(&origin(), &mut transport, &daniel(), &AllowAll)
        .unwrap();
    assert!(!fetched.resumed.is_empty());
    let requests = transport.side().requests.borrow();
    let [_, second] = requests.as_slice() else {
        panic!("{requests:?}");
    };
    // The second request has a commit that the first stream brought, and its stream is shorter.
    assert!(second.have().len() > 1, "{second:?}");
    assert!(fetched.received.commits < 6, "{:?}", fetched.received);
    assert_eq!(
        remote_refs(&repository),
        vec![("remotes/origin/heads/main".to_owned(), tip)]
    );
    assert!(!journal_exists(&repository));
}

/// A side that lists the hub but streams only what the parent of each wanted commit needs.
struct Short<'a>(Answering<'a>, CommitId);

impl OtherSide for Short<'_> {
    fn refs(&self) -> Result<RefList, Refusal> {
        self.0.refs()
    }

    fn fetch(&self, request: &FetchRequest, stream: &mut dyn Write) -> Result<(), Refusal> {
        let short = FetchRequest::try_new([self.1], request.have().iter().copied()).unwrap();
        self.0.fetch(&short, stream)
    }
}

#[test]
fn no_remote_reference_moves_before_the_connectivity_check_has_passed() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(3);
    hub.set("heads/main", main[0]);
    let entry = hub.entry;
    let facts = ListFacts::new(hub.id, keys(), move |_| Some(entry));
    let mut transport =
        MemoryTransport::new(Short(Answering::new(&hub.repository, facts), main[1]));

    let error = Repository::clone_remote(
        dir.path().join("clone"),
        &InitOptions::default(),
        &origin(),
        &RemoteSetup::new(address()),
        &mut transport,
        &daniel(),
        &AllowAll,
    )
    .unwrap_err();
    assert!(error.entry().is_none());
    let CloneError::After {
        path,
        configured: true,
        source,
    } = error
    else {
        panic!("{error}");
    };
    assert!(matches!(*source, FetchError::Connectivity(_)), "{source}");
    let repository = Repository::open(path).unwrap();
    assert!(repository.refs().remote_refs().unwrap().is_empty());
    assert_eq!(repository.oplog().entries().unwrap().len(), 1);
    // The commits that arrived stay for the next fetch, which brings the rest.
    assert!(journal_exists(&repository));
    let fetched = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();
    assert_eq!(fetched.received.commits, 1);
    assert_eq!(remote_refs(&repository)[0].1, main[0]);
}

#[test]
fn another_repository_under_the_address_is_reported_and_nothing_moves() {
    let dir = TempDir::new();
    let mut hub = Hub::new(&dir);
    let main = hub.line(1)[0];
    hub.set("heads/main", main);
    let (repository, _) = clone(&dir, &hub);
    hub.set("heads/main", hub.commit("other", vec![]));

    hub.id = repository_id(0x22);
    let error = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap_err();

    let FetchError::OtherRepository {
        remote,
        expected,
        found,
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(
        (remote, *expected, *found),
        (&origin(), repository_id(0x11), hub.id)
    );
    assert!(error.to_string().contains("nothing was fetched"), "{error}");
    assert_eq!(remote_refs(&repository)[0].1, main);
    assert_eq!(repository.oplog().entries().unwrap().len(), 2);
    assert!(!journal_exists(&repository));
}

#[test]
fn a_fetch_of_an_unknown_remote_and_a_refused_fetch_change_nothing() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    hub.set("heads/main", hub.line(1)[0]);
    let (repository, _) = clone(&dir, &hub);
    let other = RemoteName::try_new("upstream").unwrap();

    let error = repository
        .fetch(&other, &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap_err();
    assert!(
        matches!(error, FetchError::NoSuchRemote(ref name) if *name == other),
        "{error}"
    );

    struct Deny;
    impl Authorizer for Deny {
        fn authorize(&self, _: &Actor, operation: &OperationName, target: &str) -> Decision {
            assert_eq!((operation.as_str(), target), (FETCH, "remote origin"));
            Decision::Deny
        }
    }
    let error = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &Deny)
        .unwrap_err();
    assert!(
        matches!(error, FetchError::Update(UpdateError::Denied { .. })),
        "{error}"
    );
    assert_eq!(repository.oplog().entries().unwrap().len(), 2);
}

#[test]
fn a_bare_repository_does_not_fetch() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let error = hub
        .repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap_err();
    assert!(
        matches!(error, FetchError::Refs(RefError::Bare { .. })),
        "{error}"
    );
}

#[test]
fn a_clone_of_an_empty_repository_has_no_remote_reference_and_no_entry() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let (repository, fetched) = clone(&dir, &hub);
    assert!(fetched.entry.is_none());
    assert!(repository.refs().remote_refs().unwrap().is_empty());
    assert_eq!(
        repository.remote(&origin()).unwrap().unwrap().repository,
        hub.id
    );
}

#[test]
fn a_clone_whose_list_fails_makes_nothing() {
    struct Failing;
    impl OtherSide for Failing {
        fn refs(&self) -> Result<RefList, Refusal> {
            Err(Refusal::Failed("down".to_owned()))
        }
        fn fetch(&self, _: &FetchRequest, _: &mut dyn Write) -> Result<(), Refusal> {
            unreachable!()
        }
    }
    let dir = TempDir::new();
    let error = Repository::clone_remote(
        dir.path().join("clone"),
        &InitOptions::default(),
        &origin(),
        &RemoteSetup::new(address()),
        &mut MemoryTransport::new(Failing),
        &daniel(),
        &AllowAll,
    )
    .unwrap_err();
    assert!(
        matches!(error, CloneError::Before(FetchError::Transport(_))),
        "{error}"
    );
    assert!(!dir.path().join("clone").exists());
}

#[test]
fn a_fetch_leaves_the_references_of_another_remote_alone() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(1)[0];
    hub.set("heads/main", main);
    hub.set("heads/old", main);
    let (repository, _) = clone(&dir, &hub);
    // A second remote that lists the same hub under another name.
    let upstream = RemoteName::try_new("upstream").unwrap();
    remote::write_config(
        &repository.metadata_dir(),
        &repository.metadata_dir().join("tmp"),
        &upstream,
        &{
            let mut config = RemoteConfig::new(address(), hub.id);
            config.hub_key = Some(PublicKey::from_openssh(KEY).unwrap());
            config
        },
    )
    .unwrap();
    repository
        .fetch(&upstream, &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();

    hub.remove("heads/old");
    let fetched = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();

    let entry = fetched.entry.unwrap();
    let [(name, _)] = entry.change().unwrap().remotes() else {
        panic!("{entry:?}");
    };
    assert_eq!(*name, branch("old"));
    assert_eq!(
        remote_refs(&repository),
        vec![
            ("remotes/origin/heads/main".to_owned(), main),
            ("remotes/upstream/heads/main".to_owned(), main),
            ("remotes/upstream/heads/old".to_owned(), main),
        ]
    );
}

#[test]
fn a_reference_below_a_name_gives_way_to_the_name() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(1)[0];
    hub.set("heads/a/b", main);
    hub.set("heads/a/c", main);
    let (repository, _) = clone(&dir, &hub);

    hub.remove("heads/a/b");
    hub.remove("heads/a/c");
    fs::remove_dir(hub.repository.metadata_dir().join("refs/heads/a")).unwrap();
    hub.set("heads/a", main);
    let fetched = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();

    assert_eq!(fetched.entry.unwrap().change().unwrap().remotes().len(), 3);
    assert_eq!(
        remote_refs(&repository),
        vec![("remotes/origin/heads/a".to_owned(), main)]
    );
}

#[test]
fn a_branch_renamed_in_case_alone_moves_its_remote_reference() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(1)[0];
    hub.set("heads/main", main);
    let (repository, _) = clone(&dir, &hub);

    hub.remove("heads/main");
    hub.set("heads/Main", main);
    repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();

    assert_eq!(
        remote_refs(&repository),
        vec![("remotes/origin/heads/Main".to_owned(), main)]
    );
}

#[test]
fn names_of_one_state_that_collide_are_refused_before_anything_is_written() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(1)[0];
    hub.set("heads/main", main);
    let (repository, _) = clone(&dir, &hub);
    let target = |name: &str| (branch(name), Some(value(main, hub.entry)));
    for (names, kind) in [
        (["a", "a/b"], ConflictKind::Reference),
        (["x/y", "x/y/z"], ConflictKind::Reference),
        (["Main", "main"], ConflictKind::CaseVariant),
        (["Feature/x", "feature/y"], ConflictKind::CaseVariant),
        (["A", "a/b"], ConflictKind::Reference),
    ] {
        let targets: Vec<_> = names.iter().map(|name| target(name)).collect();
        let error = repository
            .refs_store()
            .plan_remote_moves(&targets, repository.objects())
            .unwrap_err();
        assert!(
            matches!(&error, RefError::RemoteConflict { kind: found, .. } if *found == kind),
            "{names:?}: {error}"
        );
        assert!(
            error.to_string().contains("refs/remotes/origin/heads/"),
            "{error}"
        );
    }
    // Names that only share a beginning are no conflict.
    let targets = vec![target("a"), target("a-b"), target("ab/c")];
    repository
        .refs_store()
        .plan_remote_moves(&targets, repository.objects())
        .unwrap();
}

#[test]
fn a_restore_of_the_entry_of_a_clone_removes_what_it_created() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(1)[0];
    hub.set("heads/main", main);
    hub.set("tags/v1", main);
    let (repository, fetched) = clone(&dir, &hub);

    repository
        .restore(fetched.entry.unwrap().number(), false, &daniel(), &AllowAll)
        .unwrap();

    assert!(repository.refs().remote_refs().unwrap().is_empty());
    // The next fetch brings them back. No reference names the commits any more, so the clone
    // has no `have`, and the hub sends them again.
    repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();
    assert_eq!(remote_refs(&repository).len(), 2);
}

/// A side that lists the hub and refuses every fetch request.
struct Refusing<'a>(Answering<'a>);

impl OtherSide for Refusing<'_> {
    fn refs(&self) -> Result<RefList, Refusal> {
        self.0.refs()
    }

    fn fetch(&self, request: &FetchRequest, _: &mut dyn Write) -> Result<(), Refusal> {
        let want = *request.want().iter().next().unwrap();
        Err(Refusal::Error(ErrorMessage::UnknownWant(want)))
    }
}

#[test]
fn a_refused_fetch_request_moves_nothing() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let first = hub.line(1)[0];
    hub.set("heads/main", first);
    let (repository, _) = clone(&dir, &hub);
    let next = hub.commit("next", vec![first]);
    hub.set("heads/main", next);
    let entry = hub.entry;
    let facts = ListFacts::new(hub.id, keys(), move |_| Some(entry));
    let mut transport = MemoryTransport::new(Refusing(Answering::new(&hub.repository, facts)));
    let entries = repository.oplog().entries().unwrap().len();

    let error = repository
        .fetch(&origin(), &mut transport, &daniel(), &AllowAll)
        .unwrap_err();

    assert!(
        matches!(error, FetchError::Refused(ErrorMessage::UnknownWant(want)) if want == next),
        "{error}"
    );
    assert_eq!(remote_refs(&repository)[0].1, first);
    assert_eq!(repository.oplog().entries().unwrap().len(), entries);
}

#[test]
fn a_clone_that_the_authorizer_refuses_makes_nothing() {
    struct Deny;
    impl Authorizer for Deny {
        fn authorize(&self, _: &Actor, _: &OperationName, _: &str) -> Decision {
            Decision::Deny
        }
    }
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let error = Repository::clone_remote(
        dir.path().join("clone"),
        &InitOptions::default(),
        &origin(),
        &RemoteSetup::new(address()),
        &mut hub.transport(),
        &daniel(),
        &Deny,
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            CloneError::Before(FetchError::Update(UpdateError::Denied { .. }))
        ),
        "{error}"
    );
    assert!(!dir.path().join("clone").exists());
}

#[test]
fn a_line_of_a_remote_reference_is_read_strictly() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(1)[0];
    hub.set("heads/main", main);
    let (_, fetched) = clone(&dir, &hub);
    let text = String::from_utf8(fetched.entry.unwrap().encode()).unwrap();
    let line = format!(
        "remote remotes/origin/heads/main - - {main} {}\n",
        hub.entry
    );
    let with = |replacement: &str| text.replace(&line, replacement);

    let cases = [
        format!("remote remotes/origin/main - - {main} {}\n", hub.entry),
        format!("remote remotes/origin/heads/main - - b3:zz {}\n", hub.entry),
        format!("remote remotes/origin/heads/main - - {main} b3:zz\n"),
        "remote remotes/origin/heads/main - - - -\n".to_owned(),
        format!("remote remotes/origin/heads/main - - {main}\n"),
        format!("{line}branch main - {main}\n"),
        format!("{line}tag v1 - {main}\n"),
    ];
    for case in &cases {
        let error = Entry::parse(with(case).as_bytes()).unwrap_err();
        let expected = matches!(
            error,
            EntryParseError::InvalidRemoteName { .. }
                | EntryParseError::InvalidObjectId { .. }
                | EntryParseError::InvalidHash { .. }
                | EntryParseError::Unchanged { .. }
                | EntryParseError::Malformed {
                    reason: EntryMalformed::WrongValueCount | EntryMalformed::PointerOutOfOrder,
                    ..
                }
        );
        assert!(expected, "{case:?}: {error:?}");
    }
    // A change that lists a reference twice, or one that does not change, is refused.
    let moved = Moved::from_to(None, Some(value(main, hub.entry)));
    let operation = OperationName::try_new(FETCH).unwrap();
    assert_eq!(
        Change::try_new_with_remotes(
            1,
            daniel(),
            operation.clone(),
            vec![(branch("main"), moved.clone()), (branch("main"), moved)],
        ),
        Err(crate::oplog::EntryError::DuplicateRemote(branch("main")))
    );
    let same = Moved::from_to(Some(value(main, hub.entry)), Some(value(main, hub.entry)));
    assert!(
        Change::try_new_with_remotes(1, daniel(), operation, vec![(branch("main"), same)]).is_err()
    );
}

#[test]
fn the_commits_of_a_fetch_entry_are_roots_and_its_outcome_is_judged() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(2);
    hub.set("heads/main", main[1]);
    let (repository, first) = clone(&dir, &hub);
    hub.set("heads/main", main[0]);
    let second = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap()
        .entry
        .unwrap();
    assert_eq!(second.change().unwrap().commits(), vec![main[1], main[0]]);

    let entries = repository.oplog().entries().unwrap();
    let current = crate::oplog::Current::new(repository.refs().head().unwrap(), [], true)
        .with_remotes(repository.refs().remote_refs().unwrap());
    let outcomes = crate::oplog::outcomes(&entries, Some(&current));
    let position = |number| entries.iter().position(|e| e.number() == number).unwrap();
    use crate::oplog::Outcome;
    // The clone is confirmed by the fetch after it, the fetch by the reference now.
    assert_eq!(
        outcomes[position(first.entry.unwrap().number())],
        Outcome::Took
    );
    assert_eq!(outcomes[position(second.number())], Outcome::Took);
    // Without the remote references, the newest one is unknown.
    let blind = crate::oplog::Current::new(repository.refs().head().unwrap(), [], true);
    assert_eq!(
        crate::oplog::outcomes(&entries, Some(&blind))[position(second.number())],
        Outcome::Unknown
    );
}

#[test]
fn a_directory_renamed_in_case_alone_moves_the_references_below_it() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(1)[0];
    hub.set("heads/Feature/x", main);
    hub.set("heads/Release/1", main);
    let (repository, _) = clone(&dir, &hub);

    let heads = hub.repository.metadata_dir().join("refs/heads");
    hub.remove("heads/Feature/x");
    hub.remove("heads/Release/1");
    fs::remove_dir(heads.join("Feature")).unwrap();
    fs::remove_dir(heads.join("Release")).unwrap();
    hub.set("heads/feature/x", main);
    hub.set("heads/release/2", main);
    let fetched = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();

    assert_eq!(fetched.entry.unwrap().change().unwrap().remotes().len(), 4);
    assert_eq!(
        remote_refs(&repository),
        vec![
            ("remotes/origin/heads/feature/x".to_owned(), main),
            ("remotes/origin/heads/release/2".to_owned(), main),
        ]
    );
}

#[test]
fn a_case_variant_of_a_directory_that_stays_is_in_the_way() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let main = hub.line(1)[0];
    hub.set("heads/Feature/x", main);
    let (repository, _) = clone(&dir, &hub);
    // `Feature/x` stays, so `feature/y` cannot be written beside it.
    let targets = vec![
        (branch("Feature/x"), Some(value(main, hub.entry))),
        (branch("other"), Some(value(main, hub.entry))),
    ];
    repository
        .refs_store()
        .plan_remote_moves(&targets, repository.objects())
        .unwrap();
    let targets = vec![
        (branch("Feature/x"), None),
        (branch("feature/y"), Some(value(main, hub.entry))),
    ];
    // Here every reference below `Feature` goes, so the variant is cleared.
    repository
        .refs_store()
        .plan_remote_moves(&targets, repository.objects())
        .unwrap();
    let targets = vec![(branch("feature/y"), Some(value(main, hub.entry)))];
    let error = repository
        .refs_store()
        .plan_remote_moves(&targets, repository.objects())
        .unwrap_err();
    assert!(
        matches!(
            &error,
            RefError::RemoteConflict {
                kind: ConflictKind::CaseVariant,
                ..
            }
        ),
        "{error}"
    );
}
