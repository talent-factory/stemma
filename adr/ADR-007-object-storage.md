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
- **Verified reads.** A read accepts only a regular file at the path of the object: a directory,
  a symbolic link, or a special file is damage and is never followed. It then checks that the
  header is exactly the canonical header of the kind and the length of the body, so a length
  such as `05` or `+5` is rejected, hashes the content again, and compares the result with the
  ID. Any other content is a `Corrupt` error that names the object, the file, and the reason,
  and is never returned as an object. A missing object is `None`, which is different from a
  failed read (`Io`, with the operation and the path). A missing store directory is an `Io`
  error and not an empty store.
- **Safe writes.** The metadata directory has to exist; the store creates `tmp` and `objects`
  below it but never the directory itself, so a wrong path is an error. An object is written to
  a newly created file in `<metadata directory>/tmp/`. Its name has the process ID, a counter,
  and a random number, and a name that is taken is skipped, so one writer never touches the file
  of another. The file is flushed to disk, made read-only, and renamed to its final path. A
  reader sees no object or a complete one, provided that `tmp` and `objects` are on one file
  system. On Unix the store also flushes the directories it created and the directory that
  received the object. On other platforms only the atomicity holds, not the durability of the
  directory entries.
- **Existing objects.** Storing an object that exists and reads back correctly changes nothing
  and flushes nothing again. The call returns when the object is stored and readable; it is
  durable once the writer that created it has returned. A damaged file, for example the rest of
  a failed write, is replaced when the same object is stored again, where a rename replaces an
  existing file (Unix). An entry that is not a regular file blocks the path and is reported as
  `Corrupt`. When two writers race, the second rename replaces the file with one of identical
  content, which no reader can tell apart. After an error an object may or may not be stored;
  storing it again is safe.
- **Objects of a snapshot are flushed together** *(confirmed, STEMMA-40)*. A snapshot (ADR-010)
  writes its new blobs, trees, and commit as a batch: each object goes to its own file in `tmp/`
  without a flush, and before the log entry and the move of the pointer name any of them, every
  file is flushed and renamed, and then each directory that received one is flushed once. A
  read inside the snapshot sees the objects of the batch, so a tree is still checked against its
  blobs. What a batch may lose is decided by ADR-011: an object that nothing names is harmless,
  but no entry and no pointer may name an object that is not on disk. A crash before the batch
  is finished therefore loses its objects and nothing else; their files stay in `tmp/` until the
  next writer removes them. A batch that fails removes its files, and the objects that it had
  moved stay, unnamed. The flush of each directory once for many objects is what the batch
  saves; each file is still flushed on its own, which on macOS is a full flush of the device
  cache. Measured with `scripts/measure-track.sh` (release build, macOS with APFS on an SSD, one
  run each, files of 1 KiB, 100 to a directory, other processes running): the first `track` of
  10,000 files took 97 s before and 49 s after, of 1,000 files 11.3 s and 6.7 s.
- **Read-only files.** Objects are made read-only to protect them from accidental edits. It is
  not a security measure: whoever can write to the directory can replace the file, and the next
  read detects it.
- **Interface.** `ObjectStore` is a sealed trait: only this crate implements it. It has `put_blob`,
  `put_tree`, and `put_commit`, each of which returns the same ID as the object's own `id()`; `get`,
  `get_expecting`, and `contains`; `resolve_blob`, `resolve_tree`, and `resolve_commit`; and
  `get_tree` and `get_commit`. The store computes the ID from the kind and the body, so a caller
  cannot store an object under another ID. Writing a raw body of any kind is internal to the crate, because the body of a tree or a
  commit has to come from a value that was checked. `get` returns the kind and the encoded body of
  an object whose content hashes to the ID; `get_expecting` fails with a `KindMismatch` if the kind
  differs. The `resolve_*` methods return a typed ID for an object that exists and has the kind,
  `None` for a missing object, and a `KindMismatch` for another kind. They check the content like
  `get` does, which means a full read, because the kind is known only from the stored object, and a
  damaged object is reported as `Corrupt` whichever kind was asked for. A resolved ID says that the
  object existed with that kind when it was resolved, not that it stays in the store.
  `get_tree` and `get_commit` take a typed ID, confirm the kind, and return the value that
  `Tree::parse` or `Commit::parse` reads from the body. A body that is not canonical is a
  `ReadError::Tree` or `ReadError::Commit` with the ID, apart from `ReadError::Store`, which wraps
  the errors of the store. The store never decodes a body for any other method. `FsObjectStore` is
  the file system implementation.
- **Authorization** is not a store concern. The authorization hook of ADR-002 has to be called
  by the layer above the store, which knows the actor and the operation.

## Consequences

- Reading an object costs one hash of its content. That is the price of detecting corruption
  on read. Storing an object that exists also reads and hashes it once, so that a damaged file
  is repaired and not mistaken for a stored object.
- A valid ID does not mean a valid object. The store checks that the content hashes to the ID, not
  that a tree or a commit is in canonical form. The readers `Tree::parse` and `Commit::parse` apply
  the strict reading of ADR-005; `get_tree` and `get_commit` call them, and the other methods do
  not. The IDs inside a decoded tree or commit are claims about other objects: `Tree::verify` and
  `Commit::verify` check them against a store, one level deep.
- Many small files are slow on some file systems and large repositories. Pack files are the
  planned answer.
- Stale files can remain in `tmp/` after a crash. Their names are never reused, so they are
  harmless until the next writer that takes the lock of the references removes them (ADR-011,
  STEMMA-31). An object is written under that lock by every command, so no file of a write in
  progress is removed; a caller of the library that stores objects through
  `Repository::objects()` without the lock is not protected (ADR-011). On Windows the read-only
  flag of an object file is cleared before the removal.
- Writing holds the body and a copy with the header in memory, and reading holds the file once.
  A very large file has to fit in memory.
- A hostile store directory cannot make the store follow a symbolic link, but a very large
  regular file is still read whole. That is an availability limit, not a correctness one.
- The trait stays sealed until a second store exists. Opening it later breaks nothing. It needs
  a public constructor for the returned object that checks the content against the ID, so that
  the check does not depend on each implementation.

## Open

- [ ] Pack files and compression: format, and when objects are packed.
- [ ] Garbage collection of unreachable objects.
- [x] Removal of stale files in `tmp/`: the writer that takes the lock of the references removes
      them (ADR-011, STEMMA-31).
- [ ] A cheaper flush of each file of a batch: an ordinary `fsync` per file and one full flush
      of the device at the end, as Git does in its batch mode. On macOS this would remove most
      of the remaining cost of the first `track`; it needs `fcntl` through `libc` and `unsafe`
      code, which the crate does not have so far.
- [ ] Streaming for large blobs, so that neither writing nor reading needs the whole body in
      memory.
- [x] A repository format file (`.stemma/format`) is checked before a repository opens
      (`Repository::open`, ADR-009); the store itself does not check it.
- [ ] Windows and other platforms: the code builds and its tests pass on Windows (a CI run,
      ADR-006),
      but five tests run on Unix only, and the behavior there is not known: a rename over an
      existing or read-only file, replacing a damaged object (it is reported as an error for
      now), and flushing directory entries. Decide what the store promises on each platform
      and test it.
- [x] Type object IDs per kind, so that a caller does not have to compare the kind of the result of
      `get` itself (ADR-005). Done: the typed IDs are in ADR-005, and `resolve_blob`,
      `resolve_tree`, and `resolve_commit` are the checked way from an untyped ID to a typed one.
