# ADR-008: References: Branches, Tags, and HEAD

**Status:** Accepted. Decisions marked *(confirmed)* earlier keep their date; the rest were
confirmed by the author on 2026-10-24 (STEMMA-9). All are implemented in Phase 1. The decision on
tags (STEMMA-54) was taken on 2026-10-29 and implemented in the store; the operation `tag` and
the command followed on 2026-10-30 (STEMMA-183), and so did the deletion of a tag (STEMMA-184).
The rule for a name that is both a branch and a tag was decided on 2026-11-04 (STEMMA-186).
The command `branch` (create, list, rename, delete) was added in Phase 3 (STEMMA-166).
Remote references were decided for Phase 5 on 2026-11-23 (STEMMA-56, see "Remote references"
below). Their files, their names, and what reads them were built on 2026-11-24 (STEMMA-285),
`fetch` and `clone` with the entry `fetch` on 2026-11-25 (STEMMA-295), and the commands
`remote`, `clone`, and `fetch` with the local branch of a clone on 2026-11-27 (STEMMA-305, see
"Which local branch follows"), and `push` in the core with the entry `push` on 2026-11-27
(STEMMA-309).
Packed references have no phase yet, and
the rename on Windows waits for the CI run (STEMMA-33).

## Context

A commit has an ID, but a person or an agent works with a name: `main`, `feat/login`. A
reference gives a name to a commit, and HEAD says which name, or which commit, the working copy
is on. The references are the only mutable state of the history: an object never changes, but a
branch moves. So a wrong write here is the one kind of damage that cannot be seen from the
objects, and two processes that move one branch at once can lose a change silently. Agents work
in parallel (ADR-004), so the second case is not rare. The implementation is
`crates/stemma-core/src/refs.rs`.

## Decision

- **A branch is a file** *(confirmed)*. `<metadata directory>/refs/heads/<name>` holds the ID of
  the commit and a newline, and nothing else. One file per branch means that independent branches
  do not touch each other, that each change replaces one file atomically, and that the state can
  be read with `cat`. A single file for all branches was rejected: every change would rewrite all
  of them.
- **HEAD** *(confirmed)*. `<metadata directory>/HEAD` is `branch <name>` or `commit <ID>` and a
  newline. A branch that has no file does not exist yet, and HEAD may name it, as it does in a
  new repository. There is no Git compatibility (README), so the format is our own and strict:
  the reader accepts only what the writer writes, as for objects (ADR-005). `ref: refs/heads/…`
  was rejected, because it would only help with Git tools that we do not support.
- **The name of a branch** *(confirmed)*. The components are separated by `/`. Each is made of
  `A-Z a-z 0-9 . _ -`, does not start with `.` or `-` (so `.` and `..` are out), does not end with
  `.`, and is not the name of a device on Windows (`con`, `prn`, `aux`, `nul`, `com1` to `com9`,
  `lpt1` to `lpt9`, in any case, with or without an extension). The whole name has at most 200
  bytes. Upper case is allowed, because names such as `feat/TF-792-login` are in use. Because the
  name is a file path on every platform, a name that works on one cannot fail on another.
  Loosening the rule later changes no ID.
- **A name is checked before the commit** *(confirmed)* (`update_branch`, `delete_branch`). When another entry holds
  the name (a case variant, a branch that is a prefix, or a directory of branches), the answer is
  `Conflict` with the kind of the entry, also if the caller names an `expected` commit: there is no
  branch of this name that could be at it, and `Stale` would hide the reason. `branch(name)` says
  `None` for such a name; it means "no branch with exactly this name".
- **One name per file system entry** *(confirmed)*. A name cannot be a branch and the start of
  other names (`a` and `a/b`), and two names that differ only in case (`Main`, `main`) are
  refused, on every platform, because a repository can be copied to a file system that ignores
  case. A name is compared exactly: on such a file system `main` does not open the file of `Main`.
- **A change is a compare-and-swap, and the checks are inside** *(confirmed)*.
  `update_branch(name, expected, new, objects)` moves a branch only if it is still at `expected`
  (`None` for a branch that does not exist) and fails with `Stale` otherwise. It checks that `new`
  is a commit that the object store has, so a branch cannot point to an object that is missing, or
  to a tree. As for `Index::write_tree` (ADR-005), no caller can forget a check. HEAD to a commit
  is checked the same way.
  HEAD has the same compare-and-swap *(STEMMA-53)*: `replace_head(expected, head, objects)` and
  `Repository::set_head(operation, expected, head, ...)` change HEAD only if it still is `expected`
  and fail with `StaleHead` otherwise, under the same lock, so of two racing changes (two
  `checkout`) the later one fails and does not replace the first. A HEAD that is missing or
  damaged is an error there, not a value to compare. The unconditional `RefStore::set_head`
  stays for what writes HEAD first (`init`) and has no value to compare.
  `update_branch` keeps `Option<CommitId>` for `expected`: `None` means "the branch has to be
  new", the doc comment and the message of `Stale` say so, and no caller has misread it; an
  `Expected { Absent, At(commit) }` would change over a hundred call sites for no failure seen.
  `delete_branch(name, expected)` is the same swap with a required `expected`; a missing branch is
  `Stale` with `found` `None`.
- **Create, rename, and delete a branch are one operation, `branch`** *(confirmed, STEMMA-166)*.
  `Repository::create_branch`, `rename_branch`, and `delete_branch` each write one entry of the
  operation `branch` (level 2, ADR-004) before they move a pointer, under the lock, in the order of
  `update_branch`; the authorizer gets the branch name as the target (for a rename it is asked for
  the old and then for the new name, and both must allow). The rules that the store leaves to the
  layer above are decided there, under the same lock as the change:
  - The branch that HEAD names is not deleted (`HeadBranch`). It is checked after `expected`, so a
    branch that does not exist is `Stale`, and a detached HEAD protects no branch. There is no check
    that the commits of the branch are reachable from another branch, and no force flag: the entry
    holds the tip as its old value, so `op restore` of the entry brings the branch back (ADR-013).
  - A rename moves up to three pointers in one entry: the new branch, HEAD if it names the old
    branch, and the old branch, in this order. A stop between two moves leaves the commit under at
    least one name and HEAD on a branch that exists, and the call is `PartlyMoved` (exit code 3).
    The new name must not exist (`BranchExists`): a taken name is no lost race, as for a tag, so it
    has no `expected`. A name held by another entry is `Conflict`, which includes the rename of `a`
    to `a/b` and the other way round: the file of the old name is in the way until it is deleted.
  - `create_branch` creates a branch with `expected` `None`. Only this function passes `None` for a
    command, so the question of `Expected { Absent, At(commit) }` (STEMMA-53) stays closed.
- **One lock for all changes** *(confirmed)*. The check, the comparison, and the write happen while
  the file `<metadata directory>/refs.lock` exists. It is created with "create new" and holds a
  token that only the process that made it knows; it is removed when the change ends, but only if it
  still holds that token, because a person may have removed a lock that looked stale, and another
  process may hold a new one. A change that finds the lock fails at once with `Locked` and does not
  wait. The lock is outside `refs/`, so no branch name can collide with it. One lock for all
  references is coarse, but a change is a few file operations, and it keeps the rule simple. A
  command that writes holds it longer, from its first object to its last move, at least as long as
  its snapshot takes, and a purge from its plan to its end (ADR-014, STEMMA-49). The command line
  removes the lock of an interrupted command (Ctrl-C, a termination signal) before the process ends,
  so only `kill -9`, a crash, or a handler that could not be installed leaves it behind. The merge
  gate is the one exception to "from its first object" (decided in STEMMA-229, implemented in
  STEMMA-241): it
  writes the objects of the merge before it takes the lock, and the lock covers the check of the
  tip, the entry, and the swap (ADR-019, STEMMA-229).
- **A lock that a process left is taken over** *(confirmed, STEMMA-51)*. While a process holds
  `refs.lock`, it holds a lock of the operating system on the file
  `<metadata directory>/refs.lock.guard`, which stays and has no content (`File::try_lock`). The
  operating system releases that lock when the process ends in any way, also by `kill -9` or a
  crash, so a lock file whose guard is free was left by a process that ended. The holder writes
  `stemma-lock 1`, `host <name>`, `pid <process ID>`, and `token <token>`, one line each, and a
  change that holds the guard replaces a file of exactly this form only if it names this host. A
  lock of the operating system may not reach from one host to another on a file system that two
  hosts share (NFS without a lock manager, a mount into a virtual machine), so a file of another
  host stays and is `Locked` with that host; the message says that the file can be removed if no
  command runs there. The rule assumes that no two hosts that share a repository carry the same
  name, so the defaults `localhost`, `localhost.localdomain`, and `(none)` count as no name, and on
  macOS the name is the local host name, which stays when the network changes. A file that names
  this process is taken over as well: it was left by an earlier process with the same ID, as in a
  container whose command always runs as the same process. A lock that this process holds is found
  in its own list first, and is `Locked` as held by this process, since the lock is not re-entrant;
  the guard is then not opened a second time, because closing a second handle releases the lock of
  the first where the system keeps its locks per process (`fcntl` on NFS). While the journal of a
  purge exists, a change does not take the lock over and fails with `PurgePending`; only `--resume`
  and `--abort` replace it (ADR-014). A file of another form stays and is `Locked`: an older version
  of stemma, a command without the guard, or a person made it, and so does the empty file that a
  crash between making and writing the lock leaves; it must be removed by hand. A guard that a
  running process holds is `Locked` with that process, whose message says to try again and not to
  remove the file. If the guard cannot be locked for another reason than a holder, or the name of
  the host is not known, the lock is made without a guard, in the form of an older version, so that
  no command takes it over; a guard that fails costs the takeover and never the exclusion, which
  `create_new` keeps. Such a command takes no lock over either, and its message names why the guard
  could not be taken; only `--resume` and `--abort` replace a file of another form then, which a
  purge without a guard leaves, as they did before the guard. The guard and the lock file are taken
  in this order and released in the other, and only a holder of the guard may replace a file of the
  guarded form, so two commands cannot take over one lock at once. A person who removes the guard
  while a process holds it breaks the rule, as one who removes the lock file does.
- **Safe writes** *(confirmed)*. A file is written to a new temporary file in
  `<metadata directory>/tmp/`, flushed, and renamed over the old one; the directory is flushed on
  Unix. The file is **not** read-only, because Windows refuses to replace such a file. A reader sees
  the old content or the new one. The temporary file is made before the directories of the branch,
  so that a failure of the first leaves no directory behind; if a later step fails, the directories
  that are empty again are removed. A directory without any file below it, which a crash can leave,
  is no branch and in nobody's way: the next change that needs the name clears it. If the file is in
  place and only the flush of the directory fails, the change has happened, and the error is
  `NotFlushed`; the caller reads the reference again and does not repeat the change with the old
  state.
- **A tag is a plain pointer that never moves** *(confirmed, STEMMA-54)*.
  `<metadata directory>/refs/tags/<name>` is a file in the form of a branch file: the ID of a
  commit and a newline. A tag has no object of its own, so there is no new object kind
  (ADR-005) and no message; a note can be added later as an object without changing any ID. Who
  set the tag and when is in the operation log (ADR-011): `Repository::create_tag` writes an
  entry of the operation `tag` before it makes the tag, and `stemma tag` calls it
  (STEMMA-183). A tag name follows the rules of a branch name and the checks of a branch (one name
  per file system entry, no case variants), in its own directory, so a tag and a branch may have
  the same name, and the two are checked each among themselves. `create_tag` fails with
  `TagExists` if the tag exists, at this commit or at another: a release that someone has read
  must keep its meaning, and the answer is no lost race that a caller could retry (`Stale` is for
  a branch). The operation `tag` is level 2 (ADR-004). A repository from before the
  tags has no `refs/tags/`, which is no tags; the directory is made by the first tag.
- **A tag that was set wrongly is deleted and set again** *(confirmed, STEMMA-184)*.
  `delete_tag(name, expected)` is the same compare-and-swap as `delete_branch`, with a required
  `expected`: the tag is deleted only if it is still at that commit. Another commit, or no tag
  at all, is `StaleTag` (a variant of its own, because `Stale` is for a branch), and a name that
  another entry holds is `Conflict`, checked before `expected` (the command reads the tag first,
  so it reports such a name as a tag that does not exist, and `Conflict` reaches the command only
  if the name appears between the read and the lock, or a caller of the core). The reason for
  `expected` is the case that a tag is deleted and set again at another commit between the look
  of the caller and its deletion: without `expected`, the deletion would remove the new tag,
  which someone may have read. `Repository::delete_tag` writes an entry of the operation `untag`
  before it deletes the file, in the order of `create_tag`, and `stemma tag --delete` calls it.
  The entry has the line `tag <name> <commit> -` (ADR-011). `untag` is level 2, like `tag`
  (ADR-004), and an authorizer can grant it apart from `tag`. Deleting a tag and setting it
  again are two operations with two entries: no operation moves a tag, which keeps the rule that
  a tag never moves and keeps the whole history of a name in the log. A purge that is refused
  for a tag (`TaggedCommit`) names `stemma tag --delete <name>` instead of the file, and
  `--move-tags` (STEMMA-185, ADR-014): the one case where a tag moves is a purge that the human
  who runs it asked to move the tag, and the `purge` entry counts it.
- **A name that is both a branch and a tag is an error, unless it is qualified**
  *(confirmed, STEMMA-186)*. `switch --detach` is the first command that resolves a name to a tag
  (STEMMA-198), and this is its rule. It applies to every place that takes any revision: today
  `switch --detach`, the start of `switch -c`, and the target of `rebase` (STEMMA-208), and
  `log` and `diff` as soon as they accept a tag.
  Such a place resolves the text it gets in this order:
  1. Text that starts with `b3:` is a commit ID (a name holds no colon) and is no name.
  2. `heads/<name>` is the branch `<name>`, and `tags/<name>` is the tag `<name>`. These two
     prefixes are the qualified form. They are tried before the lookup and never fall through
     to it: if the branch or the tag does not exist, the name is unknown. Since STEMMA-285,
     `remotes/<remote>/heads/<name>` and `remotes/<remote>/tags/<name>` are the qualified forms
     of a remote reference, with the same rule (see "Remote references" below).
  3. Any other text is looked up as a branch and as a tag. If exactly one exists, it is the
     answer. If none exists, the name is unknown. If both exist, the command fails with an
     error that names both and asks for `heads/<name>` or `tags/<name>`. It does not pick one,
     and it does not warn and continue.

  Git prefers the tag and warns. That was rejected: a warning is no safe signal. Only the
  JSON, the anchor line, and the exit codes are a contract for an agent, and the text of a
  message is for people (ADR-018). The command would have done something other than the caller
  meant. An error costs one more word in a rare case and cannot move a person or an agent to
  the wrong commit. A fixed winner (always the branch, or always the tag) was rejected for the
  same reason, and because the result of a command would change when somebody creates the tag
  later: a name that was clear yesterday must not change its meaning today. A command that needs
  a branch by its nature (`switch NAME`, `switch -c NAME` without a start, `branch -d`) reads no
  tags and has no ambiguity; the rule is for the places that take any revision.

  The prefixes are reserved to keep the rule closed: a branch or a tag whose name starts with
  `heads/` or `tags/` would be unreachable by its plain name, and `heads/x` could mean two
  things. `switch -c`, `branch`, `tag`, and `worktree add -b` refuse such a name for a new branch
  or tag. The core refuses it in `update_branch` (for a new branch), `create_tag`,
  `rename_branch` (the new name), `switch`, and `add_worktree`, so the rule holds for every caller
  and not only for the command line (STEMMA-207). A repository that has one already is not rewritten; the qualified form wins for it
  as well, and `verify` does not call it damage. The store keeps its name rules (the section on the
  name of a branch), so the change touches no ID and no stored file; the rule lives in the layer
  that resolves names, and the ref store (`RefStore`, `FsRefStore`) does not know it (it stores
  and does not decide).
- **Tags are partly known** *(STEMMA-54, STEMMA-183, STEMMA-184, STEMMA-186)*. `verify` checks
  the commits of the tags. A purge refuses (`TaggedCommit`) while a tag points to a commit that
  it would rewrite, because a tag cannot move, unless the person asked it to move the tag
  (`--move-tags`, ADR-014). A restore does not touch tags: they
  are not part of the state it brings back, and a tag that a later entry created stays
  (ADR-013). The purge of a tagged commit is decided (ADR-014, STEMMA-185). The name that is both
  a branch and a tag is decided above.
- **The module stores, and nothing more** *(confirmed)*. The module does not call the
  authorization hook (ADR-002) and does not write the operation log (ADR-011); the layer above
  does, because it knows the actor and the operation. The default branch of a new repository is
  its decision as well. The module has the trait `RefStore` (sealed, like `ObjectStore`) and the
  implementation `FsRefStore`.

- **Remote references** *(decided for Phase 5, STEMMA-56; the layout, the names, and the
  readers are implemented in STEMMA-285, the moves by `fetch` in STEMMA-295, those by `push`
  in STEMMA-309)*. A remote
  reference is the state of a branch or a tag on a hub as the clone last saw it.
  - **Layout.** A namespace of their own, one file per reference:
    `<metadata directory>/refs/remotes/<remote>/heads/<name>` and
    `<metadata directory>/refs/remotes/<remote>/tags/<name>`. Each is a file of two lines in a
    strict form, the commit and the hash of the record entry of that name on the hub (ADR-025).
    They are part of the sealed `RefStore`, so `verify`, the reachability and the plan of a purge
    know them. The new files are a change of the layout (ADR-009).
  - **The file** *(STEMMA-285)*. The first line is the ID of the commit, as in the file of a
    branch. The second is `entry ` and the hash of the record entry, in the text of an object ID
    (`b3:` and 64 lowercase hexadecimal digits), and each line ends with a newline. The hash names
    an entry of the record and no object, so nothing looks it up in the store. The reader accepts
    what the writer writes and nothing else (ADR-005): the file of a branch, with its one line,
    is no remote reference.
  - **Names.** The name of a remote is one component by the rules of a branch name. The qualified
    form is `remotes/<remote>/heads/<name>` and `remotes/<remote>/tags/<name>`, and the prefix
    `remotes/` is reserved for new branch and tag names as `heads/` and `tags/` are.
  - **Resolving and reserving** *(STEMMA-285)*. Every place that takes any revision reads the
    two qualified forms. They never fall through to a branch or a tag: a remote reference that
    the clone does not hold is an unknown name. Text that starts with `remotes/` and has another
    shape (`remotes/origin`, `remotes/origin/main`) is refused when it is read, so it is never
    looked up as a plain name. The same entry points of the core that refuse `heads/` and
    `tags/` refuse `remotes/` for a new branch or tag. A branch or a tag that has such a name
    already is not rewritten and is reached by its own qualified form, such as
    `heads/remotes/x`.
  - **Reading** *(STEMMA-285)*. The store reads one remote reference by its exact name and lists
    all of them, sorted by the remote, then the branches before the tags, then the name. The
    listing is as strict as the one of the branches: an entry below `refs/remotes` whose name
    is no name of a remote, an entry below a remote that is neither `heads` nor `tags`, a name
    below those that is no branch name, and a file that is not in canonical form are each
    `Corrupt` with the path, and a file in the place of a directory is an error of the file
    system. Entries whose name starts with a dot are skipped. A repository without
    `refs/remotes` has no remote reference. The lookup of one reference agrees with the listing
    about a file in the place of the directory of the remote or of its `heads` or `tags`: it is
    an error and no missing reference. Only `fetch`, `push`, and the restore of their entries
    write one (STEMMA-295, STEMMA-309).
  - **Tags stay in the namespace of the remote.** A local tag arises only through `tag` with its
    own log entry, because a tag never moves. A fetch creates no local tag.
  - **Only `fetch` and `push` move a remote reference**, with one log entry each (ADR-011). A
    fetch writes one entry of the operation `fetch` with all remote references it moves (old and
    new state, commit and record entry) and moves them then. A remote reference that the list of
    the hub no longer names is removed in the same entry, because the list is always complete,
    and `op restore` brings it back. A push writes, after the receipt, one entry `push` with the
    remote references it moved and the number and hash of the receipt. A stale push (`Stale`)
    moves no remote reference: the clone may not have the new commit (`MissingCommit`), and only
    a fetch moves remote references, after the connectivity check (ADR-023).
    *Built in STEMMA-309*: `Repository::push` takes `previous` and `old` of each move from the
    remote reference of the same name, checks the receipt against the update and the pinned key
    of the hub, and only then, under the lock, writes the entry and moves each remote reference
    to the commit of the move and the hash of the receipt as its record entry. A remote
    reference that a fetch moved in between keeps the later state. The merge of a pull request
    in a repository whose `main` follows a remote moves the local `main` in the same entry
    `push` (ADR-025).
  - **`remote remove` deletes them** *(STEMMA-305)*. The removal of a remote writes the entry
    `remote-remove` (ADR-011) with the configuration and with every remote reference of the
    remote, its commit and its record entry, and deletes them after it. It moves none: the
    references go with the remote, and a later `remote add` and `fetch` bring them back. A
    fetch reads the configuration once more under the lock and refuses to move anything if it
    was removed or changed during the transfer (`FetchError::RemoteChanged`, code 6), so a
    removal is not undone by a fetch that ran beside it. Nor by a restore: `op restore` refuses
    an entry that moved remote references of a remote removed or added again after it
    (`RestoreError::GoneRemote`), as it refuses one of a worktree removed since.
  - **Which local branch follows** *(proposed by STEMMA-305, confirmed in its review)*. `clone`
    makes the branch `main` at `remotes/<remote>/heads/main` after its fetch, with the files of
    its tree, by the mechanism of `switch -c`; a hub without `main` leaves the branch without a
    commit. No other local branch is made, and no tag. A later `fetch` moves remote references
    only, never a local branch: the actor brings a branch along with `rebase
    remotes/<remote>/heads/main` or a merge, each with its own entry. No mapping of a branch to
    a remote reference is stored. Rejected: an upstream per branch that `fetch` follows (a fetch
    would move the working copy of an actor who did not ask, and it would need a merge on the
    way), and a clone without a local branch (a clone that shows no files).
  - **The fetch as built** *(STEMMA-295)*. `Repository::fetch` and `Repository::clone_remote`
    speak through `Transport` (ADR-022). The fetch reads the list, compares its repository ID
    with the one of the configuration of the remote and refuses another
    (`FetchError::OtherRepository`, nothing moves), wants every listed commit that is no
    `have`, and checks every listed commit for connectivity before it takes the lock. Under the
    lock it writes the entry `fetch` with every remote reference of the remote whose state
    changes, the commit or only the record entry, and moves them, the deletions first, so that
    `a` gives way to `a/b`, `a/b` to `a`, `main` to `Main`, and `Feature/x` to `feature/x`. A
    name below `refs/remotes` that is in the way and that the fetch does not remove is
    `RefError::RemoteConflict`, and so are two listed names of which one lies below the other,
    that differ in case alone, or whose directories do (`Feature/x` and `feature/y`): one file
    system cannot hold both, so the list is refused before the entry. The names to
    remove are read under the lock. A purge entry or a stub after the start of
    the fetch makes it check the listed commits once more under the lock (ADR-023). A fetch
    whose references have their state already writes no entry. A clone reads the list first,
    then runs `init`, writes the configuration with the address and the ID of the list, and
    fetches with that list; it makes no local branch. `op restore` of a `fetch` entry puts
    the old states back, also a reference that the fetch removed, and writes them into its
    own entry.
  - **Level and target** *(STEMMA-295)*. The code of `fetch` and `clone` is level 2 by
    ADR-004 (network, references, the operation log). At run time the operation asks the
    authorizer for `fetch` with the target `remote <name>`; a clone asks for `fetch` first,
    before it makes anything, and for `init` when it makes the repository.
  - **Remote references are roots of the reachability.** The level and the target for the
    authorizer are set by the slice according to ADR-004. The plan of a purge walks from them as
    it does from a branch (STEMMA-285). A purge neither moves nor removes one, so it is refused
    (`RemoteReference`) while a remote reference names a commit that it would rewrite; what a
    purge does with them in the end is open (ADR-014).
  - **The compare-and-swap on a hub** names the hash of the last record entry of the reference in
    place of the bare commit ID, so an update counts once even when an old value returns
    (ADR-025). The local compare-and-swap is unchanged.
  - Rejected: the layout of Git with tags fetched into the local tags (a fetch would create local
    tags without the operation `tag`, and two values of one reference would lie in two files), one
    file per remote with the whole list (a second form of reference beside the file per branch),
    removal only on request (the clone would show a state of the hub that no longer exists), and
    no log entry (against ADR-011).

## Consequences

- A reader sees a branch or HEAD in canonical form or gets an error with the path: `Corrupt`.
  A file that is cut off, has a second line, or has a name that is not a branch name is damage.
  A path that is not what it should be (HEAD as a directory, `refs` as a file) is an error of the
  file system and not "no such reference". The listing of the branches skips entries whose name
  starts with a dot: no branch name does, and file managers leave such files (`.DS_Store`).
  This holds for a directory as well, with all that is below it, since no component of a branch
  name starts with a dot and nothing below it can be a branch.
  The listing reads without the lock, so another change may delete a branch, or the last branch
  below a directory together with that directory, while it runs. An entry that is gone between
  the listing and the read is no branch and no error, and so is a branch file that a lookup
  found and that is gone when it is read (STEMMA-57). Any other entry stays strict: a file or a
  directory whose name is not a branch name, such as the backup `main~` of an editor, stops the
  listing with `Corrupt`, because a reader accepts only what the writer writes (ADR-005), and
  skipping it would hide damage. The error names the path, so the entry can be removed. A name
  such as `main.orig` is a valid branch name; such a file is listed as a branch if its content
  is a valid reference, and is `Corrupt` otherwise. The check whether a directory holds files,
  after which a change may remove it as empty, counts every doubt as a file: a directory below
  it that was listed and cannot be opened may still hold one. A race in which a directory is
  replaced by a file while it is listed still ends the listing with `Io`; the next one succeeds.
- `update_branch` and `set_head` refuse a commit that is not in the object store
  (`MissingCommit`) or is another object (`NotACommit`). A failure of the file system in that
  check is an `Io`, as everywhere in the module.
- A change that loses a race fails with `Stale` or `Locked` and changes nothing. The caller
  reads again and decides; the module does not retry.
- Listing the branches walks the directory, and a lookup reads the listing of each directory on
  the path of the name, to compare the case that is stored. That costs a few directory reads per
  call and is the price of exact names on file systems that ignore case. Measured on 2026-11-04
  (STEMMA-55; release build, APFS, one machine, state of the cache not controlled; a lookup is
  the mean of 200 calls, a listing is one run; `FsRefStore` over branch files written directly):

  | Branches | Lookup, flat names | Lookup, nested names | List, flat | List, nested |
  |---|---|---|---|---|
  | 1,000 | 0.34 ms | 0.14 ms | 14 ms | 34 ms |
  | 10,000 | 3.2 ms | 0.16 ms | 0.23 s | 0.30 s |
  | 100,000 | 87 ms | 1.6 ms | 21.8 s | 26.4 s |

  Flat names are `feat-TF-<n>`, nested names `feat/g<n mod 100>/b<n>`. Up to 10,000 branches the
  cost is small. At 100,000 it is noticeable. A lookup reads every entry of each directory on the
  path, and the listing reads every branch file. From 10,000 to 100,000 branches the time grows
  faster than the count: a flat lookup 27 times, a listing about 95 times (flat) and 88 times
  (nested). The code does a constant amount of work per entry plus one sort, and a control run of
  the same system calls in a script grew linearly, so the cause is not known. It may lie in the
  file system, in the state of the cache, or in the measurement. The factors are the result of one
  run on one machine and no property of the design.
- The names of `con` and its kind are refused on every platform, also for people who never use
  Windows.
- `FsRefStore::verify` (and `Repository::verify_refs`) checks, without the lock, that a detached
  HEAD, the working-copy commit, every branch, every tag, and every remote reference name a
  commit that the store has. The result lists the references in this order, the tags after the
  branches and the remote references last. Of a remote reference the commit is checked; its
  record entry names nothing in the store. A write
  checks this already, so a failure means that an object was removed or damaged, or that a
  reference file was edited by hand. The check is shallow: the commit is read and its hash
  checked, but it is not parsed, and its tree and parents are not followed. The result lists
  every reference that fails, as a `VerifyError` with the pointer as its place: a missing
  commit, another kind of object, a damaged one, or one that cannot be read. A failure of the
  storage is listed for each reference as well, as for a tree, because it may concern one
  object only. A pointer that fails is read again after the lookup and checked at its new
  commit if it moved, so a purge that moves a pointer and removes its old commit meanwhile
  reports nothing. A reference that cannot be read itself (a missing HEAD or working-copy
  file, a file not in canonical form) stops the check with its `RefError`. While the journal
  of a purge exists, before or after the check, it refuses with `PurgePending` (ADR-014). HEAD
  that names a branch which does not exist yet is no failure.

## Open items

- [x] Deleting a branch: `delete_branch(name, expected)` with the same compare-and-swap
      (STEMMA-52); a missing branch or another tip is `Stale`, a name held by another entry is
      `Conflict`. The rule that the branch of HEAD is not deleted belongs to the layer above: it is
      `Repository::delete_branch`, with the entry of the operation `branch` and the command
      `stemma branch -d` (STEMMA-166), see the decision above.
- [x] Packed references, when there are many branches. Measured (STEMMA-55, see Consequences):
      the cost is small up to 10,000 branches and noticeable only at 100,000, mainly in the
      listing. The listing is used by `branch`, by the check of the references, by the plan of a
      purge, and in the rare conflict of a branch with a directory; commit, switch, and the other
      commands only look up single names, and a lookup in nested names takes 1.6 ms at 100,000.
      No packed form is built. Decide again if a repository with about 100,000 branches shows up;
      then first find out why the time grows faster than the count (file system, cache, or
      measurement; time the phases and repeat warm and cold).
- [x] Tags: layout, plain pointer, never moves (STEMMA-54), see the decision above. The store
      has `tag`, `tags`, and `create_tag`, and `verify` and the purge know them. The operation
      `tag` in the operation log (ADR-011), its level (ADR-004), and the command `stemma tag`
      are done (STEMMA-183), and so is deleting a tag, `delete_tag` and `stemma tag --delete`
      with the operation `untag` (STEMMA-184). The name that is both a branch and a tag is an
      error that asks for `heads/<name>` or `tags/<name>` (STEMMA-186), see the decision above.
      A purge that would rewrite the commit of a tag refuses by default, and `--move-tags` lets it
      move the tag (ADR-014).
- [x] The first command that resolves a name to a tag implements the rule for names
      (STEMMA-186, STEMMA-198): the two prefixes, the error for an ambiguous name, and the
      refusal of new names that start with `heads/` or `tags/`. The first command is
      `switch --detach`; the rule lives in `repository/revision.rs`, and `switch -c` and `rebase`
      reuse it (STEMMA-208). The refusal of new names is enforced in the entry
      points of the core (STEMMA-207).
- [x] Checking that the branches, tags, and HEAD of a repository point to commits that the store
      has
      (as `Tree::verify` does for a tree): `FsRefStore::verify` (STEMMA-50), see Consequences.
- [x] A compare-and-swap for HEAD (`replace_head`, `Repository::set_head` with `expected`,
      `StaleHead`, STEMMA-53), see the decision above. `Expected { Absent, At(commit) }` in place
      of `Option` for `update_branch` is not made. `stemma branch` is the one command that creates
      a branch, and it does so through `create_branch`, which fixes `None`; no caller has misread
      it, so the item is closed (STEMMA-166).
- [x] A lock that a crashed process left behind: the guard of the lock (STEMMA-51), see the
      decision above.
- [ ] The behavior of the replacing rename on Windows is tested only by the CI workflow run by
      hand (see ADR-007 for the same question about objects).
- [x] The log of changes of a reference (a reflog) is part of the operation log (ADR-005), which
      ADR-011 designs: one entry for each change of a pointer, with the old and the new value.
- [x] Remote references and the record of a ref update on a hub (ADR-003, handed to stemma-hub).
      Decided in STEMMA-56: the layout and the moves of a remote reference are in the decision
      above, the signed ref update and the record of a hub in ADR-025. The files, the names,
      and the readers are built (STEMMA-285), and so are the moves by `fetch` (STEMMA-295)
      and by `push` (STEMMA-309). Open stay
      how a purge treats remote references in a clone (ADR-014; until then it is refused).
      Which local branch follows which remote reference is decided by the command surface
      (STEMMA-305, "Which local branch follows").
