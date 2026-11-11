# Automatic signals for weakened tests, assertions and lint rules

Research note for STEMMA-231. It feeds ADR-004 rule 10: "Relaxing or deleting existing assertions
automatically escalates level 2." The question is which signals in a Rust diff tell a machine
that a change relaxes or deletes an assertion, a test, or a lint or CI rule, how reliable each
signal is, and which tools exist.

Sources are primary unless marked otherwise. Statements marked "own reasoning" are inferences
from those sources and are not stated by them. Fetched on 2026-11-11.

## Summary

- No single signal is sound. A diff that "weakens" a test is a semantic property; every
  syntactic or tool signal catches a subset and has false positives.
- Cheap and precise: attribute and token facts read from the syntax tree of the old and the new
  file (`#[test]` removed, `#[ignore]` added, `expected = ...` dropped, `assert*!` call removed,
  lint level lowered). They are exact about what they see, but see only the literal edit.
- Expensive and semantic: mutation testing and coverage. They detect weakening that no
  syntactic rule sees (an assertion made vacuous), but need a full build and test run and have
  their own false positives.
- cargo-mutants `--in-diff` selects mutants only from the changed code under test; a diff that
  only touches tests runs no mutants. It cannot be the signal for rule 10 alone.
- Recommendation for Stemma (own reasoning): a syntactic gate on the old and new syntax tree as
  the automatic trigger of level 2, plus optional mutation and coverage evidence for the
  reviewing agents. Treat the gate as a floor that only escalates, never as a proof of safety.

## Signal 1: removed or changed assertions

Facts from the standard library:

- `assert!` is "always checked in both debug and release builds, and cannot be disabled"
  ([std::assert](https://doc.rust-lang.org/std/macro.assert.html)). Unsafe code may rely on it.
- `debug_assert!` runs only in non-optimized builds by default, unless `-C debug-assertions` is
  passed. Its expansion is still type-checked
  ([std::debug_assert](https://doc.rust-lang.org/std/macro.debug_assert.html)).

Weakening patterns a detector can see (own reasoning, built on the facts above):

| Edit | Signal | Reliability |
|---|---|---|
| `assert!` / `assert_eq!` / `assert_ne!` call removed | Count of assertion macro calls per function drops | High for the literal case |
| `assert!` becomes `debug_assert!` | Macro path changes, count of always-on assertions drops | High; the docs themselves say this is a relaxation in release builds |
| `assert_eq!(a, b)` becomes `assert!(a.is_ok())` | Argument shape changes | Low; needs a heuristic |
| Condition loosened (`==` to `>=`, constant changed) | Token diff inside the macro | Low; needs expression comparison |
| Assertion replaced by a helper call | Count drops, behaviour may be equal | Gives a false positive |

Two limits:

- Macro bodies are opaque to a parser. syn treats macro input as raw tokens: `Macro` models an
  invocation such as `println!("{}", mac)`, and the caller must parse the body with its own
  parser ([syn docs](https://docs.rs/syn/latest/syn/)). The macro path and the position are
  visible; the comparison inside is not, unless the tool parses the arguments itself.
- Assertion macros can be wrapped or re-exported (for example `pretty_assertions`,
  project macros). A name-based count misses them (own reasoning).

clippy lints that push against vacuous or weak assertions (restriction and pedantic lints are
off by default):

- `assertions_on_constants` (style, warn): flags `assert!(true)` and similar
  ([clippy lint list](https://clippy.rust-lang.org/master/index.html)). It catches one way to
  make an assertion vacuous.
- `assertions_on_result_states` (restriction): flags `assert!(r.is_ok())` and
  `assert!(r.is_err())` (same list).
- `missing_assert_message` (restriction): flags assertions without a custom message and
  explicitly ignores test functions. Its source states it "cannot check the quality of the
  custom panic messages"
  ([source](
  https://github.com/rust-lang/rust-clippy/blob/master/clippy_lints/src/missing_assert_message.rs)).

These lints judge the new state, not the change. They do not report that something was removed.

## Signal 2: removed, ignored or changed tests

Language facts ([Rust Reference, testing attributes](
https://doc.rust-lang.org/reference/attributes/testing.html)):

- `#[test]` marks a function as a test. It is only valid on free, monomorphic functions without
  arguments, and only compiled in test mode.
- `#[ignore]` (optionally `#[ignore = "reason"]`): "Ignored tests are still compiled when in
  test mode, but they are not executed." `--include-ignored` runs them.
- `#[should_panic]` makes a test pass only if it panics. `#[should_panic(expected = "...")]`
  is a substring match, not an equality check. The form without `expected` accepts any panic,
  so dropping `expected` weakens the test.
- A test with return type `Result<(), E>` passes only on `Ok(())`.

Detectable edits and their false positives (own reasoning on top of the facts above):

| Edit | Signal | False positives |
|---|---|---|
| `#[test]` function deleted | Test inventory shrinks | Rename, move to another module, merge of two tests, test replaced by a table-driven one |
| `#[ignore]` added | New attribute on a test | Legitimate for slow or platform-bound tests, often with a reason |
| `#[should_panic]` added | Test now passes on any panic | Legitimate for a new failure-path test |
| `expected = ...` removed or shortened | Weaker match | Message wording changed, test still precise |
| `#[cfg(...)]` added to a test | Test no longer compiled on some targets | Platform-specific tests |
| `#[test]` removed but the function stays | Function silently never runs | Helper extracted from a test |
| Test moved to a `tests/` file or a module not compiled | Inventory shrinks | Pure refactor |

Machine-readable inventory without a parser:

- `cargo test -- --list` prints all tests and benchmarks without running them; `--ignored`
  runs only ignored ones ([rustc book, tests](https://doc.rust-lang.org/rustc/tests/index.html)).
  `--format json` is unstable and needs `-Z unstable-options` (tracking issue
  [#49359](https://github.com/rust-lang/rust/issues/49359)).
- cargo-nextest offers `--run-ignored only|all` and an experimental `--message-format
  libtest-json` ([nextest docs](https://nexte.st/docs/running/)).
- Comparing the listing of the base commit with the listing of the head commit gives a test
  inventory diff that also covers tests generated by macros, which a parser cannot see
  (own reasoning). The cost is a build of both commits, and `cfg` combinations change the list.

clippy lints (all off by default):

- `ignore_without_reason` (pedantic) flags `#[ignore]` without a reason, and
  `should_panic_without_expect` (pedantic) flags `#[should_panic]` without `expected`
  ([clippy source, `attrs/mod.rs`](
  https://github.com/rust-lang/rust-clippy/blob/master/clippy_lints/src/attrs/mod.rs)).
  They make weakening visible and reviewable, but only if the project enables them.

## Signal 3: loosened lint and CI rules

Language and cargo facts:

- Lint levels are `allow`, `expect`, `warn`, `deny`, `forbid`. `forbid` is `deny` that also
  forbids changing the level afterwards; an `allow` inside a `forbid` context is an error
  ([Rust Reference, diagnostics](https://doc.rust-lang.org/reference/attributes/diagnostics.html)).
- `#[expect(lint)]` suppresses the lint and emits `unfulfilled_lint_expectations` when the lint
  no longer fires. `#[allow]` stays silent forever (same page).
- Cargo sets levels in `[lints]` and `[lints.clippy]`; a level can carry a `priority`, and
  lower numbers are overridden by higher ones
  ([Cargo manifest, lints](
  https://doc.rust-lang.org/cargo/reference/manifest.html#the-lints-section)).
  The page does not cover `[workspace.lints]`, which lives in the workspaces chapter.
- cargo-deny reads `deny.toml` with the tables `[advisories]`, `[bans]`, `[licenses]`,
  `[sources]` and `[graph]`. Escape hatches are `ignore` (advisories), `skip` and `skip-tree`
  (bans) and `exceptions` (licenses)
  ([cargo-deny config](https://embarkstudios.github.io/cargo-deny/checks/cfg.html)).

Weakening patterns (own reasoning):

| Edit | Signal |
|---|---|
| `deny` or `forbid` lowered to `warn` or `allow` in `[lints]`, `clippy.toml`, or a crate attribute | Level order comparison old vs new, per lint |
| New `#[allow(...)]` or `#![allow(...)]` in source | Added attribute; `clippy::allow_attributes` (restriction) flags every outer `allow` |
| `#[expect]` turned into `#[allow]` | Suppression no longer self-checking |
| New `ignore`, `skip`, `skip-tree`, `exceptions` entry in `deny.toml` | Added list entry |
| `-D warnings` dropped, `continue-on-error: true` added, a job or step removed in a workflow | Diff of workflow files; not Rust-specific |
| `[profile.*] overflow-checks` or `debug-assertions` set to false | Config value change |
| `forbid` becomes `deny` | Inner `allow` now possible |

Reliability: levels in TOML and attributes are structured and compare exactly. YAML workflows
and justfile recipes are not understood semantically by any tool found; a path rule is the only
reliable signal there. Stemma already puts `.github/workflows/`, `justfile` and `deny.toml` at
level 3 in ADR-004, so those edits are covered by the path rule and need no weakening detector.

Pitfall: `clippy::blanket_clippy_restriction_lints` (suspicious, warn) tells that
`warn(clippy::restriction)` is a misuse. Enabling a group is not weakening; the false positive
for a level comparison is a lint added at a stricter level (own reasoning).

## Signal 4: mutation testing

cargo-mutants inserts bugs into the code under test and reports mutants that no test notices
([welcome](https://mutants.rs/welcome.html)).

- Mutations: function bodies replaced by guessed values, binary and unary operators swapped or
  deleted, match arms deleted when a wildcard arm exists, match guards replaced by `true` and
  `false`, struct literal fields removed when a base expression exists
  ([mutants](https://mutants.rs/mutants.html)).
- Exclusions: `#[cfg(test)]` code, test functions, `#[mutants::skip]`, `unsafe` functions
  (same page).
- Outcomes: a baseline run of the unmodified tree comes first; failing baseline tests would make
  every mutant look caught ([baseline](https://mutants.rs/baseline.html)). Exit codes: 0 all
  viable mutants caught, 2 some missed, 3 timeouts, 4 baseline failed, 5 or 6 bad diff
  ([exit codes](https://mutants.rs/exit-codes.html)). Detailed results are in `mutants.out`.
- `--in-diff <file>` restricts mutants to the changed regions of the code under test and stacks
  with other filters. The docs warn that it is "not a substitute for a full test run", and that
  "a diff that only removes or alters tests triggers no mutants"
  ([in-diff](https://mutants.rs/in-diff.html)).

Use for weakened tests: run the mutants of the whole crate (or of the touched functions) on
the base commit and on the head commit, and compare the set of missed mutants. A mutant caught
at base and missed at head is hard evidence that a test or an assertion got weaker. This
base-versus-head comparison is own reasoning; cargo-mutants does not offer it. `--in-diff`
alone cannot do it, as the quote above shows.

False positives and negatives:

- Equivalent mutants (a changed program with the same behaviour) look like missed mutants.
  cargo-mutants avoids some by design: it does not replace `==` by `<` or `<=` because this is
  "too prone to generate false positives", and it does not mutate `&=`/`|=` to `^=`
  (same page). It has no general detector; manual `#[mutants::skip]` and filters remain.
- Unviable mutants (do not compile) are counted but not printed by default.
- Timeouts (exit 3) can come from an infinite loop caused by a mutant or a limit set too low.
- Cost: one build and test run per mutant. Not feasible per PR on a large crate without
  filtering; see Google below.
- Mutants that need the changed assertion are only caught if the project runs the full suite.

Google's system works on diffs during code review, skips lines without statement coverage and
lines judged "arid" (uninteresting), and thereby reduces mutants "sharply"; it covers about 30%
of diffs with coverage ([Petrovic and Ivankovic, ICSE-SEIP 2018](
https://research.google/pubs/state-of-mutation-testing-at-google/), abstract only; the
percentages are theirs, not measured on Rust). The arid-node idea transfers (own reasoning):
suppress mutants in logging, `Debug` impls and similar.

## Signal 5: coverage

cargo-llvm-cov reports line, region and, on nightly, branch coverage; `--json` exports data,
`--summary-only` gives per-file summaries ([README](
https://github.com/taiki-e/cargo-llvm-cov/blob/main/README.md)).

- Gates: `--fail-under-lines`, `--fail-under-functions`, `--fail-under-regions`,
  `--fail-under-file-lines` and the inverse `--fail-uncovered-lines` etc. (same README).
- Defaults skip `tests`, `examples`, `benches`, `tests.rs`, vendored sources (same README); a
  deleted test therefore never shows as a drop in the test files, only in the code it covered.
- Limits: branch coverage and doctest support are unstable (same README). `--lcov` gives only
  line coverage, `--codecov` gives region coverage.

What a coverage drop tells: a base-versus-head comparison of covered lines or regions of
unchanged source flags a deleted or ignored test that was the only one to reach that code.

What it cannot tell:

- Coverage does not look at assertions. A test that runs code and asserts nothing keeps
  coverage unchanged. Deleting an `assert_eq!` leaves coverage the same (own reasoning).
- Inozemtseva and Holmes report that, when the number of test methods is controlled for, the
  correlation of coverage with test suite effectiveness is "low to moderate", and that
  coverage is useful to find under-tested code but should not be a quality target
  ([ICSE 2014, DOI 10.1145/2568225.2568271](https://doi.org/10.1145/2568225.2568271); the
  publisher page was not reachable, the findings were read from a search result summary of
  the abstract, so check the paper). Java programs only.
- False positives: removing dead code lowers nothing, adding untested code lowers the ratio
  though no test got weaker; a refactor moves lines. Use a delta on unchanged lines, not the
  total (own reasoning).

## Tools for the structural diff

| Tool | What it gives | Limit |
|---|---|---|
| `syn` (`parse_file`, `visit`) | Full Rust syntax tree with attributes and spans; compare old and new file, per function | Macro bodies are token streams; rust-analyzer-level name resolution is absent ([docs](https://docs.rs/syn/latest/syn/)) |
| `cargo test -- --list` | Test inventory including macro-generated tests | Needs a build; depends on `cfg`; JSON format is unstable |
| tree-sitter (Rust grammar) | Error-tolerant parse, also of files that do not compile | Not fetched for this note; no claim made on grammar completeness |
| difftastic | Structural diff based on syntax, `--check-only --exit-code` tells whether there are syntactic changes | README: output "intended for human consumption"; no JSON mentioned; falls back to a line diff on parse errors ([README](https://raw.githubusercontent.com/Wilfred/difftastic/master/README.md)) |
| `git diff -U0` plus regex | Cheapest | Fooled by formatting, multi-line macros, renames |

Own reasoning: Stemma is written in Rust and already needs a diff in `diff.rs`; a `syn`-based
pass over old and new blobs of `.rs` files is the cheapest sound option. Fall back to "level 2"
when a file fails to parse; that keeps the rule a floor.

## What the detectors cannot see

- A test that is made vacuous without any syntactic edit: a changed production function now
  returns a constant the test accepts.
- Weakening in non-Rust files: workflow YAML, `justfile`, shell scripts used as tests.
- A test that is deleted because the feature was deleted. This is a correct deletion that is
  indistinguishable from a bad one by syntax (own reasoning).
- Splitting one strong test into two weaker ones, or a stronger assertion moved into a helper.

## Fit with ADR-004

- Rule 10 as worded ("relaxing or deleting existing assertions") covers Signal 1 and 2
  directly. Signal 3 is mostly covered by the level 3 path list; lint levels in `Cargo.toml`
  and attributes in `.rs` files are not, since `Cargo.toml` is not on the path list (own
  reading of the ADR table; check whether it needs a row).
- Since tests under `tests/` are level 0, rule 10 is the only thing that stops a level 0
  change from deleting the safety net. The escalation target, level 2, requires two agents plus
  a human maintainer.
- Sound floor: any of these in a changed `.rs` or `Cargo.toml` file escalates, with a reason
  string for the reviewer: assertion count in a function decreases, `assert` becomes
  `debug_assert`, `#[test]` function disappears, `#[ignore]` or `#[should_panic]` added,
  `expected` removed, lint level lowered, `allow` added, unparsable file. A rename-aware match
  (same body hash, new name) cuts the false positives for moved tests.

## Open points

- Not checked: rust-analyzer's or other tools' ability to expand macros for a precise
  assertion count; whether `cargo test -- --list` output is stable across Rust versions.
- Not found: an existing Rust tool that reports "weakened test" as a diff property. This is
  based on the sources above and on a few searches, not on an exhaustive survey.
- Not measured: runtime of cargo-mutants on the Stemma workspace; no benchmark in the
  primary sources. A trial run on `stemma-core` would settle if it is usable per PR.
- Not read: the full text of the Google paper and of the Inozemtseva and Holmes paper; the
  `[workspace.lints]` page of the Cargo book; the tree-sitter Rust grammar.
- Open decision: whether signals are computed by the server in the merge gate (needs a Rust
  parser in the server) or by an agent before approval (cheaper, but self-reported).
