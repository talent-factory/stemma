# ADR-021: Syntax highlighting and themes of `diff --tui`

**Status:** Proposed (STEMMA-251). The dependency is a level 2 change (ADR-004, rule 7: a new
direct runtime dependency), so the human maintainer decides before the merge. ADR-016 chose the
library of the interface and the crate `stemma-tui`; this ADR adds what colours the text.

## Context

The interface of `diff --tui` colours the change (red and green backgrounds) and the moved lines
(ADR-016, ADR-017), but not the text itself. A reader of a diff of code wants keywords, strings,
and comments to stand out, as in every other diff viewer. Two constraints come from the
project:

- `deny.toml` has `unmaintained = "all"`, so a crate that is unmaintained fails `cargo deny`.
- The workspace has `rust-version = "1.89"`.

Candidates, measured on 2026-11-22 in a scratch crate against `deny.toml` (toolchain 1.99.1,
`cargo audit` with 1560 advisories):

- **`syntect` 5.3.0** (MIT), `default-features = false`, feature `default-fancy` (no C code).
  71 crates in the tree. It highlights from Sublime Text grammars, has many languages and ready
  themes, and needs the least code. **`cargo deny` fails**: `bincode 1.3.3` (RUSTSEC-2025-0141)
  and `yaml-rust 0.4.5` (RUSTSEC-2024-0320) are unmaintained. Both are structural: `bincode`
  reads the bundled syntax dumps, `yaml-rust` loads syntax definitions. Using the crate would
  need two permanent `ignore` entries for the advisories.
- **`tree-sitter-highlight` 0.25.10** with the grammar crates `tree-sitter-rust`, `-toml-ng`,
  `-bash`, `-json`, and `-md` (all MIT). 21 crates in the tree of the scratch crate, of which
  the grammars and `tree-sitter` bring C code that `cc` compiles; the rest are `regex`,
  `memchr`, `aho-corasick`, `streaming-iterator`, and `thiserror`. `cargo audit` and
  `cargo deny` report no advisory, ban, license, or source problem. It needs a C compiler at
  build time and takes 4 to 40 seconds more to build from scratch; the binary grows by about
  0.4 MB. **Version 0.27.2 needs Rust 1.90**, above the 1.89 of the workspace, so the version is
  pinned to `=0.25.10` (MSRV 1.82). The grammars depend only on `tree-sitter-language`, so they
  do not need 0.27 either. Tree-sitter delivers capture names, not colours, so the themes are
  ours.
- **A lexer of our own** for a few languages. No dependency and no level 2, but a rough result
  (comments, strings, numbers, keywords), and each language is code that the project keeps.
- **No highlighting.** The coloured diff in a pager and the current view stay as they are.

## Decision

- **`tree-sitter-highlight` `=0.25.10` and the grammars of Rust, TOML, Bash, JSON, and Markdown**,
  as dependencies of `stemma-tui` only. `stemma-core` and the plain text of `diff` stay free of
  them (ADR-016). The version of `tree-sitter-highlight` is exact, until the workspace moves to
  Rust 1.90.
- **The grammar follows the extension of the path.** `.rs`, `.toml`, `.sh`, `.bash`, `.json`,
  `.md`. Any other file, and a text that cannot be parsed, is drawn without highlighting.
  Markdown uses the block grammar only: the inline grammar needs injections.
- **A whole side is parsed at once**, the old and the new text each, so that a multi-line
  construct is coloured on every line. The parser tolerates errors.
- **Highlighting changes the foreground colour of characters only.** The background of an added
  or removed line, the text, the widths, and the wrapping do not change. Moved lines keep their
  colours, which say that they moved.
- **Three themes**, chosen with the key `t` (no option, no setting): `dark` (the former look,
  with highlighting, the default), `light` (for a light terminal), and `plain` (the colours of
  `dark` without highlighting).
- **A grammar is added only through a new ADR or an amendment of this one**, with the same
  measurement against `deny.toml`.

## Open Points

- [ ] Confirmation by the human maintainer (level 2).
- [ ] Moving to `tree-sitter-highlight` 0.27 once the workspace has Rust 1.90.
- [ ] Not checked: the owners of the crates on crates.io, and the build on Windows (`--tui` is
      refused there, ADR-016).
