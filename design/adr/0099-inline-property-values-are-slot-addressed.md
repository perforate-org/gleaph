# 0099. Inline property values are addressed by row slot

Date: 2026-11-03
Status: Proposed
Last revised: 2026-11-03
Anchor timestamp: 2026-11-03 05:42:20 UTC +0000

Related: [ADR 0052](0052-per-label-adjacency-order-and-tombstone-reuse.md) owns the swap-compaction
gate this ADR simplifies; [ADR 0097](0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md)
owns the spill runs the values plane mirrors; [ADR 0098](0098-the-vertex-slice-owns-its-growth-bound.md)
settled where a bucket's slab prefix may end.

## Context

A bucket's edges and its inline property values live in two planes that are addressed differently:

| plane | address | delete |
| --- | --- | --- |
| edge rows | physical slot, `edge_start + i`, tombstones in place | O(1), the row is tombstoned |
| inline values | **live ordinal**, `values_offset + ordinal * width`, kept dense | **O(n)**, the whole trailing suffix is copied one block left |

The values plane is dense by construction: `inline_property_bytes_slab_slots` is a separate counter
from `stored_slots`, and every removal compacts it. `plan_bucket_inline_property_bytes_delete` reads
the trailing suffix into a fresh heap buffer and writes it back one block left, with no threshold, on
every single delete.

Measured (`crates/graph/src/bench/batch_placement.rs`, width 8, deleting a bucket's edges
front-first):

| rows | total | per delete |
| --- | --- | --- |
| 8 | 1.20 M | 150 K |
| 32 | 7.48 M | 234 K |
| 128 | 58.11 M | 454 K |
| 128, width 0 | 15.90 M | 124 K |

The per-delete cost grows with the bucket (roughly `k^1.4` across those points) and the inline case is
3.7x the width-0 case at 128 rows. A hub drained edge by edge pays a quadratic term that has nothing
to do with its rows moving: the rows are only tombstoned, the values are what move.

The same asymmetry appears on the insert side: filling a slab tombstone re-inserts the value at the
hole's live ordinal and **shifts every later value up one slot**
(`try_reuse_unordered_slab_tombstone`), which is the same O(n) work in the opposite direction.

## Problem

Dense ordinal addressing is what forces both moves. An ordinal is a rank, so removing any row but the
last renumbers every later row's value; keeping the plane dense means paying that renumbering at the
removal rather than deferring it.

That denseness also leaks into unrelated contracts: swap-compaction needs values dense to be able to
shift a window (ADR 0052's 2026-11-02 amendment), so a value-bearing bucket is refused the cheap
hole-fill and falls back to left-packing, and `prepare_materialize_publication` exists partly to
restore denseness after a delete.

## Existing Architecture Assessment

The values plane already has the structure a slot-addressed plane needs - it is the addressing rule
that differs:

- **Prefix and run.** `values_offset` plus `inline_property_bytes_slab_slots` bound the slab part;
  beyond it, values live in the bucket's `values_spill_run`, indexed by the same split the edge plane
  uses (`i < prefix` is slab, otherwise run row `i - prefix`).
- **One address function.** Every read and write goes through
  `inline_property_bytes_byte_offset_at_slot(bucket, slot)`, so the addressing rule has a single
  choke point already.
- **Compaction already moves values with rows.** `apply_edge_slot_move_in_bucket` is edge-only today
  because ordinals are preserved, and the swap path shifts a value window; both become one block move
  when values are slot-addressed.

So the change is not a new subsystem: it is redefining one address function and deleting the code
that exists only to maintain denseness.

## Alternatives

**A. Address values by row slot (chosen).** The value of the row at physical slot `s` lives at
`values_offset + (s - first_slot) * width` inside the slab prefix, and in the values run for slots
beyond it - the same split the edge plane uses. A tombstoned row's value is simply never read.

**B. Keep ordinals, compact every N deletes (hysteresis).** Deferring the copy requires a mapping
from ordinal to physical slot, i.e. exactly the structure A introduces, plus the hysteresis state.
Rejected: more machinery for the same result, and the deferred copy still runs.

**C. Store the value inside the edge row.** Delete then becomes O(1) for free, with no values plane at
all. Rejected on storage: the row stride is global, so every edge in every label would grow by the
inline width, and only a minority of labels carry inline values.

**D. Do nothing.** The cost is bounded by a bucket's promotion cap (1024 rows), so this is a
performance defect rather than an availability one - the measurement says ~2-5 % of a message budget
for a worst-case drain. Rejected because the same asymmetry makes hole-reuse O(n) and refuses
swap-compaction to value-bearing buckets, so it is also a capability cost, not only a constant.

## Decision

Values are addressed by row slot, mirroring the edge plane exactly:

1. **One geometry.** The values prefix width *is* the bucket's `stored_slots`; the values run index
   rule *is* the edge run index rule. `inline_property_bytes_slab_slots` is deleted as a field: two
   counters describing one geometry is the shape that produced this defect's sibling (a vertex record
   width disagreeing with its buckets, ADR 0098).
2. **Delete writes no value.** Tombstoning the row is enough; a reader never reads a tombstoned row's
   value. The trailing-suffix copy is deleted.
3. **Hole reuse writes one value block.** Filling a tombstone writes the value at that slot; the
   upward shift is deleted.
4. **Compaction is the only mover.** Any row move moves its value block with it: left-pack, swap,
   rebuild, fold. The swap's window shift collapses to a single block move, so the swap is now legal
   for value-bearing buckets and ADR 0052's value clause is withdrawn.
5. **`prepare_materialize_publication` stops densifying.** It has no denseness to restore; the values
   prefix follows `stored_slots` because it *is* `stored_slots`.

The invariant that replaces denseness, and the one a test must pin: **for every live row, the value at
that row's slot is the value last written to that row.** Stale bytes in abandoned slots are
unreachable by construction, not by care.

## Consequences

- Removal becomes O(1) in the bucket size, and hole reuse becomes O(1) too, which restores the
  premise unordered placement was built on.
- The values and edge planes become one geometry, so prefix/run/split logic exists once instead of
  twice. `inline_property_bytes_read`'s `min(end, prefix)` arithmetic and the separate prefix
  derivation go away.
- Swap-compaction becomes available to value-bearing buckets, removing a gate and the fixture
  caveats that came with it.
- A bucket's values now occupy slots for tombstoned rows too, so a bucket with many deletes holds
  proportionally more value bytes until compaction reclaims them. That is the same trade the edge
  plane already makes.

## Trade-offs

- Every value access changes address, so the read and write paths must all be re-derived together. The
  choke point is one function, but its callers encode assumptions (`ordinals.start`, prefix
  derivation, run split) that have to be revisited rather than mechanically renamed.
- The descriptor loses a field: a persisted layout change, requiring fresh state. The project's
  pre-production rule allows that, and there is no compatibility path to carry.
- The value plane's storage grows by one block per tombstoned row before compaction, which shows up in
  the placement statistics the benches already read.
- A test that only checks "values read back" will not distinguish the two addressings; the property
  test below has to interleave deletes, hole refills and compactions, or it passes under both.

## Migration

No compatibility path: `inline_property_bytes_slab_slots` disappears from the descriptor, so existing
state is not readable and fresh state is required.

The work is **one atomic change**, not a sequence that keeps the suite green at every step. The
first draft of this section listed "addressing only" as a step with the delete-side compaction still
in place, and that step does not exist: if values remain dense (index = live ordinal) then addressing
them by slot reads a neighbour's value the moment a tombstone sits in front of a row, and if they are
addressed by slot then a delete can no longer compact them without moving every later row's value -
so addressing, the removal of compaction, the hole-refill write, and the value move in compaction all
have to change together. The slices below are therefore checkpoints within one change, not
independently shippable steps.

1. **Property test first.** Random inserts, deletes, hole refills and compactions on a value-bearing
   bucket, asserting that every live row reads back the value last written to it. It must pass on the
   current code - that is what makes it a regression net for the rest.
   *Done, and it did not pass:* the update path resolved a value's ordinal from the row's physical
   slot, so after a delete the value landed one ordinal low
   (`GAP-GRAPH-UPDATE-AFTER-DELETE-WRITES-THE-WRONG-ROW`). Resolving the ordinal fixed it
   (`d661eaaf2`), which is a correctness argument for this ADR rather than only a cost one: keeping a
   rank consistent with the rows is exactly what one path failed to do.
2. **One addressing.** Every caller passes a physical slot: the read path, the write path, the
   delete, the hole refill and the compaction moves. `inline_property_bytes_slab_slots` becomes
   `stored_slots` in the same change.
3. **Delete and hole refill stop moving bytes.** The suffix copy and the upward shift are deleted; the
   measurements above are re-run to confirm the O(1) claim.
4. **Compaction moves value blocks** with the rows it moves, and the swap gate's value clause is
   withdrawn with its ADR amendment.
5. **Delete the field** and re-baseline the placement statistics.

Steps 2 to 4 are the atomic part; the property test from step 1 is what says whether the result is
still correct, and the benches from step 3 are what say whether it was worth it.

**The atomicity is measured, not assumed.** Redefining
`inline_property_bytes_slab_slots()` to return `stored_slots()` - the smallest possible first step,
one line, no call sites touched - turns **96 of 708 lara tests red**. That is the expected shape: the
prefix width changes while every path that maintains values still assumes the dense ordinal geometry,
so reads and writes disagree everywhere at once. It is recorded here because it is the argument for
doing the change as one pass rather than as an increment.

**Sizing, measured on the tree before starting:** 9 callers of
`inline_property_bytes_byte_offset_at_slot` (the address choke point), 2 places in `compact.rs` that
copy value bytes, and **112 references to `inline_property_bytes_slab_slots`**. The count looks worse
than it is: those 112 are accessor calls, so the cheapest shape is to redefine the accessor to return
`stored_slots()` and delete the field from the record's encode/decode - the callers then keep
compiling while the geometry unifies. That is the shape to try first; editing 112 call sites is the
fallback, not the plan.

## Design Documentation Impact

- [ADR 0052](0052-per-label-adjacency-order-and-tombstone-reuse.md): withdraw the value clause from
  the swap gate and replace the delete-cost amendment with a pointer to this ADR.
- [ADR 0097](0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md): record that the values
  run mirrors the edge run's index rule and prefix width.
- [ADR 0098](0098-the-vertex-slice-owns-its-growth-bound.md): the values prefix no longer needs the
  denseness it mentions as the reason swap-compaction was refused.
- `design/storage/lara.md`: the inline-property geometry section, once the field is gone.
- `design/implementation-gaps.md`: `GAP-LARA-INLINE-DELETE-COST` closes when step 3's measurements
  hold.
