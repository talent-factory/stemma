# ADR-023: The transfer protocol

**Status:** Accepted for phase 5. Decided by the author on 2026-11-23 and 2026-11-24 (STEMMA-270
for the format, STEMMA-272 for the mapping to HTTP, with the addenda of STEMMA-56, STEMMA-273,
STEMMA-274 and STEMMA-276) and carried into the ADRs by STEMMA-280. Nothing of it is implemented
yet. The numbers under "Basis" give a direction, not a threshold.

## Context

Chapter 8 of the book named "single objects over HTTP, addressed by their hash" as the scope and
left the format open; ADR-007 leaves pack files and compression open. A research note
(`docs/research/stemma-267-object-transfer.md` on the branch
`feat/stemma-267-research-object-transfer`) found two families of protocols: either the server
computes the difference from the commit graph and sends one stream, or the server only answers
lookups by ID and the client finds the difference. In every protocol it read, reference updates
are compare-and-swap, objects arrive before the reference moves, and the identity of the actor is
not part of the protocol.

ADR-015 fixes the premises: every clone holds the full object store, and a history has up to a few
gigabytes. ADR-022 fixes where the types live (`stemma_core::protocol`) and who owns the number.

### Basis

No `.stemma` was at hand, so the Git history of this repository (`main`) served as an
approximation: 330 commits, 4483 objects (2041 trees, 2112 blobs), 22 objects for one new commit
and 113 for ten, object size median 1.1 KB and maximum 371 KB. All objects as one stream:

| Form | Size |
|---|---|
| Raw | 103.3 MB |
| gzip per object | 26.2 MB |
| gzip over the stream | 25.1 MB |
| zstd level 3 | 13.7 MB |
| zstd level 19 | 3.5 MB |
| zstd level 3 with a window of 128 MB (0.1 s of CPU time) | 2.4 MB |
| Git pack with deltas | 7.4 MB |

It is one history, with Git trees instead of Stemma trees, and a window that covers the whole
history.

## Decision

### The unit of a request is a batch of whole objects

One request carries a stream of objects, each in the encoding of ADR-005 and ADR-007
(`<kind> <length>`, a NUL byte, the body), without deltas. The receiver stores them through the
existing write path, so no second storage format arises. The object stays the unit that is
addressed by its hash, but it is not the unit of a request.

Rejected: one object per request (about 4500 requests for a clone of this repository, 10^5 to
10^6 at the target scale, with clients that speak HTTP/1.1 only), and a pack with deltas (a delta
format, the choice of bases and their resolution; possible later, because a delta changes no
object ID).

### A fetch is the list of references and one round with `want` and `have`

1. The client reads the references in one request: the whole list, without a prefix filter.
2. It sends the commits it wants and the commits it has (the tips of its branches and of its
   remote references) in one request. The hub ignores a `have` it does not know, walks the graph
   and answers with the stream of all objects that are reachable from the `want` commits and not
   from the known `have` commits.

There is no second round and there are no acknowledgements. A clone is the same request without
`have`. This rests on ADR-015: who has a commit has everything below it. The hub may send too
much, never too little; a duplicate is harmless.

A `want` is bound to a reference: the hub serves only what is reachable from a reference of a
repository that the requester may read (ADR-024). Reading is per repository, all references alike
(ADR-027). No endpoint serves an object by its ID alone, in a public repository either: it could
not check cheaply that the object is reachable from a reference, and it would serve the unnamed
objects of a refused push to anyone who knows the ID.

Rejected: a walk by the client (as many rounds as the graph is deep), and a negotiation in
several rounds (it can be added as a new protocol number if measurements show that one round
regularly sends too much).

### A push is an idempotent upload, then a small ref update

1. The client computes the difference against its remote references and sends without asking:
   everything reachable from the new tip and not from its remote references, the same computation
   as for a fetch with the roles exchanged. The hub hashes the objects, stores them unnamed and
   confirms once they are durable. The request is idempotent; the client may split a large push
   at commit boundaries into several requests and continues after an interruption at the first
   piece that was not confirmed.
2. A small ref update follows (ADR-025). Only here the hub checks connectivity, calls the
   accepting operation and performs the compare-and-swap.

A stale remote reference shows as `Stale` in the compare-and-swap (ADR-008), and the client
fetches first. The client sends too much when the hub has objects by a way the client does not
know; that costs bytes and nothing else. A refused push leaves unnamed objects on the hub, which
are never served. Fetch and push mirror each other (the same stream, order, check and way of
resumption), and the core needs no quarantine area.

Rejected: a question to the hub before sending (it can be added as an optional request later), a
negotiation of common commits, one request with objects and ref update (a quarantine area in the
sealed store, and a first push of a large history as one request that must pass whole), and an
upload session with state on the hub.

### The whole stream is compressed, as a property of the transport

The HTTP body carries a content encoding that is negotiated, with zstd (ADR-029), and every side
must accept an uncompressed body. The store stays as ADR-007 defines it. The messages of
`stemma_core::protocol` know no compression, and the in-memory `Transport` needs none. The long
window is the reason why the format can do without deltas. The level and the size of the window
are left to the slice, which measures them on a Stemma store; the receiver may name an upper
bound for the window.

Rejected: no compression, compression per object inside the protocol, and a compressed store
whose bytes are sent as they are.

### The receiver checks hash, strict reading and full connectivity

Both sides check. The receiver repeats the walk of the sender: from the new tips down to commits
that are reachable from its own references, comparing the trees with those of the parents. Every
object it meets must be present, of the right kind and readable by the strict readers of ADR-005
(trees, `commit`, `commit2`). The walk covers objects that were in the store before as well,
because such an object may be left from an interrupted transfer. Only then does the client move
its remote references, and only then does the hub call the accepting operation.

- The frame of an object carries no ID. The receiver computes it, so a damaged object is another
  object and the walk reports the expected one as missing.
- An object that the walk does not reach stays in the store unnamed and is no error (ADR-007,
  ADR-011).
- The receiver sets upper bounds for the size of an object and of a request and refuses with a
  typed error. The values come from the slice.
- The connectivity check does not presuppose that every object comes from the sender: a commit
  whose trees lie in the store already is accepted (ADR-015).

The check concerns objects and their connectivity only. The evidence for permission and gate is
the subject of ADR-025.

Rejected: the hash alone, and a full check on the hub only (ADR-015 local first; ADR-003: a
verification without the hub keeps its meaning).

### The order of the stream is part of the format, so a transfer resumes

Ancestors come first, and for each commit its new blobs and trees come before the commit itself.
Every beginning of the stream that ends with a commit is therefore a complete state.

- The client checks connectivity commit by commit and records in `.stemma` which commits arrived
  completely: a journal of a running fetch, as `purge-pending` is for a purge, and a new file of
  the layout (ADR-009).
- After an interruption it sends the same request with these commits as further `have` lines. The
  resumption is an ordinary fetch, the protocol gets no new element, and the hub keeps no state.
- The remote references move at the end only.
- The resumption has priority over an order that helps compression. The order inside one commit
  is left to the sender, so a response need not be reproducible byte by byte.

Known limit: a single commit that is larger than a connection carries cannot be resumed.

Rejected: discard and start again, keep without resumption, and a response cut into pieces of a
bounded size (thousands of requests in a row for a large clone).

### One number covers every message

`stemma-protocol <n>` is the first line of every message itself, not an HTTP header, so it belongs
to the types of `stemma_core::protocol` and is covered by the in-memory `Transport` and the
checked-in example messages.

- Every small message is line-oriented text as `format`, `HEAD` and the entries of the log are:
  the first line, then lines `<keyword> <value>` in a fixed order, read by a reader that accepts
  only what the writer writes. The object stream stays binary and begins with the same first line.
- There are no capability lists and no optional fields. Every extension is a new number.
- Not covered, because they belong to the transport: TLS, the carriage of the token, the content
  encoding, and URL paths and status codes.
- The number is independent of `stemma-repository`, which does not travel, and of the object
  kinds. An older client that meets a kind it does not know fails in the connectivity check with a
  typed error and moves no reference; it learns this on receipt, not on the first request. An ADR
  that introduces an object kind says whether it raises the protocol number. The hash algorithm
  needs no field (ADR-001).

Rejected: a new object kind always raises the number, and a negotiation of capabilities.

### The messages

| Message | Content |
|---|---|
| List of references | The repository ID, the public key of the hub with the list of its former keys (ADR-026), and per reference its commit and its record entry (ADR-025) |
| Fetch request | `want` and `have` lines |
| Object stream | The first line, then the framed objects in the order above |
| Ref update and its answer | The signed payload of ADR-025; the answer is the record entry as a receipt |
| Challenge, answer, token | The three messages of the sign-in (ADR-024) |
| Statement and its answer | A signed statement; the answer is the lines of the hub (ADR-025) |
| Record and statements, read | A range by numbers (ADR-025) |
| Anchor and its confirmation | ADR-026 |
| Statements of a namespace | ADR-027 |
| Error | The first line, then the kind of the error with its facts |

The token travels in the `Authorization` header of a request and in no message, so fetch, upload
and ref update look the same for a public and a private repository, and no stored or logged
message holds a secret. The trait `Transport` gets a step for the sign-in.

### Mapping to HTTP

The address of a remote is the address of the repository, `<hub>/<namespace>/<repository>`
(ADR-027), and every endpoint of a repository lies below it.

| Request | Meaning |
|---|---|
| `POST <repo>/challenge` | Empty body; answers the challenge |
| `POST <repo>/token` | The answer of the client; answers the token and its lifetime |
| `GET <repo>/refs` | The list of references |
| `POST <repo>/fetch` | `want` and `have`; answers the object stream |
| `POST <repo>/objects` | The object stream; answers 204 once the objects are durable |
| `POST <repo>/refs` | The ref update; answers the receipt |
| `<repo>/record` | Reading the record of ref updates, a range by numbers |
| `<repo>/statements` | Submitting a statement, and reading statements, a range by numbers |
| `POST <repo>/anchors` | Sending an anchor; a further request reads the anchors of a log |

Above the repository lie the requests of a namespace (ADR-027): reserving a namespace, creating a
repository, submitting and reading the statements of a namespace, the list of the owner overrides
of a namespace, and the repositories of a namespace that the requester may read. The methods and
the exact paths of the reading requests and of the requests above the repository belong to the
slices that build them; the names of the endpoints are reserved (ADR-027).

Headers:

- The token travels as `Authorization: Bearer <token>`.
- The client sends `Accept-Encoding: zstd`, and the hub answers with `Content-Encoding: zstd` or
  uncompressed. The hub names `Accept-Encoding: zstd` in its responses, and the client compresses
  an upload only after it has seen that.
- The client follows no redirect and reports a 3xx as an error with the new address, so a token
  never goes to another host.
- Every response carries `Cache-Control: no-store`, also the list of references, which a proxy
  could otherwise store by its own heuristics and deliver stale.

Status codes are the coarse class only:

| Status | Meaning |
|---|---|
| 200, 204 | Success |
| 400 | A message that cannot be read, or an unknown protocol version (the answer names the versions the hub speaks) |
| 401 with `WWW-Authenticate: Bearer` | A token that is missing, expired or invalid where one is needed |
| 403 | A sender that is known and sees the repository, and an action that is refused (register, policy, write ownership, gate), or a count of a namespace exceeded |
| 404 | A repository that is missing or that the sender with a token may not read |
| 409 | A ref update that is `Stale` |
| 413 | An object, a request or a quota exceeded |
| 415 | An unknown content encoding |
| 422 | A ref update whose objects are missing or not connected; the client uploads once more |
| 429 with `Retry-After` | The limit on requests without a valid token |
| 503 with `Retry-After` | The hub is busy; the client retries a bounded number of times |

Three rules:

1. The sign-in lies below the path of the repository, so the client knows one address. The hub
   answers `challenge` and `token` for every path alike, without looking whether the repository
   exists.
2. Without a token the hub answers 401 for everything that is not public, a private and a missing
   repository alike. With a token both a missing repository and a missing right to read are 404.
   Neither answer reveals that a repository exists. The client remembers per remote that a sign-in
   is needed, which it learns once through a 401; this is kept in the configuration of the remote.
3. The typed error stands in the body. The client decides by the error message, and a response
   without a readable message, such as the page of a proxy, is a transport error with its status.
   The type of a refusal is the same as the local one (ADR-025).

The client ends with exit code 1 on a 403 (nothing was written) and on a 409, with a hint to
fetch; a 503 is exit code 6.

Rejected: a sign-in below a path of the hub (the client would have to find the root of the hub,
which is ambiguous behind a proxy with a path prefix), 404 without a token (the client would not
learn that a sign-in could help), and the kind of an error from the status alone (it has no
counterpart in the in-memory `Transport`).

### Requests per command

The sign-in costs two requests, once per command. A token must be valid when a request begins,
not until it ends, so a long response does not fail on the lifetime; a command that outlives its
token signs in again.

| Command | Requests |
|---|---|
| Fetch or clone of a public repository | 2 |
| Fetch or clone of a private repository | 4 |
| Push | 5: sign-in, upload, ref update, and the anchor of the new tip of the log (ADR-026) |
| Push without new objects | 4 |

A fetch also reads the statements of open pull requests (ADR-025) and the namespace policy
(ADR-027), which may add a request each.

### No cache in the first version

A public repository is read with the same two requests as a private one, without a token; there
is no second way of delivery. A later cache works on the clone, not on the object: a new GET
endpoint with a precomputed stream, followed by an ordinary fetch, as a new protocol number that
changes no existing request.

Rejected: a static endpoint per object, a precomputed clone stream now, and a clone as a GET with
`Cache-Control: public` (the hub cannot call a stored entry back, so a repository that became
private or a history that was removed would still be delivered).

### What would make a pack format necessary

The triggers are named without numbers; the thresholds come from a measurement on real Stemma
stores (see the open items).

1. The compression of the stream no longer carries, because the history is larger than the window
   and the ratio falls clearly behind what deltas would reach. First a larger window and another
   order inside a commit are tried, then deltas between objects.
2. The hub becomes the bottleneck of a clone, through the walk over loose objects and a
   compression per clone. The answer is a precomputed, stored stream that the hub serves as a
   file: a pack as a file, even without deltas.
3. A single commit is larger than a connection carries. The answer is a unit that is addressable
   by bytes, as in 2.
4. The store itself needs packs (ADR-007: many small files), after which sending the stored bytes
   pays.

No triggers are the number of round trips, which the batch solves, and a changed target scale,
which needs a new ADR on the whole (ADR-015). In each case no object ID changes; a pack format is
a new protocol number and, where it concerns the store, a new version of `stemma-repository`.

## Consequences

- A clone and a fetch cost a fixed number of requests, whatever the size of the history.
- The hub keeps no state between two requests of a transfer. An interrupted transfer is repeated
  as an ordinary request.
- The format has one storage form. What travels is what the store holds, and the store of a hub
  is the store of a clone.
- A fetch can deliver more than the client lacks. At the target scale that costs bytes only.
- The protocol is strict and has no room for extensions. A new feature costs a new number, and a
  hub that serves two numbers serves two sets of messages.

## Open items

- [ ] A measurement on Stemma stores, once `fetch` exists: the number of objects, the size of the
      stream, the duration of a clone and of a fetch, the holding time of `refs.lock` on the hub
      and the count of repetitions of a merge because of `Stale` (ADR-015, ADR-025). Its numbers
      give the thresholds of the pack triggers.
- [ ] The level and the window of the compression, the bound that a receiver names for the window
      and how the sender learns it, the media types, and the bounds for the size of an object and
      of a request. Left to the slices.
- [ ] Whether `push` reads the references first to report a stale state early. It belongs to the
      command surface.
