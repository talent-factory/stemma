# norma rules for Stemma

The Rust rules that [`norma`](https://github.com/talent-factory/norma) checks in this repository.
They are a copy of the Rust packs of norma, adapted to the decisions of this project. Run them with
`just norma`.

`just norma` builds a fresh database `target/norma.db` from `norma/rule-packs/` on every run. The
database of the person who runs it is not touched, and a rule that is switched off here is not
switched on again by a later import into another database.

## What differs from the packs of norma

| Rule | Change | Reason |
| ---- | ------ | ------ |
| `rust-require-thiserror-error-enum` | removed | The errors are written by hand on purpose: the message is complete and `source()` returns the cause of the cause (see the crate documentation of `stemma-core`). A derive would change that, and it adds a dependency. |
| `style-imports-before-mod-declarations-rust` | removed | The files put `mod` first and `use` after it. This is a convention of the project, not a defect. |
| `style-use-before-other-items-rust` | removed | Same convention as above. |
| `rust-no-panicking-match-arm` | `error` to `warning` | An `unreachable!` with a reason states an invariant. A bare `panic!` is still reported. |
| `rust-no-panicking-let-else` | `error` to `warning` | Same reason. |
| `rust-no-string-error-variant` | rewritten in norma | The match is the variant, not the whole enum, and `Vec<String>` no longer matches. |

A changed rule carries a comment `Local change (Stemma)` in front of its `id`.

## What the run leaves out

Files under `tests/` are not checked. `norma` gets the source text without a path, so it cannot
tell integration tests from other files. Code in `#[cfg(test)]` modules is checked, because only
some rules skip it themselves (`google-prefer-expect-over-unwrap-rust` does).

## Keeping the packs current

This directory is a copy. When norma changes a rule that we use, copy the change here. Do not edit
a rule here without the comment `Local change (Stemma)` and a line in the table above.
