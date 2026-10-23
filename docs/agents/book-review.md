# Book Review After a Phase

The book is developed in step with the code, slice by slice. Small slices drift apart over time:
terms vary, open questions stay open after they are answered, and sections lose the pattern of
`docs/agents/book-style.md`. This review brings the book back into line once per phase.

## When

- Before a phase counts as done. Each phase has a Plane issue `Review the book after phase N`
  with the label `docs` in the cycle of the phase; the cycle is not completed while it is open.
- In addition, after a slice that rewrote a chapter at large.

## How

Work in a separate worktree based on `main`.

1. **Scope.** The chapter of the phase, the sections of chapters 1 to 3 that the phase touches,
   the command line reference, the phase table in chapter 3, and chapter 11.
2. **Content.**
   - The text agrees with the code and with every ADR that the phase added or changed.
   - The `Ziel` and `Umfang` of the phase match what was built; what moved to a later phase says so.
   - Answered `Offene Fragen` are removed or turned into text; new ones are added; chapter 11
     collects the ones that remain.
   - The phase table in chapter 3 shows the status of the phase.
3. **Structure.** Every implemented section follows the pattern of `book-style.md`; edge cases
   sit in boxes or tables. The running example covers the commands of the phase.
4. **Language.** Run `/core:humanizer` in review mode on the chapters in scope. Check the
   terminology table, and add the terms the phase introduced.
5. **Figures.** Every flow with crash points, every before-and-after of pointers, and every state
   machine of the phase has a figure that follows the style rules. Every figure is referenced.
6. **Command line reference.** Every command and option in `stemma --help` has its section with
   synopsis, arguments, exit codes, and an example; nothing removed is still described.
7. **Build.** `just ci` and `just book` pass without errors or warnings.

## Result

- Small findings are fixed in one pull request, `📚 docs: Review the book after phase N`.
- Larger findings become Plane issues with the label `docs`, linked from that pull request.
- A finding that reveals a missing rule updates `book-style.md` in the same pull request.
