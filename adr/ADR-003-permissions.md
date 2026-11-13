# ADR-003: Location of Permissions

**Status:** Accepted for phase 4 (STEMMA-233). The decisions are made in STEMMA-221 to STEMMA-232
and are implemented in the slices of phase 4. What concerns the hub goes to the private project
stemma-hub (see Hand-over).

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

Local enforcement binds the processes that go through the core. A malicious process with the same
operating system rights as the actor is not stopped (STEMMA-221). Signatures do not stop cutting
the tail of the operation log either (STEMMA-224). Both go to stemma-hub.

## Open Points

- [x] Agreement with the proposal? Accepted with the changes above: the repository instead of the
      server as the place of roles and policies, local enforcement through the authorizer, and no
      signature of the policy file.
- [x] Peer-to-peer sync after the hub (phase 5), or deliberately excluded? Not part of phase 4 and
      handed over to stemma-hub. The register and the signatures reside in the repository, so it
      stays conceivable without losing enforcement.
- [x] Record of ref updates on the hub: who set which branch to which commit, and when, signed.
      Handed over to stemma-hub. Locally the operation log is the record (ADR-011); it tells who
      rewrote a branch (ADR-005).

## Hand-over to stemma-hub

Nothing in this list is built in this repository (scope decision of 2026-11-11). Each point names
where it came from.

1. Record of ref updates on the hub, signed (open point of this ADR).
2. Enforcement of protected branches on a server, in addition to the local enforcement.
3. Peer-to-peer sync, or its deliberate exclusion (open point of this ADR).
4. Malicious processes with the same operating system rights as the actor (STEMMA-221).
5. Cutting the tail of the operation log, which signatures do not stop; a hub that confirms the
   last entry does (STEMMA-224, ADR-011).
6. Visibility of an owner override per course (rule 9 of ADR-004, STEMMA-225).
7. Policies per namespace and their floor at level 3 (rules 5 and 8 of ADR-004, STEMMA-230).
8. Enforcement of the escalation when tests are weakened at the merge gate (rule 10 of ADR-004,
   STEMMA-232). The rule itself is a pure function in this repository.
9. Stricter floors that corporate customers might demand (open point of ADR-004).

## Consequences

- Local work remains possible. In an enforcing repository the core refuses an operation that the
  register and the policy do not allow, on the machine of the actor and without a server.
- A hub is no longer a prerequisite for the rules. It is needed for what the core cannot stop
  (points 4 and 5) and for what a course needs (points 6 and 7), and it is built in stemma-hub.
- Peer-to-peer sync is conceivable later without losing enforcement, because the register and the
  signatures reside in the repository.
