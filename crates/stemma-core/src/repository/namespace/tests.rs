use std::fs;

use super::*;
use crate::change::ChangeId;
use crate::index::Index;
use crate::object::{Blob, Commit, CommitId, FileMode};
use crate::protocol::record::HubKeys;
use crate::protocol::{FormerKeys, RecordHash, RepositoryId};
use crate::refs::encode_branch;
use crate::register::{self, Identity};
use crate::remote::RemoteAddress;
use crate::repository::{InitOptions, RemoteSetup};
use crate::serve::{Answering, ListFacts};
use crate::signature::PublicKey;
use crate::store::ObjectStore;
use crate::testutil::TempDir;
use crate::transport::MemoryTransport;
use crate::{Actor, AllowAll};

/// An Ed25519 public key, made for these tests; nobody holds its private half.
const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIjwWEZ4603dDnTO8ilPzaYzYG5PnbKb6TxOzauTQ9du";

const DEMANDING: &str = "stemma-policy 2\ndemand-namespace-owners\nlevel 3 grading/\n";

fn teacher() -> Actor {
    Actor::human("teacher").unwrap()
}

fn origin() -> RemoteName {
    RemoteName::try_new("origin").unwrap()
}

fn key() -> PublicKey {
    PublicKey::from_openssh(KEY).unwrap()
}

/// A bare repository at `name` in `dir` whose `main` holds `files`.
fn with_main(dir: &TempDir, name: &str, files: &[(&str, &str)]) -> (Repository, CommitId) {
    let repository = Repository::init_bare(dir.path().join(name), &teacher(), &AllowAll).unwrap();
    let objects = repository.objects();
    let mut index = Index::new();
    for (path, text) in files {
        let blob = objects.put_blob(&Blob::new(*text)).unwrap();
        index.insert(*path, FileMode::File, blob).unwrap();
    }
    let commit = Commit::try_new(
        index.write_tree(objects).unwrap(),
        Vec::new(),
        ChangeId::generate().unwrap(),
        teacher(),
        1,
        "The rules of the course",
    )
    .unwrap();
    let commit = objects.put_commit(&commit).unwrap();
    let path = repository.metadata_dir().join("refs/heads/main");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, encode_branch(&commit)).unwrap();
    (repository, commit)
}

fn register_text() -> String {
    Register::with_owner(Identity::of(&teacher()), key())
        .unwrap()
        .encode()
}

#[test]
fn a_hub_reads_the_namespace_from_the_tip_of_main_of_the_namespace_repository() {
    let dir = TempDir::new();
    let register = register_text();
    let (repository, tip) = with_main(
        &dir,
        "namespace",
        &[
            (policy::NAMESPACE_FILE_NAME, DEMANDING),
            (register::FILE, &register),
            // The policy of the namespace repository itself is no namespace policy.
            (policy::FILE_NAME, "stemma-policy 1\nlevel 0 grading/\n"),
        ],
    );
    let namespace = repository.namespace().unwrap();
    assert_eq!(namespace.commit(), tip);
    assert_eq!(namespace.policy().encode(), DEMANDING);
    let [owner] = namespace.owners() else {
        panic!("one owner");
    };
    assert_eq!(owner.identity(), &Identity::of(&teacher()));
    assert_eq!(owner.keys(), [key()]);
}

#[test]
fn a_namespace_repository_without_main_a_policy_or_a_register_sets_no_namespace() {
    let dir = TempDir::new();
    let empty = Repository::init_bare(dir.path().join("empty"), &teacher(), &AllowAll).unwrap();
    assert!(matches!(empty.namespace(), Err(NamespaceError::NoMain)));

    let register = register_text();
    let (without_policy, _) = with_main(&dir, "a", &[(register::FILE, &register)]);
    assert!(matches!(
        without_policy.namespace(),
        Err(NamespaceError::NoPolicy)
    ));

    let (without_register, _) = with_main(&dir, "b", &[(policy::NAMESPACE_FILE_NAME, DEMANDING)]);
    assert!(matches!(
        without_register.namespace(),
        Err(NamespaceError::NoRegister)
    ));

    let (invalid, _) = with_main(
        &dir,
        "c",
        &[
            (
                policy::NAMESPACE_FILE_NAME,
                "stemma-policy 1\ndemand-namespace-owners\n",
            ),
            (register::FILE, &register),
        ],
    );
    let error = invalid.namespace().unwrap_err();
    assert!(
        matches!(error, NamespaceError::Policy(PolicyError::Invalid(_))),
        "{error}"
    );
    assert!(error
        .to_string()
        .contains(".stemma-namespace-policy of the namespace repository"));
}

/// The keys of a hub without a former key, leaked so that the facts can borrow them.
fn keys() -> &'static HubKeys {
    Box::leak(Box::new(
        HubKeys::try_new(key(), FormerKeys::none()).unwrap(),
    ))
}

fn id() -> RepositoryId {
    "11".repeat(16).parse().unwrap()
}

fn entry() -> RecordHash {
    format!("b3:{}", "a1".repeat(32)).parse().unwrap()
}

#[test]
fn a_fetch_keeps_the_namespace_policy_of_the_list_and_removes_it_when_the_list_names_none() {
    let dir = TempDir::new();
    let register = register_text();
    let (course, _) = with_main(
        &dir,
        "namespace",
        &[
            (policy::NAMESPACE_FILE_NAME, DEMANDING),
            (register::FILE, &register),
        ],
    );
    let namespace = course.namespace().unwrap();
    let (hub, _) = with_main(&dir, "hub", &[("a.txt", "a\n")]);

    let facts = ListFacts::new(id(), keys(), |_| Some(entry())).with_namespace(&namespace);
    let mut transport = MemoryTransport::new(Answering::new(&hub, facts));
    let address = RemoteAddress::try_new("https://hub.example/course/work").unwrap();
    let (clone, _) = Repository::clone_remote(
        dir.path().join("clone"),
        &InitOptions::default(),
        &origin(),
        &RemoteSetup::new(address),
        &mut transport,
        &teacher(),
        &AllowAll,
    )
    .unwrap();

    let kept = clone.kept_namespace(&origin()).unwrap().unwrap();
    assert_eq!(kept.commit(), namespace.commit());
    assert_eq!(kept.policy(), namespace.policy());
    assert_eq!(
        clone.kept_namespace_policies().unwrap(),
        [namespace.policy().clone()]
    );
    let format = fs::read_to_string(clone.metadata_dir().join("format")).unwrap();
    assert_eq!(format, format!("stemma-repository {FORMAT_VERSION}\n"));

    // The hub no longer names a namespace policy: the next fetch removes the kept one.
    let facts = ListFacts::new(id(), keys(), |_| Some(entry()));
    let mut transport = MemoryTransport::new(Answering::new(&hub, facts));
    clone
        .fetch(&origin(), &mut transport, &teacher(), &AllowAll)
        .unwrap();
    assert_eq!(clone.kept_namespace(&origin()).unwrap(), None);
    assert!(clone.kept_namespace_policies().unwrap().is_empty());
}

#[test]
fn a_kept_namespace_policy_that_is_not_in_the_form_of_a_fetch_is_damage() {
    let dir = TempDir::new();
    let clone = Repository::init(
        dir.path().join("clone"),
        &InitOptions::default(),
        &teacher(),
        &AllowAll,
    )
    .unwrap();
    assert_eq!(clone.kept_namespace(&origin()).unwrap(), None);
    let path = clone.metadata_dir().join(NAMESPACES_DIR);
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("origin"), "stemma-policy 2\n").unwrap();
    assert!(matches!(
        clone.kept_namespace(&origin()),
        Err(KeptNamespaceReadError::Damaged { .. })
    ));
}

#[test]
fn a_bare_repository_keeps_no_namespace_policy() {
    let dir = TempDir::new();
    let bare = Repository::init_bare(dir.path().join("hub"), &teacher(), &AllowAll).unwrap();
    assert!(matches!(
        bare.kept_namespace(&origin()),
        Err(KeptNamespaceReadError::Refs(RefError::Bare { .. }))
    ));
}
