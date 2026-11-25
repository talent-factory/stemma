//! The tests of the ref update and of the record: the texts of the formats, the rules of the
//! chains for the writer and for the reader, the signatures, the receipt, and the standing of an
//! actor in a store that lost its commits.

use ssh_key::private::Ed25519Keypair;
use ssh_key::{HashAlg, LineEnding, PrivateKey};

use super::*;
use crate::change::sample_change;
use crate::fuzz::check_rejection;
use crate::object::{Blob, Commit, FileMode, Tree, TreeEntry};
use crate::signature::NAMESPACE;
use crate::store::FsObjectStore;
use crate::testutil::TempDir;
use crate::Actor;

const REPOSITORY: &str = "00112233445566778899aabbccddeeff";
const HUB: &str = "https://hub.example";

/// A signer that signs in process with a key from a fixed seed. Ed25519 signatures are
/// deterministic, so the examples that it signs have fixed bytes.
pub(crate) struct Seeded(PrivateKey);

impl Seeded {
    pub(crate) fn new(seed: u8) -> Self {
        Self(PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32])))
    }

    pub(crate) fn public(&self) -> PublicKey {
        PublicKey::from_openssh(&self.0.public_key().to_openssh().unwrap()).unwrap()
    }
}

impl Signer for Seeded {
    fn sign(&self, message: &[u8]) -> Result<Signature, SignError> {
        let pem = self
            .0
            .sign(NAMESPACE, HashAlg::Sha512, message)
            .unwrap()
            .to_pem(LineEnding::LF)
            .unwrap();
        Ok(Signature::from_armored(&pem).unwrap())
    }
}

/// A signer that signs other bytes than the ones it is given.
struct Liar(Seeded);

impl Signer for Liar {
    fn sign(&self, _: &[u8]) -> Result<Signature, SignError> {
        self.0.sign(b"something else")
    }
}

/// A signer that fails.
struct Broken;

impl Signer for Broken {
    fn sign(&self, _: &[u8]) -> Result<Signature, SignError> {
        Err(SignError::Unreadable {
            reason: "broken".into(),
        })
    }
}

/// A destination that takes no byte.
struct Full;

impl Write for Full {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("the disk is full"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A destination that takes every byte and cannot flush.
struct Unflushed;

impl Write for Unflushed {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("cannot flush"))
    }
}

/// The key of the actor of these tests.
fn daniel() -> Seeded {
    Seeded::new(1)
}

/// The key of the hub of these tests.
fn hub() -> Seeded {
    Seeded::new(2)
}

fn keys() -> HubKeys {
    HubKeys::try_new(hub().public(), Vec::new()).unwrap()
}

/// The text of an ID whose bytes are all `byte`.
fn id_text(byte: u8) -> String {
    format!("b3:{}", format!("{byte:02x}").repeat(32))
}

fn commit(byte: u8) -> CommitId {
    CommitId::new(id_text(byte).parse().unwrap())
}

fn entry_hash(byte: u8) -> RecordHash {
    id_text(byte).parse().unwrap()
}

fn statement(byte: u8) -> StatementHash {
    id_text(byte).parse().unwrap()
}

fn name(text: &str) -> QualifiedName {
    text.parse().unwrap()
}

fn repository() -> RepositoryId {
    REPOSITORY.parse().unwrap()
}

fn moved(
    reference: &str,
    previous: Option<RecordHash>,
    old: Option<CommitId>,
    new: Option<CommitId>,
) -> Move {
    Move::try_new(name(reference), previous, old, new, None).unwrap()
}

/// A ref update of Daniel for the repository of these tests.
fn update(moves: Vec<Move>) -> RefUpdate {
    update_for(repository(), moves)
}

fn update_for(repository: RepositoryId, moves: Vec<Move>) -> RefUpdate {
    let signer = daniel();
    RefUpdatePayload::try_new(
        HubAddress::try_new(HUB).unwrap(),
        repository,
        moves,
        signer.public(),
    )
    .unwrap()
    .sign(&signer)
    .unwrap()
}

/// What the hub of these tests states about an update, with these fast-forwards.
fn acceptance(fast_forward: &[&str]) -> Acceptance {
    Acceptance {
        time: 1_795_000_100,
        register: commit(0x11),
        policies: vec![commit(0x55)],
        fast_forward: fast_forward.iter().map(|text| name(text)).collect(),
        actor: Identity::human("daniel").unwrap(),
    }
}

/// Appends an update to a chain and a record, signed by the hub of these tests.
fn append(chain: &mut Chain, record: &mut Vec<u8>, update: RefUpdate) -> RecordEntry {
    chain
        .append(&update, acceptance(&[]), &hub(), record)
        .unwrap()
}

/// The entry that a hub would write for the update with these lines, without any check of the
/// writer: the hash and the signature are right, so only a rule of the reader can refuse it.
fn forged(update: &RefUpdate, hub_lines: &str, signer: &dyn Signer) -> Vec<u8> {
    let mut bytes = update.encode();
    bytes.extend_from_slice(hub_lines.as_bytes());
    let hash = hash_of(&bytes);
    let signature = signature::sign(
        signer,
        PayloadKind::RecordEntry,
        hash.to_string().as_bytes(),
    )
    .unwrap();
    bytes
        .extend_from_slice(format!("hub-signature {}\n", signature_to_line(&signature)).as_bytes());
    bytes
}

/// The lines of the hub for the entry with this number and this previous entry, for an update
/// with the references of `forwards`.
fn hub_lines(number: u64, previous: Option<RecordHash>, forwards: &[(&str, &str)]) -> String {
    let mut text = format!(
        "number {number}\ntime 1795000100\nregister {}\npolicy {}\n",
        id_text(0x11),
        id_text(0x55)
    );
    for (reference, forward) in forwards {
        text.push_str(&format!("fast-forward {reference} {forward}\n"));
    }
    text.push_str(&format!(
        "actor human daniel\nprevious {}\n",
        or_none(&previous)
    ));
    text
}

/// The update of the checked-in example `ref-update`: a merge into `main` that rests on a pull
/// request, and a new tag, under one signature.
pub(crate) fn example_update() -> RefUpdate {
    update(vec![
        Move::try_new(name("tags/v0.2.0"), None, None, Some(commit(0x22)), None).unwrap(),
        Move::try_new(
            name("heads/main"),
            Some(entry_hash(0xa1)),
            Some(commit(0x11)),
            Some(commit(0x22)),
            Some(statement(0xe1)),
        )
        .unwrap(),
    ])
}

/// The entry of the checked-in example `receipt`: the receipt of [`example_update`], entry 98
/// of its repository.
pub(crate) fn example_receipt() -> RecordEntry {
    // The writer begins at the first entry of a record, so the lines of entry 98 are set here.
    let lines = hub_lines(
        98,
        Some(entry_hash(0xa0)),
        &[("heads/main", "yes"), ("tags/v0.2.0", "no")],
    );
    RecordEntry::parse(&forged(&example_update(), &lines, &hub())).unwrap()
}

#[test]
fn the_ref_update_has_this_text() {
    let update = example_update();
    let payload = format!(
        "hub https://hub.example\n\
         repository {REPOSITORY}\n\
         ref heads/main {} {} {}\n\
         evidence {}\n\
         ref tags/v0.2.0 - - {}\n\
         evidence -\n\
         key {}\n",
        id_text(0xa1),
        id_text(0x11),
        id_text(0x22),
        id_text(0xe1),
        id_text(0x22),
        daniel().public()
    );
    assert_eq!(
        String::from_utf8(update.payload().encode()).unwrap(),
        payload
    );
    let text = String::from_utf8(update.encode()).unwrap();
    let signature = text
        .strip_prefix(&format!("stemma-protocol 1\n{payload}signature "))
        .and_then(|rest| rest.strip_suffix('\n'))
        .expect("the message is the first line, the payload, and the signature");
    assert!(!signature.contains('\n'));
    assert_eq!(RefUpdate::parse(text.as_bytes()), Ok(update.clone()));
    // The signature is a proof of its own type over the payload alone.
    assert_eq!(
        signature::signed_bytes(PayloadKind::RefUpdate, 1, payload.as_bytes()),
        format!("stemma-proof\nref-update\n1\n{payload}").into_bytes()
    );
    assert_eq!(
        signature::verify(
            &daniel().public(),
            PayloadKind::RefUpdate,
            1,
            payload.as_bytes(),
            update.signature()
        ),
        Ok(())
    );
}

#[test]
fn the_record_entry_has_this_text() {
    let entry = example_receipt();
    let update = String::from_utf8(example_update().encode()).unwrap();
    let hashed = format!(
        "{update}\
         number 98\n\
         time 1795000100\n\
         register {}\n\
         policy {}\n\
         fast-forward heads/main yes\n\
         fast-forward tags/v0.2.0 no\n\
         actor human daniel\n\
         previous {}\n",
        id_text(0x11),
        id_text(0x55),
        id_text(0xa0)
    );
    let text = String::from_utf8(entry.encode()).unwrap();
    let signature = text
        .strip_prefix(&format!("{hashed}hub-signature "))
        .and_then(|rest| rest.strip_suffix('\n'))
        .expect("the entry is the update, the lines of the hub, and its signature");
    assert_eq!(RecordEntry::parse(text.as_bytes()), Ok(entry.clone()));

    // The hash covers everything before the signature of the hub, under a header of its own.
    let digest = blake3::hash(format!("stemma-record-hash 1\n{hashed}").as_bytes());
    assert_eq!(entry.hash().to_string(), format!("b3:{}", digest.to_hex()));
    // The hub signs the text of the hash as a proof of its own type.
    assert_eq!(
        signature::verify(
            &hub().public(),
            PayloadKind::RecordEntry,
            1,
            entry.hash().to_string().as_bytes(),
            &signature_from_line(signature).unwrap()
        ),
        Ok(())
    );
    assert_eq!(entry.hub_key(), hub().public());

    assert_eq!(entry.number(), 98);
    assert_eq!(entry.time(), 1_795_000_100);
    assert_eq!(entry.register(), commit(0x11));
    assert_eq!(entry.policies(), [commit(0x55)]);
    assert_eq!(entry.is_fast_forward(&name("heads/main")), Some(true));
    assert_eq!(entry.is_fast_forward(&name("tags/v0.2.0")), Some(false));
    assert_eq!(entry.is_fast_forward(&name("heads/other")), None);
    assert_eq!(entry.actor(), &Identity::human("daniel").unwrap());
    assert_eq!(entry.previous(), Some(entry_hash(0xa0)));
}

#[test]
fn a_b_a_does_not_make_an_old_update_count_again() {
    let (a, b) = (commit(0x0a), commit(0x0b));
    let mut chain = Chain::new(repository());
    let mut record = Vec::new();
    let main = "heads/main";
    let first = append(
        &mut chain,
        &mut record,
        update(vec![moved(main, None, None, Some(a))]),
    );
    // The update from A to B, signed once.
    let to_b = update(vec![moved(main, Some(first.hash()), Some(a), Some(b))]);
    let second = append(&mut chain, &mut record, to_b.clone());
    // The reference returns to A.
    let third = append(
        &mut chain,
        &mut record,
        update(vec![moved(main, Some(second.hash()), Some(b), Some(a))]),
    );
    assert_eq!(chain.commit_of(&name(main)), Some(a));

    // The old update expects A, and the reference is at A again, but the last entry of the name
    // is another one: the update does not count a second time.
    let expected = ChainError::Stale(Stale(vec![StaleReference {
        name: name(main),
        entry: Some(third.hash()),
        commit: Some(a),
    }]));
    assert_eq!(chain.check(&to_b), Err(expected.clone()));
    let before = (chain.clone(), record.clone());
    let refused = chain
        .append(&to_b, acceptance(&[]), &hub(), &mut record)
        .unwrap_err();
    assert!(matches!(&refused, WriteError::Chain(error) if error == &expected));
    assert_eq!(
        (chain.clone(), record.clone()),
        before,
        "nothing was written"
    );

    // The reader refuses the same entry when it is written by hand, past the writer.
    let entry = forged(&to_b, &hub_lines(4, chain.tip(), &[(main, "yes")]), &hub());
    let entry = RecordEntry::parse(&entry).expect("the form and the signatures are right");
    assert_eq!(chain.accept(&entry), Err(expected));
    // So does the reader of the whole record, with the forged entry at its end.
    let mut whole = read_record(&record).unwrap();
    whole.push(entry);
    let mut reader = Chain::new(repository());
    assert!(matches!(
        reader.verify(&whole, &keys(), &TempDir::new().store()),
        Err(RecordError::Chain(ChainError::Stale(_)))
    ));
    assert_eq!(reader.last_number(), 3);

    // A reader of the whole record comes to the same state as the writer.
    let mut read = Chain::new(repository());
    for entry in read_record(&record).unwrap() {
        read.accept(&entry).unwrap();
    }
    assert_eq!(read, chain);
    assert_eq!(read.last_number(), 3);
}

#[test]
fn the_chain_of_a_name_survives_its_deletion() {
    let tag = "tags/v1.0.0";
    let mut chain = Chain::new(repository());
    let mut record = Vec::new();
    let set = append(
        &mut chain,
        &mut record,
        update(vec![moved(tag, None, None, Some(commit(1)))]),
    );
    let deleted = append(
        &mut chain,
        &mut record,
        update(vec![moved(tag, Some(set.hash()), Some(commit(1)), None)]),
    );
    assert_eq!(chain.commit_of(&name(tag)), None);
    assert_eq!(chain.entry_of(&name(tag)), Some(deleted.hash()));

    // Setting the tag again as a name that the hub never saw is stale: the name has a chain, and
    // the answer names its last entry and no commit.
    let as_new = update(vec![moved(tag, None, None, Some(commit(2)))]);
    let stale = ChainError::Stale(Stale(vec![StaleReference {
        name: name(tag),
        entry: Some(deleted.hash()),
        commit: None,
    }]));
    assert_eq!(chain.check(&as_new), Err(stale.clone()));
    // So is the update that set it the first time, replayed after the deletion.
    let replayed = update(vec![moved(tag, None, None, Some(commit(1)))]);
    assert_eq!(chain.check(&replayed), Err(stale));

    // The update that names the deleting entry continues the chain.
    let again = update(vec![moved(
        tag,
        Some(deleted.hash()),
        None,
        Some(commit(2)),
    )]);
    let third = append(&mut chain, &mut record, again);
    assert_eq!(chain.commit_of(&name(tag)), Some(commit(2)));
    assert_eq!(chain.entry_of(&name(tag)), Some(third.hash()));
    assert_eq!(
        chain.verify(&[], &keys(), &TempDir::new().store()).unwrap(),
        Vec::<u64>::new()
    );
}

#[test]
fn two_entries_with_one_number_and_different_hashes_are_detected() {
    let main = "heads/main";
    let write = |new: u8| {
        let mut chain = Chain::new(repository());
        append(
            &mut chain,
            &mut Vec::new(),
            update(vec![moved(main, None, None, Some(commit(new)))]),
        )
    };
    let (one, other) = (write(1), write(2));
    assert_eq!(one.number(), other.number());
    assert_ne!(one.hash(), other.hash());

    // Both are signed by the hub: the pair proves that the hub rewrote its record.
    assert!(keys().rewritten(&one, &other));
    assert!(!keys().rewritten(&one, &one));
    // An entry that another key signed proves nothing about the hub.
    let foreign = Chain::new(repository())
        .append(
            &update(vec![moved(main, None, None, Some(commit(2)))]),
            acceptance(&[]),
            &Seeded::new(9),
            &mut Vec::new(),
        )
        .unwrap();
    assert!(!keys().rewritten(&one, &foreign));
    assert!(!keys().rewritten(&foreign, &one));
    // Two entries of one record carry two numbers.
    let mut chain = Chain::new(repository());
    let mut record = Vec::new();
    let first = append(
        &mut chain,
        &mut record,
        update(vec![moved(main, None, None, Some(commit(1)))]),
    );
    let second = append(
        &mut chain,
        &mut record,
        update(vec![moved("heads/b", None, None, Some(commit(1)))]),
    );
    assert!(!keys().rewritten(&first, &second));
    // Entries of two repositories share their numbers.
    let elsewhere: RepositoryId = "ffeeddccbbaa99887766554433221100".parse().unwrap();
    let there = Chain::new(elsewhere)
        .append(
            &update_for(elsewhere, vec![moved(main, None, None, Some(commit(2)))]),
            acceptance(&[]),
            &hub(),
            &mut Vec::new(),
        )
        .unwrap();
    assert!(!keys().rewritten(&one, &there));

    // A clone that holds the receipt of entry 1 and reads the record from there finds it.
    let mut chain = Chain::after(repository(), 1, one.hash());
    assert_eq!(
        chain.accept(&other),
        Err(ChainError::Rewritten { number: 1 })
    );
    assert_eq!(
        chain
            .verify(&[other], &keys(), &TempDir::new().store())
            .unwrap_err()
            .to_string(),
        "two different record entries carry the number 1: the record was rewritten"
    );
    // An entry of another repository with that number says nothing about this record.
    assert_eq!(
        chain.accept(&there),
        Err(ChainError::Repository {
            expected: repository(),
            found: elsewhere
        })
    );
    // Its own entry, read again, is not the next one.
    assert_eq!(
        chain.accept(&one),
        Err(ChainError::Number {
            expected: 2,
            found: 1
        })
    );
}

/// Stores a commit whose tree holds this register, or no register, and returns it.
fn register_commit(store: &FsObjectStore, register: Option<&Register>) -> CommitId {
    let Some(register) = register else {
        return commit_of(store, Vec::new());
    };
    register_tree(store, |store| {
        let blob = Blob::new(register.encode());
        store.put_blob(&blob).unwrap();
        TreeEntry::file("register", FileMode::File, blob.id()).unwrap()
    })
}

/// Stores a commit whose directory `.stemma-actors` holds the entry that `entry` makes.
fn register_tree(
    store: &FsObjectStore,
    entry: impl FnOnce(&FsObjectStore) -> TreeEntry,
) -> CommitId {
    let directory = Tree::try_new(vec![entry(store)]).unwrap();
    store.put_tree(&directory).unwrap();
    commit_of(
        store,
        vec![TreeEntry::directory(".stemma-actors", directory.id()).unwrap()],
    )
}

/// Stores a commit with a file and these further entries of its tree.
fn commit_of(store: &FsObjectStore, mut entries: Vec<TreeEntry>) -> CommitId {
    let file = Blob::new("content\n");
    store.put_blob(&file).unwrap();
    entries.push(TreeEntry::file("a.txt", FileMode::File, file.id()).unwrap());
    let tree = Tree::try_new(entries).unwrap();
    store.put_tree(&tree).unwrap();
    let commit = Commit::try_new(
        tree.id(),
        Vec::new(),
        sample_change(),
        Actor::human("daniel").unwrap(),
        1_795_000_000,
        "A register\n",
    )
    .unwrap();
    store.put_commit(&commit).unwrap()
}

/// A record of two entries whose register commit is `register`.
fn record_at(register: CommitId) -> Vec<RecordEntry> {
    let mut chain = Chain::new(repository());
    let mut record = Vec::new();
    let mut write = |update| {
        let stated = Acceptance {
            register,
            ..acceptance(&[])
        };
        chain.append(&update, stated, &hub(), &mut record).unwrap()
    };
    let first = write(update(vec![moved(
        "heads/main",
        None,
        None,
        Some(commit(1)),
    )]));
    write(update(vec![moved(
        "heads/main",
        Some(first.hash()),
        Some(commit(1)),
        Some(commit(2)),
    )]));
    read_record(&record).unwrap()
}

#[test]
fn a_record_whose_commits_are_gone_is_no_longer_checkable_and_not_damaged() {
    let owner = Identity::human("daniel").unwrap();
    let register = Register::with_owner(owner, daniel().public()).unwrap();

    // The commit is in the store: the actor stands.
    let dir = TempDir::new();
    let store = dir.store();
    let entries = record_at(register_commit(&store, Some(&register)));
    assert_eq!(entries[0].standing(&store).unwrap(), Standing::Checked);
    let checked = Chain::new(repository()).verify(&entries, &keys(), &store);
    assert_eq!(checked.unwrap(), Vec::<u64>::new());

    // The same record against a store that lost the commits: the chains and the signatures
    // hold, and the two entries are reported, not refused.
    let empty = TempDir::new();
    let gone = empty.store();
    assert_eq!(
        entries[0].standing(&gone).unwrap(),
        Standing::NoLongerCheckable
    );
    let mut chain = Chain::new(repository());
    assert_eq!(chain.verify(&entries, &keys(), &gone).unwrap(), [1, 2]);
    assert_eq!(chain.last_number(), 2);

    // A commit whose tree is gone is no longer checkable either.
    let partial = TempDir::new();
    let partly = partial.store();
    let commit = store.get_commit(&entries[0].register()).unwrap().unwrap();
    partly.put_commit(&commit).unwrap();
    assert_eq!(
        entries[0].standing(&partly).unwrap(),
        Standing::NoLongerCheckable
    );

    // A record that is damaged stays damaged in a store without commits.
    let mut chain = Chain::new(repository());
    let swapped = [entries[1].clone(), entries[0].clone()];
    assert!(matches!(
        chain.verify(&swapped, &keys(), &gone),
        Err(RecordError::Chain(ChainError::Number {
            expected: 1,
            found: 2
        }))
    ));
}

#[test]
fn an_actor_that_the_register_does_not_hold_does_not_stand() {
    let dir = TempDir::new();
    let store = dir.store();
    let mia = Identity::human("mia").unwrap();
    let daniel_id = Identity::human("daniel").unwrap();

    // The register holds the key for another actor than the one that the hub names.
    let other_actor = Register::with_owner(mia, daniel().public()).unwrap();
    let entries = record_at(register_commit(&store, Some(&other_actor)));
    let error = entries[0].standing(&store).unwrap_err();
    assert!(matches!(error, StandingError::NotActive), "{error}");
    check_rejection(&error);
    let mut chain = Chain::new(repository());
    let refused = chain.verify(&entries, &keys(), &store).unwrap_err();
    assert!(matches!(
        refused,
        RecordError::Standing {
            number: 1,
            source: StandingError::NotActive
        }
    ));
    check_rejection(&refused);
    assert_eq!(
        chain,
        Chain::new(repository()),
        "the chain did not move past the refused entry"
    );

    // The register holds the actor with another key.
    let other_key = Register::with_owner(daniel_id.clone(), Seeded::new(7).public()).unwrap();
    let entries = record_at(register_commit(&store, Some(&other_key)));
    assert!(matches!(
        entries[0].standing(&store),
        Err(StandingError::NotActive)
    ));

    // A retired key signs nothing new.
    let mut retired = Register::with_owner(daniel_id.clone(), daniel().public()).unwrap();
    retired
        .add_key(&daniel_id, Seeded::new(7).public())
        .unwrap();
    retired.retire(&daniel_id, &daniel().public(), 5).unwrap();
    let entries = record_at(register_commit(&store, Some(&retired)));
    assert!(matches!(
        entries[0].standing(&store),
        Err(StandingError::NotActive)
    ));

    // A register that cannot be read is damage, not "no longer checkable".
    let garbage = register_tree(&store, |store| {
        let blob = Blob::new("no register\n");
        store.put_blob(&blob).unwrap();
        TreeEntry::file("register", FileMode::File, blob.id()).unwrap()
    });
    let entries = record_at(garbage);
    let error = entries[0].standing(&store).unwrap_err();
    assert!(
        matches!(error, StandingError::Unreadable(ModeError::Parse(_))),
        "{error}"
    );
    check_rejection(&error);
    let refused = Chain::new(repository())
        .verify(&entries, &keys(), &store)
        .unwrap_err();
    assert!(matches!(refused, RecordError::Standing { number: 1, .. }));
    check_rejection(&refused);
    let directory = register_tree(&store, |store| {
        let file = Blob::new("x\n");
        store.put_blob(&file).unwrap();
        let inner = Tree::try_new(vec![
            TreeEntry::file("x", FileMode::File, file.id()).unwrap()
        ])
        .unwrap();
        store.put_tree(&inner).unwrap();
        TreeEntry::directory("register", inner.id()).unwrap()
    });
    assert!(matches!(
        record_at(directory)[0].standing(&store),
        Err(StandingError::Unreadable(ModeError::NotRegisterFile))
    ));
    // A register file that is gone from the store is no longer checkable, as a commit is.
    let missing = register_tree(&store, |_| {
        TreeEntry::file("register", FileMode::File, Blob::new("gone\n").id()).unwrap()
    });
    assert_eq!(
        record_at(missing)[0].standing(&store).unwrap(),
        Standing::NoLongerCheckable
    );

    // A commit with an open conflict never was a tip of `main`, so no hub read its register.
    let tree = |name: &str| {
        let blob = Blob::new(name);
        store.put_blob(&blob).unwrap();
        let entry = TreeEntry::file(name, FileMode::File, blob.id()).unwrap();
        store
            .put_tree(&Tree::try_new(vec![entry]).unwrap())
            .unwrap()
    };
    let conflict = crate::object::Commit2::try_new(
        vec![tree("a"), tree("b"), tree("c")],
        Vec::new(),
        sample_change(),
        Actor::human("daniel").unwrap(),
        1_795_000_000,
        "A conflict\n",
    )
    .unwrap();
    let conflict = CommitId::new(
        store
            .put_raw(crate::object::Kind::Commit2, &conflict.encode())
            .unwrap(),
    );
    let error = record_at(conflict)[0].standing(&store).unwrap_err();
    assert!(matches!(error, StandingError::Conflict), "{error}");
    check_rejection(&error);
    // The same commit in a store that lost it is no longer checkable, as every commit is.
    assert_eq!(
        record_at(conflict)[0]
            .standing(&TempDir::new().store())
            .unwrap(),
        Standing::NoLongerCheckable
    );

    // The commit is there and holds no register.
    let entries = record_at(register_commit(&store, None));
    let error = entries[0].standing(&store).unwrap_err();
    assert!(matches!(error, StandingError::NoRegister), "{error}");
    check_rejection(&error);
}

#[test]
fn a_record_is_refused_for_an_entry_that_the_key_of_the_hub_did_not_sign() {
    let store_dir = TempDir::new();
    let entries = record_at(commit(0x11));
    // The key of the hub was another one for the first entry.
    let former = HubKeys::try_new(
        hub().public(),
        vec![FormerKey::try_new(1, 1, Seeded::new(8).public()).unwrap()],
    )
    .unwrap();
    assert_eq!(former.at(1), &Seeded::new(8).public());
    assert_eq!(former.at(2), &hub().public());
    let error = Chain::new(repository())
        .verify(&entries, &former, &store_dir.store())
        .unwrap_err();
    assert!(
        matches!(error, RecordError::HubKey { number: 1 }),
        "{error}"
    );
    check_rejection(&error);
    // With the range of the former key after the record, the current key signed both.
    let later = HubKeys::try_new(
        Seeded::new(8).public(),
        vec![FormerKey::try_new(1, 2, hub().public()).unwrap()],
    )
    .unwrap();
    assert!(Chain::new(repository())
        .verify(&entries, &later, &store_dir.store())
        .is_ok());
    // The ranges of the former keys share no number, in whatever order they are given.
    let range = |first, last| FormerKey::try_new(first, last, Seeded::new(8).public()).unwrap();
    assert_eq!(
        HubKeys::try_new(hub().public(), vec![range(5, 9), range(1, 5)]),
        Err(RefListError::OverlappingRanges(5))
    );
    let sorted = HubKeys::try_new(hub().public(), vec![range(5, 9), range(1, 4)]).unwrap();
    assert_eq!(sorted.at(4), &Seeded::new(8).public());
    assert_eq!(sorted.at(10), &hub().public());

    // A refused entry leaves the chain before it, for each of the three checks.
    let mut chain = Chain::new(repository());
    assert!(chain.verify(&entries, &former, &store_dir.store()).is_err());
    assert_eq!(chain.last_number(), 0);
    let wrong_second = HubKeys::try_new(
        hub().public(),
        vec![FormerKey::try_new(2, 2, Seeded::new(8).public()).unwrap()],
    )
    .unwrap();
    assert!(matches!(
        chain.verify(&entries, &wrong_second, &store_dir.store()),
        Err(RecordError::HubKey { number: 2 })
    ));
    assert_eq!(chain.last_number(), 1);
    assert!(matches!(
        chain.verify(&entries, &keys(), &store_dir.store()),
        Err(RecordError::Chain(ChainError::Number {
            expected: 2,
            found: 1
        }))
    ));
    assert_eq!(chain.last_number(), 1);
}

#[test]
fn a_receipt_is_checked_against_the_sent_update_and_the_key_of_the_hub() {
    let sent = example_update();
    let receipt = RecordEntry::parse(&example_receipt().encode()).unwrap();
    assert_eq!(receipt.check_receipt(&sent, &hub().public()), Ok(()));

    // Another key than the pinned one signed the receipt.
    assert_eq!(
        receipt.check_receipt(&sent, &Seeded::new(9).public()),
        Err(ReceiptError::OtherKey)
    );
    // The receipt holds another update: one more reference, signed by the same actor.
    let other = update(vec![moved("heads/main", None, None, Some(commit(0x22)))]);
    assert_eq!(
        receipt.check_receipt(&other, &hub().public()),
        Err(ReceiptError::OtherUpdate)
    );
    for error in [ReceiptError::OtherKey, ReceiptError::OtherUpdate] {
        check_rejection(&error);
    }

    // A receipt whose payload was changed on the way is refused when it is read: the signature
    // of the actor no longer verifies.
    let text = String::from_utf8(receipt.encode()).unwrap();
    let changed = text.replacen(&id_text(0x22), &id_text(0x23), 1);
    assert_eq!(
        RecordEntry::parse(changed.as_bytes()),
        Err(problem(9, RecordProblem::ActorSignature))
    );
    // So is one whose lines of the hub were changed: the signature of the hub covers them.
    let changed = text.replacen("number 98", "number 99", 1);
    assert_eq!(
        RecordEntry::parse(changed.as_bytes()),
        Err(problem(18, RecordProblem::HubSignature))
    );
}

#[test]
fn a_signature_of_another_type_or_key_is_refused() {
    let payload = example_update().payload().clone();
    // The actor signed the payload as an entry of the log.
    let as_log_entry =
        signature::sign(&daniel(), PayloadKind::LogEntry, &payload.encode()).unwrap();
    assert_eq!(
        RefUpdate::try_new(payload.clone(), as_log_entry),
        Err(VerifyError::Invalid)
    );
    // Another key signed it.
    let foreign =
        signature::sign(&Seeded::new(9), PayloadKind::RefUpdate, &payload.encode()).unwrap();
    assert_eq!(
        RefUpdate::try_new(payload.clone(), foreign.clone()),
        Err(VerifyError::WrongKey)
    );
    let sealed = payload.clone().sign(&Seeded::new(9)).unwrap_err();
    assert!(matches!(sealed, SealError::Verify(VerifyError::WrongKey)));
    check_rejection(&sealed);
    let sealed = payload.clone().sign(&Broken).unwrap_err();
    assert!(matches!(sealed, SealError::Sign(_)));
    assert!(sealed.io_error().is_none());
    check_rejection(&sealed);

    // The reader refuses the same message, at the line of the signature.
    let update = example_update();
    let text = String::from_utf8(update.encode()).unwrap();
    let own = signature_to_line(update.signature());
    let message = text.replacen(&own, &signature_to_line(&foreign), 1);
    assert_eq!(
        RefUpdate::parse(message.as_bytes()),
        Err(problem(9, RecordProblem::ActorKey))
    );
    // The key of the update signed, but not this payload as a ref update.
    let as_log_entry =
        signature::sign(&daniel(), PayloadKind::LogEntry, &update.payload().encode()).unwrap();
    let message = text.replacen(&own, &signature_to_line(&as_log_entry), 1);
    assert_eq!(
        RefUpdate::parse(message.as_bytes()),
        Err(problem(9, RecordProblem::ActorSignature))
    );
    // A `key` line that was changed to another key is told apart from a changed payload, in
    // a ref update and in an entry of the record.
    let other_key = text.replacen(
        &daniel().public().to_string(),
        &Seeded::new(9).public().to_string(),
        1,
    );
    assert_eq!(
        RefUpdate::parse(other_key.as_bytes()),
        Err(problem(9, RecordProblem::ActorKey))
    );
    let receipt = String::from_utf8(example_receipt().encode()).unwrap();
    let other_key = receipt.replacen(
        &daniel().public().to_string(),
        &Seeded::new(9).public().to_string(),
        1,
    );
    assert_eq!(
        RecordEntry::parse(other_key.as_bytes()),
        Err(problem(9, RecordProblem::ActorKey))
    );
    assert!(RefUpdate::parse_unverified(message.as_bytes()).is_ok());

    // A hub that signed the hash as a ref update did not sign a record entry.
    let lines = hub_lines(1, None, &[("heads/main", "no"), ("tags/v0.2.0", "no")]);
    let mut bytes = update.encode();
    bytes.extend_from_slice(lines.as_bytes());
    let hash = hash_of(&bytes).to_string();
    let wrong = signature::sign(&hub(), PayloadKind::RefUpdate, hash.as_bytes()).unwrap();
    bytes.extend_from_slice(format!("hub-signature {}\n", signature_to_line(&wrong)).as_bytes());
    assert_eq!(
        RecordEntry::parse(&bytes),
        Err(problem(18, RecordProblem::HubSignature))
    );
    assert!(RecordEntry::parse_unverified(&bytes).is_ok());
}

#[test]
fn a_move_refuses_what_moves_nothing_and_what_a_tag_cannot_do() {
    let try_new = |reference: &str, previous, old: Option<u8>, new: Option<u8>| {
        Move::try_new(
            name(reference),
            previous,
            old.map(commit),
            new.map(commit),
            None,
        )
        .map(|_| ())
    };
    let known = Some(entry_hash(0xa1));
    assert_eq!(
        try_new("heads/a", None, None, None),
        Err(RecordProblem::NoMove)
    );
    assert_eq!(
        try_new("heads/a", known, Some(1), Some(1)),
        Err(RecordProblem::NoMove)
    );
    assert_eq!(
        try_new("tags/a", known, Some(1), Some(2)),
        Err(RecordProblem::TagMoved)
    );
    assert_eq!(
        try_new("heads/a", None, Some(1), Some(2)),
        Err(RecordProblem::OldWithoutPrevious)
    );
    assert_eq!(
        try_new("tags/a", None, Some(1), None),
        Err(RecordProblem::OldWithoutPrevious)
    );
    for (reference, previous, old, new) in [
        ("heads/a", None, None, Some(1)),
        ("heads/a", known, None, Some(1)),
        ("heads/a", known, Some(1), Some(2)),
        ("heads/a", known, Some(1), None),
        ("tags/a", None, None, Some(1)),
        ("tags/a", known, None, Some(1)),
        ("tags/a", known, Some(1), None),
    ] {
        assert_eq!(
            try_new(reference, previous, old, new),
            Ok(()),
            "{reference}"
        );
    }

    let payload = |moves| {
        RefUpdatePayload::try_new(
            HubAddress::try_new(HUB).unwrap(),
            repository(),
            moves,
            daniel().public(),
        )
    };
    assert_eq!(payload(Vec::new()), Err(RecordProblem::NoReference));
    let twice = vec![
        moved("heads/a", None, None, Some(commit(1))),
        moved("heads/a", None, None, Some(commit(2))),
    ];
    assert_eq!(payload(twice), Err(RecordProblem::DuplicateReference));
    // The same name as a branch and as a tag is two references.
    let both = vec![
        moved("tags/a", None, None, Some(commit(1))),
        moved("heads/a", None, None, Some(commit(1))),
    ];
    let built = payload(both).unwrap();
    assert_eq!(built.moves()[0].name(), &name("heads/a"));
    assert_eq!(built.hub().as_str(), HUB);
}

#[test]
fn the_address_of_a_hub_has_one_spelling() {
    for address in [
        "https://hub.example",
        "http://127.0.0.1:8080",
        "https://h/stemma",
    ] {
        assert_eq!(HubAddress::try_new(address).unwrap().to_string(), address);
    }
    for address in [
        "",
        "hub.example",
        "https://",
        "ftp://hub.example",
        "HTTPS://hub.example",
        "https://hub.example/",
        "https:///hub.example",
        "https://user@hub.example",
        "https://hub.example?x",
        "https://hub.example#x",
        "https://hub example",
        "https://hub.example\n",
        "https://h\u{fc}b.example",
    ] {
        assert_eq!(
            HubAddress::try_new(address),
            Err(RecordProblem::Address),
            "{address:?}"
        );
    }
}

/// Replaces the first occurrence of `from` in the text of the example update and reads the
/// result without the check of the signature, which no changed message passes.
fn changed_update(from: &str, to: &str) -> Result<RefUpdate, ParseError> {
    let text = String::from_utf8(example_update().encode()).unwrap();
    assert!(text.contains(from), "{from:?} is not in the example");
    let changed = text.replacen(from, to, 1);
    let read = RefUpdate::parse_unverified(changed.as_bytes());
    // The reader that checks the signature refuses for the same reason, or for the signature.
    match (&read, RefUpdate::parse(changed.as_bytes())) {
        (Err(form), Err(error)) => assert_eq!(&error, form),
        (Ok(_), Err(error)) => {
            assert!(matches!(
                error,
                ParseError::Record {
                    reason: RecordProblem::ActorSignature | RecordProblem::ActorKey,
                    ..
                }
            ))
        }
        (_, Ok(_)) => panic!("a changed message was accepted: {changed}"),
    }
    read
}

#[test]
fn a_ref_update_refuses_lines_that_its_writer_does_not_write() {
    let key = daniel().public().to_string();
    let (main, tag) = ("ref heads/main ", "ref tags/v0.2.0 - - ");
    let (a1, c11, c22) = (id_text(0xa1), id_text(0x11), id_text(0x22));
    let expected = |keyword| Err(malformed(2, Malformed::Expected(keyword)));
    assert_eq!(changed_update("hub ", "address "), expected("hub"));
    assert_eq!(
        changed_update("hub https://hub.example", "hub hub.example"),
        Err(problem(2, RecordProblem::Address))
    );
    assert_eq!(
        changed_update("hub https://hub.example", "hub https://hub.example/"),
        Err(problem(2, RecordProblem::Address))
    );
    assert!(matches!(
        changed_update(REPOSITORY, "0011"),
        Err(ParseError::InvalidRepositoryId { line: 3, .. })
    ));
    assert_eq!(
        changed_update(&format!("repository {REPOSITORY}\n"), ""),
        Err(malformed(3, Malformed::Expected("repository")))
    );
    // A reference without its evidence, and an evidence without its reference.
    assert_eq!(
        changed_update(&format!("evidence {}\n", id_text(0xe1)), ""),
        Err(malformed(5, Malformed::Expected("evidence")))
    );
    assert_eq!(
        changed_update(&format!("{main}{a1} {c11} {c22}\n"), ""),
        Err(malformed(4, Malformed::Expected("ref")))
    );
    assert_eq!(
        changed_update(&id_text(0xe1), "b3:e1"),
        Err(problem(5, RecordProblem::Evidence))
    );
    assert_eq!(
        changed_update("evidence -", "evidence"),
        Err(malformed(7, Malformed::Expected("evidence")))
    );
    // The fields of a `ref` line.
    assert_eq!(
        changed_update(&format!("{main}{a1} "), main),
        Err(malformed(4, Malformed::Fields))
    );
    assert_eq!(
        changed_update(&format!("{c11} {c22}\n"), &format!("{c11} {c22} -\n")),
        Err(malformed(4, Malformed::Fields))
    );
    assert_eq!(
        changed_update(&format!("{c11} {c22}\n"), &format!("{c11}  {c22}\n")),
        Err(malformed(4, Malformed::Fields))
    );
    assert!(matches!(
        changed_update("heads/main", "main"),
        Err(ParseError::InvalidName { line: 4, .. })
    ));
    assert!(matches!(
        changed_update(&a1, "b3:a1"),
        Err(ParseError::InvalidRecordHash { line: 4, .. })
    ));
    assert!(matches!(
        changed_update(&c11, "b3:11"),
        Err(ParseError::InvalidObjectId { line: 4, .. })
    ));
    assert!(matches!(
        changed_update(&format!("{tag}{c22}"), &format!("{tag}B3:22")),
        Err(ParseError::InvalidObjectId { line: 6, .. })
    ));
    // The rules of a move, which the constructor holds as well.
    assert_eq!(
        changed_update(&format!("{c11} {c22}\n"), &format!("{c22} {c22}\n")),
        Err(problem(4, RecordProblem::NoMove))
    );
    assert_eq!(
        changed_update(&format!("{tag}{c22}"), &format!("{tag}-")),
        Err(problem(6, RecordProblem::NoMove))
    );
    assert_eq!(
        changed_update(
            &format!("{tag}{c22}"),
            &format!("ref tags/v0.2.0 {a1} {c11} {c22}")
        ),
        Err(problem(6, RecordProblem::TagMoved))
    );
    assert_eq!(
        changed_update(&format!("{main}{a1}"), &format!("{main}-")),
        Err(problem(4, RecordProblem::OldWithoutPrevious))
    );
    // A move that is refused is reported at its line, whatever stands in the evidence line.
    assert_eq!(
        changed_update(
            &format!("{c11} {c22}\nevidence {}", id_text(0xe1)),
            &format!("{c22} {c22}\nevidence b3:e1")
        ),
        Err(problem(4, RecordProblem::NoMove))
    );
    // The references stand in the order of their names, each once.
    assert_eq!(
        changed_update("tags/v0.2.0", "heads/main"),
        Err(malformed(6, Malformed::Order))
    );
    assert_eq!(
        changed_update("tags/v0.2.0", "heads/a"),
        Err(malformed(6, Malformed::Order))
    );
    // The key and the signature.
    assert_eq!(
        changed_update(&format!("key {key}\n"), ""),
        Err(malformed(8, Malformed::Expected("key")))
    );
    assert_eq!(
        changed_update(&key, &format!("{key} daniel")),
        Err(malformed(8, Malformed::KeyForm))
    );
    assert!(matches!(
        changed_update(&key, "ssh-ed25519 AAAA"),
        Err(ParseError::InvalidKey { line: 8, .. })
    ));
    assert_eq!(
        changed_update("signature ", "signature  "),
        Err(problem(9, RecordProblem::SignatureForm))
    );
    assert_eq!(
        changed_update("signature ", "signature x"),
        Err(problem(9, RecordProblem::SignatureForm))
    );
    assert_eq!(
        changed_update("signature ", "sig "),
        Err(malformed(9, Malformed::Expected("signature")))
    );
    // Nothing follows the signature, and every line ends with a line feed.
    let text = String::from_utf8(example_update().encode()).unwrap();
    assert_eq!(
        RefUpdate::parse(format!("{text}number 1\n").as_bytes()),
        Err(malformed(10, Malformed::Unexpected))
    );
    assert_eq!(
        RefUpdate::parse(text.trim_end().as_bytes()),
        Err(malformed(9, Malformed::CutOff))
    );
    assert_eq!(
        RefUpdate::parse(text.replace('\n', "\r\n").as_bytes()),
        Err(malformed(1, Malformed::FirstLine))
    );
    assert_eq!(
        RefUpdate::parse(
            text.replacen("stemma-protocol 1", "stemma-protocol 2", 1)
                .as_bytes()
        ),
        Err(ParseError::UnsupportedVersion { found: 2 })
    );
    // A payload that only names the key and no reference.
    assert_eq!(
        RefUpdate::parse(
            format!("stemma-protocol 1\nhub {HUB}\nrepository {REPOSITORY}\nkey {key}\n")
                .as_bytes()
        ),
        Err(malformed(4, Malformed::Expected("ref")))
    );
}

/// The example receipt with other lines of the hub, signed by the hub again, so that only the
/// form can refuse it.
fn receipt_with(change: impl FnOnce(String) -> String) -> Result<RecordEntry, ParseError> {
    let lines = hub_lines(
        98,
        Some(entry_hash(0xa0)),
        &[("heads/main", "yes"), ("tags/v0.2.0", "no")],
    );
    let changed = change(lines.clone());
    assert_ne!(changed, lines, "the lines were not changed");
    RecordEntry::parse(&forged(&example_update(), &changed, &hub()))
}

#[test]
fn a_record_entry_refuses_lines_that_its_writer_does_not_write() {
    // The helper writes the example unchanged.
    let lines = hub_lines(
        98,
        Some(entry_hash(0xa0)),
        &[("heads/main", "yes"), ("tags/v0.2.0", "no")],
    );
    assert_eq!(
        forged(&example_update(), &lines, &hub()),
        example_receipt().encode()
    );
    let replace = |from: &'static str, to: &'static str| {
        receipt_with(move |lines| {
            assert!(lines.contains(from), "{from:?}");
            lines.replacen(from, to, 1)
        })
    };
    let number = |text: &'static str| replace("number 98", text);
    for text in [
        "number 0",
        "number 098",
        "number +98",
        "number 18446744073709551616",
    ] {
        assert_eq!(
            number(text),
            Err(malformed(10, Malformed::Number)),
            "{text}"
        );
    }
    assert_eq!(
        number("count 98"),
        Err(malformed(10, Malformed::Expected("number")))
    );
    for text in [
        "time 01",
        "time +1",
        "time -0",
        "time 9223372036854775808",
        "time now",
    ] {
        assert_eq!(
            replace("time 1795000100", text),
            Err(problem(11, RecordProblem::Time)),
            "{text}"
        );
    }
    assert!(replace("time 1795000100", "time -5").is_ok());
    assert!(matches!(
        replace("register b3:", "register b4:"),
        Err(ParseError::InvalidObjectId { line: 12, .. })
    ));
    assert!(matches!(
        replace("policy b3:", "policy b4:"),
        Err(ParseError::InvalidObjectId { line: 13, .. })
    ));
    // The policies are a list: none, one, or several.
    let policy = format!("policy {}\n", id_text(0x55));
    let none = receipt_with(|lines| lines.replacen(&policy, "", 1)).unwrap();
    assert!(none.policies().is_empty());
    let two = receipt_with(|lines| {
        lines.replacen(&policy, &format!("{policy}policy {}\n", id_text(0x44)), 1)
    })
    .unwrap();
    assert_eq!(two.policies(), [commit(0x55), commit(0x44)]);

    // One `fast-forward` line for each reference, in the order of the references.
    let forward = |from, to| replace(from, to).unwrap_err();
    let refused = |line| problem(line, RecordProblem::FastForward);
    assert_eq!(forward("heads/main yes", "heads/main maybe"), refused(14));
    assert_eq!(forward("heads/main yes", "heads/main"), refused(14));
    assert_eq!(forward("heads/main yes", "heads/other yes"), refused(14));
    assert_eq!(forward("heads/main yes", "tags/v0.2.0 no"), refused(14));
    // A reference that is created is no fast-forward.
    assert_eq!(forward("tags/v0.2.0 no", "tags/v0.2.0 yes"), refused(15));
    assert_eq!(
        forward("fast-forward tags/v0.2.0 no\n", ""),
        malformed(15, Malformed::Expected("fast-forward"))
    );
    assert_eq!(
        forward("actor human", "fast-forward heads/z no\nactor human"),
        malformed(16, Malformed::Expected("actor"))
    );
    assert!(replace("heads/main yes", "heads/main no").is_ok());

    for text in [
        "actor daniel",
        "actor human",
        "actor robot x",
        "actor human a b",
    ] {
        assert_eq!(
            replace("actor human daniel", text),
            Err(problem(16, RecordProblem::Actor)),
            "{text}"
        );
    }
    assert_eq!(
        replace("actor human daniel", "actor agent bot")
            .unwrap()
            .actor()
            .to_string(),
        "agent bot"
    );
    // Only the first entry of a record names no previous entry.
    assert_eq!(
        replace("previous b3:a0a0", "previous -\nprevious b3:a0a0").unwrap_err(),
        problem(17, RecordProblem::Previous)
    );
    assert!(matches!(
        replace("previous b3:", "previous b4:"),
        Err(ParseError::InvalidRecordHash { line: 17, .. })
    ));
    assert_eq!(
        replace("number 98", "number 1"),
        Err(problem(17, RecordProblem::Previous))
    );
    let first = receipt_with(|lines| {
        lines
            .replacen("number 98", "number 1", 1)
            .replacen(&id_text(0xa0), "-", 1)
    })
    .unwrap();
    assert_eq!((first.number(), first.previous()), (1, None));

    // The signature of the hub, and nothing after it.
    let entry = String::from_utf8(example_receipt().encode()).unwrap();
    for (from, to, expected) in [
        (
            "hub-signature ",
            "hub-signature  ",
            problem(18, RecordProblem::SignatureForm),
        ),
        (
            "hub-signature ",
            "signature ",
            malformed(18, Malformed::Expected("hub-signature")),
        ),
    ] {
        let changed = entry.replacen(from, to, 1);
        assert_eq!(RecordEntry::parse(changed.as_bytes()), Err(expected));
    }
    assert_eq!(
        RecordEntry::parse(format!("{entry}number 99\n").as_bytes()),
        Err(malformed(19, Malformed::Unexpected))
    );
    assert_eq!(
        RecordEntry::parse(entry.trim_end().as_bytes()),
        Err(malformed(18, Malformed::CutOff))
    );
    // A ref update is no receipt, and a receipt is no ref update.
    assert_eq!(
        RecordEntry::parse(&example_update().encode()),
        Err(malformed(10, Malformed::Expected("number")))
    );
    assert_eq!(
        RefUpdate::parse(entry.as_bytes()),
        Err(malformed(10, Malformed::Unexpected))
    );
}

#[test]
fn the_reader_of_the_chain_refuses_what_the_writer_does_not_write() {
    let main = "heads/main";
    let mut chain = Chain::new(repository());
    let mut record = Vec::new();
    let first = append(
        &mut chain,
        &mut record,
        update(vec![moved(main, None, None, Some(commit(1)))]),
    );
    let next = update(vec![moved(
        main,
        Some(first.hash()),
        Some(commit(1)),
        Some(commit(2)),
    )]);
    let read = |lines: &str| RecordEntry::parse(&forged(&next, lines, &hub())).unwrap();
    let before = chain.clone();

    // A number that is not the next one.
    let entry = read(&hub_lines(3, Some(first.hash()), &[(main, "yes")]));
    assert_eq!(
        chain.accept(&entry),
        Err(ChainError::Number {
            expected: 2,
            found: 3
        })
    );
    // A previous entry that is not the last one.
    let entry = read(&hub_lines(2, Some(entry_hash(0xbb)), &[(main, "yes")]));
    assert_eq!(
        chain.accept(&entry),
        Err(ChainError::Previous { number: 2 })
    );
    // An update of another repository.
    let elsewhere: RepositoryId = "ffeeddccbbaa99887766554433221100".parse().unwrap();
    let foreign = update_for(
        elsewhere,
        vec![moved(
            main,
            Some(first.hash()),
            Some(commit(1)),
            Some(commit(2)),
        )],
    );
    let lines = hub_lines(2, Some(first.hash()), &[(main, "yes")]);
    let entry = RecordEntry::parse(&forged(&foreign, &lines, &hub())).unwrap();
    let expected = ChainError::Repository {
        expected: repository(),
        found: elsewhere,
    };
    assert_eq!(chain.accept(&entry), Err(expected.clone()));
    assert!(matches!(
        chain.append(&foreign, acceptance(&[]), &hub(), &mut record),
        Err(WriteError::Chain(error)) if error == expected
    ));
    // An `old` that is not the commit that the last entry left, with the right `previous`.
    let wrong_old = update(vec![moved(
        main,
        Some(first.hash()),
        Some(commit(9)),
        Some(commit(2)),
    )]);
    let entry = RecordEntry::parse(&forged(&wrong_old, &lines, &hub())).unwrap();
    assert!(matches!(chain.accept(&entry), Err(ChainError::Stale(_))));
    // A new name with a `previous` in a record that was read from its first entry.
    let unknown = update(vec![moved(
        "heads/new",
        Some(entry_hash(0xcc)),
        None,
        Some(commit(2)),
    )]);
    let lines = hub_lines(2, Some(first.hash()), &[("heads/new", "no")]);
    let entry = RecordEntry::parse(&forged(&unknown, &lines, &hub())).unwrap();
    let stale = chain.accept(&entry).unwrap_err();
    assert_eq!(
        stale.to_string(),
        "the ref update does not name the last record entry of heads/new (entry -, commit -)"
    );
    assert_eq!(chain, before, "a refused entry does not move the chain");

    // The same entry continues a chain that began in the middle: the name is not known there,
    // so its first update is taken as it is, and the next one is checked against it.
    let mut partial = Chain::after(repository(), 1, first.hash());
    partial.accept(&entry).unwrap();
    assert_eq!(partial.entry_of(&name("heads/new")), Some(entry.hash()));
    assert_eq!(partial.entry_of(&name(main)), None);
    let replayed = partial.check(&unknown).unwrap_err();
    assert!(matches!(replayed, ChainError::Stale(_)));
    // A chain after no entry is the chain of a new record.
    assert_eq!(
        Chain::after(repository(), 0, entry_hash(0xdd)),
        Chain::new(repository())
    );
    assert_eq!(Chain::new(repository()).tip(), None);
    assert_eq!(Chain::new(repository()).repository(), repository());

    // One stale reference refuses the whole update, and the answer names every one of them.
    let both = update(vec![
        moved(main, None, None, Some(commit(2))),
        moved("heads/b", Some(entry_hash(0xcc)), None, Some(commit(2))),
        moved("heads/ok", None, None, Some(commit(2))),
    ]);
    let Err(ChainError::Stale(stale)) = chain.check(&both) else {
        panic!("the update is stale");
    };
    let names: Vec<String> = stale
        .references()
        .iter()
        .map(|stale| stale.name.to_string())
        .collect();
    assert_eq!(names, ["heads/b", "heads/main"]);
    assert_eq!(stale.references()[1].commit, Some(commit(1)));

    // A chain that began in the middle reads only: the writer must know every name.
    let update = update(vec![moved("heads/x", None, None, Some(commit(1)))]);
    let mut record = Vec::new();
    let refused = partial.append(&update, acceptance(&[]), &hub(), &mut record);
    assert!(matches!(
        refused,
        Err(WriteError::Chain(ChainError::NotWhole))
    ));
    assert!(record.is_empty());
    check_rejection(&ChainError::NotWhole);
    // The largest number has no next one.
    let full = Chain::after(repository(), u64::MAX, first.hash());
    let lines = hub_lines(5, Some(first.hash()), &[("heads/x", "no")]);
    let entry = RecordEntry::parse(&forged(&update, &lines, &hub())).unwrap();
    assert_eq!(full.clone().accept(&entry), Err(ChainError::Full));
}

#[test]
fn the_writer_refuses_before_it_writes_and_leaves_the_chain_where_it_was() {
    let main = "heads/main";
    let mut chain = Chain::new(repository());
    let mut record = Vec::new();
    let first = append(
        &mut chain,
        &mut record,
        update(vec![moved(main, None, None, Some(commit(1)))]),
    );
    let before = (chain.clone(), record.clone());
    let next = || {
        update(vec![
            moved(main, Some(first.hash()), Some(commit(1)), Some(commit(2))),
            moved("tags/v1", None, None, Some(commit(2))),
        ])
    };

    // A fast-forward of a reference that the update does not list, and of one that it creates.
    for reference in ["heads/other", "tags/v1"] {
        let error = chain
            .append(&next(), acceptance(&[reference]), &hub(), &mut record)
            .unwrap_err();
        assert!(
            matches!(&error, WriteError::FastForward(found) if found == &name(reference)),
            "{error}"
        );
        check_rejection(&error);
    }
    // A deletion is no fast-forward either, for the writer and for the reader.
    let deletion = || update(vec![moved(main, Some(first.hash()), Some(commit(1)), None)]);
    let error = chain
        .append(&deletion(), acceptance(&[main]), &hub(), &mut record)
        .unwrap_err();
    assert!(matches!(error, WriteError::FastForward(_)), "{error}");
    let lines = hub_lines(2, Some(first.hash()), &[(main, "yes")]);
    assert_eq!(
        RecordEntry::parse(&forged(&deletion(), &lines, &hub())),
        Err(problem(12, RecordProblem::FastForward))
    );
    // A signer that fails, and one that signs something else.
    let error = chain
        .append(&next(), acceptance(&[main]), &Broken, &mut record)
        .unwrap_err();
    assert!(matches!(error, WriteError::Sign(_)), "{error}");
    check_rejection(&error);
    let error = chain
        .append(&next(), acceptance(&[main]), &Liar(hub()), &mut record)
        .unwrap_err();
    assert!(
        matches!(error, WriteError::Signature(VerifyError::Invalid)),
        "{error}"
    );
    assert!(error.io_error().is_none());
    check_rejection(&error);
    assert_eq!((chain.clone(), record.clone()), before);
    // The writer borrows the update, so a refused update is still the caller's to write again.
    let kept = next();
    assert!(chain
        .append(&kept, acceptance(&[main]), &Broken, &mut record)
        .is_err());
    let mut retried = (chain.clone(), record.clone());
    let entry = retried
        .0
        .append(&kept, acceptance(&[main]), &hub(), &mut retried.1)
        .unwrap();
    assert_eq!(entry.update(), &kept);

    // A destination that fails: the chain stays, and the update can be written again.
    let error = chain
        .append(&next(), acceptance(&[main]), &hub(), &mut Full)
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "cannot write the record entry: the disk is full"
    );
    assert!(error.io_error().is_some());
    check_rejection(&error);
    assert_eq!(chain, before.0);
    // So does a destination that takes the bytes and cannot flush them.
    let error = chain
        .append(&next(), acceptance(&[main]), &hub(), &mut Unflushed)
        .unwrap_err();
    assert!(matches!(error, WriteError::Io(_)), "{error}");
    assert_eq!(chain, before.0);
    let second = chain
        .append(&next(), acceptance(&[main]), &hub(), &mut record)
        .unwrap();
    assert_eq!(second.number(), 2);
    assert_eq!(second.previous(), Some(first.hash()));
    assert_eq!(second.is_fast_forward(&name(main)), Some(true));
    assert_eq!(chain.tip(), Some(second.hash()));
    // The entry is the next link of every name that it lists.
    assert_eq!(chain.entry_of(&name(main)), Some(second.hash()));
    assert_eq!(chain.entry_of(&name("tags/v1")), Some(second.hash()));
    // What was written is the receipt, byte for byte.
    assert_eq!(read_record(&record).unwrap(), [first, second]);
}

#[test]
fn a_record_is_read_entry_by_entry_and_a_cut_is_reported_with_its_entry() {
    assert_eq!(read_record(b""), Ok(Vec::new()));
    let mut chain = Chain::new(repository());
    let mut record = Vec::new();
    let first = append(
        &mut chain,
        &mut record,
        update(vec![moved("heads/main", None, None, Some(commit(1)))]),
    );
    let one = record.len();
    let second = append(
        &mut chain,
        &mut record,
        update(vec![moved("heads/b", None, None, Some(commit(1)))]),
    );
    assert_eq!(read_record(&record), Ok(vec![first.clone(), second]));
    assert_eq!(read_record(&record[..one]), Ok(vec![first]));
    // A single entry is refused by the reader of a receipt when another follows it.
    assert_eq!(
        RecordEntry::parse(&record),
        Err(malformed(16, Malformed::Unexpected))
    );

    // Every cut inside the second entry is refused, and names the second entry.
    for end in one + 1..record.len() {
        let error = read_record(&record[..end]).unwrap_err();
        assert_eq!(error.entry, 2, "cut at {end}");
        check_rejection(&error);
    }
    let error = read_record(&record[..record.len() - 1]).unwrap_err();
    assert_eq!(
        error.to_string(),
        "entry 2 of the record: line 15: the line is cut off"
    );
    // An entry whose signature of the hub does not verify is refused when the record is read.
    let text = String::from_utf8(record).unwrap();
    let changed = text.replacen("time 1795000100", "time 1795000101", 1);
    assert_eq!(
        read_record(changed.as_bytes()),
        Err(RecordParseError {
            entry: 1,
            error: problem(15, RecordProblem::HubSignature)
        })
    );
}

#[test]
fn every_problem_has_a_message_of_its_own() {
    let problems = [
        RecordProblem::Address,
        RecordProblem::Evidence,
        RecordProblem::NoMove,
        RecordProblem::TagMoved,
        RecordProblem::OldWithoutPrevious,
        RecordProblem::NoReference,
        RecordProblem::DuplicateReference,
        RecordProblem::SignatureForm,
        RecordProblem::ActorKey,
        RecordProblem::ActorSignature,
        RecordProblem::HubSignature,
        RecordProblem::Time,
        RecordProblem::Actor,
        RecordProblem::FastForward,
        RecordProblem::Previous,
    ];
    let messages: BTreeSet<String> = problems.iter().map(ToString::to_string).collect();
    assert_eq!(messages.len(), problems.len());
    for reason in problems {
        check_rejection(&reason);
        let error = problem(3, reason);
        assert_eq!(error.to_string(), format!("line 3: {reason}"));
        check_rejection(&error);
    }
    for error in [
        ChainError::Number {
            expected: 2,
            found: 5,
        },
        ChainError::Rewritten { number: 2 },
        ChainError::Previous { number: 2 },
        ChainError::Full,
        ChainError::Repository {
            expected: repository(),
            found: repository(),
        },
    ] {
        check_rejection(&error);
    }
    // The types of the two proofs are part of the format.
    assert_eq!(PayloadKind::RefUpdate.name(), "ref-update");
    assert_eq!(PayloadKind::RecordEntry.name(), "record-entry");
}
