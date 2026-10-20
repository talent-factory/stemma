# ADR-009: Repository layout and `init`

**Status:** Proposal, partially confirmed. Decisions marked *(confirmed)* were agreed with the
author on 2026-10-19; the others are proposals.

## Context

The object store (ADR-007) and the references (ADR-008) each take a root directory and do not
check it. Nothing ties the two to one repository: `update_branch` accepts the object store of any
repository, and nothing says which directory is a repository at all. `stemma init` needs a place
where the layout, the format version, and the creation of a repository are defined once.
Implementation: `crates/stemma-core/src/repository.rs` and, for the command, `crates/stemma-cli/src/cli.rs`.

## Decision

- **One type bundles the stores** *(confirmed)*. `Repository` in `stemma-core` holds the
  directory, an `FsObjectStore`, an `FsRefStore`, and an `FsOpLog` (ADR-011). The command line only calls it. Later
  operations (`add`, `commit`) are methods of `Repository`, so they use the stores of one
  repository.
- **Layout** *(confirmed)*. A repository is a directory that contains `.stemma/`. Inside:
  `format`, `HEAD`, `working-copy` (ADR-010), `objects/` (ADR-007), `oplog/` and `oplog-tip` (ADR-011),
  `refs/heads/` (ADR-008), and `tmp/` (the temporary files of the object store, the references,
  and the log).
- **The format file** *(confirmed)*. `.stemma/format` holds `stemma-repository <version>` and a
  newline, and nothing else. The version is decimal, without a leading zero. The reader accepts
  what the writer writes and nothing else, as for objects (ADR-005). `Repository::open` rejects a
  missing file, a file that is not in canonical form, and any version other than the one it
  supports. The version stays text, so a number larger than any integer is an unsupported
  version and not a damaged file. The file is read up to 64 bytes. The hash function needs no
  entry, because every ID carries its algorithm (ADR-001). `open` checks nothing but `format`
  (and that `.stemma` is a directory); the stores report what is wrong with the rest when they
  are used.
- **`init` is atomic**. The directory is built under a temporary name next to its final place
  and moved into place with one rename, so that a failure leaves no half-made repository. The
  name is `.stemma-init-<pid>-<n>-<random>`; `init` creates it itself, tries the next name if
  one is taken, and removes only the directory that it created. If removal fails, the error
  names the directory (`Leftover`). If `.stemma` exists, even an empty one or a link, `init`
  fails before it creates the temporary directory and changes nothing but missing parents. A
  rename that fails while `.stemma` exists is reported as `AlreadyExists`, so the loser of two
  concurrent `init`s sees that error.
- **Durability**. The files, the new directories, missing parent directories, and the directory
  that holds `.stemma` are flushed. If only the last flush fails, the repository is complete and
  the error is `NotDurable`, not a plain I/O error, so that a retry does not meet an
  unexplained `AlreadyExists`.
- **The initial branch** *(confirmed)*. `init` takes it as an option (`InitOptions`), and `main`
  is the default. `stemma init` offers it as `--initial-branch`. There is no configuration file in
  phase 1. The name goes through `BranchName`, so an invalid name stops `init` before anything is
  created; on the command line the argument parser rejects it with exit code 2.
  HEAD points to the branch with `branch <name>`; the branch does not exist until the first
  commit (an unborn branch, as ADR-008 allows).
- **`init` has an actor and an authorizer** *(confirmed)*. `Repository::init(path, options, actor,
  authorizer)` calls `authorize(actor, "init", <directory>)` first (ADR-002). No policy can live
  in a repository that does not exist yet, so the caller supplies the authorizer; in phase 1 it
  is `AllowAll`. `init` records the actor in the first entry of the operation log (ADR-011); the actor is not stored in the format file or in HEAD.
- **The directory is created if it does not exist**, as `git init` does.

## Consequences

- `open` is the place that checks the format. The two stores stay unchecked on construction, as
  ADR-007 and ADR-008 describe; the check in `open` closes the open item of ADR-007.
- A new format version needs an ADR, a new number in `FORMAT_VERSION`, and a decision about the
  migration of existing repositories. An older program refuses a newer repository.
- A process that is killed during `init` can leave a `.stemma-init-…` directory. A later `add`
  must not treat it as content of the working copy (ADR-005 reserves `.stemma` and its aliases;
  this name is not one of them).
- `Repository` hands out the object store, but the references only as a view for reading
  (`Repository::refs()`, ADR-011). A pointer moves through `Repository::update_branch` and
  `set_head`, which use the object store of their own repository, so the gap that the context
  names is closed for those two operations. The operations that need both (`commit`) will be methods of `Repository`, and the stores
  can become crate-private then; see the open items.

## Open items

- [ ] Hide the object store too, if an operation needs to be sure that it is the one of this
      repository; today `objects()` is safe, because anything stored through it belongs to this
      repository, and `update_branch` and `set_head` use it themselves.
- [ ] Finding the repository from a subdirectory (searching upwards for `.stemma`), which `add`,
      `commit`, and `status` need.
- [ ] Removing a stale `.stemma-init-*` directory, by `init` or by a repair command.
- [ ] A configuration file, and the user-level default branch it would allow.
- [ ] On Unix, an empty `.stemma` that another process creates between the check in `init` and
      the rename is replaced, because `rename` replaces an empty directory. Closing it needs
      `renameat2(RENAME_NOREPLACE)` or a platform call with the same effect.
- [ ] The authorizer cannot give a reason for a denial (ADR-002); `init` reports only that it is
      not allowed.
- [ ] Bare repositories (no working copy) for a hub (phase 5).
