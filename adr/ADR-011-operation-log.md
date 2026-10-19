# ADR-011: The operation log

**Status:** Proposal, partially confirmed. Decisions marked *(confirmed)* were agreed with the
author on 2026-10-19; the others are proposals.

## Context

Four decisions already fix the frame of the log:

- It is local. It is not an object of the repository and is not exchanged with the history, so a
  secret that was committed by mistake can be removed, and local paths and discarded attempts stay
  private (ADR-005).
- Every entry records the actor (ADR-002).
- It keeps every commit that an operation made unreachable, above all the working-copy commit that
  a snapshot replaces (ADR-010). Without the log the guarantee "no state of an agent is lost" is
  empty, so no snapshot is made before the log exists.
- It is the log of changes of references that ADR-008 left open (the reflog), and the garbage
  collection of ADR-007 treats the commits that it names as roots.

What is missing is the format, the place, the order of writing, and what an entry holds. This ADR
decides them. Implementation will be `crates/stemma-core/src/oplog.rs`.

## Decision

- **One file per entry, numbered** *(confirmed)*. The log is the directory `.stemma/oplog/`. Entry
  *n* is the file `oplog/<n>`, with *n* as decimal digits, zero-padded to a fixed width so that the
  names sort in the order of the numbers, starting with 1. A new entry is created with
  `create_new` on the next number; if that file exists, another writer was faster, and the writer
  tries the next number. This orders concurrent writers without a lock, as the files of
  references do (ADR-008). Each file holds one entry, so a damaged entry does not touch another,
  and one entry can be replaced alone (STEMMA-26). The number is also written inside the entry,
  and a file whose inside and name disagree is corrupt. A gap in the numbers is reported, not
  skipped: it means that an entry is lost.
- **An entry records the changed references, old and new** *(confirmed)*. It holds the actor, the
  time, the name of the operation, and for each pointer that the operation changes its old and its
  new value: a branch, HEAD, or the working-copy commit. It is the reflog of ADR-008 for every
  pointer in one place. An old value is the one that stops being reachable, so the old values and
  the new values of all entries, plus the current references, are the roots of the garbage
  collection. A state is restored by putting the old values back. The entry holds no file names and
  no file content: the log keeps what the commits keep, and it names no path of its own.
- **The entry is written before the pointer moves, and flushed** *(confirmed, consequence of
  ADR-010)*. The writer reads the current pointers, writes the entry with the old and the new
  values, flushes the file and the directory, and only then moves the pointers with the
  compare-and-swap of ADR-008. If the process stops in between, the old commit is already in the
  log. If the order were the other one, a stop after the move would leave a commit that no
  reference and no entry names.
- **An entry is an intent, not a proof** *(proposal)*. If the compare-and-swap fails after the
  entry was written, because another writer moved the pointer first, the entry names a move that
  did not happen. The log is not wrong for it: every old value in it was the value of a pointer at
  some moment, and that is all the garbage collection needs. A reader that wants to know whether a
  move happened compares the entry with the pointers or with the next entry of the same pointer
  (see the open items).
- **Only a change writes an entry** *(proposal)*. A snapshot of an unchanged directory creates no
  commit and no entry, so a command that only reads does not grow the log. `init` writes the first
  entry (operation `init`, HEAD from nothing to the initial branch).
- **Nothing is deleted** *(proposal)*. No operation removes an entry, and no garbage collection
  runs in phase 1 (ADR-007). Removing a secret replaces the entries that name it by a stub that
  keeps the number (STEMMA-26), so the numbering has no gap.
- **The format is strict text** *(proposal)*. A reader accepts what the writer writes and nothing
  else, as in ADR-005, so one entry has exactly one encoding. The sketch below is the proposal.
  The time is the one the writer claims, in seconds since the Unix epoch, written as in a commit
  (ADR-005); it is not a proof, and the order is the number, not the time.
- **The repository format version rises to 2 with the log** *(proposal)*. `oplog/` is part of the
  layout (ADR-009), and `Repository::open` rejects version 1 with the existing
  `UnsupportedFormat`. No repository exists outside of tests and the author's trials, so the
  migration is to run `init` again.
- **The first slice** *(confirmed)*. The core writes and strictly reads entries; `init` and every
  operation that changes a pointer write one; `stemma op log` lists them. Restoring and undoing
  come as commands of their own (STEMMA-28).

### Sketch of an entry *(proposal)*

```text
stemma-oplog 1
number 42
time 1700000000
actor human daniel
operation snapshot
working-copy b3:1d71…eed9 b3:4feb…b32c
branch main b3:8cb4…b3e8 b3:b5f2…e662
```

- The first four lines come in this order, each once. `operation` is a lowercase name of letters,
  digits, and `-`, starting with a letter.
- Then come the lines of the pointers that changed, at least one: `head`, then `working-copy`,
  then `branch <name>` for each branch, sorted by name in UTF-8 byte order. Each has the old and
  the new value; `-` means that the pointer did not exist or does not exist any more.
- A branch and the working-copy commit have a commit ID or `-`. HEAD has `branch:<name>` or
  `commit:<id>` or `-`.
- The file ends with a newline and has no message and no further lines.

## Consequences

- Every operation that changes a pointer goes through one function of `Repository`, which orders
  "read, write the entry, flush, move" (see STEMMA-3 and STEMMA-22). The stores alone must not move
  pointers in production code.
- The log grows with every change, and a snapshot at every command makes many entries. Nothing
  removes them in phase 1; the open items name the measure that decides about retention.
- The log keeps commits that a rewrite or a purge would otherwise drop. A secret in a tracked file
  is therefore in the log until it is purged (STEMMA-26), which is why the log is local.
- A hub never sees the log. What a hub keeps is its own record of ref updates (ADR-003).
- `Repository::init` creates `oplog/` and writes entry 1, so the order "entry, then move" holds
  from the first pointer on.

## Open items

- [ ] How a reader learns that an intent did not happen: a later entry `abort <n>`, a comparison
      with the pointers at read time, or a flag that is written after the move. Decide with the
      first operation that moves a pointer.
- [ ] Retention: whether many snapshot entries of one working-copy change are merged after some
      time, and what a garbage collection may remove (nothing before ADR-007's collection
      exists). Decide with a measurement of a real session.
- [ ] The stub that replaces an entry when a secret is purged, who may purge, and how the purge is
      audited without keeping the secret (STEMMA-26).
- [ ] The width of the number and what happens at the limit, and a recovery when the highest
      number is missing.
- [ ] Restoring: what `stemma op restore` does when the working directory has changed since, and
      the entry that the restore writes (STEMMA-28).
- [ ] Who may read the log (ADR-003): it holds IDs and the actor, no paths, but it makes
      commits reachable that a role may not be allowed to see.
- [ ] A reading order that does not read every file: the newest first, and a limit.
- [ ] The `session` of an agent is part of its actor; whether the log offers "what did this
      session do" as a view.
- [ ] Windows: the flush of a directory is a no-op there, so the order "entry, then move" holds
      for atomicity but not for durability (ADR-007).
