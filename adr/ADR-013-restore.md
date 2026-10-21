# ADR-013: Restoring the state before an entry

**Status:** Proposal. Nothing is confirmed or built yet. The decisions below are meant for the
author to confirm or change before the slices are built (STEMMA-42, STEMMA-43, STEMMA-44).

## Context

The operation log (ADR-011) records the old and the new value of every pointer that an operation
changes, and ADR-011 says that a state is restored by putting the old values back. STEMMA-28 asks
for the command that does it, `stemma op restore`.

Two facts decide the size of the work.

- The working copy is a commit, and a snapshot at every command makes it the state of the
  directory (ADR-010). If a restore only set the pointers back, the next command would take a
  snapshot of the unchanged directory and put the old state back at once. A restore that does
  something has to write the files of the old tree into the directory.
- Nothing writes files from a tree today. The directory is read by `track`, `commit`, and
  `status`, never written. ADR-005 names a *checkout layer* with duties (names that are dangerous
  on Windows, names that collide on a file system that ignores case or normalizes Unicode, never
  write through a link), but the layer does not exist.

So the restore needs a checkout layer first, and that layer is also what phase 3 (branches) needs.

## Decision

- **A restore puts back the old values of one entry** *(proposed)*. `stemma op restore NUMBER`
  brings every pointer that the entry names to the value it had before the entry: HEAD, the
  working-copy commit, and the branches. A value that the entry records as missing (`-`) means
  that the pointer does not exist afterwards. Pointers that the entry does not name stay as they
  are. The restore writes an entry of its own, the operation `restore`, like every operation that
  moves a pointer. The log is never rewritten, so a restore can itself be restored.
- **A snapshot comes first** *(proposed)*. Like every command, the restore starts with a snapshot
  of the directory under the actor of the command. A change that was not saved yet lands in the
  log, so no state is lost. The output names the number of that entry and says that the restore
  can be undone by restoring it.
- **The directory is written by a checkout layer** *(proposed; STEMMA-42)*. It brings the
  directory from the tree of one commit to the tree of another and has no command of its own.
  - It plans first and writes after. A conflict is found before the first file is written.
  - It touches only a path that the old or the new tree knows. It never touches `.stemma`, `.git`,
    a file that is not tracked, or a path that `.stemmaignore` excludes. A path of the new tree
    that is in the way of one of these is an error that names the path, and nothing is written.
  - A file is written to a temporary file in its own directory and renamed, so a reader sees the
    old or the new content. A file that the old tree had and the new one lacks is removed, and so
    is a directory that this emptied; no other directory is removed.
  - It keeps the duties of ADR-005: it rejects or escapes the names that are dangerous on
    Windows, it detects names that collide on a file system that ignores case or normalizes, and
    it never writes through a link.
- **The order is: entry, directory, pointers** *(proposed)*. Under the lock of the references the
  restore checks, writes the entry, flushes it, brings the directory to the target tree, and
  moves the pointers last (ADR-011). A stop after the entry leaves the old state, and the entry
  names a move that did not happen, which the log already allows. A stop after the directory
  leaves the target tree in the directory and the old pointers; the next snapshot saves it as an
  ordinary change of the actor. Nothing is lost in either case.
- **An entry that did not take effect is no work** *(proposed)*. The entry is an intent (ADR-011).
  The restore reads its outcome with `oplog::outcomes`. If the move did not take effect, the
  state before the entry is the current state, and the command says so and changes nothing (exit
  code 0). If the outcome is unknown, the command refuses and names the reason; `--force` restores
  anyway.
- **A missing object is an error before anything is written** *(proposed)*. The restore checks
  that every commit it needs is in the object store. After a garbage collection (ADR-007) a
  commit that no entry names may be gone; the log keeps the roots, so this is not expected for a
  commit that an entry names.
- **`--dry-run` plans and reports** *(proposed)*. It prints the pointers that would move, the
  files that would be written and removed, and the conflicts, and it writes nothing, not even the
  snapshot.
- **The actor and the authorizer** *(proposed)*. The command needs an actor like every command
  that changes a repository (ADR-002). The authorizer is asked once for the operation `restore`.
  The risk level of ADR-004 is not decided here: moving a branch back is a branch move, and the
  level follows the one that ADR-004 gives to those.

## Consequences

- **The work is three slices**, in this order: the checkout layer (STEMMA-42), then
  `Repository::restore` (STEMMA-43), then the command with its book chapter and reference
  (STEMMA-44). The first is the largest and the riskiest, as it removes and overwrites files.
- **The checkout layer is shared.** Phase 3 (branches and merge) uses it for `checkout` and for
  the working copy after a merge. The rules for conflicts with untracked files are therefore
  decided once, here.
- **A restore of a restore is possible and useful**: it is the way to undo a restore, and the
  snapshot that precedes it is the way to keep a change that was not saved.
- **A restore does not delete history.** The commits that the pointers leave behind stay in the
  log as old values, so the garbage collection keeps them.

## Open items

- [ ] Whether `restore` also takes a point in time or a range of entries ("restore the state of
      10:00"), and whether a shorter `stemma undo` for the last entry is wanted. Neither is
      needed for the first version.
- [ ] A pointer that has moved on since the entry: the restore puts the old value back even if
      other entries moved it afterwards. Whether the command should warn, and show the entries
      in between, is open.
- [ ] A restore of an entry whose old value is a commit with a conflict (phase 3).
- [ ] The risk level of a restore in ADR-004.
- [ ] A directory in the way that is not empty because of untracked files: the first version
      refuses; a way to move them aside is open.
- [ ] Windows and file systems that ignore case: the checkout layer needs a test on both
      (STEMMA-33).
