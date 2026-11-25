# ADR-025: Ref updates, signed statements, and the acceptance on a hub

**Status:** Accepted for phase 5. Decided by the author on 2026-11-23 and 2026-11-24 (STEMMA-56
for the ref update and its record, STEMMA-273 for the enforcement, with the addenda of STEMMA-274,
STEMMA-276 and STEMMA-278) and carried into the ADRs by STEMMA-280. Nothing of it was measured.
Built are the formats of the ref update and of the record with their writers and strict readers
(STEMMA-291, see "The formats as built"), and the signed statements with the set of a clone, the
entries of the log that name them, and the fold (STEMMA-290, see "The statements as built"). The
accepting operation, the lines that a hub adds to a statement, and the request that submits one
are not built. The names of functions that are not built yet are a direction for the slices.

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

### The formats as built

Built in STEMMA-291, in `stemma_core::protocol::record`. Both formats are line-oriented text by
the rules of ADR-023: a line ends with a line feed, and a field is separated by one space. New
with these formats is that a value that is not there is written `-`.

```text
stemma-protocol 1                                   the ref update
hub <address of the hub>
repository <32 lower-case hexadecimal digits>
ref <qualified name> <previous> <old> <new>         one or more pairs of `ref` and `evidence`,
evidence <hash of a statement>                      in the order of the qualified names
key ssh-ed25519 <base64>
signature <SSHSIG signature on one line>

<the ref update, from its first line on>            the record entry, and so the receipt
number <n>
time <seconds since the Unix epoch>
register <commit>
policy <commit>                                     zero or more
fast-forward <qualified name> <yes or no>           one for each `ref` line, in the same order
actor <human or agent> <id>
previous <hash of the entry before, or `-`>
hub-signature <SSHSIG signature on one line>
```

- **The payload.** The actor signs the lines from `hub` to `key`, as a proof with the type tag
  `ref-update`. The first line and the `signature` line are the message around it. The address of
  the hub is the part of the address of a remote before `/<namespace>/<repository>` (ADR-023), by
  the same rule of spelling. The references stand in the order of the bytes of their qualified
  names, each once, at least one. The evidence has the form of an object ID; how the hash of a
  statement is computed belongs to the slice of the statements.
- **A move.** A reader and the constructor refuse the same three things: `old` equal to `new`,
  which includes `-` and `-` (the record holds moves only); a tag with a commit before and after
  (ADR-008); and an `old` that names a commit while `previous` is `-`, because a name that the
  hub never saw has no value.
- **A signature on one line.** A signature is the Base64 body of the armored SSHSIG form without
  its line breaks, as the `cosign` line of the register writes it. A reader refuses every text
  that is not what the writer writes for the signature it read.
- **The entry.** The first part is the ref update byte for byte as received, its first line and
  the signature of the actor included, so a receipt begins with the message that the client
  sent. The hash of an entry is the BLAKE3 hash of the line `stemma-record-hash 1` and
  everything before the `hub-signature` line. It covers the signature of the actor too: a reader
  checks that signature without the hub, and the hub cannot exchange it afterwards. The hub
  signs the text of the hash as a proof with the type tag `record-entry`.
- **The lines of the hub.** The number counts from 1. The first entry of a repository names
  `previous -`, and no other entry does. The policies stand in the order in which the hub names
  them. `fast-forward` names each reference of the update again and says `yes` only for a
  reference that exists before and after the move: a reference that is created or deleted is
  never a fast-forward. The actor is the identity of the register, `human <id>` or `agent <id>`.
- **Requirement 5 of the accepting operation.** The entry holds one part that an actor signs, the
  payload of the ref update. Everything else is a statement of the hub under its one signature.

The readers hold the rule of ADR-005 and check the signatures as well: the signature of the actor
against the key of the payload, and the signature of the hub against the key that it names. A
signature of the actor that names another key than the `key` line is refused as such, apart from
one that names that key and does not verify, so a sender can tell a wrong key from a changed
payload.
A value of either type therefore carries signatures that verify.

Whether the keys are the right ones is asked apart: the key of the hub against the key that the
clone pinned and the former keys with their ranges (ADR-026), and the key of the actor against
the register of the register commit.
That last check answers "no longer checkable" where the commit, its tree, a tree on the path of
the register or the register file itself is gone.
It refuses where the commit is there and records an open conflict, which `main` never holds
(ADR-019), where it holds no register, where the register does not hold the key as an active key
of the actor, and where the register cannot be read: the store fails, the path of the register is
taken by something that is no file, or the register is not valid.
A store cannot tell an object that was removed from one that never was there, so "no longer
checkable" leaves the actor a statement of the hub alone.

The chains have one rule in one place (`Chain`). It holds the number and the hash of the last
entry, and for every name the hash of its last entry with the commit that this entry left; a
deleted name stays without a commit. An update continues the record if it names the repository
of the record and every reference names the last entry of its name as `previous` and the commit
that it left as `old`. The writer (`Chain::append`) and the reader (`Chain::accept`) both ask
this question, so an entry that was written past the writer is refused when it is read. The
writer takes the destination as a byte sink and the signer of the hub as a function from bytes
to a signature (ADR-026); every refusal comes before the first byte is written. The writer
flushes and does not sync: the caller makes the destination durable before the receipt leaves
the hub, and cuts it back to its old length after a failed write.

- A reader that begins in the middle of a record, such as a clone with a receipt, does not know
  the names of the entries before. It takes the first update of a name as it is and checks the
  later ones; the chain of the repository is checked from the first entry that it reads. Such a
  chain reads only: the writer refuses it, because the reader of the whole record checks every
  name.
- The chain does not compare the address of the hub in a payload. A hub may move to another
  address (ADR-026), so one record can name several; that an update names the address under
  which the hub was called is asked by the accepting operation, which knows that address.
- An entry that carries the number of the last entry and has another hash is reported as a
  rewritten record. Two such entries prove the rewrite where the key of the hub for that number
  signed both (ADR-026).
- A record is its entries one after the other, as the writer appends them to one destination.
  An entry ends with its `hub-signature` line, so a reader splits a record there and reads each
  entry once.
  The message that carries a range of the record through the protocol comes with its request.
- The answer to a stale update is typed in the core (every reference that does not match, with
  the hash of its last entry and its commit); the error message that carries it comes with the
  accepting operation.

Example messages are checked in beside those of ADR-023 (`ref-update`, `receipt`). Their keys
come from fixed seeds of the tests, so their signatures are reproducible; nobody may trust them.

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

### The statements as built

Built in STEMMA-290, in `stemma_core::protocol::statement`, for the opening of a pull request, an
approval, a check result, a closing, and an owner override. The purge statement comes with the
purge on a hub. A statement is line-oriented text by the rules of ADR-023, and a value that is
not there is written `-`, as in the ref update.

```text
stemma-protocol 1
statement <kind>
repository <32 lower-case hexadecimal digits, or `-`>
<the lines of the kind>
key ssh-ed25519 <base64>                         or `key -`
signature <SSHSIG signature on one line>         or `signature -`

pr-opening       branch <name>
                 commit <head>
                 reviewer <human or agent> <id>   zero or more, in ascending order
                 repairs <commit, or `-`>
pr-approval      pull <hash of the opening>
                 commit <head>
check-result     commit <commit>
                 base <commit, or `-`>
                 result <pass or fail>
pr-closing       pull <hash of the opening>
owner-override   branch <name>
                 holder <human or agent> <id>
                 commit <commit, or `-`>
                 reason <one line>
```

- **The payload and the type tag.** The key signs the lines from `repository` to `key`, as a
  proof whose type tag is the word of the kind: `pr-opening`, `pr-approval`, `check-result`,
  `pr-closing`, or `owner-override`. None of them is a tag of phase 4, so the signature of an
  approval of phase 4 (`approval`, over the number of a pull request in one log) never stands
  for a statement. The line `statement` is no part of the payload; the type tag stands for it,
  so a statement whose kind was changed carries a signature that does not verify.
- **The repository.** `repository` is the ID that the hub assigned (ADR-027), or `-` for a
  statement that names no repository of a hub and counts locally only. The writers of a clone
  write `-` until the slice that submits statements gives them the ID of a remote.
- **No actor, no number, no time.** The actor follows from the key and the register. In a clone
  the entry of the log that names the statement gives it its number, its time, and its actor
  (ADR-011); on a hub the lines of the hub do.
- **The opening.** The branch is not `main`. The reviewers are identities of the register
  (`human <id>` or `agent <id>`), in the ascending order of their text, each once, at most 32.
  That the opener is none of them is a rule of the fold, which knows the opener.
- **The check result.** `base` is the tip of `main` that the commit was merged onto for the
  run, or `-` for a run on the commit as it is; it is never the commit itself.
- **The owner override.** It names the branch, the holder whose write ownership is overridden,
  the commit at the tip of the branch (or `-` for a branch that does not exist), and the
  reason: one line of 1 to 1,024 bytes without a control character, without a line or
  paragraph separator, and without a space at its ends. The commit binds the statement to one state of the branch. The local entry
  `lease-override` does not name it yet (ADR-011).
- **The hash.** The hash of a statement is the BLAKE3 hash of the line `stemma-statement-hash 1`
  and the whole message, the signature included. It has the form of an object ID and is the
  evidence that a ref update names. The hash of the opening names the pull request. The hash
  covers the signature, so the same opening signed by another key is another pull request. An
  Ed25519 signature is deterministic, so signing the same payload again gives the same
  statement; that is what makes "the same opening is not accepted twice" a question of one
  hash.
- **A statement without a signature.** A repository in the open mode has no register and no
  key (ADR-003), and its evidence has the same form: a statement with `key -` and
  `signature -`, both or neither. It proves nothing. It counts where the verdict is reached
  without a register, and whoever holds a register refuses it, a hub among them. This keeps
  one form of evidence where phase 4 had entries without a proof.
- **The reader.** It holds the rule of ADR-005 and checks the signature of a signed statement
  against the key that the statement names. A signature that names another key than the `key`
  line is refused as such, apart from one that names that key and does not verify. Whether the
  key is the right one is asked apart, against a register (`Register::verify_statement`).
- **The set.** A clone keeps its statements as files named by their hash (ADR-009); the type
  takes its directory from the caller, so a hub keeps its set with the same code.
- **The fold.** `evidence::fold` takes the statements in the order of whoever holds them, each
  with its number, its time, and its actor, and the merges that the holder knows ("merged" is
  no statement), and returns the pull requests with their state, the approvals, and the
  checks. A clone calls it with the entries of its log as that order (ADR-011), and the local
  gate reads its result (ADR-019); a hub calls the same function with its lines. The fold is
  tolerant: it skips a closing or a merge of a pull request that it does not
  know, whatever follows the end of a pull request, an opening that is placed a second time,
  and an opening whose opener is one of its reviewers. It lists every approval and every
  check as it is, and where the statement of a closing or of a failing run cannot be read, a
  clone places it from the lines of its entry, because what only holds a change back must not
  vanish with a file. Which approvals and checks count is
  decided by the rules of phase 4, unchanged, which now ask the register for the signature of
  the statement.

Example messages are checked in beside the others (`statement-pr-opening`,
`statement-pr-approval`, `statement-check-result`, `statement-pr-closing`,
`statement-owner-override`, `statement-unsigned`), with keys from fixed seeds; nobody may trust
them.

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
- After the first push (ADR-024), `main` moves from `T` to `M` only. The one exception is the
  purge: a human owner by the register of `T` presents, with the ref update, a signed purge
  statement (the old tip, the new tip, a reason), and the new history must carry a register that
  holds this owner. The hub applies no gate, the entry is visible to all readers, and statements
  about removed commits no longer count (ADR-014).
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
      Until it is decided, a local purge makes stubs of the entries and leaves the set alone:
      the statement of a stub stays as a file that no entry names and counts for nothing
      (STEMMA-290). It names the IDs of removed commits, as the name of a purged blob does on
      the command line (ADR-011).
- [ ] The statements that the writers of a clone make name no repository of a hub
      (`repository -`), so they count locally only. Which remote gives a statement its ID, and
      whether a statement made before the first push is signed again, belongs to the slice that
      submits statements.
- [ ] An opening and a closing are not counted against the register by the fold of a clone,
      so one without a signature makes or ends a pull request in an enforcing repository too.
      `op verify` reports a signature that the register does not accept and counts the
      unsigned ones, and a hub checks every statement when it is submitted.
- [ ] How commands name a pull request for people (a prefix of the hash, a running number in the
      lines of the hub). It belongs to the command surface.
- [ ] A policy that names the writers of checks. Possible later without a break, because it only
      tightens.
- [ ] A list of refused attempts per namespace is not built (ADR-027).
