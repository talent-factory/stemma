# ADR-006: Trunk-based Development

**Status:** Accepted. The open items were decided by the author on 2026-10-24 (STEMMA-10).

## Context

Stemma is developed by a small team that includes agents as contributors. A long-lived branch
makes review harder, hides integration problems, and conflicts with the rule of ADR-004 that a
branch has a single write owner. The project also wants to be an example of the workflow its
own design supports.

## Decision

- `main` is the trunk. It is protected and changes only through pull requests (ADR-004).
- `main` is always green: `just ci` passes, and `just book` compiles without errors or
  warnings when the book changed.
- Work happens on short-lived branches, ideally merged within a day or two. Every slice of work
  is its own branch and its own pull request, branched from the current `main`.
- A branch that is not merged after three working days counts as too long. It is then split
  into smaller slices, or its unfinished part lands inactive behind a feature flag.
- A slice is small enough to review in one sitting and delivers something that works on its
  own, even if it is not yet reachable from the CLI.
- Incomplete features that must land on `main` stay inactive behind a feature flag or are not
  wired into the CLI yet.
- Branch names follow the commit types: `feat/…`, `fix/…`, `docs/…`, `refactor/…`.
- Merges to `main` are squash merges, so one slice is one commit on `main`. Squash is the only
  allowed merge method; merge commits and rebase merges are disabled. The pull request title
  follows the emoji conventional commit format and becomes the commit subject.
- After a merge, the next slice starts from an updated `main` on a new branch. Further work
  does not continue on the merged branch.
- Each branch has one write owner at a time, human or agent (ADR-004).
- While the repository is hosted on GitHub, the branch protection of `main` requires a pull
  request for every change (no direct pushes), forbids force pushes and the deletion of the
  branch, and requires a linear history. It does not require an approving review, because the
  review rules belong to ADR-004, and it does not require status checks, because the workflow
  does not run for pull requests (next item).
- The workflow `.github/workflows/ci.yml` runs only when the author starts it by hand
  (`gh workflow run ci.yml --ref <branch>`), for example before a larger merge or for a change
  to platform-dependent code. It is not triggered by pull requests or by pushes to `main`, and
  its jobs are not required checks. The triggers for an automatic run stay as a comment at the
  top of the file.

## Consequences

- `main` has a linear history with one commit per slice.
- Reviews stay small; the cost is more pull requests and a stricter slicing discipline.
- Feature flags add temporary code that has to be removed once a feature is complete.
- Until the server enforces the rules of ADR-004, protection of `main` relies on the GitHub
  branch protection settings and on discipline.
- The workflow checks formatting, clippy, and the tests on Linux, macOS, and Windows. "`main` is
  always green" rests on `just ci` locally and on discipline, and the workflow gives an
  independent run on all three systems on request. Nothing checks a pull request on the server,
  and the platform-dependent code (ADR-007) is only tested on the system of the person who works
  on it unless the author starts the workflow (STEMMA-33).
- An automatic run for every pull request, together with required checks, needs a new
  decision that amends this ADR.
