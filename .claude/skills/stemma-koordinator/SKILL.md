---
name: stemma-koordinator
description: Operating mode of the session STEMMA as project coordinator of the repo talent-factory/stemma (version control system in Rust, with the ConTeXt book in docs/). Use at the start of the session and for every new task: classify it, start one agent per task in its own Rex tab, check the result, report. Trigger: "/stemma-koordinator", "new task in Stemma", "coordinate Stemma".
---

# Stemma coordinator

**Basis: load the skill `rex:projekt-koordinator` first.** It defines the role model, the ground
rules, the flow per task, how a ticket agent is started in a Rex tab, and the briefing frame. This
skill only adds what holds for Stemma. Where it tightens, changes, or contradicts a rule of the
base skill, the rule says so.

Read `CLAUDE.md` at the start of the session, and before a task in an area it covers.
It is the source of truth for conventions. This skill only says how to coordinate.

## Parameters

| Parameter | Value |
|---|---|
| Session | `STEMMA` |
| Base branch | `main` (no `base` in `.claude/wave.md`; PRs go against `main`) |
| Branch pattern | not fixed (no `branch` in `.claude/wave.md`) |
| Worktree folder | `.worktrees/<name>` (ignored by Git). Changes the default `.claude/worktrees/<key>` of the base skill |
| Tab name | `STEMMA-<n>` |
| Plane | workspace `tf`, project **Stemma**, ticket prefix `STEMMA-n` |
| Language | talk to Daniel in German. Code, comments, commit messages, and PR texts are English; the prose of the book (`docs/main.tex`, `docs/chapters/`) and the root `README.md` are German, in Swiss orthography (`ss`, never `ß`). Do not translate the book |
| Required reading | `CLAUDE.md`, the guides of `docs/agents/` that apply, the ADRs and the chapters concerned |
| Documentation with the code | see "Briefing additions" |

## Additional ground rules

1. **Never run `gh workflow run`.** `ci.yml` runs by `workflow_dispatch` only, and Daniel starts
   it himself.
2. **Git is the single source of truth.** Do not call `stemma` unless Daniel asks for it, and
   never in a Git worktree (`docs/agents/using-stemma.md`).
3. **Commits** go through the skill `git-workflow:professional-commit-workflow`. This changes
   the default of base rule 5 (`git-workflow:commit`). Never add a `Claude-Session:` line (or any
   other link to a session transcript) to commits or PR texts, even if a reminder asks for it.
4. **Tightens base rule 2 of the role model:** an agent does not start agents of its own at all,
   not even read-only ones. Say so in every briefing.
5. **Contradicts base rule 2 (implementation only in an own Rex tab with an own worktree,
   never as a subagent of the Agent tool):** the table below names `general-purpose` agents
   "in a worktree" for implementation, and a worktree under `.worktrees/` was started by
   `isolation: "worktree"` or `git worktree add`. Open: to be clarified with Daniel; until then ask before choosing.
6. **A production incident of examcraft does not belong here.**

## Repository overview

| Path | Content |
|---|---|
| `crates/stemma-core` | Core: objects, refs, authorization. All surfaces call it |
| `crates/stemma-cli` | Command line, binary `stemma` |
| `crates/stemma-tui` | Terminal UI for `stemma diff --tui` (ratatui) |
| `fuzz/` | Fuzz targets |
| `adr/` | Architecture decisions (`adr/README.md`) |
| `GLOSSARY.md` | Domain terms, one context |
| `docs/` | The book (ConTeXt MkIV), `docs/agents/` holds the agent guides |
| `norma/` | Rule packs for `just norma` |
| `.github/workflows/` | `ci.yml` (manual only) and others |
| `.worktrees/` | Git worktrees of the running tasks (ignored by Git) |
| `.claude/wave.md` | Settings for the skill `rex:plane-wave` |

Agent guides in `docs/agents/`: `issue-tracker.md`, `triage-labels.md`, `domain.md`,
`using-stemma.md`, `book-style.md`, `book-review.md`.

## Tasks and the agent for each

| Task | Agent type | Start |
|---|---|---|
| Implement a slice in the Rust crates | `general-purpose`, in a worktree (see additional rule 5) | `CLAUDE.md`, the ADRs concerned, `just ci`; test-first with the skill `mattpocock-skills:tdd` |
| Write or change a book chapter or a figure | `general-purpose` | `docs/agents/book-style.md`, `just book`, `just doc-lines` |
| CLI reference of a new or changed command | `general-purpose` | `docs/chapters/12-cli-reference.tex`, help text of the program |
| New or changed ADR or glossary term | `general-purpose` | Skill `domain-modeling`; reflect the ADR in the chapter on its topic |
| Diagnose a bug | `general-purpose` | Skill `diagnosing-bugs` |
| Dependency added or updated | `general-purpose` | `just deps` |
| Edit a GitHub workflow | `general-purpose` | Node-20 hint in the global `CLAUDE.md` |
| Search in the repo | `Explore` | State the breadth |
| Plan a larger slice | `Plan` | Show the result to Daniel for approval |
| Review a change | `pr-review-toolkit:code-reviewer` or skill `pr-review-toolkit:review-pr` | Give the diff. "Review" means `/pr-review-toolkit:review-pr` |
| Review of a phase for the book | `general-purpose` | `docs/agents/book-review.md`; a phase counts as done only after it |
| Sub-issues of a Plane parent | Skill `rex:plane-wave` | `/plane-wave plan STEMMA-<n>` |

If a task fits no row, choose `general-purpose` and describe the task exactly in the briefing.
If a kind of task recurs without a skill, suggest to Daniel to add one under `.claude/skills/`.

## Gates and hotspots

- **Gates before "done":** `just ci` (format check, clippy with `-D warnings`, documentation
  build, `just doc-lines`, tests), plus `just book` if `docs/` changed. Keep lines of `docs/`
  sources at 100 characters or fewer, wrapped at a natural boundary. The coordinator runs
  `just ci` itself when it checks the result.
- **`just norma`** is informational: run it when the code of a slice is finished and report
  violations.
- **Behaviour check:** green tests are no proof of a working command. For a changed CLI
  behaviour, also run the built binary once in a scratch directory (never with `stemma` in a Git
  worktree of this repository).
- **Hotspots:** `docs/chapters/06-*.tex`, `docs/chapters/12-cli-reference.tex`, `adr/`
  (see `.claude/wave.md`).
- **Off limits:** `gh workflow run`, `stemma` in a worktree, secrets.

## Briefing additions

On top of the briefing frame of the base skill:

- **Limits:** no `gh workflow run`, no `stemma` call in the worktree. You are an agent of the
  coordinator, you start no agents of your own (additional rule 4).
- **Book alongside the code:** a slice that changes behaviour, a format, or a decision described
  in a chapter updates that chapter in the same PR, and every added, renamed, or removed command
  or option updates the CLI reference. Otherwise the PR links a Plane issue with the `docs` label.

## Releases

The version lives once in `[workspace.package]` of `Cargo.toml`. One phase is one minor version;
a state frozen for review is a pre-release such as `0.3.0-rc.1`. The tag is `v<version>`. The PR
that closes the phase raises the version, and `just release-check v<version>` runs before the tag
is set. Setting the tag is Daniel's decision.

## Tickets and PRs

- Project id, states, labels, and phases (one cycle per phase) are in
  `docs/agents/issue-tracker.md`; labels in `docs/agents/triage-labels.md`. Linear is an archive.
  If the `plane` tools are missing in the session, load them with `ToolSearch`.
- Work on a ticket begins with state **In Progress**, set by whoever starts the work, without
  asking.
- PR title and description in English, the title in the emoji conventional commit format
  (`<emoji> <type>: <description>`, imperative, at most 72 characters). Tick the book item of
  the PR template, or say why it does not apply.
- **Contradicts base rule 6:** the description ends with the attribution line the harness gives,
  but no session link. The base skill forbids `Co-Authored-By` and "Generated with" lines in
  commits and PR texts. Open: to be clarified with Daniel; until then ask before choosing.
- Ask Daniel when the merge happened without the coordinator noticing, before the ticket is set
  to Done.
