# ADR-013: Restoring the state before an entry

**Status:** Accepted. Confirmed by the author on 2026-10-21 and implemented: the checkout layer
(STEMMA-42, PR #74), `Repository::restore` (STEMMA-43 and STEMMA-45, PR #75), and the command
`stemma op restore` (STEMMA-44, PR #79). The open items at the end are limits that the first
version has and questions for later; none of them changes a decision above.

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

- **A restore puts back the old values of one entry** *(confirmed)*. `stemma op restore NUMBER`
  brings every pointer that the entry names to the value it had before the entry: HEAD, the
  working-copy commit, and the branches. A value that the entry records as missing (`-`) means
  that the pointer does not exist afterwards. Pointers that the entry does not name stay as they
  are. The restore writes an entry of its own, the operation `restore`, like every operation that
  moves a pointer. A restore never rewrites the log, so a restore can itself be restored; only a
  purge replaces entries by stubs (ADR-014).
- **A snapshot comes first** *(confirmed)*. Like every command, the restore saves the directory in
  a snapshot under the actor of the command, once it knows that there is something to restore
  (the plan is made before, so that a missing commit or a file in the way is found before the
  snapshot is written). A change that was not saved yet lands in the log, so no state is lost.
  The output names the number of that entry and says that the restore can be undone by restoring
  it. If a later step fails, the error names the entry of the snapshot.
- **The directory is written by a checkout layer** *(confirmed; STEMMA-42)*. It brings the
  directory from the tree of one commit to the tree of another and has no command of its own.
  - It plans first and writes after. A conflict that the plan can see is found before the first
    file is written; one that appears later stops the apply, which leaves what it did (see
    below).
  - It changes only a path that the old or the new tree names. It never reads, writes, or removes
    the content of a file that is not tracked. It never touches `.stemma` or `.git`, in the old
    tree or in the new one, also under their aliases (case, trailing dots, `GIT~1`, streams). A
    path that is new in the new tree and that `.stemmaignore` excludes is an error; a file that
    the old tree tracks is changed or removed even if a pattern excludes it now (ADR-012: tracked
    stays tracked). A file, a link, or a directory that holds a file and is in the way of a path
    of the new tree is an error that names it, and nothing is written. An empty directory in the
    way is removed.
  - A file is written to a temporary file in its own directory and renamed, so a reader sees the
    old or the new content. A file that the old tree had and the new one lacks is removed, and so
    is a directory that this emptied; no other directory is removed. A stop leaves what was done
    and reports how many files it removed and wrote; the next snapshot saves it as a change of
    the actor.
  - It keeps the duties of ADR-005: it rejects the names that are dangerous on Windows, it
    detects names that collide on a file system that ignores case, and it never writes through a
    link. Names that collide only in Unicode normalization form are an open item below.
- **The order is: entry, directory, pointers** *(confirmed)*. Under the lock of the references the
  restore checks, writes the entry, flushes it, brings the directory to the target tree, and
  moves the pointers last (ADR-011). A stop after the entry leaves the old state, and the entry
  names a move that did not happen, which the log already allows. A stop after the directory
  leaves the target tree in the directory and the old pointers; the next snapshot saves it as an
  ordinary change of the actor. Nothing is lost in either case.
- **Each pointer is judged on its own** *(confirmed)*. The entry is an intent (ADR-011), and it may
  have moved only some of its pointers: a `commit` that stopped after the branch has moved one of
  two. The restore therefore does not ask whether the entry as a whole took effect. It puts the
  old value of every pointer that the entry names back and leaves alone a pointer that has it
  already; if there is nothing to move, the command says so and changes nothing (exit code 0).
  This also holds for an entry that did not take effect and for a pointer that other entries
  moved afterwards. The outcome that `oplog::outcomes` reads is used for one thing: if it is
  unknown, the command refuses and names the reason, and `--force` restores anyway.
- **A missing object is an error before anything is written** *(confirmed)*. The restore checks
  that every commit it needs is in the object store. After a garbage collection (ADR-007) a
  commit that no entry names may be gone; the log keeps the roots, so this is not expected for a
  commit that an entry names.
- **`--dry-run` plans and reports** *(confirmed)*. It prints the pointers that would move and the
  files that would be written and removed, and a conflict is its error; it writes nothing, not
  even the snapshot. In the code it is a function of its own, `plan_restore`, so that it cannot
  write.
- **The actor and the authorizer** *(confirmed; built in STEMMA-43)*. The command needs an actor
  like every command that changes a repository (ADR-002). The authorizer is asked for the
  operation `restore` first, with the entry as the target; the snapshot that follows asks its
  own question, `snapshot`, as it does for every command.
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
- [ ] A dry run does not take the snapshot, so a change that is not saved yet is not in its plan.
      The real run can find a conflict that the dry run did not, and replaces a file with such a
      change (it is in the snapshot). The plan does not say which files have unsaved changes.
- [ ] If the files cannot be written after the entry, the entry names a restore that did not
      happen and no pointer moved. `op log` shows it as not taken, and a repeated restore of the
      same entry writes a second entry; a file that is there already with the right content is
      left as it is, so the second run completes the first.
- [ ] After the pointers moved in this order (branches, working-copy commit, HEAD), a stop
      between two of them leaves the files at the old state and some pointers at the new one. The
      error says so and says to run the same restore again. A first move that fails leaves all
      pointers new and the files old.
- [ ] If the clock or the log fails after the snapshot but before the entry, the snapshot stays
      and the error names it; the snapshot is not undone.
- [ ] The plan is made before the snapshot, and again under the lock. A snapshot that changes
      what the plan sees (an unsaved change in a file that the restore removes) is not planned
      for; the second plan is the one that counts.
- [ ] A directory in the way that is not empty because of untracked files: the first version
      refuses; a way to move them aside is open.
- [ ] Windows and file systems that ignore case: the functions for Windows names and for case
      collisions are tested on every platform, but the Windows branch of the name check has never
      run end to end, and no test ran on Windows (STEMMA-33).
- [ ] Names that differ only in Unicode normalization form are not found by the plan, as the
      crate has no normalization tables. The apply finds them when the second file is written, if
      the second path is new (the first file is there already), and stops. If the second path is
      also in the old tree, it replaces the first file without a word.
- [ ] A file that appears between the last check and the rename of a write is replaced. The
      window is as short as it can be with `rename`. Closing it needs a rename that fails when the
      target exists (`renameat2` with `RENAME_NOREPLACE` on Linux, `renamex_np` with `RENAME_EXCL`
      on macOS, and another call on Windows), which `std::fs::rename` does not offer.
- [ ] The layer does not compare the content of a file that it removes or replaces with the
      old tree. It relies on the snapshot that the restore takes first (decided above).
- [ ] The plan decides by platform whether names ignore case. A case-sensitive volume on macOS,
      or a rename that changes only the normalization form of a name, can pass or fail the plan
      for the wrong reason; the apply stops with a clear error, but halfway.
- [x] A temporary file `.stemma-checkout-*` stays if the process is killed between its creation
      and the rename. It holds the content of an old blob, perhaps a secret that was purged since,
      so `track` and `status` leave a name of exactly that form out, as they leave `.git` out
      (ADR-012), and `status` lists it apart for a person to remove (STEMMA-75). A path that names
      it is an error of `track`. A directory that `.stemmaignore` excludes is not entered, so a
      leftover in it is not listed, but it is not tracked either; one that a repository tracked
      before the fix stays tracked and is listed. A write that fails removes its temporary file;
      if that removal fails as well (a lock on Windows, permissions), the error of the checkout
      names the file. A later checkout that reports such a file as the path in its way names it
      as a leftover (STEMMA-79).
- [ ] A file to remove that lies below a link is skipped without a word, as it is no longer the
      file of the old tree.
