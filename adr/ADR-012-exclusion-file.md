# ADR-012: the exclusion file `.stemmaignore`

**Status:** Proposal, open confirmation. Nothing here is implemented; STEMMA-24 builds the reader
once the author has confirmed the decisions.

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

- **One file at the root of the repository directory** *(proposal)*. The path is
  `.stemmaignore` next to `.stemma`. Nested files are not read; they can come later without
  changing the meaning of this one.
- **A small pattern language** *(proposal)*. A line is a comment or a pattern.
  - A comment starts with `#`. A pattern cannot start with `#`; `/#name` writes such a name.
  - A pattern is a list of parts separated by `/`. A part is a name with the wildcards `*` (any
    run of characters, also none, inside one name) and `?` (one character, a Unicode scalar), or
    the whole part `**` (any number of directories, also none).
  - A leading `/` anchors the pattern at the root. A pattern with a `/` in the middle is anchored
    as well. A pattern with neither matches a name at any depth.
  - A trailing `/` matches only a directory. Without it, a pattern matches a file or a directory.
  - A match on a directory excludes everything below it.
  - Names are compared byte by byte (UTF-8) and case-sensitively. A file system that ignores
    case does not change that; the spelling rule of `track` (ADR-010) already makes a path spell
    the name as it is on disk.
  - There is no escape, no character class, and no negation. A pattern with `\`, `[`, `]`, or a
    leading `!` is rejected, so that these forms stay free for a later version and an existing
    file never changes its meaning. A name that contains `*` or `?` cannot be excluded by name
    (a wildcard pattern matches it, with the other names).
  - Because there is no negation, the order of the lines does not matter: a path is excluded if
    any pattern matches it or one of its parent directories.
- **A strict reader** *(proposal)*. `IgnoreRules::parse` accepts only this form and says which
  line is wrong ("line *n*: …"), as the readers of the index, trees, and the log do:
  - UTF-8 without a byte order mark, no `\r`, no NUL, no other control character.
  - Every line ends with `\n`, also the last one. An empty file (no bytes) is valid.
  - No empty line, no space or tab at the start or end of a line, no repeated line.
  - Every part is non-empty (`a//b` is rejected), not `.` or `..`, and not a name that a tree
    refuses (ADR-005); a part `**` stands alone, and a pattern whose parts are all `**` is
    rejected as a mistake (it would exclude everything).
  - A literal part `.stemma` or `.stemmaignore` is rejected: the first is always excluded, and the
    second is never.
  - A line has at most 1,024 bytes and the file at most 64 KiB, as the entries of the log have
    a limit.
  - Matching is done part by part, from the left, and takes time linear in the length of the
    path and the pattern for each pair (no backtracking that grows with the number of `**` or
    `*`).
- **The file is tracked** *(proposal)*. It is a file of the repository like any other, it reaches
  every clone with the repository, and so every clone excludes the same paths. It is never
  excluded by its own patterns (a wildcard that matches it has no effect on it), and `track`
  confirms it like any other file.
- **Tier 2 of ADR-004** *(proposal)*. Removing or loosening a line can let a secret into the
  history, so a change needs two agents and a human maintainer. It is not tier 3: it is no
  permission, no key, and no signature. The list of paths per tier (ADR-004, open point) gets
  `.stemmaignore` at tier 2.
- **`.stemma` stays excluded by name** *(proposal)*. A tree cannot hold a name that resolves to
  `.stemma` (ADR-005), and the file cannot lift that. The aliases of ADR-005 are the business of
  the name filter that exists, not of the patterns.
- **Effect on commands** *(proposal)*.
  - `track` skips an excluded file below a directory without a word, as it skips `.stemma`. A
    path that the caller names and that is excluded is an error that names the line of the
    pattern.
  - What is in the tree already stays tracked and is saved by every snapshot, whatever the file
    says. Otherwise a snapshot would drop a file from the history without a word. `stemma status`
    (STEMMA-4) can report such a file as tracked and excluded.
  - `status` does not list an excluded path as untracked.
  - A snapshot of tracked files does not read the file, so a damaged `.stemmaignore` never blocks
    a snapshot or a `commit`.
- **Where it is read** *(proposal)*. From the working directory at every call that needs it, never
  from the tree: an edit takes effect at once and not after the next snapshot. A missing file
  means no patterns. A file that is not a regular file (a link, a directory) or that the reader
  rejects is an error, and the command stops before it tracks or lists anything: a rule that
  cannot be read must not become "exclude nothing".

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
- [ ] The names a pattern should reach on a file system that ignores case (a pattern `Target/`
      and a directory `target`).
