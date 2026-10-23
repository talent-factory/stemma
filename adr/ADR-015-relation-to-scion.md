# ADR-015: Relation to Scion

**Status:** Accepted. The decisions marked *(confirmed)* were agreed with the author on
2026-10-23 (STEMMA-17). The open items at the end are questions for later phases; none of them
changes a decision above.

## Context

In October 2026 Anthropic described Scion, an in-house version control system that replaced Git
for its monorepo (`anthropic2026redesigning`). Stemma is designed for agents as well, so the
question is which of its ideas Stemma shares and which it does not. Neither the book nor the
ADRs answered it so far.

The statements below were checked against the original article on 2026-10-23:

- Scion assumes that the server is always reachable, because "nearly all" of its clients are
  agents and jobs that are online by definition. Every commit, rebase, and merge happens on the
  server; the client keeps a bounded object cache and can compute very little on its own.
- The working copy is optional and sparse by design. The protocol supports virtualized checkouts
  that materialize files on demand. No copy of the whole repository exists, not even on the
  server.
- All data is content-addressed and never changes once stored. Blame, path history, and the
  skip lists of the commit graph are derived data, computed from the parents.
- Scion discourages references other than the main branch and release branches, because global
  state that every server has to agree on is a scaling problem. Moving a reference is a
  compare-and-swap on a single row.
- Every query of the client is also an API call, so many jobs need no local repository.
- Three cache tiers answer the same protocol. The article reports a growth from about 2,500
  merged changes per day in May 2026 to more than 30,000 in September, and calls Scion "by no
  means a suitable replacement for git for general use". It is not published.

Stemma starts from other premises: a full local object store, the working copy as a commit
(ADR-010), a local operation log (ADR-011), and branches with one write owner (ADR-004).

## Decision

### Target scale *(confirmed)*

Stemma aims at small teams: one repository with a history of up to a few gigabytes, tens of
actors (humans and agents), and hundreds of merges per day. These are orders of magnitude, not
limits that the code enforces. Monorepo scale is not a goal.

### Local first *(confirmed)*

Every clone holds the full object store and a working copy. `commit`, `status`, `merge`, and the
operation log work without a server. At the target scale a full store is cheap, and Stemma is
also used in teaching, where students work on their own machines and not always online. Scion's
premise that every client is online holds for its CI fleet, not for these users.

### The hub stores and enforces; it does not merge in Phase 5 *(confirmed)*

The hub of Phase 5 stores objects and references and enforces permissions (ADR-003) and the
write ownership of branches (ADR-004). Commits and merges stay local. Commits and merges on the
server, for agents that work without a clone as in Scion, are a deferred option for Phase 5 or
Phase 6. If they come, the hub runs the same operations of the core as the CLI; there is no
second implementation of a merge.

### Short-lived branches with a lease *(confirmed)*

Agents work on named branches with one owner and a lease (ADR-004). A branch lives briefly and is
deleted after its merge (ADR-006). Scion avoids such references because global state does not
scale across many servers. At the target scale one hub holds the references, so that cost does
not arise. A reference moves by compare-and-swap, as in Scion (ADR-008).

### Adopted from Scion *(confirmed)*

- **API parity.** Every query and every operation of the CLI is also an operation of the API
  (Phase 6). No function exists only in the CLI.
- **Cheap commits without a global lock.** Writing objects needs no lock. Only the move of a
  reference is serialized, and only per reference (ADR-007, ADR-008).
- **Content-addressed, immutable data.** Already the basis of Stemma (ADR-001, ADR-005,
  ADR-007); this ADR confirms it.
- **Derived data.** Blame, path history, and indexes of the log are recomputed from the commits
  and are never the source of truth (STEMMA-18).

### Rejected for now *(confirmed)*

- Sparse and virtualized checkouts.
- Tiered caches between client and server.
- Infrastructure that is driven by scale, such as a distributed database as the store.

At the target scale they cost more than they save. The formats do not block them: trees are
content-addressed, so a later sparse checkout could fetch subtrees on demand. A new ADR revisits
the question if the target scale changes.

Stemma takes no code and no protocol from Scion.

## Consequences

- The design stays simple enough for one machine and for teaching. A clone works offline.
- Every actor needs the full history on disk. Above the target scale, clones and the first fetch
  become slow; Stemma accepts that.
- Branches as global state remain, which keeps ownership and review (ADR-004) easy to express.
- API parity holds for commands that exist before Phase 6 as well: each one rests on an
  operation of the core that the API can call later, not on logic in the CLI alone.

## Open items

- Which measurements would trigger a review of the target scale, for example the size of the
  history or the merges per day that a hub has to serve. STEMMA-37 measured the cost of a
  snapshot and gives a first point of reference.
- Whether the hub performs merges for agents without a clone (Phase 5 or Phase 6).
- Concurrent merges without a global lock on the hub (STEMMA-19).
