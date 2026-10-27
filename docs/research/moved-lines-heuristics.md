# Heuristics for moved lines in diffs

Research note for STEMMA-110. It collects what established diff tools do with
moved lines, and what each approach costs. It feeds the open question in
`docs/chapters/05-phase2-diff.tex` on better hunks for moved lines. Phase 2
must implement Myers diff itself, so the Myers entry carries the most weight.

Sources are primary unless marked otherwise. Git's source is read from
`git/git` on GitHub.

## Myers diff (1986) and its post-processing

Idea: an edit graph where a shortest edit script is a path of minimal
non-diagonal edges. The greedy forward algorithm finds it in O((M+N)D) time,
where D is the size of the minimal script
([Myers 1986](https://publications.mpi-cbg.de/Myers_1986_6330.pdf), abstract
and section 3). The paper also gives a linear-space refinement, an expected
O(N+D^2) bound under a stochastic model, and an O(N lg N + D^2) worst-case
variant with suffix trees. The 2 to 4 times speed-up over System 5 diff is
measured on a VAX 785 and is historical.

A move appears as a deletion and an insertion far apart. Myers is minimal
in edit count and does not favour grouping, so it knows nothing about moves.

Post-processing in git (`xdiff/xdiffi.c`, `xdiff/xprepare.c`):

- Change compaction slides change groups when the first line equals the
  line after the group
  ([git source](https://raw.githubusercontent.com/git/git/master/xdiff/xdiffi.c)).
- The indent heuristic (`--indent-heuristic`, on by default) scores split
  points and slides by at most `INDENT_HEURISTIC_MAX_SLIDING` lines
  ([git docs](https://raw.githubusercontent.com/git/git/master/Documentation/diff-options.adoc)).
- Git's Myers stops at a cost limit `xdl_bogosqrt(ndiags)`, floored at 256,
  and then uses a non-minimal result. `--diff-algorithm=minimal` disables this.
- Cost: the slide and scoring are bounded local passes. The sources give no
  Big-O for them and no measurements.

## Patience diff

Idea, by Bram Cohen: match lines that occur exactly once on each side, take
the LCS of those anchors, and repeat on the gaps between anchors. Trim the
common prefix and suffix first
([Cohen, "Patience Diff Advantages"](https://bramcohen.livejournal.com/73318.html)).
Cohen gives no Big-O or benchmarks. He calls LCS-based implementations
"essentially unmaintainable" and says the recursion was dropped because it
rarely finds more matches.

Cost: the LCS runs only over unique lines, so it is cheaper than a full LCS.
The asymptotic bound is not stated in the source and is open.

Trade-off: anchors on unique lines stop braces and blank lines from
matching across the file. Because the LCS keeps order, a moved block is
not an anchor and still appears as a delete plus an insert.

## Histogram diff (git)

Idea: extend patience to low-occurrence common lines, as the [git docs][algo] say.
Git's `xdiff/xhistogram.c` indexes the first sequence in hash chains with
occurrence counts. For each line of the second sequence it picks the longest
match with the lowest count, skipping chains above the count of the current
best. Then it recurses on both sides. If only too-frequent matches exist, it
falls back to the classic diff. `index.max_chain_length` is 64, and it is
the only explicit cost control in the file. The source has no Big-O.

Trade-off: it handles repetitive code better than patience. It needs the same
fallback path as Myers, so it adds code without removing the Myers
requirement.

## Git `--color-moved` (display only)

Move detection runs on the diff that is already computed. It changes colours
and not hunks
([git docs](https://raw.githubusercontent.com/git/git/master/Documentation/diff-options.adoc)).

- `plain`: any line removed in one place and added elsewhere.
- `blocks`: moved blocks of at least 20 alphanumeric characters, detected greedily.
- `zebra` (default when the option has no mode): as `blocks`, with alternating colours.
- `dimmed-zebra`: as `zebra`, and it dims uninteresting parts of moved code.
- `--color-moved-ws` ignores whitespace during detection.

The docs do not give a cost. The implementation in `diff.c` was not read.

## Difftastic (structural, graph based)

Idea: the diff is a shortest path on a DAG. A vertex pairs a position in the
left syntax tree with one in the right tree. Edges are unchanged nodes, novel
atoms, and novel delimiters. The author states that difftastic uses Dijkstra's
algorithm and that A* gave little gain
([Hughes, 2022](https://www.wilfred.me.uk/blog/2022/09/06/difftastic-the-fantastic-diff/)).
Costs from `src/diff/graph.rs`: unchanged node 1 plus up to 40 for depth,
novel atom 300
([source](https://raw.githubusercontent.com/Wilfred/difftastic/master/src/diff/graph.rs)).

Cost: the author states the graph size is quadratic, O(L*R), and that
vertex construction is the main bottleneck. He builds the graph lazily and
keeps at most two vertices per position pair. The README says it "scales
relatively poorly on files with a large number of changes, and can use a
lot of memory"
([README](https://raw.githubusercontent.com/Wilfred/difftastic/master/README.md)).
Large inputs fall back to a line diff.

Moves: difftastic treats order as important. Reordered items show as changes
([README FAQ](https://raw.githubusercontent.com/Wilfred/difftastic/master/README.md)).
It needs a parser per language.

## Trade-offs for Stemma

- Myers alone: smallest edit count, known cost, no moves. Post-processing
  (sliding, indent scoring) is cheap and local but has no cost bound in
  the sources.
- Patience or histogram as an anchor step: adds an LCS over anchors and
  still needs Myers for gaps. It does not show moves.
- Move marking as a display-only pass (git's model): leaves the hunks
  unchanged, so the merge path (Stufe 2) is unaffected.
- Difftastic-style: needs parsers and a graph search, far beyond Phase 2.

## Open points

- Patience's fallback and the complexity of its LCS step are not in Cohen's
  post. Git's `xpatience.c` was not read.
- Myers' section numbers past section 3 and the stochastic-model proof were
  not checked.
- The histogram tie-breaking and the exact fallback path were read from
  comments, not run.
- Cost of `--color-moved` in `diff.c` is unknown.
- Difftastic's priority queue, stopping rule, and graph-size threshold are
  not in the fetched excerpts.
- No runtime measurements of any heuristic were found in the primary sources.

[algo]: https://raw.githubusercontent.com/git/git/master/Documentation/diff-algorithm-option.adoc
