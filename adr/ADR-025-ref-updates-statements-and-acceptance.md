# ADR-025: Ref updates, signed statements, and the acceptance on a hub

**Status:** Accepted for phase 5. Decided by the author on 2026-11-23 and 2026-11-24 (STEMMA-56
for the ref update and its record, STEMMA-273 for the enforcement, with the addenda of STEMMA-274,
STEMMA-276 and STEMMA-278) and carried into the ADRs by STEMMA-280. Nothing of it was measured.
Built are the formats of the ref update and of the record with their writers and strict readers
(STEMMA-291, see "The formats as built"), the signed statements with the set of a clone, the
entries of the log that name them, and the fold (STEMMA-290, see "The statements as built"), and
the accepting operation for branches and tags (STEMMA-296, see "The accepting operation as
built"), and the lines that a hub adds to a statement, the request that submits one, the two
reading requests and `accept_statement` (STEMMA-299, see "The statements on a hub as built"),
and the gate on a hub for a move of `main` (STEMMA-303, see "The gate on a hub as built"),
the commands that submit and read statements (STEMMA-312, see "The commands of the statements as
built"), and the client side of a push and the merge of a pull request with a remote
(STEMMA-309, see "The push and the merge with a remote as built"). The purge and the owner
override on a hub are not built. The names of functions that are not built yet are a direction
for the slices.

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
  the hash of its last entry and its commit); the error message that carries it is `stale` (see
  "The accepting operation as built").

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
  clone places it from the lines of its entry, because what says something against a change
  must not vanish with a file. Such a stand-in may only make things stricter where a register
  is asked (ADR-011). An enforcing clone places an opening only if its register accepts
  the signature for the opener. Which approvals and checks count is
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

### The accepting operation as built

Built in STEMMA-296 for branches and tags: `Repository::accept_ref_update` in `stemma-core`, with
the reasons of a refusal in `stemma_core::protocol::refusal`. The purge and the owner override
are not built; `accept_statement` is (see "The statements on a hub as built"), and so is the
gate on a hub (see "The gate on a hub as built").

**The call.** It takes the ref update and a context: the address under which the hub was called,
the ID of the repository, the destination of the record, the time of the hub, and the signer of
the hub. It returns the record entry, which is the receipt, or an error. Two errors are answers
to the sender, `Stale` and `Refused`; every other error is a failure of the hub (the lock is
held, the record cannot be read or written or is damaged, the store or the signer fails). The
call works on a bare repository only: a repository with a working copy moves its references
through the operation log (ADR-011).

A policy at the tip of `main` that is missing, no file or not valid refuses the reference
(`policy`), because the sender can read that commit; a policy that the store cannot return is a
failure of the hub, classified as damage where an object on the way to it is missing or not in
canonical form. The commit of a first push that records an open conflict is refused as
`bad-register`: `main` never holds such a commit (ADR-019).

**The time of the hub does not step back.** The leases are counted by the time in the entries,
so an entry never carries an earlier time than the entry before it; the same second is allowed.
The rule stands in the chain: the writer refuses such an entry before it writes, which the
accepting operation reports as a failure of the hub, and the reader of a record refuses it too.
A chain that began in the middle knows no time until it has read an entry. A hub whose clock
stepped back below the last entry therefore accepts no update until its clock has passed it;
the leases are judged by that clock until then.

**The destination of the record** is a trait with two methods: read the whole record, and append
the bytes of one entry durably. The operation reads the record under the lock before it judges,
so a second process of the hub cannot write between that reading and the entry. This first
version reads and checks the whole record with every call, which is linear in the entries; the
trigger for keeping
the state of the chains between two calls is the measured holding time of the lock.

**The order.** Before the lock: the address and the ID against the payload, and nothing of the
repository. The signature was checked when the update was read, so only pure functions run
before the lock, as the rule above demands. The lock of the references opens a write with the
proof of the authorizer only (ADR-002), and the question to the authorizer depends on the
register at the tip of `main`, which is read under the lock. The operation therefore takes the
lock as a value that opens no write and turns it into the lock of a writer with the proof, once
all checks of the update have passed. For an earlier update that stopped, the question is asked
again by the register that its entry names, and its moves are made with that proof. Under the
lock:

1. The record is read and its chains are checked.
2. An update that stopped after its entry is completed (see below), and every branch and tag
   must be where the record leaves it. A reference that is elsewhere was moved past the
   operation; the call fails and repairs nothing.
3. `previous` and `old` of every reference against the chain of its name (`Stale`).
4. The names of new references: a reserved prefix (ADR-008), and a name that another reference
   holds in the file system, in the store or in the update itself, also one that differs in the
   case of ASCII letters only.
5. The connectivity of every new commit (ADR-023).
6. The sender: the entry of the register at the tip of `main` before the update that holds the
   key of the update as an active key. Without a `main`, the register of the commit that the
   update sets `main` to, in which the key must be an active key of an owner (ADR-024).
7. For every reference the authorizer, with the operation `push` for a branch, `tag` to set a
   tag and `untag` to delete one, and for a branch the write ownership.
8. The record entry with the signature of the hub, made durable by the destination.
9. The moves, the deletions first, and the receipt.

All refusals come before step 8, so only accepted updates are recorded, and a refused update
moves none of its references. Step 2 may write before a refusal: it makes the moves of an
earlier, accepted update.

**Branches.** The write owner is the actor of the last record entry of the name, and the lease
ends at the time of that entry plus the duration that `.stemma-policy` at the tip of `main`
names. Another actor is refused before that time, with the holder and the end of the lease, and
takes the branch from that time on. Every accepted move of a name is its next entry, so a move of
the write owner begins a new lease. A name whose last entry deleted the branch has no write
owner: nothing is there to own, and the chain of the name still shows who deleted it. The entry
names the commit of the policy in a `policy` line exactly when the verdict rested on it, which
is when a lease had ended and another actor took the branch; a move of the write owner and the
creation of a free name read no policy.

**Tags.** A tag has no write owner. It is set and deleted by the authorizer, and it never moves
from one commit to another (ADR-008).

**`main`.** The first push sets it, alone or with further references, which are judged by the
register of the pushed commit as well. A repository without `main` accepts no update that does
not set it. Every later update of `main` goes through the gate on a hub, for an owner too (see
"The gate on a hub as built"); the deletion of `main` is refused with `gate -`, as no move to a
merge.

**The actor.** The payload names no actor. The identity follows from the key, which stands once
in a register. The authorizer is the one of an enforcing repository, as locally, so it is asked
with an actor: for an agent, the model and the session, which are no part of the identity, are
`-`, and the sponsor is the one of the register entry.

**A stop between the entry and the moves.** The entry is durable before the first reference
moves (the order of ADR-011), so a process that ends in between leaves an accepted update whose
references stand partly at their old values. The next call for the repository reads the last
entry under the lock and makes its moves again before it judges its own update, with the proof
of the authorizer for the actor of that entry by the register that the entry names. An entry
whose moves that register does not allow (no register, the key not an active key of the actor
that the entry names, or a move that its authorizer refuses) was not written by the operation;
the call fails. No earlier entry
can be incomplete, because every call completes the last one before it appends. A reference of
the last entry that is at neither its old nor its new value was not left by a stop; the call
fails. A call whose storage fails after the entry reports that the entry is written and names
it, and the hub sends no receipt. The list of references shows the update once the next call
has completed its moves; until then a reference of it can still stand at its old value.

**The answers.** Both are kinds of the error message of ADR-023, read under number 1 only:

```text
stemma-protocol 1                                   a stale update (409)
error stale
ref <qualified name> <entry> <commit>               one or more, in the order of the names

stemma-protocol 1                                   a refused update (403, or 422)
error refused
update <hub, repository, key or no-main> <fact>     one `update` line,
ref <qualified name> <reason> <facts>               or one or more `ref` lines
```

- `stale` names every reference whose `previous` or `old` does not match, with the hash of the
  last entry of its name and its commit, each `-` where there is none. A reference of the update
  that matches is not named.
- `refused` with an `update` line refuses the update as a whole: `hub <address>` and
  `repository <id>` name what the hub is where the update names another, `key <commit>` names
  the commit of `main` whose register does not hold the key, and `no-main -` answers an update
  that does not set `main` in a repository without one.
- `refused` with `ref` lines names every refused reference with its reason: `denied <operation>`,
  `leased <human or agent> <id> <end of the lease>`, `gate -`, `reserved -`, `no-register -`,
  `bad-register <text>`, `not-owner -`, `conflict <the name in the way>`, `incomplete <text>`,
  `policy <text>`, and `namespace-owner <human or agent> <id>` (ADR-027, STEMMA-311). A reference that is not named was not refused itself, and it did not move
  either. A refusal with an `incomplete` reference is the 422 of ADR-023: the client uploads
  once more.
- After a refusal of the names or of the connectivity of the commit of a first push, the other
  references are not judged, because their register lies in that commit.

A reason fits its reference, for the constructor and for the reader: `gate`, `no-register`,
`bad-register`, `not-owner` and `namespace-owner` are reasons of `heads/main`, `leased` and `policy` of another
branch, and `denied` names `push` for a branch and `tag` or `untag` for a tag. The name of a
`conflict` is a reference of the same kind or a directory of them, written as a branch name.

The reasons are one type, `Refused`, which the hub answers and the client reads. The holder of a
lease is an identity there, as the record names it, where the local refusal names the actor of
the lease file.

### The statements on a hub as built

Built in STEMMA-299: `Repository::accept_statement` in `stemma-core`, the held statement, the
range and the reasons of a refusal in `stemma_core::protocol::held`, and the three requests in
`Transport`.

```text
<the statement, from its first line to its `signature` line>   a held statement, and so the
number <n>                                                     answer to a submission
time <seconds since the Unix epoch>
register <commit>
record <n>
actor <human or agent> <id>
hub-signature <SSHSIG signature on one line>

stemma-protocol 1                                              the request that reads the
range <first number> <last number, or `-` for the end>         statements or the record

stemma-protocol 1                                              a refused statement (403)
error statement-refused
refused <reason> <facts>

stemma-protocol 1                                              a repository that the requester
error not-readable                                             may not read
```

- **The lines of the hub.** The running number counts the statements of the repository from 1,
  the time is the time of the hub, `register` is the tip of `main` whose register was used, and
  `record` is the number of the last record entry. The `actor` line is the identity that the
  register names for the key, as in a record entry: a statement of the hub and no proof. The
  four lines of the decision above are joined by it, because a reader places a statement with
  an actor and should not need the register of an old commit for that.
- **The hash and the signature.** The hash of a held statement is the BLAKE3 hash of the line
  `stemma-held-hash 1` and everything before the `hub-signature` line, the statement with its
  signature included. The hub signs the text of the hash as a proof with the type tag
  `held-statement`, as it signs a record entry. The reader judges the form of the whole message
  first, then the signature of the actor, then the one of the hub.
- **The order of the hub.** A held statement comes after the record entry that its `record`
  line names and before the next one, and the held statements among themselves by their
  numbers. It lies before a record entry whose number is larger than its `record` line
  (`HeldStatement::lies_before`), so before a retirement or a revocation that this entry
  brought to `main`. The fold of a hub takes the held statements and the merges of the record in
  this one order; a merge is an entry that moves `main` with an opening as its evidence. The
  fold takes the actor of each step from the identity in its lines; a step whose identity a
  later register no longer holds still counts, so a closing or a merge of a removed actor does
  not open its pull request again.
- **The destination.** The hub keeps its held statements one after the other in a destination
  that it passes with every call (`StatementDestination`), as it keeps the record; the layout of
  ADR-009 gets no file. It is a parameter of `accept_statement` beside the context of the ref
  update, so the context keeps the form that the ref update built.
- **The order of the call.** Before the lock: the repository that the statement names, which
  must be the one it was sent to (a statement with `repository -` counts locally only and is
  refused), a signature, and a kind that this version accepts. An owner override is refused as
  a kind of its own (`kind owner-override`) until the slice of the override on a hub. Under the
  lock the call reads the record, completes an update that stopped, checks that the references
  agree with the record, as the ref update does, and reads the held statements. A statement that
  the hub holds already is answered with the lines that it holds it with, so a sender whose
  answer was lost submits again; an opening whose pull request ended is final, and the same
  opening submitted again is refused with `not-open`, as the local writer refuses it. Then the
  key must be an
  active key of the register at the tip of `main`, the authorizer must allow `pr-open`,
  `pr-approve`, `pr-close` or `check`, and the rules of the kind hold:
  - an opening: the opener is none of its reviewers, the branch is at the head of the opening on
    the hub, the opener is the write owner of the branch where a lease applies (the actor of the
    last record entry of the branch, before the end of the lease that the policy at the tip of
    `main` names), the branch has no open pull request, every reviewer is in the register, and a
    repaired commit is reached by a reference;
  - an approval: the pull request is known and open, its branch is at the approved head on the
    hub, and the approver is neither the opener nor a writer of a commit that the pull request
    brings: its author, or the actor of the record entry that first brought it under a branch or
    a tag, the rule of the local writer with the record in the place of the log;
  - a check result: the commit and the base are reached by a branch or a tag;
  - a closing: the pull request is known and open, and the closer is the opener or an owner.
- **A check that does not count** for a change is accepted all the same; the verdict at the
  merge counts everything again.
- **The time** of a held statement is never earlier than the time of the last held statement or
  of the last record entry, by the rule of the record; a hub whose clock stepped back accepts no
  statement until it has passed them.
- **The reasons** of `statement-refused` are `repository <id>`, `unsigned -`, `no-main -`,
  `key <commit>`, `kind <kind>`, `denied <operation>`, `not-write-owner -`,
  `head <commit or ->`, `already-open <pull>`, `unknown-reviewer <identity>`, `self-review -`,
  `no-pull -`, `not-open -`, `self-approval -`, `not-closer -`, `unreachable <commit>` and
  `policy <text>`.
- **The reading requests.** The statements and the record are read by a range of numbers
  (`held_in`, `record_in`). Whether the requester may read the repository is an input of the
  caller, which knows its sign-in and its namespaces (ADR-024, ADR-027); a requester that may
  not is answered `not-readable`, which says as little about a repository that does not exist.
  The reading takes no lock. The destination of the statements and that of the record answer
  only what an append made durable, never the bytes of an append that runs or that failed, so a
  reader never serves a held statement or an entry that the hub then denies holding; a part of
  an entry that a crash left is damage, which the reader reports. A held statement or an entry
  that does not carry the number of its place is damage too, for the reader as for the
  writer, and so is a held statement whose `record` line or time lies before the one of the
  statement before it, or whose `record` line names an entry that the record does not hold.
- **The fetch.** `statements_of_open_pulls` selects from the held statements and the record the
  openings of the open pull requests, the approvals that name them, the checks of their heads,
  and every closing: the set of a clone only grows, so a clone that holds an opening from an
  earlier fetch learns its end only from the closing. That a merge ended a pull request the
  clone learns from the record, which the entry `fetch` reads as well. The actor of a step is
  the one that the register at the tip of `main` names for its identity, else the one of the
  register commit in its lines; where neither holds it, the identity alone, which the gate on
  a hub asks the register about itself. The clone puts them into its set after it checked that the pinned key
  of the hub signed them, for the entry `fetch` that names them (ADR-011). That entry and the
  wiring into `fetch` come with the fetch in the core (STEMMA-295). The lines of the hub are not
  kept in the clone yet: the entry `fetch` places the statements in the order of the clone.

Example messages are checked in beside the others (`held-statement`, `range`,
`range-to-the-end`, `error-statement-refused`, `error-not-readable`).

### The gate on a hub as built

Built in STEMMA-303: the verdict about `main` as one function of `stemma-core`
(`Repository::judge_main`, crate-internal), which `merge_gate_by_policy` calls in a clone and
`accept_ref_update` calls on a hub, and the reason `gate <verdict> <facts>` of a refused `main`.

**The verdict.** It reads the object store and what its caller passes, and nothing else: the fold
of the statements, the order in which the caller holds them, the heads of its branches, the
register and the policy of `T`, further policies, and the time of the caller. It takes no lock,
reads no log and no record, creates no commit and moves no reference; the tree merge writes blobs
and trees only, so the requirement 1 above holds. In this order: the branch is at the head of the
opening, the merge of the head onto `T` is clean (a merge that changes nothing means that `main`
holds the change), the level is the maximum over the policies of the level of the paths and of
the escalation for weakened tests over the old and the new blobs, a check counts by the rule
table of ADR-019, the approvals suffice, and the bounds of ADR-019 hold. The approvals are asked
where a register and a policy are: as many approvals of registered agents as the policies name
for the level (the highest number of all of them), and at the levels 2 and 3 one approval of a
registered human, at level 3 of an owner. They are counted by the recount of phase 4, with the
writers of the change of the holder: the log in a clone, the record on a hub. A caller that gives
its own level (`merge_gate`) gets no escalation and no approvals, as before. The bounds take
`max_unverified` and `N` as the strictest value of the policies.

**The signatures.** The holder judges them. A clone asks the register for the number of the entry
of its log, as before. A hub asks the register of `T`, and a key that is not active there counts
by the order of the hub: a retired key for a statement that lies before the first record entry
that moved `main` to a commit whose register holds the key as no longer active, a revoked key the
same for every kind but an approval, whose statements of a revoked key never count.

**On a hub.** The context carries the destination of the held statements and the namespace of
the repository (`AcceptContext::with_statements`, `with_namespace`, ADR-027): its policy counts
beside the policy of `T` by the maximum, its commit stands in the `policy` lines of the entry
after the commit of `T`, and the owners that it demands must stand in the register of the first
push and of every `M` (`namespace-owner`). For a move of `main` after the first push, the operation asks the authorizer
for `push` and then the gate: `M` is a commit with the single parent `T`, its evidence line names
an opening that the hub holds and whose pull request is open, the hub folds its held statements
and the merges of its record in its order, and the tree of `M` is the tree that the verdict
computes. The register and the policy of `M` must be readable, as every later update reads the
register of the tip and every later move of `main` its policy: a merge that removed or broke one
of them would leave nothing that could move `main` to repair it. Nothing that the sender claims
enters it: the update names no level, and an `M` that
changes `.stemma-policy` is judged by the policy of `T`. An accepted entry names the commit of the
policy of `T` in its `policy` line, as the verdict rests on it. The first version computes all of
it under the lock; moving the tree merge and the verdict before the lock waits for the measured
holding time. A hub without a destination of statements holds none, so every move of `main` is
refused for want of a pull request. Held statements that cannot be read are
`AcceptError::Statements`, and a store that fails under the verdict `AcceptError::Verdict`. A
register that a held line or an entry of the record names and that cannot be read is
`AcceptError::Register`: the verdict would count a key as ended there and refuse, which reads as
final, though a repeat may find the store whole. All three are failures of the hub. The local
refusal for want of approvals (`GateError::NotApproved`) names each approval that does not count,
with the reason of the recount or that level 3 asks for an owner; the wire form keeps the counts.

**The refusal.** `ref heads/main gate -` stays the refusal of a move that is no merge, the
deletion of `main`. Every verdict of the gate is `ref heads/main gate <verdict> <facts>`:

```text
no-pull -                          the evidence names no opening that the hub holds
not-open -                         the pull request is closed or merged
parents -                          M is no commit with the single parent T
not-the-merge -                    the tree of M is not the clean merge
branch-moved <commit or ->         the branch is not at the head of the opening
conflict -                         the merge onto T has a conflict
policy <text>                      the policy of T cannot be used
level <text>                       the level cannot be told, or the change changes nothing
unusable <text>                    the register or the policy of M cannot be used
not-checked <level> <source>       no result of a check counts
not-approved <level> <source> <agents> <agents needed> <humans> <humans needed>
red-tip <commit>                   the first red tip
unverified <commits> <bound>       the unverified window
reserved <pull> <end>              the pull request that holds the reservation
```

`source` is `paths` or `weakened`, where the escalation for weakened tests set the level. The
refusal carries the facts that the sender acts on; the statements that do not count, each with
its reason, are not listed in it, because every reader sees them and recomputes the recount.
The example `error-refused-gate` is checked in beside the others.

### The commands of the statements as built

Built in STEMMA-312, as a proposal that the review confirms: `Repository::open_pull_for` and
`record_check_for`, `statements_of_pull`, `submit_statements` and `held_statements` in
`stemma-core`, and the commands `pr submit` and `pr show` with the options `--remote` and
`--offline` (chapter 12).

- **The repository of a statement.** A statement names the repository of a hub only if the
  command says so: `pr open --remote <name>` and `check record --remote <name>` take the ID of
  the remote. The approvals and the closing of a pull request name the repository that its
  opening names, so a pull request is for one hub or for the clone only, and never half of each.
- **At once, or later.** The command writes its entry first, so the statement counts in the clone at
  once, and then submits: `pr open` and `check record` their own statement, `pr approve` and
  `pr close` every statement of the pull request (the opening, the checks of a head of it, the
  approvals, and the closing, in the order of their entries), as `pr submit` does. A check of a
  commit that is no head of a pull request is sent by no later command. `--offline` skips the
  submission; `pr submit` makes it later. A submission is safe to repeat, because the hub answers a
  statement that it holds with its lines, so the clone keeps no record of what it sent. After the
  entry, a hub that cannot be reached or whose answer broke off is exit 3, any other failure keeps
  its code (an answer outside the protocol 1, a busy hub 6, damage 5), and a refusal of the
  statement of the command is exit 1; the entry stays. An opening that the hub answers with
  `not-open` is held for a pull request that ended there, and is no refusal for the command.
- **Refused before the entry.** A statement for a hub in an open repository (it would be
  unsigned), and without `--offline` a remote without a pinned hub key or a sign-in remote
  without a key, are refused before the entry is written. With `--offline` and without
  `--remote`, `pr approve` and `pr close` write their statement in a clone where no single
  remote has the repository of the opening.
- **The checks of an answer.** Both the submission and the reading take the list of references
  first. A held statement must be the submitted one and carry the signature of the key of the
  hub at its time: the pinned key, or a former key of the list whose range holds the number of
  its `record` line or the next one, as the hub accepted it between the two.
- **The name of a pull request for people.** A pull request is named by its number in the clone
  or by a prefix of at least 8 hexadecimal digits of the hash of its opening; `pr list` shows 12.
  Decimal digits alone are a number, so a prefix of only such digits takes `b3:`.
  The hash holds in every clone and on the hub, offline as well. The running number of the hub
  is no name: it exists only once the hub holds the opening, and two hubs count differently.
- **Not yet.** The HTTP mapping of the three requests (STEMMA-326), the statements of other
  clones in the set (STEMMA-325), and the submission of an owner override, which the hub refuses
  as a kind of its own.

### The push and the merge with a remote as built

Built in STEMMA-309: `Repository::push` and `Repository::merge_gate_remote` in `stemma-core`,
through `Transport`, with the requests `upload` and `anchor` beside the ref update. The command
surface comes with its slices.

- **The update.** Each move takes `previous` and `old` from the remote reference of its name,
  the last state of the hub that the clone saw, and the clone signs the payload with the key of
  its actor. A reference whose remote reference has the new value already is left out, and a
  push that moves nothing sends no request. A push whose entry `push` would be larger than an
  entry of the log may be (64 KiB, a few hundred references) is refused before anything is
  sent (`PushError::TooLarge`), as the clone could not record its receipt. The receipt must
  hold the update byte for byte and be signed by the pinned key of the hub
  (`RecordEntry::check_receipt`, ADR-026); otherwise nothing moves in the clone
  (`PushError::Receipt`). That the number of the receipt grows from
  receipt to receipt is not checked yet.
- **The answers.** A refusal whose reference is `incomplete` uploads once more and sends the
  same update again, once; a second one is reported. `Stale` moves nothing and is reported
  with the state of each stale reference on the hub (`PushError::Stale`); a push does not
  repeat on its own, as the actor decides what to sign after a fetch. Every other refusal is
  reported with its typed reason (`PushError::Refused`). A failure of the carrier on the ref
  update, or an answer of success whose receipt cannot be read or checked, leaves it unknown
  whether the hub accepted (`PushError::outcome_unknown`); a fetch says it.
- **After the receipt.** Under the lock the configuration of the remote is read again, one
  entry `push` names the remote references and the receipt, and they move to the commit of the
  move with the hash of the receipt as their record entry (ADR-008, ADR-011). A failure there
  carries the receipt (`PushError::Unrecorded`): the hub moved, and a fetch brings the remote
  references along. The anchor of the new tip of the log follows as a request of its own
  (ADR-026); a failure there leaves the push standing and is named in its result.
- **The merge with a remote.** `merge_gate_remote` fetches, takes `T` from the remote
  reference of `main`, judges the pull request with the verdict of the local gate
  (`judge_main`) by the policy of `T`, the namespace policy that the fetch kept for this remote
  (ADR-027), and the statements of the clone, writes `M` with the single parent `T` without a
  move, and sends the move of `main` with the hash of the opening as its evidence through the
  steps of a push. Only after the receipt does one entry `push` move
  the local `main` and the remote reference of `main` to `M`, and then `pr-merge` is written.
  `Stale` starts the loop again with a new fetch and a new signature, at most `MAX_ATTEMPTS`
  times (`RemoteMergeError::Contended`); a refusal of the hub leaves `M` unnamed and nothing to
  take back. The local `main` moves only from the value that it had when the attempt began
  (`RemoteMerge::main_followed` says whether it did), also when another worktree has it checked
  out, as in the local gate, and one with commits that the hub does not hold is refused before
  anything is sent. A change that the hub holds already, after a call that stopped after its
  receipt or lost the answer, is recorded as merged without an update, and the local `main`
  follows the hub with an entry `gate-merge`. The clone judges by its own statements, so the opening,
  the approvals and the results must be on the hub as well; binding the statements of a clone
  to the ID of the repository of a hub belongs to the commands of a pull request with a remote
  (see the open items).

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
- [x] An opening counts in an enforcing clone only with a signature that the register accepts
      for its opener: the clone asks before it places the opening, so the fold stays one
      function without a register (STEMMA-290, ADR-011). A closing is placed whatever its
      signature, because it ends a pull request; one that the register does not accept is a
      stand-in, which may only make things stricter, so the gate keeps the reservation of the
      pull request it ended (ADR-011, ADR-019). `op verify` reports it, and a hub checks every
      statement when it is submitted.
- [ ] How commands name a pull request for people (a prefix of the hash, a running number in the
      lines of the hub). It belongs to the command surface.
- [ ] A policy that names the writers of checks. Possible later without a break, because it only
      tightens.
- [ ] A list of refused attempts per namespace is not built (ADR-027).
