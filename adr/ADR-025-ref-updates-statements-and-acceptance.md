# ADR-025: Ref updates, signed statements, and the acceptance on a hub

**Status:** Accepted for phase 5. Decided by the author on 2026-11-23 and 2026-11-24 (STEMMA-56
for the ref update and its record, STEMMA-273 for the enforcement, with the addenda of STEMMA-274,
STEMMA-276 and STEMMA-278) and carried into the ADRs by STEMMA-280. Nothing of it was measured or
built. The names of functions are a direction for the slices.

## Context

Locally the authorizer refuses what the register and the policy do not allow, but only for
processes that go through the core (ADR-003). Points 1, 2 and 8 of the hand-over in ADR-003 ask a
hub for a signed record of ref updates, for the enforcement of protected branches on a server, and
for the escalation when tests are weakened at the gate. In phase 4 the pull request, the approvals
and the check results live in the operation log (ADR-011), and a hub never sees that log. ADR-015
demands that the hub runs the same operations of the core as the CLI and does not merge.

## Decision

### A ref update is a signed proof that counts once

A ref update is a new type of proof with a type tag of its own: an SSHSIG signature with the key
of the register entry, as the proofs of phase 4, independent of the token (ADR-024). The signed
payload is line-oriented text in a fixed order:

- the public address of the hub, as in the challenge;
- the ID that the hub assigned to the repository (ADR-027);
- per reference its qualified name (`heads/<name>` or `tags/<name>`), `previous`, `old` and `new`
  (a commit or `-`, so creating and deleting are the same update), and a line for the evidence
  that the move rests on: the hash of the opening of a pull request (a merge), the hash of a purge
  statement or of an owner override, otherwise `-`;
- the public key.

`previous` is the hash of the last record entry of this reference name, or `-` for a name that the
hub never saw. It takes the place of the bare commit ID as `expected`, so an update does not count
again when the old value of the reference returns (A, B, A). The chain of a name survives its
deletion. `old` is redundant to `previous` and makes an entry readable on its own. A tag knows
only `-` to a commit and a commit to `-` (ADR-008).

The payload holds no time, no name of the actor and no message: the actor follows from the key and
the register, and the time is set by the hub. The list of references names the ID in a line; the
client keeps it in the configuration of the remote and notices when another repository lies under
the same address. The payload binds the repository by its ID, so a rename voids
no prepared update, and an update with `previous -` cannot be replayed into a new repository that
took over an old name.

One payload may carry several references under one signature, all or nothing: the hub checks all
of them under the one lock of ADR-008 and moves no reference if one check fails, and the answer
names every refused reference with its reason.

The signature counts against the register at the tip of `main` on the hub before the update. If
the update moves `main` itself, that is the old value and never the pushed commit; the first push
stays the exception of ADR-024.

Rejected: one chain per repository as the value that the actor signs (every foreign push makes a
prepared update `Stale`), a nonce of the hub per update (one more request, and state beside the
record), the name of the actor and the time of the client in the payload, the signed entry `push`
of the operation log as the update (against ADR-011), one reference per request, and the address
of the repository in place of its ID.

### The record of ref updates

An entry of the record has two parts and a signature:

1. The signed payload of the actor, byte for byte as received.
2. The lines of the hub: a running number in the repository, the time of the hub, the commit of
   `main` whose register it used, the commits of the policies by which the verdict was reached as
   a list (ADR-027), per reference whether the move was a fast-forward, the identity of the actor
   from the register (a statement of the hub, no proof), and the hash of the previous entry of the
   repository.
3. The signature of the hub over the hash of the entry (ADR-026). It stands outside the hash, so
   `previous` does not depend on it.

All entries of a repository thus form a hash chain in the manner of ADR-011 that the hub alone
writes. It fixes the order across all references, and no actor signs it. The hash of an entry
covers both parts, so `previous` is bound to the number and the time as well.

- One entry is written for one request, before the first move, so a stop between two moves is
  completed from the entry (the order of ADR-011). The entry is the next link in the chain of
  every name it lists.
- The record holds moves only. An entry arises exactly when references move, so the chains have no
  links without effect and a sender cannot fill the record with refused requests. Refusals belong
  to the operational log of the hub, which is no format of the protocol.
- The record is evidence of what happened and keeps nothing alive. It is not a root of the removal
  of unreachable objects (ADR-007), so a rewritten history can leave the hub. A reader of the
  record always checks the chains and the signatures, and checks that the key was in the register
  only where the register commit is still present; the rest is reported as no longer checkable,
  not as damaged.
- The answer to an accepted update is the whole entry, as a receipt. The client checks that it
  contains its payload unchanged and the signature of the hub, and only then moves its remote
  references.
- The record is readable through the protocol, a range by numbers, by everyone who may read the
  repository. A clone holds no copy of it.
- A stale update is a 409 whose error message names every reference whose `previous` does not
  match, with its current state on the hub (the commit and the hash of the entry, or `-`). The
  client moves no remote reference and does not repeat on its own, because it would have to sign
  again and the actor decides first.

The format, the writer and the strict reader of the record live in `stemma-core`, as those of the
operation log do. The core writes the entry into a destination that the caller passes; the layout
of ADR-009 gets no file that only the hub knows.

Rejected: an entry type for refused requests, the record as a root of the removal, a record that
only the operator reads, a record that travels with every fetch, and a hub that writes the entry
between two calls of the core.

### The evidence is signed statements, and the log records the act

The opening of a pull request, an approval, a check result, a closing, an owner override and a
purge of `main` are signed statements of their own: new types of proof with type tags of their
own, line-oriented, types of `stemma_core::protocol`. They bind the repository, the pull request
and the commits, and no number, chain or time of a clone.

- The hash of the signed opening names the pull request. The opening names the repository ID, the
  branch, the head, the reviewers, `repairs` and the public key. Nobody assigns the name, it holds
  without a hub, and a hub that is taken over cannot move an approval to another pull request. An
  ended opening is final: the same opening is not accepted twice, and the next pull request needs
  a new head.
- A clone holds a set of statements (new files of the layout, ADR-009), and the hub holds one per
  repository. The local gate and the accepting operation fold the same set with the same function.
- The log keeps recording what the clone did: the entries `pr-open`, `pr-approve` and `check` name
  the hash of the statement they made, and `fetch` names the statements received (ADR-011). One
  form holds everywhere, also without a remote; a statement then names no repository of a hub and
  counts locally only.
- A statement is submitted to the hub with a request of its own. The hub checks it, keeps it with
  its own signed lines (a running number, the time, the register commit, the number of the last
  record entry) and serves it to everyone who may read the repository. A fetch brings the
  statements of open pull requests into the clone.
- "Merged" is no statement. It follows from the record entry of `main` whose evidence line names
  the pull request. Move and merge are one step under one lock, so the state "open while `main`
  holds the change" (ADR-011) does not exist on the hub. Closing stays a signed statement of the
  opener or an owner.

No entry of the operation log reaches the hub. The statement of ADR-011 holds in the precise form
of ADR-026: a hub never sees an entry of the log.

Rejected: the log entries byte for byte with their proof files (the number and chain of one clone
would be bound to the hub, and an approval from another clone would name a pull number that does
not exist there), evidence as objects in the store (ADR-011), a running number of the hub as the
name of a pull request, two forms of evidence side by side, and "merged" as a statement of its
own.

### What the hub checks when a statement is submitted

The rules of the local writer of phase 4, against the register at the tip of `main`:

- the signature and an active key;
- an opening by the write owner only, with the branch at this head on the hub and at most one open
  pull request per branch;
- an approval only for the current head of an open pull request and never one's own;
- a check result that names commits that a reference of the hub reaches;
- a closing by the opener or an owner.

A check that does not count for a change stays as what it is, and the verdict at the merge counts
everything again. Statements count towards the quota of the repository.

### How `main` moves on a hub

The clone builds the merge commit and the hub checks it again. The gate runs in the clone as today
and builds `M` on the state `T` of the remote; the push moves `main` from `T` to `M`. The hub
recomputes:

- `M` has the single parent `T`, and its tree is the clean merge of the head `H` onto `T`, a pure
  function that creates no commit;
- the level, from `.stemma-policy` of `T` (the base commit, as locally), from the paths of the
  difference between `T` and `M` and from the escalation over old and new blobs (rule 10 of
  ADR-004), combined with the namespace policy by the maximum (ADR-027). The ref update names no
  level, and an `M` that changes `.stemma-policy` is judged by the old policy;
- the approvals and the check results, folded from the statements and counted against the register
  of `T`. `M` has the single parent `T`, so the base commit of ADR-003 and the tip of `main`
  before the update are the same commit: one rule for the sender, the ref update and the evidence;
- all bounds of ADR-019 (the pause at a red tip with the exemption of a repair, the reservation,
  `max_unverified`), computed over the order and the time in the lines of the hub.

The hub never merges and never creates a commit (ADR-015). Check results are statements of
registered actors; the hub runs no foreign code and knows no runner outside the register. The
result stays the statement of that actor, and nobody proves the run.

In a repository whose `main` follows a remote, `pr merge` fetches, judges locally and builds `M`
without a move, sends the ref update, and only after the receipt moves `main` and the remote
reference, with their entries. A refusal leaves unnamed objects only and nothing to take back. On
`Stale` the command repeats the loop of ADR-019 across the network, bounded, and signs again each
time. Offline, commits, branches, opening a pull request, approving and checks stay possible; only
the move of the shared `main` waits for the hub. Without a remote the gate stays as today.

Rejected: a hub that runs the gate and creates `M`, a hub that checks the role and the
fast-forward only, a level that the sender names, a runner that the hub trusts, and a local move
that is taken back after a refusal.

### Branches, tags and a purge on a hub

- The write owner of an unprotected branch on the hub is the actor of the last record entry of
  that name. The lease runs from the time of the hub in that entry, with the duration from the
  policy at the tip of `main`. The hub gets no new state, and every reader of the record can
  recompute it. Before the expiry a push of another actor is refused; after it, whoever has a role
  that may write takes the branch (rule 1 of ADR-004). The local lease stays for actors that share
  a clone.
- The owner override is a signed statement with a reason that the hub keeps and shows to all
  readers of the repository (rule 9 of ADR-004, ADR-027).
- The write owner may make any move on an unprotected branch, and the record says that it was no
  fast-forward (the way of ADR-019 for a rebase).
- `main` otherwise moves from `T` to `M` only. The one exception is the purge: a human owner by the
  register of `T` presents, with the ref update, a signed purge statement (the old tip, the new
  tip, a reason), and the new history must carry a register that holds this owner. The hub applies
  no gate, the entry is visible to all readers, and statements about removed commits no longer
  count (ADR-014).
- A tag is set and deleted by the same rule as locally: the authorizer grants `tag` or `untag` by
  the register at the tip of `main`. The chain of the name survives the deletion, so the record
  shows that a tag was deleted and set again.

### The accepting operation

The core has two entries and one shared verdict:

- `Repository::accept_ref_update` takes the update and a context and returns the receipt or a
  typed refusal.
- `accept_statement` takes a statement and the context and returns the lines of the hub or a
  refusal.

The context carries the address of the hub, the repository ID, the destination of the record and
of the statements, the time, the signer of the hub (ADR-026), and the namespace policy with the
demanded owners (ADR-027). The operation tells the cases apart itself: `main` (recompute the gate,
or a purge), a branch (write ownership), a tag (`tag` and `untag`); all references of an update,
all or nothing. The verdict about `main` is a pure function over the commits, the register and the
policy of `T`, the namespace policy and the statements, which `merge_gate` calls locally too. The
rule stands in one place, and the in-memory `Transport` covers it in the public CI (ADR-022).

The operation is one call that takes `refs.lock` itself in the middle, as `merge_gate` does:

- Before the lock only pure functions over immutable objects may run, against the `T` that the
  update names: form and signature, the tree merge, the difference, the level and the escalation.
- Under the lock stands everything that depends on mutable state: `previous`, the connectivity
  (ADR-023), register and authorizer, the fold of the statements and the bounds, the record entry
  with its signature, and the move.
- Submitting a statement takes the lock as well, so statements and ref updates have one order.
- The first version may compute everything under the lock. The trigger for moving work before it
  is the measured holding time.

The upload of objects runs without the lock (ADR-007), and reading takes no lock: a fetch reads
references and objects while a push runs, and the list of references is not a snapshot across all
references, which is enough because the client names only commits that the hub had. The gate needs
no ref update without a global lock; the open item of ADR-015 stays open, with the measured
waiting time as its trigger.

Six properties are requirements on the slices, so that a later hub can compute for an agent
without a clone (ADR-015):

1. The tree merge and the verdict about `main` stay pure functions that are callable on a bare
   repository without the lock, without creating a commit and without moving a reference.
2. The accepting operation never demands from the sender what only a clone has: no log, no
   anchor, no ID of a log.
3. The upload and the connectivity check do not presuppose that every object comes from the
   sender.
4. The store of a bare repository may receive objects from a computation as well as from an
   upload, and the quota and the removal treat them as every unnamed object.
5. The record entry needs no second kind of signed part.
6. Every later request is a new protocol number and changes no existing request.

### A refusal names its reason, and only the evidence is shared

Everything that the register, the policy, the write ownership or the gate refuses is a 403 with
the full typed reason, per refused reference: the role, the holder of the lease and its expiry,
the computed level with the reason of an escalation, the approvals and checks counted and missing,
every statement that does not count with its reason, the bound, or that `M` is not the clean
merge. Nothing leaks: who gets a 403 may read and could compute all of it. The type belongs to
`stemma_core::protocol` and is the same as the local refusal.

A refusal is recorded in no shared format. Visible to all readers is what the verdict rests on:
the statements, the record and the repository, from which every clone recomputes why a pull
request cannot merge.

### The order of the hub

A statement stands in the order of the hub directly: it lies before a retirement or a revocation
if its lines name a smaller record number than the entry that moved `main` there. A statement
names no log, so evidence from a log with a contradicting anchor (ADR-026) still counts; only a
revocation on `main` takes the effect from the statements of a key.

## Consequences

- A process that writes a reference, a statement or a log entry past the core never reaches the
  hub without a key, and `main` moves from `T` to `M` only with the gate recomputed. Two states
  reach `main` without the gate: the first push into an empty repository, whose register must
  hold the key of the request as an owner (ADR-024), and the purge of a human owner. Points 2 and
  8 of the hand-over in ADR-003 are met.
- The formats of phase 4 get a portable form: the payload of an approval and the proof of a check
  become statements, the entries `pr-open`, `pr-approve` and `check` name a hash, and the fold of
  the gate reads from the statements. This is a new format version (ADR-009, ADR-011).
- The hub holds signed statements about a repository. It sees who opened, approved and checked
  what, and no log.
- A clone comes to the same verdict as the hub, except for what arrived since its fetch.
- One lock per repository serializes ref updates and statements. At hundreds of merges per day
  this is accepted.

## Open items

- [ ] How clones follow a `main` that was purged on the hub, how a purge treats remote references
      in a clone, and what becomes of the statements of a clone about removed commits (ADR-014).
- [ ] How commands name a pull request for people (a prefix of the hash, a running number in the
      lines of the hub). It belongs to the command surface.
- [ ] A policy that names the writers of checks. Possible later without a break, because it only
      tightens.
- [ ] A list of refused attempts per namespace is not built (ADR-027).
