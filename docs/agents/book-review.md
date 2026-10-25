# Book Review After a Phase

The book is developed in step with the code, slice by slice. Small slices drift apart over time:
terms vary, open questions stay open after they are answered, and sections lose the pattern of
`docs/agents/book-style.md`. This review brings the book back into line once per phase.

The book serves four purposes, and the review checks each of them:

- It shows how Stemma works: its flows and the workflows a user runs through.
- It is the reference manual for humans and for agents that operate Stemma.
- It is a reference and textbook for the courses DUA (data structures and algorithms) and AISE
  (AI assisted software engineering).
- It addresses computer scientists at Master's level (MSc).

## When

- Before a phase counts as done. Each phase has a Plane issue `Review the book after phase N`
  with the label `docs` in the cycle of the phase; the cycle is not completed while it is open.
- In addition, after a slice that rewrote a chapter at large.

## How

Work in a separate worktree based on `main`.

1. **Scope.** The chapter of the phase, the sections of chapters 1 to 3 that the phase touches,
   the command line reference, the phase table in chapter 3, chapter 10, and chapter 11.
2. **Content.**
   - The text agrees with the code and with every ADR that the phase added or changed.
   - The `Ziel` and `Umfang` of the phase match what was built; what moved to a later phase says so.
   - Answered `Offene Fragen` are removed or turned into text; new ones are added; chapter 11
     collects the ones that remain.
   - The phase table in chapter 3 shows the status of the phase.
3. **Structure.** Every implemented section follows the pattern of `book-style.md`; edge cases
   sit in boxes or tables.
4. **Workflows.** The book explains how the commands of the phase work together, not only each
   command on its own.
   - Every workflow the phase enables is described end to end: the situation, the sequence of
     commands, and what each step changes in the working directory, the Working-Copy-Commit,
     the pointers, and the Operation Log. Typical workflows are recording work, undoing a step,
     and recovering from a crash or an interruption.
   - The running example covers the commands of the phase, and its output matches the program.
5. **Reference for humans and agents.** Every command and option in `stemma --help` has its
   section in the command line reference; nothing removed is still described. Each section
   states, besides synopsis, arguments, and an example, the contract a script or an agent relies on:
   - the preconditions, and what the command does when one does not hold;
   - what goes to standard output and what to standard error, and whether the output is stable
     enough to be parsed;
   - every exit code with its cause and the next step;
   - whether the command can be repeated safely, after success, after an error, and after a crash.
6. **Teaching.** The book teaches what the phase uses, at the level of `book-style.md`.
   - DUA: every data structure and algorithm that the phase introduces is explained where the
     phase chapter introduces it, following the teaching pattern of `book-style.md`. The table
     of algorithms in chapter 10 lists it with its place in the code and in the course book, and
     every entry of the table still exists in the code.
   - AISE: every feature of the phase that serves agentic work (actors, provenance, ownership,
     risk levels, recovery) states the problem of agentic work it answers. Chapter 10 describes
     how Stemma itself is developed with agents; the review updates it with the practices the
     phase added or changed (ADRs, skills, review loops, the book alongside the code).
   - Every chapter in scope with teaching content has exercises, or chapter 10 lists them for
     the phase.
7. **Language.** Run `/core:humanizer` in review mode on the chapters in scope. Check the
   terminology table, and add the terms the phase introduced. Check the level against
   `book-style.md`: terms are defined before they are used, claims rest on an argument or a
   source, and basics that an MSc student knows are not explained at length.
8. **Figures.** Every flow with crash points, every before-and-after of pointers, every state
   machine, and every workflow of the phase has a figure that follows the style rules. Every
   figure is referenced.
9. **Build.** `just ci` and `just book` pass without errors or warnings.

## Result

- Small findings are fixed in one pull request, `📚 docs: Review the book after phase N`.
- Larger findings become Plane issues with the label `docs`, linked from that pull request.
  Missing teaching content or a missing workflow is usually a larger finding.
- A finding that reveals a missing rule updates `book-style.md` in the same pull request.
