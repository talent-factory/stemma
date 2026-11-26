//! A proxy of the environment and an `http://` address towards a loopback host: the request goes
//! to the host, and the proxy sees nothing, not even the connection (ADR-028).
//!
//! The test sets proxy variables for the whole process, so it is the one test of this file.

mod support;

use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use stemma_core::transport::{Answer, Transport};
use stemma_remote::{HttpTransport, Timeouts};
use support::{example, message, Action, Responder};

#[test]
fn http_towards_loopback_uses_no_proxy_of_the_environment() {
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_address = format!("http://{}", proxy.local_addr().unwrap());
    let reached = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&reached);
    thread::spawn(move || {
        if proxy.accept().is_ok() {
            flag.store(true, Ordering::SeqCst);
        }
    });
    for name in [
        "ALL_PROXY",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "all_proxy",
        "http_proxy",
    ] {
        std::env::set_var(name, &proxy_address);
    }
    std::env::remove_var("NO_PROXY");
    std::env::remove_var("no_proxy");

    let responder = Responder::start(vec![Action::Respond(message(example("refs")))]);
    // Short times, so that a request that went to the silent proxy fails fast.
    let timeouts = Timeouts::new(Duration::from_secs(5), Duration::from_secs(2));
    let mut transport = HttpTransport::new(&responder.remote(), timeouts).unwrap();
    let answer = transport.refs();
    assert!(!reached.load(Ordering::SeqCst), "the proxy was reached");
    assert!(matches!(answer.unwrap(), Answer::Message(_)));
    assert_eq!(responder.requests().len(), 1);
}
