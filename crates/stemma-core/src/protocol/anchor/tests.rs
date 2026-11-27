use super::*;
use crate::protocol::record::tests::Seeded;
use crate::protocol::ErrorMessage;

fn commit(byte: u8) -> CommitId {
    let text = format!("b3:{}", format!("{byte:02x}").repeat(32));
    CommitId::new(text.parse().unwrap())
}

fn hash(byte: u8) -> EntryHash {
    format!("b3:{}", format!("{byte:02x}").repeat(32))
        .parse()
        .unwrap()
}

fn hub() -> Seeded {
    Seeded::new(0x48)
}

fn actor() -> Seeded {
    Seeded::new(0x21)
}

fn log() -> LogId {
    "0f1e2d3c4b5a69788796a5b4c3d2e1f0".parse().unwrap()
}

fn repository() -> RepositoryId {
    "0123456789abcdef0123456789abcdef".parse().unwrap()
}

fn address() -> HubAddress {
    HubAddress::try_new("https://hub.example").unwrap()
}

fn point(number: u64, byte: u8) -> Point {
    Point {
        number,
        hash: hash(byte),
    }
}

/// An anchor of the entry `tip` after the confirmation of `confirmed`.
pub(crate) fn anchor_of(tip: Point, confirmed: Option<Point>) -> Anchor {
    AnchorPayload::try_new(
        address(),
        repository(),
        log(),
        tip,
        confirmed,
        actor().public(),
    )
    .unwrap()
    .sign(&actor())
    .unwrap()
}

fn lines(number: u64, record: u64, state: AnchorState) -> AnchorLines {
    AnchorLines {
        number,
        time: 1_764_000_000,
        register: commit(0x11),
        record,
        actor: Identity::human("daniel").unwrap(),
        state,
    }
}

/// The anchor of the checked-in example `anchor`: entry 42 after the confirmed entry 40.
pub(crate) fn example_anchor() -> Anchor {
    anchor_of(point(42, 0x42), Some(point(40, 0x40)))
}

/// The held anchor of the checked-in example `held-anchor`: the example anchor, confirmed as
/// the second anchor of its log after the record entry 98.
pub(crate) fn example_held() -> HeldAnchor {
    HeldAnchor::seal(
        example_anchor(),
        lines(2, 98, AnchorState::Confirmed),
        &hub(),
    )
    .unwrap()
}

/// The request of the checked-in example `anchors`.
pub(crate) fn example_request() -> AnchorsRequest {
    AnchorsRequest::new(log())
}

/// The refusal of the checked-in example `error-anchor-refused`.
pub(crate) fn example_refused() -> ErrorMessage {
    ErrorMessage::AnchorRefused(AnchorRefusal::Contradicts {
        held: point(42, 0x42),
    })
}

#[test]
fn an_anchor_has_this_text_and_round_trips() {
    let anchor = example_anchor();
    let text = String::from_utf8(anchor.encode()).unwrap();
    let expected = format!(
        "stemma-protocol 1\nhub https://hub.example\nrepository {}\nlog {}\ntip 42 {}\n\
         confirmed 40 {}\nkey {}\nsignature ",
        repository(),
        log(),
        hash(0x42),
        hash(0x40),
        actor().public()
    );
    assert!(text.starts_with(&expected), "{text}");
    assert_eq!(Anchor::parse(text.as_bytes()).unwrap(), anchor);
    let first = anchor_of(point(1, 0x01), None);
    let text = String::from_utf8(first.encode()).unwrap();
    assert!(text.contains("\nconfirmed -\n"));
    assert_eq!(Anchor::parse(text.as_bytes()).unwrap(), first);
}

#[test]
fn a_confirmed_anchor_after_the_tip_is_refused() {
    let payload = |confirmed| {
        AnchorPayload::try_new(
            address(),
            repository(),
            log(),
            point(5, 0x05),
            Some(confirmed),
            actor().public(),
        )
    };
    assert_eq!(payload(point(6, 0x06)), Err(AnchorProblem::Confirmed));
    assert_eq!(payload(point(5, 0x55)), Err(AnchorProblem::Confirmed));
    assert!(payload(point(5, 0x05)).is_ok());
    assert!(payload(point(4, 0x04)).is_ok());
}

#[test]
fn a_changed_anchor_breaks_the_signature_of_the_actor() {
    let text = String::from_utf8(example_anchor().encode()).unwrap();
    let changed = text.replace("tip 42 ", "tip 43 ");
    assert!(matches!(
        Anchor::parse(changed.as_bytes()),
        Err(ParseError::Anchor {
            reason: AnchorProblem::ActorSignature,
            line: 8,
        })
    ));
    assert!(Anchor::parse_unverified(changed.as_bytes()).is_ok());
    let other = text.replace(
        &actor().public().to_string(),
        &Seeded::new(0x22).public().to_string(),
    );
    assert!(matches!(
        Anchor::parse(other.as_bytes()),
        Err(ParseError::Anchor {
            reason: AnchorProblem::ActorKey,
            ..
        })
    ));
}

#[test]
fn a_held_anchor_has_this_text_and_round_trips() {
    let held = example_held();
    let text = String::from_utf8(held.encode()).unwrap();
    let anchor = String::from_utf8(example_anchor().encode()).unwrap();
    let rest = text.strip_prefix(&anchor).unwrap();
    let expected = format!(
        "number 2\ntime 1764000000\nregister {}\nrecord 98\nactor human daniel\n\
         state confirmed\nhub-signature ",
        commit(0x11)
    );
    assert!(rest.starts_with(&expected), "{rest}");
    let read = HeldAnchor::parse(text.as_bytes()).unwrap();
    assert_eq!(read, held);
    assert_eq!(read.hub_key(), hub().public());
    assert!(read.is_confirmed());
    assert_eq!(read.tip(), point(42, 0x42));
    assert_eq!(read.log(), log());
}

#[test]
fn a_changed_line_of_the_hub_breaks_its_signature() {
    let text = String::from_utf8(example_held().encode()).unwrap();
    for (old, new) in [
        ("record 98\n", "record 97\n"),
        ("state confirmed\n", "state contradicts\n"),
    ] {
        let changed = text.replace(old, new);
        assert!(
            matches!(
                HeldAnchor::parse(changed.as_bytes()),
                Err(ParseError::Anchor {
                    reason: AnchorProblem::HubSignature,
                    ..
                })
            ),
            "{new}"
        );
        assert!(HeldAnchor::parse_unverified(changed.as_bytes()).is_ok());
    }
    let unknown = text.replace("state confirmed\n", "state maybe\n");
    assert!(matches!(
        HeldAnchor::parse(unknown.as_bytes()),
        Err(ParseError::Anchor {
            reason: AnchorProblem::State,
            ..
        })
    ));
}

#[test]
fn held_anchors_are_read_one_after_the_other() {
    let first = HeldAnchor::seal(
        anchor_of(point(40, 0x40), None),
        lines(1, 0, AnchorState::Confirmed),
        &hub(),
    )
    .unwrap();
    let second = example_held();
    let mut bytes = first.encode();
    bytes.extend_from_slice(&second.encode());
    assert_eq!(
        read_held_anchors(&bytes).unwrap(),
        vec![first.clone(), second]
    );
    assert_eq!(read_held_anchors(b"").unwrap(), Vec::new());
    let cut = &bytes[..bytes.len() - 3];
    assert_eq!(read_held_anchors(cut).unwrap_err().position, 2);
    assert!(first.encode().windows(9).any(|w| w == b"record 0\n"));
}

#[test]
fn the_request_and_the_refusals_round_trip() {
    let request = example_request();
    let text = format!("stemma-protocol 1\nanchors {}\n", log());
    assert_eq!(request.encode(), text.as_bytes());
    assert_eq!(AnchorsRequest::parse(text.as_bytes()).unwrap(), request);
    assert!(AnchorsRequest::parse(b"stemma-protocol 1\nanchors 0F\n").is_err());
    for refusal in [
        AnchorRefusal::Hub { called: address() },
        AnchorRefusal::Repository { id: repository() },
        AnchorRefusal::NoMain,
        AnchorRefusal::Key {
            register: commit(0x11),
        },
        AnchorRefusal::Contradicts {
            held: point(42, 0x42),
        },
    ] {
        let message = ErrorMessage::AnchorRefused(refusal);
        assert_eq!(ErrorMessage::parse(&message.encode()).unwrap(), message);
    }
}

#[test]
fn a_log_id_has_one_text() {
    let text = log().to_string();
    assert_eq!(text.parse::<LogId>().unwrap(), log());
    assert!(text.to_uppercase().parse::<LogId>().is_err());
    assert!("0f1e".parse::<LogId>().is_err());
    assert_ne!(LogId::generate().unwrap(), LogId::generate().unwrap());
}

#[test]
fn an_entry_of_another_clone_stands_before_a_record_entry_only_by_a_confirmed_anchor() {
    // Entry 40 of the log is covered by the anchor of entry 42, confirmed at record entry 98.
    let confirmed = example_held();
    let contradicting = HeldAnchor::seal(
        anchor_of(point(41, 0x77), None),
        lines(3, 97, AnchorState::Contradicts),
        &hub(),
    )
    .unwrap();
    let held = [contradicting, confirmed];
    // A revocation that record entry 99 brought to `main` comes after it.
    assert!(stands_before(&held, log(), 40, 99));
    assert!(stands_before(&held, log(), 42, 99));
    // Entry 43 is covered by no anchor and counts as after the revocation.
    assert!(!stands_before(&held, log(), 43, 99));
    // A revocation at record entry 98 or before does not come after the confirmation.
    assert!(!stands_before(&held, log(), 40, 98));
    // A contradicting anchor confirms nothing, and another log has no anchor.
    assert!(!stands_before(&held[..1], log(), 40, 99));
    let other: LogId = "ffffffffffffffffffffffffffffffff".parse().unwrap();
    assert!(!stands_before(&held, other, 40, 99));
}
