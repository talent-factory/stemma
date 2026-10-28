# Merge base selection in criss-cross histories

Research note for STEMMA-150, part of the map STEMMA-148 (phase 3, branches and merge). It
collects what Git (merge strategy `ort`) and Jujutsu (`jj`) do when two commits have more than
one best common ancestor, and what each approach costs. It decides nothing for Stemma.

Sources are primary: the documentation and source code of both projects. Line numbers refer to
these revisions, read on 2026-10-28:

- Git: `git/git`, branch `master`, commit `8d1b215c01512b9658d4525c54ff9b9245f9a7e8`.
- Jujutsu: `jj-vcs/jj`, branch `main`, commit `c3bf0b1990057493513e88310e6adfeac589db34`.

Raw URLs below point to the branch tip, so line numbers may drift. Where the sources give no
number, this note says so and does not estimate.

## 1. The problem: several best common ancestors

Git defines the term. A common ancestor is better than another if the latter is an ancestor of
the former. A best common ancestor has no better one. With criss-cross merges, two commits can
have more than one best common ancestor, and none is better than the other
([git-merge-base](https://raw.githubusercontent.com/git/git/master/Documentation/git-merge-base.adoc)).
The documentation draws the topology: two branches that each merge the other's tip, so that
commits `1` and `2` are both merge bases of `A` and `B`. Without `--all`, `git merge-base`
prints an unspecified one of them
([same page](https://raw.githubusercontent.com/git/git/master/Documentation/git-merge-base.adoc)).

A three-way merge needs exactly one base. The two systems differ in how they reach one.

## 2. Git, merge strategy `ort`

### 2.1 Finding the candidates

`git merge-base --all` and the merge machinery use `get_merge_bases_many_0()` in
`commit-reach.c`
([source](https://raw.githubusercontent.com/git/git/master/commit-reach.c)):

1. `paint_down_to_common()` walks the graph from both sides. It uses a priority queue ordered by
   generation number, ties by commit date. A commit that has received both paints is a
   candidate and becomes `STALE`, so that its ancestors are known to be redundant
   ([paint-down-to-common](https://raw.githubusercontent.com/git/git/master/Documentation/technical/paint-down-to-common.adoc)).
2. Candidates can still contain commits that are ancestors of other candidates. When more than
   one result remains, `remove_redundant()` filters them
   (`commit-reach.c`, `get_merge_bases_many_0`, lines 568 to 615).
3. `remove_redundant_no_gen()` does this with one `paint_down_to_common()` call per remaining
   candidate against all other candidates (`commit-reach.c`, lines 360 to 400). A variant,
   `remove_redundant_with_gen()`, exists for repositories with generation numbers; it was not
   read in detail.
4. The result is sorted by commit date (`commit_list_sort_by_date`, line 608).

The documentation says the walk may stop early when a stale queue remains, because
`remove_redundant()` fixes the candidates afterwards
([paint-down-to-common](https://raw.githubusercontent.com/git/git/master/Documentation/technical/paint-down-to-common.adoc)).
It gives no Big-O bound for the walk or for the filtering.

### 2.2 What `ort` does with several bases: a virtual base by recursive merging

The strategy documentation states it: when more than one common ancestor can be used, `ort`
"creates a merged tree of the common ancestors and uses that as the reference tree for the
3-way merge". It reports fewer conflicts without mismerges, measured on Linux 2.6 merge
commits
([merge-strategies](https://raw.githubusercontent.com/git/git/master/Documentation/merge-strategies.adoc)).

The implementation is `merge_ort_internal()` in `merge-ort.c`, lines 5333 to 5412
([source](https://raw.githubusercontent.com/git/git/master/merge-ort.c)):

- The caller may pass the bases. Otherwise `repo_get_merge_bases()` computes them and the list
  is reversed (lines 5345 to 5353).
- `merge-ort.h` documents the order: "empirically, the recursive algorithm will perform better
  if you pass the merge_bases in the order of oldest commit to the newest", and cites commit
  `8918b0c9c2` of 2006
  ([merge-ort.h](https://raw.githubusercontent.com/git/git/master/merge-ort.h), lines 105 to
  112).
- The first base is popped. Each further base is merged into the running result. The call
  `merge_ort_internal(opt, NULL, prev, next, result)` recurses with `call_depth` increased by
  one (lines 5375 to 5397). The labels of the inner merge are "Temporary merge branch 1" and
  "Temporary merge branch 2".
- After each inner merge, `make_virtual_commit()` wraps the result tree in a commit that is not
  stored in the object database. Its two parents are set to `prev` and `next`, so that the
  next recursion can again compute merge bases from it (lines 5398 to 5405; the function is at
  lines 5028 to 5038).
- With no common ancestor at all, the base is the empty tree, labelled "empty tree" (lines
  5356 to 5363). With several, the base label is "merged common ancestors" (line 5365).
- The final step is a normal three-way merge of the two tips against the merged base tree
  (`merge_ort_nonrecursive_internal`, lines 5408 to 5412).

How conflicts inside the virtual base are handled, as far as the source shows:

- The inner merge does not stop on conflicts. The code comment says that the result "contains
  files with conflict markers" and that the cleanness flag is ignored, because the committed
  conflicts "were already resolved" (lines 5385 to 5392). So a conflicted inner merge becomes
  part of the base as text with conflict markers.
- Content merges at `call_depth > 0` set `ll_opts.virtual_ancestor = 1` and
  `ll_opts.variant = 0` (lines 2133 to 2135). What `virtual_ancestor` changes inside the
  low-level merge was not read in this research.
- Some conflict kinds fall back to the original base: for two different item types at one
  path the virtual merge "just use[s] the version from the merge base" (lines 4234 to 4239),
  and a conflicting symlink takes the base mode and object (lines 2306 to 2310).
- A source comment admits a known deviation: at `call_depth` with a conflicting file mode the
  result should not match either side, but it does (FIXME at lines 2224 to 2236).

### 2.3 What it costs

The sources state the structure of the cost, not measured numbers.

- Merge count: for `k` best common ancestors, `ort` runs `k - 1` inner merges before the final
  merge. Each inner merge can itself find several bases and recurse (lines 5375 to 5397).
  The function shown has no depth limit; whether another part of Git bounds the recursion
  was not checked.
- Each inner merge is a full tree merge with rename detection, as the strategy documentation
  says that `ort` "can detect and handle merges involving renames"
  ([merge-strategies](https://raw.githubusercontent.com/git/git/master/Documentation/merge-strategies.adoc)).
  `merge_ort_internal` reinitialises the internal state after each inner merge
  (`clear_or_reinit_internal_opts`, line 5406).
- Candidate filtering: one graph walk per candidate (section 2.1).
- Quality: the base can contain conflict markers, which the final merge may then conflict
  with, and the order of the bases matters empirically (`merge-ort.h`).
- Determinism: the base list is sorted by commit date, so the choice depends on timestamps
  (`commit_list_sort_by_date`, line 608). `git merge-base` without `--all` leaves the choice
  unspecified.

## 3. Jujutsu

### 3.1 Finding the candidates

`CompositeIndex::common_ancestors_pos()` in `lib/src/default_index/composite.rs` (lines 413 to
445)
([source](https://raw.githubusercontent.com/jj-vcs/jj/main/lib/src/default_index/composite.rs)):

- Two binary heaps hold the index positions of the two sets. The larger position is replaced
  by its parents until both heaps show the same position. Equal positions go to the result.
- The result is then reduced by `heads_pos()` to its heads. The doc comment says it "computes
  the greatest common ancestors" and returns positions in descending order.
- `heads_pos()` uses the generation numbers of the candidates (line 491). The number of
  steps is not bounded or stated in the source.

### 3.2 What jj does with several bases: sum algebra, no virtual commit

`find_recursive_merge_commits()` in `core/src/commit.rs` (lines 312 to 385)
([source](https://raw.githubusercontent.com/jj-vcs/jj/main/core/src/commit.rs)):

- The input is the list of commits to merge, for example the parents of a merge commit. The
  result is a `Merge<CommitId>`: an alternating list of "adds" and "removes", in the form
  `add0 - remove0 + add1 ...`.
- For position `pos`, it asks the index for the common ancestors of `commits[0..pos]` and
  `commits[pos]`. If that yields one commit, it is the base. If it yields none, the root
  commit is the base (`maybe_resolved`, line 345). If it yields several, the function recurses
  on them.
- The pieces are combined as `Merge::from_vec(vec![result, ancestor, other]).flatten()` (line
  339). The comment above the loop gives the recursive form (lines 350 to 359). An explicit
  stack replaces the call stack (line 350).
- `Merge::flatten()` turns a merge of merges into one flat merge. The docs show that a 3-way
  merge of 3-way merges becomes a 9-way merge (`core/src/merge.rs`, doc comment of
  `flatten`, lines 674 to 693)
  ([source](https://raw.githubusercontent.com/jj-vcs/jj/main/core/src/merge.rs)).
- `merge_commit_trees_no_resolve()` maps the commit-id merge to a merge of trees and calls
  `MergedTree::merge_no_resolve()` once (`core/src/commit.rs`, lines 290 to 310). No
  intermediate tree is merged, and no virtual commit is created. `merge_commit_trees()` then
  calls `.resolve()` (lines 274 to 286).

The data model is the one of first-class conflicts: a merge is an odd-numbered list of trees
`A+(C-B)+(E-D)`, and a three-way merge is `A+(C-B)`. A nested conflict is inserted into the
expression, then `Merge::flatten()` and `Merge::simplify()` remove cancelling terms
([technical/conflicts](https://raw.githubusercontent.com/jj-vcs/jj/main/docs/technical/conflicts.md)).
`simplify()` "join[s] diffs like A->B and B->C into A->C" and drops trivial diffs
(`core/src/merge.rs`, around line 397).

The user documentation claims that criss-cross and octopus merges "become trivial
(implementation-wise)" and that some cases that Git cannot handle, or that give nested conflict
markers, are resolved automatically
([conflicts](https://raw.githubusercontent.com/jj-vcs/jj/main/docs/conflicts.md), the list of
benefits). It is a project claim; the repository text gives no comparison data.

### 3.3 What it costs

The sources state the following; none gives a measurement.

- Expression size: the number of terms grows with every nested ancestor merge. The `flatten`
  documentation shows only one example, a 3-way merge of 3-way merges that becomes a 9-way
  merge. A general formula is not stated in the sources read.
- Tree work happens once, on the flat expression, and is lazy: only sub-trees that cannot be
  resolved by object id are descended into, and only paths that differ are checked
  ([technical/conflicts](https://raw.githubusercontent.com/jj-vcs/jj/main/docs/technical/conflicts.md),
  data model).
- No intermediate resolution step exists, so no marker text enters a base. A conflict in the
  result keeps the alternatives as trees. The price is a result with more than three sides,
  which the conflict model, the materialised markers, and the resolution tools must handle.
- The same-change rule resolves automatically when all sides make the same change. The
  project says this is "lossy in terms of conflict algebra" and cites bug #6369
  ([technical/conflicts](https://raw.githubusercontent.com/jj-vcs/jj/main/docs/technical/conflicts.md),
  "Same-change rule"). The setting `merge.same-change` switches between `accept` and `keep`.
- Candidate search: heap walk over index positions plus `heads_pos()`; a recursive call for
  each non-unique ancestor set (section 3.2).

## 4. Side by side

| Aspect                | Git `ort`                                  | Jujutsu                                 |
|-----------------------|--------------------------------------------|-----------------------------------------|
| Candidate search      | paint walk, then `remove_redundant`        | two-heap walk, then `heads_pos`         |
| Several bases         | merge them into a virtual tree, one by one | recurse, combine as sum of diffs        |
| Intermediate result   | tree with possible conflict markers        | none; terms stay in one expression      |
| Virtual commit        | yes, in memory, with two parents           | no                                      |
| Number of tree merges | `k - 1` inner merges plus one final        | one, on the flat expression             |
| No common ancestor    | empty tree as base                         | root commit as base                     |
| Order dependence      | yes, oldest first is better (empirical)    | not stated in the sources read          |
| Result shape          | normal 3-way result                        | possibly a conflict with more sides     |
| Published numbers     | none in the sources read                   | none in the sources read                |

## 5. Open points for the later decision

These are gaps in the primary sources, not conclusions.

- What `virtual_ancestor` does inside Git's low-level merge (`ll-merge.c`, `xdiff/xmerge.c`)
  was not read.
- Whether Git bounds the recursion depth of virtual merges elsewhere was not checked.
- Neither project publishes timings for criss-cross merges in the files read. The only
  quantitative claim is Git's report on Linux 2.6 merge commits.
- The code of `jj` changes quickly; the file layout moved from `lib/src/` to `core/src/` for
  `commit.rs` and `merge.rs`, which is why the commit above is recorded.
