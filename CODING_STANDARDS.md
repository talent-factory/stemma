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

## A doc comment names every branch of the condition

A doc comment that says what happens in one mode, role, or state lists the other branches too,
or points to the item that does. A sentence about one branch reads as a promise that the others
behave differently.

- When a diff documents a condition (`enforcing`, `open`, an actor's role), read the code for
  each branch and confirm that the text holds for it.
- Say which part depends on the condition and which part holds always.

Example: three doc comments said that an unreadable policy refuses "in an enforcing
repository", but the code refuses in both modes; only a missing file depends on the mode
(STEMMA-246).

## A loop that reads shared state again has a test that moves the state

Code that retries and reads shared state on each attempt (the merge gate, a lease, a lock)
needs a test that changes that state between two attempts. A test that sets the state before
the call passes even when the read is hoisted out of the loop.

- When a diff adds a read inside a retry loop, find the test that moves the state while the
  loop waits (`pause::queue`, a competing write) and expects the second attempt to see it.
- Test the direction that matters for safety: a state that disappears or tightens between the
  attempts, not only one that relaxes.

Example: the gate reads the policy of the tip for every attempt. Only a test that moves `main`
onto a policy with level 3, or onto a commit without a policy, fails when the read is cached
(STEMMA-246).

## A public function takes the mode, not a flag that must match it

A public function that needs a fact which another argument already carries takes that
argument (`&Mode`), not a second value (`enforcing: bool`) that the caller has to keep in
step. A bare `bool` at a call site says nothing and is easy to invert.

- When a diff adds a `bool` parameter to a public function, ask whether a type of the crate
  already answers the question, and whether the function needs to be public.
- Where only the crate calls it, make it `pub(crate)`.

Example: `policy_at(base, enforcing: bool)` was public next to entry points that take
`&Mode`, so a caller could pass a flag that disagreed with the register (STEMMA-246).
