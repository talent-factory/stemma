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
    RefusedReference::try_new(name(reference), reason).unwrap()
}

fn branch(text: &str) -> BranchName {
    BranchName::try_new(text).unwrap()
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
                operation: RefOperation::Tag,
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
        refused("heads/e", RefusalReason::Conflict(branch("E/x"))),
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
        refused(
            "heads/i",
            RefusalReason::Denied {
                operation: RefOperation::Push,
            },
        ),
        refused(
            "tags/v1",
            RefusalReason::Denied {
                operation: RefOperation::Untag,
            },
        ),
    ]);
    assert_eq!(
        round_trip(&all),
        "stemma-protocol 1\nerror refused\nref heads/a reserved -\nref heads/e conflict E/x\n\
         ref heads/f incomplete the tree b3:00 is missing\n\
         ref heads/g policy no policy file\nref heads/h leased agent bot -1\n\
         ref heads/i denied push\nref tags/v1 denied untag\n"
    );
    // The reasons of the first push are reasons of `main`.
    for (reason, line) in [
        (RefusalReason::NoRegister, "no-register -"),
        (
            RefusalReason::BadRegister(reason("line 2: not sorted")),
            "bad-register line 2: not sorted",
        ),
        (RefusalReason::NotOwner, "not-owner -"),
        (RefusalReason::Gate, "gate -"),
        // The owners that a namespace demands (ADR-027, STEMMA-311).
        (
            RefusalReason::NamespaceOwner("human teacher".parse().unwrap()),
            "namespace-owner human teacher",
        ),
    ] {
        let message = references(vec![refused("heads/main", reason)]);
        assert!(round_trip(&message).ends_with(&format!("ref heads/main {line}\n")));
    }
}

#[test]
fn a_reason_fits_its_reference_for_the_constructor_and_for_the_reader() {
    let leased = || RefusalReason::Leased {
        holder: "human mia".parse().unwrap(),
        expires: 7,
    };
    let denied = |operation| RefusalReason::Denied { operation };
    let misfits = [
        ("heads/a", RefusalReason::Gate, "gate -"),
        ("tags/main", RefusalReason::Gate, "gate -"),
        ("heads/a", RefusalReason::NoRegister, "no-register -"),
        ("heads/a", RefusalReason::NotOwner, "not-owner -"),
        (
            "heads/a",
            RefusalReason::NamespaceOwner("human teacher".parse().unwrap()),
            "namespace-owner human teacher",
        ),
        (
            "heads/a",
            RefusalReason::BadRegister(reason("x")),
            "bad-register x",
        ),
        ("heads/main", leased(), "leased human mia 7"),
        ("tags/v1", leased(), "leased human mia 7"),
        ("heads/main", RefusalReason::Policy(reason("x")), "policy x"),
        ("tags/v1", RefusalReason::Policy(reason("x")), "policy x"),
        ("heads/a", denied(RefOperation::Tag), "denied tag"),
        ("heads/a", denied(RefOperation::Untag), "denied untag"),
        ("tags/v1", denied(RefOperation::Push), "denied push"),
    ];
    for (reference, reason, line) in misfits {
        assert_eq!(
            RefusedReference::try_new(name(reference), reason),
            Err(RecordProblem::RefusalReason),
            "{reference} {line}"
        );
        assert_eq!(
            parse(&format!("error refused\nref {reference} {line}\n")),
            Err(ParseError::Record {
                line: 3,
                reason: RecordProblem::RefusalReason
            }),
            "{reference} {line}"
        );
    }
    let fit = refused("heads/main", RefusalReason::Incomplete(reason("x")));
    assert_eq!(fit.name(), &name("heads/main"));
    assert_eq!(fit.reason(), &RefusalReason::Incomplete(reason("x")));
    assert_eq!(RefOperation::Push.operation_name().to_string(), "push");
    assert_eq!(
        RefOperation::ALL.map(RefOperation::name),
        ["push", "tag", "untag"]
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
            refused("heads/a", RefusalReason::Incomplete(reason("x"))),
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
        refused("heads/a", RefusalReason::Reserved),
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
        "ref heads/a denied commit\n",
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
        parse("error refused\nupdate no-main -\nref heads/a reserved -\n"),
        Err(at(4, Malformed::Unexpected))
    );
    assert_eq!(
        parse("error refused\nref heads/a reserved -\nupdate no-main -\n"),
        Err(at(4, Malformed::Unexpected))
    );
    assert_eq!(
        parse("error refused\nref heads/b reserved -\nref heads/a reserved -\n"),
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
    for name in ["", "a b", "a//b", "-x/", "a.", ".a", "a/con", "a/"] {
        assert_eq!(
            parse(&format!("error refused\nref heads/a conflict {name}\n")),
            Err(at(3, Malformed::Fields)),
            "{name:?}"
        );
    }
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

// The verdict of the gate on a hub (STEMMA-303).

/// The example `error-refused-gate`: the gate on a hub refuses a merge whose approvals do not
/// suffice for the level that the escalation for weakened tests set.
pub(crate) fn example_refused_gate() -> ErrorMessage {
    references(vec![refused(
        "heads/main",
        RefusalReason::Verdict(GateRefusal::NotApproved {
            level: RiskLevel::Two,
            source: LevelSource::Weakened,
            agents: 1,
            agents_needed: 2,
            humans: 0,
            humans_needed: 1,
        }),
    )])
}

/// Every verdict of the gate once, with the text of its facts.
fn every_verdict() -> Vec<(GateRefusal, String)> {
    let pull: StatementHash = id_text(0x44).parse().unwrap();
    vec![
        (GateRefusal::NoPull, "no-pull -".to_owned()),
        (GateRefusal::NotOpen, "not-open -".to_owned()),
        (GateRefusal::Parents, "parents -".to_owned()),
        (GateRefusal::NotTheMerge, "not-the-merge -".to_owned()),
        (
            GateRefusal::BranchMoved { head: None },
            "branch-moved -".to_owned(),
        ),
        (
            GateRefusal::BranchMoved {
                head: Some(commit(0x11)),
            },
            format!("branch-moved {}", id_text(0x11)),
        ),
        (GateRefusal::Conflict, "conflict -".to_owned()),
        (
            GateRefusal::Policy(reason("the policy is not valid")),
            "policy the policy is not valid".to_owned(),
        ),
        (
            GateRefusal::Level(reason("no file differs")),
            "level no file differs".to_owned(),
        ),
        (
            GateRefusal::Unusable(reason("the merge holds no register")),
            "unusable the merge holds no register".to_owned(),
        ),
        (
            GateRefusal::NotChecked {
                level: RiskLevel::Three,
                source: LevelSource::Paths,
            },
            "not-checked 3 paths".to_owned(),
        ),
        (
            GateRefusal::NotApproved {
                level: RiskLevel::Zero,
                source: LevelSource::Paths,
                agents: 0,
                agents_needed: 1,
                humans: 0,
                humans_needed: 0,
            },
            "not-approved 0 paths 0 1 0 0".to_owned(),
        ),
        (
            GateRefusal::RedTip { red: commit(0x22) },
            format!("red-tip {}", id_text(0x22)),
        ),
        (
            GateRefusal::Unverified { count: 12, max: 10 },
            "unverified 12 10".to_owned(),
        ),
        (
            GateRefusal::Reserved {
                pull,
                until: 1_767_225_600,
            },
            format!("reserved {} 1767225600", id_text(0x44)),
        ),
    ]
}

#[test]
fn every_verdict_of_the_gate_is_written_after_gate_and_read_back() {
    for (verdict, facts) in every_verdict() {
        let message = references(vec![refused(
            "heads/main",
            RefusalReason::Verdict(verdict.clone()),
        )]);
        let text = round_trip(&message);
        assert_eq!(
            text,
            format!("stemma-protocol 1\nerror refused\nref heads/main gate {facts}\n")
        );
        assert!(!verdict.to_string().is_empty());
    }
    // `gate -` stays the refusal of a move that is no merge.
    let gate = references(vec![refused("heads/main", RefusalReason::Gate)]);
    assert!(round_trip(&gate).ends_with("ref heads/main gate -\n"));
}

#[test]
fn a_verdict_of_the_gate_is_a_reason_of_main_only() {
    let verdict = RefusalReason::Verdict(GateRefusal::Conflict);
    assert!(RefusedReference::try_new(name("heads/main"), verdict.clone()).is_ok());
    for other in ["heads/feature", "tags/v1"] {
        assert_eq!(
            RefusedReference::try_new(name(other), verdict.clone()),
            Err(RecordProblem::RefusalReason)
        );
    }
    assert!(parse("error refused\nref heads/feature gate conflict -\n").is_err());
}

#[test]
fn the_reader_refuses_a_verdict_of_the_gate_that_breaks_its_form() {
    for facts in [
        "unknown -",
        "conflict",
        "conflict x",
        "no-pull something",
        "not-checked 4 paths",
        "not-checked 2 guessed",
        "not-checked 02 paths",
        "not-checked 2",
        "not-approved 2 paths 1 2 0",
        "not-approved 2 paths 01 2 0 1",
        "not-approved 2 paths -1 2 0 1",
        "unverified 1",
        "reserved b3:00 1",
        "red-tip -",
        "branch-moved b3:zz",
        "policy ",
    ] {
        let line = format!("error refused\nref heads/main gate {facts}\n");
        assert!(parse(&line).is_err(), "{facts}");
    }
}

// The owners that a namespace demands (ADR-027, STEMMA-311).

/// The example `error-refused-namespace-owner`: the register of the new commit of `main` does
/// not hold an owner that the namespace demands.
pub(crate) fn example_refused_namespace_owner() -> ErrorMessage {
    references(vec![refused(
        "heads/main",
        RefusalReason::NamespaceOwner("human teacher".parse().unwrap()),
    )])
}

#[test]
fn the_identity_of_a_demanded_owner_has_one_spelling() {
    for line in [
        "namespace-owner -",
        "namespace-owner human",
        "namespace-owner human  teacher",
        "namespace-owner person teacher",
    ] {
        assert!(
            parse(&format!("error refused\nref heads/main {line}\n")).is_err(),
            "{line}"
        );
    }
    assert!(example_refused_namespace_owner()
        .to_string()
        .contains("does not hold human teacher as an owner"));
}
