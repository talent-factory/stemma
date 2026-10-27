# ADR-014: Removing a secret from the history and the operation log

**Status:** Accepted and implemented: the plan (STEMMA-46), the stub with the `purge` entry in
the log (STEMMA-47), the purge itself with the journal, resume, abort, and format 3
(STEMMA-48), and the lock that a writer takes before its first object (STEMMA-49, agreed with
the author on 2026-10-22). The hash that a stub keeps came with the hash chain of the log
(STEMMA-41, ADR-011, agreed on 2026-10-22). The decisions marked *(confirmed)* were
agreed with the author on 2026-10-21 (STEMMA-26), and the author confirmed the whole ADR on the
same day, including the steps, the journal, the lock, resume, and abort. The slices are listed at
the end. The open items at the end are questions for the slices and for later; none of them
changes a decision above.

## Context

The goal of Stemma is that no state of an agent is lost (ADR-010). That must not mean that
nothing can be removed. Two things keep a secret once it is in a tracked file, for example a key
that a `config` gained after it was tracked:

- The history. A snapshot saves every tracked file, so the blob is in the tree of the working-copy
  commit and of every commit made from it.
- The operation log. It keeps every commit that an operation made unreachable (ADR-011), so
  rewriting the history alone leaves the old commits reachable through the log.

The exclusion file (ADR-012) and the confirmation of new files (ADR-010) make this rare, but not
impossible. ADR-005 chose a local log so that a secret can be removed; ADR-010 and ADR-011 leave
the removal to an operation of its own and name a stub that replaces the entries (STEMMA-26).

Facts that bound the design:

- An entry holds IDs, the actor, and the time, never file content (ADR-011). The secret is in the
  blobs and trees that the IDs reach. An entry is harmful because its commit ID makes the old
  state reachable, for example through `stemma op restore`.
- An ID is the hash of the content (ADR-005, ADR-001). Changing a tree changes the ID of its
  commit and, through the parent links, the ID of every descendant.
- Nothing is deleted today (ADR-011), and no garbage collection exists (ADR-007).
- The log is local and no hub exists (ADR-003 and ADR-004 are proposals). Roles are not built.

## Decision

- **The purge rewrites the affected commits** *(confirmed)*. The input is the ID of a blob. Every
  commit whose tree holds that blob (in any depth) gets a new tree without the entry, and the same
  author, time, message, and change ID. Its descendants get new IDs because their parent
  changed. Branches, HEAD (when it names a commit), and the working-copy commit move to the new
  commits. The change ID stays, so a person can still follow a change across the purge.
  - A file whose blob is removed leaves the tree, so it is no longer tracked. The directory is not
    touched. If the file is still needed, the secret has to leave it, the name has to go into
    `.stemmaignore` or the file has to be tracked again, and `track` does the rest.
  - Rewriting only the blob and keeping the commits is not possible: a tree that names a missing
    blob cannot be read, and a replacement blob has another ID, so the tree would have to change
    as well.
- **The entries of the log that name a rewritten commit become stubs** *(confirmed)*. Every entry
  that names a rewritten commit as an old or a new value is replaced by a stub with the **same
  number**, so the numbering has no gap (ADR-011). The stub keeps the number, the time, and the
  actor, and drops the operation and every ID. A stub is not an entry that can be restored:
  `stemma op restore` refuses it and says that it was purged. All entries that name a rewritten
  commit are replaced, also those of descendants that do not hold the blob themselves, because
  their commit has the secret among its ancestors.
- **The purge writes an entry of its own** *(confirmed)*. Operation `purge`, with the actor who
  ran it, the time, the number of rewritten commits, and the **numbers of the replaced entries**
  (as ranges). The entry carries **no pointer line and no ID**: the audit must not keep what it
  removes, and the new values of the pointers are in the references anyway. The reader accepts
  an entry of this operation without a pointer (the one exception to ADR-011's "at least one";
  a later `prune` entry of a retention would be the second, ADR-011), and `stemma op restore`
  refuses it. A purge cannot be restored, which is its purpose. A stub is
  valid only if a `purge` entry lists its number. If the list does not fit into the limit of an
  entry (64 KiB, ADR-011), the purge writes as many `purge` entries as it needs. The stub and the
  pointer-less entry change the format, so they come with the entry encoding `stemma-oplog 2` and
  the format version of the repository (ADR-009, ADR-011): an older reader refuses the log
  instead of calling it corrupt.
- **Only a human may purge, and the call is authorized** *(confirmed)*. A purge removes history,
  which is level 3 of ADR-004 (a human owner). The core calls
  `authorize(actor, "purge", target)` like every operation that changes a repository (ADR-002)
  and **in addition refuses an
  actor that is an agent**, whatever the authorizer says: phase 1 allows everything, and an agent
  must not remove history on its own. This is a rule about the kind of actor and not a role, and
  it is an explicit exception to ADR-002, which leaves roles to the policy model. It is advisory
  until actors can be proved (ADR-002: the text of an actor proves nothing about who runs the
  command, ADR-003): an agent that names itself `human` is not stopped. A human sponsor can run
  the purge for the agent.
- **The purge deletes the objects that its rewrite replaced, and nothing else** *(confirmed)*. The
  delete set is the old versions of the rewritten commits, their trees that changed, and the blob.
  An object is deleted only if no kept root reaches it any more (the branches, HEAD, the
  working-copy commit, and the entries that remain). Everything else stays, also an object that
  only a replaced entry named and that holds no secret: it may become unreachable, but the purge
  does not decide that it is garbage. The purge is therefore not a general garbage collection and
  does not make ADR-007's collection necessary. The plan also scans the store for trees that name
  the blob, for the commits of such trees, and for the commits after them. One that no kept root
  reaches (the leftover of a crash before its entry) is deleted with them and not rewritten, so
  that no tree is left that names a missing blob and no commit that names a deleted parent. A
  leftover that holds no secret and follows no such commit stays. The plan checks at the end
  that nothing kept holds the blob or reaches the delete set, and fails if it does (STEMMA-46).
  The scratch files of `tmp/` are removed when the purge takes the lock (ADR-011); see the
  limits.
- **A synchronized repository stays open** *(confirmed)*. A secret that was pushed (phase 5) is
  also on the hub, and a purge here does not reach it. The rule for users is that a pushed secret
  counts as compromised and has to be replaced, whatever a purge does. The hub needs a removal of
  its own, which belongs to phase 5.

### The steps

A purge holds the repository lock from the plan to the end and keeps a journal. The lock is the
file `refs.lock`, which is not re-entrant, so the purge needs an internal path that writes the
`purge` entry and moves the pointers while it holds the lock (`Lock` is private to the references
today). Every other command that writes takes the same lock before its first object and keeps it
to its last move (STEMMA-49, see "Writers refuse"), so a purge and a writer cannot overlap. The
journal keeps writers out while a purge that stopped has not been resumed or aborted, when no
lock may be held.

1. **Refuse an agent** before anything is written. `stemma purge BLOB` without `--apply` prints
   the plan and writes nothing, not even a snapshot, as `op restore --dry-run` does (ADR-013).
2. **Snapshot** (with `--apply`). Like every command (ADR-010), the purge takes a snapshot of the
   directory first, so that a change that holds the blob is in a commit and in the plan. The
   plan that `--apply` executes is made again after the snapshot and can differ from the printed
   one.
3. **Plan** under the lock: the commits to rewrite, the entries to replace, the delete set. The
   flag is `--apply` and not `--force`: the plan is the default, unlike `op restore`, because a
   purge cannot be taken back, and `--force` of the restore overrides a refusal.
4. **Journal.** Write `.stemma/purge-pending` (a temporary file, a flush, and a rename). It holds
   the blob ID, the number of the last entry of the log at this moment, the old and the new ID of
   every rewritten commit, and the delete set. These are IDs of removed objects, so the file
   exists only until the last step. The blob ID is a hash of the whole file: for a short secret
   it allows a guess offline, and the ID is also on the command line and in the history of the
   shell.
   - **What the journal holds** (STEMMA-48). It is strict text that starts with `stemma-purge 1`,
     then `blob`, `last`, one `rewrite <old> <new>` per commit (parents first), the moves of
     `head`, `working-copy`, and each `branch <name>` with the old and the new value, `stubs`
     with ranges as in the log, one `new <id>` per object that the purge writes and the store
     did not have, and one `delete commit` or `delete tree` per object of the delete set (the
     blob is implied). The moves let `--resume` compare a pointer with its two values without a
     new plan; the lines `new` let `--abort` remove only what the purge wrote, as a new tree can
     be one that an unaffected commit already has. The reader checks the ranges of `stubs`
     before it expands them: a stub after `last` or more than 2^24 stubs in all are damage, and
     a few bytes cannot make it hold a trillion numbers (found by the property test of
     STEMMA-20). The plan refuses a purge that would replace more entries
     (`PurgeError::TooManyEntries`), so no purge writes a journal that its reader refuses.
   - **The format rises first.** The repository moves to format 4 just before the journal is
     written, as a temporary file and a rename of `format`. A program that knows only version 2
     does not know the journal either and would write past it; with the higher format it refuses
     the repository. An abort leaves the format, as a format is never lowered. The purge came
     with format 3; since the hash chain `init` writes version 4 and every new entry raises a
     repository to it (ADR-011, STEMMA-41), so the purge raises to 4 as well.
   - **Writers refuse.** Every command that writes (`track`, `commit`, `status`, `op restore`)
     takes the lock of the references before it writes its first object and keeps it to its
     last move (STEMMA-49). The journal is checked under the lock, in the lock itself, so a
     writer refuses while a purge has not finished and says that `stemma purge --resume` or
     `--abort` has to come first; a lock that a purge holds or left behind is reported as the
     purge (`RefError::PurgePending`), not as "locked". The snapshot takes the lock after the
     authorizer agreed and before the first file is read; `commit` and `op restore` take it
     before their snapshot and keep it over the snapshot and their own moves, through an internal
     path, as the lock is not re-entrant. `op restore` also checks for the journal before it
     judges the entry, so a pending purge is named first.
   - **Why the writer locks before its first object** (STEMMA-49). A writer that checked for the
     journal before the purge wrote it, and moved its pointer after the purge removed it, could
     name an object that the purge deleted: a blob that its snapshot saved again (a no-op, as the
     blob still existed), or a tree that the purge had written as new and an abort removed. With
     the lock held from the first object, a purge cannot start while a writer saves, and a writer
     cannot start while a purge runs, so the window is closed. Two other ways were weighed. A
     counter of purges in `.stemma/` that a writer reads before its first object and compares
     under the lock would close it as well, with a new file in the layout (ADR-009) and an order
     of reads and writes that every writer and the purge must keep. A check under the lock that
     every object the new commit reaches exists would cost a walk of the whole tree at every
     `status`, under the lock, and a walk of its own for every kind of pointer. The lock is the
     simplest rule and also covers a later deleter (a garbage collection, ADR-007).
   - **The price of the longer lock.** A writer holds the lock as long as its snapshot takes:
     about 0.04 s for 100 files, 0.7 s for 10,000, and 3.3 s for 50,000 (STEMMA-37, ADR-010), and
     longer for the first `track`, which flushes every new object (about 5 ms per file on macOS,
     ADR-007). A second command that writes in the same repository meanwhile fails at once with
     `Locked` (two processes in one repository are not supported, ADR-010), and `op log` reads
     the newest entry as possibly unfinished while the lock is held (ADR-011). Ctrl-C and a
     termination signal remove the lock before the process ends (exit code 130 for Ctrl-C, 143
     for `SIGTERM`, 129 for `SIGHUP`), so an interrupted command does not leave it behind; an
     interrupted writer is left as a crash leaves it, with objects that no commit names and at
     most an entry without its move.
     `kill -9` and a crash of the system still leave the lock file, and the next writer of this
     host takes it over, since its guard is free (ADR-008, STEMMA-51); while the journal of a
     purge exists, it stays for `--resume` and `--abort`.
5. Write the new trees and commits. They are new objects, so nothing is lost yet.
6. Check all pointers against the values of the plan, then write the `purge` entry and move the
   pointers. If a pointer differs before the first move, nothing has moved: the call fails and
   `--abort` removes the journal. If a move fails after the first (`PartlyMoved`, ADR-011), every
   pointer is at its old or at its new value, and `--resume` finishes it.
7. Replace the entries that name a rewritten commit by stubs (a temporary file in `tmp/` and a
   rename, never a name inside `oplog/`).
8. Delete the delete set, then remove the journal.

**Resume.** `stemma purge --resume` reads the journal and repeats the steps from step 5. A crash
leaves `refs.lock` behind (ADR-011), and the lock would stop the resume. While the journal exists,
`--resume` and `--abort` treat the lock as the purge's own and replace it, unless a running
process holds its guard (a purge that still runs, STEMMA-51) or the file names another host, whose
command may still run. A purge without a guard (an older version, a file system without locks of
files) is not seen, so they must not run beside it. Where `--resume` cannot take the guard itself,
it replaces only a file of the older form, which such a purge leaves, and not one that a holder
of the guard wrote; it then fails with `Locked`, and the file must be removed by hand. Any other
command that writes fails with `PurgePending`, whose message names the journal. Each step can be
repeated: a new object that exists is a no-op (ADR-007), a pointer that already has its new value
is skipped, a stub is replaced by itself, and a deleted object is gone. The `purge` entry is
found, not written again: it is the entry with an operation `purge` and a number above the one
that the journal recorded.
A pointer that is at neither its old nor its new value (a foreign change after the lock was
removed by hand) makes `--resume` refuse and name the pointer. Because the journal holds the delete
set, a crash after step 7 does not leave the blob behind as an object that "was already
unreachable".

**Abort.** `stemma purge --abort` removes the journal and the objects that step 5 wrote, as long
as the `purge` entry is not written; the format stays where the purge raised it.
Once the entry is written, the purge
can only go forward: that is the point of no return, and a purge that cannot finish (a missing
object, an unreadable log) is left for a person to repair.

## Consequences

- A purge is the one operation that makes a repository forget. The tests have to show that no
  entry and no reachable object names the blob afterwards, that a stub and the `purge` entry
  refuse `op restore`, that the numbering has no gap, that a crash after each step is finished by
  `--resume`, and that an unaffected commit is not deleted.
- **What a purge does not guarantee.** The file system may keep the bytes of a deleted file (wear
  levelling of an SSD, a snapshot of APFS, a backup). A scratch file in `tmp/` can hold the
  secret after a failed write (ADR-007). The purge takes the lock, so no writer owns such a
  file, and it removes them with the rest of the scratch (ADR-011, STEMMA-31). A file that cannot
  be removed stays: `purge --apply` and `--resume` name each one in a warning and still succeed,
  as the purge itself is done. On Windows the read-only flag of an object file is cleared before
  the removal. A temporary file `.stemma-checkout-…` of a failed checkout lies in the working
  directory (ADR-013), not in `tmp/`, and is not looked at; `track` never takes it, so a new one
  cannot bring the secret back, and `status` lists it for a person to remove (STEMMA-75),
  outside a directory that `.stemmaignore` excludes. One that a version before that rule tracked
  stays tracked, and every snapshot saves its content again until it is removed. Other objects that
  were unreachable before and name no blob stay. The commit messages and path names are kept as
  they are and can hold the secret too; the unit of the purge is the blob. A file below `objects/`
  whose path is not that of an ID is not looked at, as no read reaches it, and the scan reads
  only the header of a blob, so a tree whose header was damaged into that of a blob is not
  found. Nothing stops a later `track` from taking the secret in again unless `.stemmaignore`
  names it. A file that held the secret stays in the working directory, untracked, so
  `track .` would save it again. `purge --apply` therefore names the paths at which a rewritten
  commit held the blob and advises to name them in `.stemmaignore` or to remove the files
  (STEMMA-77). `--resume` names none, as the journal does not keep the paths and the old trees
  may already be deleted.
- Commit IDs change for the rewritten commits and their descendants. Anything that holds an old ID
  outside the repository (a note, a message) points at nothing afterwards. Change IDs stay. Two
  commits that differed only in the blob can become one. The rewrite takes new parents with
  `Commit::reparent`, as `Commit::rewrite` keeps them.
- The log needs a reader that tells a stub from an entry. `outcomes` (ADR-011) takes entries, so
  it either takes the stubs as well or skips them. If it skips them, the entry before a stub
  finds a later old value that is neither its old nor its new one and is unknown, and a restore of
  it needs `--force`. That is acceptable. `op log` needs a line for a stub and for a `purge`
  entry, which has no pointer.
- The hash chain of the log (STEMMA-41, ADR-011) would collide with a stub, as replacing an
  entry changes the hash that the next entry links to. The hash of an entry is therefore taken
  over an envelope (number, time, actor, the hash of the entry before, and the hash of the body),
  and a stub of an entry in the chain keeps the hash of the body (`stub <hash>`, entry encoding
  `stemma-oplog 3`): its hash is the hash of the entry it replaced, and the chain holds across a
  purge. The reader accepts the stub only if a `purge` entry lists its number (`check_stubs`),
  which detects a replacement without a `purge` entry. It does not stop someone who can write the
  log from adding both, because actors are not proved (ADR-002). A stub of an entry from before
  the chain stays `stub` alone, in version 2.
- The tip file (ADR-011) is not affected: a stub keeps the number, and the hash that the tip
  names, as the stub has the hash of its entry.
- The layout (ADR-009) gets the file `.stemma/purge-pending`, and `Repository::open` has to
  accept it and the writers have to check for it.
- A writer that started before a purge cannot overtake it (STEMMA-49): it holds the lock from
  its first object to its last move, so the purge fails with `Locked` before its format and its
  journal, and can be run again once the writer is done. The window that the journal alone left
  open (a writer that checked before the journal and moved after the purge) is closed. A test
  runs a purge at a stop point of a snapshot, a `commit`, and an `op restore` and shows that the
  purge is refused and that no pointer reaches a missing object. The public moves of
  `Repository` (`update_branch`, `set_head`, `update_working_copy`) take the lock only for the
  move: their caller writes its objects itself, before the call, and is responsible for them.
- `--resume` and `--abort` take any lock for the one that the purge left behind. A purge that is
  still running in another terminal loses its lock that way; this is advisory, as the lock is,
  and the documentation says not to resume a purge that runs.
- Retention of snapshot entries (STEMMA-39) is a separate question, but the two share the stub:
  ADR-011 decides that a later retention replaces a snapshot entry by the same stub, listed by
  an entry of its own, and that phase 1 drops none.

## Open items

- [x] The format of the stub and of the `purge` entry, in the strict text form of ADR-011: entry
      encoding `stemma-oplog 2`, written only for these two (STEMMA-47; see ADR-011). A
      `purge` entry has `commits <n>` and `stubs <ranges>`, and a list that does not fit is
      split, the count in the first part.
- [x] The hash that a stub keeps when the chain exists: the hash of the body of the entry, so
      that the stub has the hash of the entry (STEMMA-41, entry encoding `stemma-oplog 3`).
- [x] The outcome of an entry before a stub: a stub replaced a change whose moves are no longer
      known, so it is a barrier, and every entry before the newest stub is unknown; `op restore`
      needs `--force` for it (STEMMA-47). This is stricter than the consequences above foresaw,
      as a stub can have moved a pointer back to an old value and so turn a verdict around.
- [x] A purge that stopped between its entry and its stubs: an entry that a `purge` entry lists
      and that is not a stub yet is no error of the log, `op log` marks it, and `op restore`
      refuses it (STEMMA-47). `--resume` finishes such a purge (STEMMA-48).
- [x] The form of the journal, when the format moves to 3, and how a writer is kept out: see
      step 4 (STEMMA-48). A purge that is resumed before its entry makes the new objects again
      from the old ones, which are still there, and checks them against the journal.
- [x] A writer that checked before the journal and moves after the purge: it takes the lock
      before its first object and keeps it to its last move (STEMMA-49, see step 4).
- [ ] How the user names the secret. The blob ID is the unit; a path or a pattern that finds the
      blobs in the history is a convenience, and a text that is replaced inside a file (instead
      of removing the file from the tree) would keep the file tracked. The first version takes
      the full blob ID (STEMMA-46).
- [ ] Whether the purge may keep a file tracked with the secret replaced by a given content,
      for a `config` that should stay in the history.
- [x] A tree that becomes empty when its only file leaves: it is dropped from its parent, as a
      snapshot never makes an empty directory; the tree of a commit can become empty. A test
      compares the new tree with the one that a snapshot makes of the same files (STEMMA-46).
- [ ] A lasting record of purged blob IDs that `track` and `status` check, so that the secret
      does not come back by accident. Until then `purge --apply` names the paths and advises
      `.stemmaignore` (STEMMA-77), see Consequences.
- [ ] The rewrite of commits with several parents (phase 3), where two parents can become one
      (`CommitError::DuplicateParent`), and of commits that are signed (ADR-003): a signature does
      not survive a rewrite.
- [ ] The reachability check of the last step has to know every kind of reference that holds
      objects, including those that come later (tags, remote references).
- [ ] The removal on a hub and the rule for a repository that was synchronized (phase 5).
- [ ] A secret blob whose file is damaged cannot be purged: the plan does not know that it is a
      blob. Deleting it needs no knowledge of its content, so a later version may allow it.

Slices, all built: the plan (`Repository::plan_purge` and `stemma purge BLOB`, which writes
nothing, STEMMA-46), the stubs and the `purge` entry in the log with its reader and `op restore`
refusing a stub (STEMMA-47), and `--apply` with the snapshot, the new objects, the journal, the
removal of the objects, resume, and abort, and the move of the repository to format 3 with its
first purge, which is format 4 since the hash chain (STEMMA-48:
`Repository::apply_purge`, `resume_purge`, and `abort_purge`). The apply
waited for the other two, as a pointer moves only with an entry and a purge without stubs and
removal would leave the secret in place. The lock of a writer from its first object, with the
removal of the lock on Ctrl-C, closed the window of an early writer (STEMMA-49).
