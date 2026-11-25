//! TLS with the CA file of a remote, against the scripted responder with the test CA
//! (`tests/data/test-ca`).

mod support;

use std::fs;
use std::io::Read;

use stemma_core::transport::{Answer, Transport, TransportError};
use stemma_remote::{ClientError, HttpTransport, Timeouts};
use support::{example, message, remote, test_ca, Action, Responder};

#[test]
fn the_ca_file_of_the_remote_lets_the_client_trust_the_hub() {
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
fn a_ca_file_replaces_the_compiled_in_roots() {
    // The server certificate is signed by the test CA; as a CA file of its own it is no root
    // that signed it, so the connection is refused.
    let responder = Responder::start_tls(vec![Action::Close]);
    let config = remote(responder.address(), Some(&test_ca().join("server.pem")));
    let mut transport = HttpTransport::new(&config, Timeouts::DEFAULT).unwrap();
    assert!(matches!(transport.refs(), Err(TransportError::Carrier(_))));
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
