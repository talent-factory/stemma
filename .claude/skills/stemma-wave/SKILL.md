---
name: stemma-wave
description: Plan and run the sub-issues of a Stemma epic (for example STEMMA-13) in waves. Proposes an order and the parallel waves from the "Blocked by" lines, waits for approval, then starts one Rex tab with one Git worktree per issue. Also closes a wave after the merge. Use for "plan the sub-issues of STEMMA-n", "start wave B", or "close wave A".
---

# stemma-wave

Runs the sub-issues of an epic as waves. Every issue gets its own Rex tab, its own Git worktree, and
its own Claude session. The human reviews and merges.

Issue tracker: Plane project Stemma (MCP `plane-tf`), see `docs/agents/issue-tracker.md`.
Project ID `d988f244-ae6e-492d-a61e-c335c6c87de9`.

| State | ID |
|---|---|
| In Progress | `612fc934-8cb0-4e88-8182-4430876a266c` |
| In Review | `d567ec07-faf7-4af6-9f95-1ed2e47da941` |
| Done | `06ba4314-afe2-48e2-a234-eb84e48f6234` |

## Mode 1: plan (`/stemma-wave plan STEMMA-13`)

1. Read the epic and list the project with `workitem list` (`per_page` 100, follow `next_cursor`).
   `childOf(...)` in PQL does not work on this Plane edition, so filter by `parent` yourself.
2. Ignore sub-issues in the state Done or Cancelled. Read the description of every open one.
3. Take the dependencies from the line `Blocked by: STEMMA-n` of each description. Check the state of
   every blocker.
4. Form waves: a wave holds the issues whose blockers are all Done (or in an earlier wave).
   Mark what is likely to touch the same files (chapter 6, `docs/chapters/12-cli-reference.tex`,
   the same ADR) and propose the order of the merges.
5. Show the waves as a table with the reason for each placement and the expected conflicts.
   **Stop and wait for the approval of the human.** Never start work before it.

## Mode 2: start (`/stemma-wave start 169 171 175`)

Only after the approval of the human.

1. For each issue: set the state to In Progress (no question), read the title and the work item ID.
2. Start each issue with the script. Run it in `bash`, one call per issue, with real arguments:

   ```sh
   .claude/skills/stemma-wave/launch-issue.sh <number> <work-item-id> <slug> "<peers>"
   ```

   `<slug>` is two or three words of the title in kebab case. `<peers>` names the parallel issues,
   for example `169 conflicts in commands, 171 rewrite working-copy commit, 175 worktree`.
3. Check each new tab with `rex capture -s "$REX_SESSION" -b <block> --trim`: the session has to
   work in the worktree (folder name in the status line), and the prompt has to be submitted. If a
   tab sits at an empty prompt, close it and start it again.
4. Report the tabs, branches, and the expected conflicts. Do not poll; wait for the human.

Each session works through the template `issue-prompt.md`: implement, `just ci`, commit, push, PR,
`/pr-review-toolkit:review-pr`, state In Review, wait for the manual review.

## Mode 3: done (`/stemma-wave done 169 171 175`)

1. Check that the PRs are merged: `gh pr list --state all --search "STEMMA-<n>"`. Skip the issues
   whose PR is still open and say so.
2. Set the merged issues to Done (the auto-close of Plane does not work). `Refs` instead of
   `Closes` in the PR text means: leave the issue open and ask.
3. Close the tabs: find them with `rex window ls -s "$REX_SESSION" --json` by the name
   `STEMMA-<n>`, then `rex window close`.
4. Clean up with `cleanup-issue.sh <number>`; it refuses a worktree with changes.
5. `git pull --ff-only origin main` in the main working tree.
6. Run mode 1 again for the next wave and propose it.

## Rules

- Never merge, never run `gh workflow run` (`ci.yml` is started by the human), no `Co-Authored-By`,
  no `Claude-Session:` line in commits and PR texts (see `CLAUDE.md`).
- Do not call `stemma` in a worktree (see `CLAUDE.md`).
- The sessions of the tabs must not rename the Rex session; the template says so.
- Do not start an issue whose blocker is not merged.
