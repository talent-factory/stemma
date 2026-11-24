# ADR-028: `ureq` with rustls for the HTTP client

**Status:** Accepted for phase 5. Decided by the author on 2026-11-23 (STEMMA-272) and carried
into the ADRs by STEMMA-280. The dependency is not added yet; the slice that adds it changes
`deny.toml` and is therefore a level 3 change (ADR-004). The numbers come from the research note
`docs/research/stemma-268-http-crates.md` on the branch `feat/stemma-268-research-http-crates`
(STEMMA-268), which resolved the trees in a scratch crate against `deny.toml`. No variant was
built with Rust 1.89, on Linux or on Windows, and no transfer was run.

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
- **A setting per remote names an additional CA file**, for a hub with a certificate of an own CA
  and for tests with a test CA. A certificate that an administrator adds to the trust store of the
  system does not count; the CA file is the answer. A change to the platform verifier stays
  possible without a change of a format.
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

## Open Points

- [ ] The last release of `ring` is of March 2025. An advisory as unmaintained would fail
      `just deps`; the way out is `aws-lc-rs` or `native-tls`.
- [ ] The build with Rust 1.89 and on Windows, the owners of the crates on crates.io, and
      `cargo deny check` with the exceptions belong to the level 3 review of the change that adds
      the dependency.
- [ ] Notice of the third-party licences in the distribution of a release (see ADR-020).
- [ ] How the HTTP client is tested in this repository without a server. Left to the slice.
