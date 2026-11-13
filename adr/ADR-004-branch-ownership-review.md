# ADR-004: Branch Ownership, Review, and Risk Levels

**Status:** Accepted for phase 4 (STEMMA-233). The decisions are made in STEMMA-222 to STEMMA-232.
What concerns the hub goes to the private project stemma-hub (hand-over in ADR-003).

## Decision

1. Every branch has exactly one write owner (human or agent).
   The core enforces this through a lease, a file per branch that every writing operation renews
   (default 60 minutes; the policy may change it within bounds of 5 minutes to 24 hours that
   the code fixes). The first writing operation acquires it implicitly if the register lists more
   than one actor. There is none in open mode and none on protected branches. After the expiry
   anyone whose role may write to the branch takes it with its next writing operation
   (STEMMA-225).
2. Owner override is available only to a human with the owner role. It is signed, carries a
   reason, and leaves an entry in the operation log. The entry names no commit, so the purge never
   touches it (STEMMA-225). The override changes who writes, never what the merge gate asks.
3. Protected branches such as `main` change only through pull requests. A pull request is a
   sequence of entries in the operation log (STEMMA-226).
4. Agents may review and approve. No self-approval. Approvals are signed and bound to the head
   commit they name; any new commit voids them (STEMMA-227).
5. Risk levels 0 to 3 by impact of the change. The level follows from all touched paths.
   The policy file is itself level 3.
6. Diff display is level 1, diff computation in the merge path is level 2.
7. New direct runtime dependencies are level 2. Automated patch updates are level 1 with
   `cargo-audit`, `cargo-deny`, and a lockfile diff. Cryptography crates are level 3.
8. The policy file and the register files are level 3 and cannot be lowered, not even by the
   policy itself. Policies per namespace, with the same floor, are built in stemma-hub.
9. Owner override in courses only with an audit entry that cannot be purged, is chained and
   signed, and is visible to everyone who reads the log. This is met locally (rule 2); the
   visibility per namespace and course is built in stemma-hub.
10. Relaxing or deleting existing assertions automatically escalates to level 2. The rule is a
    pure function over the old and the new blob (STEMMA-232); the gate of stemma-hub enforces it.

"Independent" in the table below means different actor IDs, together with the ban on
self-approval (rule 4). Two agents of one sponsor count as two reviews. The fields `model` and
`session` of an approval are descriptive only (STEMMA-222), so a stronger independence cannot be
verified; the human at levels 2 and 3 stays the anchor. The gate applies the same rule to the
writer of a `check` (ADR-019).

## Risk Levels

| Level | Examples | Approval |
|---|---|---|
| 0 | Docs, examples, formatting, tests only | One independent agent, merge by agent |
| 1 | New CLI commands, output formats, logic without persistence | Two independent agents, merge by agent |
| 2 | Object format, hash, ref updates, lease, merge, network, migrations, new dependencies | Two agents plus a human maintainer |
| 3 | RBAC, signatures and keys, namespace separation, CI/release, policy files, actor register files | Human owner only, plus an ADR |

## Paths per Risk Level

The level of a change follows from all paths it touches (rule 5); the highest level wins.
The list below was decided in STEMMA-160 for the code that exists. It is prose in this ADR.
The machine-readable policy file `.stemma-policy`, which enforces it, is part of phase 4
(STEMMA-230). It is a line-based file with the header `stemma-policy 1` and the pattern language
of ADR-012, read in the version of the base commit. Lines such as `level 3 .github/workflows/`
name path lists per level, the level of run-time operations, the bounds of the gate, and the
lease duration. The number of approvals per level stays in the core, and a policy may only raise
it.

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
  The same holds for `tag` (ADR-008, ADR-011): it is level 2, because it sets a name that never
  moves and a release may rely on it, and it asks the authorizer for the operation `tag`.
  Deleting a tag (STEMMA-184) is the operation `untag` and level 2 as well: it removes a name
  that a release may rely on, so it is no lighter than setting one, and an authorizer can
  grant it apart from `tag`. A tag that was set wrongly is deleted and set again, which is two
  operations with two entries.

## Open Points

- [x] Level of runtime operations that remove history (purge, ADR-014): proposed level 3, a human
      owner only. The table classifies changes by the paths they touch, not operations at run time.
      Decided for the purge in ADR-014: only a human may purge.
- [x] Corporate customers might demand stricter floors. Out of scope for phase 4; handed over to
      stemma-hub (ADR-003). Relevant only once there are concrete customers.
- [x] Define concrete path lists per level. Decided in STEMMA-160 (table above). The
      machine-readable policy file that enforces it follows in phase 4.
- [x] Agent identities: a dedicated role, or a role with a human sponsor. Decided in STEMMA-222:
      an agent has its own register entry, key and role, bounded by the role of its human sponsor.
