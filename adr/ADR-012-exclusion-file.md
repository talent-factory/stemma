# ADR-012: the exclusion file `.stemmaignore`

**Status:** Confirmed by the author on 2026-10-20 and implemented in STEMMA-24
(`crates/stemma-core/src/ignore.rs`, used by `Repository::track`). `Repository::status`
(STEMMA-4) uses it for the untracked files.

## Context

ADR-010 decides that `.stemmaignore` lists paths that are never tracked, not even by
`stemma track`, and that are not reported as untracked, and that its reader is strict like the
others. It leaves open the syntax, negation, whether the file is tracked itself, its tier in
ADR-004, and the interplay with `.stemma` and its aliases (ADR-005).

What the file protects is the history. A snapshot at every command (ADR-010) saves what is
tracked, and the operation log keeps every replaced commit (ADR-011), so a secret that enters by
mistake stays until a purge. The file therefore has to be easy to get right and hard to get
silently wrong. Git's `.gitignore` is prior art for the familiar parts (names, `*`, a trailing
`/`) and a warning for the rest (negation and the rule that a file cannot be re-included below an
excluded directory, many forms of escape and character class, nested files that override each
other).

## Decision

- **One file at the root of the repository directory** *(confirmed)*. The path is
  `.stemmaignore` next to `.stemma`. Nested files are not read; they can come later without
  changing the meaning of this one.
- **A small pattern language** *(confirmed)*. A line is a comment or a pattern.
  - A comment starts with `#`. A pattern cannot start with `#`; `/#name` writes such a name
    at the root, and `**/#name` at any depth.
  - A pattern is a list of parts separated by `/`. A part is a name with the wildcards `*` (any
    run of characters, also none, inside one name) and `?` (one character, a Unicode scalar), or
    the whole part `**` (any number of directories, also none, so `foo/**` excludes `foo`
    itself and what is below it, and `a/**/b` matches `a/b`). A part such as `a**b` is an
    ordinary name with two stars.
  - A leading `/` anchors the pattern at the root. A pattern with a `/` in the middle is anchored
    as well. A pattern with neither matches a name at any depth.
  - A trailing `/` matches only a directory. Without it, a pattern matches a file or a directory.
  - A match on a directory excludes everything below it.
  - Names are compared byte by byte (UTF-8) and case-sensitively. A file system that ignores
    case or normalizes Unicode does not change that: a pattern `.ENV` does not exclude `.env`,
    and a pattern in NFC does not exclude a name that the file system keeps in NFD, so such a
    file is tracked without a word. The spelling rule of `track` (ADR-010) makes a *named path*
    spell the names as they are on disk, but it does not change what the person wrote in the
    file (see the open items).
  - A trailing `/` matches a directory, and a link is no directory (a link is never followed),
    so `target/` does not exclude a link named `target`; `target` does.
  - There is no escape, no character class, and no negation. A pattern with `\`, `[`, `]`, or a
    leading `!` is rejected, so that these forms stay free for a later version and an existing
    file never changes its meaning. A name that contains `*` or `?` cannot be excluded by name
    (a wildcard pattern matches it, with the other names).
  - Because there is no negation, the order of the lines does not matter: a path is excluded if
    any pattern matches it or one of its parent directories.
- **A strict reader** *(confirmed)*. `IgnoreRules::parse` accepts only this form and says which
  line is wrong ("line *n*: …"), as the readers of the index, trees, and the log do. It checks
  the lines in order, so the error names the first line that it refuses, and a last line without
  a newline is reported after the lines before it. A problem of the whole file (size, a byte
  order mark, bytes that are not UTF-8) has no line and comes first:
  - UTF-8 without a byte order mark, no `\r`, no NUL, no other control character.
  - Every line ends with `\n`, also the last one. An empty file (no bytes) is valid.
  - No empty line, no space or tab at the start or end of a line, no repeated line.
  - Every part is non-empty (`a//b` is rejected), not `.` or `..`, and not a name that a tree
    refuses (ADR-005); a part `**` stands alone, and a pattern whose parts are all `**` is
    rejected as a mistake (it would exclude everything).
  - A literal part `.stemma` (a name that a tree refuses) or `.stemmaignore` (at any depth) is
    rejected: the first is always excluded, and the second at the root is never. A file of that
    name below the root can only be reached by a wildcard.
  - A line has at most 1,024 bytes and the file at most 64 KiB, as the entries of the log have
    a limit.
  - Matching is polynomial and never exponential: for each prefix of the path and each
    pattern, a table with one cell for each pair of a part of the pattern and a part of the path,
    and for each pair a match of two names that backtracks to the last `*` only. A file of the
    largest size cannot make a walk take unbounded time, but a very large file with deep paths
    is slow in proportion to its size.
- **The file is tracked** *(confirmed)*. It is a file of the repository like any other, it reaches
  every clone with the repository, and so every clone excludes the same paths. It is never
  excluded by its own patterns (a wildcard that matches it has no effect on it), and `track`
  confirms it like any other file.
- **Level 2 of ADR-004** *(confirmed)*. Removing or loosening a line can let a secret into the
  history, so a change needs two agents and a human maintainer. It is not level 3: it is no
  permission, no key, and no signature, and ADR-004 names "the policy file" (rule 5) for the
  rules of reviews and permissions, which this file is not. The list of paths per level
  (ADR-004, open point) gets `.stemmaignore` at level 2. ADR-004 is a proposal itself, and
  nothing enforces a level yet, so this decision takes effect with the review of ADR-004.
- **`.stemma` stays excluded by name** *(confirmed)*. A tree cannot hold a name that resolves to
  `.stemma` (ADR-005), and the file cannot lift that. The aliases of ADR-005 are the business of
  the name filter that exists, not of the patterns.
- **`.git` is always excluded by name** *(proposed)*. `track` and `status` leave a file or
  directory named `.git` out at every depth, without a line in `.stemmaignore` and without a way
  to lift that. It is the metadata of Git (also the link file of a worktree or a submodule), not a
  file of the project, and it changes with every Git command. Unlike `.stemma` it is no threat to
  the repository, so a tree may hold the name and ADR-005 stays as it is; the rule lives in the
  walk and not in the name filter. The comparison ignores the case of ASCII letters, as the file
  systems of macOS and Windows do. A path that the caller names and that has a part `.git` is an
  error, not a silent skip; no line is involved. What is in the tree already stays tracked, as
  for a pattern. The price is that a repository cannot version a `.git` on purpose.
- **Effect on commands** *(confirmed)*.
  - `track` skips an excluded file below a directory without a word, as it skips `.stemma`. A
    path that the caller names and that is excluded is an error that names the line of the
    pattern.
  - What is in the tree already stays tracked and is saved by every snapshot, whatever the file
    says. Otherwise a snapshot would drop a file from the history without a word. `status`
    (STEMMA-4) lists such a file with the changes; a report "tracked and excluded" is open.
  - `status` (STEMMA-4) does not list an excluded path as untracked.
  - A snapshot of tracked files does not read the file, so a damaged `.stemmaignore` never blocks
    a snapshot or a `commit`.
- **Where it is read** *(confirmed)*. From the working directory at every call that needs it, never
  from the tree: an edit takes effect at once and not after the next snapshot. A missing file
  means no patterns. A file that is not a regular file (a link, a directory, a pipe) or that the
  reader rejects is an error, and the command stops before it tracks or lists anything: a rule
  that cannot be read must not become "exclude nothing". The same holds for a file that is
  replaced, removed, or written while it is read: the path is inspected without following a
  link, then opened, the open file has to be the one that was inspected (same device and inode
  on Unix) and a regular file, and the bytes that were read have to be as many as the file has,
  so that a file cut or still being written is an error and not a shorter rule set. A call with
  no paths (a snapshot) does not read the file.

## Examples (golden vectors for the reader)

Accepted, each as a whole file:

| File | Excludes |
|---|---|
| `target/\n` | every directory `target`, at any depth, and what is below it |
| `/build\n` | the file or directory `build` at the root only |
| `*.log\n` | every name that ends in `.log`, at any depth |
| `docs/**/draft-?.md\n` | `docs/draft-1.md`, `docs/a/b/draft-x.md` |
| `# secrets\n.env\n` | every name `.env` |
| (no bytes) | nothing |

Rejected, with the line:

| File | Reason |
|---|---|
| `a\n\nb\n` | line 2: empty line |
| `a\na\n` | line 2: repeated line |
| `a \n` | line 1: space at the end |
| `a` | line 1: no `\n` at the end |
| `!keep\n` | line 1: negation is not supported |
| `a\\b\n`, `[ab]\n` | line 1: `\`, `[`, `]` are reserved |
| `a//b\n`, `./a\n`, `../a\n` | line 1: empty part, `.`, `..` |
| `**\n`, `**/**\n` | line 1: excludes everything |
| `.stemma\n`, `.stemmaignore\n` | line 1: always, or never, excluded |
| `a\r\n` | line 1: carriage return |

## Consequences

- A `track .` no longer takes secrets and build output once the file exists. Until then, and for
  files that are tracked already, nothing changes (ADR-010).
- A person who copies a `.gitignore` finds most lines work, and the lines that do not (negation,
  classes, escapes) fail at the first command with their line number and not by being ignored.
- The reader is a pure function on bytes, so it has golden vectors like the other readers, and a
  fuzzing target costs little.

## Open items

- [ ] Negation (`!`), if a use shows up. The reserved form lets it come without changing a file.
- [ ] Nested `.stemmaignore` files, and a file in a directory above the root.
- [ ] A command `stemma ignore PATTERN` that writes the file, which would also fix the form
      (sorted lines, for example) that a writer produces.
- [ ] The report "tracked and excluded" of `status`, and a way to untrack such a file.
- [ ] Whether the 1,024 and 64 KiB limits are right once real files exist.
- [ ] The names a pattern should reach on a file system that ignores case or normalizes Unicode
      (a pattern `Target/` and a directory `target`, a pattern in NFC and a name in NFD). Today a
      pattern compares bytes, so such a file is tracked without a word. A comparison after
      normalization and case folding, or a warning for a name that differs only so, needs its own
      decision and a dependency for the normalization.
- [ ] A file replaced by a pipe between the inspection and the open blocks the open. The std
      library has no way to open without blocking, so closing the window needs a call of the
      system (`O_NONBLOCK`) and with it a dependency or `unsafe`.
