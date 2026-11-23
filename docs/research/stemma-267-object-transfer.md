# Object transfer and negotiation in Git, Jujutsu, Mercurial, Fossil, OCI registries and Git LFS

Research note for STEMMA-267, a ticket of the planning map STEMMA-266 (phase 5: network and hub).
It collects facts for three decisions: the transfer format (chapter 8, open question
"Übertragungsformat"; ADR-007, open item pack files), authentication over HTTP (STEMMA-271), and
remote references (ADR-008, open item; STEMMA-56). For each system it answers four questions:
how objects travel, how the two sides find out what is missing, how a client authenticates over
HTTP, and how a reference moves on the server. It names the number of round trips for a clone and
for a small fetch, and what the server must compute. It records facts only and decides nothing for
Stemma. Scion is described in ADR-015; no protocol is taken from it, and it is not part of this
note.

## Sources and method

Sources are primary: the specifications, documentation and source code of the projects that own
the behaviour, read on 2026-11-23 at the revisions below. Each claim names its source in square
brackets. A claim marked *(derived)* is a conclusion of this note from the cited text, not a
statement of the source. A claim marked *(measured)* comes from the trace in section 1.9.

Git, repository <https://github.com/git/git>, branch `master`, commit
`f7de81456940dd6f3274e461149bba12965de609` (2026-11-19; the newest tag is `v2.56.1`). All files
are under `Documentation/`:

- `[v2]` `gitprotocol-v2.adoc`
- `[http]` `gitprotocol-http.adoc`
- `[pack-proto]` `gitprotocol-pack.adoc`
- `[caps]` `gitprotocol-capabilities.adoc`
- `[pack-fmt]` `gitformat-pack.adoc`
- `[bundle-fmt]` `gitformat-bundle.adoc`
- `[recv]` `git-receive-pack.adoc`
- `[push]` `git-push.adoc`
- `[pack-objects]` `git-pack-objects.adoc`, `[index-pack]` `git-index-pack.adoc`
- `[bitmap]` `technical/bitmap-format.adoc`
- `[pf-uri]` `technical/packfile-uri.adoc`
- `[layout]` `gitrepository-layout.adoc`, `[usi]` `git-update-server-info.adoc`
- `[cred]` `git-credential.adoc`
- `[cfg]` the files `config/fetch.adoc`, `config/push.adoc`, `config/receive.adoc`,
  `config/uploadpack.adoc`, `config/http.adoc` and `config/protocol.adoc`

Jujutsu, repository <https://github.com/jj-vcs/jj>, branch `main`, commit
`8f7518a2e0ddd4e0ec90c77e8925f3951e3b4fcf` (2026-11-23; the newest release is `v0.47.1`):

- `[jj-compat]` `docs/git-compatibility.md`
- `[jj-bookmarks]` `docs/bookmarks.md`
- `[jj-sub]` `lib/src/git_subprocess.rs`
- `[jj-changelog]` `CHANGELOG.md`

Mercurial, repository <https://foss.heptapod.net/mercurial/mercurial-devel>, branch `default`,
changeset `3895bd310c86dc24da119a30ba7f1c5894c669a3` (2026-10-26; the newest tag is `7.2.4`):

- `[hg-wire]` `mercurial/helptext/internals/wireprotocol.txt`
- `[hg-b2]` `mercurial/helptext/internals/bundle2.txt`
- `[hg-cg]` `mercurial/helptext/internals/changegroups.txt`
- `[hg-disco]` `mercurial/setdiscovery.py` (module documentation and `findcommonheads`)
- `[hg-cb]` `hgext/clonebundles.py` (module documentation)
- `[hg-config]` `mercurial/helptext/config.txt`

Fossil, mirror <https://github.com/drhsqlite/fossil-mirror>, branch `master`, commit
`18f2de73934634c7dacfba853b3a8f069261a0c3` (2026-11-22; the file `VERSION` says 2.29):

- `[fossil-sync]` `www/sync.wiki`

OCI registries:

- `[oci]` `spec.md` of <https://github.com/opencontainers/distribution-spec> at tag `v1.1.1`
- `[oci-token]` `docs/content/spec/auth/token.md` of
  <https://github.com/distribution/distribution> at tag `v3.1.2`

Git LFS, repository <https://github.com/git-lfs/git-lfs> at tag `v3.8.1`:

- `[lfs-batch]` `docs/api/batch.md`
- `[lfs-basic]` `docs/api/basic-transfers.md`
- `[lfs-auth]` `docs/api/authentication.md`

The official Mercurial repository at `repo.mercurial-scm.org` answers scripted requests for files
with a bot check, so the Mercurial files were read from Heptapod, the development forge of the
project. The changeset ID is the same on both hosts.

### Not verified

- **Round trips other than the Git fetch side.** Only the Git clone, the empty fetch and the
  small fetch were measured (section 1.9). Every other count in this note is derived from the
  text of a specification and marked so. No push was traced for any system.
- **Server cost.** No timing and no memory figure was measured. What a server "must compute" is
  taken from the documentation, not from a profile.
- **Git source code.** Only the documentation of Git was read, not `upload-pack.c`,
  `fetch-pack.c`, `receive-pack.c` or `http-walker.c`. The order of the requests of the dumb HTTP
  client and the push over WebDAV (`git http-push`) are therefore not verified.
- **Push in protocol v2.** `[v2]` defines the commands `ls-refs`, `fetch`, `object-info` and
  `bundle-uri` and no push command. That a current Git client pushes with the older protocol
  described in `[pack-proto]` and `[http]` follows from that absence; it was not traced.
- **Jujutsu.** Only the command lines that `jj` passes to `git` were read. Which objects `jj`
  creates before a push and how it imports references after a fetch were not read.
- **Mercurial.** The second version of the wire protocol (`wireprotocolv2`), the streaming clone
  format in detail, and the server code were not read. The request sequence of a clone is derived
  from the command documentation and from `findcommonheads`.
- **Fossil.** Only `sync.wiki` was read. The artifact format (`fileformat.wiki`), the delta
  format and how a branch tip is derived from the artifacts were not read.
- **OCI.** The token format (`jwt.md`), the OAuth variant (`oauth.md`) and the chunked upload
  were not read in detail. Whether any registry implements a conditional push of a tag was not
  checked.
- **Hosting services.** How GitHub, GitLab or Gerrit authenticate and authorise is outside the
  sources read. Section 1.8 covers only what Git itself specifies.

## 1. Git over smart HTTP

### 1.1 Framing, versions and statelessness

All messages use pkt-line framing: a four-digit hexadecimal length followed by the payload.
Protocol v2 gives three lengths a special meaning: `0000` ends a message (flush), `0001`
separates sections (delimiter), and `0002` ends a response on a stateless connection `[v2]`.

Three protocol versions exist: `0` is the original protocol, `1` adds a version line to it, and
`2` is the command-oriented protocol. A client uses `2` by default and falls back to `0` if the
server does not support it `[cfg]` (`protocol.version`). Over HTTP the client asks for v2 with
the header `Git-Protocol: version=2` on its first request `[v2]`.

Git over HTTP is stateless on the server. "All state MUST be retained and managed by the client
process", so that a server farm can balance requests round-robin; servers must not need cookies
`[http]`. Protocol v2 repeats the rule: every command lasts one round, unless a capability says
otherwise `[v2]`.

The requests are `[http]` `[v2]`:

| Request | Purpose |
|---|---|
| `GET $GIT_URL/info/refs?service=git-upload-pack` | v0: references; v2: capabilities |
| `POST $GIT_URL/git-upload-pack` | v2 commands `ls-refs`, `fetch`; v0 negotiation |
| `GET $GIT_URL/info/refs?service=git-receive-pack` | references and capabilities for a push |
| `POST $GIT_URL/git-receive-pack` | reference updates and the pack |

Media types are `application/x-git-upload-pack-advertisement`, `-request` and `-result` (and the
same for `receive-pack`). A client that receives another content type for `info/refs` falls back
to the dumb protocol of section 2 `[http]`.

### 1.2 Reference advertisement

In v0 the answer to `info/refs` lists every reference of the repository with its object ID, and
hides the capabilities behind a NUL byte on the first line `[http]`. The cost grows with the
number of references, whatever the client wants.

Protocol v2 removes this: "Reference advertisement will be omitted unless explicitly requested".
The answer to `info/refs` is only `version 2` and the capability list. The client then sends the
command `ls-refs`, which takes `ref-prefix <prefix>` arguments to limit the answer, `symrefs` to
learn the target of a symbolic reference, and `peel` for the commit behind a tag. The prefix is
"purely for optimization": the server may send more, and the client filters `[v2]`.

A fetch by object ID needs no reference: in v2 "Wants can be anything and are not limited to
advertised objects" `[v2]`. Whether a server accepts an ID that no reference points to is a
server setting (`uploadpack.allowTipSHA1InWant`, `allowReachableSHA1InWant`,
`allowAnySHA1InWant`); the documentation notes that "calculating object reachability is
computationally expensive" `[cfg]`.

With the feature `ref-in-want` the client names a reference instead of an object ID
(`want-ref <ref>`), and the server answers with the ID it resolved. It exists "for the benefit of
load-balanced servers which may not have the same view of what OIDs their refs point to due to
replication delay" `[cfg]` `[v2]`.

### 1.3 Negotiation with `want` and `have`

The client names what it wants (`want <oid>`) and what it holds (`have <oid>`). The server answers
which of the `have` objects it also holds, and both sides thus find a set of common commits. The
server then sends everything reachable from the wants and not reachable from the common
commits `[v2]` `[http]`.

- **Unit.** Negotiation is about commits. Trees and blobs are never named; they follow from the
  commit graph. This works only because the server can walk the graph.
- **Client side.** The client keeps a queue of its own commits ordered by commit time, newest
  first, and sends `have` lines in batches. `[http]` describes batches of 32 and says that the
  client gives up ("done") after 256 `have` lines without a common commit, or when its queue is
  empty.
- **Server side.** For each `have` the server checks whether it holds the object reachable from a
  reference. It stops when there is "at least one path from every want to at least one common
  object" `[http]`.
- **Answer in v2.** The response has sections: `acknowledgments` (`ACK <oid>` for each common
  object, or `NAK`), then, if the server has found "an acceptable common base", the line `ready`
  and the `packfile` section in the same response. A clone sends `want` lines and `done` at once
  and receives no acknowledgments `[v2]`.
- **Algorithms.** `fetch.negotiationAlgorithm` selects how the client picks `have` lines:
  `consecutive` (the default) walks commit by commit; `skipping` skips commits "in an effort to
  converge faster, but may result in a larger-than-necessary packfile"; `noop` sends nothing and
  skips the negotiation `[cfg]`.
- **Stateless cost.** Because the server keeps nothing between requests, a client that needs a
  further round sends the common objects again together with the new `have` lines `[http]`.

A fetch can be limited: `deepen <depth>` and related arguments give a shallow history, and
`filter <filter-spec>` omits objects (for example all blobs) for a partial clone `[v2]`. Both
break the premise of ADR-015 that every clone holds the full object store, and are named here
only for completeness.

### 1.4 Packs and thin packs

Objects travel in one pack. A pack is a 12-byte header (`PACK`, a version, the number of
objects), the object entries, and a trailing checksum over everything before it `[pack-fmt]`.

- **Entry.** An entry is a variable-length header with a 3-bit type and the size, followed by
  zlib-compressed data. Unlike a loose object, the entry does not carry the `<type> <size>` NUL
  prefix; "the computation of the object ID still uses this prefix by reconstructing it"
  `[pack-fmt]`. A pack therefore contains no object IDs of its full objects: the receiver must
  inflate and hash each object to learn its ID.
- **Deltas.** An object may be stored as a delta against a base. `OBJ_OFS_DELTA` names the base
  by its offset in the same pack, `OBJ_REF_DELTA` by its object ID. A delta is a sequence of two
  instructions, "copy a byte range from the base" and "insert these bytes" `[pack-fmt]`.
- **Thin pack.** A `REF_DELTA` may name a base that is not in the pack "but known to exist at the
  receiving end". Such a pack is thin. It "can reduce the network traffic significantly, but it
  requires the receiving end to know how to thicken these packs by adding the missing bases"
  `[v2]` `[caps]`. On disk a pack must be self-contained; `git index-pack --fix-thin` appends the
  missing bases `[index-pack]`.
- **Choosing deltas.** `git pack-objects` sorts the objects by type, size and name and compares
  each against the others in a window (`--window`, `--depth`) to find a delta that saves space.
  When the repository already has packs, it "reuses existing deltas" instead of computing them
  again `[pack-objects]`.
- **Multiplexing.** In v2 the pack is always sent in side-band packets: stream 1 is pack data,
  stream 2 progress text, stream 3 a fatal error `[v2]`.

A bundle is the same pack in a file, preceded by a text header that lists references and
prerequisites (objects the reader must already hold) `[bundle-fmt]`.

### 1.5 Moving the work off the server

Two v2 features let a server answer with addresses instead of bytes:

- **`bundle-uri`.** Before the fetch the client asks for a list of bundle URIs, downloads a
  bundle from a static host, and then fetches only what is newer. The stated intent is to
  "optimize for server resource consumption" by turning the fetch of "a very large PACK" during a
  clone "into a smaller incremental fetch". A client must degrade gracefully when a bundle is
  missing or bad `[v2]`.
- **`packfile-uris`.** Inside a fetch response the server may list URIs of packs, each with the
  hash of the pack, in place of some objects. The client downloads and indexes them before it
  checks connectivity. The feature "allows server designs that improve scalability in bandwidth
  and CPU usage (for example, by serving some data through a CDN)". The shipped server
  implementation is marked experimental `[pf-uri]`.

### 1.6 Push with `receive-pack` and the atomic reference update

A push has two steps `[http]` `[pack-proto]`:

1. `GET info/refs?service=git-receive-pack`. The server lists its references and the capabilities
   `report-status`, `report-status-v2`, `delete-refs`, `ofs-delta`, `atomic`, `push-options`.
2. `POST git-receive-pack`. The body is a list of commands followed by one pack:

   ```
   <old-id> SP <new-id> SP <refname>
   ```

   A zero ID as `old-id` creates the reference, a zero ID as `new-id` deletes it.

Facts about this exchange:

- **No negotiation by default.** The client decides what to send from the reference list alone.
  `push.negotiate` adds rounds in which "the client and the server attempt to find commits in
  common"; it is off unless set `[cfg]`.
- **Compare-and-swap.** The `old-id` is the value the client saw. After unpacking, the server
  will "validate each reference that is being updated that it hasn't changed while the request
  was being processed (the obj-id is still the same as the old-id)" `[pack-proto]`. This is the
  same shape as `update_branch(name, expected, new, objects)` of ADR-008.
- **Atomic.** Without the capability "some references can be updated while others can be
  rejected". With `atomic` "the server will update the refs in one atomic transaction. Either all
  refs are updated or none" `[pack-proto]` `[caps]`.
- **Report.** With `report-status` the answer is `unpack ok` or `unpack <error>`, then one line
  per reference: `ok <refname>` or `ng <refname> <reason>`, for example
  `ng refs/heads/master non-fast-forward` `[pack-proto]`.
- **Quarantine.** Incoming objects go to a temporary directory and are "migrated into the main
  object store only after the `pre-receive` hook has completed. If the push fails before then,
  the temporary directory is removed entirely". A rejected push leaves no data, and other
  processes never see objects of a push that is not accepted `[recv]`.
- **Hooks as the policy point.** `pre-receive` receives all `old new refname` lines on standard
  input before any reference moves; a non-zero exit rejects the whole push. `update` runs per
  reference `[recv]`. Git has no built-in notion of who may move which reference beyond
  `receive.denyNonFastForwards` and `receive.denyDeletes` `[cfg]`.
- **Limits.** `receive.maxInputSize` bounds the pack, `receive.fsckObjects` checks the objects,
  and `receive.unpackLimit` decides whether a small push is exploded into loose objects or kept
  as a pack `[cfg]`.

### 1.7 Push certificates

A push certificate is a signed copy of the command list. The note of STEMMA-223 left its format
open; `[pack-proto]` defines it:

```
push-cert NUL <capabilities>
certificate version 0.1
pusher <ident>
pushee <url>
nonce <nonce>
push-option <option>        (zero or more)
                            (empty line)
<old-id> <new-id> <refname> (one line per command)
<signature lines>
push-cert-end
```

- The signature is "a detached signature for the contents recorded in the push certificate before
  the signature block begins" and certifies "that the commands were given by the pusher, who must
  be the signer" `[pack-proto]`.
- When a certificate is sent, the plain command list is not sent: "the commands recorded in the
  push certificate is used instead" `[pack-proto]`.
- **Replay protection.** The server advertises `push-cert=<nonce>`; the client must put that
  nonce into the certificate. The server derives the nonce with an HMAC under
  `receive.certNonceSeed`, so that a stateless server can recognise its own nonce on the next
  request. `receive.certNonceSlop` accepts a nonce that is a number of seconds old, which is
  needed because the nonce is issued in one HTTP request and returned in another `[caps]` `[cfg]`
  `[recv]`.
- **What the server does with it.** Git verifies the signature, stores the certificate as a blob,
  and exports `GIT_PUSH_CERT`, `GIT_PUSH_CERT_SIGNER`, `GIT_PUSH_CERT_KEY`,
  `GIT_PUSH_CERT_STATUS` and `GIT_PUSH_CERT_NONCE_STATUS` to the hooks. Git itself neither
  rejects a push on a bad certificate nor keeps a log of certificates; the documentation shows a
  `post-receive` hook that mails them to a log address `[recv]`.
- **What is covered.** The certificate binds the pusher, the target URL, the nonce and the
  triples (old ID, new ID, reference name). It does not cover the pack; the new ID covers the
  content through the hash chain.

### 1.8 Authentication over HTTP

"The protocol itself contains no authentication mechanisms. That is to be handled by the
transport" `[pack-proto]`. Over HTTP this means standard HTTP authentication, "configured and
enforced by the HTTP server software" `[http]`:

- Clients should support Basic authentication, servers should support it "by relying upon the
  HTTP server placed in front of the Git server software", and both should support TLS "to
  protect passwords" `[http]`.
- An inaccessible repository answers `403 Forbidden`; a missing one must not answer `200` `[http]`.
- **Challenge first.** By default Git sends the first request without credentials and reacts to
  the `401`. `http.proactiveAuth` sends credentials at once and saves that round trip `[cfg]`.
- **Where the secret comes from.** Git asks a credential helper. Besides `username` and
  `password` a helper may return `authtype` and `credential`, which Git joins into the
  `Authorization` header; the documented types are `basic`, `bearer` and `digest`. `ephemeral`
  marks a credential that must not be stored, `password_expiry_utc` and `oauth_refresh_token`
  support tokens with a lifetime, and Git passes the `WWW-Authenticate` headers of the server to
  the helper (`wwwauth[]`) `[cred]`.
- `http.extraHeader` adds a fixed header to every request, and `http.sslCert` configures a TLS
  client certificate `[cfg]`.

Authorisation (who may read or move what) is not part of Git over HTTP. It is done by the web
server per path, or by hooks (section 1.6).

### 1.9 Round trips and server work

Measured on 2026-11-23 with the client `git version 2.49.0` and `GIT_TRACE_CURL` against the
public repository `https://github.com/octocat/Hello-World`, without authentication, protocol v2:

| Operation | HTTP requests | Sequence |
|---|---|---|
| Clone of one branch | 3 | `GET info/refs`; `POST` `ls-refs`; `POST` `fetch` |
| Fetch, nothing new | 2 | `GET info/refs`; `POST` `ls-refs` |
| Fetch of one further branch | 3 | `GET info/refs`; `POST` `ls-refs`; `POST` `fetch` |

In the third case the single `fetch` request carried one `want` and three `have` lines; the
response carried `ACK`, `ready` and the pack. One negotiation round was enough *(measured)*.

Derived from the specifications, not measured:

- A fetch whose first batch of `have` lines contains no common commit needs one more `POST` per
  round `[http]` `[v2]`.
- With protocol v0 a clone is 2 requests, because `info/refs` already lists the references
  `[http]`.
- A push is 2 requests (section 1.6), plus the rounds of `push.negotiate` if it is set.
- A server that demands credentials adds one request with the answer `401`, unless the client
  sends them proactively `[cfg]`.

What the server must compute:

- **For `ls-refs`:** read the references that match the prefixes.
- **For `fetch`:** look up each `have`; walk the commit graph from the wants until the common
  commits are reached; enumerate every tree and blob reachable from the commits to send; select
  deltas and compress, or reuse stored deltas; stream the pack. The enumeration is the part that
  reachability bitmaps replace: a bitmap stores "reachability information about the set of
  objects in a packfile" `[bitmap]`, so the set difference is computed on bit vectors instead of
  by a walk *(derived)*.
- **For a push:** read the pack, inflate and hash each object to obtain its ID, resolve the
  deltas, add the bases of a thin pack, optionally run `fsck`, check that the new tips are
  connected, run the hooks, and move the references in a transaction `[pack-proto]`
  `[index-pack]` `[recv]`.

The client of a fetch does the same work as the server of a push: it indexes the pack and checks
connectivity.

## 2. Git over dumb HTTP

The dumb protocol "requires only a standard HTTP server": the client reads the files of the
repository by path `[http]`.

- **References.** `GET $GIT_URL/info/refs` returns a text file, one line per reference with
  object ID, a tab and the name `[http]`.
- **Objects.** A loose object is fetched by its ID, for example
  `objects/d0/49f6c27a2244e12041955e262a404c7faba355` `[http]`. The file `objects/info/packs`
  lists the packs "to help dumb transports discover what packs are available" `[layout]`.
- **No negotiation.** The client compares the references with its own, fetches the tip commit,
  parses it, and fetches every tree, blob and parent that it does not hold. It finds out what is
  missing by walking, one object per request, or by downloading a whole pack that contains the
  object *(derived; the client code was not read)*.
- **Server work.** None at request time. But `info/refs` and `objects/info/packs` must be
  regenerated with `git update-server-info` whenever a reference or a pack changes `[usi]`
  `[layout]`.
- **Authentication.** The same HTTP authentication as in section 1.8; access control is per path
  in the web server `[http]`.
- **Caching.** Responses may carry `ETag` and `Last-Modified`, and clients may revalidate
  `[http]`. An object addressed by its hash never changes, so only `info/refs` needs
  revalidation *(derived)*.
- **Round trips.** A clone costs one request for the references and then, in the worst case, one
  request per object; with packs, one request for the list, and two per pack (index and pack)
  *(derived)*. A small fetch of a few loose objects costs one request per object. If the server
  has repacked since, the client must download a whole pack to obtain a few objects *(derived)*.
- **Push.** `[http]` describes no push for the dumb protocol (see "Not verified").

## 3. Jujutsu

Jujutsu has no transfer protocol of its own. With the Git backend, "`git` is used for remote
operations under the hood", for the transport and for authentication `[jj-compat]`.

- **History of the implementation.** `jj` first used `libgit2`. Version 0.27.0 made the external
  `git` process the default, and version 0.30.0 removed the `libgit2` path `[jj-changelog]`. The
  minimum Git version is 2.42.0 `[jj-sub]`.
- **Fetch.** `jj` runs `git fetch --porcelain --prune --no-write-fetch-head` and parses the
  output `[jj-sub]`. Negotiation, pack and round trips are those of section 1.
- **Push.** `jj` runs `git push --porcelain --no-verify` with one
  `--force-with-lease=<ref>:<expected>` per reference. The source comment says: "All pushes are
  forced, using --force-with-lease to perform a test&set operation on the remote repository"
  `[jj-sub]`.
- **Remote references in the model.** `jj` records the last known position of each remote
  bookmark (`main@origin`). Before a push it checks "that the actual state of the remote bookmark
  matches jj's record of its last known position" and refuses otherwise; a bookmark that moved on
  both sides becomes a conflicted bookmark after a fetch, not a silent overwrite
  `[jj-bookmarks]`.
- **What does not travel.** The operation log is local. The change ID travels inside the Git
  commit, as a commit header, controlled by `git.write-change-id-header` `[jj-compat]`.

For the questions of this note, Jujutsu adds one fact to Git: every reference update is a
compare-and-swap against the last value that the client saw, never an unconditional write.

## 4. Mercurial

### 4.1 Transport and commands

Mercurial's wire protocol is "a request-response based protocol". Over HTTP every command is a
request to the repository URL with the command name in the query string, for example
`?cmd=capabilities`; arguments go into the query string, into `X-HgArg-<N>` headers, or into the
body `[hg-wire]`. The first request is `capabilities`.

The command `batch` exists to "issue multiple commands while sending a single command request
... avoiding multiple round trips" `[hg-wire]`.

### 4.2 Discovery: what does the other side lack?

Mercurial separates discovery from transfer.

- `heads` returns the heads of the remote graph. `known` takes a list of changeset IDs and
  answers with a string of `0` and `1`, one character per ID `[hg-wire]`.
- **Set discovery.** All local changesets start as `unknown`. The client samples some, asks
  `known(sample)`, and classifies: a changeset that the remote knows is `common` together with
  all its ancestors; one that it does not know is `missing` together with all its descendants.
  This repeats "until unknown is empty" `[hg-disco]`.
- The first sample is the local heads, so that "in the case where the local repo is a subset, you
  computed the answer in one round trip". Later samples are chosen to classify as many changesets
  as possible, as in a bisection `[hg-disco]`.
- The first round sends `heads` and `known` together in one `batch` request `[hg-disco]`.
- The motive is stated: the older protocol "could potentially require lots of roundtrips to
  discover that the local repo was a subset of remote" `[hg-disco]`.

The server answers a membership question per changeset and does not walk the graph during
discovery; the client does the classification *(derived)*.

### 4.3 Transfer: changegroups in bundle2

- `getbundle` takes `heads` (what the client wants) and `common` (the result of discovery) and
  returns "a changegroup with the nodes between the specified common and heads nodes"
  `[hg-wire]`.
- A changegroup has three segments: changesets, manifests, file logs. Each is a *delta group*,
  "a series of deltas, or patches against previous revisions" `[hg-cg]`. Deltas are part of the
  format, not an optional compression.
- **bundle2** is a container: the magic `HG20`, stream parameters, then *parts*. A part is a
  changegroup, bookmarks, phases, obsolescence markers, a reference update (`pushkey`), a check,
  or a reply. A stream parameter whose name starts with an upper-case letter, and a part type
  that contains one, is mandatory: a receiver that does not understand it must stop. All others
  are advisory. The same format is used on the wire and on disk `[hg-b2]`.
- **Streaming clone.** `stream_out` sends the store files as they are ("raw revlog data") instead
  of a computed bundle `[hg-wire]`.
- **Clone bundles.** The server advertises pre-generated bundles on a static host. The client
  downloads one, then pulls the rest. The documentation gives the reason ("there is no caching on
  the server and the server will have to redundantly generate the same outgoing bundle in
  response to each clone request") and the effect ("can reduce server load for clones to less
  than 1% of original") `[hg-cb]`. The bundle2 part `remote-changegroup` does the same inside a
  response and carries the size and a digest of the remote file `[hg-b2]`.

### 4.4 Push and reference update

- `unbundle` sends a bundle to the server. Its argument `heads` is the list of "server repository
  heads observed by the client", used "to detect race conditions and abort push operations before
  a server performs too much work" `[hg-wire]`.
- Inside a bundle2 the same check is a part: `check:heads` (all heads), `check:updated-heads`
  (only the heads this push replaces), `check:bookmarks` and `check:phases`. A mismatch is
  answered with `error:pushraced` `[hg-b2]`.
- A bookmark or a phase moves with `pushkey`: a namespace, a key, the `old` value and the `new`
  value `[hg-wire]` `[hg-b2]`. This is again a compare-and-swap.
- Data, checks and reference updates travel as parts of one bundle2 in one request, and the reply
  is a bundle2 with one `reply:*` part per request part `[hg-b2]`.

### 4.5 Authentication

HTTP authentication. The client reads user name and password per URL prefix from the `[auth]`
section, or sends cookies from a file. The built-in server decides with `web.allow-push` and
`web.deny_push`, lists of user names that the web server has authenticated, and requires TLS for
a push by default (`web.push_ssl`) `[hg-config]`.

### 4.6 Round trips and server work *(derived)*

- **Clone:** `capabilities`; `batch` with `heads` and `known`; `getbundle`. Three requests, or
  two plus the download of a clone bundle and a following pull.
- **Small fetch:** the same three requests when the local repository is a subset of the remote
  one (discovery ends after the first sample). Otherwise one more `known` request per sampling
  round.
- **Push:** `capabilities`; discovery; `unbundle`.
- **Server work:** per `known`, a lookup per ID. Per `getbundle`, the set of changesets between
  `common` and `heads`, then the manifests and file revisions they introduce, each emitted as a
  delta. Per `unbundle`, the checks, the application of the deltas, and the hooks.

## 5. Fossil

### 5.1 Model

A Fossil repository is "an unordered collection of artifacts", each "identified by a
cryptographic hash of its content". Synchronisation shares artifacts "so that all repositories
have copies of all artifacts"; the order of arrival does not matter. The document calls the model
a grow-only set (a CRDT) `[fossil-sync]`.

### 5.2 Transport and cards

- Every request is a `POST` to `<url>/xfer` with the content type `application/x-fossil`. The
  body is zlib-compressed text, a sequence of *cards*, one per line `[fossil-sync]`.
- "The client maintains state between all requests. But on the server side, each request is
  independent" `[fossil-sync]`.
- `file <id> <size>` carries a whole artifact, `file <id> <delta-source-id> <size>` a delta
  against another artifact. "A delta might be sent before the source of the delta", so both sides
  must hold deltas until the source arrives `[fossil-sync]`.
- `igot <id>` announces that the sender holds an artifact. `gimme <id>` asks for one
  `[fossil-sync]`.

### 5.3 Finding what is missing

Negotiation is by object ID, not by graph position.

- A side that learns of an ID it does not hold stores a *phantom* and sends `gimme` for every
  phantom in its next message `[fossil-sync]`.
- Listing every ID would be large, so Fossil uses *clusters*: a cluster is itself an artifact
  whose content is a list of artifact IDs. Each repository keeps a table `unclustered` of the
  artifacts that no cluster mentions. A side sends `igot` only for its unclustered artifacts.
  When the server has more than 100 of them, it writes a new cluster that covers them
  `[fossil-sync]`.
- The effect is that the optimisations "usually reduce the number of hashes that need to be
  shared to a few dozen". A client that misses a cluster asks for it, reads the IDs in it, and
  asks for those it lacks; clusters can name other clusters `[fossil-sync]`.
- The server limits a reply to "about 1MB", and "the client will keep sending HTTP requests until
  it holds all artifacts that exist on the server" `[fossil-sync]`.

### 5.4 Clone

A clone uses a sequence number instead of `igot` and `gimme`: `clone 3 <seqno>`. The server sends
artifacts from that position up to the size limit and answers with `clone_seqno`, where it
stopped. In protocol version 3 the artifacts are sent as `cfile` cards, which carry "the
compressed artifact directly from the server database", so the server does not recompress
`[fossil-sync]`.

### 5.5 Authentication

Authentication is inside the payload, not in HTTP: the first card is
`login <userid> <nonce> <signature>`. The nonce is the SHA-1 of the rest of the message, the
signature is the SHA-1 of the nonce and the password of the user `[fossil-sync]`. The message is
thus bound to a shared secret, and the secret itself does not travel. Since version 2.27 the
login card may travel in a `Cookie` header instead. The server checks the capabilities of the
user (pull, push) per request `[fossil-sync]`.

### 5.6 References

`sync.wiki` describes no command that moves a reference. Everything that is synchronised is an
artifact, and the set only grows. The one coordination device is advisory: a client that is about
to commit sends `pragma ci-lock <parent> <client-id>`, and the server answers `ci-lock-fail` if
another client announced a commit on the same parent; the lock expires after one minute
`[fossil-sync]`. How a branch tip follows from the artifacts was not read (see "Not verified").

### 5.7 Round trips and server work *(derived)*

- **Clone:** one request per megabyte of repository, roughly. A history of a few gigabytes means
  thousands of sequential requests.
- **Small fetch:** two requests in the usual case. The first returns `igot` cards for the
  unclustered artifacts of the server; the second sends `gimme` for the unknown ones and receives
  the `file` cards. A repository that is far behind needs more rounds to descend through
  clusters. Nothing is reported as "done": the client stops when a round brings nothing new.
- **Server work:** a lookup per `gimme`, a scan of the `unclustered` table, and at times the
  creation of a cluster. The server walks no graph and computes no delta at request time; it
  sends the delta it has stored.

## 6. OCI registries

A registry stores *blobs*, each "addressable by a digest", and *manifests*, JSON documents that
list the digests of the blobs that form one object. A *tag* is "a custom, human-readable pointer
to a manifest" `[oci]`. The parallel to a version control system: blob and manifest are objects,
the tag is the reference.

### 6.1 Requests

| Method | Path | Purpose |
|---|---|---|
| `GET`, `HEAD` | `/v2/<name>/blobs/<digest>` | fetch a blob, or ask whether it exists |
| `GET`, `HEAD` | `/v2/<name>/manifests/<reference>` | fetch a manifest by tag or digest |
| `POST` | `/v2/<name>/blobs/uploads/` | open an upload session |
| `PUT` | `/v2/<name>/blobs/uploads/<session>?digest=<digest>` | finish the upload |
| `PUT` | `/v2/<name>/manifests/<reference>` | store a manifest and set the tag |
| `GET` | `/v2/<name>/tags/list` | list the tags |

All from `[oci]`.

### 6.2 Transfer and finding what is missing

- **One request per object.** There is no pack and no delta. A blob is transferred whole;
  `Range` requests resume an interrupted download `[oci]`.
- **Verification by the client.** "Clients SHOULD verify that the response body matches the
  requested digest" `[oci]`. The registry need not be trusted for content.
- **The client finds what is missing.** A pull reads the manifest, which names every blob; the
  client fetches those it does not hold *(derived)*. Before a push the client asks with `HEAD`
  whether a blob exists `[oci]`. The server never computes a difference.
- **Push order.** "The blobs making up the object are uploaded first, and the manifest last". A
  registry may reject a manifest that names a blob it does not hold (`MANIFEST_BLOB_UNKNOWN`)
  `[oci]`. The reference thus never points at an incomplete object.
- **Upload.** `POST` opens a session and returns a `Location`; `PUT` to it with `?digest=` sends
  the bytes. A single `POST` with the digest also works. The `Location` may point to another
  server, so that the registry offloads the storage `[oci]`.
- **Mount.** `POST /v2/<name>/blobs/uploads/?mount=<digest>&from=<other>` makes a blob of another
  repository of the same registry available without an upload `[oci]`.

### 6.3 Reference update

`PUT /v2/<name>/manifests/<tag>` stores the manifest and points the tag at it. The specification
defines no expected old value. Its only word on concurrency concerns the fallback tag of the
referrers list: "multiple clients could attempt to update the tag simultaneously resulting in
race conditions and data loss. Protection against race conditions is the responsibility of
clients and end users", and "Clients MAY use a conditional HTTP push for registries that support
ETag conditions" `[oci]`. A compare-and-swap is therefore optional and outside the required
behaviour.

### 6.4 Authentication

The distribution specification leaves authentication open: the endpoint `/v2/` "MAY be used for
authentication/authorization purposes, but this is out of the purview of this specification"
`[oci]`. The scheme of the reference implementation `[oci-token]`:

1. The client sends its request without credentials.
2. The registry answers `401` with a `WWW-Authenticate` header of the scheme `Bearer`. Its
   parameters are `realm` (the URL of the token service), `service` (the name of the registry)
   and `scope`, for example `repository:samalba/my-app:pull,push`.
3. The client requests a token from the token URL, with the `service` and the `scope` of the
   challenge. It authenticates there with Basic authentication or OAuth 2.
4. The token service returns a signed bearer token whose claims list the granted actions per
   repository. Without `expires_in` a token is valid for 60 seconds.
5. The client repeats the request with `Authorization: Bearer <token>`.
6. The registry authorises the request "by validating the Bearer token and the claim set embedded
   within it".

Two properties follow: the registry never sees the password, and the permission (repository and
action) is inside the token, so that the storage server checks it without a user database
*(derived)*.

### 6.5 Round trips and server work *(derived)*

- **Pull:** the challenge, the token, the manifest, then one request per blob the client lacks.
  Blob requests are independent of each other and can run in parallel.
- **Push:** per blob one `HEAD`, and for a missing blob one or two upload requests; then one
  `PUT` for the manifest.
- **Server work:** a lookup by digest and a stream of bytes. On upload the registry hashes the
  bytes and compares the digest. On a manifest it may check that the named blobs exist.

## 7. Git LFS batch API

Git LFS stores large files outside the Git objects and transfers them by their SHA-256. It is the
closest existing design to "single objects over HTTP" that avoids one round trip per object for
the question of existence.

- **One request for many objects.** `POST <lfs-url>/objects/batch` with a JSON body: the
  `operation` (`download` or `upload`), and a list of objects with `oid` and `size`
  `[lfs-batch]`.
- **The answer is a list of actions.** For each object the server returns an `href` to download
  from or upload to, the headers to send, and a lifetime. The address may be on another host,
  for example a cloud store `[lfs-batch]` `[lfs-basic]`.
- **Deduplication on upload.** "If a client requests to upload an object that the server already
  has, the server should omit the actions property completely. The client will then assume the
  server already has it" `[lfs-batch]`. The batch request is thus the question "which of these do
  you lack?" for a whole set.
- **Permission by reference.** The request may carry `ref: { name: <refspec> }`, added for
  "Git server authentication schemes that take the refspec into account": a user with write
  access to one branch only may upload objects for that branch `[lfs-batch]`.
- **Authentication.** HTTP Basic, with credentials from the Git credential helper. Over SSH the
  client runs `git-lfs-authenticate <repo> <operation>` on the server and receives a header with
  a token and its lifetime to use over HTTPS. A `401` should carry an `LFS-Authenticate` header
  `[lfs-auth]` `[lfs-batch]`.
- **Round trips** *(derived)*: one batch request, then one transfer per object, in parallel, and
  optionally a `verify` callback after an upload `[lfs-basic]`.
- **References.** Git LFS has none. Objects are reachable through pointer files in Git commits.

## 8. Comparison

Counts are HTTP requests without an authentication challenge. Only the Git smart HTTP counts for
clone and fetch are measured; all others are derived.

| System | Clone | Small fetch | Push |
|---|---|---|---|
| Git smart HTTP, v2 | 3 | 3 | 2 |
| Git dumb HTTP | 1 + objects or packs | 1 + objects | not in `[http]` |
| Jujutsu | as Git | as Git | as Git |
| Mercurial | 3 | 3 | 3 |
| Fossil | about 1 per MB | 2 | 1 to 2 |
| OCI registry | 1 + blobs | 1 + new blobs | 1 to 3 per blob + 1 |
| Git LFS | 1 + objects | 1 + new objects | 1 + new objects |

How the missing set is found, and by whom:

- **Git smart HTTP:** the client offers commits (`have`), the server answers which are common,
  then the server walks the graph and computes the set.
- **Git dumb HTTP:** the client walks the graph and requests each object it lacks.
- **Mercurial:** the client samples changesets, the server answers membership (`known`), the
  client computes the common set; the server then computes the bundle between `common` and
  `heads`.
- **Fossil:** both sides announce IDs (`igot`) and request IDs (`gimme`); clusters shorten the
  announcement. No side walks a graph.
- **OCI:** the manifest names all blobs; the client compares with what it holds, or asks `HEAD`
  per blob.
- **Git LFS:** the client sends a list of IDs; the server answers per ID.

Unit of transfer and deltas:

- **Git smart HTTP:** one pack per fetch or push; deltas between arbitrary objects; thin packs
  with bases outside the pack.
- **Git dumb HTTP:** stored files as they are (loose objects, whole packs).
- **Mercurial:** one bundle2; a changegroup is deltas by definition.
- **Fossil:** many artifacts per message of about 1 MB; an artifact may be a delta against
  another artifact.
- **OCI, Git LFS:** one object per request, whole, no delta.

What the server computes per request:

- **Git smart HTTP:** graph walk, object enumeration, delta selection or reuse, compression; on a
  push, indexing of the pack and a connectivity check.
- **Git dumb HTTP:** nothing; index files must be regenerated on every change.
- **Mercurial:** membership lookups; then the revisions between two sets, as deltas.
- **Fossil:** lookups by ID; sometimes a new cluster.
- **OCI, Git LFS:** lookup by ID; a hash on upload.

Authentication of an HTTP client:

- **Git, Jujutsu, Mercurial:** HTTP authentication in front of the protocol (Basic, bearer token,
  TLS client certificate); the protocol carries no identity.
- **Fossil:** a login card in the body: user name and a hash over the message and the password.
- **OCI (reference implementation):** bearer token from a separate token service, signed, with
  repository and actions as claims, short-lived.
- **Git LFS:** HTTP Basic, or a token handed out over SSH.

Moving a reference on the server:

- **Git:** the command `old new refname`; the server compares `old`; optional all-or-nothing
  (`atomic`); optional signed command list (push certificate); policy in hooks.
- **Jujutsu:** the same, always with an expected old value.
- **Mercurial:** `pushkey` with `old` and `new`, plus `check:*` parts for the heads, in one
  bundle with the data.
- **Fossil:** no reference update in the protocol.
- **OCI:** `PUT` of a manifest under a tag, unconditional unless the registry supports `ETag`
  conditions.
- **Git LFS:** none.

## 9. Observations for the decisions of phase 5

These are facts from the sections above, put next to the premises of the map (ADR-015: small
teams, a history of up to a few gigabytes, every clone holds the full object store). They are not
recommendations.

1. **Two families.** Either the server computes the difference from the graph and sends one
   stream (Git, Mercurial), or the server only answers lookups by ID and the client finds the
   difference (dumb HTTP, OCI, Git LFS; Fossil sits between). The first family needs 3 requests
   for any size of fetch and a server that understands commits and trees. The second needs a
   server that understands only IDs and bytes, at the price of requests that grow with the number
   of objects.
2. **The number of requests of the second family is bounded by batching, not by packs.** Git LFS
   asks for a whole set in one request. Fossil packs many artifacts into one message. Neither
   needs a delta format for that.
3. **Deltas and packs are separate questions.** Fossil sends deltas without a pack format;
   a Git bundle is a pack without negotiation. ADR-007 lists both under one open item.
4. **A pack without object IDs forces the receiver to hash everything.** That is the integrity
   check of Git at the same time. A format of single objects addressed by hash gives the same
   check per object (ADR-007, verified reads).
5. **A thin pack and a `REF_DELTA` assume that the receiver holds the base.** With the premise
   that every clone holds the full object store this holds for a fetch; for a push the server
   holds every base that a reference reaches.
6. **Every system that moves a reference over the network, except OCI, sends the expected old
   value.** `update_branch(name, expected, new, objects)` of ADR-008 has this shape already.
7. **Objects first, reference last.** Git quarantines the objects until the hooks have accepted
   the push; OCI uploads blobs before the manifest; Mercurial sends data and reference update in
   one bundle. In each case the reference never points at something the server does not hold
   completely.
8. **A signed reference update exists in Git only, as the push certificate.** It binds pusher,
   target URL, a server nonce and the triples (old, new, name). Git stores it as a blob and hands
   it to hooks; it keeps no log. Point 1 of the hand-over in ADR-003 (a signed record of ref
   updates on the hub) asks for the part that Git leaves to the hook.
9. **A stateless server and a nonce need a trick.** Git derives the nonce with an HMAC under a
   server secret and accepts a slop of seconds, because the nonce is issued in one HTTP request
   and returned in the next.
10. **No protocol read here carries the identity of the actor.** Identity comes from HTTP
    authentication or, in Fossil, from a login card. Authorisation per reference is a hook
    (Git), a list of user names (Mercurial), or a claim in a token (OCI). Git LFS added the
    reference name to the batch request so that the server can decide per branch.
11. **Offloading to static hosts is a later addition in every graph-walking system** (`bundle-uri`
    and `packfile-uris` in Git, clone bundles in Mercurial), and each names server load of clones
    as the reason. Content-addressed single objects are cacheable without such an addition,
    because a URL that contains the hash never changes its content.
12. **`ls-refs` with a prefix and `ref-in-want`** exist because a full reference list on every
    contact became too large, and because servers behind a load balancer disagreed about
    references for a moment. At the target scale one hub holds the references (ADR-015).

## 10. Open questions this note cannot answer

- How many objects a typical Stemma fetch and a clone of this repository contain, and therefore
  how many requests a protocol of single objects would need with and without a batch request.
  This needs a measurement on the `.stemma` of the repository, not literature.
- How much deltas would save for Stemma objects, which are stored uncompressed (ADR-007) and
  include whole trees. The systems above give no number that transfers.
- Whether the signature scheme of phase 4 (STEMMA-223) can sign a record of the shape of a push
  certificate, and which fields of it the operation log already covers.
- Whether the hub should check connectivity of a push (every object reachable from the new tip is
  present) and what that costs with loose objects; Git and OCI both check, at different depth.
- The push side of every system, measured. All push counts here are derived.
- The behaviour of real hosting services for authentication of agents (tokens per agent, scopes
  per reference); the sources read specify only the mechanism.
