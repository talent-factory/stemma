//! Stemma remote: the HTTP client that implements the trait
//! [`Transport`](stemma_core::transport::Transport) of the core (ADR-022, ADR-028).
//!
//! The crate holds I/O and nothing else. The core writes every request and reads every answer
//! with the types of [`stemma_core::protocol`], except the token answer of a sign-in, which the
//! transport reads to keep the token (ADR-022); this crate carries the bytes over HTTP/1.1 with
//! `ureq`, blocking and without an async runtime, and TLS with rustls on `ring` and the Mozilla
//! roots of `webpki-roots` (ADR-028). The command line wires the two.
//!
//! # The mapping to HTTP
//!
//! [`HttpTransport`] maps the requests of the trait to the endpoints below the address of the
//! repository (ADR-023): `GET <repo>/refs`, `POST <repo>/fetch`, `POST <repo>/refs` for the ref
//! update, `POST <repo>/challenge` and `POST <repo>/token` for the sign-in, and
//! `POST <repo>/statements` to submit a statement, `POST <repo>/statements/range` to read the
//! statements of a range, and `POST <repo>/record/range` to read the record of a range, each
//! reading request with the range as its body (STEMMA-326). A body is streamed in both
//! directions through `Read`; the one body that the transport reads into memory
//! is the token answer of a sign-in, up to its bound. The token of a sign-in travels as
//! `Authorization: Bearer <token>` with every request but the two of the sign-in.
//!
//! An answer is told from an error message by its status and its media type together
//! ([`MESSAGE_MEDIA_TYPE`], [`ERROR_MEDIA_TYPE`]): a message is an answer with the status 200 and
//! the media type of a message, or with the status 204 and no body, and an error message is an
//! answer with a 4xx or 5xx status and the media type of an error. Everything else is
//! [`TransportError::NoAnswer`](stemma_core::transport::TransportError::NoAnswer) with the
//! status in its words: another 2xx, the page of a proxy, a redirect, a body with a content
//! encoding other than zstd or with a transfer encoding other than chunks, and a body without a
//! length (chunks count in HTTP/1.1 alone). An answer that is no HTTP, such as another service
//! behind the port, a head that is too large, or a proxy that refuses the tunnel, is `NoAnswer`
//! too, in the words of the failure, without a status. The status itself is the coarse class
//! only ([`Status`]); the typed error stands in the body, which the core reads. The transport
//! follows no redirect and relies on no `Cache-Control` header.
//!
//! # Compression
//!
//! The body carries the content encoding zstd or none (ADR-023, ADR-029). The client sends
//! `Accept-Encoding: zstd`, reads an answer with `Content-Encoding: zstd` through the decoder of
//! zstd and an answer without one as it is, and compresses an upload, with `Content-Encoding:
//! zstd`, only when the answer to the last request was a message or an error message of the
//! protocol and named zstd in its `Accept-Encoding`. The encoder runs at level 3 with a window
//! of 128 MiB and long-distance matching; the decoder refuses a frame with a larger window
//! ([`MAX_WINDOW`]) with [`WindowTooLarge`] inside the error of the body. The client wraps the
//! body in the encoder and the decoder itself; `ureq` is built without its feature `gzip`, and
//! gzip is not read.
//!
//! # Connections
//!
//! TLS is mandatory. An address with `http://` is accepted only towards a loopback address,
//! `127.0.0.1`, `::1`, or `localhost`, and refused otherwise ([`ClientError::PlainHttp`]). A
//! remote may name a CA file ([`stemma_core::remote::CaFile`]); its certificates are then the
//! only roots for that remote, in place of the compiled-in roots. The client sets a timeout to
//! resolve the name of the host, one to connect, and one for the idle time of every read and
//! write, and none for a whole request ([`Timeouts`]), so a long clone is not cut off while bytes
//! flow.
//!
//! # The token
//!
//! The token stays in memory, in the transport. It is in no error, in no `Debug` form, and in no
//! log record: the crate caps the log level of the build at `debug` (the features
//! `max_level_debug` and `release_max_level_debug` of the crate `log`), because `ureq` (through
//! `ureq-proto`) writes the raw bytes of every request and answer at the level `trace`, among
//! them the `Authorization` header and the token answer of a sign-in. At the level `debug` it
//! redacts every header but a fixed list of harmless ones.
//!
//! # Tests
//!
//! The tests of this crate run without a hub. A scripted responder on a loopback socket, written
//! for the tests with the standard library, answers each request with bytes that a test sets, and
//! records what the client sent; with the test CA under `tests/data` it speaks TLS through
//! rustls. So the mapping of every status, the refused redirect, the idle timeouts, the loopback
//! rule, the proxy rule for `http://`, the CA file, the compression in both directions, and the
//! absence of the token from errors and logs are tested here. The end-to-end
//! tests against a real hub live in stemma-hub (ADR-022).
//!
//! # Errors
//!
//! The messages follow the rule of the core: the message of an error includes the message of its
//! cause, and [`Error::source`](std::error::Error::source) returns the cause of the cause.

#![forbid(unsafe_code)]

mod address;
mod client;
mod encoding;
mod status;
mod tls;

pub use client::{ClientError, HttpTransport, Timeouts, ERROR_MEDIA_TYPE, MESSAGE_MEDIA_TYPE};
pub use encoding::{WindowTooLarge, MAX_WINDOW};
pub use status::Status;

// The token never reaches a log record: `ureq` writes the raw bytes of requests and answers at
// the level `trace`, which the features of `log` in `Cargo.toml` remove from the build.
const _: () = assert!(log::STATIC_MAX_LEVEL as usize <= log::LevelFilter::Debug as usize);
