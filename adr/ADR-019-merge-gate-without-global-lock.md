# ADR-019: The merge gate without a global lock

**Status:** Proposal, open for decision by the author (STEMMA-19). Nothing here is implemented.
The pull request as an object, the checks, and the merge gate belong to phase 4. The ADR amends
ADR-006 (what "`main` is always green" means once checks are required) and gives ADR-004 the
meaning of its "merge gate".

## Context

A merge queue with a global lock does not work at the scale of agents. While one change is
tested, every other change waits, and the queue grows with the number of writers. The usual way
out is to test after the merge and before the deployment, to accept that changes conflict while
the tests run, and to let an agent repair the result. The open question is then what a green check
means when hundreds of changes merge while the tests of one change run.

Four facts of Stemma bound the answer:

- **A reference moves by compare and swap.** A change names the value it expects, and it fails
  with `Stale` if another change moved the pointer first (ADR-008). The lock `refs.lock` is held
  for the comparison and the write only, never for a test, a build, or a review (ADR-011). This is
  the one serialisation point that exists, and it costs microseconds.
- **A conflict is data.** A merge that does not decide a place returns a sum of trees, and a
  commit can hold it (ADR-005, ADR-010). The conflict model is decided in phase 3: a conflict does
  not stop a merge, a rebase, or a commit, and it is resolved by a later commit that keeps the
  change ID. A tool can therefore hand a conflict back to its author as a commit instead of a
  failed command.
- **The merge of trees is a pure function.** It writes blobs and trees and moves no pointer, so it
  can run again on a new tip at any time and has the same result for the same inputs.
- **`main` is linear and every slice is one commit** (ADR-006). The order of the swaps of the
  pointer is the order of the history, so "the state of `main` at a moment" is one commit.

A check is a statement about one commit: its tree passed or failed a set of tests. It says
nothing about the commit that `main` becomes when the change lands, because other changes may
land first.

## Decision

### No lock is held while a check runs

Nothing waits for a check. The only serialisation is the swap of the pointer of `main`, and it
holds the lock of the references for the time of one comparison and one write (ADR-008). A
design that holds a lock, or a place in a queue, for the duration of a test is rejected.

### A check result is evidence about a pair of commits

A result names the commit that was tested and the tip of `main` it was merged onto for the test
(its *base*). The pull request keeps its results as long as its branch is unchanged. Whether a
result is still enough when `main` has moved is decided by the risk level of the change
(ADR-004), as follows.

| Level | Rule when `main` moved after the check |
|---|---|
| 0, 1 | The result stays valid if the merge of the change onto the new tip is clean. |
| 2, 3 | The result is valid only for the tip it was run against. After a move the checks run again on the new merge. |

A *clean* merge is a tree merge that returns one tree (GLOSSARY). The gate computes it at the
moment of the merge, not at the moment of the check.

### The merge is an optimistic swap

To merge, the gate

1. reads the tip `T` of `main` and merges the change onto `T`; the result is the commit `M`;
2. applies the rule of the table to the results it holds;
3. moves `main` from `T` to `M` by compare and swap.

If another change moved `main` in between, the swap fails with `Stale`, and the gate starts again
at step 1 with the new tip. Nothing is held between the steps. A repeated attempt is cheap
because only the tree merge and the table are repeated, and a check is repeated only for levels 2
and 3.

### A conflict never enters `main`

If the merge in step 1 has a conflict, the gate refuses the merge and gives the change back to its
author. The author is a human or an agent (ADR-004): the gate rebases the branch onto the new tip,
which yields a commit with the conflict as data (phase 3), and the author resolves it and pushes
again. `main` never holds a commit with an open conflict. This is the gate that ADR-010 and
chapter 6 leave to phase 4.

### The check after the merge decides what is green

A run of the checks on the tip of `main` follows the merge. It verifies what the checks before
the merge cannot: two changes that merge cleanly and break each other (a semantic conflict).

- A run covers the tip at the time it starts. Commits that land during the run are covered by the
  next run; there is no run per commit and no queue of runs.
- The result is recorded against the commit as evidence, in the same form as the result of a
  change.
- A *red* tip is repaired by a new change, a fix or a revert, and not by a rewrite of `main`. An
  agent may open it, and it is an ordinary change of risk level 0 to 3 by its paths.
- While the latest verified tip is red, changes of level 2 and 3 are not merged. Changes of level
  0 and 1 continue, since they cannot make the repair harder to find and often are the repair.
- To find the cause in a window of unverified commits, the gate bisects the linear history of
  `main` between the last green and the first red tip.

### What "`main` is always green" means

ADR-006 says that `main` is always green. With required checks this holds in a bounded sense:

- Every commit of `main` is the clean merge of a change whose checks passed, under the rule of the
  table above, at the moment of the swap.
- The latest *verified* tip is green, or a repair is in progress and level 2 and 3 are paused.
- The number of unverified commits after the latest verified tip is bounded by what lands during
  one run. It is not zero, and nobody claims it is.

A green status says "this commit passed". It does not say "the state of `main` right now passes",
and the gate does not pretend that it does.

## Consequences

- Throughput of merges does not depend on the duration of the checks. It depends on the rate of
  clean swaps, which is limited by the lock of the references only.
- Red tips will happen, in proportion to the rate of semantic conflicts. The cost moves from the
  waiting of every change to the repair of some. The pause for levels 2 and 3 limits the damage
  of a red tip to changes that need the most care.
- The rule for level 0 and 1 trusts a clean merge. Two changes that touch different places and
  break each other pass the gate and are caught by the run after the merge. The risk levels set
  how much of this the project accepts (level 2 and 3 do not).
- A result is stored with its base, so the pull request of phase 4 needs a place for evidence
  that is keyed by a pair of commits. The format is left to that phase.
- An automatic run on a pull request and required checks need a decision that amends ADR-006.
  This ADR is that decision for the design. The workflow stays manual until phase 4 is in code.
- The gate depends on the conflict model of phase 3: it relies on `rebase` and on a conflict that
  a commit can hold. A model with markers in files could not hand a conflict back as a commit.

## Alternatives rejected

- **A merge queue with a global lock.** It serialises the checks and not only the swap. Waiting
  grows with the number of writers and with the length of a check.
- **A queue that tests batches speculatively.** It keeps `main` green at every commit, but it
  needs a server that holds speculative states and discards them when one fails. The cost and the
  complexity are too high for the size of the project, and the effect shrinks when checks are
  quick.
- **Rebase and retest every change on every move of `main`.** It gives the strongest statement per
  commit, and under many writers a change would never catch up. Only levels 2 and 3 pay this cost.
- **No check after the merge.** It leaves semantic conflicts undetected until someone notices.

## Open Points

- [ ] The author decides the proposal: the table of rules by risk level, the pause for levels 2
  and 3 while the tip is red, and the repair by a new change.
- [ ] The form of the evidence and where phase 4 stores it (a result per pair of commits).
- [ ] Whether a result may name a *set* of tested base commits when several changes were tested
  together, and what this does to the rule for levels 0 and 1.
- [ ] A bound for the unverified window, such as a maximum number of commits after which merges
  wait for a run.
- [ ] How the gate treats a change whose files are disjoint from all that landed since the check,
  as a cheaper test than a clean merge. Not needed for the first version.
