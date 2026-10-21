# ADR-010: The working copy is a commit

**Status:** Accepted. The decisions marked *(confirmed)* were agreed with the author on
2026-10-19, `commit` and the tracked set on 2026-10-21, and the actor of a snapshot on
2026-10-21 (STEMMA-38). The open items at the end are limits that the first version has and
questions for later; none of them changes a decision above.

## Context

Git keeps three states: the working copy, the index as a staging area, and the last commit. What
an agent has not staged and committed is not in the history, so a crash, a wrong `checkout`, or
an overwritten file loses it without a trace. The goal of Stemma is that no state of an agent is
lost *(confirmed)*. The index of `crates/stemma-core/src/index.rs` is built as a data model
(`stemma-index 1`), but nothing depended on it yet when this ADR was written: there was no `add`,
`commit`, or `status`.

Jujutsu treats the working copy as a commit that every command updates, and the Anthropic
article on Scion (`anthropic2026redesigning`) works without a local staging area as well. Both
are prior art, not a template.

## Decision

- **The working copy is a commit** *(confirmed)*. A repository has a working-copy commit. The
  files in the directory are its tree. A command that changes files does not stage them first.
- **A snapshot at every command** *(confirmed)*. Before a command runs, Stemma builds the tree of
  the working directory. If it differs from the tree of the working-copy commit, a new
  working-copy commit replaces the old one, with the same parents and the same change ID. What
  changed between two commands is safe from the next call on. What an agent overwrites while no
  command runs is not seen. There is no file watcher and no background process in phase 1; a
  watcher can follow in a later phase.
- **The operation log keeps every replaced commit** *(confirmed, as a consequence)*. A snapshot
  makes the previous working-copy commit unreachable from every reference, so only the log keeps
  it. Without the log the guarantee is empty, so no snapshot is made before the operation log
  (ADR-005, STEMMA-5) exists. Garbage collection (ADR-007) treats the commits that the log names
  as roots.
- **Every commit has a change ID** *(confirmed)*. A snapshot gives a new commit a new ID, because
  the content differs. The change ID stays the same through every snapshot and rewrite, so that
  a review, a pull request (phase 4), and a rebase (phase 3) can name "the same change". The
  `Commit` type has the field `change`, written as the header `change <id>` (ADR-005).
- **The change ID is `ch:` and 32 lowercase hexadecimal digits** *(confirmed)*. It has 128 bits and
  comes from the random source of the operating system (the crate `getrandom`); the standard
  library has none meant for it. `ChangeId::generate` never falls back to a weaker source and
  reports a failure instead, because two changes that share an ID would be joined, and it refuses
  a source that returns only zeros. A rewrite takes the ID of the commit it replaces, a new change
  gets a new one, and **a merge commit gets a new change ID** *(confirmed)*: it joins two changes
  and continues neither. `Commit::new` takes the ID as an input and does not decide which case
  applies. A short form for people can be derived from the long one later, for display only.
- **Untracked files are not saved until they are confirmed** *(confirmed)*. A file that is not in
  the tree of the working-copy commit is *untracked*. A snapshot saves a tracked file that
  changed or was deleted. It does not save an untracked file; `stemma track <path>` confirms
  one. The name `track` is confirmed (STEMMA-2). This replaces `stemma add` and keeps its purpose:
  a new file never enters the history by accident.
- **An exclusion file** *(confirmed)*. `.stemmaignore` lists paths that are never tracked, not
  even by `stemma track`, and are not reported as untracked. Its syntax is open, and it is
  strict like the other readers: what the writer would not have written is rejected.
- **What `track` does** *(confirmed)*.
  `Repository::track` takes relative paths, builds the new tree from the tracked files and from
  the files at the paths, and replaces the working-copy commit like a snapshot does. A directory
  stands for the files below it, without a directory named `.stemma`. The tracked files are saved
  as well, because the new tree is built from the working directory and a track that kept the old
  blobs would drop a change. A path that is tracked already is not an error. A path that does not
  exist or breaks a path rule is refused before anything is written; a special file or a name that
  is not UTF-8 stops the call, as it stops a snapshot. A path spells every name as the directory
  has it: a file system that ignores case would take `a.txt` for `A.txt`, and the tree would hold
  the file twice, so the call refuses the other spelling and names the one on disk. A file that
  the caller named and that is gone when it is saved is an error (`NotFound`); a file found below
  a directory that is gone is left out. Below a directory, a name that resolves to `.stemma` is
  skipped without a word, since a tree cannot hold it (this skips a repository inside the
  directory as well), and any other name that a tree refuses stops the call (`InvalidName`).
  `.stemmaignore` (ADR-012) is read by `track`: an excluded file below a directory is skipped
  like those names, and an excluded path that the caller names is an error. A file whose name a
  tree refuses is skipped as well if a pattern excludes it, and it stops the call otherwise.
- **`commit` names the working-copy commit and starts a new one** *(confirmed, implemented in
  `Repository::commit`)*. The current working-copy commit gets its message, and its change keeps
  its ID. A new, empty working-copy commit starts on top of it. HEAD keeps naming a branch
  (ADR-008); the working-copy commit is a child of the commit of that branch. Details:
  - *Empty* means the same tree as the commit it follows. The tracked set is the tree, so a tree
    without the files would leave every file untracked.
  - `commit` takes a snapshot of the tracked files first, so that a change since the last command
    is part of the commit. The snapshot is an operation of its own and has its own entry.
  - The named commit `W'` is the working-copy commit `W` with the message, the actor, and the time
    now: the parents and the change ID are kept. The new working-copy commit `N` has `W'` as its
    only parent, the tree of `W'`, an empty message, and a new change ID.
  - Two pointers move in **one** entry of the log: the branch from its tip (or from nothing) to
    `W'`, and the working-copy commit from `W` to `N`. The branch moves first, so that a stop
    between the moves keeps `W'` reachable. If the second move fails, the error says that one
    pointer moved (`UpdateError::PartlyMoved`) and `op log` shows the entry as not taken (`!`),
    although the branch did move. The next `commit` finds the state (the tip of the branch has
    the parents and the change ID of the working-copy commit, and a message) and finishes the
    commit: it starts the new working-copy commit and moves that pointer alone, with an entry of
    its own. The message of that call is not used (`Committed::resumed`).
  - A commit is refused when the message is empty (there is no editor in phase 1), when HEAD
    names a commit and not a branch, when the tree is that of the parent (or empty, for a first
    commit), and when the branch is not at the parent of the working-copy commit: a commit on
    top of that parent would drop what the branch has. A rebase (phase 3) brings the two
    together.
- **`status` takes the snapshot and compares the commit with its parent** *(confirmed,
  implemented in `Repository::status` and `stemma status`)*. Like every command that does not only read the log,
  `status` saves the tracked files (so it has an actor, and the entry appears only if a tracked
  file changed). The snapshot makes every tracked file equal to the working-copy commit, so
  "changed" cannot mean "differs from the working-copy commit": it means what the working-copy
  commit does against its **parent**, as in Jujutsu. That is `A` (in the commit and not in the
  parent), `M` (another content or mode, also a link in place of a file), and `D` (in the parent
  and not in the commit), sorted by path, with the files of a directory that is added or deleted
  listed one by one, and a file that becomes a directory (or the reverse) as a `D` and the `A`
  files below it. A commit with no parent is compared with the empty tree, and one with several
  parents with the first. The **untracked** files are the files below the working directory that
  are not in the tree of the working-copy commit and that `.stemmaignore` does not exclude
  (ADR-012), listed one by one.
  - `status` reads the exclusion file and walks the working directory **before** the snapshot.
    It stops where `track` stops (an invalid or unreadable `.stemmaignore`, a name that a tree
    refuses and no pattern excludes, a name that is not UTF-8, which a pattern cannot exclude),
    and so it stops before it writes anything.
  - A failure after the snapshot (reading the references, the commits, or the trees) leaves the
    snapshot: the error says so and carries the entry (`StatusError::saved`). A snapshot that
    moved the pointer and could not flush it is a result with a flush error, as for `commit`,
    and the command ends with code 3 after it has written the status (`stemma status`). A
    failure of reading after a snapshot, and an output that cannot be written, end with code 1
    and name the entry of the snapshot; code 1 is the code of a command that only reads.
  - The references are read one after the other and not under a lock, so a process that moves a
    pointer in between can make HEAD and the working-copy commit come from two states.
  - `op log` still takes no snapshot, because the snapshot would change the log it is about to
    show. A tracked file that a pattern excludes stays in the changes; a report "tracked and
    excluded" is an open item of ADR-012.
- **The tracked set is the tree of the working-copy commit** *(confirmed)*. There is no index file
  as a staging area. `Index` stays in the code as the model that builds a tree from a set of
  paths and as the format of a cache, if one is needed for speed.
- **Where the working-copy commit is recorded** *(confirmed)*. A file `.stemma/working-copy`
  holds the ID of the working-copy commit, in the form of a branch file (ADR-008), replaced with
  the same compare-and-swap under the lock of the references. One working copy per repository
  directory in phase 1. A missing file is an error and not "no commit yet" (`RefError::NoWorkingCopy`).
- **`init` makes the first working-copy commit** *(confirmed)*. It has the empty tree, no parents,
  a change ID of its own, and the actor of `init` as its author. It goes into the first entry of
  the log together with HEAD, so a snapshot always replaces a commit that exists, and the log
  holds every working-copy commit from the start.
- **What a snapshot does** *(confirmed)*. `Repository::snapshot` reads the tree of
  the working-copy commit, reads each file in it from the working directory, and builds a new
  tree. Only tracked files are looked at: a file that changed is saved, a file that is gone (or
  that a directory has replaced) leaves the tree, and a file that is not in the tree is not
  touched. The new commit keeps the parents, the change ID, and the message of the old one; its
  author is the actor of the snapshot and its time is now. If the tree is the same, there is no
  commit and no entry. A path is never followed through a link: a tracked file below a directory
  that is now a link counts as gone. A tracked path that is a special file stops the snapshot
  with an error instead of dropping it from the history. A link is saved as its target, and the
  executable bit is a part of the file mode where the file system has one. A file is opened
  and compared by device and inode with the one that was looked at, so a file or a directory
  swapped for a link between the two is an error and not a read outside the working directory.
  A file that vanishes between the two counts as gone.

## Consequences

- **The format of `commit` changes in place** *(confirmed)*. The field `change` is new. ADR-005
  says an incompatible format gets a new object name, and makes one exception: no repository holds
  a commit, so `commit` is changed in place and the golden vectors were computed again. The
  exception ends with the first commit that a command writes.
- **`stemma add` (STEMMA-2) becomes `stemma track`.** `stemma commit` (STEMMA-3) names the
  working-copy commit and starts a new one. `status` (STEMMA-4) saves the tracked files,
  lists what the working-copy commit changes against its parent, and lists the untracked files.
- **A secret that was tracked is in the history and in the log.** The exclusion file and the
  confirmation of new files make this rare, but not impossible: a tracked `config` can gain a
  key. Removing a secret from the log is an operation of its own (see the open items).
- **Conflicts can be data in a commit.** This model allows it, because every state is a commit.
  It does not decide it; the conflict model stays with phase 3 (STEMMA-19).
- **A working copy belongs to one actor** *(confirmed)*. A snapshot is made by the actor of the
  command that takes it, and the log names that actor. A snapshot saves whatever is in the
  directory, so when two actors (a human and an agent, say) work in one directory, a change of
  one can be saved by a command of the other, and the log then names the actor who saved it,
  not the one who made the change. The rule is therefore: **every actor works in a working copy
  of its own**. Two actors in one directory are not a supported mode. The core does not enforce
  the rule yet: nothing records an owner in the file `working-copy`, and a command of another
  actor is not refused. Enforcement comes with the second working copy (a worktree, phase 3, see
  the open items), which needs a mechanism for owning a directory anyway. Until then the rule is
  a convention that the people and the agents who use a repository keep.
- **The lease of ADR-004 and the rule are independent.** ADR-004 proposes a lease on a branch,
  which a server would enforce: it governs who may move the branch. It does not govern who
  writes into a local directory, and a server cannot see that. The rule above is what covers
  the directory, and the working-copy commit has no lease of its own. Once the lease exists, an
  actor that holds the lease of a branch is the natural owner of the working copy that follows
  it; that link is part of the enforcement in the open items.

## Open items

- [x] `Commit::rewrite`, which takes the parents and the change ID from the commit it replaces
      (STEMMA-3).
- [ ] A struct of named parts instead of six positional arguments of `Commit::new`.
- [ ] A branch that someone moved to an older version of the working-copy commit that has a
      message looks like a `commit` that stopped, and the next `commit` finishes it.
- [ ] A commit with an empty tree as an allowed empty commit, a commit while HEAD names a commit,
      and a commit while the branch has moved (a rebase, phase 3) are refused today.
- [ ] What a rebase does with the change ID of a commit that has two parents, and what a merge
      does when the working-copy commit has one (phase 3).

- [ ] Which commands take a snapshot. `track`, `commit`, and `status` do. `op log` reads and
      takes no actor, so it takes none: a snapshot adds an entry and would change the log it is
      about to show. Decide for the other commands that read. The path `.` of `track` tracks
      every file of the repository directory, secrets included, unless `.stemmaignore` excludes
      them (ADR-012).
- [ ] The path form is a `String` that the core validates and `normalize_path` writes. A
      `RepoPath` type that cannot be built from an invalid path would make the rule one
      definition; do it with the next command that takes paths (`status` takes none).
- [ ] A command finds its repository in a parent directory when it runs below the root, as Git
      does. Today a command takes the directory (`-C`, or the argument of `op log`) and the paths
      of `track` are relative to it.
- [x] The syntax of `.stemmaignore`: patterns, negation, and whether it is itself tracked and
      which level of ADR-004 it belongs to. Decided in ADR-012 and built (STEMMA-24).
- [ ] How long the operation log keeps the replaced commits of many snapshots (retention). The
      format and what an entry holds are decided in ADR-011. Tracked in STEMMA-39.
- [ ] Removing a secret from the history and from the log: a purge that rewrites the commits,
      replaces log entries by stubs, and deletes what the rewrite replaced. Decided in ADR-014
      (STEMMA-26); built in STEMMA-46 to STEMMA-48.
- [x] Who the actor of a snapshot is when two actors use one directory, and how the lease of
      ADR-004 applies. Decided above (STEMMA-38): one actor per working copy, kept by convention.
- [ ] Enforcing that rule: an owner in the file `working-copy`, and a command of another actor
      that takes a snapshot is refused unless it takes the working copy over explicitly (one
      possible design). The file has the form of a branch file today, so an owner changes its
      format. Comes with the second working copy (phase 3).
- [x] The cost of a snapshot for a large working directory: reading every file at every
      command. It is twice the tracked files today, because `Index::write_tree` verifies the
      blobs again (the code points here and does not repeat it). Measured in STEMMA-37 (release
      build, macOS with APFS on an SSD, one run each, files of 1 KiB): a snapshot of an unchanged
      working directory costs about 70 microseconds per file (0.04 s for 100 files, 0.7 s for
      10,000, 3.3 s for 50,000), and a changed file adds no measurable time. Ten files of 100 MiB
      take 1.5 s. The first `track` is slower by two orders of magnitude, about 6.6 ms per file
      (66 s for 10,000 files), because every object is flushed. **Decision:** no cache keyed by
      modification time and size for now. A snapshot stays under one second up to about 10,000
      files, and the cache is a risk for correctness. Look at it again when repositories with more
      than 50,000 files are realistic, and measure on Linux and Windows first (STEMMA-33). The
      flush of the first `track` is tracked in STEMMA-40.
- [ ] Files that change during a snapshot. Each file is read once and its blob is made from what
      was read, so a file is consistent in itself, but the files together are not one instant.
      Special files are refused; whether a link that is not UTF-8 may be saved is open.
- [ ] A repository without the file `working-copy` (made before STEMMA-22, or the file removed). A
      snapshot stops with `NoWorkingCopy`, and no command makes the file again. Raising the format
      version would refuse such a repository at once; decide with the first command that needs it.
- [ ] A tracked path that is now a special file blocks every snapshot until it is gone, and a
      tracked directory that is now a link drops its files from the tree without a warning. Both
      want a way to report or to untrack (`stemma track`, STEMMA-2).
- [ ] A second working copy (a worktree) for a second agent in one repository (phase 3).
- [ ] Whether `stemma commit` needs a flag that makes a snapshot and a commit in one step for a
      script.
