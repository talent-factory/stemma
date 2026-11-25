# ADR-009: Repository layout and `init`

**Status:** Accepted. The layout, the format file, the initial branch, the actor of `init`, and
the bundle of the stores were confirmed by the author on 2026-10-19, the other decisions on
2026-10-24 (STEMMA-9); all are implemented in Phase 1, and the linked worktrees of Phase 3
(STEMMA-175, STEMMA-176) came on top. The reason for a denial moves to Phase 4, bare
repositories to Phase 5; the other open items stay open, among them `worktree repair`,
`--json` for `worktree list`, and the undo of `worktree remove`. The additions of Phase 5 to the
layout were decided on 2026-11-23 and 2026-11-24 (see "The layout in Phase 5"). Of them, the
configuration of the remotes and the remote references (STEMMA-285, format 14), the bare
repository (STEMMA-284, format 15), and the journal of a running fetch with the directory of a
receiving operation in `tmp/` (STEMMA-289) are implemented; the others are not yet.

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
  `leases/` (one file per branch with a lease, made by the first lease; ADR-004, ADR-011),
  `refs/heads/`, `refs/tags/` (made by the first tag, ADR-008), `refs/remotes/` (the remote
  references, ADR-008) and `remotes/` (the configuration of the remotes, one file per remote; see
  "The layout in Phase 5"), `refs.lock` (while a command
  writes), and `refs.lock.guard` (the guard of the
  lock, which stays; ADR-008), `purge-pending` (the journal of a purge, only while one runs or after
  it stopped, ADR-014), `fetch-pending` (the journal of a fetch, only while one runs or after it
  stopped, ADR-023), `derived/` (the cache of derived data, optional, ADR-017), `worktrees/`
  (one state directory for each linked worktree, made by the first `worktree add`; see below),
  and `tmp/` (the temporary files of the object store, the references, the log, and the purge,
  and one directory `receive-<pid>-<n>-<random>/` for each receiving operation that runs,
  ADR-023).
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
  If another actor owns such a worktree, `status --take-over` cannot run in the missing directory,
  so `worktree remove --take-over` removes the state (STEMMA-217): only a human may (level 3 of
  ADR-004, the authorizer is asked for `take-over` as well), the owner does not change, and the
  entry of `worktree-remove` names the former owner in the line `former-owner` (ADR-011). The
  option does nothing while the directory exists: there the files are saved first, and the owner
  or `status --take-over` decides.
  A failure after the snapshot that came first keeps the exit code of the failure and names the
  entry of the snapshot; it is code 3 only if the snapshot moved a pointer and stopped or its
  flush failed (ADR-018). The output lists the first 20 files that `--force` removed with the
  directory and counts the rest. A state directory below `.stemma/worktrees` whose name no
  worktree can have (the name of an entry or of the journal of a purge, ADR-011, ADR-014) is
  damage (exit code 5): every command that has to read all worktrees (`worktree list`, `purge`,
  a switch, a branch delete) names it and stops, and `worktree remove` of another worktree still
  works. A directory without `HEAD` is no worktree and is skipped, whatever its name; a `HEAD`
  that cannot be inspected is an error (exit code 1), not a missing file.
- **The format file** *(confirmed)*. `.stemma/format` holds `stemma-repository <version>` and a
  newline, and nothing else. The version is decimal, without a leading zero. The reader accepts
  what the writer writes and nothing else, as for objects (ADR-005). `Repository::open` rejects a
  missing file, a file that is not in canonical form, and any version other than the ones it
  supports: `init` and `init_bare` write version 15, the version of the bare repository
  (STEMMA-284, see "A bare repository"), and `open` reads the versions 2 to 15.
  A repository made before the purge existed (2), before the chain
  (3), before the owner (4), before the worktree of an entry (5), before the tags of a purge (6),
  before the leases (7, the entries `lease-acquire`, `lease-release`, `lease-takeover`, and
  `lease-override` and the directory `leases/`; ADR-004, ADR-011), before the pull requests (8, the
  entries `pr-open`, `pr-close`, and `pr-merge`; ADR-004, ADR-011), before the approvals (9, the
  entry `pr-approve`; ADR-004, ADR-011), before the checks (10), before the kind in the stub
  (11), before the former owner of a worktree (12), before the remotes (13), or before the bare
  repository (14) rises to 15 with its next entry or purge (ADR-014, ADR-011, ADR-010).
  Version 14 adds files and changes none (the configuration of the remotes and the remote
  references, STEMMA-285): a program that knows only version 13 would not count a remote
  reference as a root, and its purge could remove what the reference names, so it has to refuse
  the repository.
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

- **The layout in Phase 5** *(decided in STEMMA-56, STEMMA-269, STEMMA-270, STEMMA-272,
  STEMMA-273 and STEMMA-274; the first two items are implemented in STEMMA-285, the others not
  yet)*. A clone that knows a remote gets new files in
  `.stemma/`. Each slice that adds one raises `stemma-repository` by the rule above; the names and
  the strict forms are set by the slices.
  - The configuration of remotes: per remote its address, the ID of the repository on the hub
    (ADR-025), the pinned public key of the hub (ADR-026), an additional CA file (ADR-028), and
    whether a sign-in is needed (ADR-023).
    It is one file per remote, `remotes/<remote>` *(STEMMA-285)*, in the metadata directory that
    every worktree shares. The name of the file is the name of the remote, one component by the
    rules of a branch name (ADR-008). The file has six lines in a fixed order, each with a
    newline: `stemma-remote 1`, `address <address>`, `repository <ID>`, `hub-key <key>`,
    `ca-file <path>`, and `sign-in yes` or `sign-in no`. The address starts with `https://` or
    `http://` and goes on with printable ASCII characters without a space. It has no `@`, `?`,
    or `#` and does not end with `/`: every endpoint is the address and a name (ADR-023), one
    repository has one spelling, and no password lies in the file. It is kept as given,
    and that `http://` is accepted towards a loopback address only stays a rule of the client
    (ADR-028). The ID is 32 lowercase hexadecimal digits (ADR-027). The key is an Ed25519 public
    key in the OpenSSH format without a comment, or `-` as long as none is pinned. The path of
    the CA file is absolute by the rule of the platform, has no control character, and neither
    starts nor ends with a space, or `-` for none. It is absolute because every worktree shares
    the configuration, and a relative path would name another file from another directory. The
    reader accepts what the writer writes and nothing else (ADR-005), and an entry of the
    directory whose name is no name of a remote is damage, except one that starts with a dot.
    One file per remote follows the branches and the leases: a change replaces one
    file atomically, and two remotes do not touch each other. A repository without the directory
    knows no remote. The core reads the configuration (`Repository::remote`, `remotes`) and has
    the encoding; the commands that write it (`remote add`, `clone`, pinning a key again) come
    with their slices and their entries in the log.
  - `refs/remotes/<remote>/heads/` and `refs/remotes/<remote>/tags/`, the remote references
    (ADR-008). Their file and their readers are built *(STEMMA-285)*; `fetch` and `push` write
    them.
  - The ID of the log: random, created by `init` and `clone`; an existing repository gets it when
    it is first needed (ADR-026).
  - The set of statements: the signed statements that the clone made and those that a fetch
    brought (ADR-025).
  - The journal of a running fetch, which records the commits that arrived completely, as
    `purge-pending` does for a purge (ADR-023): the file `fetch-pending` (STEMMA-289). It lies
    in the metadata directory that the worktrees share, and a fetch holds a lock of the system
    on it. Unlike `purge-pending` it stops no other command and raises no version of the
    format: it names objects that no reference and no entry names, an older program does not
    read it, and a fetch that finds none starts from its references.
  - The namespace policy that the clone last fetched (ADR-027), and the last confirmation of an
    anchor (ADR-026); where they are kept is left to their slices.
- **A bare repository** *(decided in STEMMA-269 and STEMMA-272; implemented in STEMMA-284)*. A
  hub opens a bare `Repository`: the layout above without a working copy.
  - **Where it lies.** The directory of a bare repository is the metadata directory itself; there
    is no `.stemma/` inside it and no directory of a working copy around it. `discover` therefore
    never finds one, and no command of the command line works in one by accident.
  - **What it contains.** `format`, the marker `bare`, `objects/` (ADR-007), `refs/heads/`, and
    `tmp/`, with one directory `receive-<pid>-<n>-<random>/` in it for each upload that runs
    (ADR-023). `refs/tags/` (made by the first tag, ADR-008) and `refs.lock` with
    `refs.lock.guard` (while a writer holds the lock, and the guard stays) come with the first
    writer of a reference, which is the accepting operation of ADR-025; nothing that is built
    yet makes them. It has no `HEAD`, no `working-copy`
    (so no index, which is the tree of the working-copy commit, ADR-010), no `oplog/` and
    `oplog-tip` (ADR-011), no `leases/`, no `worktrees/`, no `purge-pending`, no
    `fetch-pending`, and no `derived/`.
    `tmp/` and `objects/` lie on one file system (ADR-007).
  - **The marker.** The file `bare` holds `stemma-bare` and a newline, and nothing else. It says
    which of the two layouts a metadata directory has, so that a missing `HEAD` is never the
    sign: `open` and `discover` refuse a `.stemma/` that holds it (`OpenError::Bare`), and
    `open_bare` refuses a directory with a `format` and without it (`OpenError::NotBare`). A
    marker that is no regular file or has another content is damage for both
    (`OpenError::MalformedBareMarker`, exit code 5 by ADR-018), because it must not decide either
    way. A linked worktree is refused as well if the directory that the worktrees share holds
    the marker.
  - **The version.** A bare repository is written in version 15 of the format. A program that
    knows only version 14 refuses it with the unsupported version and does not report the missing
    `HEAD` and log as damage. The files of a repository with a working copy do not change; it
    rises to 15 with its next entry or purge, as for every version. `open_bare` reads what
    `init_bare` writes: the marker beside a version before 15 is damage
    (`OpenError::BareFormat`).
  - **How it is created and opened.** `Repository::init_bare(path, actor, authorizer)` asks the
    authorizer for `init` with the directory as the target, as `init` does. `path` must not
    exist, not even as an empty directory; missing directories above it are created. The
    directory is built under a temporary name next to its place, flushed, and moved into place
    with one rename, and the directory above is flushed (`NotDurable` if only that fails). The
    temporary name is `.stemma-bare-<pid>-<n>-<random>` and not the one of `init`: a writer of a
    repository removes the `.stemma-init-…` directories next to its `.stemma` as stale, and a
    bare repository may be made in such a directory. As for `init`, an empty directory that
    appears at `path` between the check and the rename is replaced on Unix. Nothing records the
    actor: there is no log, and the one who holds the bare repository records who made it (on a
    hub the record beside it, ADR-025).
    `Repository::open_bare(path)` checks `format` and the marker and nothing else, checks no
    owner, and follows links, as `open` does. No command of the command line creates or opens a
    bare repository; both are calls of the library for a hub (ADR-022).
  - **What is refused.** An operation that needs what a bare repository lacks is refused with
    the typed error `RefError::Bare`, which names the directory, the operation, and what is
    lacking (`BareLacks`): the working copy or the operation log. It arrives inside the error of
    the operation (`SnapshotError::Refs`, `UpdateError::Refs`, and so on), is classified as an
    ordinary failure and not as a lock or damage (ADR-018), and comes first: before the
    authorizer is asked and before anything is read or written.
    The working copy is lacking for `snapshot`, `track`, `status`, `commit`, `diff`, the plan and
    the apply of a checkout, `switch`, `op restore`, `merge`, `rebase`, the rewrite of the
    working copy, `follow_main`, the three worktree operations, and the moves of `HEAD` and of
    the working-copy commit.
    The operation log is lacking for every other operation that writes an entry or reads the
    log: the
    moves of a branch and a tag (`update_branch`, `create_branch`, `delete_branch`,
    `rename_branch`, `create_tag`, `delete_tag`), the leases, the pull requests with their
    approvals, the checks and the bounds of the gate that read them, the merge gate, and the
    purge. A reference of a bare repository moves through the accepting operation of ADR-025,
    which writes the record of the hub in place of the log; until that slice no call of the core
    moves one.
    The reference store of a bare repository refuses every read and write of `HEAD` and of the
    working-copy commit itself with the same error, so code that is not behind a check of
    `Repository` neither takes the missing file for damage nor makes it.
  - **What works.** The object store, for objects from an upload as well as from a computation;
    reading the branches and the tags, and the remotes and the remote references, of which a
    bare repository has none; the verification of the references (`verify_refs`, which
    checks the branches and the tags, as there is no `HEAD` and no working-copy commit) and of
    trees and commits; the register of a commit (`Register::at_commit`) and the authorizer built
    from it; the policy of a base commit (`Repository::policy_of`, the public form of
    `policy_at`, which takes the mode and not a flag) and the level of a change
    (`level_of_change`); the escalation when tests are weakened; the tree merge
    (`merge_commits`, `merge_trees`); and the functions of the gate that judge given evidence
    (the modules `check`, `bounds`, and `pull`). All of them read the object store, the
    branches and the tags, or their arguments, take no lock, create no commit by themselves, and
    move no reference (ADR-025). `verify_refs` refuses while the journal of a purge exists, as
    in every repository; nothing makes one in a bare repository.
    The refusal is found by matching the error of the operation that was called: the chain of
    sources leads to it for some errors only. The message of such an error is the refusal
    alone, without a prefix that says a read or a write failed, and the field `operation` is
    wording for the message. `init_bare`
    reports a final flush that failed with the directory above the repository
    (`NotDurable`), and an entry of the log that code past the checks tries to write is
    refused once more (`OpLogError::Io` of the kind `Unsupported`).
    The mode of a repository (`Mode::of`) is read from the commit of `HEAD` and is refused; a
    hub reads the register of the tip of `main` (ADR-024). The step to the enforcing
    mode (`enter_enforcing`) writes the register file into the working copy and is refused too.
    `Repository::oplog()` is still handed out: reading through it fails with `OpLogError::Io`
    or `OpLogError::Missing`, as the directory does not exist, so a caller asks `is_bare` first.
  - **What lies beside it.** The record of ref updates, the statements and the anchors of a hub
    lie beside the bare repository, not inside it: the core knows their formats and writes them
    into a destination that the caller passes, so the layout gets no file that only a hub knows
    (ADR-025). The stores stay sealed, so a bare repository is written and read through
    `stemma-core` (ADR-022). The removal of unreachable objects is an operation of the core on a
    bare repository (ADR-007); it is not built yet.

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
- [x] The format is raised with the first entry that can carry the line `tags` of a `purge`
      entry: version 7 (STEMMA-185). A program that knows only version 6 refuses such a
      repository with the unsupported version and does not report damage for a log or a journal
      that it cannot read.
- [x] The format is raised with the configuration of the remotes and the remote references:
      version 14 (STEMMA-285). No command writes either yet, so a repository reaches the version
      with its next entry or purge, and a program that knows only version 13 refuses it with the
      unsupported version.
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
- [x] Bare repositories (no working copy) for a hub (phase 5): `init_bare`, `open_bare`, the
      marker `bare`, version 15 of the format, and the typed refusal `RefError::Bare`
      (STEMMA-284), see "A bare repository".
- [x] A writer that takes the lock of the references removes every file in `tmp/` that is not a
      directory (ADR-011), and a caller that stores objects without the lock is not protected.
      On a hub the upload runs without the lock (ADR-007, ADR-025). The two are kept apart
      (STEMMA-289): a receiving operation writes its temporary files into a directory of its
      own in `tmp/`, which the cleanup under the lock leaves, and a directory that a crashed
      operation left is told from one in use by the lock of the system on its file `guard`
      (ADR-023, "The receiving side").
- [ ] A process that is killed during `init_bare` leaves its `.stemma-bare-…` directory next to
      the place of the repository. No writer removes it, as one does for `init`, because the
      directory above a bare repository belongs to the one who holds it; a hub removes it.
- [ ] `open_bare` checks `format` and the marker only. A directory with the marker that also
      holds `HEAD`, `working-copy`, or `oplog/`, put together by hand, opens as bare and the
      files are ignored; refusing it costs three looks at the directory and a variant.
- [ ] Whether a bare repository stays the type `Repository` with a check at the start of each
      operation, or becomes a type of its own: to decide with the accepting operation
      (ADR-025). Until then a test reads the source and fails for a public operation without
      the check.
- [ ] `Repository::oplog()` and `Mode::of` do not give the typed refusal on a bare repository:
      the first is handed out and fails on reading with an error of the storage, the second
      carries the refusal as text (`ModeError::Head`). Both change with the slice that gives a
      hub its record and its mode (ADR-024, ADR-025).
- [ ] The mode of a bare repository: the marker of the enforcing mode lies in the
      metadata directory and `Mode::of` reads `HEAD`. How a hub states the mode, from the
      register at the tip of `main`, belongs to ADR-024 and its slice.
