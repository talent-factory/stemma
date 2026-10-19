# ADR-009: Repository layout and `init`

**Status:** Proposal, partially confirmed. Decisions marked *(confirmed)* were agreed with the
author on 2026-10-19; the others are proposals.

## Context

The object store (ADR-007) and the references (ADR-008) each take a root directory and do not
check it. Nothing ties the two to one repository: `update_branch` accepts the object store of any
repository, and nothing says which directory is a repository at all. `stemma init` needs a place
where the layout, the format version, and the creation of a repository are defined once.
Implementation: `crates/stemma-core/src/repository.rs`.

## Decision

- **One type bundles the stores** *(confirmed)*. `Repository` in `stemma-core` holds the
  directory, an `FsObjectStore`, and an `FsRefStore`. The command line only calls it. Later
  operations (`add`, `commit`) are methods of `Repository`, so they use the stores of one
  repository.
- **Layout** *(confirmed)*. A repository is a directory that contains `.stemma/`. Inside:
  `format`, `HEAD`, `objects/` (ADR-007), `refs/heads/` (ADR-008), and `tmp/` (the temporary files
  of the object store).
- **The format file** *(confirmed)*. `.stemma/format` holds `stemma-repository <version>` and a
  newline, and nothing else. The version is decimal, without a leading zero. The reader accepts
  what the writer writes and nothing else, as for objects (ADR-005). `Repository::open` rejects a
  missing file, a file that is not in canonical form, and any version other than the one it
  supports. The hash function needs no entry, because every ID carries its algorithm (ADR-001).
- **`init` is atomic**. The directory is built under the temporary name `.stemma-init-<pid>-<n>`
  next to its final place and renamed to `.stemma` at the end, so that a failure leaves no
  half-made repository. If `.stemma` exists, even an empty one, `init` fails before it creates
  anything and changes nothing.
- **The initial branch** *(confirmed)*. `init` takes it as an option; the command line offers
  `--initial-branch`, and `main` is the default. There is no configuration file in phase 1. The
  name goes through `BranchName`, so an invalid name stops `init` before anything is created.
  HEAD points to the branch with `branch <name>`; the branch does not exist until the first
  commit (an unborn branch, as ADR-008 allows).
- **`init` has an actor and an authorizer** *(confirmed)*. `Repository::init(path, options, actor,
  authorizer)` calls `authorize(actor, "init", <directory>)` first (ADR-002). No policy can live
  in a repository that does not exist yet, so the caller supplies the authorizer; in phase 1 it
  is `AllowAll`. The actor is not stored.
- **The directory is created if it does not exist**, as `git init` does.

## Consequences

- `open` is the place that checks the format. The two stores stay unchecked on construction, as
  ADR-007 and ADR-008 describe; the check in `open` closes the open item of ADR-007.
- A new format version needs an ADR, a new number in `FORMAT_VERSION`, and a decision about the
  migration of existing repositories. An older program refuses a newer repository.
- A process that is killed during `init` can leave `.stemma-init-<pid>-<n>`. A later `add` must
  not treat it as content of the working copy (ADR-005 reserves `.stemma` and its aliases; this
  name is not one of them).

## Open items

- [ ] Finding the repository from a subdirectory (searching upwards for `.stemma`), which `add`,
      `commit`, and `status` need.
- [ ] Removing a stale `.stemma-init-*` directory, by `init` or by a repair command.
- [ ] The place of the actor of the command line (a flag, an environment variable, or a
      configuration file) is open for `add` and `commit`.
- [ ] A configuration file, and the user-level default branch it would allow.
- [ ] Two processes that run `init` in one directory at the same moment can see an I/O error
      instead of `AlreadyExists`; a rename that fails if the target exists exists only on some
      platforms.
- [ ] Bare repositories (no working copy) for a hub (phase 5).
