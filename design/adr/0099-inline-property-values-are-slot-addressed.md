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

**What the atomic part actually contains, from a partial pass (2026-11-03).** Two of the pieces are
small and two are not:

- *Small, and they work on their own:* the range reader
  (`read_bucket_inline_property_bytes_range_into`) is already slot-addressed - it splits at the prefix
  and indexes the spill run as `slot - prefix`. Making the single-slot reader stop mapping live ordinals
  through the spill-run chain, and having the slot-order collector pass the row's slot instead of its
  enumerate index, is a handful of lines.
- *The piece that makes it coherent:* the value span's **allocation** must follow the edge prefix.
  `ensure_bucket_inline_property_bytes_span` / `publish_bucket_inline_property_bytes_span` still size
  the span per live degree, so with the prefix width unified the reader asks for a slot the span never
  allocated and gets `MissingAscLogIndex` - measured on
  `directed_inline_property_adjacent_reverse_hub_stays_writable_after_skew`.
- *Then:* delete must stop shrinking the span and stop copying the suffix, the fold must extend both
  prefixes together, and compaction must move value blocks with rows. Those are the paths where a
  mistake corrupts data, and none of them can be left half-done.

So the pass is: accessor -> reads -> span allocation -> delete -> fold -> compaction -> record field.
Any prefix of that list leaves the tree red, which is why it is a single sitting.

**The coupling that makes it one sitting, measured.** With the accessor, the slot-based reads, the
span sizing (`stored_slots` blocks) and the delete all applied - 5 of the 7 steps - the suite is still
91 red, and the dominant failure is `InlinePropertyBytesLogRead(MissingAscLogIndex { asc_log_index: 0 })`:
a live edge row sits at bucket-local slot >= `stored_slots` while `values_spill_run` is unset, so the
values run the reader expects was never created. The edge run and the values run are created,
grown, folded and dropped by different paths, and by this ADR's geometry they must be in step
slot-for-slot. That is the remaining work, and it is the part where a mistake writes values into the
wrong rows instead of failing a test loudly.

**The real coupling, found on the third attempt: the two planes have independent residencies.** My
first reading of the failure - "a pending value is a third source" - was wrong. Materialization is not
a per-row staging area: it fills a byte span with `fill_byte` over several messages and then publishes
`slab_slots = bucket.degree()` and **clears** the values run
(`prepare_materialize_publication`). It is a schema declaration for a bucket that had no values, and
what it publishes is *dense by degree*.

That is the defect this ADR has to remove, and it is why the accessor change broke reads. A bucket's
residencies are decided independently today:

| plane | slab holds | run holds |
| --- | --- | --- |
| edges | slots `[0, stored_slots)` | rows `stored_slots..` |
| values | the first `slab_slots` **ranks** | the remaining ranks in the values run |

and `slab_slots` is not tied to `stored_slots` at all - materialization sets it to `degree` while the
edges may sit entirely in their run (`prefix = 0`, `stored_slots = 0`, `degree = 2`, no values run).
So "one geometry" is not an addressing detail: it requires **value residency to mirror edge
residency**, which is the same statement as addressing values by slot. There is no smaller step.

That is the design question to settle before the next implementation attempt; it is not a detail of
the read path but the reason the first failure is a *read* rather than a write.

**Status of this attempt:** applied as far as the delete, measured 91 red, reverted (see the branch
note at the end of this section). The tree is back
to green (lara 708/0, graph 1107/0) with the plan, the sizes and the coupling recorded here.

**What that coupling requires of the design (to settle before implementing).** The two runs must be
one lifecycle, not two that happen to agree:

1. **Same length, always.** A row beyond the edge prefix has a value beyond the value prefix, so
   `spill_used_len(values_run) == spill_used_len(edge_run)` is an invariant, not a coincidence.
2. **One publisher.** Creating, growing, and dropping a spill run are three separate operations today;
   after this ADR they must be one operation that touches both stores, or a crash between them leaves
   a row whose value was never allocated. That is the same "one writer" rule ADR 0098 applied to the
   leaf flag.
3. **Derived, where possible.** If the value run can be derived from the edge run (same id, or created
   by the same call), the invariant is structural rather than maintained - which is this ADR's own
   argument applied to the spill runs.
4. **The record's two run fields** then either collapse into one or are written together in a single
   publish, and `inline_property_bytes_log_chain`/`log_len` disappear with the live-ordinal mapping
   they exist for.

Without that, the five steps that were applied produce exactly the failure that was measured: a row
whose slot says "run" and a bucket whose value run says "nothing here".

### The single publisher is a type

Point 2 is best enforced by a type rather than by discipline, because the failure it prevents is
"one of the two stores was updated" and no amount of call-site care makes that unrepresentable.

`BucketSpillRuns` - a view over the two existing stores, not new state:

| member | meaning |
| --- | --- |
| `edges: &SpillRunStore` / `values: &SpillRunStore` | the two arenas the rows already live in |
| `bucket_slot` | which descriptor this pair belongs to |
| `edge_run` / `value_run` | the ids, both or neither |

Its API is the set of operations that must keep the two in step - `append(edge, value)`,
`drop_head(n)`, `len()` - each of which applies to **both** stores and touches no descriptor. The
only method that publishes is `commit(bucket) -> bucket`, which writes the descriptor once with both
spill-run ids and their shared length. That makes three things true by construction:

- **the pair exists or does not**: there is no way to mint an edge spill run without the value spill
  run, because the method that mints one mints the other;
- **the lengths agree**: `commit` refuses a pair whose two stores disagree, so the mismatch the 91-red
  measurement produced becomes unrepresentable rather than debug-asserted;
- **one publish**: the descriptor is the commit point, so bytes written to either store before
  `commit` are invisible to a reader, and a trap between them rolls back with the message (the IC's
  message atomicity is what makes "write then publish once" safe).

What it deletes is the reason this was hard: the paths that grow, fold or drop a spill run no longer call
the two stores independently, and `inline_property_bytes_log_chain` / `log_len` go with the
live-ordinal mapping they served.

The type is also the natural place to test the invariant: a unit test that builds a pair whose store
lengths differ and asserts `commit` refuses it is the smallest possible statement of point 1.

**What a spill run is, and why the two ids cannot be one.** A spill run is one bucket's contiguous row
array in a `SpillRunStore`: an arena over its own memory region holding fixed-size rows (`row_bytes`), where a
spill run is allocated as a power-of-two class between 8 and 1024 rows, addressed `(run_id, index)` and
carrying a `used_len`. A bucket owns at most one spill run per plane, and the planes are two arenas:

| arena | rows | referenced by | id capacity |
| --- | --- | --- | --- |
| `ArenaRows::EDGE` | edge records (`E::BYTES`) | `LabelBucket.edge_spill_run`, bypass row | 2^27-1 (the bypass row's 27-bit field binds it) |
| `ArenaRows::VALUES` | inline-value blocks (`width` bytes) | `LabelBucket.values_spill_run` | `u32::MAX` |

Their allocators are independent - different memories, different capacities, different free lists -
so the same numeric id does not denote the same pair, and one id cannot be derived from the other.
The pair therefore carries both ids and publishes them together; point 3 above (derive where
possible) applies to the *length* and the *lifecycle*, not to the id.

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

## Work list, at function level (branch `slot-addressed-values`)

A fourth attempt applied the accessor, the slot-based reads, residency-by-slot on insert (the value
of a row inside the prefix goes to the slab span sized `stored_slots`, a row beyond it appends to the
spill run) and a value-free delete. That took lara from 96 failures to 53 and is parked on the branch
so `spill-swap` stays green. What the remaining 53 need, by function:

| function | why it is on the list |
| --- | --- |
| `rebuild_bucket_edge_spill` (`compact.rs`) | repacks the edge run, which **renumbers rows and returns moves**. The value run has to be repacked in the same order and released with it, or values follow the wrong rows. This is `BucketSpillRuns` applied to the rebuild path. |
| `fold_label_bucket_inline_property_bytes_log_to_slab` | folds the value run into the slab; with a mirrored plane it moves exactly the rows the edge fold moves, into the same slots, and drops the same run head. |
| `fold_bucket_edge_spill_head_into_span` | grows `stored_slots` by `k`, so it must grow the value prefix by `k` in the same publication. |
| `try_reuse_unordered_slab_tombstone` (`insert.rs`) | fills a slab hole; it still shifts the value suffix, which the slot plane no longer needs - write the hole's slot and stop. |
| `apply_edge_slot_move_in_bucket` / swap | moves a row; the value block moves with it, one block per moved row. |
| `materialize_*` | publishes `slab_slots = degree` over the whole bucket; it must publish the residency the edges actually have (slab part `stored_slots`, the rest into a value run created with the edge run). |
| record + fixtures | the `inline_property_bytes_slab_slots` field and the tests that assert it directly (e.g. "slab_slots = w=0 edges"). |

The two failures visible at 53 are exactly the first two rows: `remove_directed_deferred` returning
false after a span compaction (the fold left values that no longer line up with slots) and
`MissingAscLogIndex` (a read reaches for a spill run the mirrored plane has not created yet).

**Progress since the work list was written.** The live-log fold (not the spill head fold) was the
transition that moved rows into the prefix and cleared the edge run without their values - found by
`#[track_caller]` on `with_stored_slots`, which reported `stored 0 -> degree` at `compact.rs:5798`.
Moving the value blocks there, and carrying them through `apply_edge_slot_move_in_bucket` (one block
copy, now that both planes share an address), took the branch from 53 to 48 failures.

A first attempt at materialization - publishing the fill over `stored_slots` and extending the value
run to the edge run's length - *regressed* to 50 and was reverted: the materialization fixtures assert
the old dense-by-degree publication, so that path needs its own pass rather than a one-line change.

**The swap simplified itself.** Its value half used to shift the window `[k_ord..m_ord]` one block
right, because ranks changed for the rows between the hole and the last live row. With one geometry
the moved row's block goes where the row goes, in one copy, and the `values_spill_run ==
NO_SPILL_RUN` restriction on that half disappears with it - a value-bearing bucket's swap is now no
more expensive than an edge-only one's, which is what ADR 0052's 2026-11-02 value clause was working
around.

**Where the branch stands: 96 -> 48 failures, and the remainder is mostly fixtures.** Of the 48,
thirteen are `values::tests` and the bulk of those assert the old geometry (`slab_slots = degree`,
dense-by-degree statistics) rather than behaviour; `traverse` (7) and `compact` (5) mix the same.
The code paths still known to be wrong are materialization's publication and the promotion path.

**Materialization landed, at 46.** Its stream length is the edge prefix rather than `degree`, its
publication keeps the value run and extends it to the edge run's length, and the accessor returns zero
value slots when the bucket has no width (which is what several "slab_slots on a width-0 bucket"
assertions were really about). Two of the four fixtures moved with it; the tombstone-inclusive fill
now expects `stored_slots` blocks and reads them, because a tombstoned slot keeps its block.

**Emptied buckets release the value plane too.** A bucket with no live rows owns no values, whether or
not its edge run survives as a tombstone holder - but the emptied branch only cleared the values plane
when the edge run was already gone, so a delete that left a tombstoned edge run kept a values run no
row could read. Releasing it at `degree == 0` and migrating the two remove fixtures that asserted
slab-residency took the branch to 44.

**Filling a hole writes the hole's slot.** The tombstone-reuse path still inserted the value at the
hole's live ordinal with every later value shifted up one block - the rank-addressed behaviour this ADR
removes - so a reused low tombstone left every value one generation behind. The shift is gone.

**Measured: the bucket is not tree, and the step fails on the second call.** Before the cursor steps,
the bucket is `tiny=false tree=false width=0 degree=5 stored=5`, and the two steps print
`progress=Unreserved total=5000` then `progress=Filling { position: 4096 } total=5000`. So the
"tree bucket" reading above was wrong - the width the error reports (1000) is the *materialization's*
width, and the second step's admission sees a bucket whose width is no longer 0. Something publishes the
descriptor between the two steps; `materialize_source` requires width 0 until the final publication,
so finding that writer is the next diagnostic. Narrowed once more: right after the first step the
bucket is still `width=0 offset=0`, and the fixture then only mints two more cursors (`0xA5`, `0xB2`)
and enqueues them before dropping and reopening the queue - so the writer is reached from the enqueue or
the reopen, not from the step. It is still the one item in this group that must be
diagnosed rather than restated.

**One of the deferred failures may be a regression, not a premise.** In
`materialize_owner_queue_identity_preserves_retry_progress_across_reopen` the step now fails with
`InlinePropertyBytesWidthMismatch { bucket_width: 1000, edge_inline_property_width: 1000 }` - equal
widths, so the error is the "materialization does not apply to tree buckets" branch, i.e. the bucket is
in **tree mode** by the time the cursor steps. Whether that is a fixture premise or the span-sizing
change causing a promotion needs measuring before the fixture is touched; it is the one item in this
group that has to be diagnosed rather than restated.

**The remaining ones are allocator-state fixtures.** Two in `values::tests` assert exact
`free_bytes` / `moved_spans` / `moved_bytes` figures (`deferred_inline_property_insert_skips_synchronous_compaction`
expects a freed 6-byte hole, `inline_property_bytes_compaction_moves_only_inline_property_bytes_spans`
expects at least 8 free bytes and two moved spans). Those numbers come from the old residency - more
spans, freed in a different order - so restating them means re-deriving the expected allocator state
for the mirrored plane rather than adjusting a constant. That is the shape of the last group and the
reason it is slower than the earlier ones.

**The pair update got the owner its insert already had.** `insert_directed_edge` is one method on
`DeferredBidirectionalLabeledLaraGraph` that owns both halves, their order and the trap discipline; the
update was two per-orientation setters with the pairing pushed up to `GraphStore` as a local closure,
so nothing owned "both rows moved together". `update_directed_edge_inline_property` now owns it and the
facade calls once.

**That refactor paid for itself immediately.** With one place to look, the real defect showed up:
`write_edge_inline_property_at_slot` delegated to the *ordinal* writer, i.e. to the slab layout, so
updating a row that lives in the values run wrote inside a span that does not exist (the bucket had
`stored_slots = 0`) while the read looked in the run - the update appeared to do nothing
(`inline_update_after_a_delete_writes_the_right_row` read back 203 after writing 204 and then 205). The
insert helper is not the answer either: it appends, which is what an insert wants and an update must
not. `write_edge_inline_property_at_existing_row` writes the row's own block - the slab block inside the
prefix, the run's row index beyond it.

**The delete failures are counterpart bookkeeping, not the value plane.** Measured
`delete_valued_directed_edge_by_handle_removes_reverse_counterpart`: after the delete the forward plane
reads `[(slot 1, [2, 0])]` - correct, the surviving row with its own value. The test then reaches the
same row through the reverse store's handle and gets `[1, 0]`, so the reverse row's canonical mapping
points at the slot the delete vacated rather than at the survivor. The value plane is not the suspect
here; the counterpart's slot bookkeeping is.

**Counterpart processing now has an owner, and the remaining bug narrowed with it.** The pair - not
either store - owns counterpart resolution (`CounterpartResolver`), the graph side keeps only the
adapter (`facade/store/counterpart.rs`), and the undirected canonical-endpoint rule that existed in both
crates now has one definition.

Tracing the delete failure through that owner: the value the test reads back through the reverse
handle (`[1, 0]`) is exactly the payload of the **tombstoned** forward row at the vacated slot, so the
reverse row's canonical mapping is landing on the dead row rather than on the live counterpart. The
candidate is therefore `select_live_edge_by_neighbor_ordinal` / the reverse-side PairOrdinal - not the
value plane, and no longer spread over several files.

**The last rank-addressed readers were in the traversal.** `visit_slab_bucket_span_edges`
attached each row's value at a live ordinal it counted alongside the walk, and
`attach_edge_inline_property` resolved one before reading - so the same slot produced different
values from different readers, which is exactly what the delete-counterpart fixture caught. Both now
pass the slot. That took the graph failures 7 -> 5.

The five left are one class: the **value run falling behind the edge run**. `RowOutOfRange { index: 1 }`
on update and `MissingAscLogIndex { asc_log_index: 1 }` on read both say a run row exists with no block
of its own - a row inserted into the edge run without inline bytes never appended a value block. The
mirror has to hold for those rows too (a zero block), but simply appending zeros in the insert path
regressed twelve lara tests that pin the current run lengths, so it is a change to make deliberately
rather than a patch: decide where a run row's block is created for a valueless insert, then restate
those expectations.

**The mirror is now declared.** Every row in the edge run owns a value block, and the insert path
passes zeros when the edge carries no inline bytes (`write_inline_property_bytes_to_log` takes bytes,
not an edge). Tiny buckets stay out of it - they hold no value state (ADR 0096 §5) - which is what the
admission debug assert caught when the mirror first tried to give them one.

The five that remain are the case that mirror cannot reach: a **bucket whose width is declared after
its rows exist**. Those rows have no blocks, and the run is only backfilled by the materialisation path
(`fill_value_run_for_edge_spill`), so an update that arrives first writes at an index the run does not
have (`RowOutOfRange { index: 1 }` in the reverse store). The fix belongs at the width-declaration
boundary - the moment a bucket acquires a value plane, its existing run rows need their blocks - not at
the write site, which would only mask it.

**The emptied bucket was the real gap.** Releasing the values run when a bucket lost its last row put
the planes out of step: the edge run stays as a reusable tombstone holder until compaction sweeps it
(`deleting_every_row_keeps_the_run_until_compaction`), so the next insert landed at values index 0 while
its edge row sat at the edge run's index 1 - every read or update at that slot was one row off. An
emptied bucket now gives up only its slab span, which is space rather than state. That took the graph
failures 5 -> 2.

**Measured why those two cannot simply be renumbered.** A fresh bucket in the test graphs never grows
a prefix: inserting eight rows into a fresh bucket gives `stored_slots = 0` at capacity 256, 4096 and
65536 alike, so every row spills and the slab stays empty. The `Measurement` fixture observes slab
geometry (`slab_occupied_tail`, `inline_property_bytes_slab_slots`) that only exists once rows have been
folded into the span, so re-expressing it means either folding the run in before the measured update, or
measuring the run geometry instead - a redesign of the harness rather than an edit of its numbers.

**The comparison that settles it.** The same eight inserts, run on `spill-swap` and on this branch:

| branch | `stored` | `degree` | `slab_slots` | `values_run` | `slab_occupied_tail` |
| --- | --- | --- | --- | --- | --- |
| `spill-swap` (rank plane) | 0 | 8 | **8** | none | **16** |
| this branch (slot plane) | 0 | 8 | 0 | 8 rows | 0 |

The rows spill in *both* models - the test graph's vertex segment has no room for a prefix at capacity
256, 4096 or 65536. What changed is where the values go: the rank plane kept them dense in the slab
regardless of edge residency, and the mirror puts them in the run beside their rows. Folding them back
into the span does not help either (`fold_bucket_edge_spill_head_into_span` returns 0 here: there is no
superior span room to grow into).

So the last two fixtures measure a geometry this shape no longer has. Their redesign is to measure the
**run** (values-run growth across the update) or to build a shape whose bucket really owns span slots -
not to move numbers.

The last two are `updating_undirected_edge_inline_property_*`: they assert
`slab_occupied_tail == canonical_prefix * 2` after inserting self-loop rows, which held when values were
dense over live rows regardless of where those rows lived. Under the mirrored plane those rows spill, so
the slab is empty and the fixture has no span to grow - it needs a shape that keeps the rows in the
slab, not a number change.

**Where it stands: lara 705/0, graph 2** (from 96 lara failures and 16 graph ones). The seven left are
mostly fixtures whose shape assumed the dense plane (`updating_undirected_...` expects 64 bytes of
occupied slab from rows that now spill).

**Where it stood at 2**, both in `deferred`: the remaining pair expect
`insert_directed_edge` to surface `InlinePropertyMaterializePending` where it now succeeds, which is a
state-machine question about whether the pending materialisation is still pending after the paths
above changed.

Along the way the group of failures each turned into a contract question rather than a constant to
nudge: the resident helper was answering "does this bucket hold value state" when its callers needed
"how many slots are backed" (the same conflation as `slab_slots`), and the selected-slot visitor
resolved live ranks where the batch wants slots.

**Where it stood at 10.** The record round-trip closed by asserting the geometry answer for a tree
bucket: no value span, so its slot count is 0 while the descriptor's allocated count still round-trips.

**Where it stood at 11.** Two of the allocator-state fixtures turned out to be asserting a state
the mirrored plane cannot reach rather than one it reaches differently: in those graphs every row
spills, so the values live in the buckets' runs and the slab owns no span at all. `free_bytes` counts
hole space between live spans, not capacity, so "six freed bytes" and "two moved spans" have no
referent. Both fixtures now assert their contract from the other side - no synchronous allocation, and
compaction with nothing of its kind to move - which is the same claim with the residency the fixture
actually has.

**Where it stood at 13.** The last of the compact group was a fixture asserting the swap gate's
withdrawn clause; it now states what the gate decides (a value run or a sparse value span no longer
stops a swap, an edge run still does, because the move needs the row to be in the span it swaps
within).

**Where it stood at 14.** Two of the last four were my own bugs rather than fixture premises:
`fill_value_run_for_edge_spill` re-ran the materialisation admission *after* publishing, so it failed its
own follow-up with a width mismatch whose two sides were equal; and the accessor answered with
`stored_slots` for tree buckets, where `stored_slots` is the tree's logical extent rather than a value
width.

**Where it stood at 16.** The rank-modelled fixtures fall one at a time as their premises are
restated: the delete no longer folds the plane (`removing_non_last_...`), a row's block is read through
the slot accessor rather than from the span's raw bytes
(`edge_inline_propertys_round_trip_via_unchecked_label_iteration`), and the storage statistics follow
the bucket's residency instead of assuming every row is slab-resident.

**Where it stood at 22.** After the swap gate, the hybrid fixtures and the accounting, the
promotion group cleared as well: `normal_and_bypass_relocation_retains_runs_and_values` now asserts the
mirror (both runs stand or fall together, same row count) instead of expecting the delete to fold the
values away, and two fixtures that proved a bucket was hybrid by "the two prefixes differ" now say so
with the values run, because the prefixes are one number.

What is left is 22: `values::tests` (10), `deferred` (5), `traverse` (3), `compact` (3) and one record
test - the fixtures whose model is the rank-addressed plane, which have to be rebuilt rather than
adjusted.

**The last design question, found in `value_span_relocation_*`.** Those fixtures drive
`fold_bucket_values_run_into_slab` - the fold that moves a bucket's **values only** from the run into
the slab - and assert the run is gone afterwards. Under the mirror that fold has no meaning: moving
only the values leaves the edge run holding rows whose blocks moved, which is precisely the divergence
this ADR removes. It has two production callers (`update_edge_inline_property_at_slot` before an
update, and the insert path when the value run is full) and both were there to restore the
rank-addressed plane's dense precondition.

So the question to settle is what replaces it, and the two candidates are:
(a) delete it and let the callers write at the slot (the update path already can; the full-run insert
needs its `ValuesRunFull` error path exercised), or
(b) redefine it to fold the **edge run and the value run together**, which is what
`fold_bucket_edge_spill_head_into_span` now does.

(a) is the smaller statement and matches the ADR; (b) keeps the call sites' shape but duplicates the
paired fold the edge side already has.

**Decision: (a).** The values-only fold is deleted, with both production callers - the update path now
writes the value at the row's slot, and the insert path lets a full run grow instead of pre-emptively
folding it. Three fixtures went with it (`value_span_relocation_reclaims_8/33` and
`..._failure_preserves_source`) because they exercised the fold itself, and the bench that measured it
is pointed at the paired fold, which is what folding values now means. That took the branch to 19
failures.

Losing those three fixtures loses their span-accounting coverage (retired span reclaimed, free-byte
arithmetic, source preserved on a failed relocation); `ensure_bucket_inline_property_bytes_span`'s
relocation path is what should carry that coverage now, and rebuilding it there is part of the
remaining work rather than something the deletion should quietly drop. Either way those three fixtures are rebuilt around whichever
answer is chosen, which is why they are the last group rather than the next.

**Where it stood at 25.** Three more distinctions landed: the swap gate's value clause is
withdrawn (ADR 0052's reason for it was the value window shift, which the slot plane removes), the
"hybrid" traversal fixture now proves its state with the values run instead of a width gap that can no
longer exist, and the accounting separates what a bucket needs from what the allocator backs.

The remaining 25 split into `values::tests` (10), `deferred` (5), `traverse` (3), `compact` (3) and
`promote` (2), and they are the ones whose model is "values are dense over the live rows, independent
of edge residency" - `value_span_relocation_*`, `inline_property_bytes_compaction_*`, `storage_stats`,
the cursor/hybrid page fixtures. Rebuilding those expectations around the mirrored plane is the work
that is left, plus the promotion path.

**Where it stood at 29.** Two more distinctions landed after the slot batch: a tree bucket owns no
value span to charge the vertex for (its values live in the property root inside the edge span), and
`inline_property_bytes_slab_slots` means the geometry a bucket needs while
`inline_property_bytes_slab_slots_allocated` means what the allocator backs - the admission had been
comparing the first against itself, which left spans unallocated and vertices uncharged.

The remaining 29 are dominated by fixtures whose model is "values are dense over the live rows,
independent of edge residency" - `value_span_relocation_*`, `inline_property_bytes_compaction_*`,
`storage_stats`, and the traversal/deferred groups. They need their expectations rebuilt around the
mirrored plane rather than adjusted, which is why they are not mechanical edits.

**Where it stood at 43**, and the remainder is dominated by fixture migration:
`values::tests` (12), `traverse` (10), `deferred` (8), `compact` (4), `promote` (3), plus a record
test, a tree-read test and a spill-read test.

**What the wholesale rewrite needs, traced.** `rewrite_vertex_edge_span_with_policy` ends in
`prepare_physical_layout(PhysicalLayoutOperation::VertexRewrite { .. })` and
`PhysicalLayoutPreparation::publish`. The rows travel through a `BackedPhysicalEdgeImage` - a copy
image over the edge slab that the preparation validates and writes in one step. The values plane is
not part of that image, so whatever the image does to the slot order (a rewrite publishes a packed
span) the value blocks stay where they were.

The packing itself happens one step earlier, at **capture**: the preparation receives `canonical` rows
and a per-bucket `desired` layout, copies each bucket's span verbatim (`PhysicalSlabPayload::Buffered`
or `Disjoint`), and publishes `new.with_edge_range(position, ..)`. So the slot order a bucket ends up
with is decided when its rows are captured as `desired`, and the value blocks are never part of that
capture.

The missing piece is therefore symmetrical to what already exists: a **value-slab image composed into
the same preparation**, published in the same `publish()` so the two planes land atomically - the
same rule this ADR applies to the spill runs, one level up. The place to hang it is the same per-bucket
loop that builds `payloads`: for every bucket it needs the value blocks in the *desired* order, and the
publish needs to write them alongside the edge image. Building it means extending
`PhysicalLayoutPreparation` with the value images for the vertex's buckets and writing them in the same
commit; that is a self-contained unit, and it is the last code gap.

**Measured: the rewrite is not what moves them.** Instrumented
`edge_inline_propertys_survive_rewrite_with_tombstones` before and after the rewrite - the bucket is
`stored_slots = 0`, `degree = 2`, in both snapshots: all of its rows are in the **edge spill run** and
the values in the value run, on both sides of the rewrite. So the rewrite copies nothing for this
shape; the values were already one behind *before* it. The step that moves them is the **delete**:
after removing slot 0 the live rows read values `[10, 20]` where `[20, 30]` were written - each row
one block early.

**Narrowed again: the reader, not the delete.** The spill delete tombstones in place
(`spill_tombstone_row`) and drops nothing, so both runs keep their head. The off-by-one is in the read
path that serves this shape: `visit_edges_with_inline_property_impl`'s slow arm batches values by
**live ordinal** (`let mut ordinal = ...`, `value_range = ordinal..ordinal + batch_rows`) while the
callback reports a *slot*, so a bucket with a tombstone reads each block one slot early. Converting
that batch to slots is the fix, and it landed: **43 -> 34**. The bound had to be the bucket's
*resident value slots* (slab prefix plus spill rows), not the slab prefix - bounding by the prefix alone
gives an empty range for a run-backed bucket and underflows the in-batch offset, which is what the
first attempt (43 -> 49) did. It also means the physical-layout value image is not needed for this
failure.

**Two things the accounting had conflated.** `inline_property_bytes_slab_slots` was answering one
question and being used for another. After this ADR it means "how many value slots this geometry
needs" - the edge prefix - but `ensure_bucket_inline_property_bytes_span` compared it against
`had_bytes` computed from the *same* accessor, so the admission was a tautology: a slab bucket could
report 16 backed slots while the vertex was charged nothing, and no span was ever allocated.
`inline_property_bytes_slab_slots_allocated` exposes the descriptor's recorded count for that side, and
the resident/charged accounting uses it. That distinction took the branch 34 -> 30.

**Next lead, from the 34.** `invariants.rs` asserts
`vertex.inline_property_bytes_allocated_bytes == sum of bucket_resident_inline_property_bytes`, and it
now fails with `left: 0` on tree-mode point-state fixtures - so some publish path changes a bucket's
value slots without growing or shrinking the span that backs them. That is the accounting side of the
rule the value image would enforce.

Two fixtures still show values one row behind after a **wholesale** span rewrite
(`edge_inline_propertys_survive_rewrite_with_tombstones`,
`remove_matching_middle_edge_removes_same_inline_property_bytes_ordinal`). The incremental moves and the
swap both carry values now, so what remains is the rewrite that copies the whole span in one pass -
the last known code gap besides promotion and the wide-value (1000 byte) materialization case that
raises a width mismatch where both sides report 1000.

**Materialization resists a one-line change, twice measured.** Publishing the fill over `stored_slots`
plus extending the value run to the edge run's length was tried twice: 48 -> 50 both times. Four of
the deferred-materialization fixtures (`materialize_atomic_on_failure`,
`materialize_owner_wrapper_publishes_both_orientations_and_surfaces_cancellation`,
`materialize_inline_property_stream_drains_via_bidi_maintenance_queue`,
`directed_insert_deferred_materialize_via_wrapper_retry_after_drain`) depend on the publication shape,
so the code change and their migration have to land together - which is the next unit, not a patch.

**And a third, found by instrumenting the simplest failing case.** In
`values::tests::edge_inline_propertys_preserved` (three valued inserts, then
`compact_vertex_edge_span`, then read back - all zeros), the head fold never runs, and by the time the
compaction step sees the bucket it is `stored_slots = degree` with **no edge run but a live values
run**: the rows are in the slab and their values are still in the spill run. At insert time the same
bucket was `stored = 0` with an edge run and a values run, so something between the insert and the
compaction publishes `stored_slots = degree` - the finalize that trims trailing tombstones - without
moving the values. That transition is the next thing to find; it is not in the fold, the rebuild or
the delete, which is why all three fixes left the count unchanged at 53.

