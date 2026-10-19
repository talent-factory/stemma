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

- `just ci`: format check, clippy (`-D warnings`), tests. Must pass before every commit.
- `just coverage`: test coverage summary (needs `cargo install cargo-llvm-cov --locked`). Informational, not part of `just ci`.
- `just book`: build the ConTeXt book (`docs/main.tex`). Must compile without errors or warnings.

## Commits

- Create commits with the `git-workflow:professional-commit-workflow` skill (`/git-workflow:professional-commit-workflow`).
- Never add `Claude-Session:` lines (or any other link to a session transcript) to commit messages or PR descriptions. The link points to a full conversation transcript and does not belong in a permanent, potentially public history. Remove such lines if a tool adds them.

## Book alongside the code

The book (`docs/`) is developed in step with the implementation, not after it.

- A slice that changes behaviour, a format, or a decision described in a chapter updates that chapter in the same PR. If the update is larger than the slice, the PR links a Plane issue with the `docs` label instead.
- A new or changed ADR is reflected in the chapter that covers its topic.
- Figures are kept as SVG sources and converted for the book; see the figure issue in Plane.
- Run `just book` whenever a chapter changes. Tick the book item in the PR template, or state why it does not apply.

## ConTeXt

The book uses ConTeXt MkIV, not LaTeX. Use `{\bf ...}` instead of `\textbf`, and `\startxtable` instead of `\starttable`.

## Agent skills

### Issue tracker

Issues live in the Plane project Stemma (`STEMMA-n`), accessed through the `plane-tf` MCP server. See `docs/agents/issue-tracker.md`.

### Triage labels

The five default labels (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `GLOSSARY.md` at the root, ADRs in `adr/`. See `docs/agents/domain.md`.
