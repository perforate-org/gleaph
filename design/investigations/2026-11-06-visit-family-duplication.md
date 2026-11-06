# 2026-11-06: the Visit family's duplication, measured before it is cut

ADR 0100 lists Visit (row iteration) as the largest remaining operation family: "35 public operations and 44
storage-mode branches in `traverse.rs`". This is the measurement behind a cut, and the one wrong cut it already
ruled out.

## What the numbers are now

| Measure | Count |
| --- | --- |
| Entry points (`pub`/`pub(crate) fn` in `traverse.rs`) | 55 |
| `visit_*` entry points | 20 |
| `BucketMode::` branch sites | 34 |
| Functions that own a mode dispatch | 8 |

The dispatch is not spread evenly:

| Function | Mode branches |
| --- | --- |
| `visit_edges_at_with_inline_property` | 8 |
| `visit_slab_bucket_span_edges` | 4 |
| `visit_edges_window` | 4 |
| `visit_edges` | 4 |
| `visit_edges_for_label_impl` | 4 |
| `visit_out_inline_property_batches_for_label_next` | 3 |
| `visit_out_edge_inline_property_batches_for_label_next` | 3 |
| `visit_edges_with_inline_property_impl` | 3 |

Two of them are already funnels: `visit_edge_slots` and the slot-only readers delegate to `visit_edges` /
`visit_edges_with_inline_property` rather than reading storage themselves, so the valueless family has **two**
dispatchers, not twenty.

## The cut that looked obvious and is wrong

`visit_edges` and `visit_edges_window` both dispatch tiny / slab / tree, and their tiny and slab arms are
identical. The natural consolidation is "`visit_edges` is `visit_edges_window` with an unbounded window" -
one dispatcher instead of two.

Row iteration has four contracts that the two differ on, and the tree arm is where they show:

1. **Corruption propagation.** The windowed tree walk validates the blocks it skips *inside* the window
   (`property_tree`, `tree_visit_break_stops_topology_reads`, `point_state_tree_native_boundaries_and_errors`).
   The unbounded walk does not have a window to validate against.
2. **Prefetch and Break ordering.** Both stop the callback on `Break`, but the windowed walk has already
   decided which leaf blocks to open; the unbounded walk opens them one at a time.
3. **Tombstone positions.** Both yield tombstone-inclusive positions, but the windowed walk's "skipped positions
   are entirely inside the window" rule changes which ones it visits at the edges.
4. **The window itself.** `window.is_empty()` is a cheap early answer the unbounded walk does not need.

Measured, not assumed: making `visit_edges` delegate to the windowed walk with `TraversalWindow::unbounded()`
compiles, and then three tests fail - `tombstoned_tree_bucket_visit_preserves_logical_positions`,
`tree_visit_break_stops_topology_reads`, and
`point_state_tree_native_boundaries_and_errors_preserve_all_memories`. The two entries are two operations
because their guarantees differ; a view that merged them would have to weaken one.

That is the ADR 0100 criterion applied honestly: duplication is *one operation written twice*, not *two
operations that look alike*. These look alike and are not.

## Where the duplication that remains actually is

The **with-values** family, because it repeats the whole dispatch instead of delegating:

- `visit_edges_at_with_inline_property` (8 branches) implements slot-selected iteration with values. Its tiny
  arm reuses the valueless selected topology (width 0), its tree arm reads values per selected slot, and its
  slab arm has to build the selected-slot value batch. None of that is shared with
  `visit_edges_with_inline_property_impl` (3 branches), which walks a whole row.
- `visit_edges_from_slot_with_inline_property` is a third entry over the same question.

The cut, when it is made, is a view whose one entry takes the *request* (slot selection **or** window, values on
or off, order) and owns the mode dispatch once; the current entries become adapters. That is a real change of
shape, not a delegation, which is why it wants its own session with the four contracts above written down as
the tests that must keep passing.

## What this record changes

- It removes the tempting wrong cut, with evidence.
- It leaves the number to beat: **8 functions owning a mode dispatch; 3 of them belong to the with-values
  family.** A cut that does not reduce that count has not cut the duplication.

## Addendum: the contract that could unify them, and the two places the code departs from it

The windowed and unbounded tree walks *look* like one operation with different ranges, and there is a contract
that would make them one:

> **Visit the tombstone-inclusive positions in a slot range `[lo, hi)`, in the requested order. Resolve a block
> when you reach it, validate its header when you scan it, yield the positions inside the range, and stop
> touching storage the moment the callback breaks.**

Unbounded is `[0, stored)`. Under that contract both entries are one walker with one dispatcher. Two departures
stop it today, and both are in the **windowed** walk:

1. **A corrupt root entry trips an assertion instead of failing closed.** The walker reads a block header
   straight from the store, whose contract is "never read unbacked ids" - so a root entry naming a block past
   the tail panics in debug and reads garbage in release, where the unbounded walk returns
   `LtbBlock(NotMinted { id })`. **Fixed** (`traverse.rs`, both order arms now check the id against
   `ltb().tail_next()` and return the typed error).
2. **A break does not stop the block it lands in.** The windowed walk hands `scan_tree_block_range_desc` a
   closure that records the break value but returns unit, so the scan finishes the whole block and only then
   stops. The unbounded walk stops at the position. That is a real difference in the guarantee "no read after
   Break", and it is why the unbounded entry cannot simply delegate: the poison in
   `tree_visit_break_stops_topology_reads` sits in the next block's root entry, which the windowed walk reaches
   while finishing the current block.

The remaining work is therefore item 2: let `scan_tree_block_range_desc` / `_asc` carry a `ControlFlow` out of
the callback so the scan stops at the position. With that, the contract above holds for both, and `visit_edges`
becomes `visit_edges_window` with an unbounded window - one walker, and one entry fewer.

The measurement to beat is unchanged: **8 functions owning a mode dispatch; 3 belong to the with-values family.**

### The third departure, and why it is the one that stays

Item 2 is now done: `scan_tree_block_range` / `_desc` carry a `ControlFlow` out of the callback, so the windowed
walk stops at the break position instead of finishing the block it landed in. That is a strict improvement -
"no read after Break" now holds at the position in both entries - and `visit_edges` still cannot delegate,
because a third difference remains and it is **semantic**:

| Entry | Rows it reports |
| --- | --- |
| `visit_edges` (whole row) | tombstone-inclusive **positions**: a deleted slot is reported as the sentinel edge it holds (ADR 0088 §2) |
| `visit_edges_window` (a window) | live rows only, at their positions |

Both are asserted, by name:

- `tree_visit_break_stops_topology_reads` removes the row at the break slot and expects the walk to report it
  as `EDGE_TOMBSTONE_SENTINEL`; the break lands on a tombstone, so a walk that skipped tombstones would not
  break there at all.
- `visit_edges_window_tree_matches_slab_tombstone_positions` expects a window over the same bucket to report
  `[(1, 101), (3, 103)]` - the hole at 2 absent - and that is what the slab window reports too, which is the
  point of the test: a window names the same live rows whichever storage class holds them.

So the difference is **what a window means**, not how it is implemented: pagination wants live rows, topology
wants positions. Making them one entry would have to change one of those contracts - and the slab window's, to
keep the storage classes interchangeable.

That is the answer to "is there a simpler contract to unify under": there is for the *walk* (a slot range,
lazy resolution, validate on scan, stop at break - all now true for both), and there is not for the *reporting*.
The two entries are one walker with two row-visibility contracts, and the duplication that remains is the
with-values family, which repeats the dispatch rather than sharing this one.

## Resolution: the difference is row visibility, and it is now a type

The tombstone question was not a contract between a window and a whole row - it was **inconsistent between
storage classes under the same entry**. `visit_edges` on a tree bucket reported the holes at their positions; on
a slab bucket the row scan hid them. Nothing in either name or signature said which you would get, and three
tests happened to encode the tree answer while every slab reader relied on the other.

Two callers want two answers for a real reason:

| Purpose | Needs | Examples |
| --- | --- | --- |
| Reading | live rows only: a tombstone is not a row | pagination, scans, a caller that wants edges |
| Mutation / topology | every position, tombstones included | removal, compaction, tombstone reuse, a rewrite that has to keep positions |

So the choice is now a value, `lara::edge::RowVisibility { LiveRows, Positions }`, threaded through the row scan
(where the tombstone is still skipped at the source for `LiveRows`), and the graph exposes the two purposes as
two entries:

| Entry | Contract |
| --- | --- |
| `visit_edges` | whole row, **live rows only** (the read shape) |
| `visit_edge_positions` | whole row, **every position including tombstones** (the mutation shape) |
| `visit_edges_with_visibility` | the one walker both call, for a caller that has the choice as a value |

`visit_edges` is uniform now: a slab bucket and a tree bucket answer the same rows under it. The three tests that
encoded the tree answer - `tree_visit_break_stops_topology_reads`,
`tombstoned_tree_bucket_visit_preserves_logical_positions`, and the native-visitor half of
`point_state_tree_native_boundaries_and_errors_preserve_all_memories` - now ask for `visit_edge_positions`,
which is what they were actually testing: that mutations see the holes.

The windowed entry keeps its own contract deliberately: a window is a page over live rows, and
`visit_edges_window_tree_matches_slab_tombstone_positions` asserts that both storage classes agree on it. The
unification that a single walker would buy is therefore not blocked by the walk any more - it is blocked by
`visit_edge_positions` having no windowed form yet. That is the next slice: the tree window walker yields
tombstones when asked, `visit_rows(owner, label, order, selection, visibility)` becomes the only dispatcher, and
`visit_edges` / `visit_edge_positions` / `visit_edges_window` become adapters over it.

## Done: one dispatcher, four shapes

`visit_rows(owner, label, order, selection, visibility)` is now the only walker that chooses a storage class for
whole-row and windowed iteration. The three public entries are adapters over it:

| Entry | Selection | Visibility |
| --- | --- | --- |
| `visit_edges` | `WholeRow` | `LiveRows` (read) |
| `visit_edge_positions` | `WholeRow` | `Positions` (mutation) |
| `visit_edges_window` | `Window(..)` | `LiveRows` (page) |

Measured after the change: the functions owning a `BucketMode` dispatch went from **8 to 7**, and the two that
merged (`visit_edges`, `visit_edges_window`) were the pair this record started from. Three properties made the
merge safe rather than hopeful:

1. The block scan carries a `ControlFlow` out, so a break stops at the position (elsewhere in this record).
2. A corrupt root fails closed as the same typed error in both (elsewhere in this record).
3. Row visibility is a parameter, and the tree scanners report a hole when asked - which is what let the whole-row
   `Positions` walk keep its answer while the windowed `LiveRows` walk kept its own.

What is left of the family's duplication is the with-values paths: `visit_edges_at_with_inline_property` (8 mode
branches), `visit_edges_with_inline_property_impl` (3), and the two batch cursors (3 each). They are the same cut
one level up - a request that says selection, visibility and whether values are attached - and the count to beat
is 7.
