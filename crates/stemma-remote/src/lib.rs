//! Stemma remote: the HTTP client that implements the trait
//! [`Transport`](stemma_core::transport::Transport) of the core (ADR-022, ADR-028).
//!
//! The crate holds I/O and nothing else. The core writes every request and reads every answer
//! with the types of [`stemma_core::protocol`]; this crate carries the bytes over HTTP/1.1 with
//! `ureq`, blocking and without an async runtime, and TLS with rustls on `ring` and the Mozilla
//! roots of `webpki-roots` (ADR-028). The command line wires the two.
//!
//! # The mapping to HTTP
//!
//! [`HttpTransport`] maps the requests of the trait to the endpoints below the address of the
//! repository (ADR-023): `GET <repo>/refs`, `POST <repo>/fetch`, `POST <repo>/refs` for the ref
//! update, and `POST <repo>/challenge` and `POST <repo>/token` for the sign-in. A body is
//! streamed in both directions through `Read`. The token of a sign-in travels as
//! `Authorization: Bearer <token>` with every request but the two of the sign-in.
//!
//! An answer is told from an error message by its status and its media type together
//! ([`MESSAGE_MEDIA_TYPE`], [`ERROR_MEDIA_TYPE`]): a message is a 2xx status with the media type
//! of a message, and an error message is a 4xx or 5xx status with the media type of an error.
//! Everything else, such as the page of a proxy, a redirect, or a body with a content encoding,
//! is [`TransportError::NoAnswer`](stemma_core::transport::TransportError::NoAnswer) with the
//! status in its words. The status itself is the coarse class only ([`Status`]); the typed
//! error stands in the body, which the core reads. The transport follows no redirect and relies
//! on no `Cache-Control` header.
//!
//! # Connections
//!
//! TLS is mandatory. An address with `http://` is accepted only towards a loopback address,
//! `127.0.0.1`, `::1`, or `localhost`, and refused otherwise ([`ClientError::PlainHttp`]). A
//! remote may name a CA file ([`stemma_core::remote::CaFile`]); its certificates are then the
//! only roots for that remote, in place of the compiled-in roots. The client sets a timeout for
//! the connection and one for the idle time of every read and write, and none for a whole
//! request ([`Timeouts`]), so a long clone is not cut off while bytes flow.
//!
//! # The token
//!
//! The token stays in memory, in the transport. It is in no error, in no `Debug` form, and in no
//! log record: the crate caps the log level of the build at `debug` (the features
//! `max_level_debug` and `release_max_level_debug` of the crate `log`), because `ureq` writes
//! the raw bytes of a request, with its headers, at the level `trace`, and redacts the
//! `Authorization` header at the level `debug`.
//!
//! # Tests
//!
//! The tests of this crate run without a hub. A scripted responder on a loopback socket, written
//! for the tests with the standard library, answers each request with bytes that a test sets, and
//! records what the client sent; with the test CA under `tests/data` it speaks TLS through
//! rustls. So the mapping of every status, the refused redirect, the timeouts, the loopback rule,
//! the CA file, and the absence of the token from errors and logs are tested here. The end-to-end
//! tests against a real hub live in stemma-hub (ADR-022).
//!
//! # Errors
//!
//! The messages follow the rule of the core: the message of an error includes the message of its
//! cause, and [`Error::source`](std::error::Error::source) returns the cause of the cause.

#![forbid(unsafe_code)]

mod address;
mod client;
mod status;
mod tls;

pub use client::{ClientError, HttpTransport, Timeouts, ERROR_MEDIA_TYPE, MESSAGE_MEDIA_TYPE};
pub use status::Status;

// The token never reaches a log record: `ureq` writes the raw bytes of a request at the level
// `trace`, which the features of `log` in `Cargo.toml` remove from the build.
const _: () = assert!(log::STATIC_MAX_LEVEL as usize <= log::LevelFilter::Debug as usize);
