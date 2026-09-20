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
3. **Descriptor.** The 29-byte row is unchanged. `overflow_log_head: i32` becomes the **spill run id** in slab mode
   (a row offset in the spill arena, so its top bit may later mean "level 2"). Tiny keeps its payload meaning and
   tree keeps its own root/run reference; only slab's meaning changes.
4. **Lazy allocation.** A bucket with nothing to spill owns nothing. The first row that does not fit the prefix
   allocates a run; no policy may pre-allocate a run "for growth" (the analogue of "slack is a hint").
5. **Allocator.** Power-of-two capacity classes from `MIN_ROWS = 8` to `MAX_ROWS = 1024` rows, a free list per class
   whose first four bytes hold the next free run's row offset (`u32::MAX` terminates), reuse only within a class, no
   coalescing. Because a run's used length is **monotone** (deletes tombstone in place; rows leave the spill only
   when compaction copies them back into the prefix, and the run is released with exactly the length it has),
   `ceil_pow2(length)` always equals the allocated class — so the descriptor needs **no capacity field**.
6. **Growth.** Appending past the current class allocates the next class, copies at most 2× the live rows and
   releases the old run (amortized O(1), waste <2×). Past `MAX_ROWS` the spill continues in **LTB blocks** (level 2,
   the store tree mode already uses); the run header carries the one-level block list.
7. **Operations.** Insert: prefix slot if free (tombstone reuse or the Insertion headroom), else append to the run,
   else grow by class. Scan: prefix rows then run rows, both contiguous. Delete: tombstone in place, live count
   down, run length unchanged. Compaction: when the live rows fit the prefix, copy them in and **release the run** —
   the only place a run is freed besides bucket death — and skipping compaction is always allowed.
8. **Invariants.** One owner per row at a time; every published width *and every spill length* is a claim that must
   be backed; ordinals of existing rows never change; resident content is `prefix + spill`, one definition shared by
   sizing, placement and publish.

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

## Open questions (each with a measurement, none blocking this decision)

Level-1 class boundaries against Gleaph's 4-byte rows (the reference's numbers are for 24-byte records); the level-2
threshold; whether tree mode is still needed once a bucket can be "prefix + LTB-backed spill run" (tree's LEG root
is a one-level block list a run header could carry); whether K moves now that the middle tier is lazy; and the
batch/deferred paths, which lose their log-capacity reservation entirely.
