# ADR-017: Derived data

**Status:** Accepted. Decisions marked *(confirmed)* agreed with the author on
2026-10-27 (STEMMA-18). Nothing here is implemented. Phase 2 shipped without a cache: the diff
reads both sides anew on every call and writes nothing under `.stemma/derived/`. The first slice
that needs derived data, `log` of phase 3, turns it into code. The ADR amends ADR-009 (the layout
names `derived/`) and ADR-014 (step 8 of the purge deletes it). The open items below are questions
for later slices; none of them changes a decision above.

## Context

ADR-015 adopts one idea of Scion: blame, path history, and indexes of the log are derived data.
They are defined recursively from the values for the parents of a commit, they can change without
touching a stored object, and they are never the source of truth. ADR-015 leaves open where such
data lives, when it is computed, and how it becomes invalid. `log` (phase 3) needs an answer,
because it walks the history and must not walk it again for every call. The diff of phase 2 was
expected to need one as well, but it reads both sides anew on every call and has no use for a
cache.

Four facts of the existing design bound the answer:

- Objects are content-addressed and immutable (ADR-005, ADR-007). A value that is a function of
  one commit and its ancestors cannot become stale because a commit changes; it can only become
  wrong because the algorithm changes.
- The working copy is a commit that is replaced whenever its files change (ADR-010). Its ID
  changes with them, so a value stored for it soon goes stale.
- A purge removes content from the history and from the operation log (ADR-014). A copy of that
  content in a cache would defeat the purge.
- Every writer of `tmp/` holds `refs.lock`, and whoever takes the lock empties `tmp/` (ADR-011,
  STEMMA-31). A new writer must keep that rule true.

## Decision

### Derived data is a cache outside the object model *(confirmed)*

Derived data lives in `.stemma/derived/`, next to `objects/` and not inside it (ADR-009). No
object, no reference, and no entry of the operation log refers to it, and no ID is computed from
it. A repository without the directory is complete; deleting the directory loses time and nothing
else. `Repository::open` does not check it. A clone, a fetch, and a push never copy it: it is
local and can be recomputed.

### The key is the kind, its version, and the commit *(confirmed)*

An entry is a file at `derived/<kind>-v<n>/<algorithm>/<first 2 hex digits>/<remaining hex
digits>`, the path form of ADR-007 for the ID of the commit, so that no colon of an ID ends up in
a file name (ADR-005). The version `n` belongs to the algorithm and the encoding of the kind and
rises when either changes. Entries of an older version are never read; the purge of ADR-014 or a
later cleanup (open items) removes them. Commits do not change, so an entry needs no other
invalidation: a new commit adds entries and invalidates none.

The encoding of every kind is deterministic: the same value gives the same bytes. A decoder
distrusts its input; an entry that it cannot decode is ignored and computed again.

### Computation is deferred and recursive *(confirmed)*

Nothing is computed by `commit`, `track`, or any other writing command. A query asks the core
for the value of a commit; the core reads the entry, or computes it from the entries of the
parents (computing those first, without recursion on the call stack for long histories) and
stores it. Eager computation is rejected because most kinds are never asked for, and a command
would pay for all of them; it is not rejected for the lock, because a query takes the lock as
well (ADR-010). A later command may fill the cache ahead of time; the format does not change
for it.

### Every commit but the working copy is stored *(confirmed)*

The value for the commit of the working copy (ADR-010) is computed from the stored value of its
parent and the changes of the working copy, in memory, and dropped. The value of every other
commit is stored, reachable from a branch or not: no check of reachability is needed, and a
detached HEAD or a rewritten branch needs no rule.

### Writes hold the lock of the references *(confirmed)*

A query reads and computes without a lock. To store its entries it takes `refs.lock`, as every
writer of `tmp/` does (ADR-011), writes each entry to `tmp/`, and moves it into place by a rename,
as objects are (ADR-007). Three rules keep the cache clean:

- A write is best-effort. If the lock is held (including by a purge, `PurgePending`), or if a
  write fails, the query returns its value and stores nothing; the next query computes it again.
- Under the lock, the writer stores an entry only if the object of its commit still exists. A
  purge deletes the commits that its rewrite replaced (ADR-014), so a query that began before a
  purge cannot put a value for a deleted commit back. The value of a commit that a purge kept
  was computed from a history that the purge did not change, so it holds nothing that the purge
  removed.
- The lock is held for the writes only, not for the computation.

### A purge removes the cache *(confirmed)*

Derived data can hold content that a purge must remove, for example the lines of a blame. Step 8
of the purge deletes `.stemma/derived/` completely, with the delete set and before the journal is
removed. `--resume` repeats it; `--abort` leaves it alone, because nothing has been removed before
the `purge` entry (ADR-014). The next queries recompute what they need.

### The first entry raises the format version *(confirmed)*

The command that stores the first entry raises `.stemma/format` to the next version, under the
lock and by the rule of ADR-009 (a version never sinks). An older program then refuses the
repository instead of opening it and purging without knowing `derived/`, which would leave the
cache in place and report success. A repository that never stores an entry stays at its version.
The version is raised again only if a later ADR changes the cache itself.

### The operations belong to the core *(confirmed)*

Each kind is an operation of `Repository` that the CLI and, later, the API call (ADR-015, API
parity). The format of the value of a kind is decided by the slice that introduces the kind, and
documented in the book chapter of that slice.

## Consequences

- The cache is optional, but the format version is not: the first stored entry makes the
  repository unreadable for older programs, which is the price of a purge that cannot miss it.
- The first query on a long history is slow; later ones are fast. A shared repository on a hub
  (phase 5) can keep its own cache by the same rules.
- A change of an algorithm costs one bump of `n` and no migration.
- A query that stores entries takes the lock for a short time, so it can fail to store while a
  writer holds it. That costs time and no correctness.
- Every kind carries a version, a deterministic encoding, a decoder that distrusts its input,
  and a test that the result with a cold cache equals the result with a warm one.
- The purge becomes slightly more expensive for the next queries, and it does not need to know
  what the cache holds.

## Alternatives considered

- **Store derived data as objects.** It would put a function of the history into the object
  model, give it IDs, and force a purge to rewrite it. Rejected: this is the opposite of
  Scion's definition.
- **Compute eagerly on commit.** Rejected for the kinds that nobody asks for (above).
- **Write without the lock, with a generation mark.** Keeps queries lock-free, but needs a
  scratch directory of its own, a mark that the purge changes, and a check before every rename.
  Rejected for now: the lock is the rule that ADR-011 and ADR-014 already rely on.
- **No cache, compute on every call.** Possible at the target scale (ADR-015) for `log`, but
  blame over a history of thousands of commits is not. The first slice may ship without a cache
  and add it later.
- **A generation number for the merge base.** `merge_bases` walks the full ancestor sets of both
  tips and reads no commit date, which is linear in the size of the history. A generation number
  (the length of the longest path to a root) would end the walk early, but it is derived data
  with a cache of its own. It is a later optimisation if a measurement asks for it, and the
  function needs no change of its interface to take it.
- **One database file** (for example SQLite). A new dependency (level 2 under ADR-004), a lock
  of its own, and a second durability story. Rejected for now; files by key are enough.

## Open items

- [ ] Whether entries of commits that a rewrite left unreachable, and entries of an older
      version, are removed by a command of their own or only by the purge. They cost space and
      nothing else.
- [ ] A size bound of the cache, if the directory grows beyond what a machine should carry.
- [ ] The first kind to implement, and whether a cache is part of that slice. The format
      version it raises is the next one after the version of the log chain.
