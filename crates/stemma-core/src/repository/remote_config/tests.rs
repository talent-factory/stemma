//! Tests of adding and removing a remote, and of the key of the hub in a fetch and a clone
//! (STEMMA-305), through the transport in memory and the answering side of a bare repository.

use std::fs;

use super::*;
use crate::change::ChangeId;
use crate::index::Index;
use crate::object::{Blob, Commit, CommitId, FileMode};
use crate::protocol::record::HubKeys;
use crate::protocol::RepositoryId;
use crate::refs::encode_branch;
use crate::repository::{CloneError, InitOptions, RestoreError};
use crate::serve::{Answering, ListFacts};
use crate::signature::PublicKey;
use crate::store::ObjectStore;
use crate::testutil::TempDir;
use crate::transport::MemoryTransport;
use crate::AllowAll;

/// Two Ed25519 public keys, made for tests; nobody here holds their private halves.
const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIjwWEZ4603dDnTO8ilPzaYzYG5PnbKb6TxOzauTQ9du";
const OTHER_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMt6BisKCOOMdkWowXYqXUrS1zKF4aE6QenzZ1SSB/Md";

fn daniel() -> Actor {
    Actor::human("daniel").unwrap()
}

fn origin() -> RemoteName {
    RemoteName::try_new("origin").unwrap()
}

fn setup() -> RemoteSetup {
    RemoteSetup::new(RemoteAddress::try_new("https://hub.example/talent-factory/stemma").unwrap())
}

fn key(text: &str) -> PublicKey {
    PublicKey::from_openssh(text).unwrap()
}

/// The keys of a hub without a former key, leaked so that the facts can borrow them.
fn keys(text: &str) -> &'static HubKeys {
    Box::leak(Box::new(
        HubKeys::try_new(key(text), crate::protocol::FormerKeys::none()).unwrap(),
    ))
}

/// A hub: a bare repository with one commit on `main`, which names the key `key`.
struct Hub {
    repository: Repository,
    id: RepositoryId,
    key: &'static str,
}

impl Hub {
    fn new(dir: &TempDir) -> Self {
        let repository =
            Repository::init_bare(dir.path().join("hub"), &daniel(), &AllowAll).unwrap();
        let objects = repository.objects();
        let mut index = Index::new();
        let blob = objects.put_blob(&Blob::new("hello\n")).unwrap();
        index.insert("file.txt", FileMode::File, blob).unwrap();
        let commit = Commit::try_new(
            index.write_tree(objects).unwrap(),
            Vec::new(),
            ChangeId::generate().unwrap(),
            daniel(),
            1,
            "first",
        )
        .unwrap();
        let commit: CommitId = objects.put_commit(&commit).unwrap();
        let path = repository.metadata_dir().join("refs/heads/main");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, encode_branch(&commit)).unwrap();
        Self {
            repository,
            id: "11".repeat(16).parse().unwrap(),
            key: KEY,
        }
    }

    fn transport(&self) -> MemoryTransport<Answering<'_>> {
        let entry = format!("b3:{}", "a1".repeat(32)).parse().unwrap();
        let facts = ListFacts::new(self.id, keys(self.key), move |_| Some(entry));
        MemoryTransport::new(Answering::new(&self.repository, facts))
    }
}

fn local(dir: &TempDir) -> Repository {
    Repository::init(
        dir.path().join("local"),
        &InitOptions::default(),
        &daniel(),
        &AllowAll,
    )
    .unwrap()
}

#[test]
fn a_remote_is_added_with_the_key_of_the_hub_pinned_and_one_entry() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let repository = local(&dir);
    let added = repository
        .add_remote(
            &origin(),
            &setup(),
            &mut hub.transport(),
            &daniel(),
            &AllowAll,
        )
        .unwrap();
    assert_eq!(added.fingerprint, key(KEY).fingerprint());
    let config = repository.remote(&origin()).unwrap().unwrap();
    assert_eq!(config, added.config);
    assert_eq!(config.repository, hub.id);
    assert_eq!(config.hub_key, Some(key(KEY)));
    // The entry holds the configuration, and no remote reference moved.
    let read = repository.oplog().read(added.entry.number()).unwrap();
    let record = read.remote_config().unwrap();
    assert_eq!(record.kind(), RemoteConfigKind::Add);
    assert_eq!(record.config(), &config);
    assert!(repository.refs_store().remote_refs().unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(repository.metadata_dir().join("format")).unwrap(),
        "stemma-repository 20\n"
    );
    // `op restore` does not undo it.
    assert!(matches!(
        repository.restore(added.entry.number(), false, &daniel(), &AllowAll),
        Err(RestoreError::IsRemoteConfig { .. })
    ));
    // A second remote of the name is refused, and nothing is written.
    let entries = repository.oplog().entries().unwrap().len();
    assert!(matches!(
        repository.add_remote(
            &origin(),
            &setup(),
            &mut hub.transport(),
            &daniel(),
            &AllowAll
        ),
        Err(RemoteConfigError::Exists(_))
    ));
    assert_eq!(repository.oplog().entries().unwrap().len(), entries);
}

#[test]
fn a_remote_whose_hub_names_another_key_than_the_expected_one_is_not_added() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let repository = local(&dir);
    let mut expecting = setup();
    expecting.expected = Some(key(OTHER_KEY).fingerprint());
    let error = repository
        .add_remote(
            &origin(),
            &expecting,
            &mut hub.transport(),
            &daniel(),
            &AllowAll,
        )
        .unwrap_err();
    assert!(
        matches!(
            error,
            RemoteConfigError::HubKey(HubKeyError::Unexpected { .. })
        ),
        "{error}"
    );
    assert_eq!(repository.remote(&origin()).unwrap(), None);
    assert_eq!(repository.oplog().entries().unwrap().len(), 1);
    // The expected fingerprint of the key of the hub is taken.
    expecting.expected = Some(key(KEY).fingerprint());
    repository
        .add_remote(
            &origin(),
            &expecting,
            &mut hub.transport(),
            &daniel(),
            &AllowAll,
        )
        .unwrap();
}

#[test]
fn a_removal_deletes_the_configuration_and_the_remote_references_and_names_them() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let repository = local(&dir);
    repository
        .add_remote(
            &origin(),
            &setup(),
            &mut hub.transport(),
            &daniel(),
            &AllowAll,
        )
        .unwrap();
    repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();
    let before = repository.refs_store().remote_refs().unwrap();
    assert_eq!(before.len(), 1);
    let removed = repository
        .remove_remote(&origin(), &daniel(), &AllowAll)
        .unwrap();
    assert_eq!(removed.refs, before);
    assert_eq!(repository.remote(&origin()).unwrap(), None);
    assert!(repository.refs_store().remote_refs().unwrap().is_empty());
    let read = repository.oplog().read(removed.entry.number()).unwrap();
    let record = read.remote_config().unwrap();
    assert_eq!(record.kind(), RemoteConfigKind::Remove);
    assert_eq!(record.refs(), &before[..]);
    // The log still verifies, and a remote that is gone cannot be removed again.
    repository.oplog().verify().unwrap();
    assert!(matches!(
        repository.remove_remote(&origin(), &daniel(), &AllowAll),
        Err(RemoteConfigError::NoSuchRemote(_))
    ));
    // Added again, the remote fetches its references again.
    repository
        .add_remote(
            &origin(),
            &setup(),
            &mut hub.transport(),
            &daniel(),
            &AllowAll,
        )
        .unwrap();
    repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap();
    assert_eq!(repository.refs_store().remote_refs().unwrap(), before);
}

#[test]
fn a_removal_without_remote_references_writes_its_entry() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let repository = local(&dir);
    repository
        .add_remote(
            &origin(),
            &setup(),
            &mut hub.transport(),
            &daniel(),
            &AllowAll,
        )
        .unwrap();
    let removed = repository
        .remove_remote(&origin(), &daniel(), &AllowAll)
        .unwrap();
    assert!(removed.refs.is_empty());
    assert_eq!(repository.remote(&origin()).unwrap(), None);
}

#[test]
fn a_fetch_refuses_a_hub_that_names_another_key_than_the_pinned_one() {
    let dir = TempDir::new();
    let mut hub = Hub::new(&dir);
    let repository = local(&dir);
    repository
        .add_remote(
            &origin(),
            &setup(),
            &mut hub.transport(),
            &daniel(),
            &AllowAll,
        )
        .unwrap();
    hub.key = OTHER_KEY;
    let entries = repository.oplog().entries().unwrap().len();
    let error = repository
        .fetch(&origin(), &mut hub.transport(), &daniel(), &AllowAll)
        .unwrap_err();
    assert!(
        matches!(error, FetchError::HubKey(HubKeyError::Changed { .. })),
        "{error}"
    );
    assert!(error.to_string().contains("pin the key again"), "{error}");
    assert!(repository.refs_store().remote_refs().unwrap().is_empty());
    assert_eq!(repository.oplog().entries().unwrap().len(), entries);
}

#[test]
fn a_clone_pins_the_key_and_refuses_an_unexpected_one_before_it_makes_anything() {
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let mut expecting = setup();
    expecting.expected = Some(key(OTHER_KEY).fingerprint());
    let path = dir.path().join("clone");
    let error = Repository::clone_remote(
        &path,
        &InitOptions::default(),
        &origin(),
        &expecting,
        &mut hub.transport(),
        &daniel(),
        &AllowAll,
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            CloneError::Before(FetchError::HubKey(HubKeyError::Unexpected { .. }))
        ),
        "{error}"
    );
    assert!(!path.exists());
    expecting.expected = Some(key(KEY).fingerprint());
    expecting.sign_in = true;
    let (repository, _) = Repository::clone_remote(
        &path,
        &InitOptions::default(),
        &origin(),
        &expecting,
        &mut hub.transport(),
        &daniel(),
        &AllowAll,
    )
    .unwrap();
    let config = repository.remote(&origin()).unwrap().unwrap();
    assert_eq!(config.hub_key, Some(key(KEY)));
    assert!(config.sign_in);
}

#[test]
fn a_refused_operation_writes_nothing() {
    struct Deny;
    impl Authorizer for Deny {
        fn authorize(
            &self,
            _actor: &Actor,
            _operation: &OperationName,
            _target: &str,
        ) -> crate::Decision {
            crate::Decision::Deny
        }
    }
    let dir = TempDir::new();
    let hub = Hub::new(&dir);
    let repository = local(&dir);
    assert!(matches!(
        repository.add_remote(&origin(), &setup(), &mut hub.transport(), &daniel(), &Deny),
        Err(RemoteConfigError::Denied { .. })
    ));
    repository
        .add_remote(
            &origin(),
            &setup(),
            &mut hub.transport(),
            &daniel(),
            &AllowAll,
        )
        .unwrap();
    let error = repository
        .remove_remote(&origin(), &daniel(), &Deny)
        .unwrap_err();
    assert!(error.to_string().contains("remote-remove"), "{error}");
    assert!(repository.remote(&origin()).unwrap().is_some());
}
