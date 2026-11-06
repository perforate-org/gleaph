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
