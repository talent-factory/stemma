# Contributing to Stemma

Thank you for your interest in Stemma.
The project is at an early stage (phase 1, local core),
and its formats, its commands, and its API may still change without notice.

## Before you start

- Open an issue on GitHub before you start a larger change,
  so that we can agree on the approach first.
- Read the architecture decisions in `adr/` and the glossary in `GLOSSARY.md`.
  A change that contradicts an accepted ADR needs a new or revised ADR first.

## Contributor License Agreement

Stemma is developed and owned by Talent Factory GmbH.
Before the first contribution of a person is merged,
that person signs the Contributor License Agreement (CLA) of Talent Factory GmbH.
The CLA lets Talent Factory GmbH license the contribution under other terms as well,
for example for a commercial offering of the hub (phase 5).
You keep the copyright of your contribution.
The text of the agreement is in `CLA.md`.
When you open your first pull request, the CLA assistant asks you to sign it with a comment
on the pull request. The pull request cannot be merged before you have signed.

Apart from the CLA, every contribution is licensed under the terms of the project,
`MIT OR Apache-2.0` (see `LICENSE-MIT` and `LICENSE-APACHE`).

## Development

All routine tasks run through `just` (list them with `just`).

- `just ci` runs the format check, clippy, the build of the documentation, the line length
  of the documentation sources, and the tests. It must pass before every commit.
- `just deps` checks the dependency tree with `cargo audit` and `cargo deny check`.
  Run it when you add or update a dependency.
- `just book` builds the book (`docs/main.tex`, ConTeXt MkIV).

## Conventions

- Source code, comments, error messages, commit messages, and pull requests are written in
  English.
  The prose of the book (`docs/main.tex`, `docs/chapters/`) and the root `README.md` are written
  in German for now.
- Commit messages follow the format `<emoji> <type>: <description>`,
  in the imperative mood and with at most 72 characters in the first line.
- A change of behaviour, of a format, or of a decision updates the chapter of the book that
  describes it, in the same pull request.
  A new or changed command updates the command line reference
  (`docs/chapters/12-cli-reference.tex`).
- Lines in the documentation sources under `docs/` hold at most 100 characters.

The pull request template lists the checks that every pull request confirms.

## Reporting a security issue

Do not report a security issue in a public issue.
Write to Talent Factory GmbH at support@talent-factory.ch instead.
