# Coding standards

Read during review, not during implementation. A rule that a linter, a test, or a CI job can
enforce belongs there; this file holds the judgement calls that no check can substitute for.

## A rule of the writer has its twin in the reader

Whatever a writer enforces when it appends an entry (an approval, a proof, a key state, a
limit), the code that reads the entry back enforces too: the recount, `op verify`, the merge
gate. A log can be edited by hand, so the reader trusts no writer.

- When a diff adds a check to a write path, find the read path that decides whether the
  written entry counts, and confirm that it asks the same question.
- A test for the writer's refusal is not enough: it needs a second test that writes the
  entry by hand and expects the reader to reject it.
- Where the two paths cannot ask the same question, the text (doc comment, ADR) says why.

Example: `Repository::approve_pull` refused a retired key, but the recount accepted a
hand-written approval of that key until `verify_approval` took the number of the entry
(STEMMA-239).

## Every refusal comes before the first write

A command that can refuse for a reason it is able to know in advance (a key that does not
prove the actor, a limit, a role) decides before it appends the first entry. An entry that is
written and then fails costs a repair path: exit code 3, an entry without its proof, a state
that a repeat of the command does not undo.

- When a diff writes an entry and then checks something about the actor or the arguments,
  move the check in front of the write. Where the core only knows the answer after the write
  (the number of the entry), try the same question with a probe first.
- What cannot be known in advance (a full disk, a lock) is exit 3 with a message that names
  what to do next.

Example: `lease override` replaced the lease file and then found that the key was not the
key of the owner, so the lease changed hands without a proof (STEMMA-244).
