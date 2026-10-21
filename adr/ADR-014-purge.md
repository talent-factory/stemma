# ADR-014: Removing a secret from the history and the operation log

**Status:** Proposal, partially confirmed. The decisions marked *(confirmed)* were agreed with the
author on 2026-10-21 (STEMMA-26). The details that follow from them (the order of the steps, the
plan before the purge, the form of the stub) are proposals. No code exists yet.

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
  an entry of this operation without a pointer (the one exception to ADR-011's "at least one"),
  and `stemma op restore` refuses it. A purge cannot be restored, which is its purpose. A stub is
  valid only if a `purge` entry lists its number. If the list does not fit into the limit of an
  entry (64 KiB, ADR-011), the purge writes as many `purge` entries as it needs. The stub and the
  pointer-less entry change the format, so they come with the entry encoding `stemma-oplog 2` and
  the format version of the repository (ADR-009, ADR-011): an older reader refuses the log
  instead of calling it corrupt.
- **Only a human may purge, and the call is authorized** *(confirmed)*. A purge removes history,
  which is level 3 of ADR-004 (a human owner). The core calls
  `authorize(actor, "purge", target)` like every operation (ADR-002) and **in addition refuses an
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
  the blob and for the commits of such trees. One that no kept root reaches (the leftover of a
  crash before its entry) is deleted with them, so that no tree is left that names a missing blob.
  The scratch files of `tmp/` are not touched (see the limits).
- **A synchronized repository stays open** *(confirmed)*. A secret that was pushed (phase 5) is
  also on the hub, and a purge here does not reach it. The rule for users is that a pushed secret
  counts as compromised and has to be replaced, whatever a purge does. The hub needs a removal of
  its own, which belongs to phase 5.

### The steps

A purge holds the repository lock from the plan to the end and keeps a journal. The lock is the
file `refs.lock`, which is not re-entrant, so the purge needs an internal path that writes the
`purge` entry and moves the pointers while it holds the lock (`Lock` is private to the references
today). The lock does not cover the writes of objects, which a snapshot makes before it takes the
lock, so the journal is what keeps other writers out.

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
   - **Writers refuse.** Every command that writes (`track`, `commit`, `status`, `op restore`)
     checks for the journal first, before it writes any object, and again under the lock, and
     says that `stemma purge --resume` or `--abort` has to come first. The lock alone cannot do
     this: it only answers "locked".
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
`--resume` and `--abort` treat the lock as the purge's own and remove it, and the message of
`Locked` of any other command names the journal. Each step can be repeated: a new object that
exists is a no-op (ADR-007), a pointer that already has its new value is skipped, a stub is
replaced by itself, and a deleted object is gone. The `purge` entry is found, not written again:
it is the entry with an operation `purge` and a number above the one that the journal recorded.
A pointer that is at neither its old nor its new value (a foreign change after the lock was
removed by hand) makes `--resume` refuse and name the pointer. Because the journal holds the delete
set, a crash after step 7 does not leave the blob behind as an object that "was already
unreachable".

**Abort.** `stemma purge --abort` removes the journal and the objects that step 5 wrote, as long
as the `purge` entry is not written. Once it is, the purge can only go forward: that is the point
of no return, and a purge that cannot finish (a missing object, an unreadable log) is left for
a person to repair.

## Consequences

- A purge is the one operation that makes a repository forget. The tests have to show that no
  entry and no reachable object names the blob afterwards, that a stub and the `purge` entry
  refuse `op restore`, that the numbering has no gap, that a crash after each step is finished by
  `--resume`, and that an unaffected commit is not deleted.
- **What a purge does not guarantee.** The file system may keep the bytes of a deleted file (wear
  levelling of an SSD, a snapshot of APFS, a backup). The scratch files in `tmp/` can hold the
  secret after a failed write (ADR-007) or a failed checkout (ADR-013) until a cleanup of `tmp/`
  exists (STEMMA-31); the purge cannot tell a stale file from the live file of a writer that does
  not take the lock. Other objects that were unreachable before and name no blob stay. The commit
  messages and path names are kept as they are and can hold the secret too; the unit of the
  purge is the blob. Nothing stops a later `track` from taking the secret in again unless
  `.stemmaignore` names it.
- Commit IDs change for the rewritten commits and their descendants. Anything that holds an old ID
  outside the repository (a note, a message) points at nothing afterwards. Change IDs stay. Two
  commits that differed only in the blob can become one, and the rewrite needs a constructor that
  takes new parents (`Commit::rewrite` keeps them).
- The log needs a reader that tells a stub from an entry. `outcomes` (ADR-011) takes entries, so
  it either takes the stubs as well or skips them. If it skips them, the entry before a stub
  finds a later old value that is neither its old nor its new one and is unknown, and a restore of
  it needs `--force`. That is acceptable. `op log` needs a line for a stub and for a `purge`
  entry, which has no pointer.
- The hash chain of the log (STEMMA-41) collides with a stub: replacing an entry changes the hash
  that the next entry links to. Once the chain exists, a stub keeps the hash of the entry it
  replaced, and a verifier accepts it only if a `purge` entry lists its number. That detects a
  replacement without a `purge` entry. It does not stop someone who can write the log from adding
  both, because actors are not proved (ADR-002). The chain has to be decided before the stub gets
  its final format; until then the stub holds no hash.
- The tip file (ADR-011) is not affected: a stub keeps the number.
- The layout (ADR-009) gets the file `.stemma/purge-pending`, and `Repository::open` has to
  accept it and the writers have to check for it.
- Retention of snapshot entries (STEMMA-39) is a separate question, but the two share the stub:
  an entry that retention drops could be replaced by the same stub.

## Open items

- [ ] The format of the stub and of the `purge` entry, in the strict text form of ADR-011, and the
      hash that a stub keeps when the chain exists (STEMMA-41).
- [ ] How the user names the secret. The blob ID is the unit; a path or a pattern that finds the
      blobs in the history is a convenience, and a text that is replaced inside a file (instead
      of removing the file from the tree) would keep the file tracked.
- [ ] Whether the purge may keep a file tracked with the secret replaced by a given content,
      for a `config` that should stay in the history.
- [ ] A tree that becomes empty when its only file leaves: whether it is dropped from its parent,
      as a snapshot builds trees from files (to be checked against the snapshot in the slice).
- [ ] A record of purged blob IDs that `track` and `status` check, so that the secret does not
      come back by accident.
- [ ] The rewrite of commits with several parents (phase 3), where two parents can become one
      (`CommitError::DuplicateParent`), and of commits that are signed (ADR-003): a signature does
      not survive a rewrite.
- [ ] The reachability check of the last step has to know every kind of reference that holds
      objects, including those that come later (tags, remote references).
- [ ] The removal on a hub and the rule for a repository that was synchronized (phase 5).

Slices, once the ADR is confirmed: the rewrite of commits with the plan (`Repository::purge`),
the stub in the log with its reader and `op restore` refusing it, and the removal of the
objects.
