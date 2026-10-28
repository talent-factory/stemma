# ADR-018: the machine-readable contract of the command output

**Status:** Proposal, decided by the author on 2026-10-27 in STEMMA-98. A change of an exit code
that scripts rely on is level 2 under ADR-004, so the change that implements it needs two agents
and a human maintainer.

## Context

A script or an agent that runs `stemma` needs to know which parts of the output it may parse. Today
the command line reference says only that `op log --json` and the line `anchor N:hash` of
`op verify` are meant for programs, and that the rest is "not yet decided".

The exit codes are partly fixed already: 0 (done), 1 (failed), 2 (wrong arguments), 3 (made, but
not finished), 4 (`diff --exit-code`: a file differs), and 129, 130, 143 (signals).
Code 1 is still overloaded. A held lock (retry later), nothing to commit, no repository, a
missing actor, a moved branch, and corruption all end with 1.
A caller can tell them apart only by the text of the message, which is written for people.

## Decision

- **Three things are a contract**: the JSON output (`--json`), the line
  `anchor N:hash`, and the exit codes. Everything else is for people and may change in any
  release: the text lines (`tracked <path>`, `Committed <id> on <branch>`, the status letters, the
  restore and purge lines), the messages after `stemma:`, and the order of the sections.
- **JSON carries a version**. Every JSON object has the field `schema`, a whole number that
  starts at 1. One number covers all objects, so a removed, renamed, or retyped field of any
  object raises it for all. Adding a field does not change it; a caller ignores fields it does
  not know. The command line reference names a change of `schema`. `op log --json` gets the
  field in the same change.
- **Two exit codes split code 1**, and 0, 2, 3, 4, and the signal codes keep their meaning. Code 4
  stays with `diff --exit-code`, where a script reads it as "a file differs"; a lock therefore
  takes 6.
  - **5:** the command found damage: the operation log has a gap or a wrong number, an entry or
    a reference file or the file `format` is not in canonical form, an object that a tree or an
    index names is missing or does not parse, the tip file is missing or behind, or the chain
    does not match an anchor (the entry is missing, from before the chain, or has another hash).
    `op log` ends with 5 and lists nothing when the references cannot be read because of
    damage, or when the entry before the oldest shown one is damaged. A caller does not retry;
    a person looks at the repository. The tip file is damage although a stopped writer can leave
    it behind, because a removed tip file could also hide a replaced last entry.
  - **6:** the command failed because a lock is held, or because the log writer lost every race
    for a number. A later call can succeed, so a caller waits and tries a few times. A lock of
    another machine, or one in an older form, is taken over by no command, so it stays; the
    message names the file that a person removes.
  - **1** stays for every other failure, among them nothing to commit, no repository, a missing
    actor, a moved branch, a commit that the caller or a pointer names and the store lacks, and
    most errors of `init` and of opening a repository. A commit that is missing stays at 1
    because the code cannot tell a wrong argument from damage. A caller that needs to tell
    these apart reads `--json` or the message.
  - Codes 1, 5, and 6 can leave the same traces: an entry for a move that did not happen, or
    the entry of a snapshot that came first. Nothing is lost.
  - Code 3 takes precedence when a result exists but is incomplete or not durable: a pointer
    moved only partly, a flush failed (also the flush of a snapshot that came first), or the
    output that alone names the result could not be written. A snapshot that came first and was
    followed by another failure does not make it 3; the failure keeps its own code. So does a
    `purge` whose journal stays, also when the journal cannot be read: the journal keeps every
    writer out until `purge --resume` or `--abort` has run. Code 3 does not say whether a lock
    or damage lay behind it; the message does.
  - A failure is classified by `stemma-core::failure`, which lists every variant of every error
    and has no fallback arm, so a new error cannot end with 1 unnoticed.
- **`--json` for `status` and `commit`**, the two commands that agents read most.
  `diff` and the other commands follow with the API of phase 6; until then they have no JSON.
  - `status --json` writes one object: `schema`, `clean` (boolean), `changes` (a list of
    `{kind, path}`), `untracked` (a list of paths), and `leftovers` (a list of paths). `kind` is
    one of `A`, `M`, `D`, and these letters are part of schema 1 although the text form may
    change. Paths are JSON strings, not escaped as the text form escapes them.
  - `commit --json` writes one object: `schema`, `commit`, `branch`, and `resumed` (boolean).
- **Errors stay on the standard error stream**. With `--json`, a failure writes no
  JSON to the standard output; the exit code and the message on the standard error stream say
  what happened. A command that has its result and could not flush or write it (code 3) writes
  its object first, as the text form writes its line first. Another code 3, such as a `commit`
  that moved one pointer, writes no object, because no result exists yet. An object followed by
  a non-zero code names what was made, not that the command worked; the exit code decides. A
  reader that closes the pipe early gets no error, as in the text form.

## Consequences

- The reference states the contract in the section "Aufruf und gemeinsame Regeln" and its
  paragraph "Maschinenlesbare Ausgabe", and the sections of the commands name the codes 5 and 6
  where they can occur.
- The text form can change without notice. Tests of the text keep their value as a check of what
  people read, but a script must not parse it.
- Code 5 and code 6 are new values. A script that treated every value other than 0 as a failure
  keeps working; one that matched on 1 sees the two cases move.

## Open Points

- [ ] Whether "nothing to commit" gets a code of its own. It is a normal result for a script that
      commits in a loop, but it needs no new code until a caller asks for it.
- [ ] `diff --json`, `restore --json`, and `purge --json` with the API and the MCP server of
      phase 6 (ADR-015).
- [ ] Whether a lock of another machine, or in an older form, which no command takes over, gets
      a code of its own. It is code 6 until a caller needs to tell it from a lock that clears.
