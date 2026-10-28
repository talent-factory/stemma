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

Every command that changes the working copy, and `status` and `diff`, first saves the tracked
files as a snapshot (ADR-010). There is no file watcher and no background process. A state that
no command has seen is not saved. So the way to keep an intermediate state is to run a command.

## Setup (once per session)

1. Set the actor. It is required, and there is no default (ADR-002):

   ```sh
   export STEMMA_ACTOR="agent <id> model=<model> session=<session> sponsor=<name>"
   ```

   An agent never acts as `human ...`. An agent may not run `purge` at all.
2. Read the state: `stemma status --json`. Look at `clean`, `changes`, `untracked`, and
   `leftovers`. A `leftovers` entry is a `.stemma-checkout-...` file of an interrupted restore;
   report it, do not delete it.
3. Note the number of the newest log entry: `stemma op log -n 1 --json`. It is the point to
   go back to if the task fails (`op restore`, see below).
4. Optional: `stemma op verify` checks the whole log. It needs no actor. Keep the line
   `anchor N:hash` outside the repository, for example in the hand-over text, so that a later
   `op verify --anchor N:hash` can show that the log was not rewritten.

## While working

- A new file is not saved until it is tracked: `stemma track <path>...`. This is deliberate, so
  that a file never enters the history by accident.
- After each finished step, and after any risky change such as a large rewrite or a deletion, run
  `stemma status --json`. It is the cheapest way to save the state.
- Before a commit, read your own change: `stemma diff`. With no argument it compares the
  working-copy commit with its parent. `stemma diff --exit-code` ends with 4 if a file differs;
  that is not an error. `diff` has no JSON form yet, so read it as text.
- Commit a logical unit with `stemma commit -m "<message>" --json`. The message needs a
  character that is not white space.

## At the end

1. `stemma status --json`. Expected: `clean` is `true`. Explain or track what `untracked` lists.
2. `stemma commit -m "<message>" --json` if something is still open. "Nothing to commit" ends
   with exit code 1; a loop that commits treats it as a normal result, not as a failure.
3. For the hand-over, name the first and the last log entry of the session
   (`stemma op log -n <N> --json`) and the `anchor` line of `stemma op verify`.

## Exit codes (ADR-018)

Read the exit code, not the text of a message. Only `--json`, the `anchor` line and the exit
codes are a contract.

| Code | Meaning | What the agent does |
| ---- | ------- | ------------------- |
| 0 | Done | Continue. |
| 1 | Failed (no actor, nothing to commit, moved branch, no repository, ...) | Read the message; fix the cause. |
| 2 | Wrong arguments | Fix the call. |
| 3 | A result exists but is incomplete or not durable | Do not repeat blindly. The section of the command in the reference says what to do. |
| 4 | Only `diff --exit-code`: a file differs | Not an error. |
| 5 | The command found damage | Do not repeat. Stop and call the human. |
| 6 | A lock is held | Wait, try a few times, then report. |
| 129, 130, 143 | Interrupted | A repeat is safe, except after an interrupted `purge --apply`. |

With `--json`, a failure writes no JSON. An object before a non-zero code says what was made,
not that the command worked.

## Only on the instruction of the human

- `stemma op restore <N>`. It rewrites the working directory. Run `--dry-run` first. In this
  repository it also changes files that Git owns, so ask first.
- `stemma purge` of any kind. It is for humans only (ADR-002, ADR-014).

## Until phase 3: no `stemma` in a Git worktree

An actor needs its own working copy (ADR-010), and the per-agent branch in its own workspace
that the design foresees does not exist yet. Until it does, the rule is administrative:

- **One agent per directory.** Two agents in one directory is not supported, and the program
  does not check it. A held lock only shows up as exit code 6.
- **In a Git worktree, do not call `stemma`.** Two things speak against it, both tested on a copy
  of the layout (a root with `.stemma` and a worktree at `.worktrees/task`):
  - Without a `stemma init` of its own, a command in the worktree opens the `.stemma` of the
    root, because it searches the parent directories. The files of the worktree then appear as
    untracked paths such as `.worktrees/task/a.txt`, and a `track` or `commit` would write them
    into the history of the root.
  - With a `stemma init` of its own, the worktree is an independent repository, but its history
    is lost with it. Git does not carry the ignored `.stemma/` into a merge, Stemma has no merge
    before phase 3, and `git worktree remove` deletes the directory with the `.stemma` in it,
    without `--force` and without a message. Only the content of the files survives, through
    the Git merge.
- Never call `stemma` with `-C` pointing at the root from a worktree.
- `.worktrees/` is in `.stemmaignore` of this repository, so that `track .` in the root does not
  sweep up the files of the worktrees.

Whether a command should warn in a worktree, and how a workspace keeps its history after the
merge, is decided in phase 3 (STEMMA-132).

## Not yet there

Phase 3 brings branches, `switch`, and merge; phase 4 the agent identities and pull requests;
phase 6 the API and the MCP server, with `--json` for `diff`, `restore`, and `purge`.
This file changes with each of them.
