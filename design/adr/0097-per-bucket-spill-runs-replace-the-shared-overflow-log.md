# 0097 — Per-bucket spill runs replace the shared per-leaf overflow log

**Status:** proposed (2026-09-20). Contract for the log→spill swap recorded in
`design/implementation-gaps.md` ("The spill design, in one place"). Pre-production: no deployed data, so no
migration, no legacy decoder and no compatibility path — a layout change means fresh state or reinstall.

## Context

A slab bucket's rows are a contiguous prefix plus everything that does not fit it. Today the overflow lives in a
**shared per-leaf log**: a per-segment area whose capacity is provisioned eagerly, per bucket a chain of entries
linked by `prev`, emptied by a leaf-wide fold (`prepare_vertex_edge_span_for_overflow_log_fold` then a leaf-wide
rebalance), and released as a whole segment after proving every bucket in the leaf was drained.

That design produced every serious defect this project found in the tier:

* **Row loss is possible** because one owner releases an area others may still use:
  `LogStore::release_segment` zeroes a segment and resets its index without an emptiness check, and the stepped
  compaction packed a bucket to `stored_slots = degree` while an unfolded log row was still chained
  (GAP-2026-09-20-001; the I2 guard was added as a stopgap).
* **Capacity is paid regardless of use**: the audit measured 87 040 entries (~696 KB) of log capacity for 5 000
  vertices with **zero** entries used, because capacity is per segment and segments grow with the vertex count
  (GAP-006).
* **Buckets are coupled**: the production-shaped hub showed 1 of 20 buckets spilling (74 of 10 000 rows), yet that
  one bucket's pressure triggers a leaf-wide fold and a leaf-wide segment release, touching 19 buckets that hold no
  spill at all.
* **The chain is walked on every read**, and the fold/fold-prelude/recovery choreography exists only to empty a
  shared area so it can be reused.

The reference implementation (`~/dev/lara`) settles the two questions this raises: its graph is **correct with the
log disabled** (`LaraConfig { max_log_entries: 0 }`, verified against the DGAP port), and its capacity sweep shows
capacity buys **speed only** (cap 0 near-fastest on the hub; persisted bytes per insert flat across capacity). Its
docs also list "chunk numbering" for the log as a candidate blocked by sharing one variable-size area — which is the
thing this ADR removes.

## Decision

Replace the shared per-leaf log with a **per-bucket spill run**, allocated lazily, contiguous, and owned by the
bucket.

1. **Ownership.** A spill run belongs to exactly one bucket. Nothing else may read, append to, compact or free it.
   There is no shared area, no segment, and no cross-bucket capacity pressure.
2. **Shape.** A run is a contiguous block of fixed-width rows (`E::BYTES` each). A bucket's logical row `i` is the
   prefix slot for `i < prefix_len`, otherwise spill row `i - prefix_len` — the same ordinal scheme the log used, so
   **existing ordinals do not move** when the spill grows (the spill is append-only).
3. **Descriptor.** The row stays 29 bytes wide, but its *packing* changes, because the field this design first
   assumed it could reuse is only **8 bits**: `decode_bucket_overflow_log_head` reads
   `(word >> BUCKET_LOG_SHIFT) & 0xFF` and the encoder caps the value at 170 entries, i.e. an encoded entry index
   inside one shared segment — it cannot address a spill arena. Since tiny and slab are mutually exclusive modes and
   the wire is already mode-dependent (ADR 0096 §1 reuses the former stored-width bytes), slab mode takes the bytes
   tiny uses for its payload to carry the **spill run id**, and the old log-head bits are retired. Tiny keeps its
   payload meaning and tree keeps its own root/run reference; only slab's packing changes. Pre-production, no reader
   for the old packing, so this is a layout break with fresh state.
   **The used length does not live in the descriptor.** Each run carries a header row inside the arena: while the run
   is live the header holds its used length, and the same word holds the free-list link while the run is free. The
   capacity class follows from that length (a spill's length is monotone), so no capacity field is needed and the
   descriptor holds only the run id. The store change (a header row per run, classes counting data rows) is part of
   the first slice.

   **Two run ids, not one.** The edge log and the inline-property-bytes log are separate planes: their rows
   have different widths and their counts differ (a bucket can have edges without values), so a single keyed id
   would either over-provision values for valueless rows or couple two allocators with different classes. Slab mode
   therefore carries an **edge spill run id** and, when values exist, an **independent values spill run id**, each
   allocated lazily by its own arena. "One owner per row" is per plane, and the values run exists only because
   values exist — the same rule as for edges.
   **Packing constraints (the exact bit budget is the implementer's to derive, with these bounds).** The row stays
   29 bytes wide, or widens only if the derived table cannot cover the intended arena scale — and if it widens, say
   so with the numbers. Retired by this change: the 8-bit log head (an in-segment index that cannot address an
   arena) and, in slab mode only, the bytes tiny uses for its payload (modes are mutually exclusive, the wire is
   already mode-dependent). `stored_slots` may narrow only if the promotion cap is *proved* (a test that a slab
   bucket never exceeds the promotion threshold, since the field's width would otherwise silently truncate), and
   the same applies to `ipb_width`. The unlabeled core path already has a 27-bit head at tail28 and needs no
   packing change, so the labeled ids should follow that precedent where the numbers allow. The derivation must
   state the arena size each id can address and the first workload scale at which that would fail.
   **Layout decision (2026-09-20, from the implementer's derivation).** The 29-byte row cannot carry useful ids:
   retiring the log head (8 bits), the two reserved bits and `ipb_log_len` (8) and repurposing `ipb_log_byte` (8)
   yields 26 bits for two ids — 13 each, i.e. 8 192 arena rows and about 4 096 spilled rows. So the row **widens to
   35 bytes**, carrying **two `u32` run ids** (edge and values) with 10 spare bits. The rationale is not bit
   economy: the store's `RunId` is already `u32`, so a 29-bit wire id would invent a second, tighter bound than the
   store has, while `u32` makes the wire and the store agree (SSOT) and leaves the only real limit where it belongs —
   the arena's `u32` row count (2^32 rows = 16 GiB of edge rows, ≈290 billion edges at the measured spill fraction).
   The alternatives considered and rejected: 32 bytes (25-bit ids, below the unlabeled core's 27-bit precedent),
   33 bytes (29-bit ids, the artificial bound above), 34 bytes (two u32 with 2 spare bits). `u32::MAX` stays the
   sentinel for "no run", so a live id is at most `u32::MAX - 1`; the packing must reject the sentinel.
   The narrowing this ADR had asked for is **impossible and does not happen**: `stored_slots` has no
   `<= T_PROMOTE` bound (the repository tests the opposite — `batch_run_past_t_promote_then_scalar_promotes_correctly`
   drives `stored_slots` to `T_PROMOTE + 100` while still slab, promotion being deferred to the next scalar insert),
   and `ipb_width` is a full `u16` bounded by `MAX_EDGE_INLINE_PROPERTY_BYTES = u16::MAX`. The derivation was asked
   for as a precondition and it disproved the assumption it was testing; the widening is therefore forced, not chosen.


   **Interim width, and why the final wire is still 35 bytes.** The values twin is log-based until slice 5 and reads
   bytes 27–28 as `inline_property_bytes_log_head()/_len()` from 79 sites in `labeled/graph/values.rs`, so those two
   bytes cannot be the values run id's low half while that path still compiles. The swap therefore uses a **37-byte
   row for the interim slices**, with both ids in the new bytes (29..37) and bytes 27–28 left to the values twin; every
   slice keeps compiling and the packing tests are verified directly rather than behind a shim. **Slice 5 deletes the
   values log, and with it that last reader of bytes 27–28: the values id then moves down into the freed bits (27..35)
   and the row returns to the 35-byte final layout above.** The 2-byte difference is the price of verifiable
   intermediate states — ~50 KB across the audit's 25 000 descriptors against the ~696 KB the swap removes — and it
   disappears at the end.

   **The bypass row widens too.** `LabeledVertex` (21 bytes) carries its run id in `metadata28` bits 4–11 (eight bits)
   with `bucket_slack_slots` in bits 12–27 and bit 3 reserved, so bypass mode has 25 free bits, not 32 — it cannot
   carry the `u32` the store mints, and a 25-bit bypass id would be the same artificial bound this ADR rejected for the
   labeled row. The bypass row is therefore widened by exactly what a `u32` run id needs, with the existing fields
   kept in place and the new width reported; if the bypass still reaches its log through an interim path, the interim
   encoding stays compiling now and the final one lands when that path dies, exactly as for the labeled row. There is
   one `RunId` type and one arena for both rows: no second, narrower bound is introduced anywhere.


   **Three encoding rules the swap settles.** (i) A run id is valid **only** on slab and bypass rows: tiny, tree and
   normal (bucket-mode) rows must reject one on read, because the arena serves the two spill-owning modes and nothing
   else. (ii) The bypass row widens by exactly 4 bytes (21 → 25) with the run id in a new `u32` field; its log head
   stays in `metadata28` bits 4–11 for the interim. (iii) `LabeledVertex`'s `Default` becomes a manual impl: the
   derived zero would decode as the live run id 0, and the sentinel is `u32::MAX`. Widened rows also mean the
   descriptor/vertex byte-shape tests (G6) move by whole multiples of the row delta (+8 labeled, +4 bypass); those
   tests keep their contracts (tiny reads no edge bytes, slab append moves strictly more than tiny) and the new
   numbers are derived from the delta, not observed and adopted.


   **`release` learns the class from the run, not from its caller.** A run's class is recoverable from its own header
   row, which the store owns and which already records the used length, so `release` derives the free list from the
   run's header and treats a caller-supplied length as a sanity check only. This matters as soon as `grow_to` relocates
   a run: a run allocated as class 128 and grown at 3 used rows would otherwise be released into the 8-row free list
   and later handed out with mismatched capacity. Two rules follow, both tested: (i) release **validates before it
   mutates** and never returns an error after its first metadata write, so a failing release cannot free a run and then
   report failure (the caller would keep a descriptor pointing at a free run); (ii) a `grow_to` that fails in its
   allocation leaves the old run allocated and the descriptor unchanged, which is the no-data-loss direction of
   section 6. The publication order is fixed as well: allocate the new run, copy, return, and let the caller publish
   the new id — the descriptor is never pointed at a run the store has already freed.


4. **Lazy allocation.** A bucket with nothing to spill owns nothing. The first row that does not fit the prefix
   allocates a run; no policy may pre-allocate a run "for growth" (the analogue of "slack is a hint").
5. **Allocator.** Power-of-two capacity classes from `MIN_ROWS = 8` to `MAX_ROWS = 1024` rows, a free list per class
   whose first four bytes hold the next free run's row offset (`u32::MAX` terminates), reuse only within a class, no
   coalescing. Because a run's used length is **monotone** (deletes tombstone in place; rows leave the spill only
   when compaction copies them back into the prefix, and the run is released with exactly the length it has),
   `ceil_pow2(length)` always equals the allocated class — so the descriptor needs **no capacity field**.
6. **Growth.** Appending past the current class allocates the next class, copies at most 2x the live rows and
   releases the old run (amortized O(1), waste <2x), and grows in place when the run is the arena tail (see the slot
   rules). **The spill tops out at `MAX_ROWS` (1024 rows); beyond that the bucket promotes to tree mode** — the
   existing, already-validated chunked path with its own thresholds — instead of inventing a second level in this
   change. That removes the level-2 block-list question entirely: there is no block list to format, and a bucket
   larger than the largest class is simply a tree bucket. (Generalising tree mode into the spill is the separate
   follow-up question already recorded, not part of this swap.)

   **Arena exhaustion is fail-closed, never a panic.** When a bucket's run cannot be allocated because the arena
   has no addressable row left (an exhaustion the 29-bit ids make reachable, and which the store currently turns
   into `expect("spill arena overflow")`), the allocation returns an error and the caller **promotes that bucket to
   tree mode** — the same path as the class limit above. No insert loses data, no abort is reachable from a workload,
   and the arena bound becomes a stated limit of the spill tier rather than a panic. The store must return an error
   where it now expects (the `expect` sites go with this rule), and the insert path must treat that error as the
   promotion trigger, distinct from a genuine memory-growth failure (which stays an error the caller reports).
7. **Operations.** Insert: prefix slot if free (tombstone reuse or the Insertion headroom), else append to the run,
   else grow by class. Scan: prefix rows then run rows, both contiguous. Delete: tombstone in place, live count
   down, run length unchanged. Compaction: when the live rows fit the prefix, copy them in and **release the run** —
   the only place a run is freed besides bucket death — and skipping compaction is always allowed.
8. **Invariants.** One owner per row at a time; every published width *and every spill length* is a claim that must
   be backed; ordinals of existing rows never change; resident content is `prefix + spill`, one definition shared by
   sizing, placement and publish.

## Facts and assumptions (separated)

**Facts, measured in this repository:** the audit's 87 040 entries (~696 KB) of log capacity with zero entries used
for 5 000 vertices; 1 of 20 buckets spilling 74 of 10 000 rows in the production-shaped hub; `release_segment`
zeroing a segment without an emptiness check; a stepped compaction packing to `stored_slots = degree` while an
unfolded row was still chained (two reproducible row-loss paths); chunked sequential scans at ~41 ins/edge
(ADR 0088); the LTB store's fixed 4096-byte payload (1024 rows); a focused canbench cost of 122 K instructions in
`labeled_resolve_in_leaf` on the non-tail bypass insert, i.e. the price of *finding* a span when content outgrows
the prefix.

**Facts, measured in the reference implementation** (`~/dev/lara`): correctness with the log disabled
(`max_log_entries: 0` against the DGAP port); capacity buys speed only (persisted bytes per insert flat across
capacity; cap 0 near-fastest on the hub); per-owner runs with power-of-two classes and per-class free lists,
measured for descriptor runs.

**Assumptions to confirm after the swap, not before:** that the growth-copy spike is removed by in-place tail
growth in Gleaph's shapes; that hole reuse in the spill can stay O(1)-ish with today's "earliest hole" rule (a hint
if a measurement says otherwise); that the class boundaries measured against 24-byte reference records transfer to
Gleaph's 4-byte rows; and that the canbench regression above disappears because the span query is no longer made.

## Why the existing concept cannot own this (Step 2)

The log *is* the existing concept, so the question is whether it can be extended rather than replaced. Its three
defining choices are exactly the three problems: it is **shared** (so a release is an area others may use, which is
where row loss becomes possible), it is **per segment** (so capacity is provisioned with the vertex count, which is
the measured fixed cost), and it is a **chain** (so reads walk, and folds exist to empty it). Each could be patched
— a drain guard for the first, on-demand segment capacity for the second, chunked entries for the third — and the
minimum-change alternative below does exactly that; but the patches leave the ownership, the granularity and the
shape in place, so the first two properties keep producing the same class of failure and the third keeps the
choreography. Extending it to a per-bucket, contiguous, lazily allocated area *is* the proposal, which is why this
is a replacement rather than an extension.

**State representability (Step 2a).** The descriptor already has an `i32` field for the log head, so the spill run id
is representable **as-is**, with no new field, table, or enum variant; no new ownership claim appears that the
current types cannot carry. What changes is the *meaning* of that field in slab mode and the store behind it, i.e. a
breaking layout change — acceptable pre-production and explicitly not migrated (fresh state or reinstall). The one
new persistent object is the arena's 64-byte header plus eight free-list heads, one per graph.

## Alternatives considered (Step 3)

| Alternative | Benefits | Drawbacks | Complexity |
| --- | --- | --- | --- |
| **Minimum change**: keep the shared log, add the drain guard, provision segment capacity on demand, chunk entries to shorten walks | smallest diff; keeps the fold and recovery known-good; fixes the two measured loss paths and part of the fixed cost | ownership, granularity and shape stay; a segment release is still an area others may use, so the loss *class* survives; folds and the recovery loop stay; the measured coupling (1 bucket of 20 driving leaf-wide work) stays | low |
| **Moderate change (this ADR)**: per-bucket, contiguous, lazily allocated spill runs with class free lists and an LTB level 2 | ownership, shape and lifetime all become per-bucket, so the guard, fold, prelude, recovery and per-segment capacity are deleted rather than patched; reads become contiguous; capacity tracks spilled rows | a new allocator (~200 lines); growth copies unless the run is at the arena tail; hole reuse needs a rule | medium, with a large deletion |
| **Large redesign**: chunked runs for every non-inline bucket (tree mode generalised; A′ in the ledger) | one storage strategy above the inline threshold; no prefix, so no prediction, no slack, no span growth | a degree-5 bucket would occupy a 1024-slot granule (the LTB payload is fixed), trading a prediction problem for a utilization problem; loses the flat prefix's scan path | high |
| **One shared arena with per-bucket runs** | one allocator, one header | the arena is shared again, so a release or compaction interacts across buckets — the coupling that caused the failures | medium but conceptually the same as today |

## Costs (Step 4)

Migration: none (fresh state; no reader for the old layout). Compatibility: internal only. Documentation: ADR 0096
§5's log rows, `design/storage/lara.md`'s per-segment overflow-log contract, the D-GAP contract text. Tests: the log
families are deleted and replaced (lazy first allocation, class reuse, tail in-place growth, level-2 hand-off, no
pre-allocation, audit fixed cost zero). Benches: rerun `compact` and `ins`, and confirm the predicted regression
disappearance. Operations: no new canister, role or lifecycle. Maintenance: eight class free lists and one header in
exchange for the segment table, per-segment capacity, the fold, the prelude, the drain guard and the recovery loop.
Future extension: level 2 and tree mode can be unified, and K can be re-evaluated, once the middle tier is lazy.
Net complexity: a ~200-line allocator replaces what is being deleted, so the change is a net deletion.

## Long-term effects (Step 5)

It simplifies the architecture (one fewer store, one fewer choreography), clarifies boundaries (storage that a
bucket owns), improves encapsulation (a run is private to its bucket), strengthens invariant enforcement (a
published length must be backed; the shared-release failure mode becomes unrepresentable), keeps canonical/derived
consistency unchanged (ordinals and the resident definition are untouched), strengthens SSOT (resident = prefix +
spill, one definition), and reduces duplication (two log twins collapse into one design). On the negative side it
adds one concept — a run allocator — which is justified because the concept it replaces is the source of the
invariant fragility; and it does not widen any boundary or expose internal state.

## Consequences

**Deleted, not adapted** (each for a stated property): the segment store and its per-segment capacity and segment
table; the chain (`prev`, chain-length walks); the fold and its prelude; the drain proof and the I2 guard; the
log-full error and the leaf-wide recovery loop; the parts of span growth and slack sizing that existed only to
predict the prefix width. The log-specific tests and benches go with them, replaced by spill tests (lazy first
allocation, class reuse, level-2 hand-off, "no pre-allocation", and the audit shape showing zero fixed cost).

**Kept**: tiny inline, the slab prefix and the flat scan it buys, tree mode for now, the 29-byte descriptor, the
ordinal scheme, the resident-content definition, the ownership rules, and the memory-region count (the freed log
slot carries the spill store, so the 31 `LabeledLaraGraph::new` sites are untouched).

**Closed by deletion** rather than by fix, and their ledgers must say so: GAP-2026-09-20-001 (log loss while
promoting), the log half of GAP-2026-09-20-005, and GAP-006 (eager per-segment capacity).

**Measured basis**: chunked sequential scans cost ~41 ins/edge (ADR 0088, the structure level 2 reuses); a
74-row spill lands in a 128-row class (512 B at 4 B per row); the audit's ~696 KB of unused log capacity becomes 0;
the reference's cap sweep justifies lazy ownership over capacity tuning.

## Slot reuse and ordering in the spill (rules the insert path must follow)

* **The region is the log's.** The spill store takes the memory region the log store occupied, so the graph's
  region count and every `LabeledLaraGraph::new` call site stay unchanged. (An earlier note suggested sharing the
  LTB region; that is superseded — the LTB allocator has no capacity ceiling, so a reservation at its end would be
  overwritten as the tree store grows.)

* **No row ever shifts.** A bucket's ordinal space is sparse by construction (`stored + offset` with tombstones),
  so "insert at a position" is expressed as delete-then-insert, exactly as today (ADR 0052). Splitting rows across
  prefix and spill introduces no middle-shift case, because neither side ever compacts by shifting: the prefix keeps
  its `used` headroom and the run appends at its dense end.
* **The store owns the length, and it enforces addressing bounds.** The used length may only move
  through `allocate`, `grow_in_place` and the release/reuse path — never through a public setter — because
  those are exactly the operations that keep `class_rows(len)` equal to the class the run was allocated with.
  Reads and writes whose index falls outside the class derived from the header length are rejected rather than
  silently addressing the next run's header. Both rules were decided by the implementer during slice 1 and are
  recorded here because the ADR stated the invariant but not who enforces it.
* **A deleted slot is reusable.** `Unordered` fills a reusable slot rather than growing, and `Insertion` appends at
  the dense end (`used` grows only there). The *observable* rule is that a delete's slot becomes reusable before
  the bucket grows; the *implementation* may reuse the earliest such slot (today's behaviour) or carry a hint — the
  prefix keeps the current rule unchanged, and the spill may add a descriptor hint if measurement shows that
  scanning for the earliest hole matters. This keeps the tombstone-reuse purpose (bounded memory) independent of
  which side of the boundary the hole sits on.
* **Growth extends in place at the arena tail.** When a run's class is exhausted and its rows sit at the arena's
  tail, the class is extended in place; otherwise the bucket allocates the next class, copies at most 2x its live
  rows and releases the old run. This is a requirement rather than an optimisation: it removes the growth-copy spike
  that an append-only log did not have, and it is the reference's measured rule (`bucket-tail-growth` reduced
  read/write requests and mapped bytes on exactly this pattern).

## Open questions (each with a measurement, none blocking this decision)

Level-1 class boundaries against Gleaph's 4-byte rows (the reference's numbers are for 24-byte records); the level-2
threshold; whether tree mode is still needed once a bucket can be "prefix + LTB-backed spill run" (tree's LEG root
is a one-level block list a run header could carry); whether K moves now that the middle tier is lazy; and the
batch/deferred paths, which lose their log-capacity reservation entirely.
