# ADR-026: The key of a hub and the anchors

**Status:** Accepted for phase 5. Decided by the author on 2026-11-23 and 2026-11-24 (STEMMA-274)
and carried into the ADRs by STEMMA-280. Nothing of it is implemented yet.

## Context

Signatures do not stop cutting the tail of the operation log; only an anchor kept outside the
repository finds it (`stemma op verify --anchor`, ADR-011), and so far that anchor is a copy that a
person keeps. Points 5 and 10 of the hand-over in ADR-003 ask a hub to confirm the last entry, to
order a log that someone else holds (the revocation of a key), and to settle the signature of a
policy copy on a server. ADR-011 says that a hub never sees the log, which can hold a secret until
it is purged (ADR-014).

## Decision

### A hub has one Ed25519 key per instance

The key signs the statements of the hub only: every entry of the record of ref updates (and so the
receipt), the lines that the hub adds to a statement (ADR-025), and the confirmation of an anchor.
It never speaks for an actor (ADR-024).

- The hub signs the hash of a record entry, which covers both parts and so the chain, as a new
  type of proof with a type tag of its own, an SSHSIG signature as the proofs of phase 4. The
  signature stands outside the hash. Every receipt can be checked on its own, and the time becomes
  a signed statement of the hub.
- A receipt and a confirmation are thus proofs that a clone checks without trusting TLS or a proxy
  that terminates it. Two signed entries with the same number and different hashes prove a hub
  that rewrites its record.
- The core never knows the key. The accepting operation receives a signer (bytes in, signature
  out) from the caller, calls it under the lock and writes the entry with its signature in one
  step. The same pattern holds for the anchor: an operation of the core on the bare repository
  checks the signature of the actor, writes the anchor into the destination that the caller
  passes and returns the confirmation. The in-memory `Transport` covers both with a test key.

Rejected: no key (number, time and anchor would be statements that hold only during the
connection), one key per repository (as many secrets as repositories in one process), a signature
of the tip on request only, a hub that signs after the call of the core, and a core that reads the
private key.

### A clone pins the key in the configuration of the remote

The hub names its public key in the list of references. On the first contact (`remote add`,
`clone`) the clone pins it in the configuration of the remote in `.stemma` and shows the
fingerprint; an option takes the expected fingerprint, so the first contact need not be taken on
trust. A different key later is a typed error, and nothing moves. No format in the tree changes.

A human pins a new key again, with a fingerprint from a second channel, and this writes a log
entry. The hub keeps a list of its former keys with the range of record numbers that each signed,
signed by the current key, so old entries stay checkable for a clone that pinned the current key.

Rejected: the key in a level 3 file of the tree (the tree would know its remotes, and the first
clone does not have the repository yet), no pinning, and a statement of rotation that the old key
signs and the clone follows on its own (worthless when the old key was stolen).

### The binding of a proof stays with the address of the hub

The signed challenge and the payload of a ref update name the public address of the hub as the
client calls it; the key of the hub is not added. The address is what stops a relay: a foreign hub
that poses as a mirror can name the public key of another hub as its own and pass a challenge or a
ref update on, and the other hub refuses only because the client signed the address that it called
itself. The key authenticates the statements of the hub, not the target of the proofs of the
actors. A move of the hub to another address voids prepared updates, which are signed again; the
same key shows that it is the same hub.

### An anchor names both directions

An anchor is the number and the hash of an entry of a chain, kept by someone who does not write
that chain.

- The receipt of a ref update is an anchor of the record that the clone holds and the hub cannot
  change. The clone checks on every receipt the signature of the hub and that the number grows.
  The full check (read the record from the number the clone knows, check the chain and the
  signatures, find its own receipts again) runs on a command, so `fetch` and `push` keep their
  number of requests.
- Of the operation log the hub confirms the anchor of the tip alone. The clone sends the number
  and the hash of its last entry, signed with the key of the actor as a new type of proof. The hub
  checks the signature against the register at the tip of `main`, keeps the anchor and answers
  with a confirmation that it signs with its own key and that names its time and the number of the
  last record entry.

The hub sees no entry and no secret. The statement of ADR-011 is made precise: a hub never sees an
entry of the log; it confirms the number and the hash of the tip. Only who holds the log can
check: `op verify` against the anchors that the hub holds.

Rejected: nothing of the log (the anchor would stay a copy that a person keeps), and the whole log
as a copy (the log can hold a secret, and a purge would not reach the copy).

### When an anchor is sent, and which log it names

- A push writes its log entry after the receipt and then sends the anchor of the new tip as a
  request of its own, with the same token. The anchor thus covers the `push` entry, which names
  the receipt, so the log and the record bind each other. If the anchor fails, the push has still
  succeeded and the command warns. A command of its own anchors without a push. Sending an anchor
  needs a token, that is a key in the register.
- A log belongs to one `.stemma`: one actor can have two clones with the same key, and several
  actors (agents in worktrees) write into one log. `init` and `clone` create a random ID of the
  log, a file in the layout of ADR-009; an existing repository gets it when it is first needed.
  The hub keeps anchors per repository and log ID, each signed by the actor who sends it. Who
  changes the ID begins a new log for the hub, and the old one ends visibly at its last anchor.

Rejected: an anchor on a command only, an anchor on every fetch as well (a public fetch has no
token), the hash of the first entry as the identity of a log (two clones of one actor in the same
second can have the same first entry), and an anchor per key.

### A contradicting anchor is recorded and blocks nothing

The anchors of a log are a list that is only appended to. The payload also names the anchor that
the clone last had confirmed, and the client checks its log against it before it signs, which
finds accidents locally. The hub can see a contradiction only where the same log ID comes with the
same number and another hash, or with a smaller number. It keeps a contradicting anchor as such,
with the signature of the actor as evidence, does not confirm it, and answers with a typed error
that names the anchor it holds. The anchors are readable by everyone who may read the repository.
Ref updates go on, because they count by their proofs.

Rejected: refuse and discard (the signed evidence of the rewritten log is lost), and block ref
updates from the log until an owner decides (an accident such as a restored backup would stop an
actor whose proofs are valid).

### The order across clones is the order of the hub

The register writes the bound of a retired or revoked key as a number of the local log
(`revoked 40`, checked as `number < from`), and every clone has numbers of its own, so the bound
holds only in the log that wrote it.

- A revocation acts across clones from the record entry that moves `main` on the hub to the commit
  that holds it.
- A log entry of another clone stands before the revocation only if an anchor covers it whose
  confirmation names a smaller record number; otherwise it counts as after it. The confirmation of
  an anchor is therefore made under the same lock as a ref update.
- The number in the register counts only in the log that wrote it and names the ID of that log
  from now on, a change of the register format. In every other log the bound is the `fetch` entry
  that brought the register commit.

Rejected: the record alone (log entries of other clones would have no order), and the time of the
hub (a clock would decide whether a proof counts).

### The hub holds no copy of a policy

The hub reads `.stemma-policy` from the base commit in the bare repository, through the accepting
operation, as the core does locally. No file of the hub restates the policy of a repository, so
there is nothing to sign; the protection is level 3 in the tree, the same locally and on the hub.
A cache in memory is allowed, because it can be built again from the commit. A policy per
namespace is never a loose file of the hub either (ADR-027).

Rejected: a copy that an owner signs, and a copy that the hub signs.

### A purge leaves a confirmed anchor valid

A stub keeps the hash of the entry it replaces (ADR-011), so a confirmed anchor stays checkable
after a purge. The hub holds a number and a hash only, so nothing is purged there. The purge stays
a local operation without a network; its `purge` entry is anchored with the next push or with the
command.

## Consequences

- A cut or rewritten tail of the operation log shows as a contradicting anchor that every reader
  of the repository sees. It is not prevented.
- A push costs one request and one signature more.
- The register format and the layout change (ADR-009).
- An actor without a clone has no log and so no anchor (ADR-015).

## Open items

- [ ] Log entries from before the hash chain (versions 1 and 2 of the operation log) stay
      uncovered by an anchor of a hub.
- [ ] Where a clone keeps its last confirmation. Left to the slice.
- [ ] Where the key of a hub lies, and how it is created, protected and backed up. It belongs to
      stemma-hub.
