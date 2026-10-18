# Stemma

Version control system in Rust. See `README.md` for the project overview and `adr/` for architecture decisions.

## Language conventions

- Directory names, file names, and all source code are written in English (academic level). This covers identifiers, comments, doc comments, log and CLI output, error messages, and commit-relevant naming.
- Example: `docs/images/`, not `docs/bilder/`.
- Exception for now: the book text (`docs/main.tex`) and the root `README.md` stay in German. The book will be translated to English at a later date; do not translate it before then.

## Commands

All routine tasks run through `just` (see `justfile`, list with `just`):

- `just ci`: format check, clippy (`-D warnings`), tests. Must pass before every commit.
- `just book`: build the ConTeXt book (`docs/main.tex`). Must compile without errors or warnings.

## ConTeXt

The book uses ConTeXt MkIV, not LaTeX. Use `{\bf ...}` instead of `\textbf`, and `\startxtable` instead of `\starttable`.
