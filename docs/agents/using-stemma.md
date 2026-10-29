# Using Stemma as an agent

How an agent calls `stemma` during a task: at the start, in between, and at the end.
The command line reference (`docs/chapters/12-cli-reference.tex`) is the source for what each
command does and how it fails. This file says which commands to call, and when.

It describes the state of phase 2. Branches and workspaces per agent come with phase 3 (see
"Until phase 3" below); update this file when they land.

## Which repository this is for

- **In this repository**, Git stays the single source of truth: commits, branches and pull
  requests run through Git (see `CLAUDE.md`). Stemma runs next to it as the example of the book,
  so the history is double on purpose. Call `stemma` here only when the human asks for it.
- **In any other repository** that uses Stemma, the rules below apply as written.

## The rule behind all of it

`track`, `commit`, `status`, `op restore`, and `purge --apply` first save the tracked files as a
snapshot (ADR-010), and so does `diff` when one side is the working copy. The snapshot covers
tracked files only, and it writes a log entry only if a tracked file changed. There is no file
watcher and no background process. A state that no command has seen is not saved, and a command
that fails before its snapshot saves nothing. So the way to keep an intermediate state is to run
a command.

## Setup (once per session)

1. Set the actor. It is required, and there is no default (ADR-002):

   ```sh
   export STEMMA_ACTOR="agent <id> model=<model> session=<session> sponsor=<name>"
   ```

   An agent never acts as `human ...`.
   The working copy belongs to one actor, its owner (ADR-010; the second line of
   `.stemma/working-copy`). A command of another actor that would take a snapshot fails with
   exit code 1 and saves nothing. If that happens, stop and report it: the directory is the
   working copy of someone else. The program refuses `--take-over` from an agent (level 3,
   ADR-004), so only the human can give the working copy to you.
2. Read the state: `stemma status --json`. Look at `clean`, `changes`, `untracked`, and
   `leftovers`. A `leftovers` entry is a `.stemma-checkout-...` file of an interrupted restore;
   report it, do not delete it. If a purge is pending (the file `.stemma/purge-pending`),
   `track`, `commit`, `status`, and `op restore` fail with exit code 1: stop and report it.
3. Note the number `N` of the newest log entry: `stemma op log -n 1 --json`; the field is
   `number`. `op restore` puts back the state *before* the entry it names, so the first entry
   that the task writes, `N+1`, is the point to go back to (`op restore N+1`, see below).
4. Optional: `stemma op verify` checks the whole log. It needs no actor. Keep the line
   `anchor <number>:<hash>` (if there is one) outside the repository, for example in the
   hand-over text, so that a later `op verify --anchor <number>:<hash>` can show that the log
   was not rewritten. The hash starts with `b3:` itself.

## While working

- A new file is not saved until it is tracked: `stemma track <path>...`. This is deliberate, so
  that a file never enters the history by accident. Name the files; `track .` takes everything
  that `.stemmaignore` does not exclude, secrets included, and only a human can purge them.
- After each finished step, and after any risky change such as a large rewrite or a deletion, run
  `stemma status --json`. It is the cheapest way to save the state.
- Before a commit, read your own change: `stemma diff`. With no argument it compares the
  working-copy commit with its parent. `stemma diff --exit-code` ends with 4 if a file differs;
  that is not an error. `diff` has no JSON form yet, so read it as text. Do not use `--tui`:
  it needs a terminal and ends with exit code 1 without one.
- Commit a logical unit with `stemma commit -m "<message>" --json`. The message needs a
  character that is not white space.

## At the end

1. `stemma status --json`. Expected: `clean` is `true`. Explain or track what `untracked` lists.
2. If `changes` is not empty, `stemma commit -m "<message>" --json`. If it is empty there is
   nothing to commit, and `commit` would end with exit code 1, the same code as a missing actor
   or a moved branch. Skip the call; do not read the message to tell the cases apart.
3. For the hand-over, name the first entry that the task wrote (`N+1`), the last one
   (`stemma op log -n <count> --json`), and the `anchor` line of `stemma op verify`.

## Exit codes (ADR-018)

Read the exit code, not the text of a message. Only `--json`, the `anchor` line and the exit
codes are a contract. The message of a failure helps a person find the cause; do not match on it.

| Code | Meaning | What the agent does |
| ---- | ------- | ------------------- |
| 0 | Done | Continue. |
| 1 | Failed (no actor, nothing to commit, moved branch, no repository, ...) | Fix the cause; a person can read the message. |
| 2 | Wrong arguments | Fix the call. |
| 3 | A result exists but is incomplete or not durable | Do not repeat blindly; see below. |
| 4 | Only `diff --exit-code`: a file differs | Not an error. |
| 5 | The command found damage | Do not repeat. Stop and call the human. |
| 6 | A lock is held | Wait, try a few times, then report. A lock of another machine or in an older form is never taken over, so it stays: report it. |
| 129, 130, 143 | Interrupted | A repeat is safe, except after an interrupted `purge --apply`. |

Exit code 3, by case (the table `tab:exit3` of the reference has all of them):

- `commit`, and the snapshot before it moved a pointer but could not save it: the commit is not
  made, so repeat the call.
- `commit`, and only the branch moved: call `commit` again. It finishes the first commit and
  does not use the new message; `resumed` is `true` in the JSON.
- `op restore` that says `Half made`: repeat the call.
- A result that is made, but the output could not be written or a directory is not saved:
  do not repeat.

With `--json`, a failure writes no JSON. An object before a non-zero code says what was made,
not that the command worked.

## Only on the instruction of the human

- `stemma op restore <N>`. It rewrites the working directory. Run `--dry-run` first. In this
  repository it also changes files that Git owns, so ask first.

## Never

- `stemma purge`, not even the plan without `--apply`. The program refuses an agent at every
  step (ADR-002, ADR-014). If a secret is in the history, report it to the human, who runs the
  purge.
- `--take-over`. The program refuses it from an agent (ADR-004, ADR-010). If the working copy
  belongs to someone else, report it to the human.

## Until phase 3: no `stemma` in a Git worktree

An actor needs its own working copy (ADR-010), and the per-agent branch in its own workspace
that the design foresees does not exist yet. Until it does, the rule is administrative:

- **One agent per directory.** Two actors in one directory are not supported. Since format 5
  the program refuses a snapshot of an actor who does not own the working copy (exit code 1),
  so a second agent finds out at its first command; a held lock still shows up as exit code 6.
- **In a Git worktree, do not call `stemma`.** Two things speak against it, both tested on a copy
  of the layout (a root with `.stemma` and a worktree at `.worktrees/task`):
  - Without a `stemma init` of its own, a command in the worktree opens the `.stemma` of the
    root, because it searches the parent directories. Its snapshot then covers the files of the
    root, under the actor of the worktree: two actors in one working copy. In a repository whose
    `.stemmaignore` lacks the directory of the worktree, the files of the worktree also appear
    as untracked paths such as `.worktrees/task/a.txt`, and a `track` or `commit` would write
    them into the history of the root.
  - With a `stemma init` of its own, the worktree is an independent repository, but its history
    is lost with it. Git does not carry the ignored `.stemma/` into a merge, and Stemma has no
    merge before phase 3. `git worktree remove` deletes the directory with the `.stemma` in it,
    and on a clean worktree, which is the normal state after the merge, it does so without
    `--force` and without a message. Only the content of the files survives, through the Git
    merge.
- Never call `stemma` with `-C` pointing at the root from a worktree.
- Commands for examples of the book (`docs/agents/book-review.md`) run in the checkout of the
  root or in a scratch repository, not in the worktree of the review.
- `.worktrees/` and `.claude/` are in `.stemmaignore` of this repository, so that `track .` in
  the root does not sweep up the files of the worktrees (Claude Code creates its worktrees under
  `.claude/worktrees/`).

A command in a Git worktree or a nested clone below the root warns on stderr
(`stemma: warning: ... lies in the Git checkout ...`, STEMMA-132) and still runs, with the exit
code of the command. The warning is a sign to stop, not a permission to continue. After a
`stemma init` in the worktree there is no warning, because the repository is found at the start;
that is the second case above, and nothing protects the history then. How a workspace keeps its
history after the merge is decided in phase 3.

## Not yet there

Phase 3 brings branches, `switch`, and merge; phase 4 the agent identities and pull requests;
phase 6 the API and the MCP server, with `--json` for `diff`, `restore`, and `purge`.
This file changes with each of them.
