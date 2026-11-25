//! The token of a sign-in: kept by the transport, sent as `Authorization: Bearer` with the
//! requests that follow, and in no error, no `Debug` form, and no log record.
//!
//! The test installs a logger for the whole process, so it is the one test of this file.

mod support;

use std::num::NonZeroU64;
use std::sync::Mutex;
use std::time::Duration;

use log::{Level, LevelFilter, Log, Metadata, Record};
use stemma_core::protocol::sign_in::{Token, TokenAnswer};
use stemma_core::transport::{Answer, SignedIn, Transport, TransportError};
use stemma_remote::{HttpTransport, Timeouts, ERROR_MEDIA_TYPE};
use support::{example, message, response, Action, Responder, REPOSITORY_PATH};

/// A token that no other text of the test holds.
const SECRET: &str = "s3cr3t-T0KEN-of-the-hub-4f9a2c71e8b5d063";

/// Every log record of the process, with its level.
static RECORDS: Mutex<Vec<(Level, String)>> = Mutex::new(Vec::new());

struct Capture;

impl Log for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        RECORDS.lock().unwrap().push((
            record.level(),
            format!("{} {}", record.target(), record.args()),
        ));
    }

    fn flush(&self) {}
}

/// Whether the text holds the token or a piece of it of eight characters, as a dump of the raw
/// bytes in rows would.
fn holds_token(text: &str) -> bool {
    SECRET
        .as_bytes()
        .windows(8)
        .any(|piece| text.contains(std::str::from_utf8(piece).unwrap()))
}

fn error_text(error: &TransportError) -> String {
    format!("{error} {error:?}")
}

#[test]
fn the_token_goes_with_the_requests_and_into_no_error_and_no_log() {
    log::set_logger(&Capture).unwrap();
    log::set_max_level(LevelFilter::Trace);

    let token_answer = TokenAnswer::new(
        Token::try_new(SECRET).unwrap(),
        NonZeroU64::new(600).unwrap(),
    )
    .encode();
    let page: &[u8] = b"<html>Unauthorized</html>";
    let responder = Responder::start(vec![
        Action::Respond(message(example("challenge"))),
        Action::Respond(message(&token_answer)),
        Action::Respond(message(example("refs"))),
        Action::Respond(response(
            401,
            &[
                ("WWW-Authenticate", "Bearer"),
                ("Content-Type", "text/html"),
            ],
            page,
        )),
        Action::Respond(response(
            302,
            &[("Location", "https://elsewhere.example/x")],
            b"",
        )),
        Action::Close,
        // A second sign-in that is refused keeps the token of the first.
        Action::Respond(response(
            403,
            &[("Content-Type", ERROR_MEDIA_TYPE)],
            example("error-malformed"),
        )),
        Action::Respond(message(example("refs"))),
    ]);
    let mut transport = HttpTransport::new(&responder.remote(), Timeouts::DEFAULT).unwrap();
    let mut texts = Vec::new();

    assert!(matches!(transport.challenge().unwrap(), Answer::Message(_)));
    let signed = example("signed-challenge");
    match transport.answer_challenge(&mut &signed[..]).unwrap() {
        SignedIn::Accepted(lifetime) => assert_eq!(lifetime, Duration::from_secs(600)),
        other => panic!("expected a token, got {other:?}"),
    }
    assert!(transport.holds_token());
    texts.push(format!("{transport:?}"));

    assert!(matches!(transport.refs().unwrap(), Answer::Message(_)));
    for _ in 0..3 {
        let error = transport.refs().unwrap_err();
        texts.push(error_text(&error));
    }
    let refused = transport.answer_challenge(&mut &signed[..]).unwrap();
    assert!(matches!(refused, SignedIn::Error(_)));
    drop(refused);
    assert!(transport.holds_token());
    assert!(matches!(transport.refs().unwrap(), Answer::Message(_)));
    texts.push(format!("{transport:?}"));

    // The two requests of a sign-in carry no token; every other request after it carries it.
    let requests = responder.requests();
    let authorization: Vec<(String, Option<&str>)> = requests
        .iter()
        .map(|request| (request.path.clone(), request.header("authorization")))
        .collect();
    let bearer = format!("Bearer {SECRET}");
    let expected = [
        ("challenge", None),
        ("token", None),
        ("refs", Some(bearer.as_str())),
        ("refs", Some(bearer.as_str())),
        ("refs", Some(bearer.as_str())),
        ("refs", Some(bearer.as_str())),
        ("token", None),
        ("refs", Some(bearer.as_str())),
    ];
    assert_eq!(authorization.len(), expected.len());
    for ((path, found), (endpoint, wanted)) in authorization.iter().zip(expected) {
        assert_eq!(path, &format!("{REPOSITORY_PATH}/{endpoint}"));
        assert_eq!(*found, wanted, "{path}");
    }

    for text in &texts {
        assert!(!holds_token(text), "the token is in `{text}`");
    }
    let records = RECORDS.lock().unwrap();
    // The logger saw the requests of `ureq`, with the header of the token redacted, so the check
    // below has something to look at.
    assert!(records
        .iter()
        .any(|(_, text)| text.contains("Request {") && text.contains("REDACTED")));
    for (level, text) in records.iter() {
        assert_ne!(*level, Level::Trace, "a record at the level trace: {text}");
        assert!(
            !holds_token(text),
            "the token is in the log record `{text}`"
        );
    }
}
