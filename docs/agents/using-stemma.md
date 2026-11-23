# Using Stemma as an agent

How an agent calls `stemma` during a task: at the start, in between, and at the end.
The command line reference (`docs/chapters/12-cli-reference.tex`) is the source for what each
command does and how it fails. This file says which commands to call, and when.

It describes the state of phase 4: the commands of phase 3 (branches, `switch`, `merge`, `rebase`,
a second working copy per actor with `stemma worktree add`) and the commands of collaboration
(`enforce`, `register`, `lease`, `pr`, `check`). Update this file when a later phase lands.

## Which repository this is for

- **In this repository**, Git stays the single source of truth: commits, branches and pull
  requests run through Git (see `CLAUDE.md`). Stemma runs next to it as the example of the book,
  so the history is double on purpose. Call `stemma` here only when the human asks for it.
- **In any other repository** that uses Stemma, the rules below apply as written. An agent gets
  its own working copy there with `stemma worktree add <path> -b <branch>` (run by the human or
  by the agent, as the authorizer allows); it works on its own branch and the human merges it.

## The rule behind all of it

`track`, `commit`, `switch`, `merge`, `rebase`, `status`, `op restore`, and `purge --apply`
first save the tracked files as a snapshot (ADR-010), and so does `diff` when one side is the
working copy. The snapshot covers tracked files only, and it writes a log entry only if a tracked
file changed. There is no file watcher and no background process. A state that no command has
seen is not saved, and a command that fails before its snapshot saves nothing. So the way to keep
an intermediate state is to run a command.

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
   `track`, `commit`, `switch`, `merge`, `rebase`, `status`, `op restore`, and `diff` against the
   working copy fail with exit code 1: stop and report it.
3. Note the number `N` of the newest log entry: `stemma op log -n 1 --json`; the field is
   `number`. `op restore` puts back the state *before* the entry it names, so the first entry
   that the task writes, `N+1`, is the point to go back to (`op restore N+1`, see below).
4. Find out whether the repository is enforcing: the file `.stemma/enforcing` exists, and
   `.stemma-actors/register` lists the identities. In an open repository (the case that the
   steps above assume) there is no lease and no signing: `lease take` ends with exit code 1 and
   `--key` is not read. `pr` and `check` work there as described in "Enforcing repositories".
   Do not ask for the mode with `enforce`: it changes the repository.
5. Optional: `stemma op verify` checks the whole log. It needs no actor. Keep the line
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
- `switch`, `merge`, or `rebase` interrupted after the files were written: repeat the same call
  before any other command. It reads the log entry, takes no new snapshot, and reuses its
  commits; a new message has no effect.
- A result that is made, but the output could not be written or a directory is not saved:
  do not repeat.

With `--json`, a failure writes no JSON. An object before a non-zero code says what was made,
not that the command worked.

## Enforcing repositories (phase 4)

A repository that began with `init --owner-key`, or that a human led there with `enforce`, has a
register of actors (ADR-011) and signs approvals and check results.
Everything above holds there too. This section adds what changes for an agent.
The reference has the synopsis and the failures of each command
(`docs/chapters/12-cli-reference.tex`); this file says which ones an agent calls.

**State of the lease.** The lease is not yet wired into the write commands. `commit`, `track`,
`switch`, `merge`, and `rebase` neither take nor renew a lease, and none of them checks one. Only
`lease take`, `lease release`, `lease override`, and `pr open` touch it, and a lease exists only in
an enforcing repository whose register lists more than one identity. So:

- Do not expect the first `commit` on a branch to take the lease. It does not.
- A lease is no protection against another writer yet. Keep to one actor per branch and one
  working copy per actor, as above.
- Where a lease exists (see the next bullet), take it by hand with `stemma lease take <branch>`
  before `pr open` on that branch, because `pr open` refuses an actor who does not hold a valid
  one (exit code 1). A protected branch such as `main` has no lease. The lease
  expires (60 minutes by default); `lease show <branch>` says who holds it and for how long.
  Give it back with `lease release <branch>` when the branch is done.
- In an open repository, or one whose register lists a single identity, no lease applies:
  `lease take` ends with exit code 1, and `pr open` asks for none. Go on without it.

**What an agent calls, and when:**

| Command | When the agent calls it |
| ------- | ----------------------- |
| `lease show <branch>` | Any time. It only reads. |
| `lease take`, `lease release` | Around the work on its own branch, as above. |
| `pr open <branch>` | When the task is done and the human asked for a pull request. |
| `pr list` | To read the state of the pull requests. It only reads. |
| `pr approve <n>` | Only when the human named the pull request as the review task. Not for a pull request that the agent opened or wrote a commit in (no self-approval, ADR-004). |
| `check record <commit> --result pass\|fail` | After a test run of the agent, with the real result of that run. `check list` only reads. |
| `pr close <n>` | Only for a pull request the agent opened itself and wants to withdraw. |
| `pr merge <n>` | Only on the instruction of the human. Never lower `--level` to get past the gate; without the option it is 3, the strictest. |

An agent does not call `enforce` and `register`:

- `enforce` accepts only a human actor, and the step has no way back.
- `register add-key`, `retire`, `revoke`, and `add-agent` change the register file, a file of
  level 3 (ADR-004): only a human with the role owner approves the change, and the entry of
  another identity only an owner changes. `register cosign` needs a human sponsor and ends with
  exit code 1 for an agent.
- If the agent has no entry in the register, report it to the human: the sponsor runs
  `register cosign`, and an owner runs `register add-agent`. If its key is no longer active, an
  owner adds a new one with `register add-key --identity "agent <id>"`; `add-agent` fails for an
  agent that already has an entry.

**Signing (`--key`).** In an enforcing repository `pr approve` and `check record` sign the entry,
and the option `--key KEYFILE` is required.
It names the key of the actor for `ssh-keygen -Y sign`: the private key file, or the public key
file if `ssh-agent` holds the private key. The key must be an active key of this actor in the
register; the command tries it before it writes anything. The task gets the path from the
human. The agent does not create keys, does not read the contents of a key file, never prints it
into a log or a message, and never tracks it (`track` names files, so keep the key outside the
working copy). In an open repository the option is not read.

**Never `lease override`.** It takes a lease from another actor before it expires, and only a human
with the role owner may do it (ADR-004). An agent never calls it, not even to get past a lease
that another actor holds. If `lease take` or `pr open` says that someone else holds the lease,
report holder and expiry to the human, or wait until it expires.

**Exit codes of the new commands** (the sections of the reference have all cases):

| Code | Where | What the agent does |
| ---- | ----- | ------------------- |
| 1 | `lease`, `pr`, `check` | Nothing was written. Typical causes: another actor holds the lease, the pull request is not open, self-approval, a missing or inactive key, no result of a check counts for the gate, or `Paused` from the merge gate. Fix the cause or report it; for `Paused` repeat after the next run changes the state. |
| 3 | `pr`, `check`, `lease` | An entry is written, but a part is missing (a proof file, the move of `main`, the lease file). Do not repeat blindly. `pr merge` can be repeated: it repeats or completes the move. A repeated `pr approve` writes a second approval, which is meant to be. `check record` that says the proof is missing: the result does not count, so run it again; if it is only unclear whether the entry is saved, `check list` shows it. After `lease`, run `lease show <branch>`, then repeat `take` or `release` if the state is not what you wanted. |
| 6 | `pr`, `lease` | The lock of the references is held. Wait, try a few times, then report. |

Codes 2 and 5 are as in the table above. Any command of this section that ends with 5 means
damage: stop and call the human.

## Conflicts

A conflict after `merge`, `rebase`, or `switch` is data in the commit, not a failure: the exit
code is 0 and the output names the paths. Read `stemma status --json`; edit the files until they
carry no markers. Stemma does not read markers: the next snapshot (`status`, `diff`, `commit`, and
the others) resolves a path as soon as its file differs from the text shown, even with a half
edited marker. Finish editing a conflicted file before you run a command that takes a snapshot.

## Only on the instruction of the human

- `stemma op restore <N>`. It rewrites the working directory. Run `--dry-run` first. In this
  repository it also changes files that Git owns, so ask first.
- `stemma pr merge`, and `pr approve` and `pr close` as the table in "Enforcing repositories"
  says.

## Never

- `stemma purge`, not even the plan without `--apply`. The program refuses an agent at every
  step (ADR-002, ADR-014). If a secret is in the history, report it to the human, who runs the
  purge.
- `--take-over`. The program refuses it from an agent (ADR-004, ADR-010). If the working copy
  belongs to someone else, report it to the human.
- `lease override`, `enforce`, and `register`: see "Enforcing repositories".

## No `stemma` in a Git worktree

An actor needs its own working copy (ADR-010). The Stemma way to get one is `stemma worktree add`
(ADR-009); a Git worktree is something else, and in this repository the agents work in Git
worktrees. For those, the rule stays:

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
    is lost with it. Git does not carry the ignored `.stemma/` into a merge, and the merge of
    Stemma joins branches of one repository only. `git worktree remove` deletes the directory
    with the `.stemma` in it, and on a clean worktree, which is the normal state after the merge,
    it does so without `--force` and without a message. Only the content of the files survives,
    through the Git merge.
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
that is the second case above, and nothing protects the history then. A worktree of Stemma
(`stemma worktree add`) keeps its state in the `.stemma` of the root, so `git worktree remove`
does not take it away.

## Not yet there

- The lease in the write commands. When `commit`, `track`, `switch`, `merge`, and `rebase` take
  and renew it, the steps about `lease take` change.
- The levels of the gate follow from the policy file. Until then `pr merge` takes the level from
  `--level`, and the gate does not check the approvals.
- Phase 6 brings the API and the MCP server, with `--json` for `diff`, `op restore`, and `purge`.
  The commands of phase 4 have no `--json` form yet.

This file changes with each of them.
