# Stemma alongside Git in the root

How the human runs Stemma next to Git in the root of this repository: once at the start, after
every `git pull` or merge on `main`, and at a release.
Git stays the single source of truth. Stemma follows it as the example of the book, so every
Stemma commit in the root records a state that Git already has.
The agent side is in `using-stemma.md`; the command line reference
(`docs/chapters/12-cli-reference.tex`) says what each command does and how it fails.

## Who does what

- **The human** owns the working copy of the root (ADR-010) and runs every command of this
  file that takes a snapshot or writes: `init`, `track`, `commit`, `status`, `diff` against the
  working copy, `tag`, and `just stemma-sync`.
- **The coordinator** (the session that runs the waves) reminds the human to run
  `just stemma-sync` after each merged wave. It checks the result with `stemma op log` and
  `stemma op verify` in the root, and with no other command: both only read and take no
  snapshot. It never calls a command that takes a snapshot or writes, because the working copy
  belongs to the human.
- **Agents** keep working in Git worktrees and never call `stemma` there
  (`using-stemma.md`, section "No `stemma` in a Git worktree").

## Prerequisites

- `stemma` on the PATH: `just install` in the root, again after every release.
- `jq` (`brew install jq`), which the sync uses to read `stemma status --json`.
- A human actor in the shell (ADR-002). It is required, and there is no default:

  ```sh
  export STEMMA_ACTOR="human daniel"
  ```

  The variable holds only in the shell that set it. Never use an actor `agent ...` here.

## Once: the start

The root has no `.stemma` at the moment. The log that the notes of `v0.4.0` anchor (entry 13,
created on 2026-11-18) is gone, and the history starts anew.

1. Go to the root of the Git checkout, on `main`, with a clean working tree:

   ```sh
   cd ~/GitRepository/stemma
   git switch main && git pull
   git status --short           # expected: no output for tracked files
   ```

2. Check that there is no `.stemma` yet (`ls -d .stemma` fails). `init` refuses an existing
   one with exit code 1, the same code as for any other failure.
3. Create the repository:

   ```sh
   stemma init
   ```

   `.stemma/` is in `.gitignore`, so Git does not see it. `.stemmaignore` keeps build output
   (`target/`), the directories of the worktrees (`.worktrees/`, `.claude/`), and secrets
   (`.env`) out of the history.
4. Run the first sync. It tracks exactly the files that Git tracks, minus those that
   `.stemmaignore` excludes, and makes the first commit:

   ```sh
   just stemma-sync
   ```

   Never use `stemma track .` for this: it takes every file that `.stemmaignore` does not
   exclude, files that Git ignores included, and only a purge takes a file out again.
5. Keep the anchor outside the repository, for example in a note of the release:

   ```sh
   stemma op verify             # last line: anchor <number>:<hash>
   ```

The anchor of `v0.4.0` (`anchor 13:b3:0cb2...`) can no longer be checked: it belongs to the
log that is gone, and `op verify --anchor` against the new log would compare it with another
entry 13 or fail. The notes of the next release say that the log was created anew, as the notes
of `v0.4.0` did for the anchor of `v0.3.0`.

## After every `git pull` or merge on `main`

```sh
git switch main && git pull
just stemma-sync
```

The recipe calls `scripts/stemma-sync.sh`. It refuses (exit code 1, nothing written) when

- the directory is a Git worktree, or not the root of the Git checkout,
- the root has no `.stemma`,
- `STEMMA_ACTOR` is missing or is not `human ...`,
- a file that Git tracks differs from `HEAD` (commit or discard it first, so that the Stemma
  commit matches the Git commit that its message names),
- the working copy of Stemma holds a conflict.

Otherwise it reads `stemma status --json`, tracks the paths that are untracked for Stemma and
listed by `git ls-files`, and prints a line `tracked <path>` for each. A file that
`.stemmaignore` excludes is never untracked for Stemma, so it stays out; a file that Git does
not track stays out as well. A file that Git deleted needs no step: the snapshot records the
deletion. If the working-copy commit then has a change, the script prints it (`A`, `M`, `D` and
the path) and commits with the message `Sync with Git <short hash>: <subject of HEAD>`.
Without a change it prints `Nothing to commit: Stemma matches Git <short hash>` and ends with 0.
A failing `stemma` call ends the script with its own exit code (table of `using-stemma.md`): 3
means a result that is not complete, 5 damage (stop and look), 6 a held lock (try again later).

The sync is not part of `just ci`: it writes into the history of the root.

## Further commands

| Command | When |
| ------- | ---- |
| `stemma status` | To see what Stemma would record. It takes a snapshot. |
| `stemma diff` | To read the change of the working-copy commit. It takes a snapshot when one side is the working copy. |
| `stemma log -n 5` | To read the commits. It only reads. |
| `stemma op log -n 5` | To read the operation log. It only reads. |
| `stemma op verify` | After each sync, or at least before a release. Keep the `anchor` line outside the repository; a later `op verify --anchor <number>:<hash>` shows that the log was not rewritten. |
| `stemma tag v<version>` | At a release, on the Stemma commit of the release state (see below). It writes a log entry and takes no snapshot. |
| `stemma op restore <N>` | Only with care. It rewrites the working directory, and in the root those files belong to Git. Run `--dry-run` first, and bring Git back in line afterwards (`git status`, `git restore`). |

## At a release

The release step of `CLAUDE.md` (section "Releases"), with the sync:

1. Merge the PR that closes the phase, then `git switch main && git pull` in the root.
2. `just stemma-sync`. Its commit is the Stemma commit of the release state. If it prints
   `Nothing to commit`, the last sync commit already is that state; `stemma commit -m` would end
   with exit code 1, because there is nothing to commit.
3. Optional: `stemma tag v<version>` on that commit.
4. `stemma op verify`. The `anchor` line and the entry of the commit (`stemma op log -n 3`
   names it) go into the section `Stemma history` of the release notes, in the format of the
   notes of `v0.2.0` (`gh release view v0.2.0`).
5. `just release-check v<version>` once the draft of the release exists.

## Never

- `stemma` in a Git worktree, or with `-C` pointing at the root from a worktree. An agent's
  worktree is a Git worktree; the Stemma way to a second working copy is `stemma worktree add`.
- `stemma track .` in the root. Name files, or let the sync name them.
- `just stemma-sync` or any other command that takes a snapshot or writes, called by an agent or
  by the coordinator.
- `--take-over` by anyone but the human, and only on purpose.
- `stemma purge` without a reason: a secret in the history is the only one (ADR-014).
