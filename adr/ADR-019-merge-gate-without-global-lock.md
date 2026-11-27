# ADR-019: The merge gate without a global lock

**Status:** Accepted (decided in STEMMA-229). The evidence of the checks (STEMMA-240), the
core of the gate (STEMMA-241: the rule table, the optimistic swap, the objects before the lock),
and the bounds (STEMMA-242: the pause at a red tip with the exemption of a declared repair, the
reservation, `max_unverified`, and the window and helper for the bisection; the gate itself does
not bisect) are implemented, and the approvals (ADR-004) in the gate by the policy of an
enforcing repository (STEMMA-303). Not implemented, and left to the caller of the gate until
their slices land: the rule that the authors of a change are registered actors, and the level
by the policy in the command. Their slices are on the map of phase 4.
The pull request as an object, the checks, and the merge gate belong to phase 4. The ADR defines
what "`main` is always green" means for the gate (ADR-006 decides the GitHub side, which stays
open) and gives ADR-004 the meaning of its "merge gate".
The gate on a hub was decided for phase 5 on 2026-11-23 and 2026-11-24 (STEMMA-273, see "The gate
and a hub"); the verdict about `main` that a clone and a hub share, with the approvals and the
escalation for weakened tests, is implemented (STEMMA-303), and so is the swap across the network
in a repository whose `main` follows a remote (STEMMA-309).

## Context

A merge queue with a global lock does not work at the scale of agents. While one change is
tested, every other change waits, and the queue grows with the number of writers. The usual way
out is to test after the merge and before the deployment, to accept that changes conflict while
the tests run, and to let an agent repair the result. The open question is then what a green check
means when many changes merge while the tests of one change run.

Four facts of Stemma bound the answer:

- **A reference moves by compare and swap.** A change names the value it expects, and it fails
  with `Stale` if another change moved the pointer first, and with `Locked` if it finds the lock
  of another change, without waiting (ADR-008). The lock `refs.lock` is held for the check, the
  comparison, and the write of one change, from its first object to its last move, including the
  flushed entry of the operation log (ADR-008, ADR-011); the gate is the one exception (below).
  It is never held for a test, a build, or a review. This is the one serialisation point that exists.
- **A conflict is data.** A merge that does not decide a place returns a sum of trees, and a
  commit can hold it (ADR-005, ADR-010). The conflict model is decided in phase 3: a conflict does
  not stop a merge, a rebase, or a commit, and it is resolved by a later commit that keeps the
  change ID. A tool can therefore hand a conflict back to its author as a commit instead of a
  failed command.
- **The merge of trees is a pure function.** It writes blobs and trees and moves no pointer, so it
  can run again on a new tip at any time and has the same result for the same inputs.
- **The state of `main` at a moment is one commit.** The gate makes the history of `main` linear
  (next section), so the order of the swaps of the pointer is the order of the history. ADR-006
  decides the same for the repository of this project on GitHub; the gate here is the one of
  Stemma itself.

A check is a statement about one commit: its tree passed or failed a set of tests. It says
nothing about the commit that `main` becomes when the change lands, because other changes may
land first.

## Decision

### No lock is held while a check runs

Nothing waits for a check. The only serialisation is the swap of the pointer of `main`, and it
holds the lock of the references for the time of one change, not of a check (ADR-008).
The gate writes the objects of the merge before it takes the lock (next section). A
design that holds a lock, or a place in a queue, for the duration of a test is rejected.

### The gate writes its objects before the lock

This amends ADR-008 and ADR-011 for the gate only. The gate writes the blobs, the trees, and the
commit `M` of the merge before it takes the lock of the references. Under the lock remain the check
of the tip, the entry of the operation log, and the swap of the pointer, so the lock covers the
check, the entry, and the move only.

A purge may remove objects that no reference reaches (ADR-014), so the gate checks under the lock
that `M` is in the store. ADR-011 already makes `Repository` refuse a commit that is not in the
store; the gate reacts by starting again at step 1 of the merge. `track`, `commit`, `status`, and
`op restore` keep taking the lock before their
first object (ADR-014, STEMMA-49); the amendment does not reach them.

### A check result is evidence about a pair of commits

A result names the commit that was tested and the tip of `main` it was merged onto for the test
(its *base*). The result is the entry `check` of the operation log (ADR-011), one entry per pair.
The pull request keeps its results as long as its branch is unchanged. Whether a result is still
enough when `main` has moved is decided by the risk level of the change (ADR-004), as follows.

| Level | Rule when `main` moved after the check |
|---|---|
| 0, 1 | The result stays valid if the merge of the change onto the new tip is clean. |
| 2, 3 | The result is valid only for the tip it was run against; after a move the checks run again. |

A *clean* merge is a tree merge that returns one tree, not a sum of several (chapter 6).
The gate computes it at the moment of the merge, not at the moment of the check.

### The merge is an optimistic swap

To merge, the gate

1. reads the tip `T` of `main` and merges the change onto `T`; the result is the commit `M`, a
   commit with the single parent `T` that holds the change as one commit, so `main` stays linear
   (a decision of this gate; the `merge` command of Stemma makes two parents);
2. applies the rule of the table to the results it holds;
3. moves `main` from `T` to `M` by compare and swap.

If another change moved `main` in between, the swap fails with `Stale`, or with `Locked` if the
lock of that change is still held, and the gate starts again at step 1 with the new tip.
Nothing is held between the steps.
`Repository::merge_gate` is this loop. It takes the risk level from the caller, because the
policy that derives it from the paths is a separate slice. Under the lock it writes the entry
`gate-merge` (the move of `main`, an ordinary change entry, so `op restore` and `op verify` see it
as they see any move), moves `main`, and then writes `pr-merge` (ADR-011). The loop is cut
off after 8 attempts with `Contended`, a technical cap that is not the reservation below.
The record of the merge, `pr-merge`, follows the swap under the same lock. A stop between the
two leaves the pull request open while `main` holds its change; the next call finds that merging
the branch onto the tip changes nothing and records `pr-merge` without a move (the repair path).
A repeated attempt is cheap because only the tree merge and the table are repeated, and a check
is repeated only for levels 2 and 3.

### The worktree of `main` follows after the swap

The gate moves the branch and reads no working copy: a merge must not wait for the directory of
anyone, and the lock covers the entries and the move only. The worktree that has `main` checked
out is therefore on the old tip after the swap, with a working-copy commit whose parent is no
longer the tip (decided in STEMMA-261).

The command brings it along in a second step, after `pr-merge` and outside the lock of the gate
(`Repository::follow_main`). The step is a `switch` to `main` in that worktree (ADR-010): a
snapshot, the next working-copy commit on the tip, one entry `switch` that moves the working-copy
commit only, and the files. A working-copy commit that holds changes is carried as `T + (W - P)`,
and a conflict of the carry is data of the new commit, as for `merge`, `rebase`, and `switch`.
The worktree can be the one of the command or another one; another one is reached through the
directory that its state names.

The step never blocks or undoes the merge. The worktree stays on the old tip, and the command
says so, when

- another actor owns its working copy: only the owner saves a working copy (ADR-010), the step
  takes nothing over, and an agent never does;
- its directory is gone, was moved, or holds another working copy now;
- the switch is refused before it writes: a file that is not tracked is in the way of a file of
  the new tip, the lock is held, or a command of the worktree stopped after its files (ADR-010);
- the switch writes its entry and fails after it.

The exit code follows the rule of the other commands. The first three cases wrote nothing of the
second step, so the command is complete and ends with code 0. The last case ends with code 3, as
an entry is written and a part is missing; so does a switch that is made while its flush is not
known. A worktree whose state cannot be read is named in the output, as it may be the one.

A stop between `pr-merge` and the step leaves the same state. In all of these cases the owner
runs `switch main` in the worktree (after the stopped command is repeated, or with `--discard`,
if a command of the worktree stopped): a switch to the branch that HEAD names brings a working copy
that stayed behind to the tip (ADR-010). A second `pr merge` does not help, as the pull request
is merged.

### Bounds for retries and for the unverified window

The gate applies the two bounds, and the pause at a red tip below, after the rule table and
before it writes the commit `M`. A change that has to wait gets `GateError::Paused`; no entry, no
commit `M`, and no reference change is written (the blobs and trees of the merge may stay,
ADR-014). The gate reads the bounds again for every attempt. The rules are pure functions over the
log (`stemma_core::bounds`); the log is the only state, so no entry is new. Where the log lacks a
fact, the implementation takes the nearest one it has:

- The log records finished runs only. "Until a run on the tip has started" is therefore "until a
  finished run covers a newer commit". The unverified count is the position of the latest verified
  tip in the history of `main`, and all of `main` if no run on a tip exists.
- The duration of a check run is not recorded. It is the time between the entry of the run and the
  entry of the run before it in the log (any run of the change), or the opening for its first run.
  A time is a claim of its writer, so a reservation lasts at most one hour, and a reservation that
  begins in the future holds nothing yet.
- An invalidated run is a passing result for the head whose base is not the current tip. The runs
  count in a row up to a failing result, one per base. A result for the current tip counts as no
  run and does not end the reservation: it is what the holder needs to merge. The reservation
  begins at the `N`-th run, and again at every further `N`; the oldest change that holds one wins,
  and only a change of level 2 or 3 holds one.
- Only the runs of a writer who is independent of the change count for its bounds (the rule of
  `check::is_independent`): the opener and the authors of a change could otherwise clear a red tip
  or start a reservation for their own change. In an enforcing repository a run counts only with a
  signature of its writer on the statement of the run (ADR-025), a failing one as well: anyone
  can write the entry of a run. One failing run counts without it: the one whose statement is
  lost or damaged and that is read from the lines of its entry, so that the loss of a file
  never clears a red tip (ADR-011). Such a stand-in makes its commit red and does nothing
  else: it moves no verified tip, ends no reservation, and opens no exemption of a repair,
  because anyone who writes the log can make one. Likewise a pull request that a closing
  without its proof ended keeps its reservation. A failing run
  stays whoever wrote it, since it only holds a merge back. A change whose path has no level holds
  no reservation: the gate refuses it at its own merge, and it must not stop another change.
  The exemption of a repair counts independent runs only, so nobody opens it for their own change
  by writing the failing run.
- Whoever calls the gate with a level of its own (`merge_gate`) reads no policy: the bounds have
  their defaults, and the reservation is not applied, as the levels of the other changes are the
  policy's to tell (`merge_gate_by_policy`).
- Where one commit has a passing and a failing run, the pass stands (the core reads the last green
  tip from the passing runs alone). In the open mode an actor can therefore write a pass for a red
  commit; the open mode trusts every actor, and the signature that the register asks for closes
  it.

Two bounds keep the gate from starving a change and from running far ahead of its checks. Both
values are in the policy file (ADR-004), so changing them is level 3: a human owner and an ADR. An
agent cannot loosen the bounds under which its own change is merged.

- **Reservation.** After `N` invalidated runs in a row of a change of level 2 or 3, the change gets
  a *reservation*; `N` is 3 by default. A run is *invalidated* when `main` moved while it ran, so
  that the rule table asks for a new run. While the reservation holds, every other change waits,
  whatever its level, until the change is merged, has failed, or is closed. Only a repair of a red
  tip (below) goes ahead of it. The reservation ends after twice the
  duration of the last check run of the change, so a stuck change cannot block `main`. At most one
  reservation exists at a time, and the oldest change wins.
- **Unverified window.** The policy has `max_unverified`, 10 by default. When that many commits lie
  after the latest verified tip, all merges of levels 0 to 3 wait until a run on the tip has
  started. The repairing change is exempt.

### A conflict never enters `main`

If the merge in step 1 has a conflict, the gate refuses the merge and gives the change back to its
author. The author is the write owner of the branch, a human or an agent (ADR-004). The gate does
not rewrite a branch it does not own: the author runs `rebase` onto the new tip, which carries the
conflict as data in the rewritten commits (phase 3), resolves it, and pushes again. Where `rebase`
refuses (no common ancestor, a criss-cross history, a merge commit in the range, ADR-010), the
author gets an ordinary error and decides by hand.
`main` never holds a commit with an open conflict. This is the gate that ADR-010 and chapter 6
leave to phase 4.

### The check after the merge decides what is green

A run of the checks on the tip of `main` follows the merge. It verifies what the checks before
the merge cannot: two changes that merge cleanly and break each other (a semantic conflict).

- A run covers the tip at the time it starts. Commits that land during the run are covered by the
  next run; there is no run per commit and no queue of runs.
- The result is recorded against the commit as evidence, in the same form as the result of a
  change.
- A *red* tip is repaired by a new change, a fix or a revert, and not by a rewrite of `main`. An
  agent may open it, and it is an ordinary change of risk level 0 to 3 by its paths.
- While the latest verified tip is red, changes of level 2 and 3 are not merged. The change that
  repairs the red tip is exempt, for example the revert of the commit that the bisection finds,
  since the break may lie in level 2 code. Changes of level 0 and 1 continue, since they cannot
  make the repair harder to find.
- A repair is declared and then checked by its content. The pull request names the red commit
  (`repairs <commit>`). The gate accepts the declaration only if that commit lies after the last
  green tip, up to and including the first red tip, and the diff of the repair reverts that commit or changes the paths
  that it changed. The gate computes this as a pure function over the two diffs, as for the rule on
  weakened tests (ADR-004, rule 10); nobody has to confirm it. A change that only declares a repair
  gets no exemption. The checks of the repair follow the rule table: levels 2 and 3 need a result
  with the current tip as base, levels 0 and 1 a clean merge onto it.
- The *last green tip* is the newest commit of `main` that a finished run covered with success. The
  *first red tip* is the oldest commit after it that a finished run covered with failure. To find
  the cause in a window of unverified commits, the linear history of `main` between the two
  is the window of a bisection (`Repository::red_window`, `bounds::bisect`; the gate itself does
  not bisect). A failing run does not take back a passing run of the same commit: the core
  reads the last green tip from the passing runs alone (ADR-011), and the failure stays in the log
  for the gate.
- The form of the entry `pr-open` and the syntax of `repairs` are fixed (ADR-011, STEMMA-238),
  and so is the form of the entry `check` (ADR-011, STEMMA-240). This ADR fixes what the gate
  does with them, not how they are written.

### What "`main` is always green" means

ADR-006 says that `main` is always green. With required checks this holds in a bounded sense:

- Every commit of `main` is the clean merge of a change whose checks passed, under the rule of the
  table above, at the moment of the swap.
- The latest *verified* tip is green, or a repair is in progress and level 2 and 3 are paused.
- The number of unverified commits after the latest verified tip is bounded by `max_unverified`
  plus what lands during one run. It is not zero, and nobody claims it is.

A green status says "this commit passed". It does not say "the state of `main` right now passes",
and the gate does not pretend that it does.

### Who counts as independent

The gate relies on the ban on self-approval (STEMMA-227) and gives "independent" in ADR-004 one
meaning: different actor IDs.

- A `check` for level 2 and 3 counts only if its writer is not the opener of the change, not the
  author of a commit in `base..head`, and not an agent whose sponsor is one of them. It does not
  count either if its writer is the sponsor of an agent that opened the change or wrote a commit
  in it (ADR-004). For level 0 and 1 every registered actor counts. The identity counts, not the
  model or the session. The rule is `check::is_independent` and `check::counts`;
  `Repository::counted_checks` applies it, with the signature of the statement of the check,
  which the register must accept for the writer (ADR-011, ADR-025).
- Two agents of one sponsor count as two reviews. The fields `model` and `session` are descriptive
  only (STEMMA-222), so a stronger independence cannot be verified. The human at level 2 and 3
  stays the anchor.
- The sponsor of an actor is the one of the register where the actor is registered, and
  otherwise the claim of its actor line. An author that is no registered agent can therefore name
  another sponsor than the true one, and the sponsor rule does not reach it. The check of the
  writer asks for a registered actor wherever a register exists; the same for the authors is a
  rule for the gate.

### The gate and a hub

Decided for phase 5 (STEMMA-273, ADR-025). The second item is implemented (STEMMA-290), and the
first, the third and the last are (STEMMA-303), and the swap across the network as well
(STEMMA-309).

- The verdict about `main` is a pure function over the commits, the register and the policy of
  the tip `T`, the namespace policy (ADR-027), and the signed statements. `merge_gate` calls it
  locally, and the accepting
  operation calls it on a hub, so the rule stands in one place.
  *(STEMMA-303)* The function is `Repository::judge_main`. It takes the fold of the statements,
  the order of their holder, the heads of the branches, the register and the policy of `T`,
  further policies combined by the maximum, and the time of the holder, and reads nothing but
  the store besides. It adds two rules to the gate by the policy (`merge_gate_by_policy`): the
  level is raised by the escalation for weakened tests (ADR-004, rule 10), and where a register
  exists the approvals of ADR-004 must count, by the recount of phase 4: the agent approvals
  that the policy names for the level, and a registered human at the levels 2 and 3, an owner at
  level 3 (`GateError::NotApproved`). `merge_gate` with the level of the caller asks neither, as
  before. The rule table, the bounds, and the rule of independence are unchanged; the bounds
  read the runs and the pull requests from the fold that the caller passes and the heads of the
  branches from the caller as well.
- The evidence is signed statements. An approval and a check result are statements of registered
  actors, and the gate folds the set of statements instead of the entries of the log; the entries
  `pr-open`, `pr-close`, `pr-approve` and `check` name the hash of the statement they made
  (ADR-011). The rule for who counts as independent holds unchanged.
  *(STEMMA-290)* The local gate reads the pull request, the results of the checks, and the runs
  of its bounds from one fold of the statements (`evidence::fold`), which takes the statements
  in the order of whoever holds them and is the function that a hub calls for its set. The
  rule table, the bounds, and the rule of independence are unchanged and read the result of
  that fold, so the gate comes to the verdicts that it came to from the entries. One thing is
  new: an ended pull request is final in the sense of ADR-025, so the same opening is not
  accepted twice, and the next pull request of a branch needs a new head.
- A hub enforces all bounds: the pause at a red tip with the exemption of a repair, the
  reservation and `max_unverified`. They are computed over the order and the time in the lines
  that the hub adds to every accepted statement. *(STEMMA-303)* The accepting operation calls
  the verdict with the held statements in the order of the hub and the time of the hub (ADR-025,
  "The gate on a hub as built"). A fetch brings the statements with these lines,
  so a clone comes to the same verdict except for what arrived since its fetch; statements not
  yet submitted are ordered by the local log.
- In a repository whose `main` follows a remote, the optimistic swap runs across the network:
  the gate builds `M` on the tip `T` of the remote without a move, sends the ref update, and
  moves `main` only after the receipt. On `Stale` the command fetches and repeats, bounded, and
  signs again each time. Without a remote the gate stays as described above.
  *(STEMMA-309)* The entry is `Repository::merge_gate_remote`. One attempt fetches, takes `T`
  from the remote reference of `main`, judges with `judge_main` by the policy of `T`, writes
  `M` with the single parent `T`, and sends the move from `T` to `M` with the opening as its
  evidence, through the upload and the receipt of a push (ADR-025). After the receipt, under
  the lock, one entry `push` moves the local `main` and its remote reference to `M`, and then
  `pr-merge` follows (ADR-011). `Stale` starts the next attempt with a new fetch and a new
  signature, at most `MAX_ATTEMPTS` times, the cap of the local loop. A refusal of the hub
  leaves `M` unnamed, as the failed attempts of the local gate leave their objects. A change
  that the hub holds already is recorded as merged without a move, the repair path across the
  network. The local `main` therefore never names an `M` that the hub did not accept.
- The write owner of an unprotected branch may push a move that is no fast-forward, and the
  record of the hub says so, which is the way of this ADR for a rebase.
- The accepting operation takes `refs.lock` itself in the middle, as the gate does. Pure
  computations over immutable objects may run before the lock; the fold of the statements, the
  bounds, the record entry and the move stand under it. *(STEMMA-303)* The first version
  computes the verdict under the lock; the trigger for moving the tree merge before it is the
  measured holding time.

## Consequences

- Throughput of merges does not depend on the duration of the checks. It depends on the rate of
  clean swaps, which is limited by the lock of the references only.
- Red tips will happen, in proportion to the rate of semantic conflicts. The cost moves from the
  waiting of every change to the repair of some. The pause for levels 2 and 3 limits the damage
  of a red tip to changes that need the most care.
- The rule for level 0 and 1 trusts a clean merge. Two changes that touch different places and
  break each other pass the gate and are caught by the run after the merge. The risk levels set
  how much of this the project accepts (level 2 and 3 do not).
- Changes of level 2 and 3 can fall behind under many writers: each move of `main` repeats their
  checks. The reservation bounds this: after `N` invalidated runs the change is merged before
  any other change continues, at the price of a short pause that the timeout of the reservation
  limits.
- A result is stored with its base: the entry `check` of the operation log is the evidence about a
  pair of commits (ADR-011, STEMMA-240).
- An automatic run on a pull request and required checks on GitHub need their own decision that
  amends ADR-006; this ADR does not make it. It defines what "green" means for the gate of
  Stemma, which a later decision on GitHub can reuse.
  The workflow stays manual until phase 4 is in code.
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

- [x] The author decides the proposal: the table of rules by risk level, the pause for levels 2
  and 3 while the tip is red, and the repair by a new change. Decided in STEMMA-229 as proposed;
  the repair is declared by `repairs <commit>`.
- [x] The form of the evidence and where phase 4 stores it: the entry `check` of the log, one
  entry per pair of commits, and a run on a tip without a base (ADR-011, STEMMA-240).
- [x] The form of the entry `pr-open` and the syntax of `repairs`: the lines `repairs <commit>`
  after the reviewers (ADR-011, STEMMA-238).
- [x] The form of the entry `check`: the lines `version 1`, `commit`, `base`, and `result`
  (ADR-011, STEMMA-240). The approval (STEMMA-227) is a separate record. Since STEMMA-290 the
  result is a signed statement, and the entry has `version 2` and names its hash (ADR-025).
- [ ] Whether a result may name a *set* of tested base commits when several changes were tested
  together, and what this does to the rule for levels 0 and 1.
- [x] A bound for the retries of a change of level 2 or 3: the reservation after `N` = 3
  invalidated runs (STEMMA-229).
- [x] Writing the objects of the merge before the lock, so that the lock covers the entry and the
  move only (amends ADR-008 and ADR-011 for the gate; STEMMA-229).
- [x] A bound for the unverified window: `max_unverified`, 10 by default (STEMMA-229).
- [ ] How the gate treats a change whose files are disjoint from all that landed since the check,
  as a cheaper test than a clean merge. Out of scope: the first version does not build it, and it
  would loosen the rule for levels 0 and 1 without a measured need.
