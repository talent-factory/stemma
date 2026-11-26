//! The tests of the sign-in: the texts of the three messages, the refusals of their readers, the
//! separation of the signature from every other type of proof, the binding to the address, the
//! check against a register, and the token, which no output shows.

use std::cell::Cell;

use super::*;
use crate::fuzz::check_rejection;
use crate::protocol::record::tests::Seeded;
use crate::protocol::record::RecordProblem;
use crate::register::{sponsorship_payload, Role};
use crate::signature::{sign, verify, SignError};

const HUB: &str = "https://hub.example";

/// The token of the checked-in example `token`. No hub issued it.
const TOKEN: &str = "c3RlbW1hLWV4YW1wbGUtdG9rZW4";

fn hub() -> HubAddress {
    HubAddress::try_new(HUB).unwrap()
}

fn other_hub() -> HubAddress {
    HubAddress::try_new("https://other.example").unwrap()
}

/// The key of the sender of these tests.
fn daniel() -> Seeded {
    Seeded::new(1)
}

/// The judgement of a hub that accepts every nonce.
fn accepted(_: &Nonce) -> bool {
    true
}

/// The judgement of a hub that accepts no nonce.
fn stale(_: &Nonce) -> bool {
    false
}

fn human(id: &str) -> Identity {
    Identity::human(id).unwrap()
}

/// The challenge of the checked-in example `challenge`: 32 bytes that count up from 0.
pub(crate) fn example_challenge() -> Challenge {
    Challenge::new(hub(), Nonce::try_new((0..32).collect::<Vec<u8>>()).unwrap())
}

/// The answer of the checked-in example `signed-challenge`, to [`example_challenge`].
pub(crate) fn example_signed() -> SignedChallenge {
    example_challenge()
        .answer(&hub(), &daniel().public(), &daniel())
        .unwrap()
}

/// The token answer of the checked-in example `token`: ten minutes.
pub(crate) fn example_token_answer() -> TokenAnswer {
    TokenAnswer::new(
        Token::try_new(TOKEN).unwrap(),
        NonZeroU64::new(600).unwrap(),
    )
}

/// A signer that counts how often it is asked.
struct Counting(Seeded, Cell<u32>);

impl Signer for Counting {
    fn sign(&self, message: &[u8]) -> Result<Signature, SignError> {
        self.1.set(self.1.get() + 1);
        self.0.sign(message)
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

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap()
}

#[test]
fn the_three_messages_have_these_texts() {
    let nonce = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    assert_eq!(
        text(example_challenge().encode()),
        format!("stemma-protocol 1\nhub {HUB}\nnonce {nonce}\n")
    );
    let signed = example_signed();
    let key = daniel().public();
    assert_eq!(
        text(signed.payload()),
        format!("hub {HUB}\nnonce {nonce}\nkey {key}\n")
    );
    assert_eq!(
        text(signed.encode()),
        format!(
            "stemma-protocol 1\nhub {HUB}\nnonce {nonce}\nkey {key}\nsignature {}\n",
            signature_to_line(signed.signature())
        )
    );
    assert_eq!(
        text(example_token_answer().encode()),
        format!("stemma-protocol 1\ntoken {TOKEN}\nlifetime 600\n")
    );
}

#[test]
fn every_message_is_read_back_as_the_value_that_wrote_it() {
    let challenge = example_challenge();
    assert_eq!(Challenge::parse(&challenge.encode()).unwrap(), challenge);
    assert_eq!(challenge.hub(), &hub());
    assert_eq!(challenge.nonce().as_bytes().len(), 32);

    let signed = example_signed();
    let read = SignedChallenge::parse(&signed.encode()).unwrap();
    assert_eq!(read, signed);
    assert_eq!(read.hub(), &hub());
    assert_eq!(read.nonce(), challenge.nonce());
    assert_eq!(read.key(), &daniel().public());
    assert_eq!(
        SignedChallenge::parse_unverified(&signed.encode()).unwrap(),
        signed
    );

    let answer = example_token_answer();
    let read = TokenAnswer::parse(&answer.encode()).unwrap();
    assert_eq!(read, answer);
    assert_eq!(read.lifetime(), Duration::from_secs(600));
    assert_eq!(read.token().expose(), TOKEN);
    assert_eq!(read.into_token().expose(), TOKEN);
}

#[test]
fn a_nonce_is_16_to_64_bytes_in_lowercase_hexadecimal() {
    for length in [16, 17, 32, 64] {
        let nonce = Nonce::try_new(vec![0xab; length]).unwrap();
        assert_eq!(nonce.to_string(), "ab".repeat(length));
        assert_eq!(nonce.to_string().parse::<Nonce>().unwrap(), nonce);
        assert_eq!(format!("{nonce:?}"), format!("Nonce({nonce})"));
    }
    for length in [0, 1, 15, 65, 1000] {
        assert_eq!(
            Nonce::try_new(vec![0; length]).unwrap_err(),
            SignInProblem::Nonce
        );
    }
    let refused = [
        String::new(),
        "ab".repeat(15),
        "ab".repeat(65),
        "AB".repeat(16),
        format!("{}a", "ab".repeat(16)),
        format!("{}g0", "ab".repeat(16)),
        format!("{} ", "ab".repeat(16)),
        format!("{}\u{e4}", "ab".repeat(16)),
    ];
    for text in refused {
        assert_eq!(
            text.parse::<Nonce>().unwrap_err(),
            SignInProblem::Nonce,
            "{text:?}"
        );
    }
}

#[test]
fn a_token_is_what_a_bearer_header_carries() {
    for text in ["a", "A-z.0_9~+/", "abc=", "abc==", TOKEN] {
        assert_eq!(Token::try_new(text).unwrap().expose(), text);
    }
    for text in [
        "", "=", "==", "a=b", "a b", " a", "a\n", "a\tb", "a,b", "a:b", "\u{e4}",
    ] {
        assert_eq!(
            Token::try_new(text).unwrap_err(),
            SignInProblem::Token,
            "{text:?}"
        );
    }
}

#[test]
fn the_reader_of_the_challenge_refuses_everything_else() {
    let nonce = "ab".repeat(16);
    let refused = |rest: &str| {
        let message = format!("stemma-protocol 1\n{rest}");
        let error = Challenge::parse(message.as_bytes()).unwrap_err();
        check_rejection(&error);
        error
    };
    assert_eq!(refused(""), malformed(2, Malformed::Expected("hub")));
    assert_eq!(
        refused(&format!("nonce {nonce}\nhub {HUB}\n")),
        malformed(2, Malformed::Expected("hub"))
    );
    assert_eq!(
        refused(&format!("hub {HUB}\n")),
        malformed(3, Malformed::Expected("nonce"))
    );
    assert_eq!(
        refused(&format!("hub {HUB}\nnonce {nonce}")),
        malformed(3, Malformed::CutOff)
    );
    assert_eq!(
        refused(&format!("hub {HUB}\nnonce {nonce}\nnonce {nonce}\n")),
        malformed(4, Malformed::Unexpected)
    );
    assert_eq!(
        refused(&format!("hub {HUB}\nnonce {nonce}\n\n")),
        malformed(4, Malformed::Unexpected)
    );
    assert_eq!(
        refused(&format!("hub {HUB}/\nnonce {nonce}\n")),
        ParseError::Record {
            line: 2,
            reason: RecordProblem::Address
        }
    );
    for nonce in ["", "abcd", &"AB".repeat(16), &format!("{nonce} ")] {
        assert_eq!(
            refused(&format!("hub {HUB}\nnonce {nonce}\n")),
            problem(3, SignInProblem::Nonce),
            "{nonce:?}"
        );
    }
    assert_eq!(
        Challenge::parse(format!("stemma-protocol 2\nhub {HUB}\nnonce {nonce}\n").as_bytes())
            .unwrap_err(),
        ParseError::UnsupportedVersion { found: 2 }
    );
    assert!(matches!(
        Challenge::parse(b"stemma-protocol 1\nhub \xff\n").unwrap_err(),
        ParseError::NotUtf8 { .. }
    ));
}

#[test]
fn the_reader_of_the_token_answer_refuses_everything_else() {
    let refused = |rest: &str| {
        let message = format!("stemma-protocol 1\n{rest}");
        let error = TokenAnswer::parse(message.as_bytes()).unwrap_err();
        check_rejection(&error);
        error
    };
    assert_eq!(refused(""), malformed(2, Malformed::Expected("token")));
    assert_eq!(
        refused("lifetime 600\ntoken abc\n"),
        malformed(2, Malformed::Expected("token"))
    );
    assert_eq!(
        refused("token abc\n"),
        malformed(3, Malformed::Expected("lifetime"))
    );
    assert_eq!(
        refused("token abc\nlifetime 600"),
        malformed(3, Malformed::CutOff)
    );
    assert_eq!(
        refused("token abc\nlifetime 600\nlifetime 600\n"),
        malformed(4, Malformed::Unexpected)
    );
    for token in ["", "a b", "a=b", "=", "\u{e4}"] {
        assert_eq!(
            refused(&format!("token {token}\nlifetime 600\n")),
            problem(2, SignInProblem::Token),
            "{token:?}"
        );
    }
    for lifetime in [
        "",
        "0",
        "060",
        "+60",
        "-1",
        "60 ",
        "1.5",
        "18446744073709551616",
    ] {
        assert_eq!(
            refused(&format!("token abc\nlifetime {lifetime}\n")),
            malformed(3, Malformed::Number),
            "{lifetime:?}"
        );
    }
    assert_eq!(
        TokenAnswer::parse(b"stemma-protocol 1\ntoken abc\nlifetime 18446744073709551615\n")
            .unwrap()
            .lifetime(),
        Duration::from_secs(u64::MAX)
    );
}

/// Replaces the first occurrence of `from` in the text of the example answer and reads the
/// result.
fn changed(from: &str, to: &str) -> Result<SignedChallenge, ParseError> {
    let text = text(example_signed().encode());
    assert!(text.contains(from), "{from:?} is not in the example");
    SignedChallenge::parse(text.replacen(from, to, 1).as_bytes())
}

#[test]
fn the_reader_of_the_signed_challenge_refuses_everything_else() {
    let signed = text(example_signed().encode());
    let lines: Vec<&str> = signed.split_inclusive('\n').collect();
    // Every line must be there, in its place.
    for (missing, keyword) in [(1, "hub"), (2, "nonce"), (3, "key"), (4, "signature")] {
        let mut cut = lines.clone();
        cut.remove(missing);
        let error = SignedChallenge::parse(cut.concat().as_bytes()).unwrap_err();
        assert_eq!(error, malformed(missing + 1, Malformed::Expected(keyword)));
        check_rejection(&error);
    }
    assert_eq!(
        SignedChallenge::parse(format!("{signed}key x\n").as_bytes()).unwrap_err(),
        malformed(6, Malformed::Unexpected)
    );
    assert_eq!(
        SignedChallenge::parse(&signed.as_bytes()[..signed.len() - 1]).unwrap_err(),
        malformed(5, Malformed::CutOff)
    );
    assert_eq!(
        changed("hub https://", "hub ftp://").unwrap_err(),
        ParseError::Record {
            line: 2,
            reason: RecordProblem::Address
        }
    );
    assert_eq!(
        changed("nonce 00", "nonce 0G").unwrap_err(),
        problem(3, SignInProblem::Nonce)
    );
    let key = format!("key {}", daniel().public());
    assert_eq!(
        changed(&key, &format!("{key} daniel")).unwrap_err(),
        malformed(4, Malformed::KeyForm)
    );
    assert!(matches!(
        changed("key ssh-ed25519 ", "key ssh-rsa ").unwrap_err(),
        ParseError::InvalidKey { line: 4, .. }
    ));
    for signature in ["signature x y", "signature ", "signature AAAA"] {
        let mut other = lines.clone();
        let line = format!("{signature}\n");
        other[4] = &line;
        assert_eq!(
            SignedChallenge::parse(other.concat().as_bytes()).unwrap_err(),
            problem(5, SignInProblem::SignatureForm),
            "{signature:?}"
        );
    }
}

#[test]
fn a_changed_address_nonce_or_key_does_not_carry_the_signature() {
    // The address and the nonce are signed: the signature of the example is for no other.
    for (from, to) in [
        ("hub https://hub.example", "hub https://hub.example:8443"),
        ("hub https://hub.example", "hub http://hub.example"),
        ("nonce 00", "nonce 01"),
    ] {
        let error = changed(from, to).unwrap_err();
        assert_eq!(error, problem(5, SignInProblem::Signature), "{to}");
        check_rejection(&error);
        assert!(SignedChallenge::parse_unverified(
            text(example_signed().encode())
                .replacen(from, to, 1)
                .as_bytes()
        )
        .is_ok());
    }
    // The key is signed too, and the signature names the key that made it.
    let other = Seeded::new(7).public();
    let error = changed(
        &format!("key {}", daniel().public()),
        &format!("key {other}"),
    )
    .unwrap_err();
    assert_eq!(error, problem(5, SignInProblem::Key));
    assert_eq!(
        SignedChallenge::try_new(
            example_challenge(),
            other,
            example_signed().signature().clone()
        )
        .unwrap_err(),
        VerifyError::WrongKey
    );
    // A signature of the same key for another challenge.
    let later = Challenge::new(hub(), Nonce::try_new(vec![9; 32]).unwrap());
    let signature = later
        .answer(&hub(), &daniel().public(), &daniel())
        .unwrap()
        .signature()
        .clone();
    assert_eq!(
        SignedChallenge::try_new(example_challenge(), daniel().public(), signature).unwrap_err(),
        VerifyError::Invalid
    );
}

/// The types of proof that are no challenge. The match stops to build when a type is added, so
/// that the test below covers it.
fn other_kinds() -> Vec<PayloadKind> {
    let known = |kind: PayloadKind| match kind {
        PayloadKind::LogEntry
        | PayloadKind::Approval
        | PayloadKind::Sponsorship
        | PayloadKind::RefUpdate
        | PayloadKind::RecordEntry
        | PayloadKind::PullOpening
        | PayloadKind::PullApproval
        | PayloadKind::CheckResult
        | PayloadKind::PullClosing
        | PayloadKind::OwnerOverride
        | PayloadKind::Challenge
        | PayloadKind::HeldStatement => kind,
    };
    vec![
        known(PayloadKind::LogEntry),
        known(PayloadKind::Approval),
        known(PayloadKind::Sponsorship),
        known(PayloadKind::RefUpdate),
        known(PayloadKind::RecordEntry),
        known(PayloadKind::PullOpening),
        known(PayloadKind::PullApproval),
        known(PayloadKind::CheckResult),
        known(PayloadKind::PullClosing),
        known(PayloadKind::OwnerOverride),
        known(PayloadKind::HeldStatement),
    ]
}

#[test]
fn the_signature_of_a_challenge_verifies_as_no_other_type_of_proof() {
    let signed = example_signed();
    let key = daniel().public();
    let payload = signed.payload();
    assert_eq!(PayloadKind::Challenge.name(), "challenge");
    assert_eq!(
        verify(
            &key,
            PayloadKind::Challenge,
            FORMAT_VERSION,
            &payload,
            signed.signature()
        ),
        Ok(())
    );
    for kind in other_kinds() {
        // The signature of a sign-in is no proof of another type over the same bytes.
        assert_eq!(
            verify(&key, kind, FORMAT_VERSION, &payload, signed.signature()),
            Err(VerifyError::Invalid),
            "{kind:?}"
        );
        // A proof of another type over the same bytes is no sign-in.
        let proof = sign(&daniel(), kind, &payload).unwrap();
        assert_eq!(
            SignedChallenge::try_new(example_challenge(), key.clone(), proof.clone()).unwrap_err(),
            VerifyError::Invalid,
            "{kind:?}"
        );
        let mut message = b"stemma-protocol 1\n".to_vec();
        message.extend_from_slice(&payload);
        message.extend_from_slice(format!("signature {}\n", signature_to_line(&proof)).as_bytes());
        assert_eq!(
            SignedChallenge::parse(&message).unwrap_err(),
            problem(5, SignInProblem::Signature),
            "{kind:?}"
        );
    }
    // A register does not take it for a log entry or an approval either.
    let register = Register::with_owner(human("daniel"), key).unwrap();
    let actor = crate::Actor::human("daniel").unwrap();
    assert!(register
        .verify_log_entry(&actor, 1, &payload, signed.signature())
        .is_err());
    assert!(register
        .verify_approval(&actor, &payload, signed.signature(), None)
        .is_err());
}

#[test]
fn a_challenge_for_another_address_is_not_signed() {
    let signer = Counting(daniel(), Cell::new(0));
    let key = daniel().public();
    let challenge = Challenge::new(other_hub(), example_challenge().nonce().clone());
    let error = challenge.answer(&hub(), &key, &signer).unwrap_err();
    assert!(
        matches!(
            &error,
            AnswerError::OtherAddress { named, called }
                if named == &other_hub() && called == &hub()
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "the challenge is one of the hub at https://other.example, and this side calls \
         https://hub.example; nothing was signed"
    );
    assert!(error.io_error().is_none());
    check_rejection(&error);
    assert_eq!(signer.1.get(), 0, "the signer was asked");

    // The address is compared byte for byte: a port, a path, or another case is another one.
    for named in [
        "https://hub.example:443",
        "https://hub.example/stemma",
        "https://HUB.example",
        "http://hub.example",
    ] {
        let challenge = Challenge::new(
            HubAddress::try_new(named).unwrap(),
            example_challenge().nonce().clone(),
        );
        assert!(
            matches!(
                challenge.answer(&hub(), &key, &signer),
                Err(AnswerError::OtherAddress { .. })
            ),
            "{named}"
        );
    }
    assert_eq!(signer.1.get(), 0, "the signer was asked");

    assert!(example_challenge().answer(&hub(), &key, &signer).is_ok());
    assert_eq!(signer.1.get(), 1, "the signer is asked once");
}

#[test]
fn a_signer_that_fails_or_holds_another_key_gives_no_answer() {
    let challenge = example_challenge();
    let error = challenge
        .answer(&hub(), &daniel().public(), &Broken)
        .unwrap_err();
    assert!(
        matches!(error, AnswerError::Seal(SealError::Sign(_))),
        "{error:?}"
    );
    check_rejection(&error);

    let error = challenge
        .answer(&hub(), &daniel().public(), &Seeded::new(7))
        .unwrap_err();
    assert!(
        matches!(
            error,
            AnswerError::Seal(SealError::Verify(VerifyError::WrongKey))
        ),
        "{error:?}"
    );
    check_rejection(&error);
}

#[test]
fn a_hub_refuses_a_challenge_that_was_signed_for_another_address() {
    let key = daniel().public();
    let register = Register::with_owner(human("daniel"), key.clone()).unwrap();
    // The client called another hub, which issued this challenge; the signature verifies.
    let signed = Challenge::new(other_hub(), example_challenge().nonce().clone())
        .answer(&other_hub(), &key, &daniel())
        .unwrap();
    assert_eq!(signed.check(&other_hub(), accepted), Ok(()));
    assert_eq!(signed.check(&hub(), accepted), Err(Refused::OtherAddress));
    // The address is judged first, so an active key does not help.
    assert_eq!(
        signed.actor(&hub(), accepted, &register),
        Err(Refused::OtherAddress)
    );
    assert_eq!(
        signed.actor(&other_hub(), accepted, &register),
        Ok(&human("daniel"))
    );
    check_rejection(&Refused::OtherAddress);
    // What travelled is refused as well: the reader accepts it, the check does not.
    let read = SignedChallenge::parse(&signed.encode()).unwrap();
    assert_eq!(read.check(&hub(), accepted), Err(Refused::OtherAddress));
}

#[test]
fn the_hub_compares_the_address_byte_for_byte() {
    let signed = example_signed();
    assert_eq!(signed.check(&hub(), accepted), Ok(()));
    // A port, a path, another case, or another scheme is another address for the hub as well.
    for own in [
        "https://hub.example:443",
        "https://hub.example/stemma",
        "https://HUB.example",
        "http://hub.example",
    ] {
        let own = HubAddress::try_new(own).unwrap();
        assert_eq!(
            signed.check(&own, accepted),
            Err(Refused::OtherAddress),
            "{own}"
        );
    }
}

#[test]
fn a_nonce_that_the_hub_does_not_accept_does_not_sign_in() {
    let key = daniel().public();
    let register = Register::with_owner(human("daniel"), key).unwrap();
    let signed = example_signed();

    // The hub is asked about the nonce of the message, and its answer decides.
    let seen = Cell::new(None);
    let look = |nonce: &Nonce| {
        seen.set(Some(nonce.clone()));
        true
    };
    assert_eq!(signed.check(&hub(), look), Ok(()));
    assert_eq!(seen.take().as_ref(), Some(example_challenge().nonce()));
    assert_eq!(signed.check(&hub(), stale), Err(Refused::Nonce));
    assert_eq!(signed.actor(&hub(), stale, &register), Err(Refused::Nonce));
    check_rejection(&Refused::Nonce);

    // The address is judged before the nonce: the hub is not asked about the nonce of a
    // challenge that was signed for another address.
    let never = |_: &Nonce| -> bool { panic!("the hub was asked about the nonce") };
    assert_eq!(
        signed.check(&other_hub(), never),
        Err(Refused::OtherAddress)
    );
    // The nonce is judged before the register: an unknown key learns nothing about it.
    let stranger = Seeded::new(4);
    let unknown = example_challenge()
        .answer(&hub(), &stranger.public(), &stranger)
        .unwrap();
    assert_eq!(unknown.actor(&hub(), stale, &register), Err(Refused::Nonce));
    assert_eq!(
        unknown.actor(&hub(), accepted, &register),
        Err(Refused::NotActive)
    );
}

#[test]
fn the_check_of_the_fuzz_target_holds_for_a_signature_that_does_not_verify() {
    // The seeds carry signatures that verify, so the branch of the check for a message whose
    // form is right and whose signature is wrong is run here.
    let signed = text(example_signed().encode());
    let other = Seeded::new(7).public();
    for forged in [
        signed.replacen("nonce 00", "nonce ff", 1),
        signed.replacen(&daniel().public().to_string(), &other.to_string(), 1),
    ] {
        assert!(SignedChallenge::parse_unverified(forged.as_bytes()).is_ok());
        assert!(SignedChallenge::parse(forged.as_bytes()).is_err());
        crate::fuzz::protocol(forged.as_bytes());
    }
}

#[test]
fn a_key_that_is_not_active_in_the_register_does_not_sign_in() {
    let (first, second, erin, stranger) = (
        Seeded::new(1),
        Seeded::new(2),
        Seeded::new(3),
        Seeded::new(4),
    );
    let answer = |signer: &Seeded| {
        example_challenge()
            .answer(&hub(), &signer.public(), signer)
            .unwrap()
    };
    let daniel = human("daniel");
    let mut register = Register::with_owner(daniel.clone(), first.public()).unwrap();
    register
        .add_human(human("erin"), Role::Member, erin.public())
        .unwrap();
    register.add_key(&daniel, second.public()).unwrap();

    // Every active key signs in, as the identity of its entry.
    assert_eq!(
        answer(&first).actor(&hub(), accepted, &register),
        Ok(&daniel)
    );
    assert_eq!(
        answer(&second).actor(&hub(), accepted, &register),
        Ok(&daniel)
    );
    assert_eq!(
        answer(&erin).actor(&hub(), accepted, &register),
        Ok(&human("erin"))
    );

    // A key that the register does not hold. Its signature verifies all the same.
    assert_eq!(
        answer(&stranger).actor(&hub(), accepted, &register),
        Err(Refused::NotActive)
    );
    assert_eq!(answer(&stranger).check(&hub(), accepted), Ok(()));

    // A retired key signs nothing new, whatever the number from which it is retired.
    register.retire(&daniel, &first.public(), u64::MAX).unwrap();
    assert_eq!(
        answer(&first).actor(&hub(), accepted, &register),
        Err(Refused::NotActive)
    );
    assert_eq!(
        answer(&second).actor(&hub(), accepted, &register),
        Ok(&daniel)
    );

    // A revoked key does not either.
    register
        .revoke(&human("erin"), &erin.public(), u64::MAX)
        .unwrap();
    assert_eq!(
        answer(&erin).actor(&hub(), accepted, &register),
        Err(Refused::NotActive)
    );
    check_rejection(&Refused::NotActive);
}

#[test]
fn an_agent_signs_in_with_the_key_of_its_register_entry() {
    let (owner, bot) = (Seeded::new(1), Seeded::new(5));
    let daniel = human("daniel");
    let agent: Identity = "agent bot".parse().unwrap();
    let mut register = Register::with_owner(daniel.clone(), owner.public()).unwrap();
    let cosign = sign(
        &owner,
        PayloadKind::Sponsorship,
        &sponsorship_payload(&agent, Role::Member, &daniel),
    )
    .unwrap();
    register
        .add_agent(
            agent.clone(),
            Role::Member,
            daniel.clone(),
            bot.public(),
            cosign,
        )
        .unwrap();
    let signed = example_challenge()
        .answer(&hub(), &bot.public(), &bot)
        .unwrap();
    assert_eq!(signed.actor(&hub(), accepted, &register), Ok(&agent));

    // The revocation of the sponsor's key takes the key of the agent with it.
    register.add_key(&daniel, Seeded::new(6).public()).unwrap();
    register.revoke(&daniel, &owner.public(), 9).unwrap();
    assert_eq!(
        signed.actor(&hub(), accepted, &register),
        Err(Refused::NotActive)
    );
}

#[test]
fn no_output_shows_a_token() {
    let token = Token::try_new(TOKEN).unwrap();
    assert_eq!(format!("{token:?}"), "Token(..)");
    assert_eq!(format!("{token:#?}"), "Token(..)");
    let answer = example_token_answer();
    for shown in [format!("{answer:?}"), format!("{answer:#?}")] {
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains("Token(..)"), "{shown}");
    }
    // A token answer that its reader refuses is reported without a part of the token: by the line
    // and the rule alone.
    let secret = "c2VjcmV0LXRva2Vu";
    for message in [
        format!("stemma-protocol 1\ntoken {secret} {secret}\nlifetime 600\n"),
        format!("stemma-protocol 1\ntoken {secret}=x\nlifetime 600\n"),
        format!("stemma-protocol 1\ntoken {secret}\nlifetime 0\n"),
        format!("stemma-protocol 1\ntoken {secret}\nlifetime 600\ntoken {secret}\n"),
        format!("stemma-protocol 1\ntoken {secret}\n"),
        format!("stemma-protocol 1\ntoken {secret}"),
        format!("stemma-protocol 2\ntoken {secret}\nlifetime 600\n"),
        format!("stemma-protocol 1\nlifetime 600\ntoken {secret}\n"),
    ] {
        let error = TokenAnswer::parse(message.as_bytes()).unwrap_err();
        for shown in [error.to_string(), format!("{error:?}")] {
            assert!(!shown.contains(secret), "{shown}");
        }
        // The answer that a reader of this module would give to the message does not either.
        let answer = text(crate::protocol::ErrorMessage::from(&error).encode());
        assert!(!answer.contains(secret), "{answer}");
    }
}

#[test]
fn two_tokens_are_equal_if_every_byte_is() {
    let token = |text: &str| Token::try_new(text).unwrap();
    assert_eq!(token("abc"), token("abc"));
    assert_ne!(token("abc"), token("abd"));
    assert_ne!(token("abc"), token("bbc"));
    assert_ne!(token("abc"), token("abcd"));
    assert_ne!(token("abc"), token("ab"));
    assert_ne!(token("abc"), token("abc="));
}

#[test]
fn every_problem_has_a_message() {
    for reason in [
        SignInProblem::Nonce,
        SignInProblem::SignatureForm,
        SignInProblem::Key,
        SignInProblem::Signature,
        SignInProblem::Token,
    ] {
        check_rejection(&reason);
        let error = problem(3, reason);
        assert_eq!(error.to_string(), format!("line 3: {reason}"));
        check_rejection(&error);
    }
}
