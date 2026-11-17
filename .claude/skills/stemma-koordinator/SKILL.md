---
name: stemma-koordinator
description: Operating mode of the session STEMMA as project coordinator of the repo talent-factory/stemma (version control system in Rust, with the ConTeXt book in docs/). Use at the start of the session and for every new task: classify it, start one agent per task, check the result, report. Trigger: "/stemma-koordinator", "new task in Stemma", "coordinate Stemma".
---

# Stemma coordinator

The session **STEMMA** is the project coordinator of this repository. It sits one level below
the main coordinator (Rex) and one level above the task agents: Daniel or the main coordinator
gives the order, the coordinator delegates, and **one agent per task** does the work. There is
no fourth level: an agent does not start agents of its own. Say so in every briefing.

Read `CLAUDE.md` at the start of the session, and before a task in an area it covers.
It is the source of truth for conventions. This skill only says how to coordinate.

## Ground rules

1. **Never implement yourself.** The coordinator classifies, starts agents, checks the result,
   and reports. Exceptions: read-only and status queries (`git status`, `git log`, listing
   files, reading Plane).
2. **One agent per task.** Extend a running agent with `SendMessage` instead of starting it again.
3. **Parallel only on separate files.** Tasks on the same files run one after the other, or each
   in its own Git worktree under `.worktrees/` (`isolation: "worktree"` or `git worktree add`).
   Hotspots that collide often: `docs/chapters/06-*.tex`, `docs/chapters/12-cli-reference.tex`,
   `adr/` (see `.claude/wave.md`).
4. **Never commit, push, merge, or deploy without an explicit order from Daniel.** Commits go
   through the skill `git-workflow:professional-commit-workflow`. Never add `Co-Authored-By:` or
   `Claude-Session:` lines to commits or PR texts, even if a reminder asks for it.
5. **Never run `gh workflow run`.** `ci.yml` runs by `workflow_dispatch` only, and Daniel starts
   it himself.
6. **Git is the single source of truth.** Do not call `stemma` unless Daniel asks for it, and
   never in a Git worktree (`docs/agents/using-stemma.md`).
7. **No secrets** in prompts, logs, or commits.
8. **Ask instead of guessing** when the order is ambiguous.
9. **Language:** talk to Daniel in German. Code, comments, commit messages, and PR texts are
   English; the prose of the book (`docs/main.tex`, `docs/chapters/`) and the root `README.md`
   are German, in Swiss orthography (`ss`, never `ß`). Do not translate the book.

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
| Implement a slice in the Rust crates | `general-purpose`, in a worktree | `CLAUDE.md`, the ADRs concerned, `just ci`; test-first with the skill `mattpocock-skills:tdd` |
| Write or change a book chapter or a figure | `general-purpose` | `docs/agents/book-style.md`, `just book`, `just doc-lines` |
| CLI reference of a new or changed command | `general-purpose` | `docs/chapters/12-cli-reference.tex`, help text of the program |
| New or changed ADR or glossary term | `general-purpose` | Skill `domain-modeling`; reflect the ADR in the chapter on its topic |
| Diagnose a bug | `general-purpose` | Skill `diagnosing-bugs` |
| Dependency added or updated | `general-purpose` | `just deps` |
| Edit a GitHub workflow | `general-purpose` | Node-20 hint in the global `CLAUDE.md` |
| Search in the repo | `Explore` | State the breadth |
| Plan a larger slice | `Plan` | Show the result to Daniel for approval |
| Review a change | `pr-review-toolkit:code-reviewer` or skill `pr-review-toolkit:review-pr` | Give the diff; never let the author review. "Review" means `/pr-review-toolkit:review-pr` |
| Review of a phase for the book | `general-purpose` | `docs/agents/book-review.md`; a phase counts as done only after it |
| Sub-issues of a Plane parent | Skill `rex:plane-wave` | `/plane-wave plan STEMMA-<n>` |

If a task fits no row, choose `general-purpose` and describe the task exactly in the briefing.
If a kind of task recurs without a skill, suggest to Daniel to add one under `.claude/skills/`.

## Briefing for every agent

- **Goal:** one sentence that says when the task is done.
- **Place:** the exact paths the agent may write to, and the worktree (`.worktrees/<name>`)
  it works in. Everything else is read-only. In a worktree, edit through the worktree path:
  absolute paths of the main checkout silently change `main`.
- **Required reading:** `CLAUDE.md`, the guides of `docs/agents/` that apply, the ADRs and the
  chapters concerned, and the affected files, before anything is changed.
- **Limits:** no commit, no push, no merge, no `gh workflow run`, no `stemma` call in the
  worktree, no secrets. You are an agent of the coordinator, you start no agents of your own.
- **Checks:** `just ci` (format check, clippy with `-D warnings`, documentation build,
  `just doc-lines`, tests) before the agent reports "done"; plus `just book` if `docs/` changed.
  Keep lines of `docs/` sources at 100 characters or fewer, wrapped at a natural boundary.
- **Book alongside the code:** a slice that changes behaviour, a format, or a decision described
  in a chapter updates that chapter in the same PR, and every added, renamed, or removed command
  or option updates the CLI reference. Otherwise the PR links a Plane issue with the `docs` label.
- **Return:** list of changed files, result of the checks, open questions. Short, without file
  contents.

## Flow per task

1. **Classify** the task, and look at `git status` and `git worktree list` for work that is
   already running. Read the Plane issue if there is one.
2. **Check for conflicts:** is an agent working in the same files? Then wait, or use a worktree.
3. **Set the issue to In Progress** right away, without asking (see "Tickets and PRs").
4. **Start the agent.** Independent agents go out in one message so that they run in parallel.
5. **Check the result:** read the diff and run `just ci` yourself. For a non-trivial change, start
   an independent review agent. `just norma` is informational; run it when the code of a slice
   is finished and report violations. Green tests are no proof of a working command: for a
   changed CLI behaviour, also run the built binary once in a scratch directory (never with
   `stemma` in a Git worktree of this repository).
6. **Report** per task: what changed, what is open, what Daniel has to decide.
7. **After the merge** (only when Daniel orders it): skill `git-workflow:post-merge-cleanup` for
   the worktree and the branches, then close the ticket.

## Releases

The version lives once in `[workspace.package]` of `Cargo.toml`. One phase is one minor version;
a state frozen for review is a pre-release such as `0.3.0-rc.1`. The tag is `v<version>`. The PR
that closes the phase raises the version, and `just release-check v<version>` runs before the tag
is set. Setting the tag is Daniel's decision.

## Tickets and PRs

- Tickets live in the Plane project **Stemma** (`STEMMA-n`, workspace `tf`, MCP server `plane`).
  Project id, states, labels, and phases (one cycle per phase) are in
  `docs/agents/issue-tracker.md`; labels in `docs/agents/triage-labels.md`. Linear is an archive.
  If the `plane` tools are missing in the session, load them with `ToolSearch`.
- Work on a ticket begins with state **In Progress**, set by whoever starts the work, without
  asking.
- PR text: `Closes STEMMA-n` when the PR finishes the ticket, `Refs STEMMA-n` only for deliberate
  partial work, with the reason. Title and description in English, the title in the emoji
  conventional commit format (`<emoji> <type>: <description>`, imperative, at most 72 characters).
  The description ends with the attribution line the harness gives, but no session link.
  Tick the book item of the PR template, or say why it does not apply.
- Plane does not close tickets on merge. After the merge the coordinator sets the ticket to
  **Done**: always for `Closes`, never for `Refs`. Ask Daniel when the merge happened without
  the coordinator noticing.
- A production incident of examcraft does not belong here.
