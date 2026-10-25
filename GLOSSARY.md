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
A mutable name for a commit: a branch, HEAD, or the reference to the working-copy commit.
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

**Sponsor**:
The human who is accountable for an agent's actions. (Sponsor)
_Avoid_: Owner, principal

**Lease**:
The time-limited right of one actor to write to a branch. (Lease)
_Avoid_: Lock (for a branch)

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
