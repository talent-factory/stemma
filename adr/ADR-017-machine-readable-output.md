# ADR-017: the machine-readable contract of the command output

**Status:** Proposal, decided by the author on 2026-10-27 in STEMMA-98. A change of an exit code
that scripts rely on is level 2 under ADR-004, so the change that implements it needs two agents
and a human maintainer.

## Context

A script or an agent that runs `stemma` needs to know which parts of the output it may parse. Today
the command line reference says only that `op log --json` and the line `anchor N:hash` of
`op verify` are meant for programs, and that the rest is "not yet decided".

The exit codes are partly fixed already: 0 (done), 1 (failed), 2 (wrong arguments), 3 (made, but
not finished), 4 (`diff --exit-code`: a file differs), and 129, 130, 143 (signals). Code 1 is
still overloaded. A held lock (retry later),
nothing to commit, no repository, a missing actor, a moved branch, and corruption all end with 1.
A caller can tell them apart only by the text of the message, which is written for people.

## Decision

- **Three things are a contract**: the JSON output (`--json`), the line
  `anchor N:hash`, and the exit codes. Everything else is for people and may change in any
  release: the text lines (`tracked <path>`, `Committed <id> on <branch>`, the status letters, the
  restore and purge lines), the messages after `stemma:`, and the order of the sections.
- **JSON carries a version**. Every JSON object has the field `schema`, a whole
  number that starts at 1. One number covers all objects, so a removed, renamed, or retyped field
  of any object raises it for all. Adding a field does not change it; a caller ignores fields it does not
  know. Removing a field, renaming it, or changing its type raises `schema`, and the command line
  reference names the change. `op log --json` gets the field in the same change.
- **Two exit codes split code 1**, and 0, 2, 3, 4, and the signal codes keep their meaning. Code 4
  stays with `diff --exit-code`, where a script reads it as "a file differs"; a lock therefore
  takes 6.
  - **5:** the command found damage: the operation log has a gap or a wrong number, an entry or
    a reference file is not in canonical form, an object that a commit names is missing or does
    not parse, or the chain does not match an anchor (the entry is missing, from before the
    chain, or has another hash). A caller does not retry; a person looks at the repository.
  - **6:** the command failed because a lock is held, or because the log writer lost every race
    for a number. A later call can succeed, so a caller waits and retries. The call may have
    left the traces of code 1 (an entry for a move that did not happen, or the entry of the
    snapshot that came first), but nothing is lost.
  - **1** stays for every other failure, among them nothing to commit, no repository, a missing
    actor, a moved branch, and the errors of `init`. A caller that needs to tell these apart
    reads `--json` or the message.
  - Code 3 takes precedence: a command that moved a pointer ends with 3 whatever the cause was.
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
  what happened. A command that made something and could not finish (code 3) writes its object
  first, as the text form writes its line first.

## Consequences

- The reference states the contract in the sections "Aufruf und gemeinsame Regeln" and
  "Maschinenlesbare Ausgabe", and the sections of the commands name the codes 5 and 6 where they
  can occur.
- The text form can change without notice. Tests of the text keep their value as a check of what
  people read, but a script must not parse it.
- Code 5 and code 6 are new values. A script that treated every value other than 0 as a failure
  keeps working; one that matched on 1 sees the two cases move.

## Open Points

- [ ] Whether "nothing to commit" gets a code of its own. It is a normal result for a script that
      commits in a loop, but it needs no new code until a caller asks for it.
- [ ] `diff --json`, `restore --json`, and `purge --json` with the API and the MCP server of
      phase 6 (ADR-015).
- [ ] Whether a lock held by a crashed process (a stale lock) is code 6 or code 5. It is code 6
      until the core can tell the two apart.
