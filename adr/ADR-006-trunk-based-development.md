# ADR-006: Trunk-based Development

**Status:** Proposal, open for confirmation. The direction was agreed; the details are proposals.

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
- A slice is small enough to review in one sitting and delivers something that works on its
  own, even if it is not yet reachable from the CLI.
- Incomplete features that must land on `main` stay inactive behind a feature flag or are not
  wired into the CLI yet.
- Branch names follow the commit types: `feat/…`, `fix/…`, `docs/…`, `refactor/…`.
- Merges to `main` are squash merges, so one slice is one commit on `main`. The pull request
  title follows the emoji conventional commit format and becomes the commit subject.
- After a merge, the next slice starts from an updated `main` on a new branch. Further work
  does not continue on the merged branch.
- Each branch has one write owner at a time, human or agent (ADR-004).

## Consequences

- `main` has a linear history with one commit per slice.
- Reviews stay small; the cost is more pull requests and a stricter slicing discipline.
- Feature flags add temporary code that has to be removed once a feature is complete.
- Until the server enforces the rules of ADR-004, protection of `main` relies on the GitHub
  branch protection settings and on discipline.
- The workflow `.github/workflows/ci.yml` runs only when someone starts it by hand
  (`gh workflow run ci.yml --ref <branch>`). It checks formatting, clippy, and the tests on
  Linux, macOS, and Windows. For now, "`main` is always green" rests on `just ci` locally and on
  discipline, and the workflow gives an independent run on all three systems on request. Nothing
  checks a pull request on the server, and the platform-dependent code (ADR-007) is only
  tested on the system of the person who works on it unless someone starts the workflow.

## Open

- [ ] Squash merge as the only allowed merge method, or also allow rebase merges?
- [ ] Maximum lifetime of a branch before it counts as too long.
- [ ] Which GitHub branch protection settings are enabled while the repository is hosted there
      (required checks, required review, no direct pushes).
- [ ] When to run the workflow for every pull request and every push to `main` again, and to
      make its jobs required checks. The triggers are in a comment at the top of the file.
