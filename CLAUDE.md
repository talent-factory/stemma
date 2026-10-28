# Stemma

Version control system in Rust. See `README.md` for the project overview and `adr/` for architecture decisions.

## Language conventions

- Directory names, file names, and all source code are written in English (academic level). This covers identifiers, comments, doc comments, log and CLI output, and error messages.
- Commit messages and pull request titles and descriptions are written in English as well. Keep the emoji conventional commit format of the skill below (`<emoji> <type>: <description>`, imperative mood, 72 characters or fewer in the first line).
- Example: `docs/images/`, not `docs/bilder/`.
- Exception for now: the prose content of the book (`docs/main.tex` and all chapter files under `docs/chapters/`) and the root `README.md` are written in German, because the author drafts the content in German first. The book will be translated to English at a later date; do not translate it before then.
- The exception covers the prose only. File names, directory names, and ConTeXt code (macros, labels, comments) stay English.

## Commands

All routine tasks run through `just` (see `justfile`, list with `just`):

- `just ci`: format check, clippy (`-D warnings`), the build of the documentation (`-D warnings`), the line length of the documentation sources (`just doc-lines`), tests. Must pass before every commit.
- `just coverage`: test coverage summary (needs `cargo install cargo-llvm-cov --locked`). Informational, not part of `just ci`.
- `just deps`: `cargo audit` and `cargo deny check` (configured in `deny.toml`) over the dependency tree (needs `cargo install cargo-audit cargo-deny --locked`). Run it when a dependency is added or updated; not part of `just ci`, because it fetches the advisory database.
- `just norma`: checks the Rust code against the norma rules in `norma/rule-packs/` (needs `norma` on the PATH; see `norma/README.md` for what differs from the packs of norma). Informational, not part of `just ci`: exit code 1 means that rules were violated.
- `just book`: build the ConTeXt book (`docs/main.tex`). Must compile without errors or warnings. It first runs `just figures`, which converts the SVG sources in `docs/images/src/` to PDF (needs `brew install librsvg`).

## Commits

- Create commits with the `git-workflow:professional-commit-workflow` skill (`/git-workflow:professional-commit-workflow`).
- Never add `Claude-Session:` lines (or any other link to a session transcript) to commit messages or PR descriptions. The link points to a full conversation transcript and does not belong in a permanent, potentially public history. Remove such lines if a tool adds them.

## Book alongside the code

The book (`docs/`) is developed in step with the implementation, not after it.

- A slice that changes behaviour, a format, or a decision described in a chapter updates that chapter in the same PR. If the update is larger than the slice, the PR links a Plane issue with the `docs` label instead.
- A new or changed ADR is reflected in the chapter that covers its topic.
- The command line reference (`docs/chapters/12-cli-reference.tex`) is updated with every command or option that is added, renamed, or removed: its synopsis, its arguments, its exit codes, and an example. The help text of the program is the machine-readable source, the appendix explains purpose, behaviour, and failures.
- Figures are kept as SVG sources and converted for the book; see the figure issue in Plane.
- Run `just book` whenever a chapter changes. Tick the book item in the PR template, or state why it does not apply.
- Write and change chapters and figures according to `docs/agents/book-style.md` (structure of a section, language, terminology, figure style).
- A phase counts as done only after the review in `docs/agents/book-review.md` has run for it.

## ConTeXt

The book uses ConTeXt MkIV, not LaTeX. Use `{\bf ...}` instead of `\textbf`, and `\startxtable` instead of `\starttable`.

## Line length in documentation sources

Source files under `docs/` (ConTeXt chapters, Markdown) are read by humans and compared in diffs, so they are wrapped by hand.

- Lines hold at most 100 characters, ideally 80 to 100. Do not leave a long paragraph on one line.
- Break at a natural boundary: after a sentence, or at a clause (comma, conjunction), so that a change to one sentence touches only its own lines. Do not break in the middle of a phrase, a `\type{...}` argument, or a command.
- Continuation lines of an `\item` are not indented; a list item still begins with `\item` at the start of the line.
- No trailing whitespace.
- Verbatim blocks (`\starttyping`) and tables keep their own layout and are exempt.
- `just doc-lines` (part of `just ci`) checks the length and the trailing whitespace of the lines that differ from `main`.
- Apply the rule to every line you write or change. When a passage is touched anyway, bring its paragraph into line; do not reformat whole files that a slice does not touch.

## Stemma in this repository

Git stays the single source of truth; `stemma` runs next to it as the example of the book. Call
`stemma` only when the human asks for it. The commands to call at the start, in between, and at
the end of a task, the exit codes, and the rule for Git worktrees are in
`docs/agents/using-stemma.md`. Do not call `stemma` in a Git worktree until phase 3: it would
open the `.stemma` of the root, or, with a `stemma init` of its own, lose its history when the
worktree is removed.

## Agent skills

### Issue tracker

Issues live in the Plane project Stemma (`STEMMA-n`), accessed through the `plane-tf` MCP server. See `docs/agents/issue-tracker.md`.

### Triage labels

The five default labels (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `GLOSSARY.md` at the root, ADRs in `adr/`. See `docs/agents/domain.md`.
