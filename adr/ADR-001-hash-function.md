# ADR-001: Hash Function and Hash Agility

**Status:** Proposal, open.

## Decision (proposal)

- BLAKE3 is the default hash. It is fast, parallelizable, and has a mature Rust implementation.
- SHA-256 is the alternative if standardization or certification is required.
- Every object ID carries the algorithm as a prefix, for example `b3:…`.

## Open

- [ ] Final choice: BLAKE3 or SHA-256?
- [ ] Are standard signature formats or certification required?
