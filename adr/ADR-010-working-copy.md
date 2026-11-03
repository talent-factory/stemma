# ADR-010: The working copy is a commit

**Status:** Accepted. The decisions marked *(confirmed)* were agreed with the author on
2026-10-19, `commit` and the tracked set on 2026-10-21, and the actor of a snapshot on
2026-10-21 (STEMMA-38). The open items at the end are limits that the first version has and
questions for later; none of them changes a decision above.

## Context

Git keeps three states: the working copy, the index as a staging area, and the last commit. What
an agent has not staged and committed is not in the history, so a crash, a wrong `checkout`, or
an overwritten file loses it without a trace. The goal of Stemma is that no state of an agent is
lost *(confirmed)*. The index of `crates/stemma-core/src/index.rs` is built as a data model
(`stemma-index 1`), but nothing depended on it yet when this ADR was written: there was no `add`,
`commit`, or `status`.

Jujutsu treats the working copy as a commit that every command updates, and in Scion, as the
Anthropic article (`anthropic2026redesigning`) describes it, the server creates every commit
while the client only lays out the working copy and watches it for changes. Both are prior
art, not a template (ADR-015).

## Decision

- **The working copy is a commit** *(confirmed)*. A repository has a working-copy commit. The
  files in the directory are its tree. A command that changes files does not stage them first.
- **A snapshot at every command** *(confirmed)*. Before a command runs, Stemma builds the tree of
  the working directory. If it differs from the tree of the working-copy commit, a new
  working-copy commit replaces the old one, with the same parents and the same change ID. What
  changed between two commands is safe from the next call on. What an agent overwrites while no
  command runs is not seen. There is no file watcher and no background process in phase 1; a
  watcher can follow in a later phase.
- **The operation log keeps every replaced commit** *(confirmed, as a consequence)*. A snapshot
  makes the previous working-copy commit unreachable from every reference, so only the log keeps
  it. Without the log the guarantee is empty, so no snapshot is made before the operation log
  (ADR-005, STEMMA-5) exists. Garbage collection (ADR-007) treats the commits that the log names
  as roots. This holds for every replaced commit in phase 1; a later retention gives up the
  oldest snapshot states (ADR-011, STEMMA-39).
- **Every commit has a change ID** *(confirmed)*. A snapshot gives a new commit a new ID, because
  the content differs. The change ID stays the same through every snapshot and rewrite, so that
  a review, a pull request (phase 4), and a rebase (phase 3) can name "the same change". The
  `Commit` type has the field `change`, written as the header `change <id>` (ADR-005).
- **The change ID is `ch:` and 32 lowercase hexadecimal digits** *(confirmed)*. It has 128 bits and
  comes from the random source of the operating system (the crate `getrandom`); the standard
  library has none meant for it. `ChangeId::generate` never falls back to a weaker source and
  reports a failure instead, because two changes that share an ID would be joined, and it refuses
  a source that returns only zeros. A rewrite takes the ID of the commit it replaces, a new change
  gets a new one, and **a merge commit gets a new change ID** *(confirmed)*: it joins two changes
  and continues neither. `Commit::try_new` takes the ID as an input and does not decide which case
  applies. A short form for people can be derived from the long one later, for display only.
- **Untracked files are not saved until they are confirmed** *(confirmed)*. A file that is not in
  the tree of the working-copy commit is *untracked*. A snapshot saves a tracked file that
  changed or was deleted. It does not save an untracked file; `stemma track <path>` confirms
  one. The name `track` is confirmed (STEMMA-2). This replaces `stemma add` and keeps its purpose:
  a new file never enters the history by accident.
- **An exclusion file** *(confirmed)*. `.stemmaignore` lists paths that are never tracked, not
  even by `stemma track`, and are not reported as untracked. Its syntax is open, and it is
  strict like the other readers: what the writer would not have written is rejected.
- **What `track` does** *(confirmed)*.
  `Repository::track` takes relative paths, builds the new tree from the tracked files and from
  the files at the paths, and replaces the working-copy commit like a snapshot does. A directory
  stands for the files below it, without a directory named `.stemma`. The tracked files are saved
  as well, because the new tree is built from the working directory and a track that kept the old
  blobs would drop a change. A path that is tracked already is not an error. A path that does not
  exist or breaks a path rule is refused before anything is written; a special file or a name that
  is not UTF-8 stops the call, as it stops a snapshot. A path spells every name as the directory
  has it: a file system that ignores case would take `a.txt` for `A.txt`, and the tree would hold
  the file twice, so the call refuses the other spelling and names the one on disk. A file that
  the caller named and that is gone when it is saved is an error (`NotFound`); a file found below
  a directory that is gone is left out. Below a directory, a name that resolves to `.stemma` is
  skipped without a word, since a tree cannot hold it (this skips a repository inside the
  directory as well), and any other name that a tree refuses stops the call (`InvalidName`).
  `.stemmaignore` (ADR-012) is read by `track`: an excluded file below a directory is skipped
  like those names, and an excluded path that the caller names is an error. A file whose name a
  tree refuses is skipped as well if a pattern excludes it, and it stops the call otherwise.
- **`commit` names the working-copy commit and starts a new one** *(confirmed, implemented in
  `Repository::commit`)*. The current working-copy commit gets its message, and its change keeps
  its ID. A new, empty working-copy commit starts on top of it. HEAD keeps naming a branch
  (ADR-008); the working-copy commit is a child of the commit of that branch. Details:
  - *Empty* means the same tree as the commit it follows. The tracked set is the tree, so a tree
    without the files would leave every file untracked.
  - `commit` takes a snapshot of the tracked files first, so that a change since the last command
    is part of the commit. The snapshot is an operation of its own and has its own entry. A
    failure after a snapshot that wrote an entry carries that entry
    (`CommitError::AfterSnapshot`), and the message names it, as for `status` and `op restore`.
  - The named commit `W'` is the working-copy commit `W` with the message, the actor, and the time
    now: the parents and the change ID are kept. The new working-copy commit `N` has `W'` as its
    only parent, the tree of `W'`, an empty message, and a new change ID.
  - Two pointers move in **one** entry of the log: the branch from its tip (or from nothing) to
    `W'`, and the working-copy commit from `W` to `N`. The branch moves first, so that a stop
    between the moves keeps `W'` reachable. If the second move fails, the error says that one
    pointer moved (`UpdateError::PartlyMoved`) and `op log` shows the entry as not taken (`!`),
    although the branch did move. The next `commit` finds the state (the tip of the branch has
    the parents and the change ID of the working-copy commit, and a message) and finishes the
    commit: it starts the new working-copy commit and moves that pointer alone, with an entry of
    its own. The message of that call is not used (`Committed::resumed`).
  - The commits alone do not prove such a stop: `op restore` of an entry from before a commit
    puts the working-copy commit back to an older version of itself and leaves the branch
    (ADR-013), which looks the same. So `commit` finishes only if the log says so: the newest
    entry of a `commit` moved the branch to its tip, and `oplog::outcomes` reads it as not
    taken. The next move of the working-copy commit decides it. After a stop it starts from the
    old value of the entry: a snapshot of a change since the stop, which keeps the parents and
    the change ID, so such a stop is still finished. After a restore it starts from the new
    value, so the entry took effect, and the call is refused as a branch that moved, with its
    message kept. An outcome that the log does not decide (behind a stub) is refused the same
    way (STEMMA-76).
  - A commit is refused when the message is empty (there is no editor in phase 1), when HEAD
    names a commit and not a branch, when the tree is that of the parent (or empty, for a first
    commit), and when the branch is not at the parent of the working-copy commit: a commit on
    top of that parent would drop what the branch has. Moving the working-copy commit
    onto the new tip is the rewrite below (STEMMA-171).
- **`switch` changes the branch while the working-copy commit is empty** *(confirmed, implemented
  in `Repository::switch` and `stemma switch`; STEMMA-167)*. `switch <name>` makes HEAD name an
  existing branch, `switch -c <name> [<start>]` makes the branch (at `<start>`, a branch or a
  commit, by default at the commit that HEAD leads to) and switches to it, and
  `switch --detach <commit-or-branch>` makes HEAD name a commit (`Head::Detached`). A name that
  does not exist is an error without `-c`, and a name that exists is an error with it. Details:
  - **A snapshot comes first**, as for every command that changes something. Then the
    working-copy commit `W` must be *empty* (the tree of its first parent, or the empty tree if it
    has none, as for `commit` and `status`). If it is not, the command is refused with a message
    to commit first; the snapshot stays, so nothing is lost. A switch that carries the changes
    over replaces the refusal later.
  - **A new, empty working-copy commit** `W'` starts on the tip `T` of the target: the tree of
    `T`, the only parent `T`, an empty message, and a change ID of its own. `W` stays reachable
    through the log only (ADR-011), as the old working-copy commit does after a `commit`.
  - **One entry holds all moves** (operation `switch`): HEAD, the working-copy commit `W` to
    `W'`, and for `-c` the branch from nothing to `T`. The order is that of a restore
    (ADR-013): the checks and the plan, the entry, the files, then the pointers (the branch, the
    working-copy commit, HEAD last). The checkout layer of ADR-013 is used unchanged, from the
    tree of `W` to the tree of `T`; equal trees move only the pointers. A file that is not
    tracked and in the way of a path of `T` is found by the plan, which runs before the snapshot
    when `W` is empty, so that nothing is written, not even a snapshot; the plan is made again
    under the lock.
  - **Switching to the branch that HEAD names is not a change**: the command ends with code 0 and
    the line `Already on <name>`, with no snapshot and no entry.
  - **A stop after the files is repeated by the same command.** The entry is written before the
    files, so a stop leaves the files of `T` in the directory and the pointers old, as for a
    restore. A snapshot would save those files as changes of `W` and `W` would then be refused as
    not empty, so the repeat is recognised before it takes a snapshot: the newest entry is a
    `switch` whose result is the one asked for (the same HEAD, the same new branch, and a
    working-copy commit that is a child of `T`) and whose pointers are each at the old or the new
    value of the entry, with at least one not moved yet. This holds after a stop between two
    moves as well, when the new branch exists already. The target is read again under the lock,
    and a branch that `-c` would make is refused only if no such entry explains it. The
    repeat takes no snapshot and uses that working-copy commit again, so its change ID is kept,
    and writes an entry of its own. It still applies the rule of the owner: another actor is
    refused without `--take-over`, and an agent never takes over. An entry of `take-over` names
    no pointer and does not hide the stopped switch. The error of a stop after the entry says to
    run the same switch again before any other command.
  - **`commit` stays refused on a detached HEAD**, and its message points to `switch -c <name>`.
  - **The level** of `switch` is 2, like `restore` (ADR-004): it moves refs and writes files in
    the working copy. The command has the option `--take-over` of the other commands that take a
    snapshot, and no `--json`.
- **`status` takes the snapshot and compares the commit with its parent** *(confirmed,
  implemented in `Repository::status` and `stemma status`)*.
  Like every command that does not only read the log,
  `status` saves the tracked files (so it has an actor, and the entry appears only if a tracked
  file changed). The snapshot makes every tracked file equal to the working-copy commit, so
  "changed" cannot mean "differs from the working-copy commit": it means what the working-copy
  commit does against its **parent**, as in Jujutsu. That is `A` (in the commit and not in the
  parent), `M` (another content or mode, also a link in place of a file), and `D` (in the parent
  and not in the commit), sorted by path, with the files of a directory that is added or deleted
  listed one by one, and a file that becomes a directory (or the reverse) as a `D` and the `A`
  files below it. A commit with no parent is compared with the empty tree, and one with several
  parents with the first. The **untracked** files are the files below the working directory that
  are not in the tree of the working-copy commit and that `.stemmaignore` does not exclude
  (ADR-012), listed one by one.
  - `status` reads the exclusion file and walks the working directory **before** the snapshot.
    It stops where `track` stops (an invalid or unreadable `.stemmaignore`, a name that a tree
    refuses and no pattern excludes, a name that is not UTF-8, which a pattern cannot exclude),
    and so it stops before it writes anything.
  - A failure after the snapshot (reading the references, the commits, or the trees) leaves the
    snapshot: the error says so and carries the entry (`StatusError::saved`). A snapshot that
    moved the pointer and could not flush it is a result with a flush error, as for `commit`,
    and the command ends with code 3 after it has written the status (`stemma status`). A
    failure of reading after a snapshot, and an output that cannot be written, end with code 1
    and name the entry of the snapshot; code 1 is the code of a command that only reads. ADR-018
    refines this: a failure of reading that finds damage (a missing or corrupt object) ends with
    code 5, and a held lock with code 6.
  - The references are read one after the other and not under a lock, so a process that moves a
    pointer in between can make HEAD and the working-copy commit come from two states.
  - `op log` still takes no snapshot, because the snapshot would change the log it is about to
    show. A tracked file that a pattern excludes stays in the changes; a report "tracked and
    excluded" is an open item of ADR-012.
- **The tracked set is the tree of the working-copy commit** *(confirmed)*. There is no index file
  as a staging area. `Index` stays in the code as the model that builds a tree from a set of
  paths and as the format of a cache, if one is needed for speed.
- **Where the working-copy commit is recorded** *(confirmed)*. A file `.stemma/working-copy`
  holds the ID of the working-copy commit, in the form of a branch file (ADR-008), replaced with
  the same compare-and-swap under the lock of the references. One working copy per repository
  directory in phase 1. A missing file is an error and not "no commit yet"
  (`RefError::NoWorkingCopy`).
- **`init` makes the first working-copy commit** *(confirmed)*. It has the empty tree, no parents,
  a change ID of its own, and the actor of `init` as its author. It goes into the first entry of
  the log together with HEAD, so a snapshot always replaces a commit that exists, and the log
  holds every working-copy commit from the start.
- **What a snapshot does** *(confirmed)*. `Repository::snapshot` reads the tree of
  the working-copy commit, reads each file in it from the working directory, and builds a new
  tree. Only tracked files are looked at: a file that changed is saved, a file that is gone (or
  that a directory has replaced) leaves the tree, and a file that is not in the tree is not
  touched. The new commit keeps the parents, the change ID, and the message of the old one; its
  author is the actor of the snapshot and its time is now. If the tree is the same, there is no
  commit and no entry. A path is never followed through a link: a tracked file below a directory
  that is now a link counts as gone. A tracked path that is a special file stops the snapshot
  with an error instead of dropping it from the history. A link is saved as its target, and the
  executable bit is a part of the file mode where the file system has one. Where links are not
  written (Windows), the checkout writes a link as a plain file that holds its target, as Git
  does with `core.symlinks=false`, and the snapshot reads that file back as the target of a link
  that stays a link, so a checkout followed by a snapshot changes nothing. Unlike Git, the
  snapshot refuses such a file unless it holds a target that a link on Unix can have: UTF-8,
  not empty, at most 4096 bytes, and without a NUL byte or a line break. A line break also
  catches content that was written over the file as if it were the target itself; on Windows a
  tracked link cannot become a regular file yet. Where the file system has them, a file is opened
  and compared by device and inode with the one that was looked at, so a file or a directory
  swapped for a link between the two is an error and not a read outside the working directory.
  A file that vanishes between the two counts as gone.

## Consequences

- **The format of `commit` changes in place** *(confirmed)*. The field `change` is new. ADR-005
  says an incompatible format gets a new object name, and makes one exception: no repository holds
  a commit, so `commit` is changed in place and the golden vectors were computed again. The
  exception ends with the first commit that a command writes.
- **`stemma add` (STEMMA-2) becomes `stemma track`.** `stemma commit` (STEMMA-3) names the
  working-copy commit and starts a new one. `status` (STEMMA-4) saves the tracked files,
  lists what the working-copy commit changes against its parent, and lists the untracked files.
- **A secret that was tracked is in the history and in the log.** The exclusion file and the
  confirmation of new files make this rare, but not impossible: a tracked `config` can gain a
  key. Removing a secret from the log is an operation of its own (see the open items).
- **Conflicts can be data in a commit.** This model allows it, because every state is a commit.
  The conflict model of phase 3 makes it so: a conflict is a sum of trees in a `commit2`
  (ADR-005), and the algebra is in `conflict.rs` of the core.
- **A conflict in the working copy is a view, and the snapshot compares bytes** *(confirmed;
  STEMMA-153, STEMMA-157, built in STEMMA-169)*. The directory holds files, so a commit with an
  open conflict is shown as one tree: each path whose values do not cancel is a file that holds
  the text with markers of `marker_text`, and every other path holds its value. The same tree
  is what `status`, `diff`, and the checkout layer (ADR-013) work on, and one function makes it.
  - **A path is conflicted** if the values of its name in the terms of the sum do not cancel and
    are not all directories. A directory is entered, so the names below it are paths of their
    own. A term that is a directory stands against a file or against nothing: it has no content
    of its own, and the text shows it as the line `(directory <id>)`, with the ID of its tree, so that two
    different directories never cancel; the files below it stay in
    the commit and are not written. A symbolic link shows as its target. If the contents
    cancel and only the modes differ, there is no text to show: the file holds the content that
    all of them have, and its mode counts in the comparison below. If the contents cancel and
    the path is absent in the view (a mode change against a deletion), the view holds no file;
    the conflict shows in `status` and is resolved by creating the file and tracking it. The
    mode of a file with markers is not recorded: a `chmod` on it changes nothing.
  - **The snapshot keeps a conflict only on an exact match.** If the file at a conflicted path
    has the bytes of the text that Stemma wrote, the path keeps its values; the new commit holds
    the same terms for it. Any other text resolves the path with exactly that text, in every
    term, and so does a file that is gone. Stemma does not parse markers, so a half-edited
    marker (one line removed, a marker left in a resolved file) counts as a resolution, and a
    file that was resolved by hand but still holds markers stays resolved. Parsing would not
    save these cases either.
  - **A change next to a conflict** is made in every term. A file that is not conflicted and
    that is edited gets the new content in all terms of the sum, so the conflict stays what it
    was. When the last conflicted path is resolved, the sum cancels down to one tree and the
    snapshot writes a plain `commit` again, with the same change ID (ADR-005).
  - **A commit with an open conflict is allowed.** `commit` succeeds with exit code 0 and
    says so in its one line; the new working-copy commit holds the same sum of trees. A gate
    that refuses to merge or to push a conflict belongs to the merge gate (phase 4).
  - **`status` and `log` mark it.** `status` has a section `Conflicts` (paths and the number of
    sides of each) before `Changes`. `log` marks the commit in its header line with
    `conflict (N sides)`. The two fields are added to `--json` without a new `schema` (ADR-018).
  - **`switch` still refuses** a working-copy commit or a target that holds a conflict
    (STEMMA-19).
- **A working copy belongs to one actor** *(confirmed)*. A snapshot is made by the actor of the
  command that takes it, and the log names that actor. A snapshot saves whatever is in the
  directory, so when two actors (a human and an agent, say) work in one directory, a change of
  one can be saved by a command of the other, and the log then names the actor who saved it,
  not the one who made the change. The rule is therefore: **every actor works in a working copy
  of its own**. Two actors in one directory are not a supported mode.
- **The owner of a working copy** *(confirmed; STEMMA-156, STEMMA-162)*. Format 5 (ADR-009)
  records the owner in the file `working-copy`: a second line `owner <actor>` after the line of
  the commit, with the actor as the exact string of ADR-002. `init` writes the owner, the actor
  of `init`. A repository of format 4 has no owner; it rises to 5 with its next log entry, and
  the actor of that entry becomes the owner once the entry is written, so the entry names it.
  The owner is recorded after the entry and never before it, so an entry that fails leaves no
  owner behind that no entry names; if recording the owner fails, the entry stands and the
  next entry records it. A purge raises the format as well, but its entries are no changes and
  record no owner; until a change does, any actor may save, as in format 4.
  A command of another actor that would take a snapshot is refused before it reads a file
  (`status` checks before it reads the exclusion file, and the snapshot again under the lock).
  The message names the owner and, for a human, the option `--take-over`; an agent is told to
  ask a human. The option is on the commands that take a snapshot (`track`, `commit`,
  `status`, `diff`, `op restore`, `switch`); `purge --apply` has none, and a person who has to
  take over runs `status --take-over` first. The take-over is part of the snapshot of the command
  (`Repository::taking_over`): it is decided before a file is read (an agent is refused, and
  the authorizer is asked for the operation `take-over`) and made after the files are read and
  the tree is written, just before the move. A command whose own checks fail first, such as a
  path that does not exist or an empty message, takes nothing over; one that fails only after
  its snapshot, such as a commit with nothing to commit, keeps the take-over, as it keeps its
  snapshot. The change of the owner has an entry of its own: the operation `take-over`, with no
  pointer, as the owner is not one (ADR-011). The entry is written first, and then the file
  names the new owner, in the order of every pointer. An owner who takes over writes nothing.
  Taking over is level 3 (ADR-004): it decides whose changes a snapshot may save, so only a
  human may do it, as only a human may purge. The refusal is an error of the class that the
  other refusals of a repository state have (exit code 1), so it needs no exit code of its
  own. Every move of the working-copy commit keeps the owner, `op restore` included: the owner
  is not a pointer, and a restore does not give the directory back to an earlier actor.
- **A second working copy is a linked worktree** (STEMMA-175). The rule "every actor works in a
  working copy of its own" has a place to stand now: `stemma worktree add` makes a second
  directory of the same repository, with the `HEAD` and the file `working-copy` (and so the
  working-copy commit and the owner) in a state directory of its own (ADR-009). A snapshot in one
  worktree reads and replaces only the working-copy commit of that worktree, so the commands of
  two actors in two worktrees take no snapshot from each other and need no `--take-over`. The
  objects, the branches, the tags, the log, and the lock of the references are shared, so a commit
  of one worktree is at once visible to the others, and one writer at a time holds the lock. The
  owner of the new working copy is the actor of `worktree add`, or the actor of `--owner`; the new
  working-copy commit names the owner as its author. A branch is checked out in one worktree only
  (ADR-009), so the branch of a worktree moves only by the commands of its owner. The check of the
  owner of a snapshot is unchanged: an actor that is not the owner of the worktree where the
  command runs is refused there, with `--take-over` for a human.
- **The lease of ADR-004 and the rule are independent.** ADR-004 proposes a lease on a branch,
  which a server would enforce: it governs who may move the branch. It does not govern who
  writes into a local directory, and a server cannot see that. The rule above is what covers
  the directory, and the working-copy commit has no lease of its own. Once the lease exists, an
  actor that holds the lease of a branch is the natural owner of the working copy that follows
  it; that link is part of the enforcement in the open items.

- **The working-copy commit follows a new base by a rewrite** *(confirmed; STEMMA-171, from the
  resolutions of STEMMA-154 and STEMMA-155)*. When the branch gets a new tip `T` (a merge, a
  rebase, later a `switch` that carries changes), the working-copy commit `W` with the old parent
  `P` is replaced by `W'`, one mechanism for all of them.
  - *The content* is the three-way merge `T + (W - P)` of the trees as a sum (ADR-005), so a
    conflict of the carry is data of `W'`, and a `W` with an open conflict goes through the same
    algebra: its terms cancel against the new ones, or the sum grows. A `W` without a parent has
    the empty tree as its base. A `W` with several parents is refused until a rule for its base
    exists.
  - *The identity.* `W'` has `T` as its only parent and keeps the change ID, the author, the
    time, and the message of `W`. A `W` that holds no change (its sum equals that of `P`) is not
    carried: it is replaced by an empty commit on `T` with a new change ID, as `switch` starts
    one, because an empty working copy has no change that a review could follow.
  - *The order* is that of ADR-013: snapshot, plan of the files, entry, files, pointer, under one
    lock. The mechanism moves the working-copy commit only and writes the entry `rewrite`; a
    caller that moves a branch or HEAD at the same time writes one entry for all moves. A stop
    after the files is finished by the same call, which takes no snapshot and uses the commit of
    the stopped entry again, as a repeated `switch` does.
  - *The directory* of a conflicted `W'` is the working-copy view of the commit (STEMMA-169): the
    file of every decided path and the text of `marker_text` for every path that is not decided,
    so the competing values are visible. The snapshot keeps the conflict while the text is
    unchanged, so a second rewrite carries an open conflict through the same algebra.
  - *Only the working-copy commit moves.* The other branches keep their commits, and the log keeps
    the replaced ones (ADR-011). The commands that move a branch or HEAD in the same operation
    cannot call it: they share its steps and write one entry for all moves (`rebase` does, below;
    merge is to). `op restore` undoes a rewrite like any entry.

- **`rebase` replays the branch of HEAD on a target** *(confirmed, implemented in
  `Repository::rebase` and `stemma rebase`; STEMMA-173)*. `rebase <target>` (a branch or a
  commit) replays the commits that the tip of the branch of HEAD has and the target has not, and
  moves the branch and the working-copy commit in one entry `rebase`.
  - *The range* runs from the tip down to the best common ancestor of the tip and the target
    (`merge_bases`). No common ancestor, several best ones (a criss-cross), a merge commit in the
    range, and a HEAD that names a commit are refused before anything is written. A tip below
    the target has an empty range, so the branch moves forward; a target that the branch has
    already is "up to date" and writes nothing, not even a snapshot.
  - *Each commit* is replayed as the sum `new parent + (commit - old parent)`, the algebra of the
    rewrite above, with the same change ID, author, time, and message. A conflict is data of the
    replayed commit and stops nothing; the commits above carry it on, and a source commit with an
    open conflict goes through the same sum. There is no `--continue` and no `--abort`: one
    `op restore` of the entry undoes the whole rebase.
  - *The working-copy commit* is carried onto the new tip by the same steps as
    `rewrite_working_copy`: it keeps its change ID unless it holds no change, in which case it is
    replaced by an empty commit with a new one, and its conflicts are shown in the directory.
  - *The order* is that of ADR-013, under one lock: snapshot, the replay (objects only), plan of
    the files, entry, files, pointers. The entry holds the move of the branch and of the
    working-copy commit; HEAD and every other branch stay. The branch moves first, so that a stop
    between the two moves leaves the new commits reachable. A stop after the entry is finished by
    the same call, which takes no snapshot and uses the commits of the stopped entry; a rebase
    onto another target is refused in that state, for the reason that a repeated `switch` is.
    The repeat is for the same target exactly: the commits the stopped rebase replayed must lie
    directly on it.
  - *Owner and authorization.* The authorizer is asked for `rebase` (the target is the revision)
    and, for the snapshot, for `snapshot`. A repeat checks the owner of the working copy as the
    snapshot would.

- **`merge` joins a branch into the branch of HEAD in one operation** *(confirmed; STEMMA-172)*.
  `stemma merge <branch> [-m <message>]` is the first command that calls the rewrite above with
  an entry of its own.
  - *The result* follows from the merge bases of the two tips `A` (HEAD's branch) and `B`: if `B`
    is an ancestor of `A`, nothing is done and no entry is written (the snapshot of the call, if
    the directory held changes, stays); if `A` is an ancestor of `B`, the branch takes `B` (a
    fast-forward, no commit); otherwise the command makes a merge commit with the parents `A` and
    `B` in this order, a change ID of its own, the actor and the time of the call, and the tree
    of `merge_commits` (ADR-005). A conflict is no failure: the merge commit is a `commit2`, and
    the command lists the paths. The default message is `Merge <branch> into <current>`; an empty
    message is refused.
  - *One entry for both moves.* The branch and the working-copy commit move under one lock in one
    entry named `merge` (a repeat names both moves as well, also when the branch moved
    already, so that a second stop is found again and `op restore` undoes both), in the order of
    ADR-013: snapshot, plan of the files, entry, files, the
    branch, then the working-copy commit. The branch moves by compare-and-swap against `A`, so a
    concurrent change is refused and not overwritten. HEAD and the other branches stay. The
    working-copy commit `W` follows the new tip `T` by the carry of the rewrite above (`T + (W -
    P)`), so the changes of the directory survive and an empty working copy is replaced by an
    empty commit with a new change ID. `op restore` undoes the merge with one call.
  - *A stop* after the files is finished by the same call, which takes no snapshot, uses the
    commits of the stopped entry again (the message of the repeat is not used), and moves the
    pointers that are still at their old value: the branch first, so a stop between the two
    moves leaves the merge commit reachable. Another merge is refused while the newest entry is a
    merge that did not finish, as a snapshot would save its files as changes. Another actor does
    not finish the merge of the owner without `--take-over`; an agent never takes over.
  - *Authorization.* The operation is `merge`, asked for twice: with the branch that moves as
    target, which gives the proof for the lock, and with the other branch, as `branch -m` asks
    for both names. It is not an approval of ADR-004: computing the merge is risk level 2, and only the branch of the current
    worktree moves (a branch is checked out in one worktree only, ADR-009).
  - *Known limit.* A repeat finds the stopped merge as the newest entry of the log, and the log
    is shared by all worktrees: a command that writes an entry in another worktree between the
    stop and the repeat hides it, and the repeat then takes a snapshot of the half-written
    files. `rewrite` and `switch` have the same limit. The error text names the entry and
    `op restore` as the way out; a repeat that looks for the stopped entry of its own worktree is
    a slice of its own.
  - *Not decided here:* what a merge does when `W` has two parents (the carry refuses it, as
    above), a merge of a commit or a tag instead of a branch, and a merge that stops for a
    conflict (`--no-commit`) or resolves one with a strategy.

## Open items

- [x] `Commit::rewrite`, which takes the parents and the change ID from the commit it replaces
      (STEMMA-3).
- [ ] A struct of named parts instead of six positional arguments of `Commit::try_new`.
- [x] A branch that someone moved to an older version of the working-copy commit that has a
      message, or a working-copy commit that `op restore` put back, looked like a `commit` that
      stopped, and the next `commit` finished it. The log decides it now (STEMMA-76).
- [ ] A commit with an empty tree as an allowed empty commit, and a commit while HEAD names a
      commit (its message points to `switch -c`), are refused today.
- [x] A commit while the branch has moved (a rebase, a merge). The mechanism exists:
      `Repository::rewrite_working_copy` replaces the working-copy commit by a commit on the new
      tip (STEMMA-171, below). `merge` calls it (STEMMA-172, below), and `rebase` shares its
      steps (STEMMA-173, below).
- [ ] `switch` refuses a working-copy commit that holds changes. Carrying them to the target uses
      the rewrite below and is a slice of its own (phase 3).
- [ ] What a rebase does with the change ID of a commit that has two parents, and what a merge
      does when the working-copy commit has one (phase 3).

- [x] Which commands take a snapshot. The commands that change something take one first:
      `track`, `commit`, `switch`, `op restore` (ADR-013), and `purge --apply` (ADR-014);
      `status` takes one, as it reports the state of the files. The commands that only read take
      none, as a snapshot adds an entry and would change what they are about to show: `op log`,
      `op verify`, the plan of `purge`, and `op restore --dry-run` (STEMMA-77). The path `.` of
      `track` tracks every file of the repository directory, secrets included, unless
      `.stemmaignore` excludes them (ADR-012).
- [ ] The path form is a `String` that the core validates and `normalize_path` writes. A
      `RepoPath` type that cannot be built from an invalid path would make the rule one
      definition; do it with the next command that takes paths (`status` takes none).
- [x] A command finds its repository in a parent directory when it runs below the root, as Git
      does, and the paths of `track` are relative to the directory of `-C` (STEMMA-72, ADR-009).
- [x] The syntax of `.stemmaignore`: patterns, negation, and whether it is itself tracked and
      which level of ADR-004 it belongs to. Decided in ADR-012 and built (STEMMA-24).
- [x] How long the operation log keeps the replaced commits of many snapshots (retention). The
      format and what an entry holds are decided in ADR-011. Decided there (STEMMA-39): phase 1
      keeps every snapshot entry; a later retention replaces them by stubs, together with the
      garbage collection of ADR-007.
- [x] Removing a secret from the history and from the log: a purge that rewrites the commits,
      replaces log entries by stubs, and deletes what the rewrite replaced. Decided in ADR-014
      (STEMMA-26) and built in STEMMA-46 to STEMMA-48.
- [x] Who the actor of a snapshot is when two actors use one directory, and how the lease of
      ADR-004 applies. Decided above (STEMMA-38): one actor per working copy, kept by convention.
- [x] Enforcing that rule: an owner in the file `working-copy`, and a command of another actor
      that takes a snapshot is refused unless it takes the working copy over explicitly.
      Decided above (STEMMA-156) and built in format 5 (STEMMA-162).
- [x] The cost of a snapshot for a large working directory: reading every file at every
      command. It is twice the tracked files today, because `Index::write_tree` verifies the
      blobs again (the code points here and does not repeat it). Measured in STEMMA-37 (release
      build, macOS with APFS on an SSD, one run each, files of 1 KiB): a snapshot of an unchanged
      working directory costs about 70 microseconds per file (0.04 s for 100 files, 0.7 s for
      10,000, 3.3 s for 50,000), and a changed file adds no measurable time. Ten files of 100 MiB
      take 1.5 s. The first `track` is slower by two orders of magnitude, about 6.6 ms per file
      (66 s for 10,000 files), because every object is flushed. **Decision:** no cache keyed by
      modification time and size for now. A snapshot stays under one second up to about 10,000
      files, and the cache is a risk for correctness. Look at it again when repositories with more
      than 50,000 files are realistic, and measure on Linux and Windows first (STEMMA-33). The
      first `track` flushes the new objects of a snapshot together since STEMMA-40, which halved
      it (49 s for 10,000 files, ADR-007). The time of the snapshot is also the time that a
      command holds the lock of the references, as it takes the lock before its first object
      (ADR-014, STEMMA-49).
- [ ] Files that change during a snapshot. Each file is read once and its blob is made from what
      was read, so a file is consistent in itself, but the files together are not one instant.
      Special files are refused, and so is a link whose target is not UTF-8, for Phase 1
      (STEMMA-77).
- [ ] A repository without the file `working-copy` (made before STEMMA-22, or the file removed). A
      snapshot stops with `NoWorkingCopy`, and no command makes the file again. Raising the format
      version would refuse such a repository at once; decide with the first command that needs it.
- [ ] A tracked path that is now a special file blocks every snapshot until it is gone, and a
      tracked directory that is now a link drops its files from the tree without a warning. Both
      want a way to report or to untrack (`stemma track`, STEMMA-2).
- [x] A second working copy (a worktree) for a second agent in one repository (phase 3,
      STEMMA-175): `stemma worktree add`, with the layout and the rules of ADR-009. Each worktree
      has its own `HEAD`, its own `working-copy` with its own owner, and its own working-copy
      commit; the objects, the branches, the tags, the log, and the lock are shared. Two actors
      therefore work at the same time without taking a working copy over from each other. See
      Consequences.
- [x] Whether `stemma commit` needs a flag that makes a snapshot and a commit in one step for a
      script. It does not: `commit` takes the snapshot itself before it commits, so
      `stemma commit -m` is that step (STEMMA-77).
