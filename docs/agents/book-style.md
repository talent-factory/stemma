# Book Style

How to write and change the book under `docs/`. Every agent and every person who touches a
chapter or a figure follows these rules, so that the book reads as one text across all phases.
The line length and the ConTeXt rules stay in `CLAUDE.md`; this file covers structure, language,
terminology, and figures.

The book is read by students (DUA, AISE), colleagues, and future contributors, usually without the
code open. The code and the ADRs are authoritative; the book explains them.

## What belongs where

| Content | Place |
|---|---|
| Idea, flow, guarantees, reasons, examples | Book chapter |
| Decision, rejected alternatives, the complete rule | ADR in `adr/` |
| Synopsis, options, exit codes, one example per command | `docs/chapters/12-cli-reference.tex` |
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
this table in the same pull request.

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
| Akteur, Sponsor | Benutzer (for an actor) | ADR-002 |
| Exit-Code | Rückgabewert | |

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
