use super::*;
use crate::protocol::record::tests::Seeded;
use crate::protocol::statement::tests::example;
use crate::protocol::ErrorMessage;

fn commit(byte: u8) -> CommitId {
    let text = format!("b3:{}", format!("{byte:02x}").repeat(32));
    CommitId::new(text.parse().unwrap())
}

fn hub() -> Seeded {
    Seeded::new(0x48)
}

fn lines(number: u64, record: u64) -> HubLines {
    HubLines {
        number,
        time: 1_764_000_000,
        register: commit(0x11),
        record,
        actor: Identity::human("daniel").unwrap(),
    }
}

/// The held statement of the checked-in example `held-statement`: the example approval, held as
/// the third statement of its repository after the record entry 98.
pub(crate) fn example_held() -> HeldStatement {
    HeldStatement::seal(example(StatementKind::Approval), lines(3, 98), &hub()).unwrap()
}

/// The values of the checked-in examples of a range.
pub(crate) fn example_range() -> Range {
    Range::try_new(3, Some(17)).unwrap()
}

pub(crate) fn example_range_to_the_end() -> Range {
    Range::try_new(99, None).unwrap()
}

/// The refusal of the checked-in example `error-statement-refused`.
pub(crate) fn example_refused() -> ErrorMessage {
    ErrorMessage::StatementRefused(StatementRefusal::Head(Some(commit(0x44))))
}

#[test]
fn a_held_statement_has_this_text_and_round_trips() {
    let held = example_held();
    let text = String::from_utf8(held.encode()).unwrap();
    let statement = String::from_utf8(example(StatementKind::Approval).encode()).unwrap();
    let lines = text.strip_prefix(&statement).unwrap();
    let expected = format!(
        "number 3\ntime 1764000000\nregister {}\nrecord 98\nactor human daniel\n",
        commit(0x11)
    );
    assert!(lines.starts_with(&expected), "{lines}");
    assert!(lines[expected.len()..].starts_with("hub-signature "));
    let read = HeldStatement::parse(text.as_bytes()).unwrap();
    assert_eq!(read, held);
    assert_eq!(read.hub_key(), hub().public());
    assert_eq!(read.hash(), example(StatementKind::Approval).hash());
    assert_eq!(read.number(), 3);
    assert_eq!(read.record(), 98);
}

#[test]
fn a_held_statement_lies_before_the_entries_after_its_record_line() {
    let held = example_held();
    assert!(!held.lies_before(97));
    assert!(!held.lies_before(98));
    assert!(held.lies_before(99));
}

#[test]
fn a_changed_line_of_the_hub_breaks_its_signature() {
    let text = String::from_utf8(example_held().encode()).unwrap();
    let changed = text.replace("record 98\n", "record 97\n");
    assert!(matches!(
        HeldStatement::parse(changed.as_bytes()),
        Err(ParseError::Held {
            reason: HeldProblem::HubSignature,
            ..
        })
    ));
    // The unverified reader takes the form.
    assert!(HeldStatement::parse_unverified(changed.as_bytes()).is_ok());
}

#[test]
fn the_reader_refuses_what_the_writer_does_not_write() {
    let text = String::from_utf8(example_held().encode()).unwrap();
    let cases = [
        text.replace("number 3\n", "number 03\n"),
        text.replace("number 3\n", "number 0\n"),
        text.replace("record 98\n", "record -\n"),
        text.replace("actor human daniel\n", "actor human  daniel\n"),
        text.replace("time 1764000000\n", "time +1\n"),
        format!("{text}number 4\n"),
        text[..text.len() - 1].to_owned(),
        text.replace("record 98\n", ""),
    ];
    for case in cases {
        assert!(HeldStatement::parse(case.as_bytes()).is_err(), "{case}");
    }
    // A statement that is not signed by its key is refused as the statement reader refuses it.
    let statement = String::from_utf8(example(StatementKind::Approval).encode()).unwrap();
    assert!(HeldStatement::parse(statement.as_bytes()).is_err());
}

#[test]
fn held_statements_one_after_the_other_are_read_and_a_cut_one_is_refused() {
    let one = example_held();
    let two = HeldStatement::seal(example(StatementKind::Closing), lines(4, 98), &hub()).unwrap();
    let mut bytes = one.encode();
    bytes.extend_from_slice(&two.encode());
    assert_eq!(read_held(&bytes).unwrap(), [one.clone(), two.clone()]);
    assert_eq!(read_held(b"").unwrap(), []);
    // A part of a held statement, also within its last line, is refused at its position.
    for cut in 1..two.encode().len() {
        let partial = &bytes[..one.encode().len() + cut];
        assert_eq!(read_held(partial).unwrap_err().position, 2, "{cut}");
    }
}

#[test]
fn a_range_has_this_text_and_refuses_what_its_writer_does_not_write() {
    assert_eq!(example_range().encode(), b"stemma-protocol 1\nrange 3 17\n");
    assert_eq!(
        example_range_to_the_end().encode(),
        b"stemma-protocol 1\nrange 99 -\n"
    );
    assert_eq!(
        Range::parse(b"stemma-protocol 1\nrange 3 17\n").unwrap(),
        example_range()
    );
    assert!(example_range().contains(3));
    assert!(example_range().contains(17));
    assert!(!example_range().contains(18));
    assert!(Range::all().contains(u64::MAX));
    assert_eq!(Range::try_new(0, None), Err(HeldProblem::Range));
    assert_eq!(Range::try_new(5, Some(4)), Err(HeldProblem::Range));
    for case in [
        "stemma-protocol 1\nrange 5 4\n",
        "stemma-protocol 1\nrange 0 -\n",
        "stemma-protocol 1\nrange 1\n",
        "stemma-protocol 1\nrange 1 +\n",
        "stemma-protocol 1\nrange 1 2\nrange 3 4\n",
        "stemma-protocol 2\nrange 1 2\n",
    ] {
        assert!(Range::parse(case.as_bytes()).is_err(), "{case}");
    }
}

#[test]
fn every_refusal_round_trips_through_the_error_message() {
    let refusals = [
        StatementRefusal::Repository {
            id: "00112233445566778899aabbccddeeff".parse().unwrap(),
        },
        StatementRefusal::Unsigned,
        StatementRefusal::NoMain,
        StatementRefusal::Key {
            register: commit(1),
        },
        StatementRefusal::Kind(StatementKind::Override),
        StatementRefusal::Denied(OperationName::try_new("pr-open").unwrap()),
        StatementRefusal::NotWriteOwner,
        StatementRefusal::Head(None),
        StatementRefusal::Head(Some(commit(2))),
        StatementRefusal::AlreadyOpen(example(StatementKind::Opening).hash()),
        StatementRefusal::UnknownReviewer(Identity::human("mia").unwrap()),
        StatementRefusal::SelfReview,
        StatementRefusal::NoPull,
        StatementRefusal::NotOpen,
        StatementRefusal::SelfApproval,
        StatementRefusal::NotCloser,
        StatementRefusal::Unreachable(commit(3)),
        StatementRefusal::Policy(Reason::try_new("the policy is missing").unwrap()),
    ];
    for refusal in refusals {
        let message = ErrorMessage::StatementRefused(refusal);
        let bytes = message.encode();
        assert_eq!(ErrorMessage::parse(&bytes).unwrap(), message);
        assert!(!message.to_string().is_empty());
    }
    assert_eq!(
        ErrorMessage::NotReadable.encode(),
        b"stemma-protocol 1\nerror not-readable\n"
    );
    for case in [
        "stemma-protocol 1\nerror statement-refused\nrefused unsigned x\n",
        "stemma-protocol 1\nerror statement-refused\nrefused unknown -\n",
        "stemma-protocol 1\nerror statement-refused\nrefused kind pr-merge\n",
        "stemma-protocol 1\nerror statement-refused\n",
        "stemma-protocol 1\nerror statement-refused\nrefused no-pull -\nrefused no-pull -\n",
        "stemma-protocol 1\nerror not-readable\nreason x\n",
    ] {
        assert!(ErrorMessage::parse(case.as_bytes()).is_err(), "{case}");
    }
}
