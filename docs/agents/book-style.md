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
One exception: a design that an ADR proposes and an issue asks the book to describe gets a section
before `Offene Fragen`, marked as a proposal in its first paragraph.

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
| Worktree | Workspace, zweite Kopie | A further working directory of the repository with its own HEAD, working-copy commit, and owner; the working directory of the root is the main worktree (ADR-009). Not a Git worktree: say \quotation{Git-Worktree} for that |
| Snapshot | Schnappschuss | Recording the working directory into the Working-Copy-Commit |
| Merge-Basis | gemeinsamer Vorfahre (alone) | A best common ancestor of two commits (ADR-017) |
| Criss-Cross-Merge | | A history in which two merges each take in the other side, so that there are several merge bases |
| Konflikt | Merge-Konflikt (alone) | An open conflict, kept as data in the commit (ADR-005) |
| Summe von Trees | | The list of trees of a conflict: add, remove, add, ... (ADR-005) |
| Fixzeile | Anker (that is the anchor of the log) | A line of the base that every comparison of a line merge matches |
| Zwischenstück | Abschnitt (of the line merge) | The text between two fixed lines, one part per term; the parts are a sum of their own |
| Zeiger | | A branch, HEAD, a tag, or the working-copy reference |
| Zeigerbewegung | Zug | Setting a pointer from an old to a new value |
| Operation Log, Eintrag | Operationslog, Logeintrag | ADR-011 |
| nach bestem Bemühen | auf Verdacht | Best effort: a failure does not fail the command |
| Mindeststufe | Boden | Policy floor: a risk level that a namespace cannot lower (ADR-004) |
| Akteur, Sponsor | Benutzer (for an actor) | ADR-002 |
| die Lease | das Lease | The time-limited right of one actor to write to a branch; the core enforces it and every writing operation renews it (ADR-004) |
| Schreibinhaber | Besitzer, Owner (of a branch) | The one actor that holds the lease of a branch (ADR-004) |
| Owner-Override | | Taking a branch over before its lease expires; only a human with the owner role, signed, with a reason and an entry in the operation log (ADR-004) |
| Exit-Code | Rückgabewert | |
| Change-ID | Änderungs-ID | The random ID a commit keeps when it is rewritten (ADR-010) |
| Inhaltsadressierung | | Content addressing: an object stored under its hash (ADR-001, ADR-007) |
| Merkle-Graph | Merkle-Baum, Hash-Baum | The graph of trees and commits named by ID (ADR-005) |
| atomares Ersetzen | atomares Schreiben | Temporary file, flush, rename or link, flush the directory (ADR-007) |
| verfolgt, unverfolgt | getrackt | A file in the tree of the Working-Copy-Commit, or not |
| Ausschlussdatei | Ignore-Datei | `.stemmaignore` (ADR-012) |
| Merge-Gate | Merge-Queue | The rule that decides whether a change lands on `main` (ADR-019) |
| Prüfergebnis | Build-Status | A statement about one commit and the tip of `main` it was tested on (ADR-019) |
| Prüfung, Lauf | Check, Build (in prose; `check` only as the command or entry) | A run of the tests on one commit; the run is one *Lauf*, its recorded outcome a *Prüfergebnis* |
| geprüfte Spitze | | The newest commit of `main` that a finished run covered (ADR-019) |
| letzte grüne Spitze | | The newest commit of `main` that a finished run without a base covered with success (ADR-019) |
| rote Spitze | | The oldest commit after the last green tip that a finished run covered with failure; while the newest verified commit is red, a change of level 2 or 3 lands only as a repair (ADR-019) |
| Fenster ungeprüfter Commits | | The commits of `main` after the verified tip (ADR-019) |
| Schranke | Limit, Grenze (for these three) | A rule of the gate that makes a change wait: the pause at a red tip, the window of unverified commits, the reservation (ADR-019) |
| Reservierung | Reservation | The claim of a change of level 2 or 3 after `N` invalidated runs; all other changes wait, except the repair of a red tip (ADR-019) |
| Pull Request | Merge Request, PR | The request of the write owner to merge a branch into `main`; its evidence is signed statements, the hash of its signed opening names it, and the number of its `pr-open` entry is its name in one log (ADR-004, ADR-025) |
| Freigabe | Approval, Review (for the entry) | A reviewer signs the head of a pull request: a signed statement, which the entry `pr-approve` names (ADR-004, ADR-025) |
| Kopf (eines Pull Requests) | Spitze (of the branch at approval) | The commit at the tip of the branch when the pull request was opened or approved; an approval binds it |
| Risikostufe | Severity | A level from 0 to 3 that decides who must approve a change (ADR-004) |
| Akteursregister, Register | Benutzerliste | The file `.stemma-actors/register` that binds identities to keys (ADR-003, ADR-004) |
| Beweis | Token, Login | A signature of an entry by the key that the register binds to the actor (ADR-003) |
| Branch | Zweig | A named pointer to a commit; a branch is checked out in one worktree at most |
| sauberer Merge | trivialer Merge | A tree merge that returns one tree |
| semantischer Konflikt | logischer Konflikt | Two changes that merge cleanly and break each other |
| Spitze | Tip, Kopf (except the head of a pull request) | The commit that a branch points to |
| Sperre der Referenzen | Lock | `refs.lock`: one writer at a time (ADR-008) |
| verwaiste Sperre | tote Sperre, Stale Lock | `refs.lock` of a process that ended; the next writer of the host takes it over (ADR-008) |
| Write-Ahead-Regel | Write-Ahead-Log (for the Operation Log) | The entry is flushed before its pointer moves (ADR-011) |
| Ausgang | | Whether the moves of an entry took effect (ADR-011) |
| Hash-Kette | Hashkette | The link from each entry to the hash of the one before (ADR-011) |
| Anker | | Number and hash of an entry of a chain, kept by someone who does not write that chain; a hub confirms the anchor of an operation log, and a receipt is an anchor of the record of the hub (ADR-011, ADR-026) |
| Aufbewahrung | | How many snapshot entries the log keeps (ADR-011) |
| Restore | Wiederherstellung | `op restore` and its entry (ADR-013) |
| Purge | Bereinigung | Removing a blob from history and log (ADR-014) |
| Stub | | An entry whose operation and IDs a purge removed (ADR-014) |
| Journal | | The record of a purge in progress (ADR-014) or of a fetch in progress (ADR-023); not the Operation Log |
| Wurzel | | A commit from which the plan of a purge starts (working-copy commit, branch, HEAD, log) |
| Memo | | The hash table of rewritten trees and commits in the plan of a purge |
| Undo, Redo | | Only the two directions of the journal of a purge; never for `op restore` |
| Commit-Satz | | The record that completes a transaction in database recovery |
| strenger Leser | | A reader that accepts only the one encoding (ADR-005) |
| Hunk | Chunk, Block, Abschnitt (of the diff output) | A group of changes with its context lines in a diff (Chapter 5) |
| Kontextzeile | | An unchanged line before or after a change in a hunk |
| Seite (eines Diffs) | | One of the two compared versions, the old and the new |
| Pager | | The program in `PAGER` that shows the output page by page |
| Terminal-Oberfläche (TUI) | Anzeige | `diff --tui`, crate `stemma-tui` (ADR-016) |
| Option | Schalter, Flag | A named argument of a command, such as `--actor` |
| abgeleitete Daten | Metadaten, Index (für den ganzen Cache) | A value computed from a commit and its ancestors and kept in `.stemma/derived/`; never part of the object model (ADR-017) |
| Sitzung | | A work session, such as the running example; `Session` only for the field of an agent |
| Änderungsgruppe | Änderungsblock | A maximal run of changed lines on one side of a diff (Chapter 5) |
| Merge-Commit | | A commit with two parents, made by `merge` when neither side is an ancestor of the other |
| Fast-Forward | | A merge that moves the branch to the tip of the other side and makes no commit |
| Rebase, neu aufspielen | rebasen (as a verb) | `rebase`: the commits of a branch are written again on a new target; the verb is \quotation{neu aufspielen} |
| Eigentümer der Arbeitskopie | Besitzer (of a working copy; that is the owner of a file in the file system) | The one actor whose changes a snapshot saves (ADR-010) |
| Haupt-Worktree | Root-Worktree | The worktree of the root of the repository, named `main` (ADR-009) |
| Konfliktansicht, Ansicht | Marker-Datei | The tree that shows an open conflict as files with markers (ADR-010) |
| Tree-Merge | Merge der Trees (as a name) | The function that merges two commits on their trees (ADR-005) |
| Zeilen-Merge | Text-Merge | The merge of the contents of one file by lines |
| Term | Summand | One tree in a sum of trees, with a sign (ADR-005) |
| Autorisierer | Authorizer | The component that decides whether an actor may run an operation on a target (ADR-002) |
| Algebra | | Do not use as a name for the sum of trees; say \quotation{Summe von Trees} |
| Elternteil | Elter | A parent of a commit |
| Verlauf | Geschichte, Historie, History | The commit graph; \quotation{Geschichte} only for the history of the project |
| Akteur | Benutzer, Nutzer | The one who runs a command; \quotation{Benutzer} only for the user of the operating system |
| Seite (eines Merges) | Zweig | One of the two commits that a merge joins |
| Git | git (lower case, except in a command) | The version control system |
| Klon | Kopie, Checkout (for a clone) | A repository on the machine of an actor, with the full object store and a working copy (ADR-015); it may know a remote |
| Hub | Server (for the role), Origin | The server that stores repositories and enforces their rules in addition to the local enforcement; known by its address and its key |
| das Remote | die Remote, Gegenstelle | A repository on a hub as a clone knows it: name, address, repository ID, pinned key (ADR-022) |
| Remote-Referenz | Remote-Branch, Tracking-Branch | The state of a branch or tag on the hub as the clone last saw it (ADR-008) |
| Bare-Repository | nacktes Repository | A repository without a working copy, as a hub holds it (ADR-009) |
| Protokollversion | | The number `stemma-protocol <n>` at the start of every message (ADR-023) |
| Objektstrom | Paket, Pack | The body of a fetch or an upload: whole objects, ancestors first, without deltas (ADR-023) |
| Upload | Hochladen (as a noun) | The first step of a push: the object stream goes to the hub, which stores it unnamed |
| Konto | Benutzerkonto, Login | A name on a hub with public keys and no secret of its own (ADR-024) |
| Anmeldung | Login, Sign-in | The signed challenge that yields a token, once per command (ADR-024) |
| Challenge | Herausforderung | The nonce of the hub that the key of the sender signs (ADR-024) |
| Token | Sitzung; Beweis (a token is none) | The short-lived credential of the hub that says which key speaks (ADR-024) |
| Hardware-Schlüssel | Token-Schlüssel, Sicherheitsschlüssel | A key of the type `sk-ssh-ed25519@openssh.com` on a hardware token (ADR-020) |
| Ref-Update | Push (for the signed request) | The signed request to move one or several references on a hub (ADR-025) |
| Aufzeichnung der Ref-Updates, Aufzeichnung | Protokoll (that is the transfer protocol), Journal, Reflog | The hash chain of accepted ref updates that the hub writes and signs (ADR-025) |
| Eintrag der Aufzeichnung | Record-Eintrag | One entry of the record: the signed text of the actor, the lines of the hub, and the signature of the hub (ADR-025) |
| Quittung | Bestätigung (that is the answer to an anchor) | The record entry that the hub returns for an accepted ref update (ADR-025) |
| signierte Erklärung, Erklärung | Statement, Aussage (for the proof type) | A signed proof that binds a repository and not the log of a clone: opening, approval, check result, closing, owner override, purge (ADR-025) |
| Menge der Erklärungen | Statement-Store, Erklärungslog | The statements that a clone or a hub holds, one file per statement, named by its hash; it has no order of its own (ADR-009, ADR-025) |
| Zeilen des Hubs | | What the hub adds, signed, to a record entry (number, time, register and policy commits, fast-forward per reference, actor, hash of the previous entry) or to a statement (number, time, register commit, number of the last record entry) (ADR-025) |
| Namespace | Organisation, Gruppe (as a type) | A flat group of repositories on a hub with an ID, a name and one namespace policy (ADR-027) |
| Transport (`Transport`) | | The interface of the core through which `fetch` and `push` exchange messages; pure I/O (ADR-022) |
| Ordnung des Hubs | Reihenfolge des Servers | The order that the numbers in the lines of the hub fix across clones (ADR-026) |
| annehmende Operation | Annahme-Operation | The operation of the core that judges a ref update or a statement on a bare repository (ADR-025) |
| Schlüssel des Hubs | Server-Schlüssel | The one Ed25519 key of a hub instance, pinned in the remote (ADR-026) |
| Bestätigung (eines Ankers) | Quittung (that is the receipt of a ref update) | The signed answer of the hub to an anchor (ADR-026) |
| Log-ID | | The random ID of the operation log of one `.stemma` (ADR-026) |
| Namespace-Repository | | The repository of a namespace that holds its register and the namespace policy (ADR-027) |
| Namespace-Policy | | The policy file of a namespace; combined with the policy of a repository by the maximum (ADR-027) |
| Gewährung | Berechtigung, Rolle (for a grant) | The right `create` or `read` of an account in a namespace (ADR-027) |
| einfrieren, das Einfrieren | sperren (for a repository) | The hub refuses every ref update and statement to a repository; reading goes on (ADR-027) |
| Betreiber | Administrator | Whoever runs a hub |

## Tables

A table that is longer than a page must break across pages. Without it, the rows run over the
footer. Every table with more than about eight rows, or with rows that wrap to several lines,
is built so that it can break, and repeats its header row on every page:

```tex
\startplacetable[title={...},reference=tab:...,location=split]
\startxtable[frame=off,split=yes,option=stretch]
\startxtablehead
<header row>
\stopxtablehead
\startxtablenext
<header row>
\stopxtablenext
<body rows>
\stopxtable
\stopplacetable
```

- `location=split` and `split=yes` allow the break; `option=stretch` fits the table to the text
  width.
- `\startxtablehead` is the header of every following page. `\startxtablenext` is the header of
  the page on which the table starts. Both hold the same header row.
- A column with commands in `\type{...}` is as wide as the longest command, with a little air
  (`worktree remove` needs about 0.26 of the text width). A command that is wider than its
  column runs into the next one; look for it in the PDF.
- The continuation pages carry the caption with a letter, such as Tabelle 11b. That is the
  intended result.
- Check the result in the PDF after `just book`: no row may reach into the footer, and every page
  of the table starts with the header. Short tables that fit on a page stay as they are.
- Examples: `tab:algorithms` in chapter 10 and `tab:exit3` in chapter 12.

A table with long running text in several columns (several lines per cell, three or more
columns) goes on landscape pages. `\startlandscape` does not exist in this setup; switch the
paper size around the table instead, and give the column with the most text a width:

```tex
\page
\setuppapersize[A4,landscape][A4,landscape]
\startplacetable[...,location=split]
...
\stopplacetable
\page
\setuppapersize[A4][A4]
```

- Header and footer follow the page width, so they need no change.
- Set the widths on the cells of the header rows (`\startxcell[width=0.44\textwidth]`), in both
  `\startxtablehead` and `\startxtablenext`. Equal columns waste the width on the short ones.
- The switch ends the page before the table, so the text before it may leave white space. Look at
  the page in the PDF. Example: `tab:algorithms` in chapter 10.

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
  | Branch, HEAD und Tags | `#2F7D32` |
  | Operation Log | `#6B46C1` |
  | Text | `#0F172A` |
  | Secondary text, frames | `#64748B` |
  | Light divider lines (grid, column separator) | `#E2E8F0` |
  | Crash point (dashed line) | `#B91C1C` |
  | Background | `#FFFFFF` |

- A crash point in a flow is a dashed line in its color, labelled with what remains after a crash
  at that point.
- Red (`#B91C1C`) means a crash and what it leaves behind: the dashed crash line, its label, and
  the state or the arrow that a crash leads to. A plain error that writes nothing, such as an
  invalid argument, is not a crash and is drawn in `#64748B`.
- A dashed line has one of two meanings. Dashed red is a crash point. Dashed `#64748B` is a path
  or a box that is not the main flow: a return path, an optional or rejected attempt, something
  that is only abridged or no longer referenced, or something that does not exist yet (a later
  phase). The figure labels the line or the box, or a legend says what it means. A thing that
  exists and belongs to the flow is drawn solid.
- The four zone colors mark zones only. A column or a box that is not a zone, such as a state
  directory or a pointer file, is `#64748B`.
- A figure that shows a design which an ADR only proposes says so in a line of its own in
  `#64748B` at the bottom, with the ADR and its status.
- Every text of a figure is at least 6.5 pt on the printed page. The size on the page is
  `font-size * w / viewBox width`, where `w` is the width in the chapter (the text block is about
  455 pt wide, so `width=0.9\textwidth` is 410 pt). Cut empty space from the `viewBox` or raise the
  font; do not shrink the font to fit a line in.
- The same state has the same name in every figure. The outcome of an entry (`Ausgang`) is
  `stattgefunden` or `nicht stattgefunden` once the log can decide, and `unbekannt` while the lock
  of a crashed writer is not taken over yet (ADR-011).

## Checks before a pull request

- `just doc-lines` and `just book` pass without errors or warnings.
- New figures appear in the list of figures and are referenced in the text.
- A long table breaks across pages with a repeated header (section Tables), and the footer is free.
- New terms are in the terminology table.
- A new data structure or algorithm follows the teaching pattern and is in the table of chapter 10.
- A new or changed command states its contract in the command line reference.
