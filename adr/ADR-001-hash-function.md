# ADR-001: Hash Function and Hash Agility

**Status:** Accepted for BLAKE3 (confirmed). The question of standards and certification stays open.

## Decision

- BLAKE3 is the default hash. It is fast, parallelizable, and has a mature Rust implementation.
- Every object ID carries the algorithm as a prefix, for example `b3:…`, so that another
  algorithm can be introduced later without ambiguity (hash agility).
- SHA-1 is not used.
- SHA-256 stays the fallback if standardization or certification is ever required. Switching
  means a new prefix, for example `s256:`, and an ADR; existing `b3:` IDs remain valid.

## Consequences

- The hash input includes the object kind and the body length (see ADR-005 and
  `crates/stemma-core/src/object.rs`), so equal bytes of different kinds have different IDs.
- IDs of different algorithms are never equal, even for the same content.
- BLAKE3 is not a NIST standard. If an environment demands FIPS-approved algorithms, it needs
  the SHA-256 fallback.

## Open

- [ ] Are standard signature formats or certification required for any user group?
