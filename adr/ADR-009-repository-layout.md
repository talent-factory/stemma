# ADR-009: Repository layout and `init`

**Status:** Accepted. The layout, the format file, the initial branch, the actor of `init`, and
the bundle of the stores were confirmed by the author on 2026-10-19, the other decisions on
2026-10-24 (STEMMA-9); all are implemented in Phase 1. The reason for a denial moves
to Phase 4, bare repositories to Phase 5; the other open items stay open.

## Context

The object store (ADR-007) and the references (ADR-008) each take a root directory and do not
check it. Nothing ties the two to one repository: `update_branch` accepts the object store of any
repository, and nothing says which directory is a repository at all. `stemma init` needs a place
where the layout, the format version, and the creation of a repository are defined once.
Implementation: `crates/stemma-core/src/repository.rs` and, for the command,
`crates/stemma-cli/src/cli.rs`.

## Decision

- **One type bundles the stores** *(confirmed)*. `Repository` in `stemma-core` holds the
  directory, an `FsObjectStore`, an `FsRefStore`, and an `FsOpLog` (ADR-011).
  The command line only calls it. The operations (`track`, `commit`, `status`, `op restore`,
  `purge`) are methods of `Repository`, so they use the stores of one repository.
- **Layout** *(confirmed)*. A repository is a directory that contains `.stemma/`. Inside:
  `format`, `HEAD`, `working-copy` (ADR-010), `objects/` (ADR-007),
  `oplog/` and `oplog-tip` (ADR-011),
  `refs/heads/`, `refs/tags/` (made by the first tag, ADR-008), `refs.lock` (while a command
  writes), and `refs.lock.guard` (the guard of the
  lock, which stays; ADR-008), `purge-pending` (the journal of a purge, only while one runs or after
  it stopped, ADR-014), `derived/` (the cache of derived data, optional, ADR-017), `worktrees/`
  (one state directory for each linked worktree, made by the first `worktree add`; see below),
  and `tmp/` (the temporary files of the object store, the references, the log, and the purge).
- **Linked worktrees** (STEMMA-175). The shared store (`objects/`, `refs/`, `oplog/`, `tmp/`, the
  lock, the journal of a purge, and `format`) stays in the `.stemma/` of the main worktree. A
  linked worktree adds only the pointers that belong to a working copy: its state directory
  `.stemma/worktrees/<name>/` holds the files `HEAD` and `working-copy` (ADR-010), as the
  main `.stemma/` does for the main worktree, and `location`, which holds the absolute path of
  the worktree and a newline. The directory of the worktree holds a file `.stemma` and no
  directory: its content is `stemma-worktree ` and the absolute path of the state directory and a
  newline. The state directory is named after the last part of the path, reduced to letters,
  digits, `-`, `_`, and `.` and numbered if the name is taken; `main` is the name of the main
  worktree and is never given to a linked one.
  `Repository::open` reads the pointer file, checks that it names `worktrees/<name>` of a
  `.stemma`, and checks that `location` in that state directory is the directory that holds the
  pointer file, with its links resolved. Otherwise the worktree was moved or copied, and the
  command fails with `WorktreeMoved` and a message that says so; a state directory that is gone
  is `MissingWorktreeState`, and a pointer file that claims the prefix and has another form is
  `MalformedPointer`, which counts as damage (exit code 5). A file `.stemma` without the prefix
  is a file in the way (`NotADirectory`), as before. Everything else about the repository is
  read from the shared `.stemma/`: the `format` is checked there, and `discover` checks the
  owner of the pointer file and of the shared `.stemma/` on Unix and Windows as it does for a
  directory.
  The branch is the unit of exclusion: **a branch is checked out in one worktree only**. A
  `HEAD` that names a branch holds its name, whether the branch has a commit yet or not. The
  rule has no override. It is checked under the lock of the references, through one helper that
  the code calls where the pointers are planned: `switch` and `op restore` (the move of `HEAD`
  to a branch, and the move or deletion of any branch that another worktree has checked out),
  `branch -d` and `branch -m` (the branch of another worktree would lose its name), and
  `worktree add`. The
  check reads the `HEAD` of every other worktree. A state directory without `HEAD` is an add that
  stopped and holds no branch. The error is `RefError::CheckedOut` and names the worktree.
  `worktree add <path> <branch>` checks out an existing branch that is free, and `-b <new>`
  makes `<new>` at the commit that `HEAD` of the calling worktree leads to. A call without a
  branch is a usage error (exit code 2): there are no detached worktrees. The path must not
  exist or must be an empty directory, and it must not be a worktree, lie in one, or contain
  one; the main worktree counts. `--owner` gives the working copy of the new worktree to another
  actor than the caller. The core asks the authorizer for the operation `worktree-add` (an
  operation name holds no space) with the path as the target. The new working-copy commit is
  empty, on the tip of the branch, with the owner as its author. `worktree list` shows the name,
  the path, the branch of `HEAD`, and the owner of every worktree, the main one first.
  The order of an add is the rule of ADR-011: the files are planned, the entry is written and
  flushed, the files are written, and then the pointers move: the new branch, `location`,
  `working-copy`, `HEAD`, and the pointer file last, so that a worktree that can be opened is a
  complete one. The entry names `HEAD` and the working-copy commit of the new worktree from
  nothing to their values, as the first entry of `init` does, and the new branch.
  The check of the paths and the making of the directory happen under the lock of the
  references, so of two adds of one path at once the second is turned away by the lock and
  has made nothing. `HEAD` follows the rule of the branches in every way it can move
  (`set_head`, `replace_head`, `set_head_after`), not only in the commands. A directory that a
  failed add made and cannot remove is named in the error (`WorktreeError::NotCleaned`).
  `worktree remove <worktree>` (STEMMA-176) takes the name or the path of a linked worktree. It
  refuses the main worktree, which holds the shared store, and the worktree where it runs (the
  directory would go under the feet of the command). The core asks the authorizer for
  `worktree-remove` with the argument as the target. The directory must hold the pointer file
  that names the state directory of this worktree, or no file of it is touched. Then the files
  are saved in a snapshot of the worktree (a second question, `snapshot`, and the owner of the
  working copy is checked as for every snapshot, so an agent cannot remove the worktree of a
  human; the owner is checked as well when the directory is gone and there is no snapshot), and every file that the working-copy commit does not track stops the call
  (`WorktreeError::Dirty`) unless `--force` is given. The order is the rule of ADR-011: the
  entry is written and flushed, then the directory is renamed to a hidden sibling and
  removed (a stop in the delete leaves no half of the worktree at its path), then `HEAD` of the state
  directory, so that the worktree is gone from `worktree list` and holds no branch, then the
  rest of the state directory. The entry names the worktree, its directory, and `HEAD` and the
  working-copy commit with the new value `-`. The branch and the commits stay: the branch is
  free again, and the entry keeps the working-copy commit for the log and for a purge. A
  worktree whose directory was deleted outside Stemma has only its state directory removed.
  A failure after the snapshot that came first keeps the exit code of the failure and names the
  entry of the snapshot; it is code 3 only if the snapshot moved a pointer and stopped or its
  flush failed (ADR-018). The
  output lists the first 20 files that `--force` removed with the directory and counts the rest.
  A state directory below `.stemma/worktrees` whose name no worktree can have (the name of an
  entry or of the journal of a purge, ADR-011, ADR-014) is damage (exit code 5): every command
  that has to read all worktrees (`worktree list`, `purge`, a switch, a branch delete or rename,
  `op restore`) names it and stops, and `worktree remove` of another worktree still works. A
  directory without `HEAD` is no worktree and is skipped, whatever its name; a `HEAD` that cannot
  be inspected is an error (exit code 1), not a missing file.
- **The format file** *(confirmed)*. `.stemma/format` holds `stemma-repository <version>` and a
  newline, and nothing else. The version is decimal, without a leading zero. The reader accepts
  what the writer writes and nothing else, as for objects (ADR-005). `Repository::open` rejects a
  missing file, a file that is not in canonical form, and any version other than the ones it
  supports: `init` writes version 6, the version of the worktree of an entry and of the journal
  of a purge across worktrees (ADR-011, ADR-014), and `open` reads versions 2 to 6: a repository
  made before the purge existed (2), before the chain (3), before the owner (4), or before the
  worktree of an entry (5) rises to 6 with its next entry or purge (ADR-014, ADR-011, ADR-010).
  Version 4 is the version of the hash chain of the log (ADR-011). From version 5
  on, the file `working-copy` names the owner on a second line, `owner <actor>`; a purge that
  raises the version records none, and the next entry of a change does (ADR-010). A version
  never sinks, with one exception: if the directory cannot be flushed after the rise, the old
  content is put back, so that the next entry or purge raises the version again and flushes it.
  Nothing has been written in the new version at that point, because the entry or purge stops
  at the error.
  The version stays text, so a number larger than any integer is an unsupported version and not
  a damaged file. The file is read up to 64 bytes. The hash function needs no
  entry, because every ID carries its algorithm (ADR-001). `open` checks nothing but `format`
  (and that `.stemma` is a directory); the stores report what is wrong with the rest when they
  are used.
- **`init` is atomic** *(confirmed)*. The directory is built under a temporary name next to its
  final place and moved into place with one rename, so that a failure leaves no half-made
  repository. The name is `.stemma-init-<pid>-<n>-<random>`; `init` creates it itself, tries
  the next name if one is taken, and removes only the directory that it created. If removal
  fails, the error names the directory (`Leftover`). If `.stemma` exists, even an empty one or a
  link, `init` fails before it creates the temporary directory and changes nothing but missing
  parents. A rename that fails while `.stemma` exists is reported as `AlreadyExists`, so the
  loser of two concurrent `init`s sees that error.
- **Durability** *(confirmed)*. The files, the new directories, missing parent directories, and
  the directory that holds `.stemma` are flushed. If only the last flush fails, the repository
  is complete and the error is `NotDurable`, not a plain I/O error, so that a retry does not
  meet an unexplained `AlreadyExists`.
- **The initial branch** *(confirmed)*. `init` takes it as an option (`InitOptions`), and `main`
  is the default. `stemma init` offers it as `--initial-branch`. There is no configuration file in
  phase 1. The name goes through `BranchName`, so an invalid name stops `init` before anything is
  created; on the command line the argument parser rejects it with exit code 2.
  HEAD points to the branch with `branch <name>`; the branch does not exist until the first
  commit (an unborn branch, as ADR-008 allows).
- **`init` has an actor and an authorizer** *(confirmed)*. `Repository::init(path, options, actor,
  authorizer)` calls `authorize(actor, "init", <directory>)` first (ADR-002). No policy can live
  in a repository that does not exist yet, so the caller supplies the authorizer; in phase 1 it
  is `AllowAll`. `init` records the actor in the first entry of the operation log (ADR-011);
  the actor is not stored in the format file or in HEAD.
- **The directory is created if it does not exist** *(confirmed)*, as `git init` does.

## Consequences

- **Finding the repository** (STEMMA-72). `Repository::discover(start)` opens the repository at
  `start`, as `open` does, or else the nearest directory above it that contains `.stemma`, and
  names the directory of `start` below the root. Only a missing `.stemma` at `start` leads the
  search upwards; anything else in the way there, and anything wrong with the repository that
  the search finds, is the error of `open`. A `start` that does not exist or is no directory is
  said so (`Missing`, `NotADirectoryToStartIn`), and a path from the root to `start` with a part
  that is not UTF-8 is `NotUtf8`, since such a part would become another name.
  The search goes along the path with its links resolved, and the result says whether it reached
  the repository through a link (`through_link`); the command line warns then, as a command in a
  link to a directory of another repository works in that one. The path is read as the shell
  reads it: `.` and `..` apply to the names before any link is resolved, so `-C ..` from a link
  is the directory of the link, and a relative start is taken from the `PWD` of the shell. Two
  links are not seen: one that led to the directory of a process that a program started without
  such a `PWD`, as `subprocess.run(cwd=...)` does, and one to the root of a repository itself,
  which is no detour.
  A `.stemma` that the search finds must be trusted. On Unix the effective user of the process
  owns it (`OtherOwner` otherwise), as Git checks for `safe.directory`, at `start` as well as
  above it (STEMMA-77): `-C` defaults to the current directory, so `cd /tmp` and a command there
  would otherwise open a `/tmp/.stemma` of another user. A user could otherwise put a `.stemma`
  into a shared directory such as `/tmp`, and a command run in it or below it would save the
  files that its tree names into the store of that user. There is no way around the check, and
  the message says to run the command as the owner. On Windows the owner SID of the `.stemma`
  must be the user SID or the default owner SID of the token of the process (`OtherOwner`
  otherwise), at `start` as well as above it, as Git for Windows checks for `safe.directory`;
  the default owner is the group of administrators for an elevated process. The owner of the
  entry itself counts, as on Unix, but a link or a junction in the place of `.stemma` is refused
  (`ReparsePoint`) even when the user made it, because the API would name the owner of its
  target. The default owner is an accepted widening: an elevated process also trusts a `.stemma`
  that another administrator made. Two limits stay: a `.stemma` that an elevated shell made is
  not trusted from a normal shell of the same administrator, whom Git for Windows would accept as
  a member of the group, and a file system that records no owner, such as FAT, fails with an
  error of the inspection, since nothing proves who owns it. A `.stemma` above
  `start` must also not be at the root of a volume, where every user may create directories
  (`AtVolumeRoot`), even when the user owns it; one at `start` is opened there, as `-C` reaches
  the root of a volume only when it names it, and the message gives that call.
  The search also reports the nearest directory from the start, included, up to the root, not
  included, that holds a `.git`, a file in a Git worktree or a directory in a nested clone
  (`nested_git`, STEMMA-132). Such a command works in the repository of the root, and unless
  `.stemmaignore` excludes the directory, the files of the checkout appear there as untracked
  paths. The command line warns, as for a link, and does not refuse: a checkout may be meant to
  belong to the repository, and the warning does not change the exit code. The `.git` of the
  root itself is the normal case and says nothing, and a checkout with a `.stemma` of its own is
  found at the start and does not warn.
  Every command but `init` uses the search, and the paths of `track`
  are relative to the directory of `-C`, as in Git: a `..` at their start leads up to the root
  and not beyond, and any other `..` is refused as before, since it may follow a link. Neither
  the search nor `..` crosses the root, so a nested repository is a repository of its own.
- `open` is the place that checks the format. The two stores stay unchecked on construction, as
  ADR-007 and ADR-008 describe; the check in `open` closes the open item of ADR-007.
- A new format version needs an ADR, a new number in `FORMAT_VERSION`, and a decision about the
  migration of existing repositories. An older program refuses a newer repository.
- A process that is killed during `init` can leave a `.stemma-init-…` directory. A later `track`
  must not treat it as content of the working copy (ADR-005 reserves `.stemma` and its aliases;
  this name is not one of them). The next writer that takes the lock of the references removes
  it (ADR-011, STEMMA-31): once `.stemma` exists, an `init` that still builds such a directory
  fails, at the latest at its rename (earlier, with the error of the step that found its
  directory gone, if a writer removes it under it; `init` then reports no `Leftover`, as nothing
  is left). Only a directory whose name is exactly `.stemma-init-<pid>-<n>-<16 lowercase
  hexadecimal digits>` is removed; a link, a file, and a directory of the working copy whose name
  only starts with the prefix, such as `.stemma-init-notes`, stay.
- `Repository` hands out the object store, but the references only as a view for reading
  (`Repository::refs()`, ADR-011). A pointer moves through `Repository::update_branch` and
  `set_head`, which use the object store of their own repository, so the gap that the context
  names is closed for those two operations. The operations that need both (`track`, `commit`,
  `op restore`, `purge`) are methods of `Repository` as well. The object store stays public for
  reading; hiding it is an open item.

## Open items

- [ ] `worktree repair`, for a worktree that was moved, and `--json` for `worktree list`
      (STEMMA-175 leaves both out). Until then a moved worktree is added again, and its state
      directory is removed by hand.
- [x] `worktree remove`, and the removal of a worktree directory that was deleted outside
      Stemma (STEMMA-176): the state directory goes, the branch is free again.
- [x] The operation log is one log for all worktrees, but `HEAD` and the working-copy commit are
      per worktree. An entry of a linked worktree names it (ADR-011), `op log` judges the
      pointers of each worktree by their own later entries, and `op restore` puts back the
      pointers of the worktree where it runs, so an entry of another worktree is refused there
      (STEMMA-176, ADR-013).
- [x] A purge over all worktrees: the roots are `HEAD` and the working-copy commit of every
      worktree, and a purge moves what it rewrites in each of them (STEMMA-176, ADR-014). The
      refusal `PurgeError::LinkedWorktrees` of STEMMA-175 is gone.
- [x] A state directory without `HEAD` (what a removal or an add that stopped leaves) is no
      worktree. `worktree remove` names it and says what to do (`WorktreeError::Unfinished`);
      `worktree add` removes it under the lock when it chooses the name, so the name is free
      again (STEMMA-176).
- [x] The format is raised with the first entry that can carry the line `worktree`: version 6
      (STEMMA-176). A program that knows only version 5 refuses such a repository with the
      unsupported version and does not report damage for a log or a journal that it cannot read.
- [ ] `worktree remove` does not undo with `op restore`: the entries of `worktree add` and
      `worktree remove` are refused by a restore. A removed worktree is added again with
      `worktree add` on its branch, and its last working-copy commit is named by the entry.
- [ ] Hide the object store too, if an operation needs to be sure that it is the one of this
      repository; today `objects()` is safe, because anything stored through it belongs to this
      repository, and `update_branch` and `set_head` use it themselves.
- [x] Finding the repository from a subdirectory: `Repository::discover` (STEMMA-72), see
      Consequences.
- [x] The owner of a `.stemma` at the start: checked on Unix as well, so `cd /tmp` does not
      open a `/tmp/.stemma` of another user (STEMMA-77), see Consequences.
- [x] On Unix the owner is checked on the handle that `open` holds (STEMMA-88): `discover` opens
      `.stemma` with `O_NOFOLLOW`, reads its owner from that handle (`fstat`), and opens `format`
      relative to it, also without following a link, so a `.stemma` that is swapped after the
      look at its path is refused before `format` is read. The look at the path stays as the
      first, cheap refusal and is repeated after the open, above the start as at it. A link in
      the place of `.stemma` or `format` is not followed by `discover`, even when the user
      made it (`NotADirectory`, `FormatNotRegular`), as a link is refused on Windows
      (`ReparsePoint`). `Repository::open` checks no owner and follows both links. Both read
      `format` only if it is a regular file, and open it without waiting for a writer, so on
      Unix a FIFO or a device fails with `FormatNotRegular` at once and does not block (the
      error of a directory in its place changed from `Io` to it; on Windows a directory still
      gives `Io`). A link that leads nowhere is in the way and not a missing `format`.
      Where the system has no `O_PATH` (Linux, Android, and FreeBSD have it; macOS and the
      other systems do not) a `.stemma` that the user may search but not read cannot be held.
      The directory is then looked at by its path and must belong to the user, `format` is
      opened by its path and must belong to the user (`FormatOwner`) and have no other name
      (`FormatShared`: a hard link in the directory of another user would pass the owner
      test), and the directory must still be the same one afterwards (`Changed`). This is
      weaker than the handle: a swap to another directory and back between the looks is not
      seen.
      Windows checks the owner on the path, between the look and the call.
- [x] The stores of a repository work by path, and stay so (STEMMA-193, decided: not worth the
      change now). The handle ends once `format` is read, so a `.stemma` that is swapped after
      the open is used without a check, in the search above the start as at it. Closing it needs
      the stores to work relative to a held directory handle (`openat`), in the object store,
      the references, and the operation log, with their locks, renames, flushes, the purge, and
      the injected failures (about 17,000 lines with their tests), and a second implementation
      for Windows, where `std` has no call relative to a handle.
      The gain is small. The owner is checked on the handle before anything is read, so a
      `.stemma` of another user is refused; what stays is a swap during the run of one command.
      It needs write access to the directory that holds `.stemma`, and whoever has that access
      can already change the working tree that the command saves, and can replace `.stemma`
      before the next command, where the check refuses it again if the new one is not the
      user's own. A replacement owned by the user is no attack. Revisit the decision if a
      repository is served to several users on one machine (phase 5, the list of trusted
      directories below) or if a command runs for long enough that the window matters.
- [ ] A list of trusted directories, as Git's `safe.directory`, for a repository that several
      users share on one machine; it needs the configuration file below.
- [x] On Windows the owner of a `.stemma` is checked by its SID, so places that all users may
      write, such as `C:\Users\Public`, no longer let another user plant a repository
      (STEMMA-73). Only the Windows run of the CI workflow compiles and tests it.
- [x] Removing a stale `.stemma-init-*` directory: the next writer that takes the lock removes
      it, if its name is exactly one that `init` makes (STEMMA-31).
- [ ] A configuration file, and the user-level default branch it would allow.
- [ ] On Unix, an empty `.stemma` that another process creates between the check in `init` and
      the rename is replaced, because `rename` replaces an empty directory. Closing it needs
      `renameat2(RENAME_NOREPLACE)` or a platform call with the same effect.
- [ ] The authorizer cannot give a reason for a denial (ADR-002); `init` reports only that it is
      not allowed. Phase 4, with the policy model (ADR-004).
- [ ] Bare repositories (no working copy) for a hub (phase 5).
