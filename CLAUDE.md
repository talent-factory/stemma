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

## ConTeXt

The book uses ConTeXt MkIV, not LaTeX. Use `{\bf ...}` instead of `\textbf`, and `\startxtable` instead of `\starttable`.
