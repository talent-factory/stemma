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
  `refs/heads/`, `refs.lock` (while a command writes), and `refs.lock.guard` (the guard of the
  lock, which stays; ADR-008), `purge-pending` (the journal of a purge, only while one runs or after
  it stopped, ADR-014), `derived/` (the cache of derived data, optional, ADR-017), and `tmp/`
  (the temporary files of the object store, the references, the log, and the purge).
- **The format file** *(confirmed)*. `.stemma/format` holds `stemma-repository <version>` and a
  newline, and nothing else. The version is decimal, without a leading zero. The reader accepts
  what the writer writes and nothing else, as for objects (ADR-005). `Repository::open` rejects a
  missing file, a file that is not in canonical form, and any version other than the ones it
  supports: `init` writes version 4, the version of the hash chain of the log (ADR-011), and
  `open` reads versions 2 to 4: a repository made before the purge existed (2) or before the
  chain (3) rises to 4 with its next entry or purge (ADR-014, ADR-011). A version never sinks,
  with one exception: if the directory cannot be flushed after the rise, the old content is put
  back, so that the next entry or purge raises the version again and flushes it. Nothing has been
  written in the new version at that point, because the entry or purge stops at the error.
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
  the message says to run the command as the owner. On Windows, where `std` gives no owner, a
  `.stemma` above `start` must not be at the root of a volume, where every user may create
  directories (`AtVolumeRoot`); one at `start` is opened there, as `-C` reaches the root of a
  volume only when it names it, and the message gives that call.
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

- [ ] Hide the object store too, if an operation needs to be sure that it is the one of this
      repository; today `objects()` is safe, because anything stored through it belongs to this
      repository, and `update_branch` and `set_head` use it themselves.
- [x] Finding the repository from a subdirectory: `Repository::discover` (STEMMA-72), see
      Consequences.
- [x] The owner of a `.stemma` at the start: checked on Unix as well, so `cd /tmp` does not
      open a `/tmp/.stemma` of another user (STEMMA-77), see Consequences.
- [ ] The owner is checked on the path, before `open` and, at the start, after it too, and not
      on a handle that `open` holds. A `.stemma` that appears or is swapped between the checks
      and the open is therefore read before it is refused, and a `format` that is a FIFO would
      block that read. Closing it needs `O_NOFOLLOW`, the owner of that handle (`fstat`), and a
      `read_format` that refuses anything but a regular file (STEMMA-88).
- [ ] A list of trusted directories, as Git's `safe.directory`, for a repository that several
      users share on one machine; it needs the configuration file below.
- [ ] On Windows only the root of a volume is refused; other places that all users may write,
      such as `C:\Users\Public`, stay open until the owner is checked by its SID (STEMMA-73).
- [x] Removing a stale `.stemma-init-*` directory: the next writer that takes the lock removes
      it, if its name is exactly one that `init` makes (STEMMA-31).
- [ ] A configuration file, and the user-level default branch it would allow.
- [ ] On Unix, an empty `.stemma` that another process creates between the check in `init` and
      the rename is replaced, because `rename` replaces an empty directory. Closing it needs
      `renameat2(RENAME_NOREPLACE)` or a platform call with the same effect.
- [ ] The authorizer cannot give a reason for a denial (ADR-002); `init` reports only that it is
      not allowed. Phase 4, with the policy model (ADR-004).
- [ ] Bare repositories (no working copy) for a hub (phase 5).
