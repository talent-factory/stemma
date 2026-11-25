//! The tests of the protocol messages: the checked-in examples against values that are built
//! here, the refusals of the readers one rule at a time, and generated input through the check of
//! the fuzz target.

use std::io::{BufReader, Cursor};

use super::examples::{self, Message};
use super::*;
use crate::change::sample_change;
use crate::object::{Blob, Commit, FileMode, Tree, TreeEntry};
use crate::properties::{cases, seed, Rng};
use crate::Actor;

/// Three Ed25519 public keys, made for these tests; nobody holds their private halves.
const KEYS: [&str; 3] = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIjwWEZ4603dDnTO8ilPzaYzYG5PnbKb6TxOzauTQ9du",
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMvFr9B9A1KP4dOTnLhI/EgEXCwoXP3pX0wkzADNmpZG",
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF95/u2UGkOnSZeFLSp5AKJ+ZHAzKs6FvxQ/kpc8m7yg",
];

const REPOSITORY: &str = "00112233445566778899aabbccddeeff";

fn key(index: usize) -> PublicKey {
    PublicKey::from_openssh(KEYS[index]).unwrap()
}

/// The text of an ID whose bytes are all `byte`.
fn id_text(byte: u8) -> String {
    format!("b3:{}", format!("{byte:02x}").repeat(32))
}

fn commit(byte: u8) -> CommitId {
    CommitId::new(id_text(byte).parse().unwrap())
}

fn record(byte: u8) -> RecordHash {
    id_text(byte).parse().unwrap()
}

fn name(text: &str) -> QualifiedName {
    text.parse().unwrap()
}

fn repository() -> RepositoryId {
    REPOSITORY.parse().unwrap()
}

/// The objects of the example stream in the order of the format: the blob and the tree of the
/// one commit, then the commit.
fn example_objects() -> Vec<StreamObject> {
    let blob = Blob::new("hello\n");
    let tree = Tree::try_new(vec![TreeEntry::file(
        "greeting.txt",
        FileMode::File,
        blob.id(),
    )
    .unwrap()])
    .unwrap();
    let commit = Commit::try_new(
        tree.id(),
        Vec::new(),
        sample_change(),
        Actor::human("daniel").unwrap(),
        1_795_000_000,
        "Add a greeting\n",
    )
    .unwrap();
    vec![
        StreamObject {
            kind: Kind::Blob,
            body: blob.content().to_vec(),
        },
        StreamObject {
            kind: Kind::Tree,
            body: tree.encode(),
        },
        StreamObject {
            kind: Kind::Commit,
            body: commit.encode(),
        },
    ]
}

/// The commit of the example stream, which the other examples name.
fn example_commit() -> CommitId {
    CommitId::new(example_objects()[2].id())
}

fn example_refs() -> RefList {
    RefList::try_new(
        repository(),
        key(0),
        vec![
            FormerKey::try_new(41, 97, key(2)).unwrap(),
            FormerKey::try_new(1, 40, key(1)).unwrap(),
        ],
        vec![
            Reference::new(name("tags/v0.1.0"), example_commit(), record(0xa3)),
            Reference::new(name("heads/main"), example_commit(), record(0xa1)),
            Reference::new(name("heads/feat/login"), commit(0x22), record(0xa2)),
        ],
    )
    .unwrap()
}

fn example_fetch() -> FetchRequest {
    FetchRequest::try_new(
        [commit(0x22), example_commit()],
        [commit(0x44), commit(0x33)],
    )
    .unwrap()
}

fn write_stream(objects: &[StreamObject]) -> Vec<u8> {
    let mut writer = ObjectStreamWriter::new(Vec::new()).unwrap();
    for object in objects {
        writer.write(object.kind, &object.body).unwrap();
    }
    writer.finish()
}

fn read_stream(bytes: &[u8], limit: u64) -> Result<Vec<StreamObject>, StreamError> {
    let mut reader = ObjectStreamReader::new(bytes, limit)?;
    let mut objects = Vec::new();
    while let Some(object) = reader.next_object()? {
        objects.push(object);
    }
    Ok(objects)
}

/// What the writers of this module write for the value of each example.
fn written(example_name: &str) -> Vec<u8> {
    match example_name {
        "refs" => example_refs().encode(),
        "refs-empty" => RefList::try_new(repository(), key(0), Vec::new(), Vec::new())
            .unwrap()
            .encode(),
        "fetch" => example_fetch().encode(),
        "clone" => FetchRequest::try_new([example_commit()], [])
            .unwrap()
            .encode(),
        "objects" => write_stream(&example_objects()),
        "objects-empty" => write_stream(&[]),
        "error-unsupported-version" => ErrorMessage::UnsupportedVersion(Spoken::here()).encode(),
        "error-malformed" => {
            ErrorMessage::from(&FetchRequest::parse(b"stemma-protocol 1\n").unwrap_err()).encode()
        }
        "ref-update" => record::tests::example_update().encode(),
        "receipt" => record::tests::example_receipt().encode(),
        "error-stale" => refusal::tests::example_stale().encode(),
        "error-refused" => refusal::tests::example_refused().encode(),
        "error-refused-update" => refusal::tests::example_refused_update().encode(),
        other => panic!("no value for the example {other}"),
    }
}

/// Whether the reader of the message accepts the bytes and writes them back unchanged.
fn round_trips(message: Message, bytes: &[u8]) -> bool {
    match message {
        Message::RefList => RefList::parse(bytes).is_ok_and(|m| m.encode() == bytes),
        Message::FetchRequest => FetchRequest::parse(bytes).is_ok_and(|m| m.encode() == bytes),
        Message::ObjectStream => {
            read_stream(bytes, u64::MAX).is_ok_and(|objects| write_stream(&objects) == bytes)
        }
        Message::Error => ErrorMessage::parse(bytes).is_ok_and(|m| m.encode() == bytes),
        Message::RefUpdate => record::RefUpdate::parse(bytes).is_ok_and(|m| m.encode() == bytes),
        Message::Receipt => record::RecordEntry::parse(bytes).is_ok_and(|m| m.encode() == bytes),
    }
}

#[test]
fn the_first_protocol_number_is_1() {
    assert_eq!(VERSION, 1);
    assert_eq!(first_line(VERSION), "stemma-protocol 1\n");
    assert_eq!(Spoken::here().versions(), [1]);
}

#[test]
fn every_example_is_what_its_writer_writes_and_what_its_reader_reads() {
    for example in examples::ALL {
        assert_eq!(
            example.bytes,
            written(example.name),
            "{} is not what the writer writes",
            example.name
        );
        assert!(
            round_trips(example.message, example.bytes),
            "{} does not round-trip",
            example.name
        );
    }
}

#[test]
fn every_example_is_refused_by_the_readers_of_the_other_messages() {
    let all = [
        Message::RefList,
        Message::FetchRequest,
        Message::ObjectStream,
        Message::Error,
        Message::RefUpdate,
        Message::Receipt,
    ];
    for example in examples::ALL {
        for other in all {
            // An object stream without an object is the first line alone, which is no other
            // message either.
            assert_eq!(
                round_trips(other, example.bytes),
                other == example.message,
                "{} read as {other:?}",
                example.name
            );
        }
    }
}

#[test]
fn the_list_of_references_has_this_text() {
    let main = example_commit();
    let expected = format!(
        "stemma-protocol 1\n\
         repository {REPOSITORY}\n\
         key {}\n\
         former 1 40 {}\n\
         former 41 97 {}\n\
         ref heads/feat/login {} {}\n\
         ref heads/main {main} {}\n\
         ref tags/v0.1.0 {main} {}\n",
        KEYS[0],
        KEYS[1],
        KEYS[2],
        id_text(0x22),
        id_text(0xa2),
        id_text(0xa1),
        id_text(0xa3),
    );
    let list = example_refs();
    assert_eq!(String::from_utf8(list.encode()).unwrap(), expected);
    let read = RefList::parse(expected.as_bytes()).unwrap();
    assert_eq!(read, list);
    assert_eq!(read.repository(), repository());
    assert_eq!(read.key(), &key(0));
    assert_eq!(read.former()[1].first(), 41);
    assert_eq!(read.former()[1].last(), 97);
    assert_eq!(read.former()[1].key(), &key(2));
    assert_eq!(read.references()[1].name().kind(), RefKind::Branch);
    assert_eq!(read.references()[1].name().name().as_str(), "main");
    assert_eq!(read.references()[1].commit(), main);
    assert_eq!(read.references()[1].entry(), record(0xa1));
    assert_eq!(read.references()[2].name().kind(), RefKind::Tag);
}

#[test]
fn the_fetch_request_has_this_text() {
    let mut wanted = [id_text(0x22), example_commit().to_string()];
    wanted.sort();
    let expected = format!(
        "stemma-protocol 1\nwant {}\nwant {}\nhave {}\nhave {}\n",
        wanted[0],
        wanted[1],
        id_text(0x33),
        id_text(0x44),
    );
    let request = example_fetch();
    assert_eq!(String::from_utf8(request.encode()).unwrap(), expected);
    let read = FetchRequest::parse(expected.as_bytes()).unwrap();
    assert_eq!(read, request);
    assert_eq!(read.want().len(), 2);
    assert!(read.have().contains(&commit(0x33)));
}

#[test]
fn the_error_messages_have_this_text() {
    let unsupported = ErrorMessage::UnsupportedVersion(Spoken::here());
    assert_eq!(
        unsupported.encode(),
        b"stemma-protocol 1\nerror unsupported-version\nspoken 1\n"
    );
    let malformed =
        ErrorMessage::Malformed(Reason::try_new("line 2: the line is cut off").unwrap());
    assert_eq!(
        malformed.encode(),
        b"stemma-protocol 1\nerror malformed\nreason line 2: the line is cut off\n"
    );
    for message in [unsupported, malformed] {
        assert_eq!(ErrorMessage::parse(&message.encode()).unwrap(), message);
    }
}

#[test]
fn the_object_stream_has_these_bytes_and_carries_no_id() {
    let bytes = write_stream(&[
        StreamObject {
            kind: Kind::Blob,
            body: b"a\0b".to_vec(),
        },
        StreamObject {
            kind: Kind::Blob,
            body: Vec::new(),
        },
    ]);
    assert_eq!(bytes, b"stemma-protocol 1\nblob 3\0a\0bblob 0\0");
    let objects = read_stream(&bytes, 3).unwrap();
    assert_eq!(objects.len(), 2);
    // The ID is computed by the receiver, and is that of the store.
    assert_eq!(objects[0].id(), Blob::new(*b"a\0b").id().object_id());
    assert_eq!(objects[1].id(), Blob::new(Vec::new()).id().object_id());
}

#[test]
fn the_objects_of_the_example_stream_are_one_complete_state() {
    let objects = read_stream(written("objects").as_slice(), u64::MAX).unwrap();
    assert_eq!(objects, example_objects());
    let tree = Tree::parse(&objects[1].body).unwrap();
    assert_eq!(tree.entries()[0].id(), objects[0].id());
    let commit = Commit::parse(&objects[2].body).unwrap();
    assert_eq!(commit.tree().object_id(), objects[1].id());
    assert_eq!(commit.id().object_id(), objects[2].id());
}

/// An unknown protocol version is a typed error, and the answer to it names the versions
/// spoken.
#[test]
fn another_protocol_version_is_a_typed_error_that_names_the_versions_spoken() {
    let refs = String::from_utf8(written("refs")).unwrap();
    let fetch = String::from_utf8(written("fetch")).unwrap();
    for found in [2, 7, u32::MAX] {
        let first = format!("stemma-protocol {found}\n");
        let expected = ParseError::UnsupportedVersion { found };
        let other = refs.replacen("stemma-protocol 1\n", &first, 1);
        assert_eq!(RefList::parse(other.as_bytes()).unwrap_err(), expected);
        let other = fetch.replacen("stemma-protocol 1\n", &first, 1);
        assert_eq!(FetchRequest::parse(other.as_bytes()).unwrap_err(), expected);
        // Only the first line is read: a later version may have any lines.
        let other = format!("{first}anything\n");
        assert_eq!(RefList::parse(other.as_bytes()).unwrap_err(), expected);
        assert_eq!(FetchRequest::parse(other.as_bytes()).unwrap_err(), expected);
        assert_eq!(ErrorMessage::parse(other.as_bytes()).unwrap_err(), expected);
        let other = format!("{first}error malformed\nreason x\n");
        assert_eq!(ErrorMessage::parse(other.as_bytes()).unwrap_err(), expected);
        assert!(matches!(
            read_stream(format!("{first}blob 0\0").as_bytes(), 9),
            Err(StreamError::UnsupportedVersion { found: f }) if f == found
        ));

        assert_eq!(
            expected.to_string(),
            format!("protocol version {found} is not spoken here; this side speaks version 1")
        );
        let answer = ErrorMessage::from(&expected);
        assert_eq!(answer, ErrorMessage::UnsupportedVersion(Spoken::here()));
        assert_eq!(
            answer.to_string(),
            "the other side does not speak this protocol version; it speaks: 1"
        );
    }
}

/// The answer of a hub that speaks other versions is read by a client that speaks this one.
#[test]
fn an_unsupported_version_is_read_under_every_number() {
    let message = ErrorMessage::UnsupportedVersion(Spoken::try_new([3, 2, 3]).unwrap());
    let bytes = message.encode();
    assert_eq!(
        bytes,
        b"stemma-protocol 3\nerror unsupported-version\nspoken 2 3\n"
    );
    let read = ErrorMessage::parse(&bytes).unwrap();
    assert_eq!(read, message);
    let ErrorMessage::UnsupportedVersion(spoken) = read else {
        panic!("another kind");
    };
    assert_eq!(spoken.versions(), [2, 3]);
    assert_eq!(
        message.to_string(),
        "the other side does not speak this protocol version; it speaks: 2, 3"
    );

    let refused = |text: &str| ErrorMessage::parse(text.as_bytes()).unwrap_err();
    let at = |line, reason| ParseError::Malformed { line, reason };
    // The first line carries the highest version named, and no other.
    for text in [
        "stemma-protocol 2\nerror unsupported-version\nspoken 2 3\n",
        "stemma-protocol 1\nerror unsupported-version\nspoken 2 3\n",
        "stemma-protocol 4\nerror unsupported-version\nspoken 2 3\n",
    ] {
        assert_eq!(refused(text), at(1, Malformed::NotHighestVersion));
    }
    for (spoken, reason) in [
        ("3 2", Malformed::Order),
        ("3 3", Malformed::Order),
        ("0 3", Malformed::Number),
        ("03", Malformed::Number),
        ("+3", Malformed::Number),
        ("2  3", Malformed::Number),
        ("2 3 ", Malformed::Number),
        ("", Malformed::Number),
        ("4294967296", Malformed::Number),
    ] {
        let text = format!("stemma-protocol 3\nerror unsupported-version\nspoken {spoken}\n");
        assert_eq!(refused(&text), at(3, reason), "{spoken:?}");
    }
    assert_eq!(
        refused("stemma-protocol 3\nerror unsupported-version\n"),
        at(3, Malformed::Expected("spoken"))
    );
    assert_eq!(
        refused("stemma-protocol 3\nerror unsupported-version\nspoken 3\nspoken 3\n"),
        at(4, Malformed::Unexpected)
    );
}

#[test]
fn the_parts_of_an_error_message_refuse_what_the_reader_refuses() {
    assert_eq!(
        Spoken::try_new([]).unwrap_err(),
        ErrorMessageError::NoVersion
    );
    assert_eq!(
        Spoken::try_new([1, 0]).unwrap_err(),
        ErrorMessageError::VersionZero
    );
    for text in ["", "a\nb", "a\rb", "tab\there", "\u{85}", "nul\0"] {
        assert_eq!(
            Reason::try_new(text).unwrap_err(),
            ErrorMessageError::Reason,
            "{text:?}"
        );
        let written = format!("stemma-protocol 1\nerror malformed\nreason {text}\n");
        assert!(
            ErrorMessage::parse(written.as_bytes()).is_err(),
            "{text:?} was read"
        );
    }
    assert_eq!(
        ErrorMessage::parse(b"stemma-protocol 1\nerror malformed\nreason \n").unwrap_err(),
        ParseError::Malformed {
            line: 3,
            reason: Malformed::Reason
        }
    );
    // Spaces and letters of every script are part of a reason, verbatim.
    let reason = Reason::try_new("  Zeile 2: «ungültig»  ").unwrap();
    assert_eq!(reason.as_str(), "  Zeile 2: «ungültig»  ");
    let message = ErrorMessage::Malformed(reason);
    assert_eq!(ErrorMessage::parse(&message.encode()).unwrap(), message);
    // Every message of an error gives a reason.
    assert_eq!(Reason::from_message(&"a\nb\tc").as_str(), "a b c");
    assert_eq!(Reason::from_message(&"").as_str(), "-");
    for error in [ErrorMessageError::NoVersion, ErrorMessageError::Reason] {
        assert!(!error.to_string().is_empty());
    }
}

#[test]
fn the_answer_to_a_refused_message_carries_the_message_of_the_reader() {
    let error = RefList::parse(b"stemma-protocol 1\nrepository xyz\n").unwrap_err();
    let answer = ErrorMessage::from(&error);
    let ErrorMessage::Malformed(reason) = &answer else {
        panic!("another kind");
    };
    assert_eq!(
        reason.as_str(),
        "line 2: invalid repository ID: a repository ID is 32 bytes long, found 3"
    );
    assert_eq!(ErrorMessage::parse(&answer.encode()).unwrap(), answer);
    assert_eq!(
        answer.to_string(),
        format!("the other side could not read the message: {error}")
    );
}

#[test]
fn an_error_message_refuses_everything_else() {
    let refused = |text: &str| ErrorMessage::parse(text.as_bytes()).unwrap_err();
    let at = |line, reason| ParseError::Malformed { line, reason };
    assert_eq!(
        refused("stemma-protocol 1\n"),
        at(2, Malformed::Expected("error"))
    );
    assert_eq!(
        refused("stemma-protocol 1\nwant x\n"),
        at(2, Malformed::Expected("error"))
    );
    for kind in [
        "quota",
        "Stale",
        "Malformed",
        "malformed ",
        "",
        "unsupported-version x",
    ] {
        assert_eq!(
            refused(&format!("stemma-protocol 1\nerror {kind}\nreason x\n")),
            at(2, Malformed::UnknownError),
            "{kind:?}"
        );
    }
    assert_eq!(
        refused("stemma-protocol 1\nerror malformed\n"),
        at(3, Malformed::Expected("reason"))
    );
    assert_eq!(
        refused("stemma-protocol 1\nerror malformed\nspoken 1\n"),
        at(3, Malformed::Expected("reason"))
    );
    assert_eq!(
        refused("stemma-protocol 1\nerror malformed\nreason x\nreason y\n"),
        at(4, Malformed::Unexpected)
    );
    assert_eq!(
        refused("stemma-protocol 1\nerror malformed\nreason x"),
        at(3, Malformed::CutOff)
    );
}

/// The rules of the first line hold for every reader of a small message.
#[test]
fn a_malformed_first_line_is_refused_by_every_reader() {
    let first_line = ParseError::Malformed {
        line: 1,
        reason: Malformed::FirstLine,
    };
    let cut_off = ParseError::Malformed {
        line: 1,
        reason: Malformed::CutOff,
    };
    let cases = [
        ("", &first_line),
        ("\n", &first_line),
        ("stemma-protocol\n", &first_line),
        ("stemma-protocol \n", &first_line),
        ("stemma-protocol1\n", &first_line),
        ("stemma-protocol 0\n", &first_line),
        ("stemma-protocol 01\n", &first_line),
        ("stemma-protocol +1\n", &first_line),
        ("stemma-protocol -1\n", &first_line),
        ("stemma-protocol 1 \n", &first_line),
        ("stemma-protocol  1\n", &first_line),
        ("stemma-protocol 1\r\n", &first_line),
        ("stemma-protocol 4294967296\n", &first_line),
        ("Stemma-Protocol 1\n", &first_line),
        (" stemma-protocol 1\n", &first_line),
        ("stemma-oplog 1\n", &first_line),
        ("stemma-protocol 1", &cut_off),
        ("stemma-prot", &cut_off),
    ];
    for (text, expected) in cases {
        let bytes = text.as_bytes();
        assert_eq!(&RefList::parse(bytes).unwrap_err(), expected, "{text:?}");
        assert_eq!(
            &FetchRequest::parse(bytes).unwrap_err(),
            expected,
            "{text:?}"
        );
        assert_eq!(
            &ErrorMessage::parse(bytes).unwrap_err(),
            expected,
            "{text:?}"
        );
        assert!(
            matches!(read_stream(bytes, 9), Err(StreamError::FirstLine)),
            "{text:?}"
        );
    }
    let not_utf8 = ParseError::NotUtf8 { valid_up_to: 18 };
    let bytes = b"stemma-protocol 1\n\xff\n";
    assert_eq!(RefList::parse(bytes).unwrap_err(), not_utf8);
    assert_eq!(FetchRequest::parse(bytes).unwrap_err(), not_utf8);
    assert_eq!(ErrorMessage::parse(bytes).unwrap_err(), not_utf8);
}

/// The text of the example list, to change one place at a time.
fn refs_text() -> String {
    String::from_utf8(written("refs")).unwrap()
}

#[test]
fn a_list_of_references_refuses_lines_that_are_missing_unknown_or_out_of_order() {
    let text = refs_text();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    assert_eq!(lines.len(), 8);
    let refused = |lines: &[&str]| RefList::parse(lines.concat().as_bytes()).unwrap_err();
    let at = |line, reason| ParseError::Malformed { line, reason };

    // Without the repository, without the key.
    assert_eq!(
        refused(&lines[..1]),
        at(2, Malformed::Expected("repository"))
    );
    assert_eq!(
        refused(&[lines[0], lines[2]]),
        at(2, Malformed::Expected("repository"))
    );
    assert_eq!(refused(&lines[..2]), at(3, Malformed::Expected("key")));
    assert_eq!(
        refused(&[lines[0], lines[1], lines[3]]),
        at(3, Malformed::Expected("key"))
    );
    // Repeated: the repository, the key.
    assert_eq!(
        refused(&[lines[0], lines[1], lines[1], lines[2]]),
        at(3, Malformed::Expected("key"))
    );
    assert_eq!(
        refused(&[lines[0], lines[1], lines[2], lines[2]]),
        at(4, Malformed::Unexpected)
    );
    // A former key after a reference, the first line again, an unknown keyword, an empty line.
    assert_eq!(
        refused(&[lines[0], lines[1], lines[2], lines[5], lines[3]]),
        at(5, Malformed::Unexpected)
    );
    for extra in [
        "stemma-protocol 1\n",
        "want x\n",
        "\n",
        " \n",
        "ref\n",
        "refs x\n",
    ] {
        let mut with_extra = lines.clone();
        with_extra.push(extra);
        assert_eq!(
            refused(&with_extra),
            at(9, Malformed::Unexpected),
            "{extra:?}"
        );
    }
    // The last line without its line feed, anywhere.
    for end in 1..text.len() {
        if text.as_bytes()[end - 1] == b'\n' {
            continue;
        }
        let error = RefList::parse(&text.as_bytes()[..end]).unwrap_err();
        let line = text[..end].matches('\n').count() + 1;
        assert_eq!(error, at(line, Malformed::CutOff), "cut at {end}");
    }
    // A carriage return is no part of a line end.
    let crlf = text.replace('\n', "\r\n");
    assert!(RefList::parse(crlf.as_bytes()).is_err());
}

#[test]
fn a_list_of_references_refuses_values_that_are_not_canonical() {
    let text = refs_text();
    let at = |line, reason| ParseError::Malformed { line, reason };
    let changed = |from: &str, to: &str| {
        assert!(text.contains(from), "{from:?} is not in the example");
        RefList::parse(text.replacen(from, to, 1).as_bytes()).unwrap_err()
    };

    // The repository ID.
    let upper = REPOSITORY.to_uppercase();
    for (id, source) in [
        (&REPOSITORY[..31], RepositoryIdParseError::WrongLength(31)),
        ("", RepositoryIdParseError::WrongLength(0)),
        (upper.as_str(), RepositoryIdParseError::NotHex(20)),
        (
            "g0112233445566778899aabbccddeeff",
            RepositoryIdParseError::NotHex(0),
        ),
    ] {
        assert_eq!(
            changed(REPOSITORY, id),
            ParseError::InvalidRepositoryId { line: 2, source },
            "{id:?}"
        );
    }
    assert_eq!(
        changed(REPOSITORY, &format!("{REPOSITORY} ")),
        ParseError::InvalidRepositoryId {
            line: 2,
            source: RepositoryIdParseError::WrongLength(33)
        }
    );

    // The key: a comment, a space at either end, another type, no key.
    let current = format!("key {}", KEYS[0]);
    for key in [
        format!("key {} hub", KEYS[0]),
        format!("key {} ", KEYS[0]),
        format!("key  {}", KEYS[0]),
    ] {
        assert_eq!(
            changed(&current, &key),
            at(3, Malformed::KeyForm),
            "{key:?}"
        );
    }
    assert!(matches!(
        changed(
            &current,
            "key ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQCXBd05Px5wE40nIIPlaxp8OFFtQUUxC8OSO9Xc0QM4HmCOn0LWhhBu2vMg/GJKsoq3K4xNrn7n50ImsVh+xsd24RxlC7GSqlN5hKNP15Jv9M+xBCOqjVF7Z2Lh9LSE2BAW5TmX5W5/3m57WaljZXHqFUcROkBIYNXGJoDd5535Sw=="
        ),
        ParseError::InvalidKey {
            line: 3,
            source: KeyError::NotEd25519 { .. }
        }
    ));
    assert!(matches!(
        changed(&current, "key none"),
        ParseError::InvalidKey { line: 3, .. }
    ));
    assert_eq!(changed(&current, "key"), at(3, Malformed::Expected("key")));

    // The former keys: their fields, their numbers, their ranges.
    let first_former = format!("former 1 40 {}", KEYS[1]);
    assert_eq!(
        changed(&first_former, "former 1 40"),
        at(4, Malformed::Fields)
    );
    assert_eq!(changed(&first_former, "former 1"), at(4, Malformed::Fields));
    for range in ["0 40", "01 40", "1 040", "+1 40", "1 -40", "1  40", "a 40"] {
        assert_eq!(
            changed("former 1 40 ", &format!("former {range} ")),
            at(4, Malformed::Number),
            "{range:?}"
        );
    }
    assert_eq!(
        changed("former 1 40 ", "former 1 18446744073709551616 "),
        at(4, Malformed::Number)
    );
    assert_eq!(
        changed("former 1 40 ", "former 41 40 "),
        at(4, Malformed::Range)
    );
    // The second range begins inside the first, at its end, or before it.
    for first in ["40", "39", "1"] {
        assert_eq!(
            changed("former 41 97 ", &format!("former {first} 97 ")),
            at(5, Malformed::Range),
            "{first}"
        );
    }
    // A gap between two ranges is no error, and neither is a range of one entry.
    let gap = text.replacen("former 41 97 ", "former 97 97 ", 1);
    assert!(RefList::parse(gap.as_bytes()).is_ok());
    assert_eq!(
        changed(
            &format!("former 1 40 {}", KEYS[1]),
            &format!("former 1 40 {} x", KEYS[1])
        ),
        at(4, Malformed::KeyForm)
    );

    // The references: their fields, their names, their IDs.
    assert_eq!(
        changed("ref heads/main ", "ref heads/main  "),
        at(7, Malformed::Fields)
    );
    let entry = id_text(0xa1);
    assert_eq!(changed(&format!(" {entry}"), ""), at(7, Malformed::Fields));
    assert_eq!(
        changed(&format!(" {entry}"), &format!(" {entry} ")),
        at(7, Malformed::Fields)
    );
    for name in [
        "main",
        "refs/heads/main",
        "head/main",
        "Heads/main",
        "/main",
    ] {
        assert_eq!(
            changed("ref heads/main ", &format!("ref {name} ")),
            ParseError::InvalidName {
                line: 7,
                source: QualifiedNameError::UnknownKind
            },
            "{name:?}"
        );
    }
    for name in ["heads/", "heads/-x", "tags/a//b", "tags/con"] {
        assert!(
            matches!(
                changed("ref heads/main ", &format!("ref {name} ")),
                ParseError::InvalidName {
                    line: 7,
                    source: QualifiedNameError::Name(_)
                }
            ),
            "{name:?}"
        );
    }
    // A space in a name is a further field.
    assert_eq!(
        changed("ref heads/main ", "ref heads/a b "),
        at(7, Malformed::Fields)
    );
    assert!(matches!(
        changed(&entry, &entry.to_uppercase()),
        ParseError::InvalidRecordHash { line: 7, .. }
    ));
    assert!(matches!(
        changed(&entry, "-"),
        ParseError::InvalidRecordHash { line: 7, .. }
    ));
    let feat = id_text(0x22);
    assert!(matches!(
        changed(&feat, &feat[3..]),
        ParseError::InvalidObjectId { line: 6, .. }
    ));
    assert!(matches!(
        changed(&feat, "-"),
        ParseError::InvalidObjectId { line: 6, .. }
    ));
}

/// The order and the uniqueness that `RefList::try_new` gives are demanded by the reader from a
/// message that was written by hand.
#[test]
fn a_list_of_references_refuses_references_that_are_repeated_or_out_of_order() {
    let text = refs_text();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let (feat, main, tag) = (lines[5], lines[6], lines[7]);
    let head = lines[..5].concat();
    let refused = |references: &[&str]| {
        RefList::parse(format!("{head}{}", references.concat()).as_bytes()).unwrap_err()
    };
    let out_of_order = |line| ParseError::Malformed {
        line,
        reason: Malformed::Order,
    };
    assert_eq!(refused(&[main, feat, tag]), out_of_order(7));
    assert_eq!(refused(&[feat, tag, main]), out_of_order(8));
    assert_eq!(refused(&[tag, feat]), out_of_order(7));
    assert_eq!(refused(&[feat, main, main, tag]), out_of_order(8));
    // The same name at another commit is the same name.
    let moved = main.replace(&example_commit().to_string(), &id_text(0x55));
    assert_eq!(refused(&[feat, main, moved.as_str(), tag]), out_of_order(8));
    // A branch and a tag of the same name are two references.
    let tag_main = main.replace("heads/main", "tags/main");
    assert!(RefList::parse(format!("{head}{feat}{main}{tag_main}{tag}").as_bytes()).is_ok());

    // The writer refuses the same, and sorts what it is given.
    let reference = |text: &str, commit| Reference::new(name(text), commit, record(1));
    assert_eq!(
        RefList::try_new(
            repository(),
            key(0),
            Vec::new(),
            vec![
                reference("heads/main", commit(1)),
                reference("tags/main", commit(1)),
                reference("heads/main", commit(2)),
            ],
        )
        .unwrap_err(),
        RefListError::DuplicateReference(name("heads/main"))
    );
    let former = |first, last| FormerKey::try_new(first, last, key(1));
    assert_eq!(
        former(0, 3).unwrap_err(),
        RefListError::EmptyRange { first: 0, last: 3 }
    );
    assert_eq!(
        former(4, 3).unwrap_err(),
        RefListError::EmptyRange { first: 4, last: 3 }
    );
    assert_eq!(
        RefList::try_new(
            repository(),
            key(0),
            vec![former(10, 20).unwrap(), former(1, 10).unwrap()],
            Vec::new(),
        )
        .unwrap_err(),
        RefListError::OverlappingRanges(10)
    );
    for error in [
        RefListError::EmptyRange { first: 4, last: 3 },
        RefListError::OverlappingRanges(10),
        RefListError::DuplicateReference(name("heads/main")),
    ] {
        assert!(!error.to_string().is_empty());
    }
}

#[test]
fn qualified_names_are_ordered_by_their_text() {
    let mut names: Vec<QualifiedName> = [
        "tags/a",
        "heads/z",
        "heads/a/b",
        "heads/a.b",
        "heads/Z",
        "tags/Z",
        "heads/a-b",
    ]
    .map(name)
    .into();
    names.sort();
    let sorted: Vec<String> = names.iter().map(ToString::to_string).collect();
    let mut by_text = sorted.clone();
    by_text.sort();
    assert_eq!(sorted, by_text);
    assert_eq!(sorted.first().unwrap(), "heads/Z");
    assert_eq!(sorted.last().unwrap(), "tags/a");
    let built = QualifiedName::new(RefKind::Tag, BranchName::try_new("v1").unwrap());
    assert_eq!(built, name("tags/v1"));
    assert_eq!(built.to_string(), "tags/v1");
}

#[test]
fn a_repository_id_has_one_text() {
    let id = repository();
    assert_eq!(id.to_string(), REPOSITORY);
    assert_eq!(format!("{id:?}"), format!("RepositoryId({REPOSITORY})"));
    assert_eq!(RepositoryId::from_bytes(*id.as_bytes()), id);
    assert_eq!(id.as_bytes()[1], 0x11);
    for error in [
        RepositoryIdParseError::WrongLength(3),
        RepositoryIdParseError::NotHex(3),
    ] {
        assert!(error.to_string().contains('3'));
    }
}

#[test]
fn a_fetch_request_refuses_everything_else() {
    let (a, b, c) = (id_text(0x11), id_text(0x22), id_text(0x33));
    let refused = |body: &str| {
        FetchRequest::parse(format!("stemma-protocol 1\n{body}").as_bytes()).unwrap_err()
    };
    let at = |line, reason| ParseError::Malformed { line, reason };

    // A request wants at least one commit; the writer refuses the same.
    assert_eq!(refused(""), at(2, Malformed::Expected("want")));
    assert_eq!(
        refused(&format!("have {a}\n")),
        at(2, Malformed::Expected("want"))
    );
    assert_eq!(
        FetchRequest::try_new([], [commit(1)]).unwrap_err(),
        FetchRequestError::NothingWanted
    );
    assert!(!FetchRequestError::NothingWanted.to_string().is_empty());

    // Each set in the order of the IDs, each commit once; the writer sorts and counts once.
    assert_eq!(
        refused(&format!("want {b}\nwant {a}\n")),
        at(3, Malformed::Order)
    );
    assert_eq!(
        refused(&format!("want {a}\nwant {a}\n")),
        at(3, Malformed::Order)
    );
    assert_eq!(
        refused(&format!("want {a}\nhave {c}\nhave {b}\n")),
        at(4, Malformed::Order)
    );
    assert_eq!(
        refused(&format!("want {a}\nhave {b}\nhave {b}\n")),
        at(4, Malformed::Order)
    );
    let request = FetchRequest::try_new(
        [commit(0x22), commit(0x11), commit(0x22)],
        [commit(0x33); 2],
    )
    .unwrap();
    assert_eq!(
        request.encode(),
        format!("stemma-protocol 1\nwant {a}\nwant {b}\nhave {c}\n").as_bytes()
    );
    // The same commit may be wanted and had.
    let both = format!("stemma-protocol 1\nwant {a}\nhave {a}\n");
    assert_eq!(
        FetchRequest::parse(both.as_bytes()).unwrap().encode(),
        both.as_bytes()
    );

    // A `want` after a `have`, an unknown keyword, an empty line, further fields.
    assert_eq!(
        refused(&format!("want {a}\nhave {b}\nwant {c}\n")),
        at(4, Malformed::Unexpected)
    );
    for extra in ["done\n", "\n", "want\n", "have\n", "ref heads/main\n"] {
        assert_eq!(
            refused(&format!("want {a}\n{extra}")),
            at(3, Malformed::Unexpected),
            "{extra:?}"
        );
    }
    for value in [
        format!("{a} "),
        format!(" {a}"),
        format!("{a} {b}"),
        a.to_uppercase(),
        a[3..].to_owned(),
        "-".to_owned(),
        String::new(),
    ] {
        assert!(
            matches!(
                refused(&format!("want {value}\n")),
                ParseError::InvalidObjectId { line: 2, .. }
            ),
            "{value:?}"
        );
        assert!(
            matches!(
                refused(&format!("want {a}\nhave {value}\n")),
                ParseError::InvalidObjectId { line: 3, .. }
            ),
            "{value:?}"
        );
    }
    assert_eq!(refused(&format!("want {a}")), at(2, Malformed::CutOff));
    assert_eq!(
        refused(&format!("want {a}\nhave {b}")),
        at(3, Malformed::CutOff)
    );
}

/// A stream that is cut anywhere gives the objects that arrived whole and then reports the cut,
/// except between two frames, where a stream without a count is complete.
#[test]
fn a_stream_that_is_cut_reports_the_cut_unless_it_ends_between_two_frames() {
    let objects = example_objects();
    let bytes = write_stream(&objects);
    let mut boundaries = vec![first_line(VERSION).len()];
    for object in &objects {
        let frame = object.kind.header(object.body.len()).len() + object.body.len();
        boundaries.push(boundaries.last().unwrap() + frame);
    }
    assert_eq!(*boundaries.last().unwrap(), bytes.len());

    for end in 0..=bytes.len() {
        let cut = &bytes[..end];
        let whole = boundaries.iter().filter(|&&b| b <= end).count();
        match ObjectStreamReader::new(cut, u64::MAX) {
            Err(StreamError::FirstLine) => assert_eq!(whole, 0, "cut at {end}"),
            Err(other) => panic!("cut at {end}: {other}"),
            Ok(mut reader) => {
                let mut read = Vec::new();
                let last = loop {
                    match reader.next_object() {
                        Ok(Some(object)) => read.push(object),
                        last => break last,
                    }
                };
                assert_eq!(read, objects[..whole - 1], "cut at {end}");
                if boundaries.contains(&end) {
                    assert!(matches!(last, Ok(None)), "cut at {end}");
                } else {
                    let expected = whole as u64;
                    assert!(
                        matches!(last, Err(StreamError::CutOff { object }) if object == expected),
                        "cut at {end}: {last:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_stream_refuses_frames_that_its_writer_does_not_write() {
    let refused = |frames: &[u8]| {
        let mut bytes = first_line(VERSION).into_bytes();
        bytes.extend_from_slice(frames);
        read_stream(&bytes, 16).unwrap_err()
    };
    // The length: a leading zero, a sign, no digits, not a number, more than 64 bits.
    for header in [
        "blob 01",
        "blob 00",
        "blob +1",
        "blob -1",
        "blob ",
        "blob x",
        "blob 1 ",
        "blob  1",
        "blob 0x1",
        "blob 18446744073709551616",
    ] {
        let frame = format!("{header}\0a");
        assert!(
            matches!(refused(frame.as_bytes()), StreamError::Header { object: 1 }),
            "{header:?}"
        );
    }
    // The shape: no space, not UTF-8, a line feed in place of the NUL byte, a header without
    // an end.
    assert!(matches!(
        refused(b"blob\0"),
        StreamError::Header { object: 1 }
    ));
    assert!(matches!(
        refused(b"blob\xff 1\0a"),
        StreamError::Header { object: 1 }
    ));
    assert!(matches!(
        refused(b"blob 1\na"),
        StreamError::CutOff { object: 1 }
    ));
    assert!(matches!(
        refused(&[b'a'; MAX_FRAME_HEADER + 1]),
        StreamError::Header { object: 1 }
    ));
    assert!(matches!(
        refused(&[b'a'; MAX_FRAME_HEADER]),
        StreamError::CutOff { object: 1 }
    ));
    // The kind: unknown to this version, another case, with a space in front.
    for kind in ["commit3", "Blob", "", "blob\n", "pack"] {
        let frame = format!("{kind} 1\0a");
        match refused(frame.as_bytes()) {
            StreamError::UnknownKind {
                object: 1,
                kind: found,
            } => assert_eq!(found, kind),
            other => panic!("{kind:?}: {other}"),
        }
    }
    // An object of the store with its ID in front, as no frame carries it.
    let with_id = format!("{} blob 1\0a", id_text(1));
    assert!(matches!(
        refused(with_id.as_bytes()),
        StreamError::Header { object: 1 }
    ));
    // The second object is counted as the second.
    assert!(matches!(
        refused(b"blob 1\0atree 01\0a"),
        StreamError::Header { object: 2 }
    ));
    // A body that is shorter than its length says, and one that is longer.
    assert!(matches!(
        refused(b"blob 2\0a"),
        StreamError::CutOff { object: 1 }
    ));
    assert!(matches!(
        refused(b"blob 1\0ab"),
        StreamError::CutOff { object: 2 }
    ));
    // A line feed after the last frame is the beginning of a frame that does not end.
    assert!(matches!(
        refused(b"blob 1\0a\n"),
        StreamError::CutOff { object: 2 }
    ));
}

#[test]
fn a_stream_refuses_an_object_above_the_bound_before_it_reads_the_body() {
    let bytes = write_stream(&[
        StreamObject {
            kind: Kind::Blob,
            body: vec![b'a'; 16],
        },
        StreamObject {
            kind: Kind::Tree,
            body: vec![b'a'; 17],
        },
    ]);
    assert_eq!(read_stream(&bytes, 17).unwrap().len(), 2);
    let mut reader = ObjectStreamReader::new(bytes.as_slice(), 16).unwrap();
    assert_eq!(reader.next_object().unwrap().unwrap().body.len(), 16);
    let error = reader.next_object().unwrap_err();
    assert!(matches!(
        error,
        StreamError::TooLarge {
            object: 2,
            length: 17,
            limit: 16
        }
    ));
    assert_eq!(
        error.to_string(),
        "object 2: 17 bytes are more than the limit of 16 bytes"
    );
    // A length that no memory holds is refused by the bound, and without a bound by the end of
    // the stream; nothing is reserved for it.
    let lying = b"stemma-protocol 1\nblob 18446744073709551615\0a";
    assert!(matches!(
        read_stream(lying, 1 << 20),
        Err(StreamError::TooLarge { object: 1, .. })
    ));
    assert!(matches!(
        read_stream(lying, u64::MAX),
        Err(StreamError::CutOff { object: 1 })
    ));
}

/// A reader that fails after the bytes it was given.
struct Failing;

impl io::Read for Failing {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("the connection is gone"))
    }
}

impl io::Write for Failing {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("the connection is gone"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_failure_of_the_carrier_is_reported_as_such() {
    let bytes = write_stream(&example_objects());
    // In the first line, in a header, and in a body.
    for end in [5, first_line(VERSION).len() + 2, bytes.len() - 3] {
        let carrier = Cursor::new(&bytes[..end]).chain(BufReader::new(Failing));
        let error = ObjectStreamReader::new(carrier, u64::MAX)
            .and_then(|mut reader| {
                while reader.next_object()?.is_some() {}
                Ok(())
            })
            .unwrap_err();
        assert!(error.io_error().is_some(), "cut at {end}: {error}");
        assert_eq!(
            error.to_string(),
            "cannot read the object stream: the connection is gone"
        );
        crate::fuzz::check_rejection(&error);
    }
    assert!(StreamError::FirstLine.io_error().is_none());

    assert!(ObjectStreamWriter::new(Failing).is_err());
    let mut failing = ObjectStreamWriter { writer: Failing };
    assert!(failing.write(Kind::Blob, b"a").is_err());
}

/// The messages of every refusal are complete and their chains end (crate documentation).
#[test]
fn every_refusal_has_a_message_and_a_chain_that_ends() {
    let name_error = "heads/-x".parse::<QualifiedName>().unwrap_err();
    assert_eq!(
        name_error.to_string(),
        "invalid branch name `-x`: a component starts with `.` or `-`"
    );
    crate::fuzz::check_rejection(&name_error);
    crate::fuzz::check_rejection(&QualifiedNameError::UnknownKind);
    let refs = refs_text();
    let inputs = [
        refs.replacen(REPOSITORY, "x", 1),
        refs.replacen(KEYS[0], "ssh-ed25519 AAAA", 1),
        refs.replacen("heads/main", "heads/-x", 1),
        refs.replacen("heads/main", "main", 1),
        refs.replacen(&id_text(0x22), "b3:x", 1),
        refs.replacen(&id_text(0xa1), "b3:x", 1),
        refs.replacen("former 1 ", "former 0 ", 1),
        "stemma-protocol 2\n".to_owned(),
        "\u{feff}".to_owned(),
    ];
    for input in &inputs {
        let error = RefList::parse(input.as_bytes()).unwrap_err();
        crate::fuzz::check_rejection(&error);
        crate::fuzz::protocol(input.as_bytes());
    }
    assert!(RefList::parse(b"\xff")
        .unwrap_err()
        .to_string()
        .contains("UTF-8"));
    for reason in [
        Malformed::CutOff,
        Malformed::FirstLine,
        Malformed::Expected("key"),
        Malformed::Unexpected,
        Malformed::Fields,
        Malformed::Number,
        Malformed::Range,
        Malformed::Order,
        Malformed::KeyForm,
        Malformed::UnknownError,
        Malformed::NotHighestVersion,
        Malformed::Reason,
    ] {
        let error = malformed(3, reason);
        assert!(error.to_string().starts_with("line 3: "), "{error}");
        crate::fuzz::check_rejection(&error);
    }
    for error in [
        StreamError::FirstLine,
        StreamError::UnsupportedVersion { found: 2 },
        StreamError::CutOff { object: 1 },
        StreamError::Header { object: 1 },
        StreamError::UnknownKind {
            object: 1,
            kind: "a\nb".to_owned(),
        },
    ] {
        assert!(!error.to_string().contains('\n'), "{error}");
        crate::fuzz::check_rejection(&error);
    }
}

/// Pieces that change a message: separators, signs, digits, and the keywords of the messages.
const PIECES: &[&str] = &[
    " ",
    "  ",
    "\n",
    "\n\n",
    "\r",
    "\r\n",
    "\t",
    "\0",
    "+",
    "-",
    "0",
    "00",
    "1",
    "2",
    "9",
    "a",
    "f",
    "A",
    "/",
    "b3:",
    "heads/",
    "tags/",
    "ref ",
    "former ",
    "key ",
    "repository ",
    "want ",
    "have ",
    "error ",
    "spoken ",
    "reason ",
    "malformed",
    "unsupported-version",
    "stemma-protocol ",
    "stemma-protocol 1\n",
    "stemma-protocol 2\n",
    "blob ",
    "tree 0\0",
    "commit2 ",
    "ssh-ed25519 ",
    "\u{e9}",
    "\u{feff}",
];

/// Changes a message in one to three places: a piece is put in, a part is cut out, a byte is
/// changed, or two parts change places.
fn changed(rng: &mut Rng, message: &[u8]) -> Vec<u8> {
    let mut bytes = message.to_vec();
    for _ in 0..=rng.below(3) {
        let at = rng.below(bytes.len() + 1);
        match rng.below(5) {
            0 => {
                let piece = rng.pick(PIECES).as_bytes();
                bytes.splice(at..at, piece.iter().copied());
            }
            1 => {
                let end = (at + 1 + rng.below(8)).min(bytes.len());
                bytes.drain(at..end);
            }
            2 if at < bytes.len() => bytes[at] = rng.next() as u8,
            3 if !bytes.is_empty() => {
                // A whole line is repeated or moves to another place.
                let lines: Vec<&[u8]> = bytes.split_inclusive(|&b| b == b'\n').collect();
                let line = rng.pick(&lines).to_vec();
                let to = rng.below(bytes.len() + 1);
                bytes.splice(to..to, line);
            }
            _ => bytes.truncate(at),
        }
    }
    bytes
}

/// Generated input: every example, changed in a few places, either stays a message that its
/// reader writes back unchanged or is refused with a message; no reader panics.
#[test]
fn changed_examples_are_refused_or_read_back_unchanged() {
    let mut rng = Rng::new(seed(), 283);
    let cases = cases();
    for example in examples::ALL {
        for end in 0..=example.bytes.len() {
            crate::fuzz::protocol(&example.bytes[..end]);
        }
        for _ in 0..cases / examples::ALL.len() {
            crate::fuzz::protocol(&changed(&mut rng, example.bytes));
        }
    }
}

/// A list of references built from generated parts.
fn generated_refs(rng: &mut Rng) -> RefList {
    let mut bytes = [0u8; 16];
    bytes.fill_with(|| rng.next() as u8);
    let mut former = Vec::new();
    let mut next = 1 + rng.below(3) as u64;
    for _ in 0..rng.below(4) {
        let last = next + rng.below(50) as u64;
        former.push(FormerKey::try_new(next, last, key(rng.below(3))).unwrap());
        next = last + 1 + rng.below(3) as u64;
    }
    let names = [
        "heads/main",
        "heads/a",
        "heads/a.b",
        "heads/a/b",
        "heads/Z",
        "heads/feat/TF-792-login",
        "tags/main",
        "tags/v0.1.0",
        "tags/v0.1.0-rc.1",
        "tags/a",
    ];
    let mut references = Vec::new();
    for text in names {
        if rng.one_in(2) {
            let (commit, entry) = (commit(rng.next() as u8), record(rng.next() as u8));
            references.push(Reference::new(name(text), commit, entry));
        }
    }
    RefList::try_new(
        RepositoryId::from_bytes(bytes),
        key(rng.below(3)),
        former,
        references,
    )
    .unwrap()
}

/// One value, one encoding: what a writer writes for a generated value, its reader reads as that
/// value.
#[test]
fn generated_messages_round_trip() {
    let mut rng = Rng::new(seed(), 284);
    for _ in 0..cases() / 8 {
        let list = generated_refs(&mut rng);
        assert_eq!(RefList::parse(&list.encode()).unwrap(), list);

        let commits = |rng: &mut Rng, least: usize| -> Vec<CommitId> {
            (0..least + rng.below(6))
                .map(|_| commit(rng.next() as u8))
                .collect()
        };
        let request = FetchRequest::try_new(commits(&mut rng, 1), commits(&mut rng, 0)).unwrap();
        assert_eq!(FetchRequest::parse(&request.encode()).unwrap(), request);

        let spoken = Spoken::try_new((0..1 + rng.below(4)).map(|_| 1 + rng.below(9) as u32));
        let message = ErrorMessage::UnsupportedVersion(spoken.unwrap());
        assert_eq!(ErrorMessage::parse(&message.encode()).unwrap(), message);
        let reason: String = (0..1 + rng.below(6)).map(|_| *rng.pick(PIECES)).collect();
        let message = ErrorMessage::Malformed(Reason::from_message(&reason));
        assert_eq!(ErrorMessage::parse(&message.encode()).unwrap(), message);

        let kinds = [Kind::Blob, Kind::Tree, Kind::Commit, Kind::Commit2];
        let objects: Vec<StreamObject> = (0..rng.below(5))
            .map(|_| StreamObject {
                kind: *rng.pick(&kinds),
                body: (0..rng.below(40)).map(|_| rng.next() as u8).collect(),
            })
            .collect();
        assert_eq!(read_stream(&write_stream(&objects), 40).unwrap(), objects);
    }
}

/// The version is judged before anything else of a message: a later version may hold bytes and
/// lines that this one cannot read, and its sender must still learn the versions spoken.
#[test]
fn another_version_is_refused_as_such_whatever_follows_the_first_line() {
    let expected = ParseError::UnsupportedVersion { found: 2 };
    let rests: [&[u8]; 6] = [
        b"",
        b"\xff\n",
        b"cut off",
        b"error malformed\nreason \xff\n",
        b"error",
        b"\0\0\0",
    ];
    for rest in rests {
        let bytes = [b"stemma-protocol 2\n".as_slice(), rest].concat();
        assert_eq!(RefList::parse(&bytes).unwrap_err(), expected, "{rest:?}");
        assert_eq!(
            FetchRequest::parse(&bytes).unwrap_err(),
            expected,
            "{rest:?}"
        );
        assert_eq!(
            ErrorMessage::parse(&bytes).unwrap_err(),
            expected,
            "{rest:?}"
        );
        assert!(
            matches!(
                read_stream(&bytes, 9),
                Err(StreamError::UnsupportedVersion { found: 2 })
            ),
            "{rest:?}"
        );
        assert_eq!(
            ErrorMessage::from(&expected),
            ErrorMessage::UnsupportedVersion(Spoken::here())
        );
    }
    // Under the version spoken here the same rests are what they are.
    let at = |line, reason| ParseError::Malformed { line, reason };
    assert_eq!(
        ErrorMessage::parse(b"stemma-protocol 1\ncut off").unwrap_err(),
        at(2, Malformed::CutOff)
    );
    assert_eq!(
        ErrorMessage::parse(b"stemma-protocol 1\nerror malformed\nreason \xff\n").unwrap_err(),
        ParseError::NotUtf8 { valid_up_to: 41 }
    );
    // An `unsupported-version` of another number is still read strictly.
    assert_eq!(
        ErrorMessage::parse(b"stemma-protocol 2\nerror unsupported-version\nspoken 2").unwrap_err(),
        at(3, Malformed::CutOff)
    );
    // A first line that is not UTF-8 is no first line of any version.
    assert_eq!(
        RefList::parse(b"stemma-protocol \xff\nx\n").unwrap_err(),
        ParseError::NotUtf8 { valid_up_to: 16 }
    );
    assert!(matches!(
        read_stream(b"stemma-protocol \xff\n", 9),
        Err(StreamError::FirstLine)
    ));
}

/// A reader that has refused a stream does not go on: what follows a refused frame is not read
/// as frames, and a stream that was cut does not look complete on the next call.
#[test]
fn a_stream_reader_that_failed_stays_failed() {
    // A body above the bound that holds a frame of its own.
    let smuggled = b"stemma-protocol 1\nblob 20\0blob 1\0atree 1\0bxxxxx";
    let mut reader = ObjectStreamReader::new(smuggled.as_slice(), 8).unwrap();
    assert!(matches!(
        reader.next_object(),
        Err(StreamError::TooLarge { object: 1, .. })
    ));
    for _ in 0..3 {
        assert!(matches!(reader.next_object(), Err(StreamError::Failed)));
    }
    // A stream that is cut inside a body is at its end afterwards.
    let mut reader =
        ObjectStreamReader::new(b"stemma-protocol 1\nblob 2\0a".as_slice(), 8).unwrap();
    assert!(matches!(
        reader.next_object(),
        Err(StreamError::CutOff { object: 1 })
    ));
    assert!(matches!(reader.next_object(), Err(StreamError::Failed)));
    // The same after a header and a kind that are refused.
    for frames in [b"blob 01\0a".as_slice(), b"pack 1\0a"] {
        let bytes = [b"stemma-protocol 1\n".as_slice(), frames].concat();
        let mut reader = ObjectStreamReader::new(bytes.as_slice(), 8).unwrap();
        assert!(reader.next_object().is_err());
        assert!(matches!(reader.next_object(), Err(StreamError::Failed)));
    }
    // The end of a stream is no failure: it is reported again.
    let mut reader = ObjectStreamReader::new(b"stemma-protocol 1\n".as_slice(), 8).unwrap();
    assert!(matches!(reader.next_object(), Ok(None)));
    assert!(matches!(reader.next_object(), Ok(None)));
    assert_eq!(
        StreamError::Failed.to_string(),
        "an earlier read of the object stream failed, so the stream is lost"
    );
    crate::fuzz::check_rejection(&StreamError::Failed);
}

#[test]
fn a_refused_stream_has_an_answer_where_this_version_knows_one() {
    assert_eq!(
        StreamError::UnsupportedVersion { found: 2 }.answer(),
        Some(ErrorMessage::UnsupportedVersion(Spoken::here()))
    );
    for error in [
        StreamError::FirstLine,
        StreamError::CutOff { object: 2 },
        StreamError::Header { object: 2 },
        StreamError::UnknownKind {
            object: 2,
            kind: "a\nb".to_owned(),
        },
    ] {
        let answer = error.answer().unwrap();
        assert_eq!(
            answer,
            ErrorMessage::Malformed(Reason::try_new(error.to_string()).unwrap())
        );
        assert_eq!(ErrorMessage::parse(&answer.encode()).unwrap(), answer);
    }
    for error in [
        StreamError::Io(io::Error::other("gone")),
        StreamError::Failed,
        StreamError::TooLarge {
            object: 1,
            length: 9,
            limit: 8,
        },
    ] {
        assert_eq!(error.answer(), None, "{error}");
    }
    assert_eq!(
        StreamError::Header { object: 2 }.to_string(),
        "object 2: the frame does not begin with a header `<kind> <length>` of at most 64 bytes \
         and a NUL byte"
    );
}

/// A sender that never ends a line or a header does not make the reader hold what it sends.
#[test]
fn the_stream_reader_reads_a_bounded_first_line_and_a_bounded_header() {
    let endless = || BufReader::new(io::repeat(b'a'));
    assert!(matches!(
        ObjectStreamReader::new(endless(), u64::MAX),
        Err(StreamError::FirstLine)
    ));
    let mut carrier = Cursor::new(vec![b'a'; 1000]);
    assert!(ObjectStreamReader::new(&mut carrier, u64::MAX).is_err());
    assert_eq!(carrier.position(), MAX_FIRST_LINE as u64 + 1);

    let carrier = Cursor::new(first_line(VERSION)).chain(endless());
    let mut reader = ObjectStreamReader::new(carrier, u64::MAX).unwrap();
    assert!(matches!(
        reader.next_object(),
        Err(StreamError::Header { object: 1 })
    ));
}

/// A carriage return at the end of any line is refused by the value of that line.
#[test]
fn a_carriage_return_is_refused_on_every_line_of_a_list_of_references() {
    let text = refs_text();
    let lines: Vec<&str> = text.lines().collect();
    let with_cr = |at: usize| {
        let changed: String = lines
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{line}{}\n", if i == at { "\r" } else { "" }))
            .collect();
        RefList::parse(changed.as_bytes()).unwrap_err()
    };
    let key_form = |line| ParseError::Malformed {
        line,
        reason: Malformed::KeyForm,
    };
    assert_eq!(
        with_cr(1),
        ParseError::InvalidRepositoryId {
            line: 2,
            source: RepositoryIdParseError::WrongLength(33)
        }
    );
    assert_eq!(with_cr(2), key_form(3));
    assert_eq!(with_cr(3), key_form(4));
    assert_eq!(with_cr(4), key_form(5));
    for at in 5..8 {
        assert!(
            matches!(with_cr(at), ParseError::InvalidRecordHash { line, .. } if line == at + 1),
            "line {}",
            at + 1
        );
    }
    // A tab is trimmed by the reader of a key and refused by the comparison with its text.
    let tab = text.replacen(
        &format!("key {}\n", KEYS[0]),
        &format!("key {}\t\n", KEYS[0]),
        1,
    );
    assert_eq!(RefList::parse(tab.as_bytes()).unwrap_err(), key_form(3));
}

/// A reason is one line for a terminal too; the reader names the rule that fired.
#[test]
fn a_reason_is_refused_by_the_rule_that_it_breaks() {
    let read = |reason: &str| {
        let text = format!("stemma-protocol 1\nerror malformed\nreason {reason}\n");
        ErrorMessage::parse(text.as_bytes())
    };
    let at = |line, reason| ParseError::Malformed { line, reason };
    for reason in [
        "a\rb",
        "tab\there",
        "\u{85}",
        "nul\0",
        "a\u{2028}b",
        "a\u{2029}b",
        "",
    ] {
        assert_eq!(
            read(reason).unwrap_err(),
            at(3, Malformed::Reason),
            "{reason:?}"
        );
        assert_eq!(
            Reason::try_new(reason).unwrap_err(),
            ErrorMessageError::Reason,
            "{reason:?}"
        );
    }
    // A line feed ends the reason, and what follows is a line that does not belong there.
    assert_eq!(read("a\nb").unwrap_err(), at(4, Malformed::Unexpected));
    assert_eq!(
        Reason::from_message(&"a\u{2028}b\u{2029}c").as_str(),
        "a b c"
    );
    // Marks of the text direction are part of a reason, as the type says.
    assert!(read("a\u{202e}b").is_ok());
    assert!(!ErrorMessageError::VersionZero.to_string().is_empty());
}

#[test]
fn the_edges_of_ranges_and_of_ids_are_exact() {
    let former = |first, last| FormerKey::try_new(first, last, key(1)).unwrap();
    assert_eq!(former(5, 5).first(), 5);
    assert_eq!(
        RefList::try_new(
            repository(),
            key(0),
            vec![former(5, 20), former(1, 10)],
            Vec::new()
        )
        .unwrap_err(),
        RefListError::OverlappingRanges(5)
    );
    assert_eq!(
        "0g112233445566778899aabbccddeeff".parse::<RepositoryId>(),
        Err(RepositoryIdParseError::NotHex(1))
    );
    assert_eq!(
        "é".repeat(16).parse::<RepositoryId>(),
        Err(RepositoryIdParseError::NotHex(0))
    );
    // An invalid key on a `former` line is reported at that line.
    let text = refs_text().replacen(KEYS[2], "ssh-ed25519 AAAA", 1);
    assert!(matches!(
        RefList::parse(text.as_bytes()).unwrap_err(),
        ParseError::InvalidKey { line: 5, .. }
    ));
}

/// The message of a refusal holds no control character that the other side put into a key.
#[test]
fn the_text_of_a_refused_key_is_escaped() {
    // The blob of this key names the algorithm `ESC [31mevil`.
    let text = refs_text().replacen(
        KEYS[0],
        "ssh-ed25519 AAAACRtbMzFtZXZpbAAAACAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==",
        1,
    );
    let error = RefList::parse(text.as_bytes()).unwrap_err();
    assert!(matches!(error, ParseError::InvalidKey { line: 3, .. }));
    let message = error.to_string();
    assert!(message.contains("\\u{1b}[31mevil"), "{message:?}");
    assert!(!message.chars().any(char::is_control), "{message:?}");
}

/// A carrier that reports an end and has more later is asked again: the end of a stream is no
/// state of the reader.
#[test]
fn a_call_after_the_end_of_a_stream_asks_the_carrier_again() {
    struct Parts(Vec<&'static [u8]>);
    impl io::Read for Parts {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let part = if self.0.is_empty() {
                &[][..]
            } else {
                self.0.remove(0)
            };
            buffer[..part.len()].copy_from_slice(part);
            Ok(part.len())
        }
    }
    let carrier = BufReader::new(Parts(vec![b"stemma-protocol 1\n", b"", b"blob 1\0a"]));
    let mut reader = ObjectStreamReader::new(carrier, 8).unwrap();
    assert!(matches!(reader.next_object(), Ok(None)));
    assert_eq!(reader.next_object().unwrap().unwrap().body, b"a");
    assert!(matches!(reader.next_object(), Ok(None)));
}

/// The bound is the memory bound of the receiver: nothing of a body above it is taken from the
/// carrier.
#[test]
fn a_body_above_the_bound_is_not_read_from_the_carrier() {
    let header = b"stemma-protocol 1\nblob 9\0";
    let mut carrier = Cursor::new([header.as_slice(), b"123456789"].concat());
    let mut reader = ObjectStreamReader::new(&mut carrier, 8).unwrap();
    assert!(matches!(
        reader.next_object(),
        Err(StreamError::TooLarge { object: 1, .. })
    ));
    assert_eq!(carrier.position(), header.len() as u64);
}

/// A carrier that fails once and works again does not get the reader back into the stream: the
/// bytes of the read that failed are gone.
#[test]
fn a_reader_stays_failed_after_a_failure_of_the_carrier() {
    struct FailsOnce(bool, Cursor<&'static [u8]>);
    impl io::Read for FailsOnce {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if std::mem::take(&mut self.0) {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "not yet"));
            }
            self.1.read(buffer)
        }
    }
    let rest = FailsOnce(true, Cursor::new(b"bcblob 1\0d"));
    let carrier =
        Cursor::new(b"stemma-protocol 1\nblob 3\0a".as_slice()).chain(BufReader::new(rest));
    let mut reader = ObjectStreamReader::new(carrier, 8).unwrap();
    assert!(reader.next_object().unwrap_err().io_error().is_some());
    assert!(matches!(reader.next_object(), Err(StreamError::Failed)));
}
