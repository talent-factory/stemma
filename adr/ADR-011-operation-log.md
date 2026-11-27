# ADR-011: The operation log

**Status:** Accepted. The decisions marked *(confirmed)* were agreed with the author on
2026-10-19, and the rest on 2026-10-21; the hash chain (STEMMA-41) and the cleanup of `tmp/`
(STEMMA-31) on 2026-10-22; the limit of the outcome (STEMMA-32) and the retention of snapshot
entries (STEMMA-39) on 2026-10-23. All are implemented; for the retention that means that
phase 1 keeps every entry, and its rules for later are not built. The open items at the end are
questions for later (retention together with the garbage collection, who may read the log, the
width of the number) and none of them changes a decision. The flush of directories on Windows
was decided on 2026-10-24 (STEMMA-84). The anchor
that the actor cannot change, the statement about what a hub sees, and the entries that name a
signed statement were decided for phase 5 on 2026-11-23 and 2026-11-24 (STEMMA-273, STEMMA-274,
STEMMA-56; see "The log and a hub"). The entries that name a signed statement are implemented
(STEMMA-290, see "The entries that name a signed statement"), and so is the entry `fetch` with
its remote references (STEMMA-295); the rest is not yet.

## Context

Four decisions already fix the frame of the log:

- It is local. It is not an object of the repository and is not exchanged with the history, so a
  secret that was committed by mistake can be removed, and local paths and discarded attempts stay
  private (ADR-005).
- Every entry records the actor (ADR-002).
- It keeps every commit that an operation made unreachable, above all the working-copy commit that
  a snapshot replaces (ADR-010). Without the log the guarantee "no state of an agent is lost" is
  empty, so no snapshot is made before the log exists. This holds for every commit in phase 1;
  a later retention gives up the oldest snapshot states (see "Retention").
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
  possibly unfinished. Ctrl-C and a termination signal remove the lock before the process ends.
  The merge gate (decided in STEMMA-229, implemented in STEMMA-241) writes its objects before
  the lock and
  checks under the lock that its commit is in the store; if a purge removed it, the gate starts
  again (ADR-019, STEMMA-229).
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
  last entry that moves it unknown. A lock that a crashed writer left makes the newest entries
  unknown until the next writer of the same host takes it over (ADR-008, STEMMA-51)
  or someone removes it. A separate marker can come with a later version of the entry
  encoding if a use appears; version 2 went to the stub and the `purge` entry (ADR-014). The
  `purge` entry has an optional line `tags <n>` after `commits <n>`, the number of tags that the
  purge moved (STEMMA-185, ADR-014); it is left out for 0, so an entry from before keeps its
  encoding and its hash, and it is covered by the hash of the body.
  The reading has two limits, and both are accepted (STEMMA-32). A pointer that returns to an
  earlier value inside the log is decided correctly as long as nothing outside the log moves it,
  because each entry reads its old value under the lock. A change outside the log to one of the
  two values of an entry cannot be told from a move of the log. A pointer set back to the old
  value of an entry that took effect looks like a failed move, so the entry is said not to have
  taken effect (for the newest entry of a pointer only while no writer is at work; otherwise it is
  unknown). A pointer set to the new value of an entry that failed looks like a move that
  happened, so the entry is said to have taken effect. A marker written after the move would not
  remove both: when it survives, it settles the first limit, but an entry without one may have
  failed or have lost its marker, so the second would turn into an unknown outcome rather than a
  decided one. This decision leaves the marker out (see above), and a later encoding may add it.
- **Only a change writes an entry** *(confirmed)*. A snapshot of an unchanged directory creates no
  commit and no entry, so a command that only reads does not grow the log. `init` writes the first
  entry (operation `init`, HEAD from nothing to the initial branch).
- **The change of the owner is an entry without a pointer** *(confirmed; STEMMA-162)*. The
  operation `take-over` makes its actor the owner of the working copy (ADR-010). The owner is
  not a pointer, so its entry has no line after the operation, and the actor of the entry is
  the new owner. It is the only change that may name no pointer, and it may name none: the
  constructor and the reader refuse `take-over` with a pointer and any other operation without
  one, so every entry keeps one encoding. The entry is in the chain like any other, and the
  hash of its body covers the line of the operation. With no pointer to judge, `outcomes`
  judges the newest such entry by the owner that the file `working-copy` names now: its actor
  confirms it, another owner refutes it when no writer is at work, and a caller that does not
  give the owner leaves it unknown. An older one is unknown, as the owner of today comes from a
  later take-over. A repository of format 5 has it
  (ADR-009), so a program that cannot read it refuses the repository before it reads the log.
  The alternative, a line `owner` with the old and the new actor, was explicit, but it needed
  two lines (an actor has spaces) and changes to the reading of the outcomes and to the
  restore, for a value that no restore puts back.
- **An entry of a linked worktree names it** *(confirmed; STEMMA-176)*. The log is one log for
  all worktrees, but `HEAD`, the working-copy commit, and the owner belong to a worktree
  (ADR-009). A change made in a linked worktree has the line `worktree <name>` right after the
  line of the operation, with the name of its state directory; a change made in the main
  worktree has no such line, so every log from before linked worktrees reads and hashes as it
  did. `main` is never written, and the reader refuses it, so an entry has one encoding. The
  entries of `worktree add` and `worktree remove` name the worktree that they make or remove,
  and a line `location <path>` follows, with the absolute path of its directory on one line
  (a path with a control character or an empty path is refused). The name
  holds ASCII letters, digits, `-`, `_`, and `.` (not first) and is at most 255 bytes long. A program that does not know the line stops at
  it: the reader refuses an unknown line, so an older program refuses the log and does not read
  it wrongly. The hash of the body covers both lines.
  The entry of `worktree remove --take-over` (STEMMA-217, ADR-010) has a third line,
  `former-owner <actor>`, right after `location`: the owner of the working copy whose state a
  human removed after its directory was gone. The actor of the entry is that human, the owner
  does not change, and no entry of `take-over` is written, as the working copy ends with the
  call. Only an entry of `worktree-remove` with a `location` has the line; the reader refuses it
  elsewhere, refuses a line that is no actor, and, as the writer does, refuses an agent as the
  actor of the entry and the actor as its own former owner. The hash covers it, and `op log` shows it (text and the field
  `former_owner` of `--json`). It is version 13 of the repository format, so an older program
  refuses the log instead of reporting damage.
  `outcomes` judges the pointers by the worktree. The next move of `HEAD` and of the
  working-copy commit that confirms or refutes an entry is the next one *of the same worktree*,
  and the value now (`Current`, which `in_worktree` names, `main` by default) is compared
  only with the entries of its own worktree: the `HEAD` of the entry of another worktree is
  not known in the call, so the newest entry of it is unknown. The branches and the tags are
  shared and are judged as before. `take-over` is judged in its own worktree only.
- **Nothing is deleted** *(confirmed)*. No operation removes an entry, except the purge that
  replaces entries by stubs (ADR-014) and, later, a retention that does the same, and no garbage
  collection runs in phase 1 (ADR-007). Removing a secret replaces the entries that name it by a
  stub that keeps the number (ADR-014, STEMMA-26), so the numbering has no gap. How long entries
  are kept is decided below (retention).
- **Retention: phase 1 keeps every snapshot entry and every object** *(STEMMA-39)*. A snapshot
  at every command makes many entries, and each names the working-copy commit that it replaced.
  The decision rests on a measurement and fixes the rules that a later retention and the garbage
  collection of ADR-007 have to follow.
  - **The measurement.** No real session of an agent exists yet, so one was replayed with
    `scripts/measure-retention.sh`: the 110 commits of the first-parent history of this
    repository up to `11f06a4` (2026-10-18 to 2026-10-23, six days; release build, macOS 27 with
    APFS on an SSD, one run each). Sizes are apparent sizes, the sum of the lengths of the files.
    In the fine replay each changed file was written alone and followed by one command (`track`
    for a new file, `status` otherwise), as an agent that runs a command after every edit; the
    coarse replay ran `track .` once per commit. Neither ran `commit`.
    - The fine replay wrote 600 entries (468 `snapshot`, 131 `track`, one `init`) with 208 KB in
      `oplog/`, about 350 bytes per entry, and 17.24 MB of objects in 3,004 files: 15.96 MB of
      blobs, 1.16 MB of trees, and 0.12 MB of commits.
    - A repository with only the final state holds 2.13 MB in 146 objects, so the log keeps
      about 15.1 MB alive, some 25 KB per entry and about 2.5 MB per day. The entries themselves
      are about 1.4 % of that.
    - The coarse replay wrote 111 entries (38 KB) and 16.33 MB of objects, so its log keeps
      14.2 MB alive. One snapshot per command keeps about 6 % more than one per commit: the size
      comes from the versions of a few large files, not from the number of snapshots.
  - **Nothing is removed in phase 1.** About 2.5 MB per day is no reason to give up the guarantee
    that no state is lost (ADR-010). Removing an entry frees no byte while no garbage collection
    exists, as its objects stay in the store, so a retention alone would only lose information.
    Pack files with deltas (ADR-007) shrink the versions of one file without dropping a state,
    and are the first answer if the size matters. A later retention gives up the oldest snapshot
    states; the guarantee then holds for the states that it keeps.
  - **The roots of the garbage collection.** A later collection keeps every object that a
    reference reaches or that the old or the new value of an entry reaches
    (`Entry::commits`); a stub names no commit and is no root. The collection removes objects
    only, never an entry. It runs under the lock of the references, as every writer stores its
    objects before its entry and holds the lock from its first object (above, ADR-014), and it
    refuses while `.stemma/purge-pending` exists. This is the one statement of the rule;
    ADR-007 and the book point here.
  - **A stub alone frees little.** A working-copy commit is the new value of one entry and the
    old value of the next, so it stays a root until every entry that names it is a stub, or a
    reference reaches it. A retention therefore replaces a run of snapshot entries together, and
    it frees the commits inside the run, not those at its ends.
  - **A retention replaces an entry by a stub, it neither deletes nor merges.** A stub keeps the
    number and the hash of the entry (ADR-014, STEMMA-41), so the numbering has no gap and the
    chain holds; no checkpoint where the chain starts again is needed. Removing the oldest
    entries would need one, and merging the snapshot entries of one change would write entries
    again and break the chain.
  - **The price is higher than that of a purge.** A stub is a barrier: every entry before the
    newest stub has an unknown outcome (`?` in `op log`), and its restore needs `--force`
    (ADR-014). A purge is rare; a retention runs again and again, so its newest stub stays near
    the end of the log. After a retention the outcome of every entry older than the kept
    snapshots is unknown, the entries of `commit`, `track`, and `restore` included. They stay,
    but they lose their outcome. Whether a stub of a retention could keep which kind of pointer
    it moved (without an ID), so that the barrier holds for the working-copy commit only, is an
    open item.
  - **Only snapshot entries are candidates, and never the newest.** Every entry whose operation
    is not `snapshot` stays. An entry of the operation `snapshot` is a backup that the program
    made, also the snapshot that `op restore` takes before it moves (ADR-013); every other entry
    is an act of an actor. The name of an operation is the caller's choice
    (`Repository::update_working_copy`, `update_branch`, and `set_head` take any
    `OperationName`), so a caller must not name its own moves `snapshot`. At least the newest
    snapshot entry stays, as its old value is the state that an undo most likely needs. How many
    and how old the snapshot entries that stay are is not decided here, for want of a session of
    an agent to measure.
  - **Retention and the garbage collection come as one slice.** Neither is of use without the
    other. The retention writes an entry of its own, as the purge does: an operation `prune`
    without a pointer that lists the numbers of its stubs as ranges, so that a stub is still
    valid only if an entry lists it. It needs a new kind of record (`Record` is exhaustive), a
    rise of the repository format (ADR-009) so that an older reader refuses the log instead of
    calling it corrupt, as the purge brought, an extension of `check_stubs`, which accepts only
    the lists of a `purge` entry today, and the crash safety of the purge (a journal, resume,
    abort). It would be the second entry without a pointer, after the `purge` entry (ADR-014).
    This is a sketch, not a decision of its form.
- **The format is strict text** *(confirmed)*. A reader accepts what the writer writes and nothing
  else, as in ADR-005, so one entry has exactly one encoding. The sketch below is the format.
  The time is the one the writer claims, in seconds since the Unix epoch, written as in a commit
  (ADR-005); it is not a proof, and the order is the number, not the time.
- **The repository format version rises to 2 with the log** *(confirmed)*. `oplog/` is part of
  the layout (ADR-009), and `Repository::open` rejects version 1 with the existing
  `UnsupportedFormat`. No repository exists outside of tests and the author's trials, so the
  migration is to run `init` again. Version 3 came with the purge (ADR-014), version 4 with
  the hash chain (below), version 8 with the lease entries, version 9 with the pull request
  entries, version 10 with the approval entry, version 11 with the check entry, version 12
  with the kind in the stub (below), version 13 with the line `former-owner` (below), and
  version 16 with the entries that name a signed statement (below; the versions 14 and 15
  changed no entry, ADR-009), and version 17 with the line `remote` (below).
- **Each entry names the hash of the entry before it** *(confirmed; STEMMA-41)*. Entry encoding
  `stemma-oplog 3` adds the line `previous <hash>` after the actor, or `previous -` for the entry
  that starts the chain. The writer writes this version for a change and a `purge` entry; a new stub has version 4
  (below), a stub that a purge replaces again keeps its version,
  only the stub of an entry from before the chain keeps version 2. The reader checks the chain: a
  change, a removal, an insertion, or a swap of entries breaks it at that place
  (`OpLogError::ChainBroken`, which names the hash it expected and the one it found).
  - **What the hash covers: an envelope.** The hash of an entry is BLAKE3 over the text
    `stemma-oplog-hash 1`, `number`, `time`, `actor`, `previous`, and `body <hash of the body>`.
    The hash of the body is BLAKE3 over `stemma-oplog-body 1` and the lines after `previous`:
    the operation and the pointers of a change, or the operation, the count, and the stubs of a
    purge. A stub keeps the hash of the body that it replaced (`stub <hash>`), so its hash is the
    hash of the entry, the next entry still links to it, and the tip stays valid after a purge;
    time and actor stay covered. Two other ways were weighed: a stub that keeps the hash of the
    whole entry leaves its time and actor free to change, and a chain that skips over listed
    stubs checks nothing there. That a `purge` entry lists the stub is checked as before
    (`check_stubs`). The hash of a body is a hash over commit IDs that depend on the secret; to
    guess the secret from it, one has to build the trees, the commits, and the entry again, which
    is harder than to guess it from the blob ID that the purge names on the command line.
  - **The tip names the hash too.** The tip file of version 2,
    `stemma-oplog-tip 2\nnumber <n>\nhash <hash>\n`, names the hash of the last entry, so a
    replaced last entry, which no later entry links to, is found (`OpLogError::TipMismatch`).
    A tip that is behind is compared with the entry it names. The writer checks the tip before
    it links a new entry, and refuses to take a replaced entry into the chain. The tip of
    version 1 is still read and names no hash; naming an entry in the chain it is refused
    (`OpLogError::BadTip`), as only a program from before the chain writes it. The readers and
    the writers accept a tip that is missing or more than one behind, which a stop or a failed
    write of the tip leaves; `stemma op verify` reports both when the last entry is in the chain
    (`OpLogError::TipMissing`, `OpLogError::TipBehind`), as the writers then compared nothing.
  - **The writer links under the lock and after a lost race.** The writer reads the entry
    before the number it tries and names its hash; after a lost race (the hard link of the
    publication fails) it builds the entry again for the next number, with the hash of the new
    entry before it. If the entry before cannot be read, the writer refuses and writes nothing
    (`OpLogError::Predecessor`): starting the chain again would hide the damage. It refuses as
    well if the entry before was written before the chain but follows an entry in the chain.
    The tip is compared once, before the first attempt; an entry that another writer linked in
    between is covered by the chain, as its writer compared the tip in turn.
  - **A log from before the chain.** The repository format rises to 4 with the first new
    entry: `Repository` raises it under the lock before it writes the entry, and the purge raises
    it as well (ADR-014), so a program that knows only version 3 refuses the repository instead
    of writing entries outside the chain. A format is never lowered; `init` writes version 4. The
    old entries (versions 1 and 2) stay in front, and the first new entry starts the chain; an
    entry of version 1 or 2 after the chain is refused (`OpLogError::UnchainedAfterChain`). The
    old entries are not covered; to write them again as old entries, one has to compute the whole
    chain after them again, which only an anchor finds.
  - **Where it is checked.** Reading the whole log (`FsOpLog::entries`, which `op restore` and
    the purge use) checks the whole chain and the tip. Reading a window (`FsOpLog::window`, for
    `stemma op log -n`) checks the window, the link of its first entry to the entry before it
    if that can be read, and the tip if it is in the window; `op log` warns if the link was not
    checked. The command `stemma op verify` checks the whole log and the tip strictly, and
    prints the anchor of the last entry, `<number>:<hash>`, if it is in the chain; with
    `--anchor` it checks that the entry of an anchor kept from earlier still has that hash.
  - **What it does not prove.** The chain shows that the entries agree with one another, not
    who wrote them: whoever can write `.stemma` can compute every later hash and the tip again,
    and is noticed only by an anchor kept outside the repository. A hub that confirms the tip
    is an anchor that the actor cannot change (ADR-026). Signatures of
    the actors do not stop cutting the tail.
- **A tip file names the last entry** *(confirmed; STEMMA-30)*. `.stemma/oplog-tip`
  holds the number of the last entry in the strict form `stemma-oplog-tip 1\nnumber <n>\n`, or,
  since the hash chain, `stemma-oplog-tip 2` with its hash as well (above). The number is not 0. The
  writer replaces it after the entry is in place and before the pointer moves, with a temporary
  file that is flushed and a rename. The flush of the directory is best effort: a tip that is not
  on disk after a crash is one behind or missing, and both are accepted.
  - The writer reads the tip before it takes a number. A tip *beyond* the largest number in the
    directory means that the last entries are lost, and the writer refuses
    (`OpLogError::TipAhead`) because it would give a number a second time to another move. The
    reader reports the same. The message names the file and says what to do (restore the lost
    entries, or remove the tip once the loss is understood), because no command repairs the log
    yet.
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
    the tip and the entry, stays unnoticed (a missing tip is accepted by design). The hash chain
    (above) finds a change in the middle, and only an anchor outside the repository finds a log
    whose chain was computed again (STEMMA-41).
- **The first slice** *(confirmed)*. The core writes and strictly reads entries, and `init` and
  every operation that changes a pointer write one. The commands that show and use the log are
  slices of their own: `stemma op log` (STEMMA-27), and restoring and undoing (STEMMA-28).
- **Only `Repository` writes the log and moves a pointer** *(confirmed)*.
  `Repository::update_branch` and `set_head` order "check, write the entry, flush, move" under the
  lock, `Repository::refs()` returns a view for reading, and `Repository::oplog()` can read the
  log. `FsOpLog::append` and `Entry::try_new` are visible inside the crate only, so through
  `Repository` no caller can move a pointer past the log, add an entry past the authorizer, invent
  an entry that was never in the log, or pair the references of one repository with the objects of
  another. This is a convention of the API and not a boundary: the files are plain files, and code
  that builds its own store on the same directory is not stopped. A refusal by the authorizer, a
  name that is in the way, a branch that is not where the caller saw it (seen under the lock), and
  a commit that is not in the store write no entry. A move to the value that a pointer has writes
  none either.
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
  writer of files in `tmp/` holds the lock of the references while it writes there: the commands
  that write from before their first object (ADR-014), the moves of `Repository`, and the purge
  with its resume and abort. The one writer without the lock, the receiving side of an object
  stream, writes into a directory of its own below `tmp/` and no file into `tmp/` itself (see
  "The log and a hub", STEMMA-289).
  Whoever takes the lock is therefore the only writer (but see the advisory lock
  below), and every file in `tmp/` is left over from a write that failed or was interrupted. Right
  after the lock is taken and no journal of a purge is found (or `--resume` or `--abort` took the
  lock over), every name in `tmp/` that is not a directory is removed, without following a link,
  and so is every directory next to `.stemma` whose name is exactly one that `init` gives its
  temporary directory, `.stemma-init-<pid>-<n>-<16 lowercase hexadecimal digits>` (ADR-009). A
  directory of the working copy whose name only starts like it, such as `.stemma-init-notes`, is
  content and stays. A change that the journal of a purge refuses removes nothing; the resume or
  the abort does it. The cleanup is best effort, like the removal after the link: a name that
  cannot be removed stays, and the next writer tries again. The lock decides and not the age of a
  file, so a slow writer, such as the first `track` of many files, is never cut short, and there
  is no command to remember. Two other ways were weighed: a cleanup in `open` or a garbage
  collection after an age, which is a guess, and the process ID in the name, which is reused and
  cannot be checked without platform code. The lock is advisory: a person who removes the lock of
  a running process by hand lets the next writer remove its temporary file, and that writer then
  fails at its rename or link without damage. The same holds for `purge --resume` or `--abort`
  while the purge still runs in another terminal, which ADR-014 already warns against: the
  take-over removes the scratch of the running purge, and that purge fails and can be resumed. A
  caller of the library that stores objects through `Repository::objects()` without the lock is
  not protected either. A lock that stays after `kill -9` is taken over by the next writer of
  this host, whose guard is free then (ADR-008, STEMMA-51), and that writer removes the scratch
  as well.

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

- The first five lines, with the header, come in this order, each once. `operation` is a
  lowercase name of letters, digits, and `-`, starting with a letter.
- Then come the lines of the pointers that changed, at least one: `head`, then `working-copy`,
  then `branch <name>` for each branch, sorted by name in UTF-8 byte order, then `tag <name>` for
  each tag, sorted in the same way. Each has the old and the new value; `-` means that the
  pointer did not exist or does not exist any more.
- A branch, a tag, and the working-copy commit have a commit ID or `-`. HEAD has `branch:<name>`
  or `commit:<id>` or `-`.
- A tag never moves (ADR-008), so the line of the operation `tag` is `tag <name> - <commit>`,
  and the line of the operation `untag`, which deletes a tag (STEMMA-184), is
  `tag <name> <commit> -`: the commit that the tag had, and `-` for the tag that is gone. The
  writers write no other form, and `tag` and `untag` are the only operations that write a tag
  line. The line is part of encodings 1 and 3 and needs no new version: an entry without a tag
  line reads as before, a reader from before the operation `tag` rejects the line as an unknown
  pointer, which is right, since it cannot say what the entry did, and a reader that knows the
  line reads both forms. A tag and a branch may have the same name (ADR-008), so the two lists
  are separate, and a name is unique within each.
  The operations `tag` and `untag` are level 2 (ADR-004). Who set or deleted a tag and when is
  the actor and the time of its entry. The outcome of a `tag` entry is read from the tag as it
  is now: the tag exists with the commit of the entry (took effect), or it does not exist and no
  writer is at work (did not). The outcome of an `untag` entry is read the same way: the tag
  does not exist (took effect), or it exists at the commit of the entry and no writer is at work
  (did not); at any other commit the outcome is not known, and the entries that follow decide.
  A tag that was deleted and set again is judged by the entries that follow, as for a branch.
  A restore does not touch tags (ADR-013), so `op restore` refuses an entry that only made or
  deleted tags.
- The file ends with a newline and has no message and no further lines.
- Version 2 of the encoding is written only for a stub and for the entry of a purge (ADR-014,
  STEMMA-47). A stub has the header, `number`, `time`, `actor`, and the line `stub`. A `purge`
  entry has the header, `number`, `time`, `actor`, `operation purge`, `commits <n>`, and
  `stubs <ranges>` (`4-5 7`, or `-`), and no pointer. A change cannot have the operation `purge`,
  so every entry still has one encoding. A stub that no `purge` entry lists is an error of the
  log, like a gap.
- Version 3 is the encoding of the hash chain (STEMMA-41), and the only one that the writer
  writes since, except for the stub of an entry from before the chain, which stays version 2.
  Version 3 has the line `previous <hash>` or `previous -` after `actor`, and then the
  lines of a change, `stub <hash of the body>`, or the lines of a `purge` entry:

  ```text
  stemma-oplog 3
  number 42
  time 1700000000
  actor human daniel
  previous b3:7f0c…91aa
  operation snapshot
  working-copy b3:1d71…eed9 b3:4feb…b32c
  ```

- **The stub names the kind of the entry it replaced** *(confirmed; STEMMA-256)*. Entry encoding
  `stemma-oplog 4` is version 3 for a stub with one more word on the line of the hash:
  `stub <hash of the body> approval` or `stub <hash of the body> other`. The writer writes it for
  every new stub of an entry in the chain (a stub that a purge replaces again keeps its
  version), and the repository format version rises to 12 with it. The
  hash of the entry is the same as without the word, so the chain does not change. A stub of
  version 3 has no kind and is read as before; its proof is not checked, as the proof of a stub
  of an approval is not. The kind is not covered by the hash of the entry: whoever can rewrite
  the log files can mark any stub as an approval, and so give it an unchecked proof. The proof
  adds nothing there that the files do not already allow; the anchor (`op verify --anchor`)
  covers the chain, not the kind.

- **The four lease entries** *(confirmed; STEMMA-225, STEMMA-237)*. The lease of a branch is
  state in a file (ADR-004); the log records its changes in four entries that name a branch and
  actors and no commit: `lease-acquire`, `lease-release`, `lease-takeover`, and `lease-override`.
  They are the third kind of entry without a pointer, after `take-over` and `purge`, so the
  rule "at least one pointer line" has three exceptions now, and no purge can list them.
  - **The encoding stays `stemma-oplog 3`.** STEMMA-225 named a new encoding `stemma-oplog 3`,
    but that number is taken by the hash chain (above). A lease entry is always in the chain, so
    it has no older encoding, and its lines follow `previous`, as the lines `worktree` and `tags`
    came to version 3 without a new header. What an older reader must not read is kept out by the
    version of the repository format, which rises to 8 with the next entry or purge, the first lease
    entry among them (ADR-009): a
    program that knows only version 7 refuses the repository instead of calling the entry
    damaged. The older headers refuse a lease body as well (`stemma-oplog 1` and `2` are read
    only for the entries that they were written for).
  - **The lines.** After `operation lease-<kind>` come `branch <name>`, then `from <actor>` (a
    takeover and an override: the earlier holder), `expires <seconds>` (all but a release), and
    `reason <text>` (a takeover: `expired`; an override: one line of 1 to 1,024 bytes without a
    control character and without a space at its ends). Any other order, a missing line, or an
    extra line is an error, so one entry has one encoding. The actor of the entry is the one who
    acts: the new holder, the holder who releases, or the owner who overrides. A takeover or an
    override that names the actor as the earlier holder is invalid (the earlier holder acquires
    again), and `expires` is after `time`.
  - **State and entry.** The file `leases/<branch>` (the name with every byte outside
    lowercase letters, digits, `-`, and `_` as `%` and two hex digits; a name longer than 200
    bytes is cut and ends in `~` and 16 hex digits of its BLAKE3 hash) holds `stemma-lease 1`, the branch, the
    holder, the time of the acquisition, and the expiry. A writer that holds the lock of the
    references decides from the file, writes the entry first, and then replaces the file with
    a rename, as the entry of a change comes before its move. A crash in between leaves the old
    file, whole, and an entry for a lease that did not take effect; the next claim reads the file
    again and decides again. A renewal replaces the file and writes no entry. `op restore`
    refuses a lease entry, as it moved no pointer, and `outcomes` passes over it like a purge.

- **The three pull request entries** *(confirmed; STEMMA-226, STEMMA-238)*. A pull request has
  no object and no reference (ADR-004 rule 3). In the formats 9 to 15 it is the sequence of the
  entries `pr-open`, `pr-close`, and `pr-merge`, named by the number of its `pr-open` entry, and
  they move no pointer, like the lease entries. The state is derived and not stored: `open`,
  `closed`, or `merged`. Since format 16 the evidence is signed statements, and the state is
  folded from them and from the entries `pr-merge` (see "The entries that name a signed
  statement"); the entries, their lines, and the rules of this item stay. "Approved" is no
  state of the log; it follows from the approvals on the current head (below). The prototype of STEMMA-226
  compared an object chain, one mutable reference, and entries of the log. Entries won, since the
  log is already the history, the hash chain, and the signature; the other two would have needed
  a new object kind or a new reference namespace, and a store that a purge rewrites.
  - **The encoding stays `stemma-oplog 3`, the format rises to 9.** As for the lease entries, an
    entry is always in the chain and has no older encoding. A program that knows only version 8
    refuses the repository instead of calling the entry damaged (ADR-009).
  - **The lines.** An opening has `operation pr-open`, `branch <name>`, `commit <id>` (the tip of
    the branch), then `reviewer <actor>` once for each reviewer in ascending order of their text
    (at most 32, none the opener, none twice), and optionally `repairs <id>`, the red commit that
    the pull request declares to repair (ADR-019). A closing has `operation pr-close` and
    `pull <number>`. A merge has `operation pr-merge`, `pull <number>`, and `commit <id>`, the
    head that was merged. Any other order, a missing line, or an extra line is an error. The
    target is always `main`, so no line names it.
  - **The rules of the writer.** They are checked under the lock of the references
    (`Repository::open_pull`, `close_pull`, `record_pull_merge`). Only the write owner opens: where
    the branch has a lease, the actor holds a valid one. A branch has at most one open pull
    request, `main` is never the branch, and the opener names the reviewers from the register
    (none in open mode). A pull request is closed by its opener or an owner, and an ended pull
    request is final: the next change of the branch is a new pull request with a new number. The
    entry of a merge records the merge; the gate that moves `main` writes it (ADR-019,
    `Repository::merge_gate`) under the same lock as the move, after the entry `gate-merge` that
    names the move and after the move itself, and it checks no approval itself. The log takes one
    entry per append, so the two entries cannot be one step. The order keeps the dangerous state
    out: `pr-merge` never stands without the change on `main`. The state that is left is a pull
    request that is open while `main` holds its change (a crash or a failed write between the
    move and `pr-merge`). The next call of the gate repairs it: it finds that merging the branch
    onto the tip changes nothing, and records `pr-merge` under the lock without a move. An entry
    `gate-merge` whose move failed stays in the log as an intent, like any entry of ADR-011. The
    name `gate-merge` is not reserved: any writer can make such an entry through
    `update_branch`, so no reader takes it as proof that the gate moved `main`. Likewise "merged"
    in the log is no proof that `main` holds the change: the log is no evidence for the content
    of `main`, and later slices (approvals, repairs) must not take it as such. A move that is not
    known to be flushed writes no `pr-merge` either; the repair path records it later.
  - **The reader is tolerant, the writer is not.** After a purge the log can hold a closing whose
    opening is a stub. The fold skips an entry about a pull request that it does not know and an
    entry after the end of one.
  - **Purge.** An opening and a merge name commits, so a purge may replace them with stubs, as
    any entry that names a rewritten commit. A closing names none. The purge refuses to replace
    the opening of a pull request that is still open (`PurgeError::OpenPullRequest`), because the
    pull request would vanish from the log. It replaces the opening of a closed or merged one, and
    it replaces the opening of a pull request together with its `pr-merge`, so that the pull
    request is not open again. `op restore` refuses a pull request entry, and
    `outcomes` passes over it.
- **The check entry** *(confirmed; STEMMA-240)*. A check run is evidence about a pair of commits
  (ADR-019): the commit that was tested and the tip of `main` it was merged onto for the test, its
  base. It is an entry of the log, like the pull request entries, and for the same reason: the log
  is already the history, the hash chain, and the place of the signature. In the formats 11 to
  15 a proof of the writer is the proof of the entry (ADR-003), so a check needs no format of
  its own for it. Since format 16 the result is a signed statement that the entry names, and
  the signature of the statement is the proof (see "The entries that name a signed
  statement"); what this item says about the proof file holds for the entries of version 1.
  - **The encoding stays `stemma-oplog 3`, the format rises to 11.** The entry is always in the
    chain and has no older encoding. A program that knows only version 10 refuses the repository
    instead of calling the entry damaged (ADR-009).
  - **The lines.** `operation check`, `version 1`, `commit <id>`, `base <id>` or `base -`, and
    `result pass` or `result fail`, in this order. The line `version` gives the entry a version;
    the reader accepts version 1 only. `base -` is a run on the tip of `main` itself, the run
    after a merge (ADR-019). One entry holds one base: a result for a pair of commits is the
    entry with that `commit` and that `base`, and a run against another tip is another entry.
    The target is always `main`, so no line names it.
  - **The writer asks no more than the authorizer.** `Repository::record_check` asks the
    authorizer for `check`, takes the lock of the references, refuses a commit that the store
    does not hold as a commit and a base that is the commit itself, and writes the entry. It
    does not judge the writer: who counts depends on the pull request, its authors, and its risk
    level, which the entry does not hold, so the rule is applied when the evidence is read, in
    `check::counts` and `Repository::counted_checks` (ADR-019, "Who counts as independent").
    Any actor that the authorizer allows can therefore write a result through the core (no
    command does yet), and it counts
    only for some changes; a check that the opener wrote stays in the log as what it is. The
    sponsor that the rule uses is the one of the register where the actor is registered, and
    otherwise the claim of the actor line, so an actor that is not registered can claim a
    sponsor; every level asks for a registered writer where a register exists.
  - **The proof.** Where the repository is enforcing, the writer signs the hash of the entry with
    a key of the register and writes the proof file, as for the entries whose rule depends on who
    acts (ADR-003). `counted_checks` counts a result only with a proof that the register accepts
    for the number of the entry, and `Repository::last_green_tip` reads the last green tip by the
    same rule: the newest commit in the first-parent history of the tip of `main` that a passing run
    without a base covered, where a run on another commit says nothing about `main`.
    A missing or malformed proof does not count. A failed run never counts towards a merge, but
    it stays in the log, where the gate finds the first red tip. `stemma op verify` checks the
    proof files that exist, and a missing proof only means that the result does not count. In
    the open mode the actor is a claim and no proof exists.
  - **Purge and restore.** The entry names commits, so a purge may replace it with a stub, as any
    entry that names a rewritten commit; the evidence for a rewritten commit is then gone, and the
    change is checked again. `op restore` refuses it, and `outcomes` passes over it.

- **The approval entry `pr-approve`** *(confirmed; STEMMA-227, STEMMA-239)*. An approval is the
  fourth entry of a pull request. It moves no pointer and changes no state of the fold:
  "approved" stays a count over the log and not a state of it.
  - **The encoding stays `stemma-oplog 3`, the format rises to 10**, for the reason of the
    entries before it: a program that knows only version 9 refuses the repository instead of
    calling the entry damaged (ADR-009).
  - **The lines.** `operation pr-approve`, `pull <number>`, and `commit <id>`, the head that the
    reviewer reviewed. Any other line is an error. The reviewer is the actor of the entry.
  - **The signature is not a line.** This is the approval of the formats 10 to 15; since
    format 16 the signature is part of the statement that the entry names, and an approval has
    no proof file (see "The entries that name a signed statement"). Where the repository has a
    register, the proof is the file
    `proofs/<number of the entry>` (ADR-003). It signs, as the type `approval`, the bytes
    `pull <number>\ncommit <id>\n`: the pull request and the head, not the hash of the entry.
    Binding it to the head is what lets a new commit void it (ADR-004 rule 4), and the number
    keeps it from standing for another pull request of the same head. A revoked key voids the
    approval, a retired one only for approvals written before its retirement, as the register says
    (ADR-003). The signature is checked before the state of the key, so a key that no longer counts
    cannot hide a forged proof. `op verify` checks the
    proof as it checks any other, and a void approval is no damage of the log.
  - **The rules of the writer** (`Repository::approve_pull`), checked under the lock of the
    references: the pull request is open; the branch still has the head that the reviewer signed
    (the signature is made before the lock, since a key may ask for a passphrase, so the head is
    read again under it); the reviewer is neither the opener nor the author of a commit that the
    pull request brings, by identity (`allowed::self_approval`); and with a register, the
    signature is accepted before the entry is written and its key is active. A retired key keeps
    the approvals it made, but signs no new one. The entry comes before the proof. A crash
    in between leaves an approval without a proof, which does not count.
  - **The recount does not trust the writer** (`Repository::recount_approvals`,
    `pull::recount`). Only an open pull request is recounted: the authors are the commits that
    `main` lacks, and after a merge `main` has them all. An approval counts if it names the head
    of the branch now, its reviewer passes the ban on self-approval again, its
    proof holds against the register, and no earlier approval of the same identity counts. So an
    entry that was written by hand counts only if it passes the same questions. The recount
    names the reason for each approval that does not count. How many approvals a level needs is
    the policy (ADR-004), and the merge gate asks for it (ADR-019).
  - **Purge.** An approval names a commit, so a purge may replace it with a stub, like any
    entry that names a rewritten commit. It is no opening, so the purge guard for open pull
    requests does not apply. The proof of an approval signs the pull request and the head, which
    the stub no longer has, so `op verify` lets the proof of an approval stub pass even if its
    signature does not verify. The proof of any other entry signs the hash of the entry, which a
    stub keeps, so `op verify` checks it for the stub of any other entry (STEMMA-256; before, a
    stub of any entry passed). The stub says which of the two it is: encoding 4 above.

- **The entry `remote-pin`** *(confirmed; STEMMA-297)*. A human who pins the key of a hub again
  (ADR-026) writes an entry with the lines `operation remote-pin`, `remote <name>`,
  `old <key>` (or `old -` for a configuration that held no key) and `new <key>`, the keys in
  the OpenSSH form without a comment. The entry is written under the lock of the references
  before the configuration of the remote is replaced, as every entry comes before its move. It
  names no commit, so no purge lists it, and a restore refuses it, as it moved no pointer. It is
  always in the chain and keeps the encoding `stemma-oplog 3`. An older program would read it as
  a change without a pointer, which is damage, so the repository format rises to 18 with it
  (ADR-009). The same key as the pinned one writes no entry. Only a human pins, so the reader
  refuses an entry whose actor is an agent, as the writer does.

- **The entries that name a signed statement** *(decided in STEMMA-273; implemented in
  STEMMA-290)*. The evidence about a pull request is signed statements (ADR-025): its opening,
  an approval, the result of a check, and its closing. The log keeps recording what the clone
  did, and the entry of such an act names the statement that it made.
  - **The lines.** `pr-open`, `pr-close`, and `pr-approve` end with the line
    `statement <hash>`. `check` has the line `version 2` in place of `version 1` and ends with
    `statement <hash>`. The hash is that of the statement, in the form of an object ID
    (ADR-025). `pr-merge` names none: "merged" is no statement, and in a clone this entry is
    the fact. The other lines stay as they were, so the log reads without the set (`op log`),
    and a purge finds the commits that an entry names (ADR-014). The entry repeats what the
    statement says, with one difference of form: the reviewers of an entry are actors, and
    those of an opening are identities.
  - **The encoding stays `stemma-oplog 3`, the format rises to 16.** The reader accepts both
    forms: an entry of a pull request without the line, and a `check` of version 1, is an entry
    of an older format. The writer writes the new form only. A program that knows only version
    15 refuses the repository instead of calling the line damage (ADR-009).
  - **An entry of an older format is no evidence.** It stays an entry of the chain, `op log`
    shows it, and `op verify` checks its proof file as before. The fold reads statements only,
    so a pull request that was open under an older format is opened again, and a check is
    recorded again. Nothing is converted: a statement is signed by the key of its actor, and
    two forms of evidence side by side are what ADR-025 rules out.
  - **The entry places the statement.** A statement names no number, no time, and no actor. In a
    clone the entry that names it gives it the three: its own number and time, and its actor.
    The fold (`evidence::fold`, the function that a hub calls for its set) takes the statements
    in this order. An entry places its statement if the set holds it (ADR-009), the strict
    reader accepts it, it is of the kind of the entry, and it says what the lines of the entry
    say. `pull <number>` of an entry and the hash of the opening in a statement are joined by
    the entry `pr-open` with that number; where a purge made a stub of that entry, the number
    cannot be compared and is taken as it is. An entry that places nothing counts for nothing.
  - **The name of a pull request** is the hash of its signed opening, in every clone and on a
    hub. The number of its `pr-open` entry stays its name in this log: the lines `pull <number>`
    use it, and so do the commands, until the command surface decides how people name a pull
    request (ADR-025).
  - **The proof is the signature of the statement.** Where the repository has a register, the
    statement is signed with a key that the register holds for the actor as an active key; in
    the open mode it carries no key and no signature (ADR-025). These entries have no proof
    file. Whoever counts the evidence asks the register for the signature, with the actor and
    the number of the entry (`Register::verify_statement`): the recount of the approvals, the
    rule table of the checks, and the bounds of the gate (ADR-019). A key counts as for the
    proofs before: a retired key for what stands before its retirement, a revoked key never
    for an approval and for every other kind only before its revocation. An opening
    is asked too, before it is placed: in an enforcing repository an opening without a
    signature, or with the signature of a key that is not the opener's or no longer counts for
    the number of the entry, makes no pull request. A pull request that was opened while the
    repository was in the open mode is therefore opened again. A closing is placed
    whatever its signature, because it ends a pull request; one whose signature the register
    does not accept is a stand-in (below). `op verify` reports a signature that the register
    does not accept, for every kind, and counts the unsigned ones.
  - **What speaks against a change stays, and does no more.** An entry whose statement is
    missing, damaged, or says something else places nothing, with one exception: a closing
    and a failing run are placed from the lines of their entry, as a stand-in without a
    signature (`Placed::stand_in`). A closing whose signature the register does not accept is
    a stand-in as well. Losing a file of the set therefore never opens a pull request again
    and never takes a red tip away, in either mode. Anyone who writes the log can make a
    stand-in, under any actor, so in an enforcing repository it does only what makes things
    stricter (CODING_STANDARDS, "A filter on who wrote a result"):
    - a closing ends its pull request, and the gate keeps the reservation of that pull
      request (`PullRequest::ended_without_proof`);
    - a failing run makes its commit red and pauses a change of level 2 or 3, and nothing
      else: it moves no verified tip, ends no row of invalidated runs, and opens no red
      window for a declared repair (ADR-019). A repair of such a commit waits until the
      statement is restored or a signed run is written.

    A failing run that was made without a signature is no stand-in and does not count there.
    In the open mode, where every statement is a claim, a stand-in counts as any other.
    `op verify` reports the entry of every stand-in.
  - **The order of writing.** The statement is signed before the lock of the references is
    taken, because a key with a passphrase may ask a person, and the register is asked for the
    signature before anything is written: every refusal comes before the first write. Under
    the lock the rules are checked again, the statement is put into the set (a temporary file,
    a flush, a rename), and then the entry is written. A stop in between leaves a statement
    that no entry names, which counts for nothing, and the call can be made again. The state
    "an approval without its proof", which the formats 10 to 15 had, does not exist any more.
  - **An ended pull request is final** (ADR-025): the writer refuses an opening that the fold
    knows already, and the fold skips one that is placed a second time. The next pull request
    of a branch needs a new head, other reviewers, or another repair. The writer asks the
    log for it and not only the fold, so an opening whose statement was lost is refused as
    well.
  - **`op verify` checks the statements and the entries that name them.** Every file of the
    set is a statement under the name of its hash, with a signature that verifies for the key
    that it names. Every entry that names a statement names one that the set holds, of its
    kind, that says what its lines say. In an enforcing repository the signature of that
    statement is one of a key of the actor of the entry that counts for the number of the
    entry. Three things are no damage: a statement that no entry names, an unsigned statement
    in an enforcing repository (an opening, an approval, and a check result do not count there), and an
    approval of a key that was retired
    or revoked since (it is void, which the recount shows).
  - **Purge.** The entries are replaced by stubs as before (above). The purge does not touch
    the set: the statement of a stub stays as a statement that no entry names. A closing names
    no commit and stays; that its opening is a stub is no damage. What a purge does with such
    statements is an open item of ADR-025. The purge asks by the hash of the opening whether
    a pull request is open, not by the number of an entry. It also refuses to replace an
    entry `pr-open` whose opening the fold does not know, because its statement is missing,
    damaged, or says something else (`PurgeError::UnreadableOpening`, exit code 5): whether
    that pull request is open cannot be told, and a stub would hide the damage from
    `op verify` for good. The statement is restored first.
  - **Not built.** The `fetch` entry that names the statements received, and an entry
    `lease-override` that names the statement of an owner override, whose format exists
    (ADR-025). They come with their slices.

- **The log and a hub** *(decided for phase 5 in STEMMA-56, STEMMA-273 and STEMMA-274; the
  entries that name a statement are implemented, see above, and so are the ID of the log, the
  anchor, its confirmation, `op verify` against the anchors and the bound of the register in its
  log (STEMMA-300); the rest is not yet)*.
  - **A hub never sees an entry of the log.** The log can hold a secret until it is purged
    (ADR-014), and a purge would not reach a copy. What reaches a hub is the anchor of the tip
    alone: the number and the hash of the last entry, signed by the actor, which the hub confirms
    with its own key (ADR-026). `op verify` checks the log against the anchors that the hub holds.
    In the core that check is `Repository::verify_anchors` *(STEMMA-300)*: every held anchor
    must be signed by the pinned key and name this log and the repository, and every confirmed
    one must name an entry that the log holds with that hash. A confirmed entry past the end of
    the log is a cut tail, one with another hash or outside the chain a rewrite. The
    contradicting anchors are returned, not refused. The command comes with its slice.
  - **A log has an ID.** A random ID in `.stemma` names the log (ADR-009), because one actor can
    have two clones with the same key and several actors write into one log. The hub keeps
    anchors per repository and log ID. It is the file `log-id` *(STEMMA-300)*, which `init` and
    `clone` write and an older repository gets on its first use (`Repository::log_id`); every
    worktree shares it with the log.
  - **The anchor checks the log before it is signed** *(STEMMA-300)*. `Repository::anchor`
    names the last entry of the log and the last confirmation that the clone keeps for the
    remote (`anchors/<remote>`, ADR-009), and refuses to sign while the log no longer holds the
    entry of that confirmation with its hash, so a restored backup or a cut tail is found in the
    clone first. A log whose last entry is outside the chain has no anchor yet.
  - **The evidence of a pull request is signed statements; the log records the act.** The
    opening, an approval, a check result and a closing are signed statements that a clone and a
    hub fold with one function (ADR-025). The entries `pr-open`, `pr-approve` and `check` name
    the hash of the statement they made, and the fold of the gate reads from the statements. One
    form holds everywhere, also without a remote. This is a new version of the entries and of
    the repository format (ADR-009). A pull request is named by the hash of its opening, not by
    the number of its `pr-open` entry, so the name holds in every clone. Built in STEMMA-290;
    `pr-close` names its statement as well, since a closing is one.
  - **`fetch` and `push` are operations with one entry each.** A `fetch` entry names the remote
    references it moves and removes, with their old and new state, and the statements received
    (the statements come with their slice). A
    `push` entry is written after the receipt and names the remote references it moved and the
    number and hash of the receipt (ADR-008). `pr-merge` is written after the receipt as well,
    in a repository whose `main` follows a remote. Pinning the key of a hub again writes an
    entry (ADR-026).
  - **The bound of a retired or revoked key** is a number of the local log and counts only in the
    log that wrote it; the register names the ID of that log from now on. In every other log the
    bound is the `fetch` entry that brought the register commit, and across clones the order is
    the order of the hub (ADR-026). Built in STEMMA-300: the line of a bound reads
    `revoked 40 log <ID>` in version 2 of the register, which the command `register` writes
    with the ID of its own log. `Repository::register_for_log` gives the register as it counts
    in one log: a bound of another log becomes the number of the first `fetch` entry that
    brought a commit whose register shows the key retired or revoked in that log, or 1 if none
    did, so no entry of the log stands before it; `op verify` checks the proofs against it. A
    retirement that a later fetch turns into a revocation counts from the first fetch, and a
    fetched commit that the store no longer holds is damage. A bound of version 1, without a
    log, counts by its number in every log, as before.
  - **A receiving operation writes outside the lock**, the upload on a hub and the receiving
    side of a fetch alike (STEMMA-289). Its temporary files lie in a subdirectory of its own,
    `tmp/receive-<pid>-<n>-<random>/`, which the cleanup above leaves, because it removes files
    and not directories (ADR-007). The cleanup of such a directory is not the business of the
    lock holder: the receiving operation removes its directory at its end, and one that a
    crashed operation left is removed by the next receiving operation, which tells it from a
    directory in use by a lock of the system on the file `guard` in it (ADR-023; a directory
    without a guard goes only while it is empty). Here too a
    lock decides and not an age. The operation needs no lock of the references because it
    names nothing: its objects stay unnamed until a ref update or the move of a remote
    reference names them, and those moves hold the lock.
  - **The line `remote`** *(STEMMA-295)*. A change names the remote references after its tags,
    one line each, sorted by the remote, the branches before the tags, and the name, and each
    once: `remote remotes/<remote>/heads/<name> <old> <new>`, where a state is the commit and
    the hash of the record entry, two tokens, and `- -` stands for a reference that does not
    exist. A state with one token `-` is refused. The entry `fetch` writes the lines, and the
    restore of a `fetch` entry writes them into its own entry. The outcome of a line is judged
    as that of a branch, against the remote references of the clone (`Current::with_remotes`),
    and the commits of the lines are roots like those of a branch. The line came with version
    17 of the repository format (ADR-009): a program that knows only version 16 would call it
    damage (`UnknownPointer`), so the repository rises with its next entry.
  - **A purge leaves a confirmed anchor valid**, because a stub keeps the hash of the entry it
    replaces. The purge sends nothing; its entry is anchored with the next push or by the
    command (ADR-026).

## Consequences

- Every operation that changes a pointer goes through a function of `Repository`
  (`update_branch`, `set_head`), which orders "check, write the entry, flush, move" under the lock.
  An operation that moves several pointers in one entry needs a function of its own: `commit`
  moves a branch and the working-copy commit under one lock and writes one entry
  (`Repository::commit`, STEMMA-3). The stores alone must not move pointers in production code.
- The log grows with every change, and a snapshot at every command makes many entries. Nothing
  removes them in phase 1; the measurement under "Retention" (STEMMA-39) shows what they keep
  alive.
- The log keeps commits that a rewrite or a purge would otherwise drop. A secret in a tracked file
  is therefore in the log until it is purged (ADR-014, STEMMA-26), which is why the log is local.
- A hub never sees an entry of the log. It confirms the number and the hash of the tip
  (ADR-026). What a hub keeps is its own record of ref updates and the signed statements about a
  repository (ADR-025).
- `Repository::init` creates `oplog/` and writes entry 1, so the order "entry, then move" holds
  from the first pointer on. Entry 1 moves HEAD and the working-copy commit (ADR-010).

## Open items

- [x] A function that reads the outcome of an entry from the chain of its pointer
      (`oplog::outcomes`, STEMMA-34); `stemma op log` marks an entry that did not take effect (`!`,
      `took_effect: false`) or whose outcome is not known (`?`, `null`). The working-copy commit
      has its place in the references since STEMMA-22, and `oplog::Current::with_working_copy`
      hands its value to `outcomes`. STEMMA-32 added the tests of a failed entry in the middle and
      its retry, of the first entry, of a pointer that returns to an earlier value inside the log,
      of the two limits above, and of the barrier of a stub in a hash-chained log (encoding 3).
- [x] Every `append` lists the directory to find the end of the log. Measured in STEMMA-37: about
      2 microseconds for each entry, 17 ms more at 10,000 entries than at 1,000, so it does not
      matter. The tip file does not replace the listing; the writer still takes the largest number
      from the directory.
- [ ] `Moved` can be built with an old and a new value that are equal, and `Change::try_new` rejects
      that; a fallible `Moved::new` would keep the rule in one place. `Change::try_new` takes two
      adjacent optional pointers, which gets harder to read with every new kind of pointer; a
      struct of pointers, a context of operation, actor and authorizer, and a general `apply` that
      moves any set of pointers in one entry are open. `commit` has a function for its two
      pointers (a branch and the working-copy commit); a third kind of operation, such as a
      merge, decides the general form.
- [x] `write_replacing` reported a failure to flush the directory as `NotFlushed` only for a
      `StoreError::Io` and turned any other error into `RefError::Store`, so a move that was made
      could have been reported as one that was not. The flush gives only `StoreError::Io`, so
      the other branch was dead; the references now take the error of the file system
      (`flush_directory`), and every failed flush after a rename is `NotFlushed` (STEMMA-77).
- [ ] If both moves of a `commit` are not flushed, the error names the first directory only, and
      if the second move fails after the first was not flushed, `PartlyMoved` does not carry the
      flush error of the first. The caller may not repeat the change in either case.
- [ ] The type of the number of an entry (a newtype with its file name and its limit in one
      place) and a read-only trait for the references that `RefsView` and the store share.
- [x] Retention: whether many snapshot entries of one working-copy change are merged after some
      time, and what a garbage collection may remove. Decided with a replayed session
      (STEMMA-39, see "Retention" above): nothing in phase 1, no merging, and a later retention
      replaces snapshot entries by stubs.
- [ ] Retention together with the garbage collection of ADR-007, as one slice: how many and how
      old the snapshot entries are that stay, measured with a real session of an agent; the
      `prune` entry, its record, and the format it raises; and the journal of its crash safety.
- [ ] Whether a stub of a retention keeps which kind of pointer it moved (without an ID), so that
      the barrier of the outcome holds for the working-copy commit only and the entries of
      `commit`, `track`, and `restore` before it keep their outcome.
- [x] The stub that replaces an entry when a secret is purged, who may purge, and how the purge is
      audited without keeping the secret. Decided in ADR-014 (STEMMA-26) and built: the plan
      (STEMMA-46), the stub with the `purge` entry in the log (STEMMA-47), and the purge itself
      (STEMMA-48).
- [x] A recovery when the highest number is missing: the tip file (STEMMA-30). What remains is
      a command that repairs a log with a lost last entry (the tip has to be removed by hand
      today).
- [x] The hash chain (STEMMA-41): entry encoding 3, the hash over an envelope that a stub keeps,
      the tip with the hash, format 4, and `stemma op verify` with `--anchor`.
- [x] An anchor that the actor cannot change: a hub that confirms the tip. Decided in STEMMA-274
      (ADR-026, see "The log and a hub"); the core holds it since STEMMA-300, and the sending
      after a push and the commands come with their slices. Until then, the anchor is a copy
      that a person keeps. Entries from before the chain (versions 1 and 2) stay uncovered.
- [x] Retention (STEMMA-39) needs a checkpoint where the chain starts again, if entries before
      it are ever removed. Not needed: a retention replaces entries by stubs, which keep the
      hash, and removes none.
- [ ] A command that repairs a broken chain, or a log with a lost last entry; today it is
      restored from a backup of `.stemma`.
- [x] A cleanup of `tmp/`: names that stay after a failed or interrupted write, of the log and of
      the references. The writer that takes the lock removes them (STEMMA-31).
- [x] Restoring: what `stemma op restore` does when the working directory has changed since, and
      the entry that the restore writes (STEMMA-28). Decided in ADR-013 and implemented (STEMMA-28).
- [ ] Who may read the log (ADR-003): it holds IDs and the actor, no paths, but it makes
      commits reachable that a role may not be allowed to see.
- [x] A reading order that does not read every file: the newest first, and a limit
      (`stemma op log`, STEMMA-27). It lists all file names, which finds a gap, and reads only
      the files of the entries that are shown.
- [ ] The `session` of an agent is part of its actor; whether the log offers "what did this
      session do" as a view.
- [x] Windows: the flush of a directory was a no-op there, so the order "entry, then move" held
      for atomicity but not for durability (ADR-007). Decided and implemented on 2026-10-24
      (STEMMA-84): Windows flushes the directory as Unix does, so the order holds for atomicity
      and for durability, and `NotFlushed` and `NotDurable` can occur there as well.
