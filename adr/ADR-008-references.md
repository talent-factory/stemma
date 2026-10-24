# ADR-008: References: Branches and HEAD

**Status:** Accepted. Confirmed by the author on 2026-10-24 (STEMMA-9) and implemented in Phase 1.
The open items move to Phase 3 (deleting a branch, tags, a compare-and-swap for HEAD) and to
Phase 5 (remote references); packed references come later.

## Context

A commit has an ID, but a person or an agent works with a name: `main`, `feat/login`. A
reference gives a name to a commit, and HEAD says which name, or which commit, the working copy
is on. The references are the only mutable state of the history: an object never changes, but a
branch moves. So a wrong write here is the one kind of damage that cannot be seen from the
objects, and two processes that move one branch at once can lose a change silently. Agents work
in parallel (ADR-004), so the second case is not rare. The implementation is
`crates/stemma-core/src/refs.rs`.

## Decision

- **A branch is a file** *(confirmed)*. `<metadata directory>/refs/heads/<name>` holds the ID of
  the commit and a newline, and nothing else. One file per branch means that independent branches
  do not touch each other, that each change replaces one file atomically, and that the state can
  be read with `cat`. A single file for all branches was rejected: every change would rewrite all
  of them.
- **HEAD** *(confirmed)*. `<metadata directory>/HEAD` is `branch <name>` or `commit <ID>` and a
  newline. A branch that has no file does not exist yet, and HEAD may name it, as it does in a
  new repository. There is no Git compatibility (README), so the format is our own and strict:
  the reader accepts only what the writer writes, as for objects (ADR-005). `ref: refs/heads/…`
  was rejected, because it would only help with Git tools that we do not support.
- **The name of a branch** *(confirmed)*. The components are separated by `/`. Each is made of
  `A-Z a-z 0-9 . _ -`, does not start with `.` or `-` (so `.` and `..` are out), does not end with
  `.`, and is not the name of a device on Windows (`con`, `prn`, `aux`, `nul`, `com1` to `com9`,
  `lpt1` to `lpt9`, in any case, with or without an extension). The whole name has at most 200
  bytes. Upper case is allowed, because names such as `feat/TF-792-login` are in use. Because the
  name is a file path on every platform, a name that works on one cannot fail on another.
  Loosening the rule later changes no ID.
- **A name is checked before the commit** *(confirmed)* (`update_branch`). When another entry holds
  the name
  (a case variant, a branch that is a prefix, or a directory of branches), the answer is
  `Conflict` with the kind of the entry, also if the caller names an `expected` commit: there is
  no branch of this name that could be at it, and `Stale` would hide the reason. `branch(name)`
  says `None` for such a name; it means "no branch with exactly this name".
- **One name per file system entry** *(confirmed)*. A name cannot be a branch and the start of
  other names (`a` and `a/b`), and two names that differ only in case (`Main`, `main`) are
  refused, on every platform, because a repository can be copied to a file system that ignores
  case. A name is compared exactly: on such a file system `main` does not open the file of `Main`.
- **A change is a compare-and-swap, and the checks are inside** *(confirmed)*.
  `update_branch(name, expected, new, objects)` moves a branch only if it is still at `expected`
  (`None` for a branch that does not exist) and fails with `Stale` otherwise. It checks that `new`
  is a commit that the object store has, so a branch cannot point to an object that is missing, or
  to a tree. As for `Index::write_tree` (ADR-005), no caller can forget a check. HEAD to a commit
  is checked the same way.
- **One lock for all changes** *(confirmed)*. The check, the comparison, and the write happen while
  the file
  `<metadata directory>/refs.lock` exists. It is created with "create new" and holds a token that
  only the process that made it knows; it is removed when the change ends, but only if it still
  holds that token, because a person may have removed a lock that looked stale, and another
  process may hold a new one. A change that finds the lock fails at once with `Locked` and does
  not wait. The lock is
  outside `refs/`, so no branch name can collide with it. One lock for all references is coarse,
  but a change is a few file operations, and it keeps the rule simple. A command that writes holds
  it longer, from its first object to its last move, at least as long as its snapshot takes, and a
  purge from its plan to its end (ADR-014, STEMMA-49). The command line removes the lock of an
  interrupted command (Ctrl-C, a termination signal) before the process ends, so only `kill -9`,
  a crash, or a handler that could not be installed leaves it behind.
- **A lock that a process left is taken over** *(confirmed, STEMMA-51)*. While a process holds
  `refs.lock`,
  it holds a lock of the operating system on the file `<metadata directory>/refs.lock.guard`,
  which stays and has no content (`File::try_lock`). The operating system releases that lock when
  the process ends in any way, also by `kill -9` or a crash, so a lock file whose guard is free was
  left by a process that ended. The holder writes `stemma-lock 1`, `host <name>`,
  `pid <process ID>`, and `token <token>`, one line each, and a change that holds the guard
  replaces a file of exactly this form only if it names this host. A lock of the operating system
  may not reach from one host to another on a file system that two hosts share (NFS without a
  lock manager, a mount into a virtual machine), so a file of another host stays and is `Locked`
  with that host; the message says that the file can be removed if no command runs there. The
  rule assumes that no two hosts that share a repository carry the same name, so the defaults
  `localhost`, `localhost.localdomain`, and `(none)` count as no name, and on macOS the name is
  the local host name, which stays when the network changes. A file that names this process is
  taken over as well: it was left by an earlier process with the same ID, as in a container whose
  command always runs as the same process. A lock that this process holds is found in its own
  list first, and is `Locked` as held by this process, since the lock is not re-entrant; the
  guard is then not opened a second time, because closing a second handle releases the lock of
  the first where the system keeps its locks per process (`fcntl` on NFS). While the journal of a
  purge exists, a change does not take the lock over and fails with `PurgePending`; only
  `--resume` and `--abort` replace it (ADR-014). A file of another form stays and is `Locked`: an
  older version of stemma, a command without the guard, or a person made it, and so does the
  empty file that a crash between making and writing the lock leaves; it must be removed by
  hand. A guard that a running process holds is `Locked` with that process, whose message says
  to try again and not to remove the file. If the guard cannot be locked for another reason than
  a holder, or the name of the host is not known, the lock is made without a guard, in the form
  of an older version, so that no command takes it over; a guard that fails costs the takeover
  and never the exclusion, which `create_new` keeps. Such a command takes no lock over either,
  and its message names why the guard could not be taken; only `--resume` and `--abort` replace
  a file of another form then, which a purge without a guard leaves, as they did before the
  guard. The guard and the lock file are taken in this order and released in the other, and only
  a holder of the guard may replace a file of the guarded form, so two commands cannot take over
  one lock at once. A person who removes the guard while a process holds it breaks the rule, as
  one who removes the lock file does.
- **Safe writes** *(confirmed)*. A file is written to a new temporary file in `<metadata
  directory>/tmp/`,
  flushed, and renamed over the old one; the directory is flushed on Unix. The file is **not**
  read-only, because Windows refuses to replace such a file. A reader sees the old content or the
  new one. The temporary file is made before the directories of the branch, so that a failure of
  the first leaves no directory behind; if a later step fails, the directories that are empty
  again are removed. A directory without any file below it, which a crash can leave, is no
  branch and in nobody's way: the next change that needs the name clears it. If the file is in
  place and only the flush of the directory fails, the change has happened, and the error is
  `NotFlushed`; the caller reads the reference again and does not repeat the change with the old
  state.
- **The module stores, and nothing more** *(confirmed)*. The module does not call the
  authorization hook (ADR-002) and does not write the operation log (ADR-011); the layer above
  does, because it knows the actor and the operation. The default branch of a new repository is
  its decision as well. The module has the trait `RefStore` (sealed, like `ObjectStore`) and the
  implementation `FsRefStore`.

## Consequences

- A reader sees a branch or HEAD in canonical form or gets an error with the path: `Corrupt`.
  A file that is cut off, has a second line, or has a name that is not a branch name is damage.
  A path that is not what it should be (HEAD as a directory, `refs` as a file) is an error of the
  file system and not "no such reference". The listing of the branches skips entries whose name
  starts with a dot: no branch name does, and file managers leave such files (`.DS_Store`).
  This holds for a directory as well, with all that is below it, since no component of a branch
  name starts with a dot and nothing below it can be a branch.
  The listing reads without the lock, so another change may delete a branch, or the last branch
  below a directory together with that directory, while it runs. An entry that is gone between
  the listing and the read is no branch and no error, and so is a branch file that a lookup
  found and that is gone when it is read (STEMMA-57). Any other entry stays strict: a file or a
  directory whose name is not a branch name, such as the backup `main~` of an editor, stops the
  listing with `Corrupt`, because a reader accepts only what the writer writes (ADR-005), and
  skipping it would hide damage. The error names the path, so the entry can be removed. A name
  such as `main.orig` is a valid branch name; such a file is listed as a branch if its content
  is a valid reference, and is `Corrupt` otherwise. The check whether a directory holds files,
  after which a change may remove it as empty, counts every doubt as a file: a directory below
  it that was listed and cannot be opened may still hold one. A race in which a directory is
  replaced by a file while it is listed still ends the listing with `Io`; the next one succeeds.
- `update_branch` and `set_head` refuse a commit that is not in the object store
  (`MissingCommit`) or is another object (`NotACommit`). A failure of the file system in that
  check is an `Io`, as everywhere in the module.
- A change that loses a race fails with `Stale` or `Locked` and changes nothing. The caller
  reads again and decides; the module does not retry.
- Listing the branches walks the directory, and a lookup reads the listing of each directory on
  the path of the name, to compare the case that is stored. That costs a few directory reads per
  call and is the price of exact names on file systems that ignore case.
- The names of `con` and its kind are refused on every platform, also for people who never use
  Windows.
- `FsRefStore::verify` (and `Repository::verify_refs`) checks, without the lock, that a detached
  HEAD, the working-copy commit, and every branch name a commit that the store has. A write
  checks this already, so a failure means that an object was removed or damaged, or that a
  reference file was edited by hand. The check is shallow: the commit is read and its hash
  checked, but it is not parsed, and its tree and parents are not followed. The result lists
  every reference that fails, as a `VerifyError` with the pointer as its place: a missing
  commit, another kind of object, a damaged one, or one that cannot be read. A failure of the
  storage is listed for each reference as well, as for a tree, because it may concern one
  object only. A pointer that fails is read again after the lookup and checked at its new
  commit if it moved, so a purge that moves a pointer and removes its old commit meanwhile
  reports nothing. A reference that cannot be read itself (a missing HEAD or working-copy
  file, a file not in canonical form) stops the check with its `RefError`. While the journal
  of a purge exists, before or after the check, it refuses with `PurgePending` (ADR-014). HEAD
  that names a branch which does not exist yet is no failure.

## Open items

- [ ] Deleting a branch (`delete_branch(name, expected)`) with the same compare-and-swap, and the
      rule that the branch of HEAD is not deleted (that one belongs to the layer above).
- [ ] Packed references, when there are many branches.
- [ ] Tags.
- [x] Checking that the branches and HEAD of a repository point to commits that the store has
      (as `Tree::verify` does for a tree): `FsRefStore::verify` (STEMMA-50), see Consequences.
- [ ] `set_head` replaces HEAD whatever it was; a compare-and-swap for HEAD when `checkout` needs
      it. `Expected { Absent, At(commit) }` in place of `Option` for `update_branch`, if the
      meaning of `None` (the branch has to be new) proves easy to misread.
- [x] A lock that a crashed process left behind: the guard of the lock (STEMMA-51), see the
      decision above.
- [ ] The behavior of the replacing rename on Windows is tested only by the CI workflow run by
      hand (see ADR-007 for the same question about objects).
- [x] The log of changes of a reference (a reflog) is part of the operation log (ADR-005), which
      ADR-011 designs: one entry for each change of a pointer, with the old and the new value.
- [ ] Remote references and the record of a ref update on a hub (ADR-003).
