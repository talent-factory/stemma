# Book Style

How to write and change the book under `docs/`. Every agent and every person who touches a
chapter or a figure follows these rules, so that the book reads as one text across all phases.
The line length and the ConTeXt rules stay in `CLAUDE.md`; this file covers structure, language,
terminology, and figures.

The book is read by students of computer science at Master's level (MSc) in the courses DUA and
AISE, by colleagues, by future contributors, and by agents that operate Stemma, usually without the
code open. The code and the ADRs are authoritative; the book explains them.

The reader knows Git, operating systems, and the basic data structures and algorithms. The text
does not explain these at length; it names them and refers to the literature. What it does state
is the precise definition of each new concept, the invariants, the complexity, and the argument
why a guarantee holds, including after a crash.

## What belongs where

| Content | Place |
|---|---|
| Idea, flow, guarantees, reasons, examples | Book chapter |
| Decision, rejected alternatives, the complete rule | ADR in `adr/` |
| Workflows across several commands | Phase chapter, with the running example |
| Teaching: definition, invariant, complexity, correctness | Phase chapter, where it is introduced |
| Map of algorithms, exercises, how Stemma is developed with agents | Chapter 10 |
| Synopsis, options, exit codes, contract, one example per command | `docs/chapters/12-cli-reference.tex` |
| Function names, error variants, file names of the code | Code |

- Names from the code (`Repository::commit`, `PartlyMoved`) appear only where a reader needs them
  to find the code: in parentheses after the explanation, or in a box. A sentence never depends on
  one.
- Plane IDs (`STEMMA-n`) do not appear in the prose. A reader of the book cannot resolve them.
  ADR references (`ADR-011`) are welcome.
- A rule that needs more than a short box of edge cases belongs in the ADR, and the chapter
  points to it.

## Chapters and sections

A phase chapter keeps the order `Ziel`, `Umfang`, the sections of what is implemented, and
`Offene Fragen`. A phase that is not started yet has only `Ziel`, `Umfang`, and `Offene Fragen`.

A section that describes an implemented feature follows this pattern:

1. **What it is about.** Two to four sentences: the problem and the idea.
2. **Flow.** Numbered steps (`\startitemize[n]`) or a figure, when there are more than two steps.
3. **Guarantees.** What holds after success, after an error, and after a crash.
4. **Edge cases.** In an `infobox` (or `warnbox` for data loss) or a table, not in the paragraphs
   of steps 1 to 3.

A running example threads through chapter 4 and the later phase chapters: one short session
(`init`, `track`, an edit, `status`, `commit`, `op log`, `op restore`) whose real output appears
where a section discusses the command. Extend it rather than inventing a new one; regenerate the
output when the format changes.

A section that explains a workflow describes the situation, the sequence of commands, and what
each step changes in the working directory, the Working-Copy-Commit, the pointers, and the
Operation Log. It uses the running example where it can.

## Teaching content

Where a phase chapter introduces a data structure or an algorithm (a Merkle graph, the hash chain
of the log, the atomic replacement of a file), it explains it in this order:

1. **Definition.** What the structure is, in precise terms, with the notation the course uses.
2. **Invariant.** What holds at all times, and which operation could break it.
3. **Complexity.** Time and space in O-notation, with the meaning of each variable (`n` files,
   `k` entries).
4. **Correctness.** Why the invariant and the guarantees of the section hold, including after a
   crash at any step.
5. **Literature.** A reference to the course book or a primary source with `\cite[...]`, and an
   entry in the table of algorithms in chapter 10.

A short structure can combine the steps in one paragraph. A long argument goes into the ADR, and
the chapter gives its idea.

For AISE, a section on a feature that serves agentic work (actors, provenance, ownership, risk
levels, recovery) states the problem of agentic work it answers. Chapter 10 describes how Stemma
itself is developed with agents, as a case study.

## Command line reference

A section of `12-cli-reference.tex` serves humans and agents alike. Besides the synopsis, the
arguments, and an example, it states the contract a script or an agent relies on:

- the preconditions, and what the command does when one does not hold;
- what goes to standard output and what to standard error, and whether the output is stable
  enough to be parsed;
- every exit code with its cause and the next step;
- whether the command can be repeated safely, after success, after an error, and after a crash.

## Paragraphs and sentences

- One paragraph carries one idea, as a rule three to eight sentences. A longer paragraph becomes
  a list, a table, or two paragraphs.
- Explain why before you explain the exception.
- One statement per sentence. Prefer the indicative. Avoid chains of Konjunktiv II
  (`hielte`, `läse`, `stellte`); write `würde`, or the indicative with `sonst`.
- Swiss orthography: `ss`, never `ß`.
- Quotations use `\quotation{...}`, not typed quotation marks.
- Use a term the same way every time (see the table below). Do not vary it for style.

## Patterns to avoid

These patterns make the text sound generated and slow the reader down. Run the `/core:humanizer`
skill in review mode on new or rewritten prose.

- `nicht X, sondern Y` and `Die Frage ist nicht ..., sondern ...`: at most once per section.
- Lists whose items begin with a bold label and a colon: only for reference material (exit codes,
  options). In argument and overview text, write prose.
- Announcing a count (`Drei Themen ...`, `fünf Aussagen`) unless the list follows directly and the
  number matters.
- Repeated hedged attributions (`laut dem Beitrag ... nach dem Beitrag ...`): cite once, then state.
- Inflated significance, promotional words, and closing sentences without content.

## Terminology

Use the term in the first column. Define a new term at its first use in a chapter and add it to
this table and to `GLOSSARY.md` (the English domain glossary) in the same pull request.

| Use | Avoid | Meaning |
|---|---|---|
| Working-Copy-Commit | Arbeitskopie-Commit, WC-Commit | The commit that holds the state of the working directory (ADR-010) |
| Arbeitskopie | | The concept: the editable state of a repository |
| Arbeitsverzeichnis | Arbeitsordner | The directory on disk that holds the files |
| Snapshot | Schnappschuss | Recording the working directory into the Working-Copy-Commit |
| Verlauf | Historie, History | The commit graph |
| Zeiger | | A branch, HEAD, or the working-copy reference |
| Zeigerbewegung | Zug | Setting a pointer from an old to a new value |
| Operation Log, Eintrag | Operationslog, Logeintrag | ADR-011 |
| nach bestem Bemühen | auf Verdacht | Best effort: a failure does not fail the command |
| Mindeststufe | Boden | Policy floor: a risk level that a namespace cannot lower (ADR-004) |
| Akteur, Sponsor | Benutzer (for an actor) | ADR-002 |
| Exit-Code | Rückgabewert | |
| Change-ID | Änderungs-ID | The random ID a commit keeps when it is rewritten (ADR-010) |
| Inhaltsadressierung | | Content addressing: an object stored under its hash (ADR-001, ADR-007) |
| Merkle-Graph | Merkle-Baum, Hash-Baum | The graph of trees and commits named by ID (ADR-005) |
| atomares Ersetzen | atomares Schreiben | Temporary file, flush, rename or link, flush the directory (ADR-007) |
| verfolgt, unverfolgt | getrackt | A file in the tree of the Working-Copy-Commit, or not |
| Ausschlussdatei | Ignore-Datei | `.stemmaignore` (ADR-012) |
| Sperre der Referenzen | Lock | `refs.lock`: one writer at a time (ADR-008) |
| Write-Ahead-Regel | Write-Ahead-Log (for the Operation Log) | The entry is flushed before its pointer moves (ADR-011) |
| Ausgang | | Whether the moves of an entry took effect (ADR-011) |
| Hash-Kette | Hashkette | The link from each entry to the hash of the one before (ADR-011) |
| Anker | | Number and hash of an entry, kept outside the repository (ADR-011) |
| Aufbewahrung | | How many snapshot entries the log keeps (ADR-011) |
| Restore | Wiederherstellung | `op restore` and its entry (ADR-013) |
| Purge | Bereinigung | Removing a blob from history and log (ADR-014) |
| Stub | | An entry whose operation and IDs a purge removed (ADR-014) |
| Journal | | The record of a purge in progress (ADR-014); not the Operation Log |
| Wurzel | | A commit from which the plan of a purge starts (working-copy commit, branch, HEAD, log) |
| Memo | | The hash table of rewritten trees and commits in the plan of a purge |
| Undo, Redo | | Only the two directions of the journal of a purge; never for `op restore` |
| Commit-Satz | | The record that completes a transaction in database recovery |
| strenger Leser | | A reader that accepts only the one encoding (ADR-005) |
| Option | Schalter, Flag | A named argument of a command, such as `--actor` |
| Sitzung | | A work session, such as the running example; `Session` only for the field of an agent |

## Figures

Add a figure when a section describes

- a flow with four or more steps or with crash points,
- the state of pointers before and after an operation,
- a state machine (for example `--resume` and `--abort`), or
- a structure that the text otherwise lists over more than a paragraph.

Rules:

- The source is an SVG in `docs/images/src/` with an English file name. `just figures` converts it;
  the PDF is not versioned.
- Every figure is placed with `\startplacefigure[title={...},reference=fig:...]` and referenced in
  the text with `\in{Abbildung}[fig:...]`. A figure without a reference does not go in.
- The text in a figure and its `<title>` element are German, like the prose.
- Font: `Helvetica, Arial, sans-serif`; IDs, commands, and file names in `Courier, monospace`.
- Shortened IDs look like `b3:9f2c…` and `ch:7a41…`.
- Colors carry meaning and are the same in every figure. A zone always has its color (taken from
  `commands.svg`):

  | Zone or role | Color |
  |---|---|
  | Arbeitsverzeichnis | `#2B6CB0` |
  | Working-Copy-Commit | `#B45309` |
  | Branch und HEAD | `#2F7D32` |
  | Operation Log | `#6B46C1` |
  | Text | `#0F172A` |
  | Secondary text, frames | `#64748B` |
  | Crash point (dashed line) | `#B91C1C` |
  | Background | `#FFFFFF` |

- A crash point in a flow is a dashed line in its color, labelled with what remains after a crash
  at that point.

## Checks before a pull request

- `just doc-lines` and `just book` pass without errors or warnings.
- New figures appear in the list of figures and are referenced in the text.
- New terms are in the terminology table.
- A new data structure or algorithm follows the teaching pattern and is in the table of chapter 10.
- A new or changed command states its contract in the command line reference.
