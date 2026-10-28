# ADR-004: Branch Ownership, Review, and Risk Levels

**Status:** Proposal, open for decision.

## Decision

1. Every branch has exactly one write owner (human or agent).
   The server enforces this through a lease with heartbeat and expiry.
2. Owner override is available only to the owner role, with a justification and an audit entry.
   The override never bypasses the merge gate.
3. Protected branches such as `main` change only through pull requests.
4. Agents may review and approve. No self-approval. Approvals are signed.
5. Risk levels 0 to 3 by impact of the change. The level follows from all touched paths.
   The policy file is itself level 3.
6. Diff display is level 1, diff computation in the merge path is level 2.
7. New direct runtime dependencies are level 2. Automated patch updates are level 1 with
   `cargo-audit`, `cargo-deny`, and a lockfile diff. Cryptography crates are level 3.
8. Every namespace has its own policy with a floor at level 3 that cannot be lowered.
9. Owner override in courses only with an undeletable audit entry that is visible to students.
10. Relaxing or deleting existing assertions automatically escalates to level 2.

## Risk Levels

| Level | Examples | Approval |
|---|---|---|
| 0 | Docs, examples, formatting, tests only | One independent agent, merge by agent |
| 1 | New CLI commands, output formats, logic without persistence | Two independent agents, merge by agent |
| 2 | Object format, hash, ref updates, lease, merge, network, migrations, new dependencies | Two agents plus a human maintainer |
| 3 | RBAC, signatures and keys, namespace separation, CI/release, policy files | Human owner only, plus an ADR |

## Paths per Risk Level

The level of a change follows from all paths it touches (rule 5); the highest level wins.
The list below was decided in STEMMA-160 for the code that exists. It is prose in this ADR.
The machine-readable policy file, which enforces it, is part of phase 4.

| Level | Paths and operations |
|---|---|
| 3 | `purge` (ADR-014), actor and authorizer (`actor.rs`, `allowed.rs`), CI and release files (`.github/workflows/`, `justfile`, `deny.toml`), policy files, the option `--take-over` |
| 2 | Object and store (`object.rs`, `store.rs`), refs (`refs.rs`), operation log (`oplog.rs`), repository (`repository.rs`, `repository/`), index (`index.rs`), diff computation in the merge path (`diff.rs`), verify (`verify.rs`), `checkout`, `commit`, `restore` and `snapshot`, the parts of phase 3, `.stemmaignore` (ADR-012) |
| 1 | CLI (`stemma-cli`), TUI (`stemma-tui`), `status`, `track`, the code of the exclusion rules (`ignore.rs`), diff display |
| 0 | Docs (`docs/`, `adr/`, `README.md`), tests (`tests/`, `fuzz/`), examples |

- Paths that no row names are level 2 until a row names them. A new directory therefore never
  lowers a level by accident.
- A change of the file `.stemmaignore` is level 2 (it can let a secret into the history), a change
  of the code that reads it is level 1.
- Operations at run time are no paths (see the first open point): `restore` is level 2 for the
  change that implements it, and the command itself asks the authorizer, not the review.

## Open Points

- [x] Level of runtime operations that remove history (purge, ADR-014): proposed level 3, a human
      owner only. The table classifies changes by the paths they touch, not operations at run time.
      Decided for the purge in ADR-014: only a human may purge.
- [ ] Corporate customers might demand stricter floors.
      Relevant only once there are concrete customers.
- [x] Define concrete path lists per level. Decided in STEMMA-160 (table above). The
      machine-readable policy file that enforces it follows in phase 4.
- [ ] Agent identities: a dedicated role, or a role with a human sponsor.
