# ADR-010: The working copy is a commit

**Status:** Proposal, partially confirmed. Decisions marked *(confirmed)* were agreed with the
author on 2026-10-19; the others are proposals.

## Context

Git keeps three states: the working copy, the index as a staging area, and the last commit. What
an agent has not staged and committed is not in the history, so a crash, a wrong `checkout`, or
an overwritten file loses it without a trace. The goal of Stemma is that no state of an agent is
lost *(confirmed)*. The index of `crates/stemma-core/src/index.rs` is built as a data model
(`stemma-index 1`), but nothing depends on it yet: there is no `add`, `commit`, or `status`.

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
  one. The name of the command is a proposal. This replaces `stemma add` and keeps its purpose:
  a new file never enters the history by accident.
- **An exclusion file** *(confirmed)*. `.stemmaignore` lists paths that are never tracked, not
  even by `stemma track`, and are not reported as untracked. Its syntax is open, and it is
  strict like the other readers: what the writer would not have written is rejected.
- **`commit` names the working-copy commit and starts a new one** *(proposal)*. The current
  working-copy commit gets its message, and its change keeps its ID. A new, empty working-copy
  commit starts on top of it. HEAD keeps naming a branch (ADR-008); the working-copy commit is a
  child of the commit of that branch.
- **The tracked set is the tree of the working-copy commit** *(proposal)*. There is no index file
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
- **What a snapshot does** *(implemented, STEMMA-22)*. `Repository::snapshot` reads the tree of
  the working-copy commit, reads each file in it from the working directory, and builds a new
  tree. Only tracked files are looked at: a file that changed is saved, a file that is gone (or
  that a directory has replaced) leaves the tree, and a file that is not in the tree is not
  touched. The new commit keeps the parents, the change ID, and the message of the old one; its
  author is the actor of the snapshot and its time is now. If the tree is the same, there is no
  commit and no entry. A path is never followed through a link: a tracked file below a directory
  that is now a link counts as gone. A tracked path that is a special file stops the snapshot
  with an error instead of dropping it from the history. A link is saved as its target, and the
  executable bit is a part of the file mode where the file system has one.

## Consequences

- **The format of `commit` changes in place** *(confirmed)*. The field `change` is new. ADR-005
  says an incompatible format gets a new object name, and makes one exception: no repository holds
  a commit, so `commit` is changed in place and the golden vectors were computed again. The
  exception ends with the first commit that a command writes.
- **`stemma add` (STEMMA-2) becomes `stemma track`.** `stemma commit` (STEMMA-3) names the
  working-copy commit and starts a new one. `stemma status` (STEMMA-4) compares the working
  directory with the working-copy commit and lists the untracked files.
- **A secret that was tracked is in the history and in the log.** The exclusion file and the
  confirmation of new files make this rare, but not impossible: a tracked `config` can gain a
  key. Removing a secret from the log is an operation of its own (see the open items).
- **Conflicts can be data in a commit.** This model allows it, because every state is a commit.
  It does not decide it; the conflict model stays with phase 3 (STEMMA-19).
- **The working copy has an owner.** A snapshot has an actor. For an agent that shares a
  directory with others, the lease of ADR-004 has to cover the working-copy commit too.

## Open items

- [ ] `Commit::rewrite`, which takes the change ID from the commit it replaces, and a struct of
      named parts instead of six positional arguments of `Commit::new`. They belong with the first
      command that creates commits (STEMMA-3), because that is the first real caller.
- [ ] What a rebase does with the change ID of a commit that has two parents, and what a merge
      does when the working-copy commit has one (phase 3).

- [ ] The syntax of `.stemmaignore`: patterns, negation, and whether it is itself tracked and
      which tier of ADR-004 it belongs to.
- [ ] How long the operation log keeps the replaced commits of many snapshots (retention). The
      format and what an entry holds are decided in ADR-011.
- [ ] Removing a secret from the history and from the log: a purge that rewrites the log and
      lets garbage collection delete what only the log held.
- [ ] Who the actor of a snapshot is when two actors use one directory, and how the lease of
      ADR-004 applies.
- [ ] The cost of a snapshot for a large working directory: reading every file at every
      command. It is twice the tracked files today, because `Index::write_tree` verifies the
      blobs again. A cache keyed by modification time and size is the usual answer and a risk for
      correctness; decide with a measurement.
- [ ] Files that change during a snapshot. Each file is read once and its blob is made from what
      was read, so a file is consistent in itself, but the files together are not one instant.
      Special files are refused; whether a link that is not UTF-8 may be saved is open.
- [ ] A second working copy (a worktree) for a second agent in one repository (phase 3).
- [ ] The name `track`, and whether `stemma commit` needs a flag that makes a snapshot and a
      commit in one step for a script.
