# ADR-029: `zstd` for the compression of the object stream

**Status:** Accepted for phase 5. Decided by the author on 2026-11-23 (STEMMA-272, on the
direction of STEMMA-270) and carried into the ADRs by STEMMA-280. The dependency was added by
STEMMA-304 on 2026-11-27, a level 3 change because it changes `deny.toml` (ADR-004). `cargo deny
check` and `cargo audit` passed on the tree then (`just deps`), and the build with Rust 1.89 on
macOS. The level and the window were measured then on a Stemma store (see "Measurement").

## Context

The transfer protocol sends whole objects without deltas and compresses the whole stream as a
property of the transport (ADR-023). The measurement there shows why: with a long window, zstd
brought the stream of this repository to 2.4 MB, against 7.4 MB for a Git pack with deltas and
25.1 MB for gzip over the stream. The decision against deltas rests on that window.

Candidates:

- **`zstd` 0.14**, the binding to the C library, compiled through `cc`. It is the only candidate
  that is known to offer the long window. It adds five package names (`zstd`, `zstd-safe`,
  `zstd-sys`, `jobserver`, `pkg-config`). The lock file holds `zstd` 0.14.5 with `zstd-sys`
  2.2.0 and the C library 1.6.0, built without the default features, so that neither the decoder
  of the legacy formats nor the dictionary builder is compiled.
- **`ruzstd`**, pure Rust and `MIT`. It was not verified that its encoder offers the long window
  and the higher levels.
- **`flate2`** (gzip). The stream was about ten times larger in the measurement.

## Decision

- **`zstd` 0.14 as a direct runtime dependency of `stemma-remote`.** The core does not depend on
  it: the messages of `stemma_core::protocol` know no compression, and the in-memory `Transport`
  needs none. A hub that speaks the protocol needs the same encoding on its side.
- **The uncompressed body is the mandatory fallback.** gzip is not built beside it; it stays the
  replacement if `zstd` fails on a licence or a build.
- **The client wraps the body in the encoder and the decoder itself**, without the feature `gzip`
  of `ureq` (ADR-028). The content encoding is negotiated through the headers of ADR-023.
- **The receiver sets an upper bound for the window**, because a decoder holds the window in
  memory per connection, and refuses above it with a typed error. The bound is 2^27 bytes
  (128 MiB) on both sides and a fixed rule of the protocol (ADR-023), so a sender learns it
  from the protocol and not from a header. It is the default bound of the C library, so a
  sender that uses the library without settings keeps to it. In `stemma-remote` the typed error
  is `WindowTooLarge`, inside the error of the body of the answer.
- **The sender compresses at level 3 with a window of 2^27 bytes and long-distance matching**
  (see "Measurement").
- **`deny.toml` gets three exceptions by crate**: `BSD-3-Clause` for `zstd` and `zstd-safe`, and
  `BSD-3-Clause AND BSD-2-Clause` for `zstd-sys`.

Rejected: `ruzstd` and `flate2`, for the reasons above.

## Measurement

On 2026-11-27 the 349 commits of the first-parent history of `main` of this repository were
imported into a new Stemma store with `stemma-core` (`track` and `commit` per Git commit, on
macOS on arm64), and the answer of a fetch for the whole history was written:
4548 objects, 125.9 MB. The stream was compressed in memory with `zstd` 0.14 (C library 1.6.0);
the time is that of the encoder, and the decoder took 0.03 to 0.09 s in every row.

| Level | Window | Long-distance matching | Size | Time |
|---|---|---|---|---|
| 1 | default | no | 30.4 MB | 0.26 s |
| 1 | 2^27 | yes | 2.97 MB | 0.11 s |
| 3 | default | no | 16.8 MB | 0.24 s |
| 3 | 2^24 | yes | 4.15 MB | 0.13 s |
| 3 | 2^25 | yes | 3.13 MB | 0.13 s |
| 3 | 2^26 | yes | 2.71 MB | 0.12 s |
| 3 | 2^27 | yes | 2.67 MB | 0.13 s |
| 9 | 2^27 | yes | 2.36 MB | 0.27 s |
| 19 | default | no | 3.89 MB | 6.79 s |
| 19 | 2^27 | yes | 1.98 MB | 5.13 s |

The window decides, not the level: level 3 with the long window is a sixth of level 3 without
it and within 35 % of level 19 with it, at a fortieth of its time. Above 2^26 the window gains
little on this history, which is 126 MB; it gains on a longer history, which ADR-015 allows up
to a few gigabytes. 2^27 is the largest window that a stock decoder takes without settings, and
the memory that a decoder holds, 128 MiB per connection, is the price.

## Consequences

- The build of `stemma-remote` compiles the C library of zstd. The core gets no new C code.
- The compression is no part of a format. Neither an object ID nor a stored byte depends on it,
  so the crate can be replaced without a new protocol number.
- A decoder is a parser of foreign bytes written in C. The bound of the window and the bounds for
  the size of an object and of a request (ADR-023) limit what a sender can make it hold; every
  object is hashed and read strictly after the decoder.

## Open Points

- [x] `cargo deny check` and `cargo audit` on the tree and the build with Rust 1.89
      (STEMMA-304).
- [ ] The build on Windows, in the CI run of the change that adds the dependency.
- [x] The level and the size of the window, measured on a Stemma store (STEMMA-304).
- [ ] Notice of the third-party licences in the distribution of a release (see ADR-020).
