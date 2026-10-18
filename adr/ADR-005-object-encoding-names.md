# ADR-005: Object Encoding, Names, and Identity Characters

**Status:** Proposal, partially confirmed.

## Context

Object IDs are hashes of canonical encodings (ADR-001). Any change to an encoding changes the ID
of every affected object, so the rules for what may appear in an encoding have to be settled
before the first object is stored. This ADR covers the character rules for tree entry names and
actor fields. The implementation is `crates/stemma-core/src/object.rs`.

## Decision (proposal)

- **Actor fields** (`id`, `model`, `session`, `sponsor`) are non-empty printable ASCII
  (U+0021 to U+007E). They are identifiers, not display names. Whitespace, control characters,
  invisible characters, and look-alike characters such as a Cyrillic `а` are rejected, so an
  identity cannot be spoofed in the provenance chain of ADR-002. A display name, if needed,
  belongs in a separate field.
- **Tree entry names** are UTF-8 strings. They must not be empty, must not be `.` or `..`, and
  must not contain `/`, control characters (`char::is_control`), or invisible format characters
  (Unicode category Cf).
- **No normalization.** Names are compared and hashed as given. `é` in NFC and in NFD are two
  distinct names, and `README` and `readme` are two distinct names. Mapping them onto a
  file system is the job of the checkout layer.
- **Names are encoded as raw UTF-8 bytes.** A file whose name is not valid UTF-8 cannot be
  stored. The layer that reads files has to report that as an error and must not convert the
  name silently.
- **Sort order** of tree entries is UTF-8 byte order of the name. Directories get no special
  treatment, unlike in Git.
- **Parent and author rules.** A commit lists each parent at most once, and the order of the
  parents is part of its ID.
- **Golden vectors** in the test module pin the hash input and every encoding. A change to
  them is a format change and needs an ADR.

## Consequences

- A switch from UTF-8 strings to byte-string names later is compatible with existing data: a
  UTF-8 name encodes to the same bytes, so no tree ID changes. The opposite direction is not
  possible once a repository holds a name that is not valid UTF-8. This is why strings are the
  low-regret choice.
- Rejecting format characters also rejects the zero-width joiners U+200C and U+200D. Some
  scripts (for example Persian and several Indic scripts) and composite emoji such as 👨‍👩‍👧 need
  them, so file names in those scripts are rejected. See the open points.
- Actor identifiers cannot contain umlauts or other non-ASCII letters. An identifier derived
  from a person's name has to be transliterated.
- Names that differ only in case or normalization form can coexist in one tree. A checkout on a
  case-insensitive or normalizing file system has to detect the collision.
- Control and format characters are rejected in names for safety and are not needed for the
  line-based encoding. Only the newline is. A later relaxation changes no ID, because it only
  admits names that were not storable before.

## Open

- [ ] Allow U+200C and U+200D in tree names as an exception to the Cf rule?
- [ ] Reserve the name of Stemma's own metadata directory, as Git does for `.git`, including its
      case-insensitive and normalization variants.
- [ ] Add a timestamp to commits, or decide that it lives in the operation log. Without one, two
      commits with the same tree, parents, author, and message are the same object.
- [ ] Add a format version to the hashed data. Without one, a later change to a commit header
      leaves no way to tell which encoding an object uses.
- [ ] Type object IDs per kind (blob, tree, commit) once an object store exists, so that a
      directory entry cannot point at a blob and a commit cannot take a blob as its tree.
- [ ] Reject Windows device names and trailing dots or spaces in names, or leave it to the
      checkout layer.
