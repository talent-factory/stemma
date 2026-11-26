# ADR-028: `ureq` with rustls for the HTTP client

**Status:** Accepted for phase 5. Decided by the author on 2026-11-23 (STEMMA-272) and carried
into the ADRs by STEMMA-280. The numbers of the decision come from the research note
`docs/research/stemma-268-http-crates.md` on the branch `feat/stemma-268-research-http-crates`
(STEMMA-268), which resolved the trees in a scratch crate against `deny.toml`. The crate
`stemma-remote` with the dependency was added by STEMMA-301, a level 3 change (ADR-004) because it
changes `deny.toml`; see "As built".

## Context

The crate `stemma-remote` holds the HTTP client that implements the trait `Transport` (ADR-022).
A command needs 2 to 5 requests (ADR-023), and the bodies of a clone can reach gigabytes, so they
must be streamed. The core is synchronous, and so are the CLI and the event loop of the TUI. TLS
is mandatory towards a hub (ADR-024).

Candidates on the client side (research note of STEMMA-268):

- **`ureq` 3.5** uses blocking I/O without a runtime, declares `#![forbid(unsafe_code)]`, speaks
  HTTP/1.1 only, and gives bodies as `Read` in both directions. MSRV 1.85. A client with TLS
  adds 24 package names to `Cargo.lock` with rustls and 27 with `native-tls`.
- **`reqwest`** is async on `tokio` and `hyper`. Its feature `blocking` keeps the signatures
  synchronous but brings `tokio` into the tree: 86 to 131 new packages, a hidden runtime with a
  thread per client, and a default timeout of 30 seconds over the whole body.
- **`minreq` and `attohttpc`** are blocking as well and each fail the licence check on a licence
  of their own.

Against the unchanged `deny.toml` every rustls variant fails the licence check (`ISC`,
`CDLA-Permissive-2.0`); `ureq` passes with `native-tls` or without TLS.

## Decision

- **`ureq` 3 in `stemma-remote`, blocking, without an async runtime.** Only `stemma-remote`
  depends on it. The trait `Transport` and the signatures of `stemma-core` stay synchronous, and
  no `tokio` enters the workspace. HTTP/1.1 is enough for the requests of a command.
- **Bodies are streamed through `Read` in both directions.** The client wraps the body in the
  encoder and the decoder of the compression itself, without the feature `gzip` of `ureq`
  (ADR-029).
- **The client sets its own timeouts.** `ureq` sets none by default; the client sets one for the
  connection and one for the idle time, not for the whole request. The values are left to the
  slice.
- **TLS is rustls with `ring`**, through the documented feature `rustls` of `ureq`, with the
  Mozilla roots compiled in (`webpki-roots`). The behaviour is the same on every platform, and the
  build needs nothing from the system.
- **A setting per remote names a CA file of its own**, for a hub with a certificate of an own CA
  and for tests with a test CA. A certificate that an administrator adds to the trust store of the
  system does not count; the CA file is the answer. Its certificates replace the compiled-in
  roots for that remote (see "As built"). A change to the platform verifier stays possible
  without a change of a format.
- **The client accepts `http://` only towards a loopback address** (`127.0.0.1`, `::1`,
  `localhost`) and refuses it otherwise with a typed error. Local development and the end-to-end
  tests of a hub then need neither a proxy nor a test CA, and TLS stays mandatory for every
  address that crosses a network.
- **The client follows no redirect** (ADR-023).
- **`deny.toml` gets four exceptions by crate**, as ADR-016 and ADR-020 did: `ring`,
  `rustls-webpki` and `untrusted` for `ISC`, and `webpki-roots` for `CDLA-Permissive-2.0`. Two
  duplicate versions (`getrandom` 0.2, `windows-sys` 0.52) are warnings.

The slice is level 3 because it changes `deny.toml`, a level 3 file. Whether a transitive TLS
backend is a cryptography crate in the sense of rule 7 of ADR-004 is therefore not interpreted.

Rejected:

- `reqwest` with `blocking`, for the reasons above.
- An async client: the trait in the core would become `async` up to `main` and the event loop of
  the TUI.
- rustls with the platform verifier: 19 more packages, and the measured variant rests on the
  feature `_ring`, which is not public.
- `native-tls`: no exception, but OpenSSL of the system on Linux and a behaviour that differs by
  platform.
- rustls with `aws-lc-rs`: 50 new packages, `cmake` in the build, five exceptions.

## Consequences

- The lockfile grows by 24 package names, by the count of the research note.
- A flaw in rustls, `ring` or the compiled-in roots is a flaw in the confidentiality of a transfer
  and of the token. It is no flaw in a proof: receipts, confirmations and statements are signed
  and checked without trusting TLS (ADR-026).
- A hub terminates TLS as it chooses. How the hub of stemma-hub does it is decided there.

## As built

STEMMA-301 added the crate `stemma-remote` with `ureq` 3.5 (`default-features = false`,
feature `rustls`) and the four exceptions of `deny.toml`.

- **The tree.** `Cargo.lock` gained 24 package names besides `stemma-remote`, the count of the
  research note, and two second versions of names that were there: `getrandom` 0.2 and
  `windows-sys` 0.52, both through `ring`; `cargo deny check` reports them as warnings. Nine of
  the 24 are built on Windows only: `windows-targets` 0.52 and its eight target crates.
- **The mapping.** The client is `HttpTransport`, which implements `Transport` and maps its
  requests to the paths of ADR-023 below the address of the repository. A request body and the
  body of an answer are streamed through `Read`; the token answer of a sign-in is the one body
  that the transport reads into memory, up to its bound. The token travels as
  `Authorization: Bearer` with every request after the sign-in, except the two requests of a
  sign-in.
- **The media types.** ADR-022 leaves it to this slice how the transport tells a message from an
  error message without reading the body. A body of the protocol carries
  `application/vnd.stemma.message` or `application/vnd.stemma.error`. A message is an answer with
  the status 200 and the first, or with the status 204 and no body; an error message is an answer
  with a 4xx or 5xx status and the second. Every other answer is `TransportError::NoAnswer` with
  the status in its words: another 2xx, a redirect with its new address, a page of a proxy, a
  status and a media type that disagree, a body with a content encoding or with a transfer
  encoding other than chunks, and a body without a length, whose end cannot be told from a cut.
  Chunks count in HTTP/1.1 alone, because `ureq` reads an HTTP/1.0 body to the close. An answer
  that is no HTTP (another service behind the port, a head that is too large, a proxy that
  refuses the tunnel) is `NoAnswer` too, in the words of the failure and without a status.
- **The status.** The client keeps the status of the last answer as the coarse class of the
  table of ADR-023 (`Status`: the codes of the table, a 401 with `WWW-Authenticate: Bearer`,
  `Retry-After` in seconds for 429 and 503, and the new address of a 3xx). A command reads it for
  what the body does not say, such as the exit code 6 of a busy hub or that a remote needs a
  sign-in. The retries of a 503 belong to the command slices, because a request body is a stream
  that the transport cannot send a second time.
- **The timeouts.** 30 seconds to resolve the name of the host and again 30 seconds to connect,
  including the TLS handshake, and 60 seconds of idle time for every single read and write
  (`timeout_per_read`, `timeout_per_write` of `ureq`), which covers the reads and writes of the
  handshake as well. Through a proxy the name of the host is not resolved by the client; the name
  of the proxy and the `CONNECT` exchange count to the connection. No timeout covers a whole
  request.
- **The CA file.** `ureq` takes either the compiled-in roots or a list of its own, not both. With
  a CA file the certificates of the file are therefore the only roots of that remote, in place of
  the Mozilla roots, not in addition to them. That is what a hub with an own CA needs, and it is
  stricter: a public CA cannot issue a certificate for that hub. A file that cannot be read,
  holds no certificate in PEM, holds a PEM section that cannot be read, or holds a certificate
  that rustls does not take as a root is a typed error when the transport is made; `ureq` would
  drop such a certificate without an error. rustls is a direct dependency for this check, in the
  version that `ureq` brings.
- **Proxies.** A connection with `https://` uses an HTTP or HTTPS proxy of the environment
  through `CONNECT`, so the proxy sees no token: the first of `ALL_PROXY`, `HTTPS_PROXY` and
  `HTTP_PROXY` (each also in lower case) that holds a valid proxy address, unless `NO_PROXY`
  names the host, read once when the transport is made. This holds for an `https://` address
  towards a loopback host as well. An HTTPS proxy is checked against the same roots as the hub.
  If that first variable names a SOCKS proxy, no proxy is used and the later variables are not
  tried, because `ureq` without its feature `socks-proxy` would connect past it or fail. A
  connection with `http://` towards a loopback address uses none.
- **No token in a log.** `ureq` (through `ureq-proto`) writes the raw bytes of every request and
  answer at the log level `trace`, among them the `Authorization` header and the token answer of
  a sign-in, and redacts every header but a fixed list at `debug`. The crate turns on the
  features `max_level_debug` and `release_max_level_debug` of `log`, which remove the records at
  `trace` from the whole build; a constant assertion in the crate fails the build if the level is
  raised. No crate of the workspace installs a logger today.
- **Not yet.** The compression with zstd (ADR-029): the client sends no `Accept-Encoding` and
  treats a body with a content encoding as no answer. The upload of objects, the statements, the
  record, and the anchors come with the slices that add their methods to `Transport`.

How the client is tested without a server: the tests of `stemma-remote` run a scripted responder
on a loopback socket, written with the standard library, that answers each request with bytes
that the test sets and records what the client sent. With a test CA in `tests/data/test-ca` it
speaks TLS through rustls. The tests cover:

- the mapping of every status of the table, and a status and a media type that disagree;
- the redirect that is not followed, also to a second responder that must see no request;
- the refused `http://` address, and that `http://` uses no proxy of the environment;
- the idle timeout during a handshake, before and inside an answer, and while a request body is
  sent;
- a body that ends early, has no length, or comes in chunks of HTTP/1.0; the content encodings;
- a request body that cannot be read or is interrupted;
- the CA file, and the bound of the token answer;
- that the token is in no error, no `Debug` form, and no log record.

Not tested: the timeouts to resolve and to connect, which need an address that does not answer
and are not portable, and the reuse of a connection, because every answer of the responder
closes it. The end-to-end tests against a hub stay in stemma-hub (ADR-022).

## Open Points

- [ ] The last release of `ring` is of March 2025. An advisory as unmaintained would fail
      `just deps`; the way out is `aws-lc-rs` or `native-tls`. `cargo audit` reported none on
      2026-11-25 (STEMMA-301); the point stays open as a watch.
- [ ] The build with Rust 1.89 and on Windows, the owners of the crates on crates.io, and
      `cargo deny check` with the exceptions belong to the level 3 review of the change that adds
      the dependency. STEMMA-301 ran `cargo deny check` with the exceptions and the build and the
      tests with Rust 1.89 on macOS; the build on Windows and on Linux is left to the CI run of
      its pull request, and the owners to the human review.
- [ ] Notice of the third-party licences in the distribution of a release (see ADR-020).
- [x] How the HTTP client is tested in this repository without a server: with a scripted
      responder on a loopback socket and a test CA (STEMMA-301, see "As built").
