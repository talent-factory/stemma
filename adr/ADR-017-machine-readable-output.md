# ADR-017: the machine-readable contract of the command output

**Status:** Proposal, decided by the author on 2026-10-27 in STEMMA-98. A change of an exit code
that scripts rely on is level 2 under ADR-004, so the change that implements it needs two agents
and a human maintainer.

## Context

A script or an agent that runs `stemma` needs to know which parts of the output it may parse. Today
the command line reference says only that `op log --json` and the line `anchor N:hash` of
`op verify` are meant for programs, and that the rest is "not yet decided".

The exit codes are partly fixed already: 0 (done), 1 (failed), 2 (wrong arguments), 3 (made, but
not finished), and 129, 130, 143 (signals). Code 1 is still overloaded. A held lock (retry later),
nothing to commit, no repository, a missing actor, a moved branch, and corruption all end with 1.
A caller can tell them apart only by the text of the message, which is written for people.

## Decision

- **Three things are a contract** *(proposed)*: the JSON output (`--json`), the line
  `anchor N:hash`, and the exit codes. Everything else is for people and may change in any
  release: the text lines (`tracked <path>`, `Committed <id> on <branch>`, the status letters, the
  restore and purge lines), the messages after `stemma:`, and the order of the sections.
- **JSON carries a version** *(proposed)*. Every JSON object has the field `schema`, a whole
  number that starts at 1. Adding a field does not change it; a caller ignores fields it does not
  know. Removing a field, renaming it, or changing its type raises `schema`, and the command line
  reference names the change. `op log --json` gets the field in the same change.
- **Two exit codes split code 1** *(proposed)*, and 0, 2, 3, and the signal codes keep their
  meaning:
  - **4:** the command failed because a lock is held. Nothing was changed by this call, and a
    later call can succeed. A caller waits and retries.
  - **5:** the command found damage: the operation log has a gap or a wrong number, an entry or
    a reference file is not in canonical form, an object that `diff` reads does not parse, or the
    chain does not match an anchor. A caller does not retry; a person looks at the repository.
  - **1** stays for every other failure, among them nothing to commit, no repository, a missing
    actor, and a moved branch. A caller that needs to tell these apart reads `--json` or the
    message.
- **`--json` for `status` and `commit`** *(proposed)*, the two commands that agents read most.
  `diff` and the other commands follow with the API of phase 6; until then they have no JSON.
  - `status --json` writes one object: `schema`, `clean` (boolean), `changes` (a list of
    `{kind, path}`), `untracked` (a list of paths), and `leftovers` (a list of paths). Paths are
    JSON strings, not escaped as the text form escapes them.
  - `commit --json` writes one object: `schema`, `commit`, `branch`, and `resumed` (boolean).
- **Errors stay on the standard error stream** *(proposed)*. With `--json`, a failure writes no
  JSON to the standard output; the exit code and the message on the standard error stream say
  what happened. A command that made something and could not finish (code 3) writes its object
  first, as the text form writes its line first.

## Consequences

- The reference states the contract in one paragraph after "Aufruf und gemeinsame Regeln", and
  each command section says which exit codes it can end with.
- The text form can change without notice. Tests of the text keep their value as a check of what
  people read, but a script must not parse it.
- Code 4 and code 5 are new values. A script that treated every value other than 0 as a failure
  keeps working; one that matched on 1 sees the two cases move.

## Open Points

- [ ] Whether "nothing to commit" gets a code of its own. It is a normal result for a script that
      commits in a loop, but it needs no new code until a caller asks for it.
- [ ] `diff --json`, `restore --json`, and `purge --json` with the API and the MCP server of
      phase 6 (ADR-015).
- [ ] Whether a lock held by a crashed process (a stale lock) is code 4 or code 5. It is code 4
      until the core can tell the two apart.
