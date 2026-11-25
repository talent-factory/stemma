//! The tests of the two error messages that answer a ref update: their texts, every reason, and
//! the refusals of the reader one rule at a time.

use super::*;
use crate::protocol::{ErrorMessage, RecordHash};

/// The text of an ID whose bytes are all `byte`.
fn id_text(byte: u8) -> String {
    format!("b3:{}", format!("{byte:02x}").repeat(32))
}

fn commit(byte: u8) -> CommitId {
    CommitId::new(id_text(byte).parse().unwrap())
}

fn entry(byte: u8) -> RecordHash {
    id_text(byte).parse().unwrap()
}

fn name(text: &str) -> QualifiedName {
    text.parse().unwrap()
}

fn reason(text: &str) -> Reason {
    Reason::try_new(text).unwrap()
}

fn refused(reference: &str, reason: RefusalReason) -> RefusedReference {
    RefusedReference {
        name: name(reference),
        reason,
    }
}

fn references(references: Vec<RefusedReference>) -> ErrorMessage {
    ErrorMessage::Refused(Refused::References(
        RefusedReferences::try_new(references).unwrap(),
    ))
}

/// The example `error-stale`: a branch that moved on, and a tag that the hub never saw.
pub(crate) fn example_stale() -> ErrorMessage {
    ErrorMessage::Stale(
        Stale::try_new(vec![
            StaleReference {
                name: name("tags/v0.2.0"),
                entry: None,
                commit: None,
            },
            StaleReference {
                name: name("heads/main"),
                entry: Some(entry(0xa2)),
                commit: Some(commit(0x33)),
            },
        ])
        .unwrap(),
    )
}

/// The example `error-refused`: three references, each with another reason.
pub(crate) fn example_refused() -> ErrorMessage {
    references(vec![
        refused(
            "tags/v0.2.0",
            RefusalReason::Denied {
                operation: OperationName::try_new("tag").unwrap(),
            },
        ),
        refused("heads/main", RefusalReason::Gate),
        refused(
            "heads/feature",
            RefusalReason::Leased {
                holder: "human mia".parse().unwrap(),
                expires: 1_767_225_600,
            },
        ),
    ])
}

/// The example `error-refused-update`: the key is not in the register of `main`.
pub(crate) fn example_refused_update() -> ErrorMessage {
    ErrorMessage::Refused(Refused::Key {
        register: commit(0x11),
    })
}

fn round_trip(message: &ErrorMessage) -> String {
    let bytes = message.encode();
    assert_eq!(ErrorMessage::parse(&bytes).as_ref(), Ok(message));
    String::from_utf8(bytes).unwrap()
}

fn parse(lines: &str) -> Result<ErrorMessage, ParseError> {
    ErrorMessage::parse(format!("stemma-protocol 1\n{lines}").as_bytes())
}

fn at(line: usize, reason: Malformed) -> ParseError {
    ParseError::Malformed { line, reason }
}

#[test]
fn a_stale_answer_names_every_reference_with_its_state_in_the_order_of_the_names() {
    let text = round_trip(&example_stale());
    assert_eq!(
        text,
        format!(
            "stemma-protocol 1\nerror stale\nref heads/main {} {}\nref tags/v0.2.0 - -\n",
            id_text(0xa2),
            id_text(0x33)
        )
    );
    // A name that was deleted keeps its entry and has no commit.
    let deleted = ErrorMessage::Stale(
        Stale::try_new(vec![StaleReference {
            name: name("heads/gone"),
            entry: Some(entry(1)),
            commit: None,
        }])
        .unwrap(),
    );
    assert!(round_trip(&deleted).ends_with(&format!("ref heads/gone {} -\n", id_text(1))));
}

#[test]
fn a_stale_answer_is_built_from_at_least_one_reference_each_once_with_a_state_that_exists() {
    let stale = |reference: &str, entry, commit| StaleReference {
        name: name(reference),
        entry,
        commit,
    };
    assert_eq!(Stale::try_new(vec![]), Err(RecordProblem::NoReference));
    assert_eq!(
        Stale::try_new(vec![
            stale("heads/a", None, None),
            stale("heads/a", Some(entry(1)), None)
        ]),
        Err(RecordProblem::DuplicateReference)
    );
    assert_eq!(
        Stale::try_new(vec![stale("heads/a", None, Some(commit(1)))]),
        Err(RecordProblem::OldWithoutPrevious)
    );
}

#[test]
fn the_reader_of_a_stale_answer_refuses_what_the_writer_does_not_write() {
    let (a, b) = (id_text(0xa2), id_text(0x33));
    assert_eq!(
        parse("error stale\n"),
        Err(at(3, Malformed::Expected("ref")))
    );
    assert_eq!(
        parse(&format!("error stale\nref heads/main {a}\n")),
        Err(at(3, Malformed::Fields))
    );
    assert_eq!(
        parse(&format!(
            "error stale\nref heads/b {a} {b}\nref heads/a {a} {b}\n"
        )),
        Err(at(4, Malformed::Order))
    );
    assert_eq!(
        parse(&format!(
            "error stale\nref heads/a {a} {b}\nref heads/a {a} {b}\n"
        )),
        Err(at(4, Malformed::Order))
    );
    assert_eq!(
        parse(&format!("error stale\nref heads/main - {b}\n")),
        Err(ParseError::Record {
            line: 3,
            reason: RecordProblem::OldWithoutPrevious
        })
    );
    assert!(matches!(
        parse(&format!("error stale\nref main {a} {b}\n")),
        Err(ParseError::InvalidName { line: 3, .. })
    ));
    assert!(matches!(
        parse(&format!("error stale\nref heads/main x {b}\n")),
        Err(ParseError::InvalidRecordHash { line: 3, .. })
    ));
    assert!(matches!(
        parse(&format!("error stale\nref heads/main {a} x\n")),
        Err(ParseError::InvalidObjectId { line: 3, .. })
    ));
    assert_eq!(
        parse(&format!(
            "error stale\nref heads/main {a} {b}\nreason more\n"
        )),
        Err(at(4, Malformed::Unexpected))
    );
    assert_eq!(
        parse(&format!("error stale\nref heads/main {a} {b}")),
        Err(at(3, Malformed::CutOff))
    );
}

#[test]
fn a_refusal_of_references_carries_every_reason_with_its_facts() {
    let text = round_trip(&example_refused());
    assert_eq!(
        text,
        "stemma-protocol 1\nerror refused\nref heads/feature leased human mia 1767225600\n\
         ref heads/main gate -\nref tags/v0.2.0 denied tag\n"
    );
    let all = references(vec![
        refused("heads/a", RefusalReason::Reserved),
        refused("heads/b", RefusalReason::NoRegister),
        refused(
            "heads/c",
            RefusalReason::BadRegister(reason("line 2: not sorted")),
        ),
        refused("heads/d", RefusalReason::NotOwner),
        refused("heads/e", RefusalReason::Conflict(reason("E"))),
        refused(
            "heads/f",
            RefusalReason::Incomplete(reason("the tree b3:00 is missing")),
        ),
        refused("heads/g", RefusalReason::Policy(reason("no policy file"))),
        refused(
            "heads/h",
            RefusalReason::Leased {
                holder: "agent bot".parse().unwrap(),
                expires: -1,
            },
        ),
    ]);
    assert_eq!(
        round_trip(&all),
        "stemma-protocol 1\nerror refused\nref heads/a reserved -\nref heads/b no-register -\n\
         ref heads/c bad-register line 2: not sorted\nref heads/d not-owner -\n\
         ref heads/e conflict E\nref heads/f incomplete the tree b3:00 is missing\n\
         ref heads/g policy no policy file\nref heads/h leased agent bot -1\n"
    );
}

#[test]
fn a_refusal_of_the_whole_update_carries_one_line() {
    assert_eq!(
        round_trip(&example_refused_update()),
        format!(
            "stemma-protocol 1\nerror refused\nupdate key {}\n",
            id_text(0x11)
        )
    );
    let hub = ErrorMessage::Refused(Refused::Hub {
        called: HubAddress::try_new("https://hub.example").unwrap(),
    });
    assert!(round_trip(&hub).ends_with("update hub https://hub.example\n"));
    let repository = ErrorMessage::Refused(Refused::Repository {
        id: "00112233445566778899aabbccddeeff".parse().unwrap(),
    });
    assert!(
        round_trip(&repository).ends_with("update repository 00112233445566778899aabbccddeeff\n")
    );
    assert!(round_trip(&ErrorMessage::Refused(Refused::NoMain)).ends_with("update no-main -\n"));
}

#[test]
fn refused_references_are_at_least_one_and_each_once() {
    assert_eq!(
        RefusedReferences::try_new(vec![]),
        Err(RecordProblem::NoReference)
    );
    assert_eq!(
        RefusedReferences::try_new(vec![
            refused("heads/a", RefusalReason::Gate),
            refused("heads/a", RefusalReason::Reserved)
        ]),
        Err(RecordProblem::DuplicateReference)
    );
    let Refused::References(two) = (match example_refused() {
        ErrorMessage::Refused(refused) => refused,
        other => panic!("{other:?}"),
    }) else {
        panic!("references");
    };
    assert_eq!(
        two.reason_of(&name("heads/main")),
        Some(&RefusalReason::Gate)
    );
    assert_eq!(two.reason_of(&name("heads/other")), None);
    // Only missing objects ask the sender to upload again.
    assert!(!Refused::References(two).wants_upload());
    assert!(!Refused::NoMain.wants_upload());
    let incomplete = RefusedReferences::try_new(vec![
        refused("heads/a", RefusalReason::Gate),
        refused("heads/b", RefusalReason::Incomplete(reason("missing"))),
    ])
    .unwrap();
    assert!(Refused::References(incomplete).wants_upload());
}

#[test]
fn the_reader_of_a_refusal_refuses_what_the_writer_does_not_write() {
    let record = |line, reason| ParseError::Record { line, reason };
    assert_eq!(
        parse("error refused\n"),
        Err(at(3, Malformed::Expected("ref")))
    );
    for lines in [
        "update hub\n",
        "update no-main\n",
        "update no-main x\n",
        "update other -\n",
        "ref heads/a gate\n",
        "ref heads/a gate x\n",
        "ref heads/a reserved x\n",
        "ref heads/a no-register x\n",
        "ref heads/a not-owner x\n",
        "ref heads/a unknown -\n",
        "ref heads/a denied Tag\n",
        "ref heads/a denied tag more\n",
        "ref heads/a leased human\n",
    ] {
        assert_eq!(
            parse(&format!("error refused\n{lines}")),
            Err(at(3, Malformed::Fields)),
            "{lines:?}"
        );
    }
    assert_eq!(
        parse("error refused\nupdate hub ftp://hub.example\n"),
        Err(record(3, RecordProblem::Address))
    );
    assert!(matches!(
        parse("error refused\nupdate repository 00\n"),
        Err(ParseError::InvalidRepositoryId { line: 3, .. })
    ));
    assert!(matches!(
        parse("error refused\nupdate key x\n"),
        Err(ParseError::InvalidObjectId { line: 3, .. })
    ));
    // One `update` line and nothing else, and no `update` line after a `ref` line.
    assert_eq!(
        parse("error refused\nupdate no-main -\nref heads/a gate -\n"),
        Err(at(4, Malformed::Unexpected))
    );
    assert_eq!(
        parse("error refused\nref heads/a gate -\nupdate no-main -\n"),
        Err(at(4, Malformed::Unexpected))
    );
    assert_eq!(
        parse("error refused\nref heads/b gate -\nref heads/a gate -\n"),
        Err(at(4, Malformed::Order))
    );
    assert_eq!(
        parse("error refused\nref heads/a leased human  7\n"),
        Err(record(3, RecordProblem::Actor))
    );
    assert_eq!(
        parse("error refused\nref heads/a leased robot mia 7\n"),
        Err(record(3, RecordProblem::Actor))
    );
    assert_eq!(
        parse("error refused\nref heads/a leased human mia 07\n"),
        Err(record(3, RecordProblem::Time))
    );
    assert_eq!(
        parse("error refused\nref heads/a incomplete \u{2028}\n"),
        Err(at(3, Malformed::Reason))
    );
    assert_eq!(
        parse("error refused\nref heads/a conflict \n"),
        Err(at(3, Malformed::Reason))
    );
    assert!(matches!(
        parse("error refused\nref a gate -\n"),
        Err(ParseError::InvalidName { line: 3, .. })
    ));
}

#[test]
fn the_new_kinds_are_read_under_this_version_only() {
    let bytes = b"stemma-protocol 2\nerror stale\nref heads/a - -\n";
    assert_eq!(
        ErrorMessage::parse(bytes),
        Err(ParseError::UnsupportedVersion { found: 2 })
    );
    let bytes = b"stemma-protocol 2\nerror refused\nupdate no-main -\n";
    assert_eq!(
        ErrorMessage::parse(bytes),
        Err(ParseError::UnsupportedVersion { found: 2 })
    );
}

#[test]
fn the_messages_say_what_is_refused() {
    assert_eq!(
        example_refused().to_string(),
        "the ref update is refused: heads/feature: the branch is leased to human mia until \
         1767225600 (seconds since the Unix epoch); heads/main: `main` moves through the merge \
         gate only; tags/v0.2.0: the operation `tag` is not allowed"
    );
    assert!(example_stale()
        .to_string()
        .starts_with("the ref update does not name the last record entry of heads/main"));
    assert!(Refused::NoMain.to_string().contains("the first push"));
}
