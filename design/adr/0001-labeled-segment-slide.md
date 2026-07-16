# 0001. Labeled edge physical layer uses PMA leaf segment slide

Date: 2026-06-07  
Last revised: 2026-07-16 21:32:14 UTC +0000
Status: accepted

## Context

**Core LARA** (`crates/ic-stable-lara/src/lara.rs`) already follows the DGAP dynamic CSR model on IC stable memory:

- Per-vertex scan metadata (`base_slot_start`, `degree`, `log_head`) — DGAP `vertex_element`.
- PMA leaf density (`segment_edges_actual` / `segment_edges_total`) and weighted rebalance inside a fixed physical window.
- Segment relocation / slide when density or capacity requires moving a leaf block; retired physical ranges go to `FreeSpanStore`.

**Labeled LARA** adds multi-label adjacency (`LabeledVertex`, `LabelBucket`, per-label `LabelEdgeSpan`). Scan semantics and overflow logs align with DGAP. **Physical edge bytes do not.**

Today, labeled rows reserve a per-vertex `VertexEdgeSpan` (`LabeledVertex.stored_slots`) on the shared edge slab. Growth and rebalance in `labeled/graph/compact.rs` often:

1. Append a new span at `elem_capacity` (tail).
2. Retire the old footprint via `release_vertex_edge_span_footprint` (monolithic or bucket+gap intervals).

This is an **interim** layout documented in `labeled.rs` and [lara-dgap-contract.md](../storage/lara-dgap-contract.md). It treats each vertex as its own physical rope chunk instead of sharing one PMA leaf block.

**Observed failure mode:** On a high-degree, many-label hub (e.g. 50k edges across ~33 labels), a single label-bucket rebalance triggered per-vertex span release with O(n) slab peeling — ~23 minutes wall time. Root cause was not PMA log folding but **per-vertex free-span retirement** on a path that DGAP never uses for routine insert.

A reference implementation lives at `reference/DGAP/dgap/src/graph.h`. DGAP keeps slack **inside** each leaf segment's assigned `[from, to)` interval and slides data with `rebalance_weighted`; it does not peel per-vertex footprints on every growth event.

Payload storage ([labeled-edge-inline-values.md](../storage/labeled-edge-inline-values.md)) already assumes edge compaction follows a **single logical order** across the edge slab. Divergent physical units (per-vertex span vs leaf segment) increase compaction risk and maintenance surface.

## Decision

**Adopt the core LARA / DGAP physical contract for labeled edge bytes.**

1. **Physical unit = PMA leaf block.** Up to `segment_size` vertices (default 16) in one leaf share one contiguous edge-slab reservation pinned by `span_meta.physical_start` and `segment_edges_total`.
2. **Density and rebalance** for labeled edge bytes are driven by leaf PMA counts, not isolated `VertexEdgeSpan` rewrite as the primary maintenance path.
3. **Weighted slide** (`rebalance_weighted_with_layout` / segment slide) redistributes labeled edge bytes and proportional label slack **inside the leaf window**, folding per-leaf overflow logs — mirroring DGAP `rebalance_data_V1`.
4. **Free-span release** applies to **retired leaf physical footprints** after segment relocate or slide completes — **not** per-vertex peel on routine labeled insert or bucket growth.
5. **Scan contract unchanged.** Queries continue to use `LabelBucket` + `LabelEdgeSpan` (and bypass rows for default label); they must not read `span_meta` or free spans.
6. **Interim per-vertex span path** remains only until migration steps land; new features must not deepen dependence on tail-append + per-vertex `release_span` as the steady-state growth model.

Phased delivery (see [lara-dgap-contract.md § Migration](../storage/lara-dgap-contract.md#migration-direction-labeled--contract)):

| Phase | Outcome                                                        |
| ----- | -------------------------------------------------------------- |
| A     | Pin labeled edge bytes to leaf `span_meta.physical_start`      |
| B     | Leaf PMA density triggers labeled maintenance                  |
| C     | Leaf rebalance: slide + log fold + bucket `edge_start` rewrite |
| D     | Single segment `release_span` on relocate                      |
| E     | Remove per-vertex tail append for normal labeled rows          |

Each phase keeps existing `mixed_label_hub_*` regressions green.

## New-bucket insertion span contract

`LabelBucket` rows are stored in ascending `BucketLabelKey` order, so the insertion index of a new label bucket is determined by its label id alone.

When a new bucket is created for an ordinary directed edge insert, storage must not ask the caller to pre-allocate a large edge span. The contract is:

1. **New buckets are inserted with `stored_slots = 0`, except the first bucket
   under the production segment16/quota1 policy.**
   - At creation time the bucket owns no edges, so it needs no edge-slab bytes.
   - Only the bucket row ordering is updated; no edge bytes are written.
   - The segment16/quota1 first bucket reserves one vertex edge-span slot before
     the first write; the bucket prefix becomes one slot when that edge is written.
     Buckets inserted between or after existing buckets remain zero-length.

2. **`edge_start` is placed at the next bucket's start, or at the vertex span end.**
   - If a bucket with a larger `BucketLabelKey` exists, the new bucket's `edge_start` equals that successor bucket's `edge_start`.
   - If the new bucket is the last one, `edge_start` equals the current last bucket's `edge_start + stored_slots`.
   - When the preceding bucket also has `stored_slots = 0` (for example, it was created earlier but has not yet been folded into the slab), the new bucket's `edge_start` is the same numeric value as the predecessor's `edge_start`. Both buckets therefore share a zero-length logical position until a leaf-wide rebalance assigns them distinct slab ranges.
   - This gives the new bucket a zero-length reservation that is logically contiguous with its neighbors and avoids overlapping spans.

3. **Subsequent edge inserts use the shared leaf overflow log.**
   - Because the new bucket has `stored_slots = 0`, `LabelEdgeSpanAccess` presents a CSR window with no slab slack.
   - `EdgeStore::insert_edge` therefore writes the first edges into the shared per-leaf overflow log.
   - The log chain is tracked by the bucket's `overflow_log_head`.
   - `LabelBucket::grow_packed_slab_by_one` (and `try_grow_packed_slab_by_one`) must not increase `stored_slots` while the bucket is log-backed; only the logical `degree` grows.

4. **Log pressure triggers leaf-level rebalance / relocate.**
   - When the log is full or the leaf becomes dense, `rebalance_edge_log_leaf_for_labeled` or `rebalance_cascade_after_labeled_mutation` folds the log back into the slab.
   - Otherwise, the weighted slide redistributes the leaf block across its active vertices and labels, giving the new bucket real `stored_slots`.
   - If the leaf block still cannot absorb the new label, `relocate_labeled_leaf_physical_block` or element-capacity growth is used.
   - After folding a single edge-only label, `LabeledVertex::stored_slots` may retain bounded geometric tail headroom inside that vertex's non-overlapping leaf allocation. `LabelBucket::stored_slots` remains the exact resident edge width; later edge-only inserts consume the vertex tail before returning to the shared log. This amortizes repeated full-log folds without changing the zero-length later-bucket contract.
   - Labels with inline values do not use this edge-only tail optimization. Their edge slab/log and payload slab/log continue to choose capacity and maintenance timing independently.

5. **Storage-owned preflight guarantees leaf capacity, not per-bucket pre-allocation.**
   - `prepare_labeled_edge_capacity_for_insert` runs before any canonical edge write for both forward `src` and reverse `dst`.
   - It ensures the target PMA leaf block is pinned and has room for the applicable
     zero-length or initial-quota placement.
   - It may rebalance or relocate the leaf, but it does **not** reserve `DEFAULT_SEGMENT_SIZE` slots for the new bucket itself.

6. **Tail append at `elem_capacity` is forbidden for normal new-bucket placement.**
   - `ensure_labeled_bucket_edge_span_room` must not fall back to `rebalance_vertex_edge_span` tail append when the leaf is pinned.
   - Growth happens through the PMA leaf slide / relocate / resize paths that the rest of LARA already uses.

### Segment size versus per-vertex quota

The production default for fresh labeled graphs is
`segment_size = 16` vertices per PMA leaf and a per-vertex edge quota of 1 slot.
These are related but distinct quantities:

```text
initial pinned leaf block = segment_size × vertex_edge_quota
                           = 16 × 1 = 16 edge slots
```

The quota is used for fixed-quota placement and the initial pinned leaf block.
Hub edge-span growth, relocation slack, tombstone pressure, and bypass
promotion use the persisted `segment_size` from the edge header. It is not a
dedicated allocation for every vertex or label bucket: later growth remains
owned by the PMA leaf slide, relocate, and resize paths.

The quota Cargo features and production-facing segment32 policy have been removed.
The low-level persisted segment arithmetic remains generic so storage algorithms can
validate arbitrary persisted headers and focused tests can exercise boundary sizes,
but fresh labeled graph construction selects only the 16/1 policy. Explicit
`new_with_segment_size` callers retain segment-size-derived quota arithmetic for
low-level geometry tests; this is not a production configuration surface.

### Historical experiments (not active policy)

The segment32 and quota16/8/4/1 comparison results are retained here as
decision evidence, not as build features or supported production variants. The
stable-backed sparse probes measured 1,921 pages for all tested policies; the
segment16/quota1 hub probe measured 2.19M instructions and eight leaf-cascade
rebalances versus 2.02M instructions and two rebalances for quota8. The data
supports the 16/1 sparse-memory policy while recording the accepted trade-off
of more relocation work for sparse slack minimization. Development stable data
must be recreated when this policy changes.

### Segment-size sizing evidence

DGAP's reference implementation computes the PMA leaf size from the graph
size rather than treating it as a universal constant: it starts with
`ceil(log2(num_vertices))`, rounds the segment count down to a power of two,
then recomputes `segment_size = ceil(num_vertices / segment_count)`. Thus the
leaf size is approximately logarithmic in the vertex count after power-of-two
alignment. For example, the implementation yields a segment size of 16 for
1,024 and 65,536 vertices, and 32 for 1,048,576 vertices. See the [DGAP
implementation](https://github.com/DIR-LAB/DGAP/blob/main/dgap/src/graph.h#L1139-L1150)
and [paper](https://daidong.github.io/files/dgap_sc23.pdf), §3.1.1.

The measurements support the selected `segment_size = 16` default rather than
assuming that 32 vertices per leaf is always appropriate. A stable-backed probe
with 1,024 vertices and one edge per vertex measured the following historical
comparison:

| segment size | quota | instructions | stable pages |
| ---: | ---: | ---: | ---: |
| 32 | 32 | 401.05M | 1,921 |
| 32 | 16 | 401.04M | 1,921 |
| 16 | 32 | 204.31M | 1,921 |
| 16 | 16 | 204.28M | 1,921 |
| 16 | 8 | 204.27M | 1,921 |
| 16 | 4 | 204.26M | 1,921 |
| 16 | 1 | 222.01M | 1,921 |

The segment-size reduction substantially lowers update work in this sparse
probe, while stable-memory pages remain dominated by the fifteen MemoryId
regions and extent/page rounding. The segment16 contract suite covers
tail-headroom scaling, deduplicated deferred-maintenance enqueue, and a
17-vertex hub/churn sequence crossing a PMA leaf boundary. These historical
results justify the selected default; they are not alternate runtime modes.

The quota1 hub/churn contract also bounds a one-edge leaf mate's
`stored_slots` to `1 + segment_size` after hub growth and maintenance, so leaf
relocation cannot continuously transfer hub capacity to sparse vertices.

The quota1 result is correctness-valid but more relocation-heavy; its lower
initial slack does not reduce the coarse stable-page footprint in this fixture.

### Current production canbench confirmation

The current unfiltered `ic-stable-lara` canbench run confirms that the
segment16/quota1 choice is visible in operation counts even though it does not
change the coarse stable-page result. Compared with the persisted pre-policy
baseline, the 1,024-edge hub insert measured `+21.65%` overall instructions,
with the leaf-cascade scope accounting for the increase; the 4,096-edge case
measured `+19.52%`. The isolated sparse-vertex grow measured `+24.42%`, while
the saturated-leaf total remained within noise (`-0.90%`). The stable-backed
sparse segment16 probe remained at 1,921 pages, and all focused probes reported
zero heap-page delta beyond their existing fixture allocations.

These are policy-plus-current-tree deltas, not an isolated segment-size A/B:
the baseline predates the segment16/quota1 default and subsequent allocator
work. They nevertheless confirm the expected trade-off: segment16/quota1
reduces initial sparse reservation, but increases leaf relocation/cascade work
in sparse and hub-growth workloads without reducing the rounded stable-page
footprint at this scale. The slot-based delete benchmark was also corrected to
remove high physical slots first; ascending removal invalidated later handles
after overflow-log unlink shifts and caused a fixture-only panic.

### Relationship to the DGAP reference model

The reference implementation in `reference/DGAP/dgap/src/graph.h` uses a single `vertex_element { index, degree, offset }` per vertex:

- `index` is the base slot in the shared edge array.
- `degree` is the logical live edge count.
- `offset` is the per-segment overflow log head.

DGAP inserts an edge by trying `index + degree` first; if no on-segment space exists, it writes to the log and folds the log back into the segment during `rebalance_weighted`. LARA's labeled layer maps this model onto multiple `LabelBucket` rows per vertex. The difference is that label buckets are created dynamically and in sorted order, so a newly inserted bucket needs a **zero-length logical position** (`stored_slots = 0`) at the successor boundary before any edges exist. This is the labeled equivalent of DGAP's `index` for a vertex that has just been allocated but owns no edges yet.

In other words:

| DGAP concept                            | Labeled LARA equivalent                                 |
| --------------------------------------- | ------------------------------------------------------- |
| `vertex_element.index`                  | `LabelBucket.edge_start`                                |
| `vertex_element.degree`                 | `LabelBucket.degree`                                    |
| `vertex_element.offset`                 | `LabelBucket.overflow_log_head`                         |
| per-vertex gap in `calculate_positions` | per-bucket gap in `calculate_label_edge_span_positions` |
| `rebalance_weighted`                    | `rebalance_labeled_leaf_weighted_slide_in_block`        |

Keeping later new buckets at `stored_slots = 0` keeps the labeled path aligned
with the DGAP reference. The segment16/quota1 first-bucket exception avoids
paying overflow-log and relocation cost for the common one-edge vertex.

### Why this is sufficient

- A new label typically receives only a few edges before the next maintenance pass.
- Those edges fit in the shared overflow log alongside other vertices in the same leaf.
- When the log is folded, the weighted slide allocates proportional slab space to the new bucket in one leaf-wide rewrite.
- Per-bucket `DEFAULT_SEGMENT_SIZE` pre-allocation is unnecessary and is explicitly rejected as the steady-state growth model.
- The sole initial reservation is one slot for the segment16/quota1 first-bucket
  candidate; it is not a 32-slot per-bucket allocation.

### Implementation impact

- `try_place_new_bucket_edge_span` must preserve zero-length successor placement,
  while reserving one initial vertex slot for the segment16/quota1 first bucket.
- `prepare_labeled_edge_capacity_for_insert` must check for a zero-length placement opportunity in the pinned leaf, not for a 32-slot free span.
- `ensure_labeled_bucket_edge_span_room` must not fall back to `rebalance_vertex_edge_span` tail append; instead it must report failure if a zero-length placement cannot be made (the caller must then drive leaf-level rebalance/relocate).
- `rebalance_vertex_edge_span` may still be used for vertex-level compaction, but not as the default growth path for a new bucket.
- Tests that assume a non-zero `stored_slots` immediately after the first insert must be updated to distinguish dense/slab-backed buckets from log-backed buckets.

## Independent edge and inline-value physical stores

The edge store and edge inline-value store share one logical contract—bucket-local live-edge order—but do not share physical slots, log entries, capacity, or maintenance timing.

- `LabelBucket::stored_slots` counts edge slab slots only. `overflow_log_head` belongs only to the edge leaf log.
- `LabelBucket::inline_value_slab_slots` counts inline-value slab entries only. `inline_value_log_head` and `inline_value_log_len` belong only to the payload leaf log.
- A label with `inline_value_byte_width = 0` always has zero inline-value slab slots and no payload log, regardless of its edge degree or edge physical state.
- A payload-bearing label allocates its first payload span lazily at one value-width entry; subsequent payload slab growth is measured in value-width entries and is not rounded to the edge segment size or edge quota.
- Edge rebalance, resize, and relocation preserve the current bucket-local live order but do not fold, resize, release, or relocate payload storage.
- Payload rebalance, resize, and relocation preserve the same current live order but do not fold, resize, release, or relocate edge storage.
- Insert appends to both logical sequences only when the label has a non-zero inline-value width. Delete resolves the edge physical slot to its bucket-local live ordinal and removes the value at that ordinal. Physical maintenance can then occur in either order.

This rejects physical slot equality and equal edge/payload log entry indices as an invariant. They are transient implementation coincidences and cannot represent labels with no inline value.

### Structural overflow fold versus slot compaction

Edge-log fold has two distinct execution contracts:

- **Foreground overflow delete is tombstone-free direct unlink.** Removing the head advances the
  head pointer. Removing a middle entry rewrites the one newer entry whose `prev` points to it.
  Newest-to-oldest scan order is unchanged; each live entry in the newer suffix shifts down one
  logical slot and emits one `EdgeSlotMove`. A valued bucket shifts the matching payload suffix.

- **Rebalance, resize, and relocation perform a structural fold.** They append every edge-log entry,
  including tombstones, after the existing edge slab prefix. Existing slab slots and log-backed
  bucket-local slot indices are preserved. These paths therefore require no `EdgeSlotMove` and do
  not re-key aliases, properties, or index postings.
- **Deferred edge maintenance folds the overflow suffix.** New deletes leave no tombstones, so the
  normal fold emits no moves. It still drops legacy tombstones and reports their bounded suffix
  moves for upgrade compatibility. The shared leaf edge log contains at most 170 entries.
- **Slab tombstones remain a separate incremental phase.** After the overflow suffix is folded,
  `compact_vertex_edge_span_one_step` moves at most one slab edge per maintenance work item. A short
  overflow fold must never collapse an arbitrarily large slab prefix.
- **Edge overflow maintenance does not fold the inline-value log.** Payload log fold and payload slab
  relocation remain independently triggered operations; edge compaction preserves the live ordinal
  sequence on which payload lookup depends.

Capacity for a structural fold is preflighted as `stored_slots + edge_log_chain_len`. Capacity for
the maintenance fold is preflighted as `stored_slots + live_edge_log_entries`. Neither operation
writes slab bytes before its complete destination range is known to fit.

### Relocation cost and contiguous slab reads

The leaf relocation path materializes every resident edge before committing the weighted slide.
The stored portion of a label bucket is already a contiguous slab prefix, so reading it one slot at
a time needlessly repeats the stable-memory read boundary. The relocation path therefore reads the
whole stored prefix with `read_slots_contiguous` and decodes the fixed-width records in memory.
Overflow-log entries remain a separate ordered read because they are not physically contiguous with
the slab prefix.

Focused `bench_labeled_stage2_hub_insert_grow` measurements in the same instrumented build showed
the relocation-heavy totals changing as follows:

| fixture | before contiguous read | after contiguous read | change |
| --- | ---: | ---: | ---: |
| 1,024 | 10.43M | 9.26M | -11.2% |
| 4,096 | 49.08M | 42.96M | -12.5% |
| 16,384 | 153.75M | 129.92M | -15.5% |

These are same-run comparisons, not a clean segment-policy A/B: the build also contains the
segment16/quota1 policy and canbench scopes. The remaining dominant cost is the weighted slide
commit itself, especially the encoding and writing of resident edge runs. Any further optimization
must preserve descending commit order, bucket-local live order, and the leaf-wide atomic relocation
contract.

The relocation plan also retains raw bytes for slab-only buckets. Those bytes are captured before
any destination writes and consumed during commit, so the fast path does not reread a source slot
that may already have been overwritten by an in-place slide. Buckets with overflow-log entries
continue through the decoded edge path because their logical order is assembled from two physical
sources. In the same 16,384 fixture, this reduced the total from 128.59M to 121.06M instructions;
the encode scope fell from 3.58M to approximately 1K instructions. The raw plan is transient only;
it does not alter the stable layout or the persisted bucket contract.

The `LabelBucket` stable record grows from 25 to 29 bytes to persist the payload slab-slot count independently. Development stable data using the earlier record width must be wiped; backward decoding is not provided.

## Consequences

### Positive

- Aligns labeled maintenance with proven DGAP/LARA complexity: slide inside fixed capacity vs repeated global tail append.
- Removes cliff class where multi-label hubs pay O(edges) free-span work per vertex relocate.
- One physical story for edge slab and payload compaction order.
- Reviewers can judge labeled PRs against a single contract ([lara-dgap-contract.md](../storage/lara-dgap-contract.md)).

### Negative / cost

- **Large refactor** across `labeled/graph/{compact,insert,bucket}.rs` and rebalance call graph.
- Multi-label **proportional slack** inside a leaf is harder than single-label DGAP; must preserve per-label CSR windows within the shared leaf block.
- Migration is **multi-PR**; interim and target paths may coexist briefly — requires clear feature flags or phase gates in tests.
- Stable-memory **on-disk layout** may need a version bump or one-time rewrite if existing graphs cannot be reinterpreted as leaf-pinned blocks (to be confirmed in Phase A spike).

### Neutral

- Default-label **bypass** rows may continue using core `Vertex` semantics where they already share the unlabeled path.
- `FreeSpanStore` remains core LARA (see [lara.md](../storage/lara.md)); in-window rebalance still does not retire spans — only segment relocate / slide does.

## Alternatives considered

### A. Keep per-vertex `VertexEdgeSpan` (status quo + optimize peel)

- **Pros:** Smaller immediate change; recent footprint fix unblocks correctness.
- **Cons:** Does not match DGAP/LARA rope model; hub-scale cost remains structurally likely; duplicates physical logic core LARA already implements.
- **Rejected** as steady-state architecture. Acceptable only as interim with explicit sunset (this ADR).

### B. Per-label physical spans (one slab reservation per label per vertex)

- **Pros:** Simpler mental model per label.
- **Cons:** Multiplies PMA units and free-span events by label count; worse on many-label hubs; diverges further from DGAP segment model.
- **Rejected.**

### C. Global `resize_V1`-only growth (no segment slide, full re-place on pressure)

- **Pros:** Matches DGAP when local windows cannot absorb density.
- **Cons:** Full-graph re-place is expensive on IC; LARA already invests in segment slide and free-span reuse to avoid this on every cascade.
- **Rejected** as the **only** strategy. Keep as escalation when leaf + window rebalance exhaust capacity (same as core LARA).

### D. Separate edge slab per label

- **Pros:** No cross-label physical coupling.
- **Cons:** Memory overhead, no shared PMA amortization, complicates federation and iterators.
- **Rejected.**

## References

- [lara-labeled-migration-tests.md](../storage/lara-labeled-migration-tests.md) — phase test gates
- [lara-dgap-contract.md](../storage/lara-dgap-contract.md)
- [lara-and-facade.md](../storage/lara-and-facade.md)
- `reference/DGAP/dgap/src/graph.h` — `rebalance_weighted`, `rebalance_data_V1`, `vertex_element`
- `crates/ic-stable-lara/src/labeled/graph/compact.rs` — interim per-vertex span release
