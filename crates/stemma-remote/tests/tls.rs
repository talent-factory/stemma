//! TLS with the CA file of a remote, against the scripted responder with the test CA
//! (`tests/data/test-ca`).

mod support;

use std::fs;
use std::io::{self, Read};
use std::time::{Duration, Instant};

use stemma_core::transport::{Answer, Transport, TransportError};
use stemma_remote::{ClientError, HttpTransport, Timeouts};
use support::{example, message, remote, test_ca, without_proxy, Action, Responder};

#[test]
fn the_ca_file_of_the_remote_lets_the_client_trust_the_hub() {
    without_proxy();
    let refs = example("refs");
    let responder = Responder::start_tls(vec![Action::Respond(message(refs))]);
    assert!(responder.address().starts_with("https://127.0.0.1:"));
    let config = remote(responder.address(), Some(&test_ca().join("ca.pem")));
    let mut transport = HttpTransport::new(&config, Timeouts::DEFAULT).unwrap();
    let Answer::Message(mut body) = transport.refs().unwrap() else {
        panic!("expected a message");
    };
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, refs);
}

#[test]
fn without_the_ca_file_the_certificate_of_an_own_ca_is_refused() {
    without_proxy();
    let responder = Responder::start_tls(vec![Action::Close]);
    let mut transport = HttpTransport::new(&responder.remote(), Timeouts::DEFAULT).unwrap();
    match transport.refs() {
        Err(TransportError::Carrier(error)) => {
            assert!(
                error.to_string().to_lowercase().contains("certificate"),
                "{error}"
            );
        }
        other => panic!("expected a failure of the carrier, got {other:?}"),
    }
    // Nothing was sent over the connection that was refused.
    assert!(responder.requests().is_empty());
}

#[test]
fn a_ca_file_without_the_issuing_ca_is_refused() {
    // The server certificate as the CA file: it is not the CA that signed it, so no root of the
    // remote vouches for the hub. That the file replaces the compiled-in roots, and does not add
    // to them, follows from `RootCerts` of `ureq`, which is one choice.
    without_proxy();
    let responder = Responder::start_tls(vec![Action::Close]);
    let config = remote(responder.address(), Some(&test_ca().join("server.pem")));
    let mut transport = HttpTransport::new(&config, Timeouts::DEFAULT).unwrap();
    match transport.refs() {
        Err(TransportError::Carrier(error)) => {
            assert!(
                error.to_string().to_lowercase().contains("certificate"),
                "{error}"
            );
        }
        other => panic!("expected a failure of the carrier, got {other:?}"),
    };
}

#[test]
fn a_ca_file_that_cannot_be_read_or_holds_no_certificate_is_refused() {
    let dir = std::env::temp_dir().join(format!("stemma-remote-ca-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("missing.pem");
    let error = HttpTransport::new(
        &remote("https://hub.example/ns/repo", Some(&missing)),
        Timeouts::DEFAULT,
    )
    .unwrap_err();
    assert!(matches!(error, ClientError::CaFileRead { .. }), "{error}");
    assert!(error.io_error().is_some());
    assert!(error.to_string().contains("missing.pem"), "{error}");

    let empty = dir.join("empty.pem");
    fs::write(&empty, b"no certificate here\n").unwrap();
    let key_only = dir.join("key-only.pem");
    fs::copy(test_ca().join("server-key.pem"), &key_only).unwrap();
    // A section of a certificate whose bytes are no certificate.
    let not_der = dir.join("not-der.pem");
    fs::write(
        &not_der,
        b"-----BEGIN CERTIFICATE-----\nAAECAwQFBgcICQoLDA0ODw==\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    let error = HttpTransport::new(
        &remote("https://hub.example/ns/repo", Some(&not_der)),
        Timeouts::DEFAULT,
    )
    .unwrap_err();
    assert!(
        matches!(error, ClientError::CaFileInvalid { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("certificate 1"), "{error}");
    // A section that never ends: the message names the line in words, not as bytes.
    let cut = dir.join("cut.pem");
    fs::write(&cut, b"-----BEGIN CERTIFICATE-----\nAAECAwQF\n").unwrap();
    let error = HttpTransport::new(
        &remote("https://hub.example/ns/repo", Some(&cut)),
        Timeouts::DEFAULT,
    )
    .unwrap_err();
    assert!(
        matches!(error, ClientError::CaFileInvalid { .. }),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("section `CERTIFICATE` has no line that ends it"),
        "{error}"
    );
    for path in [&empty, &key_only] {
        let error = HttpTransport::new(
            &remote("https://hub.example/ns/repo", Some(path)),
            Timeouts::DEFAULT,
        )
        .unwrap_err();
        assert!(
            matches!(error, ClientError::CaFileInvalid { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("no certificate"), "{error}");
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_handshake_that_the_other_side_does_not_answer_ends_after_the_idle_time() {
    without_proxy();
    let responder = Responder::start_tls(vec![Action::Mute]);
    let config = remote(responder.address(), Some(&test_ca().join("ca.pem")));
    let idle = Duration::from_millis(300);
    let mut transport =
        HttpTransport::new(&config, Timeouts::new(Duration::from_secs(30), idle)).unwrap();
    let start = Instant::now();
    match transport.refs() {
        Err(TransportError::Carrier(error)) => {
            assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        }
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert!(start.elapsed() >= idle, "{:?}", start.elapsed());
    assert!(start.elapsed() < Duration::from_secs(10));
}
