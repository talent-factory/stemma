# ADR-024: Authentication towards a hub

**Status:** Accepted for phase 5. Decided by the author on 2026-11-23 (STEMMA-271) and carried
into the ADRs by STEMMA-280. The three messages of the sign-in, its type of proof, the check of a
signed challenge against a register, and the step of the sign-in in `Transport` are built
(STEMMA-294, see "The sign-in as built"); the hub, its nonce and its token, the accounts, and the
carriage of the token over HTTP are not.

## Context

Phase 4 proves an actor locally: the actor register in the repository binds an identity, human or
agent, to an Ed25519 key, and a proof is an SSHSIG signature over a typed payload, with no
sessions and no tokens (ADR-003, ADR-020). A hub must know who sends an HTTP request, for writing
and for reading a private repository, and it must not weaken what a verification without the hub
means.

## Decision

### The sender is a register entry and an account, bound by the key

- An account of the hub is a name with a set of Ed25519 public keys. It has no secret of its own.
- In a repository the sender is the register entry that holds the key of the request. The binding
  is the equality of the key, so there is no mapping table that only the hub holds.
- For everything that changes a repository the register alone counts. An account can grant nothing
  that the register does not grant; it is needed only above the repository: to create a
  repository, for a namespace, and to read without a register entry (ADR-027).
- The human behind an account is proven by an identity provider (OIDC; Tessera for the instance of
  Talent Factory, and the implementation is replaceable) at sign-in and when a key is bound to the
  account. The provider need not be reachable for a `fetch` or a `push`.

Rejected: the register entry alone (readers and the creation of a repository have no sender), and
an account with a credential of its own (a second secret per actor and a mapping that only the hub
knows).

### The register at the tip of `main` on the hub counts

- The register at the tip of `main` on the hub checks the sender of a `push`: not the register at
  the old value of the reference and never the one in the pushed commit. A revocation acts for all
  branches once it lies on the `main` of the hub, and a new member can push once the change of the
  register is merged there.
- The first `push` into an empty repository is made by the account that created it. The hub
  accepts it only if the pushed register holds the key of the request as an owner; after that the
  rule above applies.
- The hub accepts only enforcing repositories. Open mode stays a local state:
  `enforce --owner-key` comes before the first `push`, and a `push` that removes the register of
  `main` is refused, so the step has no way back on the hub either.
- A revocation has places that are independent of one another. A revocation in the register
  counts for the repository; the removal of a key from an account counts above the repository; a
  block of a key on the whole hub is an emergency measure of the operator that refuses requests,
  changes no register and voids no earlier signature.

### A signed challenge yields a short-lived token

1. The hub issues a nonce.
2. The key signs it once per command through `ssh-keygen -Y sign`, as locally. The signature is a
   new type of proof with a type tag of its own that binds the address of the hub as the client
   calls it, the nonce and the key, so it never counts as a log entry or an approval.
3. The hub answers with a token for the requests that follow.

The token is not a proof in the sense of the glossary. It never enters a repository, proves
nothing afterwards, lives only in the process of the command and is not written to disk. It says
only which key speaks. What the key may do, the hub checks on every request against the register
of `main`, so a revocation acts at once.

- TLS is mandatory for every address that crosses a network (ADR-028).
- The hub derives the nonce from a secret of its own and keeps no session state. The lifetime of
  the token is short (minutes) and a setting of the hub. The clock of the hub occurs in no
  repository.
- A replayed challenge is refused without state, within a time window of seconds. The signature
  binds the address of the hub, so another hub cannot pass a challenge on. The binding stays with
  the address also now that the hub has a key (ADR-026).
- Reading and uploading objects can be repeated without harm, because objects are addressed by
  their hash. A ref update counts once by its own construction (ADR-025).

Rejected: a signature per request (its cost grows with the number of requests per command, and a
key with a passphrase or on a hardware token asks every time), and a token of the identity
provider for every request (a second credential per agent, a mapping from subject to account, a
stored token, and a hub that depends on the provider for every request).

### The sign-in as built

Built in STEMMA-294, in `stemma_core::protocol::sign_in`. The three messages are line-oriented
text by the rules of ADR-023:

```text
stemma-protocol 1                                   the challenge
hub <address of the hub>
nonce <32 to 128 lower-case hexadecimal digits>

stemma-protocol 1                                   the signed challenge
hub <address of the hub>
nonce <the nonce of the challenge>
key ssh-ed25519 <base64>
signature <SSHSIG signature on one line>

stemma-protocol 1                                   the token answer
token <token>
lifetime <seconds>
```

- **Challenge.** The hub names the address under which the client calls it, in the form of the
  ref update (ADR-025): the part of the address of a remote before the namespace, kept as it is
  and compared byte for byte. The nonce is 16 to 64 bytes of the hub. A client reads nothing from
  it.
- **Signed challenge.** The answer of the client repeats the address and the nonce, because the
  hub keeps no state between the two requests, and adds the public key and the signature. The
  lines from `hub` to `key` are the payload. The signature is a proof of the type `challenge`:
  the signed bytes begin with the header `stemma-proof`, `challenge`, `1` (ADR-020), so the
  signature verifies as no log entry, approval, sponsorship, ref update or record entry, and none
  of those verifies as a sign-in. The reader checks the signature for the key of the message, as
  the reader of a ref update does.
- **Token answer.** The token is what an `Authorization: Bearer` header carries (ADR-023): at
  least one letter, digit, `-`, `.`, `_`, `~`, `+` or `/`, then any number of `=`. Its content is
  the hub's. The lifetime is in seconds, from 1, and counts from the answer.

The rules that the core holds:

- **The client signs only for the address that it calls.** A client signs through
  `Challenge::answer`. It takes the address that the client calls and refuses a challenge that
  names another one before the signer is asked, so nothing is signed for it and a hardware token
  is not touched. The procedure of the sign-in (`stemma_core::transport::sign_in`) signs in no
  other way.
- **The hub checks the address, the nonce and the register.** `SignedChallenge::check` refuses a
  challenge that was signed for another address than that of the hub, and one whose nonce the
  hub does not accept. The core cannot judge a nonce, because the hub alone knows its secret and
  its clock; the check therefore takes the judgement of the hub as an argument, so that it
  cannot be left out. `SignedChallenge::actor` also asks a register and returns the identity
  whose entry holds the key as an active key; an unknown, a retired and a revoked key are
  refused alike, because none of them signs anything new. The three are judged in this order.
- **The token shows in no output.** The type of the token has no `Display` form, its `Debug`
  form does not show it, and no error of a reader quotes it. The core writes it to no file and to
  no log. The token answer is the one message that holds a secret; it is read by the transport,
  which keeps the token in memory and hands the caller the lifetime alone (ADR-022).

The address in the challenge is redundant to the address that the client calls. It is there so
that a client that reaches a hub under a name that the hub does not know as its own learns that
from a typed error before it signs, and not from a refusal without a reason.

### An agent has no account

An agent is a register entry only. It acts in every repository whose register names it and
nowhere else; it creates no repository and manages no namespace. The sponsor stays bound in one
place, the co-signature, and the identity provider holds no technical users for agents.

An agent in CI uses the key of its register entry, loaded into an `ssh-agent` as a secret of the
CI. The protection is the revocation in the register and the role of the agent, which is never
owner. A time-limited delegation by the sponsor for a key that the run creates is deliberately not
built: it needs a clock in a proof and a chain behind the sponsor. The hub issues no keys.

### Reading is per repository and private by default

- A public repository is read without a challenge, a private one only with a token. Writing is
  always authenticated.
- A private repository is read by whoever stands in the register of `main` with an active key,
  human or agent, and by the accounts that the namespace grants reading (ADR-027). An agent has no
  account, so it reads only where the register names it.
- A private repository answers a sender without the right to read like a missing one (ADR-023).
- The visibility is a setting of the repository on the hub, changed only by an owner according to
  the register of `main`. It does not stand in `.stemma-policy`, because the core cannot enforce
  reading locally.

Rejected: reading that is always authenticated (no public project or course material on the hub),
and everything readable anonymously.

### An authentication at the hub never replaces a proof

Token and signed challenge are transport and leave no trace in a repository. `op verify` comes to
the same result with and without the hub, and a hub that is taken over can forge no approval and
no log entry. The hub checks the same proofs again with the operation of the core (ADR-025) and
accepts nothing only because the sender is authenticated.

The hub adds:

- the evidence of which key sends a request;
- accounts above the repository, a name and keys, whose humans the identity provider proves;
- the control of reading: visibility and the readers of a private repository;
- refusal in addition to the local enforcement. It may refuse more than the register and never
  allow more.

In the repository stay:

- the register as the only trust anchor for who may change what;
- every proof (log entry, approval, co-signature), made with the key of the actor;
- the revocation of a key.

The hub never writes an identity into a register, never assigns a key to an actor that the
register does not name, never signs on behalf of an actor, and never accepts a proof that
`op verify` would refuse without the hub. A web interface of the hub can show but not approve,
unless the signature is made on the machine of the actor; phase 5 builds no such interface. The
key of the hub (ADR-026) signs the statements of the hub only and never speaks for an actor.

Rejected: a hub that vouches for a signed-in account with a key of its own, for all levels or for
the low ones only.

## Consequences

- An actor has one secret, its key, for the repository and for the hub.
- A hub that is taken over can forge no approval and no log entry, and it cannot make a change
  count that the register does not cover.
- A key in a file or in an `ssh-agent` is the actor for every process of the operating system
  user; the token adds nothing to that and takes nothing from it. The limits are named in ADR-003.
- A human with a hardware token key touches it once per signature, and so once per sign-in
  (ADR-020).

## Open items

- [ ] The kind of error message that answers a refused sign-in: a signed challenge for another
      address, a nonce that the hub does not accept, or a key that the register and the accounts
      do not know. The core refuses with a typed error (`Refused`); the message on the wire
      comes with the slice that builds the answering side of a hub, and it decides whether the
      three cases are told apart to the sender.
- [ ] The bound of a client for the answers of a sign-in is 64 KiB for the challenge and for an
      error (`MAX_SIGN_IN_MESSAGE`). The token answer is read by the transport, and the slice
      of the HTTP transport sets its bound.
- [ ] The token is not wiped from memory when the transport is dropped. It lives for minutes,
      and a process that reads the memory of the command reads the key file as well (ADR-003).
- [ ] The steps by which an account is created and a key is bound through the identity provider.
      It belongs to stemma-hub.
