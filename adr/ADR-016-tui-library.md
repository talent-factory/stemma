# ADR-016: the library of the terminal interface of `diff --tui`

**Status:** Proposal, chosen by the author on 2026-10-27 in STEMMA-126. A new direct dependency is
level 2 under ADR-004, so the change that adds it needs two agents and a human maintainer. The
change of STEMMA-128 builds the crate `stemma-tui`, adds ratatui and the license `Zlib`, and
awaits that review; the decisions below stay proposals until it is confirmed.

## Context

`stemma diff --tui` is planned as a read-only interface for browsing the files and hunks of a
diff, as `git-tui` does. It needs raw mode, an alternate screen, key events, resizing, and a
layout. `stemma-core` must stay free of all of it, and the plain text output of `diff` must stay
the reference that scripts and tests rely on.

Candidates, measured on 2026-10-27 in a scratch crate against `deny.toml`:

- **ratatui 0.30.2** with its `crossterm` backend (crossterm 0.29). Both are MIT. The MSRV of
  ratatui is 1.88, below the 1.89 of the workspace. With `default-features = false` and the
  feature `crossterm`, the tree of `stemma-cli` grows from 38 to 75 crates (84 with the default
  features). `cargo audit` reports no advisory, and `cargo deny` reports no advisory, ban, or
  source problem. It warns about two versions of `syn`. It fails the license check on one crate,
  `foldhash 0.2.0` (`Zlib`, a permissive license), which `hashbrown` pulls in through `lru` and
  `kasuari`.
- **No library.** Raw mode and ANSI sequences through `rustix`, which `stemma-core` already uses
  on Unix. This adds almost no crates, but the project would own layout, scrolling, resizing, and
  input, and `rustix` does not cover Windows.
- **Deferring the interface.** The coloured diff in a pager (PR #154) already covers the need.

## Decision

- **ratatui with the feature `crossterm` only** *(proposed)*. The dependency is declared as
  `ratatui = { version = "...", default-features = false, features = ["crossterm"] }`.
- **A crate of its own, `stemma-tui`** *(proposed)*. It depends on `stemma-core` and is used by
  `stemma-cli`. `stemma-core` takes no dependency on a terminal library.
- **The view is testable without a terminal** *(proposed)*. The state of the interface (selected
  file, selected hunk, scroll position) is a plain value that changes by key events, and the
  rendering goes to the `TestBackend` of ratatui in tests. Scripts use `diff` without `--tui`.
- **`Zlib` joins the licenses of `deny.toml`** *(proposed)*, in the change that adds the
  dependency. It is permissive and compatible with `MIT OR Apache-2.0`.

## Open Points

- [ ] Confirmation of the level 2 review of the change that adds the dependency.
- [x] Windows: the pager is plain text there (PR #154), and `--tui` is refused with exit code 1
      until the console handling is verified (STEMMA-128).
- [x] `--tui` is not a cargo feature: `stemma-cli` depends on `stemma-tui` unconditionally
      (STEMMA-128), and the tree grows by the crates that ratatui needs. A feature stays possible
      if a build without the interface is wanted.
- [ ] Signals: a signal that ends the command gives the terminal back first (STEMMA-128). A
      `SIGKILL` cannot be handled, and the terminal then needs `reset`.
