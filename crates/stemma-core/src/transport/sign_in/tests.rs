//! The tests of the sign-in over a transport: the two requests travel through
//! [`MemoryTransport`], the token stays in the transport and shows in no output, a challenge for
//! another address is not signed, and every way in which a sign-in fails leaves the transport as
//! it was.

use std::cell::{Cell, RefCell};
use std::io::{self, BufReader, Cursor, Write};
use std::num::NonZeroU64;

use super::*;
use crate::fuzz::check_rejection;
use crate::protocol::examples::{self, Message};
use crate::protocol::record::tests::Seeded;
use crate::protocol::sign_in::tests::{example_challenge, example_signed, example_token_answer};
use crate::protocol::sign_in::{Refused, SignedChallenge, Token, TokenAnswer};
use crate::protocol::{FetchRequest, RefList, Spoken};
use crate::register::{Identity, Register};
use crate::signature::{SignError, Signature};
use crate::transport::{MemoryTransport, OtherSide, Refusal, SignInSide};

/// The beginning of every token that the hub of these tests issues. The tests look for it in
/// what is printed.
const TOKEN: &str = "dGhlLXRva2VuLW9mLXRoZXNlLXRlc3Rz";

/// The token of the sign-in with this number, from 1: every sign-in gets another one.
fn token(number: usize) -> String {
    format!("{TOKEN}.{number}")
}

fn address(text: &str) -> HubAddress {
    HubAddress::try_new(text).unwrap()
}

fn hub() -> HubAddress {
    address("https://hub.example")
}

fn other_hub() -> HubAddress {
    address("https://other.example")
}

/// The key of the sender of these tests, which the register of [`Door`] holds.
fn daniel() -> Seeded {
    Seeded::new(1)
}

/// A signer that counts how often it is asked.
struct Counting(Seeded, Cell<u32>);

impl Counting {
    fn new(signer: Seeded) -> Self {
        Self(signer, Cell::new(0))
    }
}

impl Signer for Counting {
    fn sign(&self, message: &[u8]) -> Result<Signature, SignError> {
        self.1.set(self.1.get() + 1);
        self.0.sign(message)
    }
}

/// The other side of these tests: a hub with one address and a register, which issues a token
/// to the active keys of the register, as a hub does with the checks of the core.
#[derive(Debug)]
struct Door {
    /// The address that the hub knows as its own.
    own: HubAddress,
    /// The address that its challenges name; another one only where a test says so.
    named: HubAddress,
    register: Register,
    /// Who was issued a token, in order.
    issued: RefCell<Vec<Identity>>,
    /// How often a token answer was asked for.
    asked: Cell<u32>,
    /// The error that the hub answers in place of a token answer, where a test sets one.
    refusing: Cell<Option<fn() -> ErrorMessage>>,
}

impl Door {
    fn new() -> Self {
        let owner = Identity::human("daniel").unwrap();
        Self {
            own: hub(),
            named: hub(),
            register: Register::with_owner(owner, daniel().public()).unwrap(),
            issued: RefCell::default(),
            asked: Cell::new(0),
            refusing: Cell::new(None),
        }
    }
}

fn not_asked<T>() -> Result<T, Refusal> {
    Err(Refusal::Failed("these tests ask for no objects".to_owned()))
}

impl OtherSide for Door {
    fn refs(&self) -> Result<RefList, Refusal> {
        not_asked()
    }

    fn fetch(&self, _request: &FetchRequest, _stream: &mut dyn Write) -> Result<(), Refusal> {
        not_asked()
    }

    fn sign_in(&self) -> Option<&dyn SignInSide> {
        Some(self)
    }
}

impl SignInSide for Door {
    fn challenge(&self) -> Result<Challenge, Refusal> {
        Ok(Challenge::new(
            self.named.clone(),
            example_challenge().nonce().clone(),
        ))
    }

    fn token(&self, answer: &SignedChallenge) -> Result<TokenAnswer, Refusal> {
        self.asked.set(self.asked.get() + 1);
        if let Some(error) = self.refusing.get() {
            return Err(error().into());
        }
        // The hub of these tests issues one nonce. This version has no kind of error for a
        // refused sign-in, so the refusal travels in the words of the carrier.
        let identity = answer
            .actor(
                &self.own,
                |nonce| nonce == example_challenge().nonce(),
                &self.register,
            )
            .map_err(|refused| Refusal::Failed(refused.to_string()))?;
        let mut issued = self.issued.borrow_mut();
        issued.push(identity.clone());
        Ok(TokenAnswer::new(
            Token::try_new(token(issued.len())).unwrap(),
            NonZeroU64::new(300).unwrap(),
        ))
    }
}

/// A side that answers the same refusal to both requests of a sign-in.
struct Refusing(fn() -> Refusal);

impl OtherSide for Refusing {
    fn refs(&self) -> Result<RefList, Refusal> {
        not_asked()
    }

    fn fetch(&self, _request: &FetchRequest, _stream: &mut dyn Write) -> Result<(), Refusal> {
        not_asked()
    }

    fn sign_in(&self) -> Option<&dyn SignInSide> {
        Some(self)
    }
}

impl SignInSide for Refusing {
    fn challenge(&self) -> Result<Challenge, Refusal> {
        Err((self.0)())
    }

    fn token(&self, _answer: &SignedChallenge) -> Result<TokenAnswer, Refusal> {
        Err((self.0)())
    }
}

/// A side that implements the requests for objects only, as a side without a sign-in does.
struct NoSignIn;

impl OtherSide for NoSignIn {
    fn refs(&self) -> Result<RefList, Refusal> {
        not_asked()
    }

    fn fetch(&self, _request: &FetchRequest, _stream: &mut dyn Write) -> Result<(), Refusal> {
        not_asked()
    }
}

/// A reader that fails.
struct Broken;

impl Read for Broken {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("the connection broke"))
    }
}

/// What a [`Scripted`] transport answers to a request.
#[derive(Clone)]
enum Script {
    Message(Vec<u8>),
    Error(Vec<u8>),
    BrokenBody,
    Accepted,
}

impl Script {
    fn body<'a>(bytes: &[u8]) -> Body<'a> {
        Box::new(Cursor::new(bytes.to_vec()))
    }
}

/// A transport that answers fixed bytes, for the answers that no [`MemoryTransport`] gives.
struct Scripted {
    challenge: Script,
    answer: Script,
}

impl Transport for Scripted {
    fn refs(&mut self) -> Result<Answer<'_>, TransportError> {
        unreachable!("the sign-in asks for no references")
    }

    fn fetch(&mut self, _request: &mut dyn Read) -> Result<Answer<'_>, TransportError> {
        unreachable!("the sign-in asks for no objects")
    }

    fn challenge(&mut self) -> Result<Answer<'_>, TransportError> {
        Ok(match &self.challenge {
            Script::Message(bytes) => Answer::Message(Script::body(bytes)),
            Script::Error(bytes) => Answer::Error(Script::body(bytes)),
            Script::BrokenBody => Answer::Message(Box::new(BufReader::new(Broken))),
            Script::Accepted => unreachable!("a challenge is no token answer"),
        })
    }

    fn answer_challenge(&mut self, request: &mut dyn Read) -> Result<SignedIn<'_>, TransportError> {
        let mut bytes = Vec::new();
        request.read_to_end(&mut bytes).unwrap();
        assert!(SignedChallenge::parse(&bytes).is_ok());
        Ok(match &self.answer {
            Script::Error(bytes) => SignedIn::Error(Script::body(bytes)),
            Script::BrokenBody => SignedIn::Error(Box::new(BufReader::new(Broken))),
            Script::Accepted => SignedIn::Accepted(Duration::from_secs(1)),
            Script::Message(_) => unreachable!("a transport hands over no token answer"),
        })
    }
}

/// The token that the transport holds. Only these tests look at it.
fn held(transport: &MemoryTransport<Door>) -> Option<String> {
    transport
        .token
        .as_ref()
        .map(|token| token.expose().to_owned())
}

fn signs_in(transport: &mut dyn Transport, signer: &Counting) -> Result<Duration, SignInError> {
    sign_in(transport, &hub(), &signer.0.public(), signer)
}

#[test]
fn a_sign_in_leaves_the_token_in_the_transport_and_returns_its_lifetime() {
    let mut transport = MemoryTransport::new(Door::new());
    let signer = Counting::new(daniel());
    assert!(!transport.holds_token());

    let lifetime = signs_in(&mut transport, &signer).unwrap();

    assert_eq!(lifetime, Duration::from_secs(300));
    assert!(transport.holds_token());
    assert_eq!(signer.1.get(), 1, "the key signs once per sign-in");
    assert_eq!(
        *transport.side().issued.borrow(),
        [Identity::human("daniel").unwrap()]
    );
    assert_eq!(held(&transport), Some(token(1)));
    // A sign-in can be repeated; the new token replaces the old one.
    signs_in(&mut transport, &signer).unwrap();
    assert!(transport.holds_token());
    assert_eq!(transport.side().issued.borrow().len(), 2);
    assert_eq!(held(&transport), Some(token(2)));
}

#[test]
fn no_output_of_a_sign_in_shows_the_token() {
    let mut transport = MemoryTransport::new(Door::new());
    assert_eq!(format!("{:?}", transport.token), "None");
    let mut request = &example_signed().encode()[..];
    let answer = transport.answer_challenge(&mut request).unwrap();
    let shown = format!("{answer:?}");
    drop(answer);
    assert_eq!(shown, "SignedIn::Accepted(300s)");
    // The transport holds the token, and it is the one that the hub issued.
    assert_eq!(held(&transport), Some(token(1)));
    // Neither the answer nor the transport as a whole shows it, with the other side inside.
    for shown in [
        shown,
        format!("{transport:?}"),
        format!("{transport:#?}"),
        format!("{:?}", transport.token),
    ] {
        assert!(!shown.contains(TOKEN), "{shown}");
    }
    assert!(format!("{transport:?}").contains("token: Some(Token(..))"));
}

#[test]
fn a_challenge_that_names_another_address_is_not_signed() {
    let mut door = Door::new();
    door.named = other_hub();
    let mut transport = MemoryTransport::new(door);
    let signer = Counting::new(daniel());

    let error = signs_in(&mut transport, &signer).unwrap_err();

    assert!(
        matches!(
            &error,
            SignInError::Answer(AnswerError::OtherAddress { named, called })
                if named == &other_hub() && called == &hub()
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "the challenge was not answered: the challenge is one of the hub at \
         https://other.example, and this side calls https://hub.example; nothing was signed"
    );
    check_rejection(&error);
    assert_eq!(signer.1.get(), 0, "the signer was asked");
    assert_eq!(transport.side().asked.get(), 0, "an answer was sent");
    assert!(!transport.holds_token());
}

#[test]
fn a_hub_grants_nothing_for_a_challenge_that_was_signed_for_another_address() {
    // The client calls the hub under an address that the hub does not know as its own, and the
    // challenge names that address, as the challenge of another hub would that is passed on.
    let mut door = Door::new();
    door.named = other_hub();
    let mut transport = MemoryTransport::new(door);
    let signer = Counting::new(daniel());

    let error = sign_in(&mut transport, &other_hub(), &daniel().public(), &signer).unwrap_err();

    assert!(
        matches!(
            &error,
            SignInError::Transport(TransportError::NoAnswer { detail })
                if *detail == Refused::OtherAddress.to_string()
        ),
        "{error:?}"
    );
    assert_eq!(signer.1.get(), 1);
    assert_eq!(transport.side().asked.get(), 1);
    assert!(transport.side().issued.borrow().is_empty());
    assert!(!transport.holds_token());
}

#[test]
fn a_key_that_is_not_active_in_the_register_gets_no_token() {
    let mut transport = MemoryTransport::new(Door::new());
    let stranger = Counting::new(Seeded::new(9));

    let error = signs_in(&mut transport, &stranger).unwrap_err();

    assert!(
        matches!(
            &error,
            SignInError::Transport(TransportError::NoAnswer { detail })
                if *detail == Refused::NotActive.to_string()
        ),
        "{error:?}"
    );
    assert!(transport.side().issued.borrow().is_empty());
    assert!(!transport.holds_token());

    // A sign-in that fails leaves the token of an earlier one where it is.
    signs_in(&mut transport, &Counting::new(daniel())).unwrap();
    signs_in(&mut transport, &stranger).unwrap_err();
    assert!(transport.holds_token());
    assert_eq!(held(&transport), Some(token(1)));
}

#[test]
fn a_sign_in_that_is_answered_with_an_error_leaves_the_earlier_token() {
    let mut transport = MemoryTransport::new(Door::new());
    let signer = Counting::new(daniel());
    signs_in(&mut transport, &signer).unwrap();

    // The hub answers an error in place of the token answer.
    transport
        .side()
        .refusing
        .set(Some(|| ErrorMessage::UnsupportedVersion(Spoken::here())));
    let error = signs_in(&mut transport, &signer).unwrap_err();
    assert!(matches!(error, SignInError::Refused(_)), "{error:?}");
    assert_eq!(held(&transport), Some(token(1)));

    // The transport answers a request that the reader refuses, and the hub is not asked.
    let asked = transport.side().asked.get();
    let mut request = &b"stemma-protocol 1\n"[..];
    assert!(matches!(
        transport.answer_challenge(&mut request).unwrap(),
        SignedIn::Error(_)
    ));
    assert_eq!(transport.side().asked.get(), asked);
    assert_eq!(held(&transport), Some(token(1)));

    // The request cannot be read.
    transport.answer_challenge(&mut Broken).unwrap_err();
    assert_eq!(held(&transport), Some(token(1)));
}

#[test]
fn a_nonce_that_the_hub_did_not_issue_gets_no_token() {
    let mut transport = MemoryTransport::new(Door::new());
    let other = Challenge::new(hub(), "ab".repeat(32).parse().unwrap());
    let signed = other.answer(&hub(), &daniel().public(), &daniel()).unwrap();

    let mut request = &signed.encode()[..];
    let failure = transport.answer_challenge(&mut request).unwrap_err();

    assert!(
        matches!(&failure, TransportError::NoAnswer { detail }
            if *detail == Refused::Nonce.to_string()),
        "{failure}"
    );
    assert!(transport.side().issued.borrow().is_empty());
    assert!(!transport.holds_token());
}

#[test]
fn an_error_of_the_other_side_travels_in_place_of_the_challenge_and_of_the_token_answer() {
    let refusal = || Refusal::from(ErrorMessage::UnsupportedVersion(Spoken::here()));
    let expected = ErrorMessage::UnsupportedVersion(Spoken::here());
    let signer = Counting::new(daniel());

    // In place of the challenge.
    let mut transport = MemoryTransport::new(Refusing(refusal));
    let error = signs_in(&mut transport, &signer).unwrap_err();
    assert!(
        matches!(&error, SignInError::Refused(message) if message == &expected),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!("the sign-in was refused: {expected}")
    );
    check_rejection(&error);
    assert_eq!(signer.1.get(), 0);

    // In place of the token answer.
    let mut request = &example_signed().encode()[..];
    let SignedIn::Error(mut body) = transport.answer_challenge(&mut request).unwrap() else {
        panic!("expected an error");
    };
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).unwrap();
    drop(body);
    assert_eq!(ErrorMessage::parse(&bytes).unwrap(), expected);
    assert!(!transport.holds_token());

    let mut scripted = Scripted {
        challenge: Script::Message(example_challenge().encode()),
        answer: Script::Error(expected.encode()),
    };
    let error = signs_in(&mut scripted, &signer).unwrap_err();
    assert!(
        matches!(&error, SignInError::Refused(message) if message == &expected),
        "{error:?}"
    );
}

#[test]
fn an_answer_that_the_reader_refuses_is_answered_with_malformed_and_the_side_is_not_asked() {
    let mut transport = MemoryTransport::new(Door::new());
    let signed = String::from_utf8(example_signed().encode()).unwrap();
    // A changed nonce: the form is right, and the signature is for another challenge.
    let forged = signed.replacen("nonce 00", "nonce ff", 1);

    for request in [&b"stemma-protocol 1\nhub x\n"[..], forged.as_bytes()] {
        let refused = SignedChallenge::parse(request).unwrap_err();
        let mut bytes = request;
        let SignedIn::Error(mut body) = transport.answer_challenge(&mut bytes).unwrap() else {
            panic!("expected an error");
        };
        let mut answer = Vec::new();
        body.read_to_end(&mut answer).unwrap();
        assert_eq!(
            ErrorMessage::parse(&answer).unwrap(),
            ErrorMessage::from(&refused)
        );
    }
    let mut other = b"stemma-protocol 2\n".as_slice();
    assert!(matches!(
        transport.answer_challenge(&mut other).unwrap(),
        SignedIn::Error(_)
    ));
    assert_eq!(transport.side().asked.get(), 0);
    assert!(!transport.holds_token());

    let failure = transport.answer_challenge(&mut Broken).unwrap_err();
    assert!(matches!(failure, TransportError::Request(_)), "{failure}");
    assert_eq!(transport.side().asked.get(), 0);
}

#[test]
fn a_side_without_a_sign_in_issues_no_challenge_and_grants_no_token() {
    let mut transport = MemoryTransport::new(NoSignIn);
    let no_sign_in = |failure: &TransportError| {
        matches!(failure, TransportError::NoAnswer { detail }
            if detail == "this side has no sign-in")
    };

    let failure = transport.challenge().unwrap_err();
    assert!(no_sign_in(&failure), "{failure}");
    let mut request = &example_signed().encode()[..];
    let failure = transport.answer_challenge(&mut request).unwrap_err();
    assert!(no_sign_in(&failure), "{failure}");
    // A request that no reader accepts gets the same answer: the side reads none.
    let mut request = &b"stemma-protocol 2\n"[..];
    let failure = transport.answer_challenge(&mut request).unwrap_err();
    assert!(no_sign_in(&failure), "{failure}");

    let signer = Counting::new(daniel());
    let error = signs_in(&mut transport, &signer).unwrap_err();
    assert!(
        matches!(&error, SignInError::Transport(failure) if no_sign_in(failure)),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "the sign-in got no answer: the other side gave no answer of the protocol: this side \
         has no sign-in"
    );
    assert!(error.io_error().is_none());
    check_rejection(&error);
    assert_eq!(signer.1.get(), 0);
    assert!(!transport.holds_token());
}

#[test]
fn an_answer_that_cannot_be_read_is_an_error_of_the_sign_in() {
    let signer = Counting::new(daniel());
    let challenge = example_challenge().encode();
    let scripted = |challenge: Script, answer: Script| Scripted { challenge, answer };
    let fails = |mut transport: Scripted| {
        let error = signs_in(&mut transport, &signer).unwrap_err();
        check_rejection(&error);
        error
    };

    // The challenge is no challenge.
    let error = fails(scripted(
        Script::Message(example_token_answer().encode()),
        Script::Accepted,
    ));
    assert!(matches!(error, SignInError::Challenge(_)), "{error:?}");
    assert!(error.to_string().starts_with("cannot read the challenge: "));

    // The challenge is longer than the bound, by one byte; at the bound it is read.
    let mut long = challenge.clone();
    long.resize(MAX_SIGN_IN_MESSAGE as usize + 1, b'x');
    let error = fails(scripted(Script::Message(long.clone()), Script::Accepted));
    assert!(matches!(error, SignInError::TooLarge), "{error:?}");
    assert_eq!(
        error.to_string(),
        "an answer of the sign-in is longer than 65536 bytes"
    );
    long.pop();
    let error = fails(scripted(Script::Message(long), Script::Accepted));
    assert!(matches!(error, SignInError::Challenge(_)), "{error:?}");

    // The body of the challenge breaks.
    let error = fails(scripted(Script::BrokenBody, Script::Accepted));
    assert!(matches!(error, SignInError::Body(_)), "{error:?}");
    assert!(error.io_error().is_some());
    assert_eq!(
        error.to_string(),
        "cannot read an answer of the sign-in: the connection broke"
    );

    // The error in place of the challenge is no error message.
    let error = fails(scripted(Script::Error(challenge.clone()), Script::Accepted));
    assert!(matches!(error, SignInError::Error(_)), "{error:?}");
    assert!(error
        .to_string()
        .starts_with("cannot read the error that answers the sign-in: "));

    // The same for the error in place of the token answer.
    let error = fails(scripted(
        Script::Message(challenge.clone()),
        Script::Error(b"<html>".to_vec()),
    ));
    assert!(matches!(error, SignInError::Error(_)), "{error:?}");
    let error = fails(scripted(
        Script::Message(challenge.clone()),
        Script::BrokenBody,
    ));
    assert!(matches!(error, SignInError::Body(_)), "{error:?}");

    // The signer was asked for the two sign-ins whose challenge was read.
    assert_eq!(signer.1.get(), 2);
    assert_eq!(
        signs_in(
            &mut scripted(Script::Message(challenge), Script::Accepted),
            &signer
        )
        .unwrap(),
        Duration::from_secs(1)
    );
}

#[test]
fn the_example_messages_of_the_sign_in_travel_byte_for_byte() {
    let example = |message: Message| {
        examples::ALL
            .iter()
            .find(|example| example.message == message)
            .unwrap()
            .bytes
    };
    let mut transport = MemoryTransport::new(Door::new());

    let Answer::Message(mut body) = transport.challenge().unwrap() else {
        panic!("expected the challenge");
    };
    let mut challenge = Vec::new();
    body.read_to_end(&mut challenge).unwrap();
    drop(body);
    assert_eq!(challenge, example(Message::Challenge));

    let mut request = example(Message::SignedChallenge);
    let answer = transport.answer_challenge(&mut request).unwrap();
    assert!(
        matches!(answer, SignedIn::Accepted(lifetime) if lifetime == Duration::from_secs(300)),
        "{answer:?}"
    );
    // The example of the token answer is read by the reader that a transport uses.
    assert_eq!(
        TokenAnswer::parse(example(Message::TokenAnswer)).unwrap(),
        example_token_answer()
    );
}

#[test]
fn the_debug_form_of_a_refused_sign_in_names_the_case() {
    let mut transport = MemoryTransport::new(Refusing(|| {
        Refusal::from(ErrorMessage::UnsupportedVersion(Spoken::here()))
    }));
    let mut request = &example_signed().encode()[..];
    let answer = transport.answer_challenge(&mut request).unwrap();
    assert_eq!(format!("{answer:?}"), "SignedIn::Error(..)");
}
