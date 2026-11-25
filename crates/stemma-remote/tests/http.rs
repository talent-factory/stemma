//! The mapping of the requests of the trait to HTTP, against the scripted responder (see the crate
//! documentation): endpoints and headers, every status, the refused redirect, the refused
//! `http://` address, the timeouts, and the streams in both directions.

mod support;

use std::io::{self, Read};
use std::time::{Duration, Instant};

use stemma_core::protocol::ErrorMessage;
use stemma_core::transport::{Answer, Transport, TransportError};
use stemma_remote::{
    ClientError, HttpTransport, Status, Timeouts, ERROR_MEDIA_TYPE, MESSAGE_MEDIA_TYPE,
};
use support::{example, message, remote, response, Action, Responder, REPOSITORY_PATH};

fn transport(responder: &Responder) -> HttpTransport {
    HttpTransport::new(&responder.remote(), Timeouts::DEFAULT).unwrap()
}

fn read_all(answer: Answer<'_>) -> (bool, Vec<u8>) {
    let (is_message, mut body) = match answer {
        Answer::Message(body) => (true, body),
        Answer::Error(body) => (false, body),
    };
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).unwrap();
    (is_message, bytes)
}

fn no_answer(result: Result<Answer<'_>, TransportError>) -> String {
    match result {
        Err(TransportError::NoAnswer { detail }) => detail,
        other => panic!("expected no answer, got {other:?}"),
    }
}

#[test]
fn refs_is_a_get_below_the_address_and_hands_the_message_over() {
    let refs = example("refs");
    let responder = Responder::start(vec![Action::Respond(message(refs))]);
    let mut transport = transport(&responder);
    let (is_message, body) = read_all(transport.refs().unwrap());
    assert!(is_message);
    assert_eq!(body, refs);
    assert_eq!(transport.last_status(), Some(&Status::Success(200)));

    let request = &responder.requests()[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, format!("{REPOSITORY_PATH}/refs"));
    assert_eq!(
        request.header("accept"),
        Some(format!("{MESSAGE_MEDIA_TYPE}, {ERROR_MEDIA_TYPE}").as_str())
    );
    assert!(request
        .header("user-agent")
        .is_some_and(|agent| agent.starts_with("stemma/")));
    assert_eq!(request.header("authorization"), None);
    assert_eq!(request.header("accept-encoding"), None);
}

#[test]
fn fetch_and_ref_update_stream_their_body_in_a_post() {
    let fetch = example("fetch");
    let update = example("ref-update");
    let objects = example("objects");
    let receipt = example("receipt");
    let responder = Responder::start(vec![
        Action::Respond(message(objects)),
        Action::Respond(message(receipt)),
    ]);
    let mut transport = transport(&responder);
    let (_, body) = read_all(transport.fetch(&mut &fetch[..]).unwrap());
    assert_eq!(body, objects);
    let (_, body) = read_all(transport.ref_update(&mut &update[..]).unwrap());
    assert_eq!(body, receipt);

    let requests = responder.requests();
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, format!("{REPOSITORY_PATH}/fetch"));
    assert_eq!(requests[0].body, fetch);
    assert_eq!(requests[0].header("content-type"), Some(MESSAGE_MEDIA_TYPE));
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].path, format!("{REPOSITORY_PATH}/refs"));
    assert_eq!(requests[1].body, update);
}

#[test]
fn a_large_body_streams_in_both_directions() {
    // Eight MiB each way: more than any buffer of the client or of the responder.
    let large: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let responder = Responder::start(vec![Action::Respond(message(&large))]);
    let mut transport = transport(&responder);
    let (_, body) = read_all(transport.fetch(&mut &large[..]).unwrap());
    assert_eq!(body.len(), large.len());
    assert!(body == large);
    assert!(responder.requests()[0].body == large);
}

/// A status code, the headers of its answer, and its class.
type Row = (u16, &'static [(&'static str, &'static str)], Status);

#[test]
fn every_status_of_the_table_hands_its_error_over_with_its_class() {
    let error = example("error-malformed");
    let table: [Row; 12] = [
        (400, &[], Status::Malformed),
        (401, &[("WWW-Authenticate", "Bearer")], Status::SignInNeeded),
        (403, &[], Status::Forbidden),
        (404, &[], Status::NotFound),
        (409, &[], Status::Stale),
        (413, &[], Status::TooLarge),
        (415, &[], Status::UnsupportedEncoding),
        (422, &[], Status::Incomplete),
        (
            429,
            &[("Retry-After", "5")],
            Status::RateLimited(Some(Duration::from_secs(5))),
        ),
        (
            503,
            &[("Retry-After", "30")],
            Status::Busy(Some(Duration::from_secs(30))),
        ),
        // Outside the table, an error message is still handed over, with the plain code.
        (401, &[], Status::Other(401)),
        (502, &[], Status::Other(502)),
    ];
    let actions = table
        .iter()
        .map(|(code, headers, _)| {
            let mut headers = headers.to_vec();
            headers.push(("Content-Type", ERROR_MEDIA_TYPE));
            Action::Respond(response(*code, &headers, error))
        })
        .collect();
    let responder = Responder::start(actions);
    let mut transport = transport(&responder);
    for (code, _, class) in table {
        let (is_message, body) = read_all(transport.refs().unwrap());
        assert!(!is_message, "{code}");
        assert_eq!(body, error, "{code}");
        // The typed error stands in the body, for the reader of the core.
        assert!(ErrorMessage::parse(&body).is_ok(), "{code}");
        assert_eq!(transport.last_status(), Some(&class), "{code}");
    }
}

#[test]
fn every_status_without_a_readable_message_is_no_answer_with_its_status() {
    let page: &[u8] = b"<html><body>Bad gateway</body></html>";
    let codes = [
        400, 401, 403, 404, 409, 413, 415, 422, 429, 500, 502, 503, 504,
    ];
    let actions = codes
        .iter()
        .map(|code| Action::Respond(response(*code, &[("Content-Type", "text/html")], page)))
        .collect();
    let responder = Responder::start(actions);
    let mut transport = transport(&responder);
    for code in codes {
        let detail = no_answer(transport.refs());
        assert!(
            detail.contains(&format!("status {code}")),
            "{code}: {detail}"
        );
        assert!(detail.contains("text/html"), "{code}: {detail}");
        assert_eq!(transport.last_status().map(Status::code), Some(code));
    }
}

#[test]
fn a_status_and_a_media_type_that_disagree_are_no_answer() {
    let refs = example("refs");
    let error = example("error-malformed");
    let responder = Responder::start(vec![
        // A message under the status of an error.
        Action::Respond(response(404, &[("Content-Type", MESSAGE_MEDIA_TYPE)], refs)),
        // An error under the status of a success.
        Action::Respond(response(200, &[("Content-Type", ERROR_MEDIA_TYPE)], error)),
        // A success without a media type.
        Action::Respond(response(200, &[], refs)),
        // A success that no request of the protocol has.
        Action::Respond(response(201, &[("Content-Type", MESSAGE_MEDIA_TYPE)], refs)),
    ]);
    let mut transport = transport(&responder);
    assert!(no_answer(transport.refs()).contains("status 404"));
    assert!(no_answer(transport.refs()).contains(ERROR_MEDIA_TYPE));
    assert!(no_answer(transport.refs()).contains("without a media type"));
    assert!(no_answer(transport.refs()).contains("status 201"));
}

#[test]
fn a_media_type_is_compared_without_case_and_parameters() {
    let refs = example("refs");
    let responder = Responder::start(vec![Action::Respond(response(
        200,
        &[(
            "Content-Type",
            "Application/Vnd.Stemma.Message; charset=utf-8",
        )],
        refs,
    ))]);
    let (is_message, body) = read_all(transport(&responder).refs().unwrap());
    assert!(is_message);
    assert_eq!(body, refs);
}

#[test]
fn a_204_is_a_message_without_a_body() {
    let responder = Responder::start(vec![Action::Respond(response(204, &[], b""))]);
    let mut transport = transport(&responder);
    let (is_message, body) = read_all(transport.fetch(&mut &b"x"[..]).unwrap());
    assert!(is_message);
    assert!(body.is_empty());
    assert_eq!(transport.last_status(), Some(&Status::Success(204)));
}

#[test]
fn a_redirect_is_not_followed_and_names_its_address() {
    // The new address lies on the responder itself, so a redirect that were followed would
    // reach the action that the test queues after the redirects, and be counted.
    let moved = format!("{REPOSITORY_PATH}/moved");
    let mut actions: Vec<Action> = [301, 302, 303, 307, 308]
        .iter()
        .map(|code| Action::Respond(response(*code, &[("Location", moved.as_str())], b"")))
        .collect();
    // A 307 resends the body of a POST, and with it the token.
    actions.push(Action::Respond(response(
        307,
        &[("Location", moved.as_str())],
        b"",
    )));
    actions.push(Action::Respond(message(example("refs"))));
    let responder = Responder::start(actions);
    let mut transport = transport(&responder);
    for code in [301, 302, 303, 307, 308] {
        let detail = no_answer(transport.refs());
        assert!(detail.contains(&moved), "{code}: {detail}");
        assert!(detail.contains("not followed"), "{code}: {detail}");
        assert_eq!(
            transport.last_status(),
            Some(&Status::Redirect(code, Some(moved.clone())))
        );
    }
    let detail = no_answer(transport.fetch(&mut &example("fetch")[..]));
    assert!(detail.contains("status 307"), "{detail}");
    // One request per redirect, and none to the new address.
    let paths: Vec<String> = responder
        .requests()
        .into_iter()
        .map(|request| request.path)
        .collect();
    let mut expected = vec![format!("{REPOSITORY_PATH}/refs"); 5];
    expected.push(format!("{REPOSITORY_PATH}/fetch"));
    assert_eq!(paths, expected);
}

#[test]
fn a_content_encoding_that_was_not_asked_for_is_no_answer() {
    let responder = Responder::start(vec![Action::Respond(response(
        200,
        &[
            ("Content-Type", MESSAGE_MEDIA_TYPE),
            ("Content-Encoding", "gzip"),
        ],
        b"\x1f\x8b",
    ))]);
    let detail = no_answer(transport(&responder).refs());
    assert!(detail.contains("gzip"), "{detail}");
}

#[test]
fn http_is_refused_towards_a_host_that_is_not_loopback() {
    for address in [
        "http://hub.example/ns/repo",
        "http://127.0.0.2/ns/repo",
        "http://localhost.hub.example/ns/repo",
    ] {
        let error = HttpTransport::new(&remote(address, None), Timeouts::DEFAULT).unwrap_err();
        assert!(matches!(&error, ClientError::PlainHttp(found) if found == address));
        assert!(error.to_string().contains("http://"), "{error}");
    }
    for address in [
        "http://127.0.0.1:1/ns/repo",
        "http://[::1]:1/ns/repo",
        "http://localhost:1/ns/repo",
        "https://hub.example/ns/repo",
    ] {
        assert!(
            HttpTransport::new(&remote(address, None), Timeouts::DEFAULT).is_ok(),
            "{address}"
        );
    }
}

#[test]
fn a_refused_connection_is_a_failure_of_the_carrier() {
    let address = format!("http://127.0.0.1:{}/ns/repo", support::unused_port());
    let mut transport = HttpTransport::new(&remote(&address, None), Timeouts::DEFAULT).unwrap();
    match transport.refs() {
        Err(TransportError::Carrier(_)) => {}
        other => panic!("expected a failure of the carrier, got {other:?}"),
    }
    assert_eq!(transport.last_status(), None);
}

#[test]
fn a_connection_closed_without_an_answer_is_a_failure_of_the_carrier() {
    let responder = Responder::start(vec![Action::Close]);
    match transport(&responder).refs() {
        Err(TransportError::Carrier(_)) => {}
        other => panic!("expected a failure of the carrier, got {other:?}"),
    }
}

fn short() -> Timeouts {
    Timeouts::new(Duration::from_secs(5), Duration::from_millis(300))
}

#[test]
fn an_answer_that_does_not_begin_ends_after_the_idle_time() {
    let responder = Responder::start(vec![Action::Stall]);
    let mut transport = HttpTransport::new(&responder.remote(), short()).unwrap();
    let start = Instant::now();
    match transport.refs() {
        Err(TransportError::Carrier(error)) => {
            assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        }
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_body_that_stops_ends_after_the_idle_time_with_an_error_of_its_reader() {
    let mut bytes = response(200, &[("Content-Type", MESSAGE_MEDIA_TYPE)], &[b'x'; 1000]);
    // The head and half the body; then the responder holds the connection.
    bytes.truncate(bytes.len() - 500);
    let responder = Responder::start(vec![Action::Partial(bytes)]);
    let mut transport = HttpTransport::new(&responder.remote(), short()).unwrap();
    let Answer::Message(mut body) = transport.refs().unwrap() else {
        panic!("expected a message");
    };
    let start = Instant::now();
    let mut read = Vec::new();
    let error = body.read_to_end(&mut read).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
    assert_eq!(read.len(), 500);
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn the_idle_time_counts_per_read_and_not_for_the_whole_answer() {
    // The answer begins at once and every read finds bytes, so a body whose reading takes more
    // than twice the idle time as a whole still arrives.
    let idle = Duration::from_millis(500);
    let body = vec![b'y'; 4 * 1024 * 1024];
    let responder = Responder::start(vec![Action::Respond(message(&body))]);
    let mut transport = HttpTransport::new(
        &responder.remote(),
        Timeouts::new(Duration::from_secs(5), idle),
    )
    .unwrap();
    let Answer::Message(mut reader) = transport.refs().unwrap() else {
        panic!("expected a message");
    };
    let start = Instant::now();
    let mut read = Vec::new();
    let mut buf = [0; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        read.extend_from_slice(&buf[..n]);
        // Time between the reads of the application does not count.
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(read.len(), body.len());
    assert!(start.elapsed() > 2 * idle, "{:?}", start.elapsed());
}

#[test]
fn a_request_body_that_the_other_side_does_not_read_ends_after_the_idle_time() {
    let responder = Responder::start(vec![Action::StallAfterHead]);
    let mut transport = HttpTransport::new(&responder.remote(), short()).unwrap();
    // More than the buffers of the sockets hold on every platform.
    let mut body = io::repeat(b'a').take(64 * 1024 * 1024);
    let start = Instant::now();
    match transport.fetch(&mut body) {
        Err(TransportError::Carrier(error)) => {
            assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        }
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_body_that_ends_before_its_length_is_an_error_of_its_reader() {
    let mut bytes = response(200, &[("Content-Type", MESSAGE_MEDIA_TYPE)], &[b'z'; 100]);
    bytes.truncate(bytes.len() - 90);
    let responder = Responder::start(vec![Action::Respond(bytes)]);
    let mut transport = transport(&responder);
    let Answer::Message(mut body) = transport.refs().unwrap() else {
        panic!("expected a message");
    };
    let mut read = Vec::new();
    assert!(body.read_to_end(&mut read).is_err());
    assert_eq!(read.len(), 10);
}

struct Failing {
    sent: usize,
}

impl Read for Failing {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.sent > 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "store"));
        }
        let n = buf.len().min(16);
        buf[..n].fill(b'a');
        self.sent += n;
        Ok(n)
    }
}

#[test]
fn a_request_body_that_cannot_be_read_is_an_error_of_the_request() {
    let responder = Responder::start(vec![Action::Close]);
    let mut transport = transport(&responder);
    match transport.fetch(&mut Failing { sent: 0 }) {
        Err(TransportError::Request(error)) => {
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(error.to_string(), "store");
        }
        other => panic!("expected an error of the request, got {other:?}"),
    }
    assert_eq!(transport.last_status(), None);
}

#[test]
fn a_body_dropped_before_its_end_allows_the_next_request() {
    let refs = example("refs");
    let responder = Responder::start(vec![
        Action::Respond(message(&vec![b'q'; 1024 * 1024])),
        Action::Respond(message(refs)),
    ]);
    let mut transport = transport(&responder);
    let Answer::Message(mut body) = transport.refs().unwrap() else {
        panic!("expected a message");
    };
    let mut first = [0; 10];
    body.read_exact(&mut first).unwrap();
    drop(body);
    let (_, body) = read_all(transport.refs().unwrap());
    assert_eq!(body, refs);
}

#[test]
fn every_line_and_value_of_content_encoding_is_checked() {
    let refs = example("refs");
    let with = |headers: &[(&str, &str)]| {
        let mut all = vec![("Content-Type", MESSAGE_MEDIA_TYPE)];
        all.extend_from_slice(headers);
        Action::Respond(response(200, &all, refs))
    };
    let responder = Responder::start(vec![
        with(&[
            ("Content-Encoding", "identity"),
            ("Content-Encoding", "zstd"),
        ]),
        with(&[("Content-Encoding", "identity, gzip")]),
        with(&[("Transfer-Encoding", "gzip")]),
        with(&[("Content-Encoding", "identity")]),
    ]);
    let mut transport = transport(&responder);
    assert!(no_answer(transport.refs()).contains("zstd"));
    assert!(no_answer(transport.refs()).contains("gzip"));
    assert!(no_answer(transport.refs()).contains("transfer encoding `gzip`"));
    let (is_message, body) = read_all(transport.refs().unwrap());
    assert!(is_message);
    assert_eq!(body, refs);
}

#[test]
fn a_body_without_a_length_is_no_answer() {
    // Its end is the close of the connection, which cannot be told from a cut.
    let mut bytes = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {MESSAGE_MEDIA_TYPE}\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    bytes.extend_from_slice(example("refs"));
    let responder = Responder::start(vec![Action::Respond(bytes)]);
    let detail = no_answer(transport(&responder).refs());
    assert!(detail.contains("without a length"), "{detail}");
}

#[test]
fn a_body_in_chunks_is_a_message() {
    let refs = example("refs");
    let mut bytes = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {MESSAGE_MEDIA_TYPE}\r\nTransfer-Encoding: chunked\r\n\
         Connection: close\r\n\r\n{:x}\r\n",
        refs.len()
    )
    .into_bytes();
    bytes.extend_from_slice(refs);
    bytes.extend_from_slice(b"\r\n0\r\n\r\n");
    let responder = Responder::start(vec![Action::Respond(bytes)]);
    let (is_message, body) = read_all(transport(&responder).refs().unwrap());
    assert!(is_message);
    assert_eq!(body, refs);
}

#[test]
fn another_service_behind_the_port_is_no_answer() {
    let responder = Responder::start(vec![Action::Respond(b"SSH-2.0-OpenSSH_9.9\r\n".to_vec())]);
    match transport(&responder).refs() {
        Err(TransportError::NoAnswer { .. }) => {}
        other => panic!("expected no answer, got {other:?}"),
    }
}
