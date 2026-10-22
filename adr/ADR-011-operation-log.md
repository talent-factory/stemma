# ADR-011: The operation log

**Status:** Accepted. The decisions marked *(confirmed)* were agreed with the author on
2026-10-19, and the rest on 2026-10-21, apart from the cleanup of `tmp/` (2026-10-22,
STEMMA-31); all are implemented. The open items at the end are
questions for later (retention, who may read the log, the width of the number, Windows) and
none of them changes a decision.

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
decides them. Implementation: `crates/stemma-core/src/oplog.rs` (the log) and
`crates/stemma-core/src/repository.rs` (the order of writing).

## Decision

- **One file per entry, numbered** *(confirmed)*. The log is the directory `.stemma/oplog/`. Entry
  *n* is the file `oplog/<n>`, with *n* as twelve decimal digits, zero-padded so that the names
  sort in the order of the numbers, starting with 1. Each file holds one entry, so a damaged entry
  does not touch another, and one entry can be replaced alone (STEMMA-26). The number is also
  written inside the entry, and a file whose inside and name disagree is corrupt. A gap in the
  numbers is reported, not skipped: it means that an entry is lost. A directory listing is not a
  snapshot while a writer adds files, so the reader lists the directory again before it reports a
  gap, and a writer, which only needs the end of the log, looks at the largest number and at the
  tip file (below), and checks nothing else. Names that start with a dot are skipped, as for the
  references, because file managers and editors leave such files; any other name that is not an
  entry is reported.
- **An entry records the changed references, old and new** *(confirmed)*. It holds the actor, the
  time, the name of the operation, and for each pointer that the operation changes its old and its
  new value: a branch, HEAD, or the working-copy commit. It is the reflog of ADR-008 for every
  pointer in one place. An old value is the one that stops being reachable, so the old values and
  the new values of all entries, plus the current references, are the roots of the garbage
  collection. A state is restored by putting the old values back. The entry holds no file names and
  no file content: the log keeps what the commits keep, and it names no path of its own.
- **The entry is written before the pointer moves, and flushed, under the lock of the
  references** *(confirmed)*. The writer takes the lock of the references and keeps it from the
  checks to the move. Under the lock it reads the pointer, checks that the change can be made (a
  name that is not in the way, a branch that is where the caller saw it, a commit that is in the
  store), writes the entry, flushes the file and the directory, and only then moves the pointer
  with the compare-and-swap of ADR-008. If the process stops in between, the old commit is already
  in the log. If the order were the other one, a stop after the move would leave a commit that no
  reference and no entry names. The lock makes the checks binding: no other change can come
  between them and the move, so no entry names a move that was refused, and `set_head` writes the
  value that HEAD really had. The price is that the entry, one small file that is written and
  flushed, is part of the critical section. A command that writes (`track`, `commit`, `status`,
  `op restore`) takes the lock even earlier, before its first object, and keeps it over its
  snapshot to its last move, so that no purge deletes an object between the two (ADR-014,
  STEMMA-49); while it saves, a reader finds the lock held and reads the newest entry as
  possibly unfinished. Ctrl-C removes the lock before the process ends.
- **An entry is an intent, not a proof, and nothing is written after the move** *(confirmed)*.
  With the lock held over the checks, an entry describes a move that was going to be made. It can
  still name a move that did not happen: the file system can fail between the entry and the move,
  and the move can fail with an I/O error. That is reported to the caller together with the entry
  (`UpdateError::MoveFailed`), and the entry stays. The log is not wrong for it: every old value
  in it was the value of a pointer at some moment, and that is all the garbage collection needs.
  There is no marker for the outcome, because a second write after the move could be half done or
  lost, and then a move that happened would look as if it had not. A reader reads the outcome
  from the chain of the entries of one pointer: a later entry whose old value is the new value of
  this one confirms the move, and the current value of the pointer confirms the last one. The
  reading is `oplog::outcomes`. "The move did not take effect" is the strongest
  statement of the log, so it is made only on clear evidence: a later entry whose old value is the
  *old* value of this one, or a current value that equals the old value while no writer is at work
  (the lock of the references is not held before and after the references are read, and the log
  has not grown meanwhile). A writer between its entry and its move looks the same as a crash
  there, so while one is at work the outcome is unknown. Any other value is a change outside the
  log and leaves the outcome unknown, as does the working-copy commit and a pointer whose value
  cannot be read. The working-copy commit has its place in the references since STEMMA-22
  (`.stemma/working-copy`); a caller that does not give its value to `oplog::Current` leaves the
  last entry that moves it unknown. A lock that a crashed writer left makes the newest entries unknown until
  someone removes it. A separate marker can come with a later version of the entry encoding if a
  use appears; version 2 went to the stub and the `purge` entry (ADR-014).
- **Only a change writes an entry** *(confirmed)*. A snapshot of an unchanged directory creates no
  commit and no entry, so a command that only reads does not grow the log. `init` writes the first
  entry (operation `init`, HEAD from nothing to the initial branch).
- **Nothing is deleted** *(confirmed)*. No operation removes an entry, except the purge that
  replaces entries by stubs (ADR-014), and no garbage collection
  runs in phase 1 (ADR-007). Removing a secret replaces the entries that name it by a stub that
  keeps the number (ADR-014, STEMMA-26), so the numbering has no gap.
- **The format is strict text** *(confirmed)*. A reader accepts what the writer writes and nothing
  else, as in ADR-005, so one entry has exactly one encoding. The sketch below is the format.
  The time is the one the writer claims, in seconds since the Unix epoch, written as in a commit
  (ADR-005); it is not a proof, and the order is the number, not the time.
- **The repository format version rises to 2 with the log** *(confirmed)*. `oplog/` is part of
  the layout (ADR-009), and `Repository::open` rejects version 1 with the existing
  `UnsupportedFormat`. No repository exists outside of tests and the author's trials, so the
  migration is to run `init` again.
- **A tip file names the last entry** *(confirmed; STEMMA-30)*. `.stemma/oplog-tip`
  holds the number of the last entry in the strict form `stemma-oplog-tip 1\nnumber <n>\n`. The
  writer replaces it after the entry is in place and before the pointer moves, with a temporary
  file that is flushed and a rename. The flush of the directory is best effort: a tip that is not
  on disk after a crash is one behind or missing, and both are accepted.
  - The writer reads the tip before it takes a number. A tip *beyond* the largest number in the
    directory means that the last entries are lost, and the writer refuses
    (`OpLogError::TipAhead`) because it would give a number a second time to another move. The
    reader reports the same. The message names the file and says what to do, because no command
    repairs the log yet.
  - A tip one behind, or a missing file (a log from before the tip file), is no fault: the next
    entry writes the tip.
  - Both the reader and the writer read the tip *before* they list the directory. A writer links
    entry *n* before it writes tip *n*, so a tip that was read first is covered by the listing
    that follows. In the other order, a reader that runs while a writer appends could see a tip
    beyond its listing and report a loss that did not happen.
  - The tip advances only for an entry that is flushed. After `NotFlushed` the entry may not
    survive a crash, and a tip that did would then be beyond the log and refuse every later
    writer for an entry whose move never happened. A failure to write the tip is no error of the
    move: the entry is in place and the tip stays one behind.
  - A gap in the middle does **not** stop the writer. A refusal would block the move that repairs
    the log (`stemma op restore`, STEMMA-28). The reader reports the gap, as before.
  - The tip guards against accidents and crashes, not against an attack: who removes the tip, or
    the tip and the entry, stays unnoticed (a missing tip is accepted by design). A hash chain
    with an anchor would be needed (STEMMA-41).
- **The first slice** *(confirmed)*. The core writes and strictly reads entries, and `init` and every
  operation that changes a pointer write one. The commands that show and use the log are slices of
  their own: `stemma op log` (STEMMA-27), and restoring and undoing (STEMMA-28).
- **Only `Repository` writes the log and moves a pointer** *(confirmed)*. `Repository::update_branch`
  and `set_head` order "check, write the entry, flush, move" under the lock, `Repository::refs()`
  returns a view for reading, and `Repository::oplog()` can read the log. `FsOpLog::append` and
  `Entry::new` are visible inside the crate only, so through `Repository` no caller can move a
  pointer past the log, add an entry past the authorizer, invent an entry that was never in the
  log, or pair the references of one repository with the objects of another. This is a convention
  of the API and not a boundary: the files are plain files, and code that builds its own store on
  the same directory is not stopped. A refusal by the authorizer, a name that is in the way, a
  branch that is not where the caller saw it (seen under the lock), and a commit that is not in the
  store write no entry. A move to the value that a pointer has writes none either.
- **The caller is told whether an entry exists, and whether the pointer moved** *(confirmed)*. An
  error that leaves an entry carries it (`UpdateError::entry()`): `MoveFailed` (the move failed
  after the entry; the pointer did not move), `PartlyMoved` (an operation that moves two pointers
  moved the first and failed on the second; do not repeat it, read the pointers), `NotDurable` (the
  move was made, a directory could not be flushed; do not repeat it), and `Log` with `NotFlushed`
  (the entry is in the log but is not known to be flushed, so the move is refused; the pointer did
  not move, and a second attempt writes another entry). `UpdateError::pointer_moved()` answers the
  second question: it is true for `NotDurable` and `PartlyMoved`. Any other error of the log means
  that no entry was written. `op log` marks the entry of a `PartlyMoved` as not taken (`!`),
  because not every pointer of it reached its new value, although one did.
- **An entry has a size limit that the writer keeps** *(confirmed)*. The reader refuses an entry
  above 64 KiB, so the writer refuses it before it writes anything (`EntryTooLarge`). The actor
  and the names in an entry have no limit of their own, so this is the only bound; without it, an
  actor of 70 000 characters would make a log that cannot be read from its first entry.
- **An entry is published with a hard link** *(confirmed)*. The writer writes the entry to a
  temporary file in `tmp/`, flushes it, and links it to the name of the next free number. The link
  fails if the name exists, so the entry is whole from the first moment it can be seen, and a
  writer that loses looks for the end of the log again and tries the next number. Writers of
  `Repository` do not meet here, because they hold the lock; the retry is for any other writer. A
  file system that has no hard links cannot hold a log; that is reported as an I/O error that
  names the hard link. The numbers are twelve digits, so the log holds up to 999 999 999 999
  entries and then reports that it is full (`Full`); a writer that is overtaken 64 times gives up
  (`Contended`).
- **`tmp/` is scratch** *(confirmed)*. A temporary name is removed after the link on a best effort
  basis. If the removal fails, the entry is whole and flushed, so the write succeeds and the move
  goes ahead; a failure of a cleanup must not stop a move, and it must not leave an entry that
  names a move that was never made. A name that stays costs only space until the next writer.
- **The writer that holds the lock empties `tmp/`** *(confirmed, STEMMA-31, 2026-10-22)*. Every
  writer of `tmp/` holds the lock of the references while it writes there: the commands that
  write from before their first object (ADR-014), the moves of `Repository`, and the purge with
  its resume and abort. Whoever takes the lock is therefore the only writer, and every file in
  `tmp/` is left over from a write that failed or was interrupted. Right after the lock is taken,
  every name in `tmp/` that is not a directory is removed, without following a link, and so is
  every directory `.stemma-init-…` next to `.stemma` that an interrupted `init` left (ADR-009).
  The cleanup is best effort, like the removal after the link: a name that cannot be removed
  stays, and the next writer tries again. The lock decides and not the age of a file, so a slow
  writer, such as the first `track` of many files, is never cut short, and there is no command
  to remember. Two other ways were weighed: a cleanup in `open` or a garbage collection after an
  age, which is a guess, and the process ID in the name, which is reused and cannot be checked
  without platform code. The lock is advisory: a person who removes the lock of a running
  process by hand lets the next writer remove its temporary file, and that writer then fails at
  its rename or link without damage. A caller of the library that stores objects through
  `Repository::objects()` without the lock is not protected either. A lock that stays after
  `kill -9` stops the cleanup, as it stops every change, until it is removed.

### Sketch of an entry

```text
stemma-oplog 1
number 42
time 1700000000
actor human daniel
operation snapshot
working-copy b3:1d71…eed9 b3:4feb…b32c
branch main b3:8cb4…b3e8 b3:b5f2…e662
```

- The first five lines, with the header, come in this order, each once. `operation` is a lowercase name of letters,
  digits, and `-`, starting with a letter.
- Then come the lines of the pointers that changed, at least one: `head`, then `working-copy`,
  then `branch <name>` for each branch, sorted by name in UTF-8 byte order. Each has the old and
  the new value; `-` means that the pointer did not exist or does not exist any more.
- A branch and the working-copy commit have a commit ID or `-`. HEAD has `branch:<name>` or
  `commit:<id>` or `-`.
- The file ends with a newline and has no message and no further lines.
- Version 2 of the encoding is written only for a stub and for the entry of a purge (ADR-014,
  STEMMA-47). A stub has the header, `number`, `time`, `actor`, and the line `stub`. A `purge`
  entry has the header, `number`, `time`, `actor`, `operation purge`, `commits <n>`, and
  `stubs <ranges>` (`4-5 7`, or `-`), and no pointer. A change cannot have the operation `purge`,
  so every entry still has one encoding. A stub that no `purge` entry lists is an error of the
  log, like a gap.

## Consequences

- Every operation that changes a pointer goes through a function of `Repository`
  (`update_branch`, `set_head`), which orders "check, write the entry, flush, move" under the lock.
  An operation that moves several pointers in one entry needs a function of its own: `commit`
  moves a branch and the working-copy commit under one lock and writes one entry
  (`Repository::commit`, STEMMA-3). The stores alone must not move pointers in production code.
- The log grows with every change, and a snapshot at every command makes many entries. Nothing
  removes them in phase 1; the open items name the measure that decides about retention.
- The log keeps commits that a rewrite or a purge would otherwise drop. A secret in a tracked file
  is therefore in the log until it is purged (ADR-014, STEMMA-26), which is why the log is local.
- A hub never sees the log. What a hub keeps is its own record of ref updates (ADR-003).
- `Repository::init` creates `oplog/` and writes entry 1, so the order "entry, then move" holds
  from the first pointer on. Entry 1 moves HEAD and the working-copy commit (ADR-010).

## Open items

- [x] A function that reads the outcome of an entry from the chain of its pointer
      (`oplog::outcomes`, STEMMA-34); `stemma op log` marks an entry that did not take effect
      (`!`, `took_effect: false`) or whose outcome is not known (`?`, `null`). The working-copy commit has its
      place in the references since STEMMA-22, and `oplog::Current::with_working_copy` hands its
      value to `outcomes`.
- [x] Every `append` lists the directory to find the end of the log. Measured in STEMMA-37: about
      2 microseconds for each entry, 17 ms more at 10,000 entries than at 1,000, so it does not
      matter. The tip file does not replace the listing; the writer still takes the largest number
      from the directory.
- [ ] `Moved` can be built with an old and a new value that are equal, and `Change::new` rejects
      that; a fallible `Moved::new` would keep the rule in one place. `Change::new` takes two
      adjacent optional pointers, which gets harder to read with every new kind of pointer; a
      struct of pointers, a context of operation, actor and authorizer, and a general `apply` that
      moves any set of pointers in one entry are open. `commit` has a function for its two
      pointers (a branch and the working-copy commit); a third kind of operation, such as a
      merge, decides the general form.
- [ ] `write_replacing` reports a failure to flush the directory as `NotFlushed` only if the
      directory cannot be opened for the flush (`StoreError::Io`); any other error of the flush
      after the rename becomes `RefError::Store`, so a move that was made is reported as one that
      was not. It is older than `commit` and applies to every move.
- [ ] If both moves of a `commit` are not flushed, the error names the first directory only, and
      if the second move fails after the first was not flushed, `PartlyMoved` does not carry the
      flush error of the first. The caller may not repeat the change in either case.
- [ ] The type of the number of an entry (a newtype with its file name and its limit in one
      place) and a read-only trait for the references that `RefsView` and the store share.
- [ ] Retention: whether many snapshot entries of one working-copy change are merged after some
      time, and what a garbage collection may remove (nothing before ADR-007's collection
      exists). Decide with a measurement of a real session.
- [x] The stub that replaces an entry when a secret is purged, who may purge, and how the purge is
      audited without keeping the secret. Decided in ADR-014 (STEMMA-26) and built: the plan
      (STEMMA-46), the stub with the `purge` entry in the log (STEMMA-47), and the purge itself
      (STEMMA-48).
- [x] A recovery when the highest number is missing: the tip file (STEMMA-30). What remains is
      a command that repairs a log with a lost last entry (the tip has to be set back by hand
      today), and the hash chain (STEMMA-41).
- [x] A cleanup of `tmp/`: names that stay after a failed or interrupted write, of the log and of
      the references. The writer that takes the lock removes them (STEMMA-31).
- [ ] Restoring: what `stemma op restore` does when the working directory has changed since, and
      the entry that the restore writes (STEMMA-28). Decided in ADR-013 and implemented (STEMMA-28).
- [ ] Who may read the log (ADR-003): it holds IDs and the actor, no paths, but it makes
      commits reachable that a role may not be allowed to see.
- [x] A reading order that does not read every file: the newest first, and a limit
      (`stemma op log`, STEMMA-27). It lists all file names, which finds a gap, and reads only
      the files of the entries that are shown.
- [ ] The `session` of an agent is part of its actor; whether the log offers "what did this
      session do" as a view.
- [ ] Windows: the flush of a directory is a no-op there, so the order "entry, then move" holds
      for atomicity but not for durability (ADR-007).
