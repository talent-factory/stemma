# ADR-023: The transfer protocol

**Status:** Accepted for phase 5. Decided by the author on 2026-11-23 and 2026-11-24 (STEMMA-270
for the format, STEMMA-272 for the mapping to HTTP, with the addenda of STEMMA-56, STEMMA-273,
STEMMA-274 and STEMMA-276) and carried into the ADRs by STEMMA-280. The module
`stemma_core::protocol` exists with the first number and the first four messages (STEMMA-283, see
"The first number is 1") and with the ref update and its receipt (STEMMA-291, ADR-025), and the
receiving side of an object stream exists in `stemma_core::receive` (STEMMA-289, see "The
receiving side"), and the answering side of the list of references and of a fetch exists in
`stemma_core::serve` (STEMMA-288, see "The answering side"); nothing else of it is implemented
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
2. It sends the commits it wants and the commits it has (the tips of its branches, its tags and
   its remote references) in one request. The hub ignores a `have` it does not know, walks the graph
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
  typed error. The values are named under "The receiving side".
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

### The receiving side

The receiving side is built (STEMMA-289) in `stemma_core::receive`: `Repository::receive_objects`
for the upload on a hub, `Repository::check_connectivity` for the check of a ref update, and
`Repository::begin_fetch` with the type `Fetch` for the client.

- **The bounds.** The body of an object has at most 1 GiB (`MAX_OBJECT_LEN`), because the store
  holds an object in memory (ADR-007), and a stream has at most 16 GiB (`MAX_REQUEST_LEN`),
  because a clone is one stream and a history has up to a few gigabytes (ADR-015). Both are the
  defaults of `Bounds`, and a caller, such as a hub with a quota, names other ones. They count
  the bytes that the messages carry, after the compression of the transport is taken off, so a
  small compressed body cannot pass a long stream. A longer object is refused before its body is
  read (`StreamError::TooLarge`) and a longer stream at the bound
  (`ReceiveError::RequestTooLarge`). What arrived whole before the refusal stays in the store
  unnamed.
- **The walk.** The own references are the branches, the tags and the remote references. A commit
  that one of them reaches is taken as complete and is not checked. The history below the
  references is read only until a commit is found in it, so a commit that a reference names
  costs nothing; the answer that a new commit is not part of it needs all of that history, one
  read of every commit below the own references per check (see the open items). For every other
  commit the walk reads the commit, then its parents, then its trees, and descends only into an
  entry that no tree of a parent has with the same ID at the same path. A parent or a tree of a parent
  that cannot be read prunes nothing. Of a blob the walk reads the header of the file and not
  the body: a blob has no strict reader, an object of a stream was hashed on arrival, and a body
  that no longer matches its ID is the matter of a full read, as in the scan of a purge
  (ADR-014).
- **Durable in batches.** The objects of a stream are written to temporary files. At a commit of
  the stream, once 1024 objects or 64 MiB have gathered, and at the end of the stream they are
  flushed and moved into place as one batch of the store (ADR-007). A fetch checks the commits
  of a batch only after that. When the stream is cut off, the same is done for everything that
  was read whole before the error is reported.
- **No lock, and a directory of its own in `tmp/`.** A receiving operation names no object and
  holds no lock of the references. Its temporary files lie in
  `tmp/receive-<pid>-<n>-<random>/`, which the writer that takes the lock leaves, because it
  removes files and not directories (ADR-011). The operation holds a lock of the system on the
  file `guard` in that directory while it runs and removes the directory at its end. A directory
  whose guard is free was left by a process that ended: every receiving operation removes such
  directories at its start, and `Repository::remove_receive_leftovers` does it on request, for a
  hub that starts. The lock decides and not an age, as for `refs.lock` (ADR-008). On a file
  system without locks of files the directory is used without one, and a cleanup that cannot
  lock or open a guard leaves the directory. A directory without a guard is removed only while
  it is empty: its operation ended before it made the guard.
- **The journal `fetch-pending`.** The file lies in the metadata directory (ADR-009) and has the
  first line `stemma-fetch 1` and one line `commit <id>` per commit that arrived completely, in
  the order of arrival. A fetch holds a lock of the system on it, so a second fetch in the same
  repository is refused while the first runs. A fetch that ends removes the name of the journal
  while it holds the lock; a lock on a file whose name no longer leads to it does not count, and
  the journal is opened again, at most 16 times in all, after which the fetch runs on the file
  without a name, and its lines are lost with it. The name is compared by the device and the number of the file on Unix and by its
  existence elsewhere, so outside Unix the race is narrowed and not closed (see the open
  items). On a file system without locks of files two fetches run at once: they may lose lines
  of each other or remove the journal of the other, which costs a second transfer of those
  commits, and a line that they tear makes the next fetch refuse the journal until the file is
  removed by hand.
  A commit that a reference reaches, or that this fetch found complete already (at its start
  from the journal, or since), gets no line; a recorded commit that did not hold at the start
  gets a second line when it arrives again. A line is written, and the file flushed, only
  after the objects of the commit are durable and the commit passed the check. The journal is a
  hint and no proof: the next fetch checks every recorded commit again and sends as `have` those
  that still hold and that no other of them has as a parent. A recorded commit that lost an
  object since, as after a purge, or that reaches an object whose file is damaged, is fetched
  again. A last line without a line feed that is the beginning of a line a fetch writes was
  left by a process that ended while it wrote, and is cut off when the journal is opened;
  every other content, also in a last line without a line feed, is refused with the number of
  its line. The journal is removed after the remote references have moved.
- **`have`.** A fetch sends the commits of its branches, its tags and its remote references, and
  the commits of the journal that still hold and that no other of them has as a parent. The
  tags were added to the decision above by this slice, because the walk takes them as complete
  too.
- **The check and the lock.** `Fetch::check` does not read a commit again that the fetch found
  complete before, at its start from the journal or while it received, without the lock of the
  references. A purge between that read
  and the move of a remote reference is not seen by it; the slice that moves the references
  under the lock decides whether it checks once more there.

Known limit: a process that is killed loses the objects that were not yet flushed. A batch is
closed only at a commit, so that is fewer than 1024 objects and less than 64 MiB, and on top
every object of the commit at which a threshold is reached, however large that commit is. They
arrive again with the next fetch.

### The answering side

The answering side is built (STEMMA-288) in `stemma_core::serve`: `Repository::answer_refs` for
the list of references and `Repository::answer_fetch` for a fetch request, with the type
`Answering` that joins both to the in-memory `Transport` (ADR-022). Both run on every repository,
bare or with a working copy, take no lock, keep no state and leave no file.

- **The list.** It names the branches and the tags of the store. The ID of the repository, the
  keys of the hub and the record entry of each name are not kept by a store; the caller passes
  them (`ListFacts`), a hub from its record (`Chain`, ADR-025). A reference without a record
  entry cannot be listed and fails the call. The references are read one after the other, so the
  list is no snapshot (ADR-025), and a reference is not compared with the record.
- **`want`.** Every `want` must be reachable from a branch or a tag, the references that the list
  names. A remote reference of a clone names nothing that is served. The refusal comes before
  the first byte of the stream and names the first such commit in the order of the IDs. It is
  the same for a commit that the store lacks and for one that it holds unnamed, so it does not
  say whether an object exists.
- **`have`.** A `have` is known if a branch or a tag reaches it. One that the store does not
  hold is ignored after one lookup, and so is one that the store holds unnamed: its history may
  have gaps, and it would tell the sender that an unnamed object exists.
- **The commits.** Sent are the commits that a `want` reaches and no known `have` reaches. The
  walk does not stop at a `have` only: it first collects everything below the known `have`
  commits, because a `want` can reach a commit below a `have` by a way around it, as after a
  merge, and would then send the history of the merged branch down to the root again.
- **The trees and blobs.** For each commit the walk sends the trees and blobs that no tree of a
  parent holds with the same ID at the same path, the comparison of the connectivity check of
  the receiver, and no object twice in one stream. An object that the receiver has by another
  way, such as a blob that returns to an older content, is sent again: too much, never too
  little.
- **The order.** Commits stand after their parents, the parents of a merge in the order in which
  it names them, and the `want` commits in the order of their IDs. Inside a commit a tree stands
  after what it names. Every beginning of the stream that ends with a commit is then complete
  for a receiver that has the `have` commits: the parents of the commit stand before it or lie
  below a `have`, and every object of its trees is held by a parent at the same path or stands
  before it.
- **Failures.** A `want` that no reference reaches is the one refusal, the error
  `unknown-want`. Every other error is a failure of the answering side and no message of the
  protocol. The commits are walked before the stream begins, so a commit that is missing fails
  the call before the first byte. A tree or a blob that is missing, as after a purge that ran
  beside the fetch, cuts the stream off; the receiver keeps what arrived whole and resumes.
- **The cost.** The walk reads the commits that it sends and every commit below the known `have`
  commits once, also for a fetch that brings one commit. A `want` or a `have` that is no tip of
  a reference costs the history below the references on top, until it is found. See the open
  items.

### One number covers every message

`stemma-protocol <n>` is the first line of every message itself, not an HTTP header, so it belongs
to the types of `stemma_core::protocol` and is covered by the in-memory `Transport` and the
checked-in example messages.

- Every small message is line-oriented text as `format`, `HEAD` and the entries of the log are:
  the first line, then lines `<keyword> <value>` in a fixed order, read by a reader that accepts
  only what the writer writes. The object stream stays binary and begins with the same first line.
- There are no capability lists and no optional fields. From the first release that carries the
  protocol on, every change of a message is a new number (see "The first number is 1").
- Not covered, because they belong to the transport: TLS, the carriage of the token, the content
  encoding, and URL paths and status codes.
- The number is independent of `stemma-repository`, which does not travel, and of the object
  kinds. An older client that meets a kind it does not know fails with a typed error and moves no
  reference: in the reader of the stream, which refuses a frame of an unknown kind, or in the
  connectivity check. It learns this on receipt, not on the first request. An ADR that introduces
  an object kind says whether it raises the protocol number. The hash algorithm needs no field
  (ADR-001).

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

### The first number is 1

The first protocol number is 1 (`stemma_core::protocol::VERSION`), set by STEMMA-283. It is the
protocol of phase 5: the slices of the phase add their messages and lines to it, and from the
first release that carries the protocol on, every change of a message is a new number.

A reader judges the first line before anything else of a message, so a message of another number
is refused as such whatever follows. The one exception is the reader of the error: a second line
`error unsupported-version` is read under every number, and strictly (see "Error" below).

A small message has no line that names its type; the request says which message is expected, and
each reader refuses the messages of the others. No message carries a count or an end mark: a
list of references that is cut after a whole line from the `key` line on, a fetch request that
is cut after a whole line from its first `want` on, and a stream that is cut between two frames
read as shorter messages. A cut before a line that must be there is refused as a missing line,
and so is every cut of an error message; a cut inside a line or a frame is refused as cut off.
The carrier must report a body that ends early, as HTTP does by the length of the body, and an
error arrives in place of the message asked for, told apart by the status.

A line ends with a line feed, a field is separated by one space, and a number of a small message
is decimal from 1 without a sign or a leading zero. The first four messages:

```text
stemma-protocol 1                                   the list of references
repository <32 lower-case hexadecimal digits>
key ssh-ed25519 <base64>
former <first> <last> ssh-ed25519 <base64>          zero or more
ref heads/<name> <commit> <record entry>            zero or more, then tags/<name>

stemma-protocol 1                                   the fetch request
want <commit>                                       one or more
have <commit>                                       zero or more

stemma-protocol 1                                   the object stream
<kind> <length> NUL <body>                          zero or more

stemma-protocol <highest version spoken>            the error
error unsupported-version
spoken <version> <version> ...

stemma-protocol 1                                   the refusal of a fetch request
error unknown-want
want <commit>
```

- **List of references.** A key is an Ed25519 public key in the OpenSSH form without a comment.
  A former key names the range of record numbers that it signed (ADR-026); the ranges ascend and
  share no number. The references stand in the order of the bytes of their qualified names, each
  once; the record entry is the hash of the last record entry of the name (ADR-025), in the form
  of an object ID. The list holds the references that exist: a deleted name is not listed, and a
  client that creates it again learns its chain from the `Stale` answer.
- **Fetch request.** `want` and `have` are sets, each written in the order of the IDs, each commit
  once. A request without a `want` is refused.
- **Object stream.** The frame is the header of the store (ADR-007): the kind, the length of the
  body in decimal (`0` for an empty body), a NUL byte, the body, and no ID. The stream has no
  count and no end mark, so a stream that ends between two frames is complete for its reader;
  what is missing shows in the connectivity check. The reader checks the frames only, refuses an
  unknown kind, and refuses a body above the bound of the receiver before it reads it.
- **Error.** The second line names the kind, and the lines after it carry its facts. The first
  kinds are `unsupported-version` with the line `spoken`, and `malformed` with one line
  `reason <text>` for a message that could not be read. `unsupported-version` is the one message
  that is read across numbers: its form never changes, and its first line carries the highest
  version that its sender speaks, so a client of another number still learns the list. The
  versions of `spoken` ascend, each once, at least one, and the first line carries the last of
  them. A reason is one line that is not empty and holds no control character and no line or
  paragraph separator. `unknown-want` answers a fetch request with a `want` that no reference
  of the repository reaches, with one line `want <commit>` for the first such commit in the
  order of the IDs (STEMMA-288, see "The answering side"). The refusals of a ref update, of a
  statement and of the bounds add their kinds with their slices.
- **Ref update and receipt.** The ref update and the record entry that answers it are messages
  of number 1 as well; their lines and rules stand in ADR-025 ("The formats as built").

Example messages are checked in under `crates/stemma-core/protocol-examples/` and are reachable
as `stemma_core::protocol::examples`, so a hub reads them through its dependency (ADR-022). They
are also the seeds of the fuzz target `protocol`.

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
| Push without new objects | 4: the same without the upload |

A fetch also reads the statements of open pull requests (ADR-025) and the namespace policy
(ADR-027), which may add a request each.

### No cache in the first version

A public repository is read with the same two requests as a private one (the list of references
and the fetch), without the sign-in and without a token; there is no second way of delivery. A
later cache works on the clone, not on the object: a new GET endpoint with a precomputed stream,
followed by an ordinary fetch, as a new protocol number that changes no existing request.

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
      and how the sender learns it, and the media types. Left to the slices.
- [x] The bounds for the size of an object and of a request: 1 GiB and 16 GiB as the defaults of
      the receiver (STEMMA-289), see "The receiving side".
- [ ] The cost of the connectivity check for a new commit: one read of every commit below the own
      references per check, on a hub once per ref update. A walk that descends from the new tips
      first and asks only at the commits it reaches, or a cache of the commits that are known
      to be reachable, would avoid it; the measurement above says whether it is needed.
- [ ] The cost of the walk of the answering side: one read of every commit below the known
      `have` commits per fetch. A generation number per commit, or a cache of the commits below
      a reference, would let the walk stop early; the measurement above says whether it is
      needed.
- [ ] The status code of the error `unknown-want`. It belongs to the slice that carries a fetch
      over HTTP; the table under "Mapping to HTTP" does not name it yet.
- [ ] The journal of a fetch outside Unix: a fetch that locks the file of a fetch that ended,
      after a third one made a new journal under the name, runs beside the third and removes
      its journal when it ends, because only the existence of the name is checked there. Both
      cost a second transfer. Closing it needs the identity of the file on
      Windows (`GetFileInformationByHandle`), or a journal that is emptied in place of being
      removed, so that its file never changes.
- [ ] The kind of the error message for a bound that was exceeded (see "Error"). The receiver
      refuses with a typed error of the core; the message on the wire comes with the slice that
      carries an upload over a transport, and until then `StreamError::answer` has none for it.
- [ ] Whether `push` reads the references first to report a stale state early. It belongs to the
      command surface.
- [ ] The list of references carries the former keys without the signature of the current key
      over them (ADR-026). The signature, its payload and its type tag belong to the slice that
      pins the key, which adds the line to protocol 1.
- [ ] Whether the list of references needs a last line or a count, so that a list that is cut
      after a whole line is refused by its reader and not only by the carrier. A list that is
      cut reads as a list with fewer references.
- [ ] Whether the list of references should name deleted names with their last record entry, so
      that a client can create one again without a `Stale` answer first.
