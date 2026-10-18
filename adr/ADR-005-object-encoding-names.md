# ADR-005: Object Encoding, Names, and Identity Characters

**Status:** Proposal, partially confirmed. Decisions marked *(confirmed)* were agreed; the others
are proposals.

## Context

Object IDs are hashes of canonical encodings (ADR-001). Any change to an encoding changes the ID
of every affected object, so the rules for what may appear in an encoding have to be settled
before the first object is stored. This ADR covers the rules for tree entry names, actor fields,
commit timestamps, and format versions. The implementation is
`crates/stemma-core/src/object.rs`; together with its golden-vector tests it is the normative
definition of the byte-level encodings. This ADR records the decisions behind them.

## Decision (proposal)

- **Actor fields** *(confirmed)* (`id`, `model`, `session`, `sponsor`) are non-empty visible
  ASCII (U+0021 to U+007E, no space). They are identifiers, not display names. Whitespace,
  control characters, invisible characters, and letters from other scripts that look like ASCII
  letters, such as a Cyrillic `а`, are rejected, so an identity cannot be spoofed with them in
  the provenance chain of ADR-002. Look-alikes within ASCII, such as `l` and `I` or `O` and `0`,
  remain possible. A display name, if needed, belongs in a separate field.
- **Tree entry names** *(confirmed)* are UTF-8 strings. They must not be empty, must not consist
  only of dots and spaces (`.`, `..`, and names such as `...` or `. ` that Windows reduces to
  them), and must not contain `/`, control characters (`char::is_control`), or format
  characters (Unicode category Cf). Most Cf characters are invisible, such as the zero-width
  space and the bidirectional overrides. The set is defined by the Unicode version of the
  `unicode-general-category` crate, which is pinned to an exact version (Unicode 16.0 for 1.1.0)
  so that the same name is valid on every build. Raising the version needs a review of the
  names it newly rejects.
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
- **Reserved name** *(confirmed)*. The metadata directory is `.stemma`. No tree entry may use
  that name or an alias of it on a common file system: the name in any letter case (including
  look-alikes such as the long s U+017F), with trailing dots or spaces (Windows drops them), or
  its 8.3 short name `STEMMA~N` for any number N. Without this rule a checkout could overwrite
  repository data, as in CVE-2014-9390 for `.git`. Only the name and these aliases are reserved:
  `.stemma-notes`, `a.stemma`, and `.stemma~1` are allowed. The constant is `METADATA_DIR`.
- **Checkout responsibility** *(confirmed)*. The tree format allows `\` and `:` in names,
  because they are valid on Linux and macOS and common in real file names. Where they are
  dangerous, on Windows, the checkout layer has to reject or escape them: `\` as a path
  separator, `:` for drive letters and NTFS alternate data streams
  (`.stemma::$INDEX_ALLOCATION`), and device names such as `CON` and `NUL`. The checkout also
  has to detect names that collide on a case-insensitive or normalizing file system.
- **No normalization** *(confirmed)*. Names are compared and hashed as given. `é` in NFC and in
  NFD are two distinct names, and `README` and `readme` are two distinct names. The
  reserved-name check is the only place that folds case.
- **Names are encoded as raw UTF-8 bytes** *(confirmed)*. A file whose name is not valid UTF-8
  cannot be stored. The layer that reads files has to report that as an error and must not
  convert the name silently.
- **Sort order** of tree entries is UTF-8 byte order of the name. Directories get no special
  treatment, unlike in Git, where a directory sorts as if its name ended in `/`.
- **Commit timestamp** *(confirmed)*. A commit carries one timestamp, the time the author
  claims, as `time <seconds>` after the `author` header. The value is a decimal integer of
  seconds since the Unix epoch in UTC, without a plus sign, without leading zeros, and without a
  time zone. Negative values (before 1970) are allowed; `-0` is not canonical. The interface
  decides how to display it. The timestamp is self-reported and proves nothing; only signatures
  make it trustworthy (ADR-003, ADR-004). There is no separate committer. This rule assumes that
  the operation log stays local and is not exchanged with the history, so the commit has to
  carry its own date (see Open).
- **Format version** *(confirmed)*. The encodings in this ADR are version 1. An incompatible
  change does not reuse an object kind name: it introduces a new one, for example `commit2`,
  together with an ADR, so that old and new objects can exist side by side. The hash algorithm
  is versioned separately by the ID prefix (ADR-001).
- **Strict reading.** A reader rejects what it does not recognize: an unknown or repeated
  header, a header out of order, a number with a plus sign, `-0`, or leading zeros, or any other
  non-canonical form. Otherwise one logical object could have several encodings and several
  IDs. The commit headers are, in this order, `tree`, `parent` (zero or more), `author`, `time`.
  The index reader (see below) applies the rule. The reader for objects does not exist yet;
  `object.rs` only writes encodings.
- **Index file** *(proposal)*. The index (staging area) is persisted as a text file and is a
  second format next to the objects. It is a file, not an object, so it is versioned by its
  first line, `stemma-index 1`, and not by an object kind name. An incompatible change raises
  the number, and a reader reports another number as an unsupported version and never reads it
  as version 1. Each following line is `<mode> <id> <path>` with one space between the fields:
  the mode is `100644`, `100755`, or `120000`, the ID is the textual object ID, and the path is
  everything after the second space. The index holds files and symbolic links only; the tree
  mode `040000` is an unknown mode in an index. Paths use `/` and satisfy the tree name rules
  for every component. Entries are sorted by the whole path string in UTF-8 byte order, which
  is not the order of a tree: `a-b`, `a.b`, and `a/z` appear in this order, while a tree orders
  only the names of one directory. A tree builder has to group the entries by directory. The
  reader follows the strict-reading rule: it rejects anything the writer would not have
  written, including a missing final newline, `\r\n`, any separator other than one space, and
  paths that are not strictly ascending. A path cannot be both a file and a directory, and the
  check is byte-exact like all name comparisons, so `Foo` and `foo/bar` do not conflict. The
  encoding is defined by `crates/stemma-core/src/index.rs` and its golden-vector test.
- **Parent rule.** A commit lists each parent at most once, and the order of the parents is part
  of its ID.
- **Golden vectors** in the test module pin the hash input and every encoding. A change to
  them is a format change and needs an ADR.

## Consequences

- Two commits with the same tree, parents, author, message, and time have the same ID, and a
  different time gives a different ID. A rebase changes the parents and so every ID; only
  re-creating a commit from identical inputs reproduces its ID.
- A timestamp gives a reader a date for foreign history without the operation log, but it can be
  set to any value. Nothing in the core may depend on its accuracy or its order.
- Adding a header later, such as `committer`, is a format change and needs a new kind name.
- A switch from UTF-8 strings to byte-string names later is compatible with existing data: a
  UTF-8 name encodes to the same bytes, so no tree ID changes. The opposite direction is not
  possible once a repository holds a name that is not valid UTF-8.
- Rejecting format characters also rejects the zero-width joiners U+200C and U+200D and the
  directional marks U+200E and U+200F; the joiners are a confirmed decision, see above. Some
  scripts (for example Persian and several Indic
  scripts) and composite emoji such as 👨‍👩‍👧 need them, so file names in those scripts are
  rejected. See Open.
- The Cf rule does not cover every invisible character. The Hangul fillers (U+115F, U+3164),
  the combining grapheme joiner (U+034F), variation selectors, and the line and paragraph
  separators (U+2028, U+2029) are accepted in names.
- Actor identifiers cannot contain umlauts or other non-ASCII letters. An identifier derived
  from a person's name has to be transliterated.
- Names that differ only in case or normalization form can coexist in one tree. Windows aliases
  other than those listed in the reserved-name rule are the checkout's job.
- Control and format characters are rejected in names as a hygiene rule. The line-based
  encoding needs only the absence of the newline. Relaxing a rule later changes no ID, because
  it only admits names that were not storable before. Tightening a rule makes stored names
  invalid, so the rules should be settled before the first store.

## Open

- [ ] Allow the directional marks U+200E and U+200F in tree names as an exception to the Cf
      rule? Right-to-left file names use them.
- [ ] Replace the Cf rule with the Unicode property `Default_Ignorable_Code_Point`, which fits
      the goal that names cannot hide their content better, and reject U+2028 and U+2029?
- [ ] Reject further aliases of `.stemma`, such as NTFS alternate data streams and HFS+
      ignorable code points, in the tree, or leave them to the checkout layer. The HFS+
      ignorable code points of Git's list are Cf characters and already rejected.
- [ ] Add a committer and a second timestamp, if rebasing and cherry-picking need to keep the
      original author time apart from the time of the rewrite.
- [ ] Decide whether the operation log is exchanged with the history. The timestamp rule
      assumes it is not.
- [ ] The store has to record the object kind, because the version is part of the kind name.
- [ ] Type object IDs per kind (blob, tree, commit) once an object store exists, so that a
      directory entry cannot point at a blob and a commit cannot take a blob as its tree.
- [ ] Validate `Actor` in its own type, so that other consumers, such as the authorization
      hook, never see an actor that a commit would reject.
