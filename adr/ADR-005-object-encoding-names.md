# ADR-005: Object Encoding, Names, and Identity Characters

**Status:** Accepted. Decisions marked *(confirmed)* earlier keep their date; the rest were
confirmed by the author on 2026-10-24 (STEMMA-9). All are implemented in Phase 1.
The format of the index applies only if the index is persisted; in Phase 1 it is not (ADR-010).

## Context

Object IDs are hashes of canonical encodings (ADR-001). Any change to an encoding changes the ID of
every affected object, so the rules for what may appear in an encoding have to be settled before the
first object is stored. This ADR covers the rules for tree entry names, actor fields, commit
timestamps, change IDs, and format versions. The implementation is
`crates/stemma-core/src/object.rs`; together
with its golden-vector tests it is the normative definition of the byte-level encodings. The check
of the actor fields is in `crates/stemma-core/src/actor.rs`. This ADR records the decisions behind
them.

## Decision

- **Actor fields** *(confirmed)* (`id`, `model`, `session`, `sponsor`) are non-empty visible
  ASCII (U+0021 to U+007E, no space). They are identifiers, not display names. Whitespace,
  control characters, invisible characters, and letters from other scripts that look like ASCII
  letters, such as a Cyrillic `а`, are rejected, so an identity cannot be spoofed with them in
  the provenance chain of ADR-002. Look-alikes within ASCII, such as `l` and `I` or `O` and `0`,
  remain possible. A display name, if needed, belongs in a separate field.
- **Tree entry names** *(confirmed)* are UTF-8 strings. They must not be empty, must not consist
  only of dots and spaces (`.`, `..`, and names such as `...` or `. ` that Windows reduces to them),
  and must not contain `/`, control characters (`char::is_control`, plus the line and paragraph
  separators U+2028 and U+2029), or invisible characters. Invisible characters are the format
  characters (Unicode category Cf) and the default-ignorable code points (property
  `Default_Ignorable_Code_Point`), together. Examples are the zero-width space, the bidirectional
  overrides, the Hangul fillers, and the combining grapheme joiner. Variation selectors are
  invisible too, but allowed in one position (see below). The two sets overlap, and neither contains
  the other: Cf holds visible characters such as the Arabic number sign U+0600, and the
  default-ignorable set holds the variation selectors and the Hangul fillers. Both follow Unicode
  16.0. The Cf set comes from the `unicode-general-category` crate, which is pinned to an exact
  version, and the default-ignorable set is a table in `object.rs` built from
  `DerivedCoreProperties.txt` of Unicode 16.0, so that the same name is valid on every build. A new
  Unicode version needs a review of the table and of the names it newly rejects.
- **Variation selectors** *(confirmed)*. A variation selector is allowed directly after a
  non-ASCII character that is not a selector itself. Everywhere else it is invisible and
  rejected like the rest of the default-ignorable code points; see Consequences. Allowing it
  admits names that were not storable before and changes no ID.
- **Joiners** *(confirmed)*. The zero-width joiners U+200C and U+200D stay rejected, like the
  rest of Cf. Persian and other languages written in Arabic script, Indic scripts, and composite
  emoji need them, so names in those scripts cannot be stored for now. Two reasons: they are
  invisible, so `README` and `READ\u200dME` look alike in a listing, and HFS+ ignores U+200C to
  U+200F when it resolves names, so `.stem\u200cma` would be `.stemma` there. Relaxing a rule
  changes no ID, while withdrawing a permission would invalidate stored names, so the stricter
  rule stays until someone needs the names. The planned relaxation, if a need arises, is:
  U+200C and U+200D are allowed only between two non-ASCII characters, not at the start or the
  end of a name, and not next to another joiner. That keeps them out of ASCII names and so out
  of every alias of `.stemma`. It does not stop look-alikes inside non-ASCII Latin words, such
  as `é\u200dé`; the standard rule (RFC 5892, CONTEXTJ) would, at the cost of more Unicode data
  and without emoji.
- **Directional marks** *(confirmed)*. The left-to-right mark U+200E and the right-to-left mark
  U+200F stay rejected, like the rest of Cf. Names in Hebrew, Arabic, Persian, and Urdu that
  mix right-to-left letters with digits or brackets sometimes carry them, so such names cannot
  be stored for now. The reasons are the same as for the joiners: the marks are invisible, so
  `README` and `READ\u200eME` look alike, and HFS+ ignores U+200C to U+200F when it resolves
  names. They are weaker than the overrides, embeddings, and isolates (U+202A to U+202E and
  U+2066 to U+2069), which reorder text and stay rejected in any case. The planned relaxation,
  if a need arises, is: the marks are allowed only in a name that contains a strong
  right-to-left letter (Unicode bidirectional class R or AL). The rule on the neighbours of the
  joiners does not fit here, because the marks often stand next to ASCII characters, as in
  `שלום` followed by U+200F, a space, and `(1).txt`. It needs bidirectional class data, which
  means one more crate or a table. Two names that differ only by a mark still look alike within
  a right-to-left name.
- **Reserved name** *(confirmed)*. The metadata directory is `.stemma`. No tree entry may use
  that name or an alias of it on a common file system: the name in any letter case (including
  look-alikes such as the long s U+017F), with trailing dots or spaces (Windows drops them), or
  its 8.3 short name `STEMMA~N` for any number N. Two more forms are covered, because they
  reach the directory on Windows. The first is the stream syntax of NTFS: only the part of a
  name before its first `:` is the file name, so `.stemma::$INDEX_ALLOCATION`, which NTFS reads
  as the directory itself, and `.stemma:x` are reserved. The second is the backslash: Windows
  reads it as a path separator, so a name is reserved if any segment between backslashes is
  reserved, as in `x\..\.stemma\config`. Without these rules a checkout could overwrite
  repository data, as in CVE-2014-9390 for `.git`. Only these forms are reserved: `.stemma-notes`,
  `a.stemma`, `.stemma~1`, `a:.stemma`, `back\slash`, and `report:final.txt` are allowed. The
  constant is `METADATA_DIR`.
- **Checkout responsibility** *(confirmed)*. The tree format allows `\` and `:` in names,
  because they are valid on Linux and macOS and common in real file names; only the forms of the
  reserved name above are rejected. Where they are dangerous, on Windows, the checkout layer has
  to reject or escape them: `\` as a path separator, `:` for drive letters and NTFS alternate
  data streams, and device names such as `CON` and `NUL`. The checkout also has to detect names
  that collide on a case-insensitive or normalizing file system, and it must never write
  through a symbolic link.
- **No normalization** *(confirmed)*. Names are compared and hashed as given. `é` in NFC and in
  NFD are two distinct names, and `README` and `readme` are two distinct names. The
  reserved-name check is the only place that folds case.
- **Names are encoded as raw UTF-8 bytes** *(confirmed)*. A file whose name is not valid UTF-8
  cannot be stored. The layer that reads files has to report that as an error and must not
  convert the name silently.
- **Sort order** *(confirmed)* of tree entries is UTF-8 byte order of the name. Directories get no
  special treatment, unlike in Git, where a directory sorts as if its name ended in `/`. The
  position of an entry depends on its name only, so a file that becomes a directory keeps its place,
  and the order can be checked without the mode. The cost is that a depth-first walk through trees
  does not list the paths in the order of the path strings: `a/z` comes before `a-b`. The index
  therefore uses the same walk order (see below). An import from Git sorts anew, because the IDs are
  new anyway.
- **Commit timestamp** *(confirmed)*. A commit carries one timestamp, the time the author
  claims, as `time <seconds>` after the `author` header. The value is a decimal integer of
  seconds since the Unix epoch in UTC, without a plus sign, without leading zeros, and without a
  time zone. Negative values (before 1970) are allowed; `-0` is not canonical. The interface
  decides how to display it. The timestamp is self-reported and proves nothing; only signatures
  make it trustworthy (ADR-003, ADR-004). There is no separate committer. The operation log stays
  local and is not exchanged with the history (see the next decision), so the commit carries its
  own date. A rewrite, such as a rebase, a cherry-pick, or an amend, creates a new commit that
  keeps the author and the time of the original change. Who rewrote it, and when, is recorded in
  the local operation log and, for other readers, in the record of ref updates of a hub (ADR-003).
- **Change ID** *(confirmed)*. A commit carries one change ID as the header `change <id>`, between
  the parents and the author. It names the change that the commit is a state of and stays the same
  through every rewrite (ADR-010), so a review or a rebase can follow it. It is random, not derived
  from the content, so `Commit::new` takes it as an input and "the same inputs give the same ID"
  still holds. The form is `ch:` and 32 lowercase hexadecimal digits (128 bits); the prefix keeps it
  apart from an object ID (`b3:`). The rules of the strict reader apply: uppercase digits, another
  length, and a missing prefix are rejected. **This changes the format of `commit` in place**, which
  the rule below ("Format version") would answer with `commit2`. It is allowed once, because no
  repository holds a commit: `commit` is not yet written by any command, and the version of the
  code is 0.0.1. The golden vectors of the three commits were computed again for the new header.
  From the first commit that a command writes, the rule applies again without an exception.
- **Operation log** *(confirmed)*. The operation log is local. It is not an object of the repository
  and is not exchanged with the history. Three reasons: it keeps every earlier state reachable, so a
  secret that was committed by mistake could not be removed by rewriting; it holds local paths and
  discarded attempts that are nobody else's business; and the commit format stays as it is. The
  choice can be reversed in one direction only: an exchange can be added later as a new object kind
  without invalidating stored objects, but a log that was once exchanged cannot be withdrawn. For
  readers who do not have the local log, the answer to who rewrote a branch is the record of ref
  updates of a hub (ADR-003), not the commit. The format and the place of the log are decided in
  ADR-011.
- **Format version** *(confirmed)*. The encodings in this ADR are version 1. An incompatible
  change does not reuse an object kind name: it introduces a new one, for example `commit2`,
  together with an ADR, so that old and new objects can exist side by side. The header `change`
  of a commit (see "Change ID") is the one exception, made while no repository held a commit; it
  is part of version 1 and not a second version. The hash algorithm
  is versioned separately by the ID prefix (ADR-001).
- **Strict reading** *(confirmed)*. A reader rejects what it does not recognize: an unknown or
  repeated header, a header out of order, a number with a plus sign, `-0`, or leading zeros, or any
  other non-canonical form. Otherwise one logical object could have several encodings and several
  IDs. The commit headers are, in this order, `tree`, `parent` (zero or more), `change`, `author`,
  `time`. The index reader (see below), `Tree::parse`, and `Commit::parse` apply the rule, and the
  actor line of a commit is read by `Actor::from_str`. A test feeds changed copies of a canonical
  encoding to each of these three readers (every byte deleted, replaced, or inserted from a set of
  troublesome bytes, and every line deleted, repeated, or swapped) and checks that whatever a reader
  accepts encodes back to exactly the bytes it was given, so that no object has a second spelling
  and a second ID. It cannot find a second spelling that needs a change of several places at once;
  those are covered by tests of the rejected forms.
- **Index file** *(confirmed as a cache format)*. The working copy is a commit (ADR-010), so
  Phase 1 keeps no index file, and `Index` is a model of the tree in memory. The format below
  applies when the index is persisted, for example as a cache of the working copy, and is then a
  second format next to the objects. It is a file, not an object, so it is
  versioned by its first line, `stemma-index 1`, and not by an object kind name. An incompatible
  change raises the number, and a reader reports another number as an unsupported version and never
  reads it as version 1. Each following line is `<mode> <id> <path>` with one space between the
  fields: the mode is `100644`, `100755`, or `120000`, the ID is the textual object ID, and the path
  is everything after the second space. The index holds files and symbolic links only; the tree mode
  `040000` is an unknown mode in an index. Paths use `/` and satisfy the tree name rules for every
  component. The reader follows the strict-reading rule: it rejects anything the writer would not
  have written, including a missing final newline, `\r\n`, any separator other than one space, and
  paths that are not strictly ascending in the sort order below. A path cannot be both a file and a
  directory, and the check is byte-exact like all name comparisons, so `Foo` and `foo/bar` do not
  conflict. Reading does not check that the IDs name stored blobs, because the reader uses no store;
  `Index::verify` checks it against a store and reports the first failing path. The encoding is
  defined by `crates/stemma-core/src/index.rs` and its golden-vector test.
- **Index sort order** *(confirmed)*. Index entries are sorted component by component: the path is
  split at `/`, the components are compared in UTF-8 byte order, and a path that is a prefix of
  another comes first. `a/z`, `a-b`, and `a.b` appear in this order, because the component `a` sorts
  before `a-b`. This is the order of a depth-first walk through trees, so an index and a tree can be
  compared in one pass and a tree builder can read the entries in order. The sort of the whole path
  string would put `a-b` and `a.b` before `a/z` and is not used.
- **Parent rule** *(confirmed)*. A commit lists each parent at most once, and the order of the
  parents is part of its ID.
- **Golden vectors** *(confirmed)* in the test module pin the hash input and every encoding. A
  change to them is a format change and needs an ADR.

## Consequences

- Two commits with the same tree, parents, change ID, author, message, and time have the same ID,
  and a different time or a different change ID gives a different ID.
  A rebase changes the parents and so every ID; only
  re-creating a commit from identical inputs reproduces its ID.
- A timestamp gives a reader a date for foreign history without the operation log, but it can be
  set to any value. Nothing in the core may depend on its accuracy or its order.
- Adding a header later, such as `committer`, is a format change and needs a new kind name.
- A switch from UTF-8 strings to byte-string names later is compatible with existing data: a
  UTF-8 name encodes to the same bytes, so no tree ID changes. The opposite direction is not
  possible once a repository holds a name that is not valid UTF-8.
- Rejecting invisible characters also rejects the zero-width joiners U+200C and U+200D and the
  directional marks U+200E and U+200F; both are confirmed decisions, see above. Some
  scripts (for example Persian and several Indic
  scripts) and composite emoji such as 👨‍👩‍👧 need them,
  so file names in those scripts are
  rejected.
- The variation selectors (U+FE00 to U+FE0F, U+E0100 to U+E01EF, and the Mongolian free variation
  selectors U+180B to U+180D and U+180F) are default-ignorable, but a name may contain one directly
  after a non-ASCII character that is not itself a selector. So `⚠️ TODO.md`, `❤️`, and the
  ideographic variation sequences of Japanese names can be stored. A selector at the start of a
  name, after an ASCII character, or after another selector is rejected, so `READ\uFE0FME` stays
  rejected and no selector can occur in an ASCII name or an alias of `.stemma`. The rule checks the
  neighbour only and does not check that Unicode defines the sequence, so `é\uFE0F` is accepted and
  looks like `é`, like the other look-alikes below. The keycap emoji, such as `1️⃣`,
  start with an
  ASCII digit and stay rejected. The Mongolian vowel separator U+180E is not a variation selector
  and stays rejected.
- The rule does not cover look-alikes made of visible characters: letters from other scripts
  that look like ASCII letters, the braille blank U+2800, the no-break space and other space
  characters, and look-alikes within ASCII such as `l` and `I`. Names are not normalized.
- The reserved-name rule cannot list every alias. Windows derives short names from a hash when
  many similar names exist, and other file systems have their own rules. The checkout stays
  responsible for the file system it writes to.
- Actor identifiers cannot contain umlauts or other non-ASCII letters. An identifier derived
  from a person's name has to be transliterated.
- Names that differ only in case or normalization form can coexist in one tree. Windows aliases
  other than those listed in the reserved-name rule are the checkout's job.
- Control and invisible characters are rejected in names as a hygiene rule. The line-based
  encoding needs only the absence of the newline. Relaxing a rule later changes no ID, because
  it only admits names that were not storable before. Tightening a rule makes stored names
  invalid, so the rules should be settled before the first store.

## Open

- [x] Add a committer and a second timestamp, if rebasing and cherry-picking need to keep the
      original author time apart from the time of the rewrite. Decided not to add them: the
      operation log is local (see the decision above), a rewrite keeps the author and the time, and
      who rewrote it is answered by the local log and the ref updates of a hub. Revisit if
      repositories are exchanged peer to peer without a hub (ADR-003, open). Then `committer` and a
      second time are added as one change under a new kind name (`commit2`), because the committer
      is a second actor.
- [x] Decide whether the operation log is exchanged with the history. Decided: it is not (see the
      decision above).
- [x] The store has to record the object kind, because the version is part of the kind name.
      Done in ADR-007: a stored object starts with its kind name.
- [x] Type object IDs per kind (blob, tree, commit) once an object store exists, so that a
      directory entry cannot point at a blob and a commit cannot take a blob as its tree.
      Done: `BlobId`, `TreeId`, and `CommitId` wrap `ObjectId`. `TreeEntry::file` takes a `BlobId`,
      `TreeEntry::directory` a `TreeId`, a commit takes a `TreeId` and `CommitId` parents, and an
      index entry holds a `BlobId`. The types only help while code is written: the kind is hashed
      into the ID but not recorded in it, so an ID read from text or from a store is untyped until
      the code knows its kind. No encoding and no ID changes.
- [x] Check the kind when a typed ID is resolved from a store, so that an ID read from text cannot
      be claimed as the wrong kind unnoticed. Available in ADR-007: `resolve_blob`, `resolve_tree`,
      and `resolve_commit` return a typed ID or a `KindMismatch`. The check is available, not
      enforced: a caller has to use it where it relies on a kind, as the next item does for the
      index.
- [x] The index reader claims every ID as a blob without asking a store, as the readers of trees and
      commits claim the kinds named by headers and modes. Done for the index: `Index::verify` takes
      an object store and checks every entry with `resolve_blob`. It reports an ID that the store
      does not have, an object of another kind, a damaged object, or a storage failure, with the
      path of the first entry that fails. It covers a parsed index and an entry staged with the ID
      of a blob that was never stored. A builder of trees from the index has to call it first. The
      check reads every blob completely; a lookup of the kind from the object header alone, or a
      read that returns the body together with the typed ID, would avoid that at the price of a
      weaker check or a second API. The same kind of check is still needed for what `Tree::parse`
      and `Commit::parse` return: nothing calls `resolve_*` on those IDs yet.
- [x] Validate `Actor` in its own type, so that other consumers, such as the authorization
      hook, never see an actor that a commit would reject. Done: `Actor` has private fields and
      is built by `Actor::human` and `Actor::agent`, which check every field and return an
      `ActorError`. `Actor::view` gives an `ActorRef` to match on. `Commit::new` no longer
      checks the author, and `CommitError::InvalidActorField` is gone. No encoding and no ID
      changes.
- [x] Readers for trees and commits, with the strict reading rule above. Done: `Tree::parse` and
      `Commit::parse` read the body of an object from bytes and reject everything that is not
      canonical, for example a header that is missing, repeated, unknown, or out of order, a number
      with a plus sign, `-0`, a leading zero, or a value outside `i64`, names that are not strictly
      ascending, a body that is not UTF-8, a missing final newline or empty line, and a parent that
      is listed twice. The body is checked as a whole (UTF-8, and for a tree the final newline)
      before the lines are read in order. They claim the kind of each ID from the mode or the
      header, and their documentation says that `resolve_*` has to confirm it.
- [x] Read an object from a store and return the decoded value. Done: `ObjectStore::get_tree` and
      `get_commit` take a typed ID, confirm the kind, and read the body with the strict reader.
      `ReadError` combines the errors: `Store` (transparent, boxed), `Tree` or `Commit` with the ID
      and the reason from the reader, and `IdMismatch` if a reader returned a value with another ID
      than the one requested (checked in every build). `Tree::verify` and `Commit::verify` (module
      `verify`)
      check the claims of a decoded object against a store, one level deep: a directory entry and
      the tree of a commit must be trees, the other entries blobs, and the parents commits. Every
      referenced object is read and hashed, so the cost grows with its size.
- [x] A commit reader needs to rebuild an `Actor` from the author header. Done: `Actor::from_str` in
      `actor.rs` splits at single spaces, requires exact token counts and the keys in order, splits
      each `key=value` token at its first `=`, and checks the fields again with `Actor::human` and
      `Actor::agent`. A malformed line (`ActorParseError::Malformed`) is reported apart from a
      rejected field (`ActorParseError::Invalid`).
- [x] Clean up the actor type. Done: the check and the writing of the actor line work from one
      list (`Repr::fields`; the reader, `Actor::agent`, and `ActorRef` remain separate lists),
      `ActorError::InvalidField` carries an `ActorField` instead of a string, and `Actor` has `Hash`
      and `Display`; the commit encoding uses the `Display` form, which `FromStr` reads back.
      `ActorRef` stays as the public type to match on, because a borrowed view with public fields
      cannot come from the same list without indexing. No encoding and no ID changes.
- [x] A tree builder from the index (`write-tree`) should verify inside. Done:
      `Index::write_tree(&store)` calls `Index::verify` itself, writes the trees from the innermost
      directory outwards, and returns the root `TreeId`. No tree is written if the check fails.
      `WriteTreeError` separates a failed check, a store that cannot write, and a tree that cannot
      be built (ruled out by the index rules). The work is a loop over a stack of open directories
      and not a recursion. The blobs are not read again after the check, so a change of the store
      after it goes unnoticed.
- [x] Property tests of the readers and encoders (`properties.rs`). Done: for every tree, commit,
      index, and actor line that the code can build, `parse(encode(x))` gives `x` back; for every
      input that a reader accepts, encoding the result gives the input, which is the rule above
      checked on generated input. The inputs are valid encodings changed in several places at
      once (whole tokens replaced by numbers, signs, and near-miss modes, characters added at the
      ends of lines, separators doubled, tokens swapped, whole lines inserted), splices of two
      encodings, pieces of the format glued together, and raw bytes. Every rejection must have a
      message, a cause chain that ends, and, where it names a line, a line inside the input (the
      commit reader reports a missing header at the line after the last). The seed is fixed, so a
      failure is reproducible; `STEMMA_PROPERTY_SEED` and `STEMMA_PROPERTY_CASES` explore further.
      A round trip cannot see a rule that the encoder and the reader share, such as a name rule,
      because both agree on it. A separate list of forbidden names and actor fields, written apart
      from the rules, has to be rejected by every entry point. Since STEMMA-20 the same properties
      cover the entries of the operation log in both encodings (a change; a stub and a `purge`
      entry, ADR-011 and ADR-014) and the journal of a purge (`purge/journal.rs`, whose reader
      returns no error to check, only the round trip and the single spelling).
- [x] Fuzz the strict readers (STEMMA-20). Done: the generator of the property tests does not see
      which branches of a reader it reaches, so `fuzz/` adds six `cargo fuzz` targets that follow
      the coverage: `tree`, `commit`, `actor`, `index`, `entry` (both encodings), and `journal`.
      Each checks the rule above for one input: it is rejected, with a message and a chain of
      causes that ends (except the journal, whose reader returns `None`), or it is the encoding
      of what was read. The actor target skips input that is not UTF-8, as the actor line is
      read from text. The readers of HEAD and of a branch file have no target; their grammar is a
      single line, and the property tests cover them. The checks are in
      `stemma_core::fuzz`, which exists only under `cfg(fuzzing)`, so the interface does not
      change. libFuzzer needs nightly, so the targets are a workspace of their own and stay out of
      `just ci`; `just fuzz TARGET [SECONDS]` and `just fuzz-all [SECONDS]` run them from the
      valid encodings in `fuzz/seeds/`, and the manual CI workflow has a job that runs each for a
      minute. The property tests stay the check of every commit. The new property test of the
      journal found that a few bytes (`stubs 1-999999999999`) made the reader expand a trillion
      numbers; the reader now refuses stubs after `last` and more than 2^24 of them (ADR-014).
- [x] Give the reasons of the parse errors a type. Done: `Malformed` in `TreeParseError`,
      `CommitParseError`, `IndexParseError`, and `ActorParseError` carries an enum instead of a
      `&'static str`: `TreeMalformed`, `CommitMalformed`, `IndexMalformed`, and `ActorMalformed`.
      A caller can match on the reason; the `Display` form of each variant is the message that
      the string had, and the tests pin it. The enums describe the shape of the lines. A value
      that is rejected has its own variant, as for the object IDs and the author: the time of a
      commit is `CommitParseError::InvalidTime` with a `TimeError` (`NotCanonical`,
      `OutOfRange`). A cut-off last line is `CutOff` in the tree, commit, and index readers, also
      where a commit expects its empty line, and not the header or line that it should have been;
      the actor line has no lines. The enums are `#[non_exhaustive]`, so a new reason is not a
      breaking change.
- [x] One convention for the chain of causes. Done: the message of an error is complete and has
      the message of its cause, so `to_string()` is enough for a log, a command line, or an agent;
      `Error::source` returns the cause of the cause and never the cause itself, so that a program
      that walks the chain shows each message once. The cause stays available as a typed field of
      the variant. `StoreError::Io` now has the message of the I/O error. The usual split (each
      error prints its own text, a reporter prints the chain) was not chosen: a complete message
      is what tests, logs, and agents read, and no code has to know how to print a chain. The
      price is that a walk of `source()` no longer leads to the typed cause, so
      `downcast_ref::<io::Error>()` on a source finds nothing. `StoreError::io_error()` and
      `store_error()` on `ReadError`, `IndexVerifyError`, `WriteTreeError`, `TreeVerifyError`, and
      `CommitVerifyError` reach it, and a program that only reports an error prints it with `{}`.
      The rule is in the crate documentation. `properties.rs` checks every rejection of the
      readers, and builds the wrapping errors from a failure that has a cause of its own, the same
      on every system (`FaultyStore`; the same failure of a real file system has another error
      kind on Windows than on Unix): the message ends in the message of the I/O error once, and
      the chain goes on with the link below it.
- [x] One type for the error of a check against a store. Done: `IndexVerifyError`,
      `TreeVerifyError`, and `CommitVerifyError` had the same two cases, the store has no such
      object and the store reports an error. They are now one generic `VerifyError<L>` (module
      `verify`) with `Missing { at, id }` and `Store { at, source }`, and the three names are
      aliases for the place: `EntryPath` for the path of an index entry, `EntryName` for the name
      of a tree entry, and `CommitReference` for the tree or a parent of a commit. The place has
      its own type each, so the path of an index entry and the name of a tree entry cannot be
      mixed up, and `WriteTreeError::Verify` holds only the error of an index. `L` only has to
      be `Debug + Display`: the `Display` form of the place starts the message (a path and a name
      in escaped backticks), so the messages are what they were. `VerifyError::check` turns the
      answer of a store into the result, once for all three checks, and makes the place only when
      a claim fails; `store_error()` and `at()` are defined once. `IndexVerifyError::Missing`
      names the ID as an `ObjectId` like the others, and the field is `at` instead of `path`,
      `name`, and `reference`.
