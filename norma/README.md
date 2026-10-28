# norma rules for Stemma

The Rust rules that [`norma`](https://github.com/talent-factory/norma) checks in this repository.
They are a copy of the Rust packs of norma, adapted to the decisions of this project. Run them with
`just norma`.

`just norma` builds a fresh database `target/norma.db` from `norma/rule-packs/` on every run. The
database of the person who runs it is not touched, and a rule that is switched off here is not
switched on again by a later import into another database. A new database also holds the five Rust
patterns that norma ships built in (`factory-overuse`, `no-debug-print`, `observer-presence`,
`singleton-quality`, `strategy-overuse`), so a run checks five rules more than the packs hold.

## What differs from the packs of norma

| Rule | Change | Reason |
| ---- | ------ | ------ |
| `rust-require-thiserror-error-enum` | removed | The errors are written by hand on purpose: the message is complete and `source()` returns the cause of the cause (see the crate documentation of `stemma-core`). A derive would change that, and it adds a dependency. |
| `style-imports-before-mod-declarations-rust` | removed | The files put `mod` first and `use` after it. This is a convention of the project, not a defect. |
| `style-use-before-other-items-rust` | removed | Same convention as above. |
| `rust-no-panicking-match-arm` | `error` to `warning` | An `unreachable!` with a reason states an invariant. The rule does not tell it from other panics: `panic!`, `todo!`, and `unimplemented!` are reported as before, now as a `warning`. |
| `rust-no-panicking-let-else` | `error` to `warning` | Same reason. |
| `rust-no-single-field-struct` | removed | A named field documents its purpose (`Tree { entries }`, `FsObjectStore { root }`), and an options type such as `InitOptions` is meant to grow. The advice to use a tuple newtype does not fit these types. |
| `rust-no-public-struct-fields` | `warning` to `info` | Result types such as `FileChange` and `Hunk` are passive data with public fields on purpose. A rule that cannot tell them from a type with an invariant is a hint, not a warning. |

A changed rule carries a comment `Local change (Stemma)` in front of its `id`. The rule
`rust-no-string-error-variant` is not a difference: this copy has the rewrite that norma itself
carries (it matches the variant, not the whole enum, and `Vec<String>` does not match), and it keeps
the comment `Local change` of norma.

## What the run leaves out

Files under `tests/` are not checked. `norma` gets the source text without a path, so it cannot
tell integration tests from other files. Code in `#[cfg(test)]` modules is checked, but some rules
skip it themselves, for example `google-prefer-expect-over-unwrap-rust` and the two
`rust-no-panicking-*` rules.

## The exit code

`just norma` ends with a non-zero code (1 with the `xargs` of macOS) if a rule of severity
`warning` or `error` was violated. A finding of severity `info` or `hint` alone gives 0.

## Keeping the packs current

This directory is a copy. When norma changes a rule that we use, copy the change here. Do not edit
a rule here without the comment `Local change (Stemma)` and a line in the table above. The comment
at the top of each file names the source and the licence of its rules; keep it.
