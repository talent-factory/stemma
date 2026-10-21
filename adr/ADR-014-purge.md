# ADR-014: Removing a secret from the history and the operation log

**Status:** Proposal, partially confirmed. The decisions marked *(confirmed)* were agreed with the
author on 2026-10-21 (STEMMA-26). The details that follow from them (the order of the steps, the
plan before the purge, the form of the stub) are proposals. No code exists yet.

## Context

The goal of Stemma is that no state of an agent is lost (ADR-010). That must not mean that
nothing can be removed. Two things keep a secret once it is in a tracked file, for example a key
that a `config` gained after it was tracked:

- The history. A snapshot saves every tracked file, so the blob is in the tree of the working-copy
  commit and of every commit made from it.
- The operation log. It keeps every commit that an operation made unreachable (ADR-011), so
  rewriting the history alone leaves the old commits reachable through the log.

The exclusion file (ADR-012) and the confirmation of new files (ADR-010) make this rare, but not
impossible. ADR-005 chose a local log so that a secret can be removed; ADR-010 and ADR-011 leave
the removal to an operation of its own and name a stub that replaces the entries (STEMMA-26).

Facts that bound the design:

- An entry holds IDs, the actor, and the time, never file content (ADR-011). The secret is in the
  blobs and trees that the IDs reach. An entry is harmful because its commit ID makes the old
  state reachable, for example through `stemma op restore`.
- An ID is the hash of the content (ADR-005, ADR-001). Changing a tree changes the ID of its
  commit and, through the parent links, the ID of every descendant.
- Nothing is deleted today (ADR-011), and no garbage collection exists (ADR-007).
- The log is local and no hub exists (ADR-003 and ADR-004 are proposals). Roles are not built.

## Decision

- **The purge rewrites the affected commits** *(confirmed)*. The input is the ID of a blob. Every
  commit whose tree holds that blob (in any depth) gets a new tree without the entry, and the same
  author, actor, time, message, and change ID. Its descendants get new IDs because their parent
  changed. Branches, HEAD (when it names a commit), and the working-copy commit move to the new
  commits. The change ID stays, so a person can still follow a change across the purge.
  - A file whose blob is removed leaves the tree, so it is no longer tracked. The directory is not
    touched. If the file is still needed, the secret has to leave it, the name has to go into
    `.stemmaignore` or the file has to be tracked again, and `track` does the rest.
  - Rewriting only the blob and keeping the commits is not possible: a tree that names a missing
    blob cannot be read, and a replacement blob has another ID, so the tree would have to change
    as well.
- **The entries of the log that name a rewritten commit become stubs** *(confirmed)*. Every entry
  that names a rewritten commit as an old or a new value is replaced by a stub with the **same
  number**, so the numbering has no gap (ADR-011). The stub keeps the number, the time, and the
  actor, and drops the operation and every ID. A stub is not an entry that can be restored:
  `stemma op restore` refuses it and says that it was purged. All entries that name a rewritten
  commit are replaced, also those of descendants that do not hold the blob themselves, because
  their commit has the secret among its ancestors.
- **The purge writes an entry of its own** *(confirmed)*. Operation `purge`, with the actor who
  ran it, the time, and the counts of rewritten commits and replaced entries. The entry does **not**
  name the ID of a commit that was rewritten, because the audit must not keep what it removes.
  The pointers that moved carry their new value only. Because of this a purge cannot be
  restored, which is its purpose.
- **Only a human may purge, and the call is authorized** *(confirmed)*. A purge removes history,
  which is level 3 of ADR-004 (human owner only, with an ADR). The core calls
  `authorize(actor, "purge", target)` like every operation (ADR-002) and **in addition refuses an
  actor that is an agent**, whatever the authorizer says: phase 1 allows everything, and an agent
  must not remove history on its own. This is a rule about the kind of actor and not a role, so it
  does not conflict with ADR-002, which leaves roles to the policy model. A human sponsor can run
  the purge for the agent.
- **The purge removes what only the log held, and nothing else** *(confirmed)*. After the
  pointers have moved and the stubs are in place, the purge computes the objects that were
  reachable before and are not reachable now (from the branches, HEAD, the working-copy commit, and
  the entries that remain), and deletes those. Objects that were already unreachable before are
  left alone, so the purge is not a general garbage collection and does not make ADR-007's
  collection necessary.
- **A synchronized repository stays open** *(confirmed)*. A secret that was pushed (phase 5) is
  also on the hub, and a purge here does not reach it. The rule for users is that a pushed secret
  counts as compromised and has to be replaced, whatever a purge does. The hub needs a removal of
  its own, which belongs to phase 5.

### The steps

The order keeps a crash harmless: the secret is never half-removed in a way that loses data, and
the same call finishes an interrupted purge.

1. **Plan.** `stemma purge BLOB` prints what it would do and changes nothing: the commits to
   rewrite, the entries to replace, the objects to delete. `--force` does it, as `stemma op
   restore` does (ADR-013). The plan includes the entries of the log, so it also finds a purge that
   stopped halfway.
2. Write the new trees and commits. They are new objects, so nothing is lost yet.
3. Under the lock, write the `purge` entry and move the pointers.
4. Replace the entries that name a rewritten commit by stubs (a temporary file and a rename, as
   for the tip file in ADR-011).
5. Delete the objects that only the log held.

A crash after step 3 leaves the old commits reachable through the old entries and the blob in the
store. The next call plans the same blob again, finds the entries, and finishes steps 4 and 5.

## Consequences

- A purge is the one operation that makes a repository forget. The tests have to show that no
  entry and no reachable object names the blob afterwards, that a stub refuses `op restore`, and
  that the numbering has no gap.
- Commit IDs change for the rewritten commits and their descendants. Anything that holds an old ID
  outside the repository (a note, a message) points at nothing afterwards. Change IDs stay.
- The hash chain of the log (STEMMA-41) collides with a stub: replacing an entry changes the hash
  that the next entry links to. The stub has to keep the hash of the entry it replaced, so that
  the chain stays checkable around it. The chain has to be decided before the stub gets its final
  format; until then the stub holds no hash.
- The tip file (ADR-011) is not affected: a stub keeps the number.
- Retention of snapshot entries (STEMMA-39) is a separate question, but the two share the stub:
  an entry that retention drops could be replaced by the same stub.

## Open items

- [ ] The format of the stub and of the `purge` entry, in the strict text form of ADR-011, and the
      hash that a stub keeps when the chain exists (STEMMA-41).
- [ ] How the user names the secret. The blob ID is the unit; a path or a pattern that finds the
      blobs in the history is a convenience, and a text that is replaced inside a file (instead
      of removing the file from the tree) would keep the file tracked.
- [ ] Whether the purge may keep a file tracked with the secret replaced by a given content,
      for a `config` that should stay in the history.
- [ ] The rewrite of commits with several parents (phase 3) and of commits that are signed
      (ADR-003): a signature does not survive a rewrite.
- [ ] The reachability check of the last step has to know every kind of reference that holds
      objects, including those that come later (tags, remote references).
- [ ] The removal on a hub and the rule for a repository that was synchronized (phase 5).

Slices, once the ADR is confirmed: the rewrite of commits with the plan (`Repository::purge`),
the stub in the log with its reader and `op restore` refusing it, and the removal of the
objects.
