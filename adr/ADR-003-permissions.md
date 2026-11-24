# ADR-003: Location of Permissions

**Status:** Accepted for phase 4 (STEMMA-233). The decisions are made in STEMMA-221 to STEMMA-232
and are implemented in the slices of phase 4. What concerns the hub goes to the private project
stemma-hub (see Hand-over); the decisions on it were made for phase 5 on the map STEMMA-266 and
carried into the ADRs by STEMMA-280.

## Decision

The proposal placed roles and policies on the server. Phase 4 builds no server, so the decision
places them in the repository and lets the core enforce them locally:

- Roles reside in the repository, in the actor register (STEMMA-221, STEMMA-222). It binds each
  actor identity, human or agent, to a key. The register files are always level 3 (ADR-004).
- Policies reside in the repository, in the file `.stemma-policy` in the root (STEMMA-230). It is
  read in the version of the base commit. The policy file is level 3 (ADR-004).
- The core enforces both through the authorizer (ADR-002) before every operation that changes a
  repository. A server is not needed for the rules themselves.
- Signatures of approvals and of the log entries whose rule depends on who acts reside in the
  repository and can be verified independently of any server. The scheme is SSHSIG with Ed25519
  (STEMMA-224). Commits are not signed; a proof signs the log entry or the approval. A signature
  counts only against the register of the base commit.
- The policy file has no signature of its own: level 3 protects it, because only a human owner
  may change it.
- There is one policy per repository. Policies per namespace are not built locally (STEMMA-230).
- A repository without an actor register is in open mode: the actor is a claim that the operation
  log records and nobody checks. The step to an enforcing repository has no way back.

## Limit

Local enforcement binds the processes that go through the core. What follows about a hub is
decided for phase 5 and not implemented yet. A hub adds a second place that
checks every proof again (ADR-024, ADR-025, ADR-026). What is left is the process that uses the key
of the actor (STEMMA-275).

### Threat model

The model is everything that the operating system user reaches.

- Within reach of a malicious process: all files of the repository, including `.stemma` with the
  configuration of the remotes and the pinned key of the hub; every key file that the user can
  read and every `ssh-agent` socket of the user, and so every actor whose key lies there (a human
  and their agents); running agents and their inputs; the `stemma` binary and what it displays;
  the token in memory.
- Out of reach: the hub and its key, keys on other machines or under other operating system
  users, and a signature that needs a touch of a hardware token.

Root and the kernel are outside the model: no local measure would count any more.

### What a hub stops, what it makes visible, and what stays

- Stopped: a reference, an object or a log entry that is written past the core without a key
  never reaches the hub, because every ref update and every statement needs a signature against
  the register at the tip of `main`, and `main` moves from `T` to `M` only with the gate
  recomputed (ADR-025). A
  forged approval or check result without a key is no proof, and the hub never signs for an actor
  (ADR-024). The record and its order cannot be forged, because the process does not reach the key
  of the hub and the receipts lie in the clones (ADR-026).
- Made visible: a cut or rewritten tail of the operation log, as a contradicting anchor that is
  recorded and blocks nothing (ADR-026).
- No gain for the process: the token, which says only which key speaks (ADR-024).
- Left open: the process uses the key of the actor, and then it is the actor for the hub.

### What phase 5 builds against a used key

Both are decided and not implemented yet. Not verified: that `ssh-keygen -Y sign` signs with a
hardware token in the way the `Signer` calls it (ADR-020).

- The register accepts the key type `sk-ssh-ed25519@openssh.com`, a key on a hardware token, and
  the verifier of the core demands the flag "user present" on every signature of such a key
  (ADR-020). The key type is voluntary and visible in the register: no rule and no policy line
  asks for it, and the register shows every reader which owner is protected how. An entry that
  holds token keys only is protected against the process; an entry with a file key and a token key
  is not.
- A command of the client reports from the record and the statements what a key did in a range of
  record numbers: the references it moved, its statements, and the merges to `main` that rested on
  them. Every reader can recompute it, because every entry carries the key. Cleaning up stays with
  the humans (a revert, a purge).

A revocation acts from the record entry that moves `main` to it (ADR-026), and the approvals of
the key become void. The block of a key by the operator of a hub bridges the time until then
(ADR-024). An owner of a namespace may freeze a repository and never sets one back (ADR-027).

Rejected: a confirmation through a second channel at the hub (the hub would vouch for an account,
and `op verify` without the hub would come to another result), keys of agents that the hub issues
for a short time (ADR-024), a policy line that demands a token key from a level upward now (it
only tightens and can come later; its trigger is a repository with several owners in which the
weakest key decides), a revocation that names "compromised since record number N", a waiting
period for a change of the owners, and a second owner for a change of the owners.

### Limits that stay

- A key as a file or in an `ssh-agent` is the actor for every process of the user. A passphrase
  protects the file at rest only and is invisible to every verifier; a passphrase and `ssh-add -c`
  are hygiene and no proof.
- The keys of several actors under one operating system user fall together. A process that holds
  the keys of two agents opens with one and approves with the other, so it can bring a change of
  level 0 or 1 to `main` on its own (ADR-004). It stays visible in the record and the statements.
- A running agent, the binary, the display and the configuration of a clone are within reach of
  the process. A human can be shown one thing and sign another, and a clone cannot protect its own
  configuration from its user: after a compromise the way back is a fresh clone that is compared
  with the record.
- A check result is the statement of its actor; nobody proves the run (ADR-025).
- A stolen key of a human owner without a token takes over the rules of the repository on the
  hub: the process changes the register itself, rewinds `main` with a purge and overrides leases,
  and the hub checks valid signatures and accepts. The takeover stays visible, because the record
  and the receipts in the clones prove it and no clone loses its objects. The way back lies above
  the repository: the block of the operator, and the freeze by an owner of the namespace. Where
  the namespace demands its owners in the register, the thief cannot remove them, and an owner of
  the namespace revokes the key and cleans up with ordinary proofs; otherwise the repository stays
  frozen as evidence, and a new one with a new ID is made from a clean clone (ADR-027).
  Prevention is a token key and two owner keys in separate places.

## Open Points

- [x] Agreement with the proposal? Accepted with the changes above: the repository instead of the
      server as the place of roles and policies, local enforcement through the authorizer, and no
      signature of the policy file.
- [x] Peer-to-peer sync after the hub (phase 5), or deliberately excluded? Decided in STEMMA-277:
      a clone is excluded on purpose as the authority over a shared protected branch and never
      accepts a push; the transport of objects and statements between clones stays possible and is
      not built (see Consequences, ADR-022).
- [x] Record of ref updates on the hub: who set which branch to which commit, and when, signed.
      Decided in STEMMA-56 (ADR-025). Locally the operation log is the record (ADR-011); it tells
      who rewrote a branch (ADR-005).

## Hand-over to stemma-hub

Nothing in this list is built in this repository as a server (scope decision of 2026-11-11). The
formats, the protocol types and the operations of the core that a hub calls are built here
(ADR-022). Each point names where it came from and the decision of phase 5.

1. Record of ref updates on the hub, signed (open point of this ADR). Decided: a ref update is a
   signed proof of the actor, and the hub writes a hash chain of record entries that it signs
   (ADR-025, ADR-026).
2. Enforcement of protected branches on a server, in addition to the local enforcement. Decided:
   the clone builds the merge commit and the hub recomputes the gate with the accepting operation
   of the core (ADR-025).
3. Peer-to-peer sync, or its deliberate exclusion (open point of this ADR). Decided: a clone is
   never the authority over a shared branch (see Consequences).
4. Malicious processes with the same operating system rights as the actor (STEMMA-221). Decided:
   the threat model and the limits above.
5. Cutting the tail of the operation log, which signatures do not stop; a hub that confirms the
   last entry does (STEMMA-224, ADR-011). Decided: the hub confirms the anchor of the tip alone
   and never sees an entry (ADR-026).
6. Visibility of an owner override per course (rule 9 of ADR-004, STEMMA-225). Decided: the
   readers of the repository see it, with a derived list per namespace (ADR-027).
7. Policies per namespace and their floor at level 3 (rules 5 and 8 of ADR-004, STEMMA-230).
   Decided: a namespace policy in a namespace repository, combined by the maximum (ADR-027).
8. Enforcement of the escalation when tests are weakened at the merge gate (rule 10 of ADR-004,
   STEMMA-232). The rule itself is a pure function in this repository. Decided: the hub recomputes
   the level from the policy of the base commit, the escalation included (ADR-025).
9. Stricter floors that corporate customers might demand (open point of ADR-004). Deferred: out
   of scope until there are concrete customers. ADR-027 records what the model keeps open.
10. Ordering of a log that someone else holds (revocation of a key, STEMMA-224), and the signature
    of the policy copy that a server holds (STEMMA-230). Decided: the order across clones is the
    order of the hub, by the record and the anchors, and the hub holds no copy of a policy
    (ADR-026).

## Consequences

- Local work remains possible. In an enforcing repository the core refuses an operation that the
  register and the policy do not allow, on the machine of the actor and without a server.
- A hub is no longer a prerequisite for the rules. It is needed for what the core cannot stop
  (points 4 and 5) and for what a course needs (points 6 and 7), and it is built in stemma-hub.
- The register and the signatures reside in the repository, so every clone checks a single proof
  without a hub. The enforcement on a shared branch needs in addition an order written by someone
  who is not the actor: for the retirement of a key, the bounds of the gate, write ownership and
  the namespace policy. A hub writes it. A clone is therefore never the authority over a shared
  branch and never accepts a push: a sync between two clones is always a fetch in each direction.
  The transport of objects and statements between clones stays possible and is not built; its
  trigger is an exchange of work without a reachable hub (ADR-022). The answer to working without
  the central hub is a hub of one's own, which a clone knows by its address and its key.
