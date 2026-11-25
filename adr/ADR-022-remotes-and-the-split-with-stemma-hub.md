# ADR-022: Remotes and the split between stemma and stemma-hub

**Status:** Accepted for phase 5. Decided by the author on 2026-11-23 (STEMMA-269) and, for the
properties that must stay, on 2026-11-24 (STEMMA-277), and carried into the ADRs by STEMMA-280.
Nothing of it is implemented yet; the slices are children of STEMMA-15.

## Context

The scope decision of 2026-11-11 (ADR-003) puts the hub into the private project stemma-hub, but
it did not draw the line. Phase 5 needs to know what is built in this repository, where the types
live that both sides must agree on, who owns the version of the protocol, how stemma-hub depends
on `stemma-core`, and how a public client and a private hub are tested together. ADR-015 demands
that the hub runs the same operations of the core as the CLI and that no second implementation
exists.

## Decision

### The cut on the client side runs along I/O

- State and logic live in `stemma-core`: the configuration of remotes in `.stemma/`, remote
  references (ADR-008), the computation of the missing objects, the verification of received
  objects, the log entries and the authorizer calls of `fetch` and `push`, and a trait
  `Transport`. The operations stay methods of `Repository` (ADR-009, ADR-015).
- A new crate `stemma-remote` in this repository holds only the HTTP client that implements
  `Transport`; the CLI wires the two. The core gets no network dependency, and the choice of the
  HTTP crate concerns `stemma-remote` alone (ADR-028).
- `Transport` is pure I/O and makes no assumption about HTTP (see "Properties that must stay").

Rejected: the HTTP client in the core, and a crate that also holds state and logic (`RefStore` is
sealed).

### The protocol types live in the core, with a version of their own

- The types that both sides agree on live in a module `stemma_core::protocol`, with one encoder
  and one parser that client and hub share. They get no crate of their own and live neither in
  `stemma-remote` nor in stemma-hub.
- The version is a number of its own, `stemma-protocol <n>`, a constant in the core. It is
  independent of the crate version and of `stemma-repository` (ADR-009).
- This repository owns the number: from the first release that carries the protocol on, a change
  of a message needs an ADR and a new number (ADR-023), as ADR-009 demands for the formats, and
  the hub never defines protocol. A hub may serve several versions, a client
  speaks one; a hub that does not know the version refuses and names the versions it speaks.
- The protocol is public because the code is. What it encodes is decided in ADR-023.

### stemma-hub depends on `stemma-core` through a git dependency

stemma-hub uses a git dependency on the public repository: no path dependency and no published
crate (`publish = false` stays; publishing remains possible later). Four rules hold:

1. The `main` of the hub pins a commit (`rev`) that lies on the `main` of stemma, never a commit
   of a feature branch, which a squash merge removes.
2. A release of the hub pins a tag `v<version>` of stemma (a pre-release tag counts) and names it
   in its release notes.
3. A change that touches both repositories goes first here, then in the hub with a raised pin. A
   local `[patch]` on the neighbouring checkout serves work on both and is never committed.
4. The hub has a version of its own, not coupled to the version of stemma. Compatibility of
   client and hub follows the protocol version.

The release rule of this repository does not change, and the CI of the hub needs no token for the
dependency.

### The hub opens a bare repository and composes no checks

- The hub opens a bare `Repository` (ADR-009) and calls its methods. An operation that needs a
  working copy refuses with a typed error.
- One operation of the core accepts a ref update and fixes the order of the checks (authorizer,
  policy, escalation, gate rule), the same locally and on the hub. The hub supplies the evidence
  and applies the result; it composes no checks itself. The operation is described in ADR-025.
- Callable without a working copy, `HEAD` and index: the authorizer against the register of a
  commit (on a hub the register at the tip of `main`, ADR-024 and ADR-025); the policy of the base
  commit and the level of a change (`policy_at` becomes reachable); the escalation when tests are
  weakened; write ownership and lease; the rule table and the bounds of the gate with the count of
  approvals; the verification of received objects, references and signatures.
- Not needed on the hub: `snapshot`, `track`, `status`, `checkout`, `switch`, `restore`,
  `worktree`, `rebase`, `follow_main`.
- `ObjectStore` and `RefStore` stay sealed. The store of the hub is therefore a Stemma layout per
  repository, written and read through `stemma-core`, and no second implementation of a store
  exists (ADR-015). The seal can be opened later with an ADR without a break.

### Testing the client and the hub together

- The public CI never names the hub and needs no secret. It tests the operations through an
  in-memory `Transport` that joins two repositories through the operations of the core (no second
  server: the other side is the accepting operation), and the protocol types against checked-in
  example messages, which the hub reads as conformance tests through its git dependency.
- The end-to-end tests with real HTTP live in stemma-hub and use `stemma-remote` and the CLI of
  the pinned commit.
- A scheduled job of the hub builds and tests against the tip of stemma `main` instead of the
  pin; it reports and blocks nothing.
- A slice that changes the protocol names its counterpart in the hub and counts as done only when
  the hub is green with the raised pin.
- How the HTTP client of `stemma-remote` is tested here without a server is left to its slice.

### Properties that must stay

A clone is never the authority over a shared branch and never accepts a push (ADR-003). Phase 5
builds no transport between clones and reserves nothing in a format for it, but four properties
are recorded so that a later fetch from a clone stays possible (STEMMA-277):

1. The trait `Transport` is pure I/O without an assumption about HTTP.
2. The answering side of `refs` and `fetch` is an operation of the core that runs on every
   repository, not on a bare repository in the hub only.
3. Signed statements (ADR-025) are valid without a hub.
4. Object IDs never depend on the way of transport.

Such a fetch would write remote references of that clone only and move no local branch and no
`main`; statements from a peer would count as not yet submitted and get their order with the
lines of the hub. The peer is a courier, and nothing from it counts before the hub has it.

A later fetch from a clone costs a new protocol number and a new version of the layout, with
decisions on a remote reference without a record entry and on statements without the lines of the
hub. Its trigger is an exchange of work without a reachable hub, for example in a school network
without internet. No phase schedules it.

## Consequences

- The core stays free of network code and of an async runtime, and every rule of a `fetch` or a
  `push` is testable without a socket.
- A change of the protocol is a change of this repository first. The hub follows with a raised
  pin.
- The hub cannot loosen a check, because it calls one operation and has no parts to rearrange.
- The ADRs of the hub itself (its HTTP server, its data tree, its operation) live in stemma-hub.
  They are written when that repository exists (STEMMA-279).

## Open items

- [ ] The design of the bare repository: what it contains exactly (no `HEAD`, no index, no
      operation log by ADR-011) belongs to the slice that builds it (ADR-009).
- [ ] How much of the hub is passed on to third parties. The answer "a hub of one's own" to
      working without the central hub (ADR-003) depends on it; it is not decided.
