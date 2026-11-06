# How Jujutsu stores conflicts as data

Research note for STEMMA-149 (map STEMMA-148, phase 3: branches and merge). It collects how
Jujutsu (`jj`) represents a conflict inside a commit, the algebra behind that representation,
how a conflict is shown in the working copy, and where the design stops. It records facts only
and decides nothing for Stemma.

## Sources and method

Sources are primary: the `jj` documentation and the `jj` source code, both read from the
repository `jj-vcs/jj` at one pinned revision, so that line numbers stay valid.

- Revision: `c3bf0b1990057493513e88310e6adfeac589db34` (commit date 2026-10-28, `main` on
  the day of the research).
- In the source references below, `core/src/...` and `lib/src/...` are paths in that
  repository, for example
  <https://github.com/jj-vcs/jj/blob/c3bf0b1990057493513e88310e6adfeac589db34/core/src/merge.rs>.
  The abbreviation `R/` stands for the prefix
  `https://github.com/jj-vcs/jj/blob/c3bf0b1990057493513e88310e6adfeac589db34/`.
- The documentation pages are cited from the same revision (`docs/...`); the published form
  lives at <https://docs.jj-vcs.dev/latest/>.

The documentation pages read are:

- D1: `R/docs/technical/conflicts.md` (internals).
- D2: `R/docs/conflicts.md` (user view, marker styles).
- D3: `R/docs/git-compatibility.md`, section "Format mapping details".

Where a statement comes from the source code and not from prose, the code is cited with its
file and line range. A statement not backed by a citation is marked "inference".

## 1. Conflict as data in a commit

### 1.1 One tree becomes an odd-length list of trees

A commit normally points to one root tree. In `jj` the root tree of a commit is a list of tree
ids, and a list of length one is the resolved case.

- The commit struct has the field `root_tree: Merge<TreeId>`, next to
  `conflict_labels: Merge<String>`. The doc comment of the labels says: "If resolved, must be
  empty string. Otherwise, must have same number of terms as `root_tree`."
  (`core/src/backend.rs` lines 194-200)
- D1 states the same in prose: a conflict is "an ordered list of tree objects" that the commit
  links in place of one tree. The count is always odd. The first tree is the start, and each
  following pair is a diff applied to it. Trees A, B, C, D, E mean `A+(C-B)+(E-D)`. A plain
  three-way merge of sides A and C over base B is stored as A, B, C, that is `A+(C-B)`.
  (D1, section "Conflict storage")
- The native (`simple`) backend stores the list as a protobuf field `repeated bytes root_tree`
  with the comment "Alternating positive and negative terms", and the labels as
  `repeated string conflict_labels`. (`lib/src/protos/simple_store.proto` lines 44-48)

There is no separate "conflict object" for a file. The unit of storage is the whole tree.
Every term of the list is a complete, ordinary tree, so the conflict is carried by N ordinary
trees and not by marker text inside a blob.

### 1.2 What each term contains

The terms are full trees of the repository, not only the conflicting paths. The merge is
computed lazily and per path.

- D1: content is computed on demand and only the parts that need merging get merged. Checkout
  looks only at paths that differ from the previously checked-out tree. Listing conflicts
  skips subtrees that resolve trivially, for example a `lib/` changed by only one side. If a
  subtree or file cannot be settled by id alone, the merge recurses into the subtree or into
  the hunks of the file. (D1, section "Conflict storage")
- A tree entry is the enum `TreeValue` with the variants `File { id, executable, copy_id }`,
  `Symlink`, `Tree`, and `GitSubmodule`. The mode of a file (the executable bit) is part of the
  entry, not of the file content. (`core/src/backend.rs` lines 383-403)
- A path inside a conflicted tree has the type `Merge<Option<TreeValue>>` (alias
  `MergedTreeValue`). `None` stands for "the path does not exist in this term", so
  additions, deletions, and file versus directory differences are all expressed with the same
  type. (`core/src/backend.rs`, `MergedTreeValueExt`, lines 430-580;
  `core/src/merge.rs` lines 592-675)

### 1.3 How a conflicted tree list is built from a per-path conflict

When `jj` merges trees and some paths stay conflicted, it writes N trees that all contain the
resolved entries, and the conflicted entries are distributed over the N trees, one term per
tree.

- `MergedTreeInput::into_backend_trees` takes the set of resolved entries and the set of
  conflicted paths. For each term index it builds one backend tree from "the resolved entries
  merged with the entries of that term". If there are no conflicts it returns a single tree.
  (`core/src/tree_merge.rs` lines 108-146)
- A term that is absent at a path (`None`) simply does not put an entry in its tree.
  (same function, `if let Some(value) = value`)
- The function `merge_trees` promises that its result "will either be resolved or have the same
  number of sides as the input". (`core/src/tree_merge.rs` lines 52-54)

Consequence stated by the code: the number of sides of a conflicted tree is global for the
commit. A commit with a three-sided conflict in one file and a two-sided conflict in another
file stores both as terms of the same odd-length list (the smaller conflict is padded with
repeated, cancelling terms; see section 2.3). (inference, from the same-arity guarantee above
and the `assert_eq!(value.num_sides(), ...)` in `into_backend_trees`)

### 1.4 Conflict labels

Each term can carry a human-readable label, for example the rebase terms below. Labels live
in the commit next to the trees, are optional, and must have the same arity as the trees. The
source comments say that conflicts created before labels were added exist without labels
(`core/src/merged_tree.rs` lines 160-165), so the field was added after the tree list.

- `ConflictLabels` wraps a `Merge<String>`; a resolved merge cannot have labels.
  (`core/src/conflict_labels.rs` lines 21-56)
- During a rebase the three terms get the labels "(rebase destination)",
  "(parents of rebased revision)", and "(rebased revision)". (`lib/src/rewrite.rs` lines 266-286)
- Labels are simplified together with the terms, so that a label disappears when its term
  cancels. (`core/src/conflict_labels.rs`, `simplify_with`, lines 105-120)

### 1.5 Mapping to the Git backend

`jj` also stores commits in a Git object database. Git has no notion of a conflict, so the
mapping adds data that plain Git tools can read without interpreting it.

- Authoritative data: a non-standard commit header `jj:trees` holds the tree ids separated by
  blanks; the header `jj:conflict-labels` holds the labels, one per line.
  (D3; `lib/src/git_backend.rs` lines 103-104 and 1358-1377)
- The header is rejected if it holds a single tree or an even number of trees, because a
  resolved tree plus a `jj:trees` header would show different content in `jj` and in `git`.
  (`lib/src/git_backend.rs` lines 654-679)
- To keep the trees from garbage collection, the Git tree of a conflicted commit contains the
  top-level directories `.jjconflict-base-N` (the removes) and `.jjconflict-side-N` (the adds),
  and a file `JJ-CONFLICT-README`. The remaining entries are copied from the first term "which
  prevents editors with Git support from highlighting all files as new".
  (`lib/src/git_backend.rs` lines 1594-1664; D3)
- D3 says the purpose of these directories "is only to prevent GC of the relevant trees".
  Using a plain `git switch` to a conflicted commit puts these directories in the working
  copy, and `jj abandon` is the documented recovery. D3 also says that Git tools "will have
  trouble with revisions that contain conflicted files".
- Older conflicted commits (before March 2024) kept the trees in an extra metadata table, not
  in the header; the reader still supports that form.
  (`lib/src/git_backend.rs` lines 686-692)

## 2. The algebra: a sum of adds and removes

### 2.1 Definition

The type is `Merge<T>` with this contract: "There is exactly one more `adds()` than
`removes()`. When interpreted as a series of diffs, the merge's (i+1)-st add is matched with
the i-th remove. The zeroth add is considered a diff from the non-existent state."
(`core/src/merge.rs` lines 179-189)

- Storage is a flat vector that alternates positive and negative terms, starting with a positive
  one. `from_vec` asserts an odd length. (`core/src/merge.rs` lines 186-216)
- `num_sides = len / 2 + 1`; `is_resolved` means length one. (lines 315-323)
- D1 writes the expression as a sum: a three-way merge is `A+(C-B)`; a term that is itself a
  conflict is inserted in place, and cancelling terms are then removed. D1 names the
  functions `Merge::flatten()` and `Merge::simplify()` in `merge.rs`.

### 2.2 Flatten: conflicts of conflicts stay flat

`Merge<Merge<T>>::flatten` turns a merge whose terms are merges into one merge. The doc
comment gives the example of a 3-way merge of 3-way merges, drawn with adds on the upper
line and removes on the lower line: the outer adds are `(4 5 / 3)` and `(7 8 / 6)`, the
outer remove is `(1 2 / 0)`. Flattening gives the 9-way merge `4 5 0 7 8 / 3 2 1 6`. In the
code, the inner terms of a remove are reversed in pairs and the first element is moved last,
"so we preserve the diffs". (`core/src/merge.rs` lines 676-709)

Flatten is what `MergedTree::merge_no_resolve` uses to merge trees that are already
conflicted: it flattens the tree ids and the labels, then simplifies.
(`core/src/merged_tree.rs` lines 376-396)

### 2.3 Simplify: cancel equal add/remove pairs

`Merge::simplify` "joins diffs like A->B and B->C into A->C" and "drops trivial diffs like
A->A". The algorithm walks the adds; for each add it looks for an equal remove, swaps the add
into the diff of that remove, and drops the pair. (`core/src/merge.rs` lines 345-403)

D1 gives two worked cases:

- A commit B based on A, rebased onto C, is `C+(B-A)`. Rebased again onto D it first becomes
  `D+((C+(B-A))-C)`, which simplifies to `D+(B-A)`, "with no trace of C". This is the reason
  why rebasing an unresolved conflict does not produce nested markers.
- Backing out a conflicted commit `E = C+(B-A)` on top of `C` applies `-(E-C)`. The result
  `E+(C-E)` simplifies to `C`, so there is no conflict.

Simplification compares terms by equality of the stored value (tree or file id), so equal
content cancels without being read. `simplify_by` compares a derived key instead.
(`core/src/merge.rs` lines 405-414)

### 2.4 Trivial resolution and the same-change rule

`trivial_merge` resolves a list without reading file content. It requires an odd length and
has two fast paths (one term; three terms). Otherwise it counts how often each distinct value
occurs, positive terms as +1 and negative terms as -1, drops values with count zero, and
returns the single remaining value if one is left. (`core/src/merge.rs` lines 124-177)

- Three-term case: if the two adds are equal and the same-change rule is `Accept`, the add
  wins; if an add equals the remove, the other add wins; else unresolved. (lines 137-146)
- The same-change rule (`SameChange`) decides what happens when all sides made the same change.
  `Accept` (default) resolves it "as if one side were unchanged (i.e. `A+(A-B)=A`)". This
  "matches what Git and Mercurial do (in the 3-way case at least), but not what Darcs does. It
  means that repeated 3-way merging of multiple trees may give different results depending on
  the order of merging." `Keep` leaves it unresolved. (`core/src/merge.rs` lines 109-122)
- D1: the auto-resolution is lossy in the algebra. Rebasing onto a commit with the same
  changes (or a subset) and then back loses changes (D1 cites bug 6369). It is kept because
  it is friendlier in most cases. Configuration key: `merge.same-change`, values `accept`
  (default) and `keep`. (D1, section "Same-change rule")

### 2.5 Resolution of a whole tree

`MergedTree::resolve` calls `merge_trees` and then simplifies the result again, "to possibly
reduce a complex conflict to a simpler one". A debug assertion checks that merging the
result a second time changes nothing (idempotence). (`core/src/merged_tree.rs` lines 168-193)

The recursion in `merge_trees` (`core/src/tree_merge.rs` lines 225-263):

1. For each name in a directory, try `resolve_trivial`. A resolved entry is kept (or dropped
   if it resolved to `None`).
2. If a non-trivial entry has only trees or absent terms (`is_tree`), recurse into that
   directory with the merge of the subtrees.
3. Otherwise treat it as a file-like entry and call `resolve_file_values`, which attempts a
   content merge of the files.
4. Whatever stays unresolved is recorded as a conflict and distributed into the N output
   trees (section 1.3).

## 3. Materialisation into the working copy

### 3.1 What is written

When a conflicted tree is checked out, each conflicted path is materialised by type
(`MaterializedTreeValue`, `lib/src/conflicts.rs` lines 134-149):

- `FileConflict`: all terms are files or absent. The file is written with conflict markers.
- `OtherConflict`: any other combination (symlink, directory, submodule mixed with files).
  The working copy gets a short text file, not markers: the output of `describe`, for example
  "Conflict:", then lines "Removing file with id ..." and "Adding executable file with id
  ...", with the label in parentheses if there is one. The dummy file is not executable.
  (`lib/src/local_working_copy.rs` lines 2534-2541; `core/src/backend.rs` lines 553-612)

Before writing, the terms are simplified together with their labels, so a conflict that has
shrunk to two sides is shown with two sides. (`lib/src/conflicts.rs` lines 263-287)

### 3.2 Marker syntax

D2 documents the formats; `lib/src/conflicts.rs` implements them.

- Style `diff` (default): the conflict is opened by `<<<<<<<` and closed by `>>>>>>>`. A line
  `%%%%%%%` starts a diff to apply to a snapshot, a line `+++++++` starts a snapshot, and
  `\\\\\\\` lets a label continue on a second line. A many-sided conflict has one snapshot
  and several diffs. (D2, "Conflict markers")
- Style `snapshot`: the full content of each side (`+++++++`) and base (`-------`).
- Style `git`: `diff3`-like markers (`<<<<<<<`, `|||||||`, `=======`, `>>>>>>>`). Only two
  sides are possible; with more sides `jj` falls back to the snapshot style.
  (D2; `lib/src/conflicts.rs` lines 539-561)
- The enum of styles is `Diff`, `DiffExperimental`, `Snapshot`, `Git`.
  (`lib/src/conflicts.rs` lines 303-316)
- Marker length: at least 7 (`MIN_CONFLICT_MARKER_LEN`). If the content already contains lines
  that look like markers of length N, the new markers get length N + 4, so they stay
  unambiguous. (`lib/src/conflicts.rs` lines 60-67, 418-432)
- Missing terminating newline: markers must be on their own lines, so `jj` adds a newline to
  each term that lacks one, omits the newline after the final end marker, and notes
  "(no terminating newline)". (D2; `lib/src/conflicts.rs` lines 69-70, 525-565)
- Line endings: CRLF is used if all non-empty terms use it. (`lib/src/conflicts.rs` lines
  434-449)
- Default labels when a term has none: "base" (one base) or "base #n", and "side #n".
  (`lib/src/conflicts.rs` lines 577-600)

The file-level merge that produces the hunks splits the inputs into line hunks (or finer, per
`FileMergeHunkLevel`), resolves trivial hunks, and leaves a conflict only for the other
hunks. With more than one base it uses the first base for the diffs; the source carries a
TODO that "there might be better heuristics for more than 3 parts".
(`core/src/files.rs` lines 288-340)

### 3.3 Reading the edited file back

On the next snapshot of the working copy, a path that was a file conflict and is a regular
file on disk is parsed back.

- The marker length used when writing is stored per file in the working-copy state
  (`MaterializedConflictData { conflict_marker_len }`), so the parser knows which lines count
  as markers. (`lib/src/local_working_copy.rs` lines 290-293, 2527-2531)
- `update_from_content` simplifies the stored file ids, parses the new content with the arity
  of the simplified conflict, and writes one new blob per term. If the content is unchanged
  from the freshly materialised form, the old conflict is kept so that an unedited file is
  not turned into a "partially resolved" version. If no valid markers remain, or their arity
  does not match, the whole file becomes one resolved blob. (`lib/src/conflicts.rs` lines
  1054-1115)
- Both the `jj` and the `git` marker style are accepted by the parser, whichever style wrote
  the file; the style is detected from the first line of the hunk.
  (`lib/src/conflicts.rs` lines 903-930)
- The parser rejects a hunk as "not a valid conflict" and treats it as resolved content when a
  diff line starts with an unexpected character, or when the number of adds is not the number
  of removes plus one. (`lib/src/conflicts.rs` lines 932-1004)
- Edited markers that change the number of sides are therefore not a way to change the arity
  of a stored conflict; the arity is fixed by the stored trees.
  (inference, from the arity parameter of `parse_conflict`)

An `OtherConflict` placeholder file is not parsed; `jj` keeps the stored conflict until the
user resolves it by changing the path to a real file, symlink or directory. (inference from
`lib/src/local_working_copy.rs` lines 2043-2108, where only `to_file_merge()` is parsed back
and every other case returns `current_tree_values.clone()`)

## 4. Limits

### 4.1 File mode (executable bit)

- A file term is `File { id, executable, copy_id }`, so the mode is separate from the content
  and takes part in the merge as its own `Merge<Option<bool>>`. (`core/src/backend.rs`
  lines 440-447)
- During automatic file resolution the executable bit and the copy id are first resolved with
  `resolve_trivial(SameChange::Accept)`. If a bit cannot be settled that way (for example,
  three different values over the terms), the function returns "unable to determine whether
  the result should be executable", the file stays conflicted, and its content is not merged.
  A source TODO asks "Whether to respect options.same_change to merge executable and
  copy_id".
  (`core/src/tree_merge.rs` lines 397-436)
- For materialisation, `resolve_file_executable` resolves the bits trivially; if the merge
  resolves to "absent" it picks the executable bit of the removes when they agree. The working
  copy writes the conflicted file as executable only when this resolution says so; otherwise
  not executable. (`lib/src/conflicts.rs` lines 289-301;
  `lib/src/local_working_copy.rs` lines 2520-2524)
- The changelog records that conflicts in executable files "can now be resolved just like
  conflicts in" other files (`CHANGELOG.md` line 4774) and that conflicted files are checked
  out as executable if all sides are executable (line 4095); both are earlier steps toward the
  current behaviour.
- The markers carry no information about the mode; a mode conflict is not visible in the
  marker text. (inference: `write_diff_hunks` and the materialisation functions only emit
  file contents)

### 4.2 Directories, symlinks, submodules

- The tree merge recurses only when all terms at a path are trees or absent (`is_tree`).
  `MergedTree::conflicts` states the same: it "recurses into subtrees and yields conflicts in
  those, but only if all sides are trees, so tree/file conflicts will be reported as a single
  conflict, not one for each path in the tree". (`core/src/merged_tree.rs` lines 195-198;
  `core/src/tree_merge.rs` lines 248-259)
- A directory-versus-file conflict is therefore one `Merge<Option<TreeValue>>` whose terms are a
  mix of `Tree` and `File`. It is stored in the N trees like any other conflict (section 1.3)
  and materialised as the descriptive placeholder (section 3.1). A source TODO says that such
  a conflict is still sent to the file-merge step although "there's no need to try to resolve
  it as a file". (`core/src/tree_merge.rs` lines 255-258)
- `try_resolve_file_conflict` returns "cannot merge" as soon as one term is not a file. The
  match arms accept only `Some(TreeValue::File { .. })`, so symlinks, submodules, directories,
  and also absent terms (`None`) are not content-merged there, although the doc comment of
  `resolve_file_values` says it "treats missing files as empty". (`core/src/tree_merge.rs`
  lines 346-349 and 397-406) Absent terms are handled later, in the materialisation, where a
  missing file is read as empty content. (`lib/src/conflicts.rs` lines 116-118)
- Submodules are not checked out: the working copy only creates the directory and prints
  "ignoring git submodule". (`lib/src/local_working_copy.rs` lines 2496-2508)

### 4.3 Binary files

- No part of the conflict code tests for binary content. `try_materialize_file_conflict_value`
  reads every term as bytes (`BString`); the file merge works on byte lines; the markers are
  written into the file. (`lib/src/conflicts.rs` lines 97-130, 263-287) A search of the source
  and of the documentation of this revision for a binary special case in the conflict path
  finds none (searched: `docs/conflicts.md`, `docs/working-copy.md`, `docs/technical/conflicts.md`,
  `lib/src/conflicts.rs`, `core/src/files.rs`).
- A source TODO shows the topic is open: the `contents` field of `MaterializedFileConflictValue`
  has the note "or `Vec<(FileId, Box<dyn Read>)>` so that caller can stop reading when null
  bytes found?". (`lib/src/conflicts.rs` lines 195-198)
- Consequence (inference): a conflicted binary file is materialised as marker text around
  binary bytes, and is resolved only by choosing one whole term or by user tooling. Content merge
  of two different binary versions yields a conflict unless the trivial algebra resolves it
  by id, because equal ids cancel without reading content.

### 4.4 Same-change loss

See section 2.4: auto-resolving identical changes loses information in the algebra and makes
repeated merges order dependent. The setting `merge.same-change = "keep"` avoids it. The
rebase code even notes that it would not need to resolve intermediate parent trees "if the
same-change rule is 'keep'". (`lib/src/rewrite.rs` lines 257-259)

### 4.5 Rebase and merge of conflicted commits

- A rebase never fails because of a conflict. D2: "a rebase that produces a conflict still
  succeeds", and conflicted commits "can be rebased, merged, or backed out further".
- The rebase of a commit onto new parents computes a three-term merge: the new parents' tree
  as the first add, the old parents' tree as the remove, and the commit's own tree as the
  second add (`MergedTree::merge` of `[new_base, old_base, old_tree]`). If a term is itself
  conflicted, its terms are flattened in and the result is simplified. The commit is rewritten
  with the resulting tree list and labels. (`lib/src/rewrite.rs` lines 248-309)
- Shortcut: if the old and new parent trees are equal, the commit's tree is reused without
  merging. (`lib/src/rewrite.rs` lines 248-255)
- The base for a merge commit with several parents is `merge_commit_trees`, which builds a
  recursive merge of the parent commits through their common ancestors
  (`find_recursive_merge_commits`). This is what lets criss-cross and octopus merges reduce to
  the same algebra. (`core/src/commit.rs`, `merge_commit_trees_no_resolve`; D2 "Advantages")
- A conflict that was resolved in a merge commit stays correct under a later rebase, because
  the changes of a merge commit are defined relative to the merge of its parents. (D2)
- Resolving is a normal edit: check out the conflicted commit, edit the file, amend; descendants
  are rebased automatically. There is no "continue" state. (D2)
- Sharing: D2 notes that collaborative resolution is possible but "sharing conflicts is risky
  if others use Git" (D2, "Advantages"; see also section 1.5).

### 4.6 Other limits seen in the code

- Arity is global per tree list (section 1.3). Padding of smaller conflicts with cancelling
  terms is the reason that `try_resolve_file_values` simplifies first: "The values may contain
  trees canceling each other (notably padded absent trees)". (`core/src/tree_merge.rs`
  lines 363-381)
- Conflict labels cannot contain a newline, because the Git header uses it as the separator
  (`assert!` in `lib/src/git_backend.rs` lines 1359-1367).
- Conflict marker length is stored as `u32` (saturating) in the working-copy state.
  (`lib/src/local_working_copy.rs` line 2530)
- A user-edited conflicted file whose markers were damaged (or whose arity differs) is read as
  a fully resolved file without a warning at the algebra level (section 3.3).

## 5. Summary of the facts

1. Representation: a commit stores `Merge<TreeId>`, an odd list of full trees that alternate
   add, remove, add, ...; length one is the resolved case. Labels are an optional list of
   strings of the same length. There is no per-file conflict object and no marker text in the
   object store.
2. Algebra: `A+(B-C)+...` over any value type. `flatten` inserts conflicts inside conflicts,
   `simplify` cancels equal add/remove pairs by value equality, `resolve_trivial` counts terms.
   Rebase of an unresolved conflict simplifies back to a conflict between the original
   terms.
3. Materialisation: per path, derived on demand from the stored terms; markers exist only in the
   working copy and in diffs, and are parsed back on snapshot using a stored marker length.
4. Limits: only files with text-like markers are materialised and parsed; other combinations
   (directory/file, symlink, submodule) become a description file; executable-bit conflicts are
   resolved only if trivial; there is no binary-aware handling; same-change resolution is lossy
   unless switched off; the Git mapping adds directories and headers that plain Git shows as
   noise.
