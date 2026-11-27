//! The content encoding of the body (ADR-023, ADR-029), against the scripted responder: an answer
//! compressed with zstd and one without compression, an upload compressed only after the hub
//! named zstd, the bound of the window, a body of zstd that cannot be decoded, and a content
//! encoding that the client does not read.

mod support;

use std::io::{self, Read, Write};

use stemma_core::transport::{Answer, Transport, TransportError};
use stemma_remote::{
    HttpTransport, Timeouts, WindowTooLarge, ERROR_MEDIA_TYPE, MAX_WINDOW, MESSAGE_MEDIA_TYPE,
};
use support::{example, message, response, Action, Responder};
use zstd::stream::raw::CParameter;

fn transport(responder: &Responder) -> HttpTransport {
    HttpTransport::new(&responder.remote(), Timeouts::DEFAULT).unwrap()
}

/// `bytes` compressed with zstd at level 3, with a window of 2^`window_log` bytes. The size of
/// the content is not named in the frame, as for a stream, so the frame names the window.
fn compress(bytes: &[u8], window_log: u32) -> Vec<u8> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    encoder
        .set_parameter(CParameter::WindowLog(window_log))
        .unwrap();
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

/// An answer with a message of the protocol, compressed with zstd.
fn compressed_message(body: &[u8], window_log: u32) -> Vec<u8> {
    response(
        200,
        &[
            ("Content-Type", MESSAGE_MEDIA_TYPE),
            ("Content-Encoding", "zstd"),
        ],
        &compress(body, window_log),
    )
}

/// An answer with a message of the protocol, without compression, from a hub that takes an
/// upload compressed in the way that `accept` names.
fn message_accepting(body: &[u8], accept: &str) -> Vec<u8> {
    response(
        200,
        &[
            ("Content-Type", MESSAGE_MEDIA_TYPE),
            ("Accept-Encoding", accept),
        ],
        body,
    )
}

fn body(answer: Answer<'_>) -> (bool, Box<dyn io::BufRead + '_>) {
    match answer {
        Answer::Message(body) => (true, body),
        Answer::Error(body) => (false, body),
    }
}

fn read_all(answer: Answer<'_>) -> (bool, Vec<u8>) {
    let (is_message, mut body) = body(answer);
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).unwrap();
    (is_message, bytes)
}

fn read_error(answer: Answer<'_>) -> io::Error {
    let (_, mut body) = body(answer);
    let mut bytes = Vec::new();
    let error = body.read_to_end(&mut bytes).unwrap_err();
    // The error stays: a second read does not give an end, which could pass for the end of the
    // body.
    let again = body.read(&mut [0; 16]).unwrap_err();
    assert_eq!(again.kind(), error.kind());
    assert_eq!(again.to_string(), error.to_string());
    error
}

fn no_answer(result: Result<Answer<'_>, TransportError>) -> String {
    match result {
        Err(TransportError::NoAnswer { detail }) => detail,
        other => panic!("expected no answer, got {other:?}"),
    }
}

#[test]
fn every_request_names_zstd_in_accept_encoding() {
    let responder = Responder::start(vec![
        Action::Respond(message(example("refs"))),
        Action::Respond(message(example("objects"))),
    ]);
    let mut transport = transport(&responder);
    read_all(transport.refs().unwrap());
    read_all(transport.fetch(&mut &example("fetch")[..]).unwrap());
    for request in responder.requests() {
        assert_eq!(request.header("accept-encoding"), Some("zstd"));
    }
}

#[test]
fn an_answer_compressed_with_zstd_is_decoded_and_one_without_is_read_as_it_is() {
    let objects = example("objects");
    let refs = example("refs");
    let error = example("error-malformed");
    let responder = Responder::start(vec![
        Action::Respond(compressed_message(objects, 27)),
        Action::Respond(message(refs)),
        Action::Respond(response(
            400,
            &[
                ("Content-Type", ERROR_MEDIA_TYPE),
                ("Content-Encoding", "identity, zstd"),
            ],
            &compress(error, 20),
        )),
    ]);
    let mut transport = transport(&responder);
    assert_eq!(
        read_all(transport.fetch(&mut &example("fetch")[..]).unwrap()),
        (true, objects.to_vec())
    );
    assert_eq!(read_all(transport.refs().unwrap()), (true, refs.to_vec()));
    assert_eq!(read_all(transport.refs().unwrap()), (false, error.to_vec()));
}

#[test]
fn a_large_answer_compressed_with_zstd_streams() {
    // Eight MiB: more than any buffer of the client, of the decoder, or of the responder.
    let large: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let responder = Responder::start(vec![Action::Respond(compressed_message(&large, 27))]);
    let (_, body) = read_all(transport(&responder).refs().unwrap());
    assert!(body == large);
}

#[test]
fn an_upload_is_compressed_only_after_the_hub_named_zstd() {
    let fetch = example("fetch");
    let update = example("ref-update");
    let refs = example("refs");
    let responder = Responder::start(vec![
        // Nothing named yet: the first upload goes as it is.
        Action::Respond(message_accepting(refs, "gzip")),
        Action::Respond(message_accepting(refs, "gzip, zstd")),
        Action::Respond(message(refs)),
        // The last answer named no zstd, so the upload goes as it is again.
        Action::Respond(message(example("receipt"))),
    ]);
    let mut transport = transport(&responder);
    read_all(transport.fetch(&mut &fetch[..]).unwrap());
    read_all(transport.refs().unwrap());
    read_all(transport.ref_update(&mut &update[..]).unwrap());
    read_all(transport.ref_update(&mut &update[..]).unwrap());

    let requests = responder.requests();
    assert_eq!(requests[0].header("content-encoding"), None);
    assert_eq!(requests[0].body, fetch);
    assert_eq!(requests[2].header("content-encoding"), Some("zstd"));
    assert_eq!(requests[2].header("content-type"), Some(MESSAGE_MEDIA_TYPE));
    assert_ne!(requests[2].body, update);
    assert_eq!(zstd::decode_all(&requests[2].body[..]).unwrap(), update);
    assert_eq!(requests[3].header("content-encoding"), None);
    assert_eq!(requests[3].body, update);
}

#[test]
fn a_large_upload_is_compressed_with_the_long_window() {
    // Two copies of the same 16 MiB, so the second one is found only with a window longer than
    // 16 MiB: with long-distance matching and the window of 128 MiB the body is about the size
    // of one copy compressed.
    let half: Vec<u8> = (0..16u32 * 1024 * 1024)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let large = [half.clone(), half].concat();
    let responder = Responder::start(vec![
        Action::Respond(message_accepting(example("refs"), "zstd")),
        Action::Respond(message(example("objects"))),
    ]);
    let mut transport = transport(&responder);
    read_all(transport.refs().unwrap());
    read_all(transport.fetch(&mut &large[..]).unwrap());
    let sent = &responder.requests()[1].body;
    assert!(sent.len() < large.len() * 3 / 4, "{} bytes", sent.len());
    assert!(zstd::decode_all(&sent[..]).unwrap() == large);
}

#[test]
fn a_weight_of_zero_names_zstd_as_not_acceptable() {
    let fetch = example("fetch");
    let mut actions = Vec::new();
    for accept in ["zstd;q=0", "zstd; q=0.000", "gzip;q=1, zstd;q=0.0"] {
        actions.push(Action::Respond(message_accepting(example("refs"), accept)));
        actions.push(Action::Respond(message(example("objects"))));
    }
    actions.push(Action::Respond(message_accepting(
        example("refs"),
        "ZSTD;q=0.5",
    )));
    actions.push(Action::Respond(message(example("objects"))));
    let responder = Responder::start(actions);
    let mut transport = transport(&responder);
    for _ in 0..4 {
        read_all(transport.refs().unwrap());
        read_all(transport.fetch(&mut &fetch[..]).unwrap());
    }
    let encodings: Vec<Option<String>> = responder
        .requests()
        .iter()
        .skip(1)
        .step_by(2)
        .map(|request| request.header("content-encoding").map(str::to_owned))
        .collect();
    assert_eq!(encodings, [None, None, None, Some("zstd".to_owned())]);
}

#[test]
fn an_error_of_the_body_of_a_compressed_upload_is_an_error_of_the_request() {
    struct Failing;
    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "the store"))
        }
    }
    let responder = Responder::start(vec![
        Action::Respond(message_accepting(example("refs"), "zstd")),
        Action::Respond(message(example("objects"))),
    ]);
    let mut transport = transport(&responder);
    read_all(transport.refs().unwrap());
    let result = transport.fetch(&mut Failing);
    match result {
        Err(TransportError::Request(error)) => {
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(error.to_string(), "the store");
        }
        other => panic!("expected an error of the request, got {other:?}"),
    };
}

#[test]
fn a_window_above_the_bound_is_refused_with_a_typed_error() {
    assert_eq!(MAX_WINDOW, 128 * 1024 * 1024);
    let responder = Responder::start(vec![
        Action::Respond(compressed_message(example("objects"), 28)),
        Action::Respond(compressed_message(example("objects"), 27)),
    ]);
    let mut transport = transport(&responder);
    let error = read_error(transport.refs().unwrap());
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    let inner = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<WindowTooLarge>())
        .expect("the error of the window");
    assert_eq!(inner.bound, MAX_WINDOW);
    assert!(error.to_string().contains("134217728 bytes"), "{error}");
    // The bound itself passes.
    let (_, body) = read_all(transport.refs().unwrap());
    assert_eq!(body, example("objects"));
}

#[test]
fn a_body_of_zstd_that_cannot_be_decoded_is_an_error_of_the_body() {
    let compressed = compress(example("objects"), 27);
    let cut = &compressed[..compressed.len() - 4];
    let with = |body: &[u8]| {
        Action::Respond(response(
            200,
            &[
                ("Content-Type", MESSAGE_MEDIA_TYPE),
                ("Content-Encoding", "zstd"),
            ],
            body,
        ))
    };
    let responder = Responder::start(vec![
        with(example("objects")),
        with(cut),
        with(&[compressed.clone(), b"trailing".to_vec()].concat()),
    ]);
    let mut transport = transport(&responder);
    let error = read_error(transport.refs().unwrap());
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("cannot be decoded"), "{error}");
    let error = read_error(transport.refs().unwrap());
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    assert!(error.to_string().contains("inside a frame"), "{error}");
    let error = read_error(transport.refs().unwrap());
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn an_unknown_content_encoding_is_no_answer() {
    let refs = example("refs");
    let with = |encoding: &str| {
        Action::Respond(response(
            200,
            &[
                ("Content-Type", MESSAGE_MEDIA_TYPE),
                ("Content-Encoding", encoding),
            ],
            refs,
        ))
    };
    let responder = Responder::start(vec![with("gzip"), with("br"), with("zstd, zstd")]);
    let mut transport = transport(&responder);
    for expected in ["gzip", "br", "zstd, zstd"] {
        let detail = no_answer(transport.refs());
        assert!(
            detail.contains(&format!("`{expected}`, which the client does not read")),
            "{detail}"
        );
    }
}
