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
