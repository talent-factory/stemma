# ADR-016: the library of the terminal interface of `diff --tui`

**Status:** Accepted. The decisions marked *(confirmed)* were confirmed by the human maintainer on
2026-10-28 after the level 2 review of STEMMA-130, which is recorded in the section "Level 2
review" below. The library was chosen by the author on 2026-10-27 in STEMMA-126.

## Context

`stemma diff --tui` is planned as a read-only interface for browsing the files and hunks of a
diff, as `git-tui` does. It needs raw mode, an alternate screen, key events, resizing, and a
layout. `stemma-core` must stay free of all of it, and the plain text output of `diff` must stay
the reference that scripts and tests rely on.

Candidates, measured on 2026-10-27 in a scratch crate against `deny.toml`:

- **ratatui 0.30.2** with its `crossterm` backend (crossterm 0.29). Both are MIT. The MSRV of
  ratatui is 1.88, below the 1.89 of the workspace. With `default-features = false` and the
  feature `crossterm`, the tree of `stemma-cli` grows from 38 to 75 crates in the scratch crate
  (84 with the default features). `cargo audit` reports no advisory, and `cargo deny` reports no
  advisory, ban, or source problem. It warns about two versions of `syn`. It fails the license
  check on one crate, `foldhash 0.2.0` (`Zlib`, a permissive license), which `hashbrown` pulls in
  through `lru` and `kasuari`.
- **No library.** Raw mode and ANSI sequences through `rustix`, which `stemma-core` already uses
  on Unix. This adds almost no crates, but the project would own layout, scrolling, resizing, and
  input, and `rustix` does not cover Windows.
- **Deferring the interface.** The coloured diff in a pager (PR #154) already covers the need.

## Decision

- **ratatui with the feature `crossterm` only** *(confirmed)*. The dependency is declared as
  `ratatui = { version = "...", default-features = false, features = ["crossterm"] }`.
- **A crate of its own, `stemma-tui`** *(confirmed)*. It depends on `stemma-core` and is used by
  `stemma-cli`. `stemma-core` takes no dependency on a terminal library.
- **The view is testable without a terminal** *(confirmed)*. The state of the interface (selected
  file, selected hunk, scroll position) is a plain value that changes by key events, and the
  rendering goes to the `TestBackend` of ratatui in tests. Scripts use `diff` without `--tui`.
- **`Zlib` joins the licenses of `deny.toml`** *(confirmed)*, in the change that adds the
  dependency. It is permissive and compatible with `MIT OR Apache-2.0`.

## Open Points

- [x] Confirmation of the level 2 review of the change that adds the dependency (STEMMA-130).
- [x] Windows: the pager is plain text there (PR #154), and `--tui` is refused with exit code 1
      until the console handling is verified (STEMMA-128).
- [x] `--tui` is not a cargo feature: `stemma-cli` depends on `stemma-tui` unconditionally
      (STEMMA-128), and the tree grows by the crates that ratatui needs. A feature stays possible
      if a build without the interface is wanted.
- [x] Signals: a signal that ends the command asks the interface to end, and the process exits
      once the interface has given the terminal back (STEMMA-128). The handler does not touch the
      terminal itself: `main` holds the lock of standard output, so a write from the handler
      would wait for it for ever. A `SIGKILL` cannot be handled, and the terminal then needs
      `reset`.

## Level 2 review

ADR-004 asks for the review before the merge. PR #158 (commit 2779004, STEMMA-128) was merged on
2026-10-27 first, so this review is a check after the fact. Two independent agents did it on
2026-10-28, one for the supply chain and security, one for maintenance, footprint, and
architecture. Neither found a blocker. The human maintainer confirmed the decisions and accepted
this ADR. The reports of both agents are summarised in a comment on STEMMA-130.

- **Checked, no finding.** At the time of the review, `cargo audit` (123 crates, 1381
  advisories) and `cargo deny check` report no advisory, ban, license, or source problem. All
  crates come from crates.io. `ratatui` has no `unsafe` code and no build script. `crossterm`
  has `unsafe` only in the platform and FFI code of the terminal paths. `stemma-core` has no
  dependency on a terminal library, and `stemma-tui` is testable with the `TestBackend`.
- **Size of the tree.** The measurement in the scratch crate (38 to 75) was lower than the
  real growth. The exact figure is the lockfile: 48 entries before PR #158, 123 now (+75). The
  tree of `stemma-cli` on macOS grows by about 50 crates (32 to 81 or more, depending on how
  versions and build dependencies are counted).
- **`terminal_size` 0.4.4** joins through the `clap` feature `wrap_help`, which wraps the help
  text at the width of the terminal. It is not a direct dependency but one new runtime crate of
  `stemma-cli` that comes from `clap`, not from ratatui; it is MIT OR Apache-2.0, and its own
  dependencies (`rustix`, `libc`, `windows-sys`) were in the tree already. The maintainer
  decided to keep it. The alternative `term_width = 100` needs no crate but does not follow the
  terminal.
- **`nix` as a development dependency** gains the feature `poll` for the tests with a
  pseudo-terminal. `term` and `signal` were there before. The features do not reach a release
  build.
- **`Zlib`** is needed by `foldhash 0.2.0` only. A `[[licenses.exceptions]]` entry for that crate
  would be narrower; it is optional and tracked in STEMMA-138.
- **Not checked.** The Windows tree (`crossterm_winapi`, `winapi`)
  is in the lockfile but not built; `--tui` is refused there (see the open points). The build
  on Linux and with the toolchain 1.89 was not run, and the owners of `ratatui` and `crossterm`
  on crates.io were not looked up.
