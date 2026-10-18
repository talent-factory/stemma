# ADR-007: Object Storage

**Status:** Proposal, partially confirmed. Decisions marked *(confirmed)* were agreed; the others
are proposals.

## Context

Objects (ADR-005) need a place to live. The first store must be simple enough to reason about,
must not lose or half-write an object, and must not tie the core to the file system, because a
server will later keep objects elsewhere (API-first). The implementation is
`crates/stemma-core/src/store.rs`.

## Decision

- **Loose objects** *(confirmed)*. One file per object, at
  `<metadata directory>/objects/<algorithm>/<first 2 hex digits>/<remaining 62 hex digits>`,
  for example `.stemma/objects/b3/ab/cdef…`. The algorithm directory follows the ID prefix of
  ADR-001, so a second hash algorithm can coexist without moving files *(confirmed)*. The hex
  digits are lowercase, so the paths do not collide on a case-insensitive file system.
- **No compression** *(confirmed)*. The file holds exactly the hashed input of ADR-001/ADR-005:
  `<kind> <length>`, a NUL byte, and the body. Compression and pack files are different storage
  formats. Introducing one needs an ADR, and it changes no object ID, because the ID covers the
  uncompressed encoding.
- **Verified reads.** A read parses the header, checks that the length is the canonical decimal
  length of the body, hashes the content again, and compares the result with the ID. Anything
  else is a `Corrupt` error and never returned as an object. A missing object is `None`, which is
  different from a failed read.
- **Safe writes.** An object is written to a uniquely named file in `<metadata directory>/tmp/`,
  flushed to disk, made read-only, and renamed to its final path. The directory entries are
  flushed afterwards. A reader sees no object or a complete one. An object that exists is never
  rewritten, and two writers storing the same object are harmless, because the content is the
  same by construction.
- **Interface.** `ObjectStore` is a trait with `put_raw`, `get`, and `contains`. `put_blob`,
  `put_tree`, and `put_commit` are provided methods that return the same ID as the object's own
  `id()`. `get` returns the kind and the encoded body; parsing trees and commits is a separate
  step that comes with the first command that needs it. `FsObjectStore` is the file system
  implementation.
- **Authorization** is not a store concern. The authorization hook of ADR-002 is called by the
  repository operations above the store, which know the actor and the operation.

## Consequences

- Reading an object costs one hash of its content. That is the price of detecting corruption
  and of never trusting the file system.
- Many small files are slow on some file systems and large repositories. Pack files are the
  planned answer.
- Stale files can remain in `tmp/` after a crash. They are harmless and can be removed when no
  writer is active.
- `put_raw` takes the whole body as a slice, so a very large file has to fit in memory.

## Open

- [ ] Pack files and compression: format, and when objects are packed.
- [ ] Garbage collection of unreachable objects, and removal of stale files in `tmp/`.
- [ ] Streaming for large blobs, so that neither writing nor reading needs the whole body in
      memory.
- [ ] A repository format file (`.stemma/format`, planned for `stemma init`) should be checked
      before a store opens a repository; the store does not check it yet.
- [ ] File permissions beyond read-only, and behavior on file systems that reject `rename` over
      an existing file (Windows).
