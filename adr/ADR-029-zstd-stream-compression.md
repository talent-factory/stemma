# ADR-029: `zstd` for the compression of the object stream

**Status:** Accepted for phase 5. Decided by the author on 2026-11-23 (STEMMA-272, on the
direction of STEMMA-270) and carried into the ADRs by STEMMA-280. The dependency is not added yet;
the slice that adds it changes `deny.toml` and is therefore a level 3 change (ADR-004). The tree
and the licences were resolved in a scratch crate on 2026-11-23 with `cargo tree`. Not verified:
`cargo deny` was not run on this tree; the licences were read from the manifests.

## Context

The transfer protocol sends whole objects without deltas and compresses the whole stream as a
property of the transport (ADR-023). The measurement there shows why: with a long window, zstd
brought the stream of this repository to 2.4 MB, against 7.4 MB for a Git pack with deltas and
25.1 MB for gzip over the stream. The decision against deltas rests on that window.

Candidates:

- **`zstd` 0.14**, the binding to the C library, compiled through `cc`. It is the only candidate
  that is known to offer the long window. It adds five package names (`zstd`, `zstd-safe`,
  `zstd-sys`, `jobserver`, `pkg-config`).
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
  memory per connection, and refuses above it with a typed error. The value, and how the sender
  learns it, are left to the slice, together with the level.
- **`deny.toml` gets three exceptions by crate**: `BSD-3-Clause` for `zstd` and `zstd-safe`, and
  `BSD-3-Clause AND BSD-2-Clause` for `zstd-sys`.

Rejected: `ruzstd` and `flate2`, for the reasons above.

## Consequences

- The build of `stemma-remote` compiles the C library of zstd. The core gets no new C code.
- The compression is no part of a format. Neither an object ID nor a stored byte depends on it,
  so the crate can be replaced without a new protocol number.
- A decoder is a parser of foreign bytes written in C. The bound of the window and the bounds for
  the size of an object and of a request (ADR-023) limit what a sender can make it hold; every
  object is hashed and read strictly after the decoder.

## Open Points

- [ ] `cargo deny check` and `cargo audit` on the tree, the build with Rust 1.89 and on Windows.
      They belong to the level 3 review of the change that adds the dependency.
- [ ] The level and the size of the window, measured on a Stemma store.
- [ ] Notice of the third-party licences in the distribution of a release (see ADR-020).
