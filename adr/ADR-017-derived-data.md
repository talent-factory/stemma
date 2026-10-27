# ADR-017: Derived data

**Status:** Proposal, open for decision (STEMMA-18). Nothing here is implemented. The first slice
that needs derived data (the diff work of phase 2, or `log` of phase 3) turns it into code.

## Context

ADR-015 adopts one idea of Scion: blame, path history, and indexes of the log are derived data.
They are defined recursively from the values for the parents of a commit, they can change without
touching a stored object, and they are never the source of truth. ADR-015 leaves open where such
data lives, when it is computed, and how it becomes invalid. The diff work (phase 2) and `log`
(phase 3) need an answer, because both walk the history and must not walk it again for every
call.

Three facts of the existing design bound the answer:

- Objects are content-addressed and immutable (ADR-005, ADR-007). A value that is a function of
  one commit and its ancestors cannot become stale because a commit changes; it can only become
  wrong because the algorithm changes.
- The working copy is a commit that is rewritten after every command (ADR-010). Its ID changes
  all the time, so a value stored for it would be dead at once.
- A purge removes content from the history and from the operation log (ADR-014). A copy of that
  content in a cache would defeat the purge.

## Decision

### Derived data is a cache outside the object model *(proposed)*

Derived data lives in `.stemma/derived/`, next to `objects/` and not inside it. No object, no
reference, and no entry of the operation log refers to it, and no ID is computed from it. A
repository without the directory is complete; deleting the directory loses time and nothing else.
`Repository::open` does not check it.

### The key is the kind, its version, and the commit *(proposed)*

An entry is addressed by `<kind>-v<n>/<commit ID>`, for example `blame-v1/<id>`. The version `n`
belongs to the algorithm and the encoding of the kind and rises when either changes. Entries of
an older version are never read; the next purge of the cache removes them. Commits do not change,
so an entry needs no other invalidation: a new commit adds entries and invalidates none.

### Computation is deferred and recursive *(proposed)*

Nothing is computed by `commit`, `track`, or any other writing command. A query asks the core
for the value of a commit; the core reads the entry, or computes it from the entries of the
parents (computing those first, without recursion on the call stack for long histories) and
stores it. Eager computation would lengthen every command that holds `refs.lock` (ADR-008) and
would pay for kinds that nobody asks for. A later command may fill the cache ahead of time; the
format does not change for it.

### The working copy commit is never stored *(proposed)*

The value for the commit of the working copy (ADR-010) is computed from the stored value of its
parent and the changes of the working copy, in memory, and dropped. Only commits that are
reachable from a branch and are not the working copy are stored.

### Writes are best-effort and need no lock *(proposed)*

An entry is written to `tmp/` and moved into place by a rename, as objects are (ADR-007). The
value for a key is the same for every writer, so two writers that race produce the same file and
need no lock; a reader never sees a half-written entry. A failed write is not an error of the
query: the value is returned and the next query computes it again. A reader that finds an entry
that it cannot decode, or that carries another version, ignores it and recomputes it.

### A purge removes the cache *(proposed)*

Derived data can hold content that a purge must remove, for example the lines of a blame. A
purge deletes `.stemma/derived/` completely before it reports success (ADR-014). The cost is a
recomputation, which is cheap next to the purge itself.

### The operations belong to the core *(proposed)*

Each kind is an operation of `Repository` that the CLI and, later, the API call (ADR-015, API
parity). The format of the value of a kind is decided by the slice that introduces the kind, and
documented in the book chapter of that slice.

## Consequences

- No format of the repository changes. `.stemma/format` keeps its version, because the cache is
  optional and absent in every repository until a kind exists.
- The first query on a long history is slow; later ones are fast. A shared repository on a hub
  (phase 5) can keep its own cache by the same rules.
- A change of an algorithm costs one bump of `n` and no migration.
- Every kind carries a version, a decoder that distrusts its input, and a test that the result
  with a cold cache equals the result with a warm one.
- The purge becomes slightly more expensive for the next queries, and it does not need to know
  what the cache holds.

## Alternatives considered

- **Store derived data as objects.** It would put a function of the history into the object
  model, give it IDs, and force a purge to rewrite it. Rejected: this is the opposite of
  Scion's definition.
- **Compute eagerly on commit.** Rejected for the lock time and for the unused kinds above.
- **No cache, compute on every call.** Possible at the target scale (ADR-015) for `log`, but
  blame over a history of thousands of commits is not. The cache is optional, so a first slice
  may ship without one and add it later.
- **One database file** (for example SQLite). A new dependency (level 2 under ADR-004), a lock
  of its own, and a second durability story. Rejected for now; files by key are enough.

## Open items

- Whether entries of commits that are no longer reachable (after a rewrite) are removed by a
  command of their own or only by the purge. They cost space and nothing else.
- The size of a bounded cache, if the directory grows beyond what a clone should carry.
- The first kind to implement, and whether a cache is part of that slice.
