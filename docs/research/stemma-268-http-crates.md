# Rust crates for the HTTP server of the hub and the HTTP client of stemma

Research note for STEMMA-268, a ticket of the map STEMMA-266 (phase 5: network and hub). It
collects facts for the decision on the HTTP stack (STEMMA-272): which crates come into question
for the server in stemma-hub and for the client in `stemma`, and what each brings along (runtime,
TLS backend, dependency tree, licences and advisories against `deny.toml`, MSRV, streaming of
large bodies, maintenance). It records facts only and decides nothing for Stemma.

## Sources and method

All numbers were measured on 2026-11-23 on `aarch64-apple-darwin` with `cargo` 1.100.0,
`cargo-deny` 0.22.0 and `cargo-audit` 0.23.0 (advisory database with 1562 advisories).

- **Trees.** One scratch crate per variant outside the repository (edition 2021,
  `rust-version = "1.89"`, the default resolver of that edition, as in the workspace), with the
  newest stable version of each candidate. `cargo generate-lockfile` resolved it. The column
  **lock** counts the packages of the lockfile without the scratch crate, and the column
  **new** counts the package names of that lockfile that the `Cargo.lock` of this repository
  (167 packages) does not contain. A lockfile also lists optional packages that no feature
  turns on. The column **graph** therefore counts the packages that `cargo tree` reaches
  through normal and build dependencies with the selected features on all platforms, and the
  columns **macOS**, **Linux** and **Windows** count them for `aarch64-apple-darwin`,
  `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`.
- **Licences and advisories.** `cargo deny check` ran in each scratch crate with a copy of the
  `deny.toml` of this repository, and `cargo audit` ran on its lockfile. The directory of
  stemma-hub is empty (notes of STEMMA-266) and has no `deny.toml`, so the server variants are
  measured against the same file.
- **Behaviour.** Claims on streaming, limits and runtimes come from the source and the README of
  the measured version, read in the local registry of `cargo`. Each such claim names the item.
- **Maintenance.** The crates.io API (versions, dates, owners, downloads of the last 90 days)
  and the GitHub API (last commit on the default branch, archived flag), both queried on
  2026-11-23.
- **Build.** `cargo check --locked` passed for ten variants on the machine above; they are
  marked with (c) in the tables. No variant was built with Rust 1.89, on Linux or on Windows.

A scratch crate resolves alone. Inside the workspace `cargo` unifies compatible versions with
the existing lockfile, so the numbers there can differ by a few packages.

## 1. Starting point in the workspace

- The workspace has three crates (`stemma-core`, `stemma-cli`, `stemma-tui`), `rust-version`
  1.89, and 167 packages in `Cargo.lock`. `fuzz/` is a workspace of its own with its own
  `Cargo.lock` and depends on `stemma-core`.
- No crate contains `async fn`, `.await` or an async runtime. The only matches are test strings
  in `crates/stemma-core/src/weakened.rs`. `stemma-core` uses threads of the standard library
  (`thread::scope` in `store.rs`, `repository.rs` and `refs.rs`), and `stemma-cli` runs one
  thread for signals (`signal-hook`).
- The baseline of `just deps` on this branch: `cargo deny check` ends with
  `advisories ok, bans ok, licenses ok, sources ok` and two warnings about duplicate versions;
  `cargo audit` reports nothing.
- `deny.toml` allows `Apache-2.0`, `MIT` and `Unicode-3.0`, and has exceptions by crate for
  `Zlib` (`foldhash`) and `BSD-3-Clause` (`curve25519-dalek`, `ed25519-dalek`, `subtle`).
  `unmaintained = "all"` and `yanked = "deny"` hold for every transitive crate.
  `multiple-versions = "warn"` reports a second version of a crate without failing.
- The lockfile already contains `cc` (C code is compiled in the build today), `log`, `itoa`,
  `smallvec`, `getrandom` 0.4, `windows-sys` 0.61, `syn` 2 and `zeroize`.

## 2. Candidates for the server (stemma-hub)

### 2.1 Overview

| Crate | Version (date) | Model | HTTP | TLS in the crate |
|---|---|---|---|---|
| `axum` | 0.8.9 (2026-04-14) | async, `tokio` | 1.1, 2 by feature | none |
| `actix-web` | 4.20.0 (2026-11-17) | async, `tokio` through `actix-rt` | 1.x, 2 by feature | features `rustls-0_20` to `rustls-0_23`, `openssl` |
| `hyper` | 1.12.0 (2026-10-06) | async, no runtime of its own | 1.1, 2 by feature | none |
| `tiny_http` | 0.12.0 (2022-10-06) | blocking, thread pool | 1.0, 1.1 | features `ssl-rustls` (rustls 0.20), `ssl-openssl` |
| `rouille` | 3.6.2 (2023-04-24) | blocking, on `tiny_http` | 1.0, 1.1 | features `ssl` and `rustls`, passed on to `tiny_http` |

- **`axum`** is a routing and extractor layer over `hyper` 1 and `tower`. `axum::serve` needs
  the feature `tokio`, which turns on `tokio/net`, `tokio/rt` and `tokio/macros`
  (`Cargo.toml` of 0.8.9). Its manifest names no TLS crate; TLS comes from a crate beside it
  (`tokio-rustls` 0.26.6, or `axum-server` 0.9.2 with MSRV 1.82 and licence MIT) or from a
  proxy in front of the server. The crate declares `#![forbid(unsafe_code)]` (README).
- **`actix-web`** brings its own server (`actix-server`) and runtime wrapper (`actix-rt`).
  `actix-rt` builds a `tokio` runtime of the flavour `new_current_thread` with a `LocalSet`
  per thread (`actix-rt/src/runtime.rs`); `actix-server` starts as many workers as
  `std::thread::available_parallelism()` reports (`actix-server/src/builder.rs`).
- **`hyper`** calls itself "a relatively low-level library, meant to be a building block" and
  points to `axum` for a server (README). Its default feature set is empty; `server` and
  `http1` are features, and the adapters to `tokio` live in `hyper-util` (feature `tokio`).
  It has no router.
- **`tiny_http`** dispatches each connection to a thread of a pool with at least four threads
  and creates a new thread when none is free (README, `src/util/task_pool.rs`). The README
  lists pipelining of requests.
- **`rouille`** is a framework on `tiny_http` (routing macro, sessions, multipart).

### 2.2 Dependency trees

| Variant | Features | lock | graph | macOS | Linux | Windows | new |
|---|---|---|---|---|---|---|---|
| `hyper` (c) | `hyper` {`server`, `http1`}, `hyper-util` {`tokio`}, `http-body-util`, `tokio` {`rt-multi-thread`, `net`} | 20 | 20 | 17 | 17 | 18 | 13 |
| `hyper` with rustls | the same and `tokio-rustls` (default: aws-lc-rs) | 54 | 42 | 36 | 36 | 38 | 36 |
| `axum` minimal (c) | `axum` without default features {`http1`, `tokio`}, `tokio` {`rt-multi-thread`, `net`} | 40 | 40 | 36 | 36 | 37 | 26 |
| `axum` default | `axum`, `tokio` {`rt-multi-thread`, `net`} | 51 | 51 | 47 | 47 | 48 | 32 |
| `axum` minimal with rustls (ring) | minimal and `tokio-rustls` without default features {`ring`, `tls12`, `logging`} | 65 | 65 | 51 | 51 | 52 | 42 |
| `axum` minimal with rustls (aws-lc-rs) | minimal and `tokio-rustls` | 74 | 62 | 55 | 55 | 57 | 49 |
| `actix-web` minimal (c) | without default features | 110 | 109 | 103 | 104 | 103 | 68 |
| `actix-web` minimal with rustls | without default features {`rustls-0_23`} | 133 | 117 | 111 | 112 | 111 | 84 |
| `actix-web` default | default features | 155 | 154 | 147 | 148 | 148 | 94 |
| `tiny_http` (c) | default (no TLS) | 5 | 5 | 5 | 5 | 5 | 4 |
| `tiny_http` with rustls | {`ssl-rustls`} | 49 | 49 | 21 | 23 | 22 | 29 |
| `rouille` | without default features | 116 | 116 | 86 | 85 | 81 | 77 |

Every variant with `tokio` has `tokio` 1.54.1, `mio`, `socket2` and `futures-core` in the graph.
The feature `tokio` of `axum` turns on `tokio/macros`, so the `axum` variants have the proc
macro crate `tokio-macros` in the graph, as the `actix-web` variants do; the `hyper` variant
has none. With aws-lc-rs the lockfile lists `ring` as well, but the graph does not contain it.
`actix-web` with default features adds four second versions beside those of the workspace
(`block-buffer` 0.12.1, `crypto-common` 0.2.2, `digest` 0.11.3, `rand_core` 0.10.1).

### 2.3 `cargo deny check` and `cargo audit`

| Variant | `cargo deny check` | `cargo audit` |
|---|---|---|
| `hyper` | passes | nothing |
| `hyper` with rustls | fails: licences of `aws-lc-rs`, `aws-lc-sys`, `rustls-webpki`, `untrusted` | nothing |
| `axum` minimal, `axum` default | fails: licence of `matchit` 0.8.4 (`MIT AND BSD-3-Clause`) | nothing |
| `axum` minimal with rustls (ring) | fails: licences of `matchit`, `ring`, `rustls-webpki`, `untrusted` | nothing |
| `axum` minimal with rustls (aws-lc-rs) | fails: licences of `matchit`, `aws-lc-rs`, `aws-lc-sys`, `rustls-webpki`, `untrusted` | nothing |
| `actix-web` minimal | fails: licence of `encoding_rs` 0.8.44 (`(Apache-2.0 OR MIT) AND BSD-3-Clause`) | nothing |
| `actix-web` minimal with rustls | fails: licences of `encoding_rs`, `rustls-webpki`, `untrusted` | nothing |
| `actix-web` default | fails: seven licences (`encoding_rs`, `brotli`, `alloc-no-stdlib`, `alloc-stdlib`, `zstd`, `zstd-safe`, `zstd-sys`) and two advisories | `h2` 0.3.27 (RUSTSEC-2026-0258), `cookie` 0.16.2 (RUSTSEC-2026-0594) |
| `tiny_http` | passes | nothing |
| `tiny_http` with rustls | fails: five licence errors (`ring` 0.16.20 and `webpki` 0.22.4 without a licence field, `ring` 0.17.14, `untrusted` twice) and five advisories | `rustls` 0.20.9 (RUSTSEC-2024-0336, RUSTSEC-2026-0349), `ring` 0.16.20 (RUSTSEC-2025-0009; unmaintained RUSTSEC-2025-0010), `rustls-pemfile` 0.2.1 (unmaintained RUSTSEC-2025-0134) |
| `rouille` | fails: licence of `sha1_smol` (`BSD-3-Clause`) and five unmaintained crates | unmaintained: `buf_redux`, `chrono` 0.4.45 (RUSTSEC-2026-0543), `multipart`, `safemem`, `twoway` |

The two advisories of `actix-web` come with the default features `http2` and `cookies`; the
minimal variant has neither crate in the tree.

## 3. Candidates for the client (`stemma`)

### 3.1 Overview

| Crate | Version (date) | Model | HTTP | TLS backends |
|---|---|---|---|---|
| `ureq` | 3.5.2 (2026-11-12) | blocking I/O, no runtime | 1.1 | rustls (default, provider `ring`) or `native-tls` |
| `reqwest` | 0.13.5 (2026-09-08) | async on `tokio`; `blocking` wraps it | 1.1, 2 by default feature, 3 by feature | rustls (default, provider aws-lc-rs) or `native-tls` |
| `minreq` | 3.0.0 (2026-06-15) | blocking I/O, no runtime | 1.1 | rustls, `native-tls` or OpenSSL by feature; none by default |
| `attohttpc` | 0.32.0 (2026-11-05) | blocking I/O, no runtime | 1.1 | `native-tls` (default) or rustls |

- **`ureq`** "uses blocking I/O instead of async I/O" and declares `#![forbid(unsafe_code)]`
  (`src/lib.rs`). It has no HTTP/2 feature, and no variant has `h2` in the tree. The default
  features are `rustls` and `gzip`. The feature `rustls` selects `ring` and `webpki-roots`;
  `rustls-no-provider` leaves provider and roots to the application; `platform-verifier`
  verifies through `rustls-platform-verifier`. `native-tls` "is never picked up as a default"
  and must be selected in the configuration of the agent (README). All timeouts are unset by
  default except `await_100` of one second (`Timeouts::default` in `src/config.rs`).
- **`reqwest`** is built on `hyper`, `hyper-util` and `tokio`. The feature `blocking` gives a
  synchronous API: each blocking `Client` starts a thread that runs a `tokio` runtime of the
  flavour `new_current_thread` (`src/blocking/client.rs`). The module documentation states
  that the blocking API "must not be executed within an async runtime, or it will panic"
  (`src/blocking/mod.rs`). The blocking client has a default timeout of 30 seconds for a whole
  request (`Timeout::default` in `src/blocking/client.rs`). Since 0.13 `default-tls` is
  `rustls` with aws-lc-rs and `rustls-platform-verifier` (feature table of 0.13.5).
- **`minreq`** has no dependency without features. Its licence is `ISC`.
- **`attohttpc`** lists "HTTP/1.1" among its goals and "stays away from Rust's asynchronous
  stack" (README). Its licence is `MPL-2.0`.

`hyper` with `hyper-util` can serve as a client too (feature `client`); it then needs a runtime
in the caller, as the async API of `reqwest` does. It was not measured as a client.

### 3.2 Dependency trees

| Variant | Features | lock | graph | macOS | Linux | Windows | new |
|---|---|---|---|---|---|---|---|
| `ureq` without TLS | without default features | 10 | 10 | 10 | 10 | 10 | 8 |
| `ureq` rustls only (c) | without default features {`rustls`} | 36 | 36 | 25 | 25 | 24 | 24 |
| `ureq` default | default (`rustls`, `gzip`) | 42 | 41 | 30 | 30 | 29 | 30 |
| `ureq` rustls, ring, platform verifier | without default features {`rustls-no-provider`, `_ring`, `platform-verifier`} | 68 | 68 | 30 | 27 | 26 | 43 |
| `ureq` rustls, aws-lc-rs, platform verifier | without default features {`rustls-no-provider`, `platform-verifier`}, `rustls` {`aws_lc_rs`, `std`, `tls12`} | 77 | 64 | 34 | 31 | 31 | 50 |
| `ureq` native-tls (c) | without default features {`native-tls-no-default`} | 49 | 49 | 29 | 34 | 19 | 27 |
| `ureq` native-tls with roots | without default features {`native-tls`} | 50 | 50 | 30 | 35 | 20 | 28 |
| `minreq` without TLS | default | 1 | 1 | 1 | 1 | 1 | 1 |
| `minreq` rustls (c) | {`https-rustls`} | 37 | 24 | 21 | 21 | 21 | 24 |
| `attohttpc` rustls (c) | without default features {`tls-rustls-webpki-roots-ring`} | 68 | 65 | 54 | 54 | 53 | 48 |
| `attohttpc` default | default (`compress`, `tls-native`) | 79 | 75 | 58 | 60 | 49 | 55 |
| `reqwest` blocking without TLS | without default features {`blocking`} | 86 | 83 | 70 | 70 | 72 | 67 |
| `reqwest` blocking, native-tls (c) | without default features {`blocking`, `native-tls`} | 115 | 112 | 85 | 88 | 78 | 86 |
| `reqwest` blocking, rustls with ring | without default features {`blocking`, `rustls-no-provider`}, `rustls` {`ring`, `std`, `tls12`} | 134 | 131 | 89 | 87 | 87 | 103 |
| `reqwest` blocking, rustls (c) | without default features {`blocking`, `rustls`} | 156 | 128 | 93 | 91 | 91 | 119 |
| `reqwest` async default | default, `tokio` {`rt-multi-thread`} | 172 | 144 | 107 | 103 | 106 | 130 |
| `reqwest` blocking default | default and `blocking` | 173 | 145 | 109 | 105 | 108 | 131 |

The feature `_ring` of `ureq` begins with an underscore; the feature `rustls` is the documented
way to it. Every `reqwest` variant, the blocking ones included, has `tokio`, `mio`, `hyper` and
`futures-core` in the graph. No variant of `ureq`, `minreq` or `attohttpc` has one of them.

### 3.3 `cargo deny check` and `cargo audit`

`cargo audit` reports nothing for any client variant. `cargo deny check` reports no advisory,
ban or source problem for any of them; the licence check differs:

| Variant | Licence check |
|---|---|
| `ureq` without TLS | passes |
| `ureq` native-tls (`native-tls-no-default`) | passes |
| `ureq` native-tls with roots | fails: `webpki-root-certs` |
| `ureq` rustls only, `ureq` default | fails: `ring`, `rustls-webpki`, `untrusted`, `webpki-roots` |
| `ureq` rustls, ring, platform verifier | fails: `ring`, `rustls-webpki`, `untrusted`, `webpki-root-certs` |
| `ureq` rustls, aws-lc-rs, platform verifier | fails: `aws-lc-rs`, `aws-lc-sys`, `rustls-webpki`, `untrusted`, `webpki-root-certs` |
| `minreq` without TLS | fails: `minreq` itself (`ISC`) |
| `minreq` rustls | fails: `minreq`, `aws-lc-rs`, `aws-lc-sys`, `rustls-webpki`, `untrusted`, `webpki-roots` |
| `attohttpc` default | fails: `attohttpc` itself (`MPL-2.0`) |
| `attohttpc` rustls | fails: `attohttpc`, `ring`, `rustls-webpki`, `untrusted`, `webpki-roots` |
| `reqwest` blocking without TLS | passes |
| `reqwest` blocking, native-tls | passes |
| `reqwest` blocking, rustls with ring | fails: `ring`, `rustls-webpki`, `untrusted`, `webpki-root-certs` |
| `reqwest` blocking, rustls | fails: `aws-lc-rs`, `aws-lc-sys`, `rustls-webpki`, `untrusted`, `webpki-root-certs` |
| `reqwest` default (async or blocking) | fails: the five above and `encoding_rs` |

## 4. TLS backends

| Backend | What it is | Build | Licences that `deny.toml` does not allow today |
|---|---|---|---|
| rustls 0.23.46 with `ring` 0.17.14 | TLS in Rust, cryptography from `ring` | compiles C and assembly through `cc` | `ring`: `Apache-2.0 AND ISC`; `rustls-webpki` and `untrusted`: `ISC` |
| rustls 0.23.46 with `aws-lc-rs` 1.19.0 | TLS in Rust, cryptography from AWS-LC | compiles C through `cc`; `cmake` is in the tree as a build crate | `aws-lc-rs`: `ISC AND (Apache-2.0 OR ISC)`; `aws-lc-sys` 0.47.0: an expression with `ISC`, `BSD-3-Clause` and `MIT-0`; `rustls-webpki` and `untrusted`: `ISC` |
| `native-tls` 0.2.18 | binding to the TLS of the system | Security.framework on macOS, SChannel on Windows, OpenSSL through `openssl-sys` elsewhere | none |

- `rustls` itself is `Apache-2.0 OR ISC OR MIT` and passes.
- The root certificates come from one of three places. `webpki-roots` and `webpki-root-certs`
  (1.0.12) compile the Mozilla roots into the binary and carry `CDLA-Permissive-2.0`, which
  `deny.toml` does not allow. `rustls-platform-verifier` 0.7.1 (MSRV 1.85) verifies against
  the trust store of the operating system; in the measured variants `webpki-root-certs` is in
  its tree as well. `native-tls` uses the trust store of the system. A certificate that an
  administrator adds to the trust store of the system is not part of the compiled-in roots.
- With `native-tls` the graph differs most by platform (`ureq`: 19 packages on Windows, 34 on
  Linux). On Linux `openssl-sys` links the OpenSSL of the system, or compiles a copy with the
  feature `vendored` (README of `reqwest`, feature list of `ureq`).
- `ring` 0.17 brings `getrandom` 0.2.17 and `windows-sys` 0.52.0 into the graph, beside
  `getrandom` 0.4.3 and `windows-sys` 0.61.2 of the workspace. `cargo deny` reports such
  duplicate versions as warnings (`multiple-versions = "warn"`). The variants with aws-lc-rs
  and with `native-tls` do not have these two.
- The last release of `ring` is 0.17.14 of 2025-03-11; the last commit in its repository is of
  2026-11-07. `aws-lc-rs` 1.19.0 is of 2026-10-28, `rustls` 0.23.46 of 2026-10-14, and
  `native-tls` 0.2.18 of 2026-02-18.
- A server can also run without TLS in the process, behind a proxy that terminates TLS. The
  variants without TLS in section 2 show the tree of that case.

## 5. MSRV

The workspace declares `rust-version = "1.89"`. No package in any measured tree declares a
`rust-version` above 1.89. Between 0 and 30 packages per graph declare none and are not
covered by this statement.

| Crate | Declared MSRV | Highest declared MSRV in its tree |
|---|---|---|
| `axum` 0.8.9 | 1.80 | 1.85 (`tokio` 1.54.1, `hyper-util` 0.1.21) |
| `actix-web` 4.20.0 | 1.89 | 1.89 (`actix-web`, `actix-http` 3.22.0, `actix-rt` 2.16.0, `actix-server` 2.10.0 and others) |
| `hyper` 1.12.0 | 1.63 | 1.85 (`tokio`, `hyper-util` 0.1.21) |
| `tiny_http` 0.12.0 | none declared | 1.71 (`log`) without TLS |
| `rouille` 3.6.2 | none declared | 1.88 (`time` 0.3.55, `icu_provider` 2.4.0) |
| `ureq` 3.5.2 | 1.85 | 1.85 |
| `reqwest` 0.13.5 | 1.85 | 1.88 (`icu_provider` 2.4.0, through `url`) |
| `minreq` 3.0.0 | 1.63 | 1.85 with rustls (`zeroize` 1.9.2, `getrandom` 0.4.3) |
| `attohttpc` 0.32.0 | none declared | 1.88 (`icu_provider` 2.4.0, through `url`) |

- `tokio` keeps "a rolling MSRV policy of at least 6 months" and raises it only in a minor
  release; 1.54 raised it from 1.71 to 1.85. The releases `1.51.x` (until March 2027) and
  `1.53.x` (until September 2027) are LTS releases with MSRV 1.71 (README of 1.54.1).
- `ureq` does "not consider MSRV changes to be breaking for the purposes of semver", makes
  none in a patch release, and gives each its own minor release (README).
- `actix-web` 4.20.0 sits exactly at 1.89, the `rust-version` of the workspace.
- The default resolver of edition 2021 does not take `rust-version` into account, which is why
  the workspace pins `tree-sitter-highlight` by hand (`crates/stemma-tui/Cargo.toml`).

## 6. Streaming of large bodies

The premise of ADR-015 is a history of up to a few gigabytes. A body that is read into memory
at once is limited by that memory; the items below show where each crate streams.

**Servers**

- `axum`: `Body::into_data_stream()` gives the request body as a stream, and
  `Body::from_stream()` builds a response from one (`axum-core/src/body.rs`). The extractor
  `Bytes` and those built on it (`String`, `Json`, `Form`) refuse a body above 2 MB by default;
  `DefaultBodyLimit` changes the limit (`axum-core/src/extract/default_body_limit.rs`).
- `actix-web`: the extractor `web::Payload` implements `Stream`, and
  `HttpResponseBuilder::streaming()` sends one. `PayloadConfig` limits the extractors `Bytes`
  and `String` to 256 kB by default; `Payload` itself "does not automatically check" the limit
  (`src/types/payload.rs`).
- `hyper`: the request body is `hyper::body::Incoming`, which yields frames; a response takes
  any implementation of `http_body::Body`. Limits are left to the application.
- `tiny_http`: `Request::as_reader()` returns `&mut dyn Read`; `Response::new()` takes any
  `Read` with an optional length, and `Response::from_file()` exists. A response without a
  length, or of 32768 bytes and more, is sent chunked (`chunked_threshold` in
  `src/response.rs`).

**Clients**

- `ureq`: `Body::into_reader()` and `as_reader()` give the response as `Read`. The convenience
  methods `read_to_vec()`, `read_to_string()` and `read_json()` stop at 10 MB unless
  `with_config().limit()` raises it (`src/body/mod.rs`). A request body can be a `File`, a
  `&File`, or any `Read` through `SendBody::from_reader()` (`src/send_body.rs`); `ureq` sends
  `Content-Length` when the size is known and `Transfer-Encoding: chunked` otherwise (README).
- `reqwest`, blocking: `Response` implements `Read` and has `copy_to()`; `Body::new(reader)`
  and `Body::sized(reader, len)` send from a `Read` (`src/blocking/`). The default timeout of
  30 seconds covers the whole request, the body included.
- `reqwest`, async: `Response::chunk()` and, with the feature `stream`, `bytes_stream()`;
  `Body::wrap_stream()` for a request (`src/async_impl/`).
- `minreq`: `send_lazy()` returns a `ResponseLazy` that implements `Read`. The request body is
  set with `with_body<T: Into<Vec<u8>>>` and is held in memory as a whole (`src/request.rs`).
- `attohttpc`: `ResponseReader` implements `Read` and has `write_to()`; a request body is
  `body::File`, `body::Bytes`, or an own implementation of the trait `Body`
  (`src/request/body.rs`).

No transfer was run; the items above are read from the sources, not measured.

## 7. Maintenance

| Crate | Latest stable | First release | Versions | Last commit | Owners on crates.io | Downloads (90 days) |
|---|---|---|---|---|---|---|
| `axum` | 0.8.9, 2026-04-14 | 2021-07-22 | 94 | 2026-11-21 | 2 users, 2 teams of `tokio-rs` | 175 million |
| `actix-web` | 4.20.0, 2026-11-17 | 2017-10-23 | 165 | 2026-11-21 | 3 users, 1 team of `actix` | 14 million |
| `hyper` | 1.12.0, 2026-10-06 | 2014-11-22 | 253 | 2026-11-17 | 1 user, 1 team of `hyperium` | 302 million |
| `tiny_http` | 0.12.0, 2022-10-06 | 2015-05-07 | 30 | 2023-05-16 | 3 users | 20 million |
| `rouille` | 3.6.2, 2023-04-24 | 2016-01-24 | 37 | 2025-06-17 | not queried | 7 million |
| `ureq` | 3.5.2, 2026-11-12 | 2018-06-22 | 119 | 2026-11-22 | 2 users | 93 million |
| `reqwest` | 0.13.5, 2026-09-08 | 2016-10-16 | 126 | 2026-11-16 | 1 user | 281 million |
| `minreq` | 3.0.0, 2026-06-15 | 2018-03-18 | 52 | 2026-09-09 | 1 user | 2 million |
| `attohttpc` | 0.32.0, 2026-11-05 | 2019-03-23 | 52 | 2026-11-17 | 1 user | 8 million |
| `tokio` | 1.54.1, 2026-10-21 | 2016-07-01 | 201 | 2026-11-21 | 2 users, 1 team of `tokio-rs` | 325 million |

- No repository is archived.
- `axum` released 0.8.5 to 0.8.9 between 2025-09-28 and 2026-04-14. `ureq` released five
  versions between 2026-09-13 and 2026-11-12, `actix-web` five between 2026-10-23 and
  2026-11-17.
- `axum`, `reqwest` and `attohttpc` are below 1.0. `reqwest` 0.13.0 of 2025-12-30 changed
  `default-tls` from `native-tls` to `rustls` (feature tables of 0.12.28 and 0.13.0), and
  `minreq` 3.0.0 of 2026-06-15 is a new major version.
- `tiny_http` has had no release for four years and no commit since 2023-05-16; its optional
  TLS depends on `rustls` 0.20 with open advisories (section 2.3).

## 8. What an async runtime in the client would change

### 8.1 For `stemma-core`

- With `ureq`, `minreq` or `attohttpc` there is no runtime: a request is a blocking call on the
  thread of the caller, as file I/O is today, and the signatures of `stemma-core` stay
  synchronous.
- With `reqwest::blocking` the signatures stay synchronous too, but `tokio` is in the tree,
  each `Client` owns a thread with a runtime, and the call panics when it runs inside an async
  runtime. `stemma-core` would then carry a runtime that its callers do not see.
- With the async API of `reqwest` (or `hyper` as a client) a request is a future. A future
  runs only when an executor polls it, and the standard library has none. Either the functions
  of `stemma-core` that reach the network become `async fn`, and so do their callers up to
  `main` in `stemma-cli` and the event loop of `stemma-tui`; or a synchronous function builds
  a runtime and calls `Runtime::block_on` at the boundary, which is what `reqwest::blocking`
  does internally.
- `stemma-cli`, `stemma-tui` and `fuzz/` depend on `stemma-core`. A dependency of
  `stemma-core` is in the tree of all three; a dependency of a new crate, or of `stemma-cli`
  alone, is not in the tree of the fuzz targets.
- `stemma-cli` handles SIGINT, SIGTERM and SIGHUP on a thread of its own (`signal-hook`,
  `crates/stemma-cli/src/cli.rs`). A blocking read on a socket returns on data, on an error or
  on a timeout. `ureq` sets no timeout by default; `reqwest::blocking` sets 30 seconds for the
  whole request.
- The parallel sections of `stemma-core` use `thread::scope`. `tokio` with `rt-multi-thread`
  starts a pool of worker threads of its own beside them; the flavour `new_current_thread`
  does not.

### 8.2 For `just deps`

- `cargo audit` scans `Cargo.lock`. The column **new** of section 3.2 is the number of package
  names that are added to the 167 of today: 24 for `ureq` with rustls, 27 with `native-tls`,
  86 to 131 for the `reqwest` variants with TLS. Each added package falls under
  `unmaintained = "all"` and `yanked = "deny"`: an advisory against any of them fails
  `just deps` from then on.
- The licence check passes without a change to `deny.toml` for `ureq` and `reqwest` with
  `native-tls` and without TLS. `minreq` and `attohttpc` fail on their own licence. Every
  rustls variant needs `ISC` (for `ring` or `aws-lc-rs`, `rustls-webpki` and `untrusted`) and,
  with compiled-in roots or the platform verifier, `CDLA-Permissive-2.0`; with aws-lc-rs the
  expression of `aws-lc-sys` in addition. `deny.toml` has two ways to admit a licence: the
  list `allow`, or an exception by crate, as ADR-016 and ADR-020 chose.
- `deny.toml` is a level 3 file (ADR-004, table of levels), so a variant that needs a licence
  entry changes a level 3 file.
- The variants with `ring` add two duplicate versions (`getrandom` 0.2, `windows-sys` 0.52) to
  the two warnings of today. `ureq` with `native-tls` adds `pem-rfc7468` 1.0.0 beside 0.7.0.
  The client variants with aws-lc-rs and `reqwest` with `native-tls` add none. These are
  warnings, not failures.
- `just deps` runs in the root of the repository. `fuzz/Cargo.lock` is a second lockfile that
  a new dependency of `stemma-core` changes and that the recipe does not scan.
- An async runtime as such changes nothing in how `just deps` works. It changes the size of
  the tree: the smallest `reqwest` variant with TLS has 86 new packages against 24 to 27 for
  `ureq`, and the smallest server variant with `tokio` (`hyper`) has 13.

## 9. Rules of the project that apply

- A new dependency needs an ADR, as ADR-016 (`ratatui`) and ADR-020 (`ssh-key`) did.
- ADR-004, rule 7: "New direct runtime dependencies are level 2. [...] Cryptography crates are
  level 3." ADR-020 was handled as level 3 under this rule. `ring`, `aws-lc-rs`, `rustls` and
  `native-tls` implement or bind cryptography; with `ureq` or `reqwest` they are transitive
  dependencies, and direct ones where a provider is selected through a dependency on `rustls`
  itself (the `rustls-no-provider` variants). The rule does not say whether a transitive TLS
  backend counts as a cryptography crate.
- The rules above are those of this repository. stemma-hub is a separate private project
  (STEMMA-266) without files today; which of them it takes over is not written down anywhere.

## Not verified

- No variant was built with Rust 1.89, on Linux or on Windows; the trees for those targets
  come from `cargo tree --target`.
- No request was sent and no server was started: throughput, memory use with large bodies, and
  the behaviour on an interrupted transfer are not measured.
- Build time and binary size were not measured.
- The licence texts were not read; the table gives the SPDX expressions of the manifests as
  `cargo deny` evaluates them.
- The individual owners on crates.io were counted, not vetted, and the owners of `rouille`
  were not queried.
- Other candidates were not measured: `warp`, `poem`, `rocket`, `salvo` and `tide` on the
  server side, `isahc`, `curl` and `hyper` as a client on the client side.
- HTTP/2 and HTTP/3 were not examined beyond the feature tables.
