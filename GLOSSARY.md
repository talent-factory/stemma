# Stemma

Stemma is a version control system for humans and agents. Its core records content, history, and
every change to its pointers, together with the actor who made it.

Each term names the German word that the book uses in parentheses. The book's terminology table
in `docs/agents/book-style.md` lists the same pairs.

## Content and history

**Object**:
An immutable piece of content in the store, addressed by the hash of its encoding. It is a blob,
a tree, or a commit.
_Avoid_: Record, item

**Object ID**:
The hash of an object's encoding with the algorithm as prefix, such as `b3:` for BLAKE3.
_Avoid_: SHA, checksum

**Content addressing**:
Storing an object under the hash of its encoding, so that equal content is stored once and a
changed byte is found on read. (Inhaltsadressierung)
_Avoid_: Hash storage

**Merkle graph**:
The directed acyclic graph of trees and commits that name their parts by object ID, so that an
ID fixes everything it reaches. (Merkle-Graph)
_Avoid_: Merkle tree (for the commit graph), hash tree

**Atomic replacement**:
Writing a file under a temporary name, flushing it, renaming or linking it to its path, and
flushing the directory, so that the path names the complete old or the complete new content.
(atomares Ersetzen)
_Avoid_: Atomic write (for the whole procedure)

**Blob**:
An object that holds the content of one file. (Blob)
_Avoid_: File object

**Tree**:
An object that lists the entries of one directory by name, mode, and object ID. (Tree, Baum)
_Avoid_: Directory object

**Commit**:
An object that names a tree, its parents, a change ID, an actor, a time, and a message. (Commit)
_Avoid_: Revision, changeset

**Change ID**:
A random identifier that a commit keeps when the same change is rewritten. (Change-ID)
_Avoid_: Change hash

**History**:
The graph of commits and their parents. (Verlauf)
_Avoid_: Historie, log (for the commit graph)

**Merge base**:
A best common ancestor of two commits: a commit that both have as an ancestor (each counts as its
own) and that is no ancestor of another such commit. There can be none, one, or several.
(Merge-Basis)
_Avoid_: Common ancestor (alone), fork point

**Criss-cross merge**:
A history in which two merges each take in the other side, so that the two commits have several
merge bases. (Criss-Cross-Merge)

**Conflict**:
A state that two or more changes made to the same place and that nothing has decided. It is data
in the commit, a sum of trees, and not a mark in a file (ADR-005). (Konflikt)
_Avoid_: Merge conflict (alone), marker conflict

**Sum of trees**:
The list of trees of a conflict as add, remove, add, and so on: take the first tree, and apply
the change from each removed tree to the added tree after it. Equal added and removed terms
cancel, so a conflict never nests. One tree is no conflict. (Summe von Trees)
_Avoid_: Merge list, tree list (for the algebra)

**Conflict view**:
The one tree that shows a commit with an open conflict to the working directory, `status`,
`diff`, and the checkout layer: at each conflicted path a file with the text of the markers, at
every other path its value. A snapshot keeps a conflicted path only if its file equals that text;
any other text resolves the path. (Konfliktansicht)
_Avoid_: Marker file, conflict tree

**Tree merge**:
The merge of two commits as a function on their trees: it sums the trees of the two commits and of
their merge bases, decides every path, and returns a sum of trees, which is one tree if nothing
conflicts. It writes blobs and trees, and no commit and no pointer. (Tree-Merge)
_Avoid_: Merge (alone, for this function)

**Line merge**:
The merge of the contents of one file. The lines of the base that every comparison with the base
matches (_fixed lines_, Fixzeilen) divide the file into pieces (_Zwischenstücke_), and each piece
is decided as a sum of its own. One undecided piece makes the whole file a conflict. (Zeilen-Merge)
_Avoid_: Text merge, anchor (the anchor is a number and a hash of the log)

**Merge commit**:
A commit with two parents, made by `merge` when neither side is an ancestor of the other. (Merge-Commit)

**Fast-forward**:
A merge in which the branch moves to the tip of the other side and no commit is made. (Fast-Forward)

**Rebase**:
The commits of a branch are written again on a new target; the working-copy commit moves with
them. (Rebase, neu aufspielen)

**Term**:
One tree in a sum of trees, added or removed. (Term)

**Merge gate**:
The rule that decides whether a change lands on `main`. It reads the tip of `main`, merges the
change onto it, applies the rule of the risk level to the check results it holds, and moves
`main` by compare and swap; a conflict never lands (ADR-019, proposal). (Merge-Gate)
_Avoid_: Merge queue (a queue holds a lock while it tests)

**Check result**:
A statement that the tree of one commit passed or failed a set of tests, together with the tip of
`main` that the commit was merged onto for the test. (Prüfergebnis)
_Avoid_: Build status

**Clean merge**:
A tree merge that returns one tree, not a sum of several. (sauberer Merge)
_Avoid_: Trivial merge

**Semantic conflict**:
Two changes that merge cleanly and break each other; only a run of the checks on the merged tip
finds it. (semantischer Konflikt)
_Avoid_: Logical conflict

## Working copy

**Working directory**:
The directory on disk that holds the files of a repository and its `.stemma` folder.
(Arbeitsverzeichnis)
_Avoid_: Working folder, Arbeitsordner

**Working copy**:
The editable state of a repository, recorded as a commit. (Arbeitskopie)
_Avoid_: Checkout, workspace

**Working-copy commit**:
The commit that holds the state of the working directory; every command replaces it with a new
one when a tracked file changed. (Working-Copy-Commit)
_Avoid_: Arbeitskopie-Commit, WC commit, draft commit

**Snapshot**:
Recording the tracked files of the working directory into the working-copy commit. (Snapshot)
_Avoid_: Schnappschuss, autosave

**Worktree**:
An additional working directory of a repository, with its own HEAD, working-copy commit, and
owner. The working directory of the repository itself is the main worktree. (Worktree)
_Avoid_: Workspace, second checkout

**Owner of the working copy**:
The one actor whose changes a snapshot saves; another actor takes the working copy over
explicitly (ADR-010). (Eigentümer der Arbeitskopie)
_Avoid_: Owner (of a file in the file system)

**Tracked file**:
A file that the tree of the working-copy commit lists, and that a snapshot therefore saves.
(verfolgte Datei)
_Avoid_: Staged file, added file

**Untracked file**:
A file in the working directory that the tree of the working-copy commit does not list and the
exclusion file does not exclude; `status` shows it with `?`. (unverfolgte Datei)
_Avoid_: New file, unstaged file

**Exclusion file**:
The file `.stemmaignore`, whose patterns keep paths out of `track` and out of the untracked files
of `status`. (Ausschlussdatei)
_Avoid_: Ignore list

## Pointers and the operation log

**Pointer**:
A name for a commit: a branch, HEAD, a tag (which never moves), or the reference to the
working-copy commit.
(Zeiger)
_Avoid_: Ref (in prose)

**Branch**:
A pointer to a commit that moves forward with each `commit`. (Branch)

**HEAD**:
The pointer that names the current branch. (HEAD)

**Move**:
Setting a pointer from an old value to a new one. (Zeigerbewegung)
_Avoid_: Zug, update (of a pointer)

**Reference lock**:
The file `refs.lock` with an operating system lock on `refs.lock.guard`; a command that writes
holds it from its first object to its last move, so only one command writes at a time.
(Sperre der Referenzen)
_Avoid_: Lock (in prose), mutex

**Orphaned lock**:
A reference lock whose holder ended without releasing it (`kill -9`, a crash): `refs.lock` stays,
but the operating system lock on `refs.lock.guard` is free. The next command of the same host
that writes takes it over; while the journal of a purge exists, only `purge --resume` and
`--abort` do. (verwaiste Sperre)
_Avoid_: dead lock, stale lock (in prose)

**Operation**:
One command's change to the pointers of a repository, recorded as one entry. (Operation)

**Operation log**:
The local, numbered record of every operation, kept outside the history. (Operation Log)
_Avoid_: Operationslog, journal, reflog

**Entry**:
One record of the operation log: the operation, its actor and time, and the old and new value of
each pointer it moves. (Eintrag)
_Avoid_: Logeintrag, event

**Write-ahead rule**:
An entry is written and flushed before the pointer that it names moves, so every value a pointer
had is the old or new value of an entry, as long as only the core moves pointers and no purge
ran: a purge moves pointers to rewritten commits without naming them (ADR-014).
(Write-Ahead-Regel)
_Avoid_: Write-Ahead-Log (for the operation log)

**Outcome**:
Whether the moves of an entry took effect, as far as the log can tell. (Ausgang)

**Hash chain**:
The link from each entry to the hash of the entry before it. (Hash-Kette)

**Anchor**:
The number and hash of the last entry, kept outside the repository to detect a rewritten log.
(Anker)

**Stub**:
An entry whose operation and object IDs were removed, keeping its number, time, and actor.
(Stub)
_Avoid_: Tombstone, deleted entry

**Restore**:
An operation that puts the old values of an entry's pointers and the matching files back.
(Restore)
_Avoid_: Undo, revert

**Purge**:
An operation that removes a blob from every commit that holds it and from the operation log.
(Purge)
_Avoid_: Delete, scrub

**Journal**:
The record of a purge in progress, from which it resumes or aborts. (Journal)
_Avoid_: Operation log (for this record)

**Root (of a purge)**:
A commit from which the plan of a purge starts its walk: the working-copy commit, a branch, a
detached HEAD, or a commit that an entry of the log names. (Wurzel)

**Memo**:
The hash table in which the plan of a purge keeps the rewritten version of every tree and commit
it has seen, so that each is rewritten once. (Memo)

**Undo, redo (of a purge)**:
The terms of database recovery for the two directions of the journal: `--abort` removes the new
objects before the `purge` entry (undo), `--resume` finishes the steps after it (redo). Used only
in this sense, never for a restore. (Undo, Redo)

**Commit record**:
In database recovery, the log record that completes a transaction; the `purge` entry plays its
role for a purge. (Commit-Satz)
_Avoid_: Commit (for this record)

**Retention**:
The rule for how many snapshot entries the operation log keeps. (Aufbewahrung)

## Actors and permissions

**Actor**:
The human or agent on whose behalf an operation runs. (Akteur)
_Avoid_: User, author (for an agent)

**Agent**:
An actor that is a program, named with its model, session, and sponsor. (Agent)
_Avoid_: Bot

**Authorizer**:
The component that decides whether an actor may run an operation on a target; the core asks it
before every operation. (Autorisierer)

**Sponsor**:
The human who is accountable for an agent's actions. (Sponsor)
_Avoid_: Owner, principal

**Lease**:
The time-limited right of one actor to write to a branch. (Lease)
_Avoid_: Lock (for a branch)

**Write owner**:
The one actor that holds the lease of a branch and may write to it. (Schreibinhaber)
_Avoid_: Owner of a branch

**Owner override**:
Taking a branch over before its lease expires. Only the owner role may do it, with a reason and an
audit entry, and it never bypasses the merge gate. (Owner-Override)

**Risk level**:
A level from 0 to 3 that decides how many independent reviews a change needs. (Risikostufe)
_Avoid_: Severity, tier

**Policy**:
The rules of a namespace, including its risk levels per path. (Policy)

**Policy floor**:
A minimum risk level in the policy of a namespace that cannot be lowered. (Mindeststufe)
_Avoid_: Boden

**Namespace**:
A group of repositories with their own policy, such as an organisation, a course, or a project.
(Namespace)

## Behaviour

**Strict reader**:
A reader that accepts only what the writer would have written, so that every object has exactly
one encoding. (strenger Leser)
_Avoid_: Validating parser

**Plan**:
What a command would change, shown without writing anything: `purge` without `--apply`, and
`op restore --dry-run`. (Plan)
_Avoid_: Preview, simulation

**Best effort**:
A step whose failure does not fail the command; the next command tries again.
(nach bestem Bemühen)
_Avoid_: auf Verdacht

**Derived data**:
A value that is a function of a commit and its ancestors, such as a blame or an index of the log,
kept in the cache `.stemma/derived/` outside the object model. It can be deleted and computed
again, and a purge removes it (ADR-017). (abgeleitete Daten)
_Avoid_: Index (for the cache as a whole), metadata

## Diff

**Hunk**:
A group of changes of an edit script together with the unchanged lines around it. Changes whose
context lines overlap or touch share one hunk. (Hunk)
_Avoid_: Chunk, block

**Context line**:
An unchanged line that a hunk shows before or after a change. The diff uses three. (Kontextzeile)

**Change group**:
A maximal run of changed lines on one side of a diff. The slide step moves a group along the
unchanged lines that equal its edge lines. (Änderungsgruppe)
_Avoid_: Change block

**Side of a diff**:
One of the two compared versions, the old and the new. A side is a commit, or the
working-copy commit as it stands after a snapshot. (Seite eines Diffs)

**Pager**:
The program named in `PAGER` that shows the coloured output of `diff` page by page on a terminal.
(Pager)

**Terminal interface**:
The read-only view of `diff --tui` in the crate `stemma-tui`, which browses the files and hunks of
a diff (ADR-016). (Terminal-Oberfläche, TUI)
_Avoid_: display
