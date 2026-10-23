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
- The working copy is optional and sparse by design, and the protocol is centred on sparse and
  virtualized checkouts that materialize files on demand. The data of the repository is
  distributed; no fully materialized copy exists anywhere, not even on the server.
- All source control data is content-addressed and never changes once stored. Blame, path
  history, and the skip lists of the commit graph are derived data, defined recursively from
  their values for the parents.
- Scion discourages references other than the main branch and release branches, because global
  state that every server has to agree on is a looming scaling problem. Moving a reference is a
  compare-and-swap on a single row, separate from creating the changeset, so a commit takes no
  lock.
- Every query that the client uses is also exposed through the API, and many writes are too, so
  many jobs need no local repository at all.
- Three tiers (a daemon per node, a regional cache, and the authoritative server) answer the
  same requests. The article reports a growth from about 2,500 merged changes per day in May
  2026 to more than 30,000 in September. It calls Scion "by no means a suitable replacement for
  git for general use" and says that its concepts "really only make sense for truly enormous
  monorepos". It offers no public release.

Stemma starts from other premises: a full local object store, the working copy as a commit
(ADR-010), a local operation log (ADR-011), and branches with one write owner (ADR-004).

## Decision

### Target scale *(confirmed)*

Stemma aims at small teams: one repository with a history of up to a few gigabytes, tens of
actors (humans and agents), and hundreds of merges per day. These are orders of magnitude, not
limits that the code enforces. Monorepo scale is not a goal; by the article's own account, it is
the scale for which Scion's concepts are made.

### Local first *(confirmed)*

Every clone holds the full object store and a working copy, and every command works without a
server: today `track`, `commit`, `status`, and the operation log, later `merge` as well. At the
target scale a full store is cheap, and Stemma is also used in teaching, where students work on
their own machines and not always online. Scion's premise that nearly all clients are online
holds for Anthropic's agents and jobs, not for these users.

### The first hub stores and enforces; it does not merge *(confirmed)*

The first hub of Phase 5 stores objects and references and enforces permissions (ADR-003) and
the write ownership of branches (ADR-004). Commits and merges stay local. Commits and merges on
the server, for agents that work without a clone as in Scion, are a deferred option for a later
slice of Phase 5 or for Phase 6. If they come, the hub runs the same operations of the core as
the CLI; there is no second implementation of a merge.

### Short-lived branches with a lease *(confirmed)*

Agents work on named branches with one owner and a lease (ADR-004). A branch lives briefly and
is not continued after its merge (ADR-006); whether it is deleted then is open (ADR-008,
STEMMA-52). Scion discourages such references because global state threatens to limit its
scale across many servers. At the target scale one hub holds the references, so that cost does
not arise. A reference moves by compare-and-swap, as in Scion (ADR-008).

### Adopted from Scion *(confirmed)*

- **API parity.** Every query and every operation of the CLI is also an operation of the API
  (Phase 6). No function exists only in the CLI.
- **Derived data.** Blame, path history, and indexes of the log, once they exist, are recomputed
  from the commits and are never the source of truth (STEMMA-18).

Stemma shares with Scion the content-addressed, immutable data. It was the basis of Stemma
before the article (ADR-001, ADR-005, ADR-007); this ADR confirms it.

### Commits without a global lock: a goal, not the current state *(confirmed)*

Today one lock, `refs.lock`, serializes every command that writes to a repository, from its
first object to its last move of a reference (ADR-008, ADR-014). Writers do not conflict over
objects, because objects are content-addressed (ADR-007), but a second writer fails with
`Locked` while the first one runs. At the target scale, with one writer per clone, this is
acceptable. Commits without a global lock, with the move of a reference as the only serialized
step, as in Scion, are a goal for the hub, where many actors write to one repository
(STEMMA-19).

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
- Until STEMMA-19 is solved, two commands that write to the same repository at the same time do
  not both succeed; the second one fails with `Locked` and is retried.

## Open items

- Which measurements would trigger a review of the target scale, for example the size of the
  history or the merges per day that a hub has to serve. STEMMA-37 measured the cost of a
  snapshot and gives a first point of reference.
- Whether a later hub performs merges for agents without a clone (a later slice of Phase 5, or
  Phase 6).
- Concurrent commits and merges without a global lock on the hub (STEMMA-19).
