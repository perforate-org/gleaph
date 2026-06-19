# 0022. Labeled overflow-log read-window fix now; degree-driven hub edge storage later

Date: 2026-06-19
Status: accepted (Stage 1 implemented 2026-06-19; Stage 2 deferred)
Last revised: 2026-06-19

Refines [ADR 0001](0001-labeled-segment-slide.md) (labeled edge physical layer
uses a PMA leaf segment slide) and interacts with
[ADR 0016](0016-overflow-log-tombstones-and-src-fields.md) (per-leaf overflow
log), [ADR 0020](0020-deferred-maintenance-timer-drain.md) (timer-driven
maintenance), and [ADR 0021](0021-resumable-supernode-detach-delete.md)
(resumable super-node `DETACH DELETE`).

> **Investigation note (2026-06-19).** This ADR was first drafted around the
> hypothesis that the synchronous *growth resolver*
> (`resolve_labeled_edge_base_for_growth`) bailed with
> `CollectAllocationOverflow` after four leaf relocations, to be fixed by an
> in-leaf reservation or a pin→unpin "dedicated-span promotion". A reproduction
> built from the failing workload **disproved that hypothesis**: the growth bail
> never fired. The actual wedge is a **read-path slab-window underflow** for the
> synthetic single-bucket access used to scan a labeled bucket that has spilled
> into the per-leaf overflow log. The sections below record the corrected
> diagnosis and the implemented fix (Stage 1). The degree-driven hub-storage idea
> (per-bucket dedicated spans, then a B-tree tier) survives only as *deferred*
> Stage 2 work for skewed-bucket isolation, which is a separate concern from this
> defect; each tier's necessity and thresholds are benchmark-gated.

## Context

The labeled edge store (`crates/ic-stable-lara/src/labeled/graph/`) places a
vertex's `(vertex, label)` edge buckets inside a **PMA leaf** that is *pinned* to
a contiguous physical block in the edge slab (ADR 0001). A leaf spans
`segment_size = 32` consecutive vertices; the pinned block is
`segment_size * quota = 32 * 32 = 1024` slots
(`labeled_leaf_physical_block_len`), so each vertex's slab window inside the
block is initially `quota = 32` slots. When a bucket needs more than its slab
window before the leaf relocates, excess edges spill into the **per-leaf overflow
log** (ADR 0016); the bucket then has `log_head >= 0` and its `stored_slots`
exceeds the on-slab window width.

Labeled bucket edges are scanned through `LabelEdgeSpanAccess`
(`labeled/access.rs`), a synthetic two-row vertex column presented to the core
`EdgeStore` scan helpers: **row 0** is the live bucket; **row 1** is a synthetic
successor whose `base_slot_start` is the caller-supplied `successor_start` (the
next bucket's `edge_start`, or the containing VertexEdgeSpan end). The access
always reports `len() == 2` and exposes the bucket as `v_ord == 0`.

Lineage: the design descends from PMA-based mutable CSR (Bender/Hu; PCSR; VCSR)
and DGAP (per-section edge log ≈ our per-leaf overflow log). The notion of
storing *high-degree* vertices differently from low/medium-degree vertices is the
core idea of **Terrace** (SIGMOD 2021): in-place for low degree, a shared PMA for
medium degree, and **per-vertex B-trees for high degree**. That idea informs the
deferred Stage 2, not the Stage 1 defect fix.

## Problem

`EdgeStore::slab_window_exclusive_end` (`lara/edge/row_layout.rs`) computes a
row's on-slab window end. Its hot path keys off `v_ord` to find the row's PMA
leaf and, when that leaf is pinned, clamps the window end to the leaf's physical
block cap:

```text
leaf  = v_ord / segment_size
cap   = span_meta[leaf].physical_start + counts[leaf].total
end   = next_base.min(cap)              # ← clamp to leaf block
```

For a **real** vertex row this is correct: the row's edges live inside its leaf's
block, so `base < cap` and `next_base.min(cap)` is a valid tightening.

For the **synthetic `LabelEdgeSpanAccess`**, `v_ord == 0` always, so the clamp
reads **leaf 0's** physical cap regardless of where the bucket's edge bytes
actually live. A bucket whose `edge_start` sits in a *later* physical leaf block
(any vertex past the first edge-leaf) then has `base >= cap_leaf0`, and
`next_base.min(cap_leaf0)` places the window **end before `base`**. The window is
only consulted when the overflow log is active (`on_slab_edges_with_layout`
returns `stored_degree` directly for clean rows), so `next_exclusive - base`
underflows and surfaces as `LaraOperationError::CollectAllocationOverflow`.

Observed evidence: `bench::large::tests::large_friends_of_friends_setup_and_execute`
(256 friends × 64 = ~16.6 K edges) reliably hit
`Graph(Forward(Store(CollectAllocationOverflow)))` during setup. The failure
originates **not** in the growth resolver but in
`labeled_bucket_span_iter` → `EdgeStore::out_edges_iter` (the *descending* scan)
invoked by `GraphStore::find_first_forward_handle_descending`, the existing
forward edge-handle lookup that `insert_directed_edge` performs on every insert.
The first friend (`vid = 16385`, after all 16 384 candidates) carried a bucket
with `edge_start = 1 049 632`, `stored_slots = 33`, slab window `= 32`,
`log_head = 0`: an overflow-log bucket whose base is far past leaf-0's cap. The
reduced 16 × 8 smoke test never reproduced because its `second_hop = 8` stays
under the 32-slot quota, so no bucket ever activates the overflow log.

Trigger conditions (all required): a labeled bucket (1) with an active overflow
log (`log_head >= 0`), (2) whose `edge_start` is at or past leaf-0's physical
cap, read (3) via the descending scan. This is reachable in production on any
graph large enough to place an overflowing labeled bucket beyond the first
edge-leaf block — independent of native-vs-wasm maintenance budgets.

Severity: the edge insert returns `Err` (graceful, rolled back via
`execute_plan_update → Result<_, String>`), not a trap — but a **correct, valid
read fails**, blocking the insert. It is a storage-core **correctness defect**,
in scope of the low-level-vulnerability goal.

## Existing Architecture Assessment

The fix belongs entirely inside `slab_window_exclusive_end`, the single owner of
slab-window geometry.

- The synthetic access already supplies the authoritative window boundary
  (`successor_start` as row 1's base). The leaf-block cap is a *secondary*
  tightening that is only meaningful when the row's edges actually live inside the
  indexed leaf's block. The bug is applying that cap when `base` is past it.
- No new state, module, or invariant is required. The other cap branches in the
  same function already cannot underflow for real rows (`base < cap` holds), and
  the synthetic access only ever reaches the hot-path branch (`len() == 2`,
  `v_ord == 0`).
- The overflow log itself is working as designed (ADR 0016): a bucket *may*
  legitimately keep edges in the log between maintenance passes. The reader must
  tolerate that state; it did not.

Conclusion: this is a localized read-geometry correctness fix, not a new
subsystem and not a change to the growth/placement policy.

## Alternatives

### A. Clamp the window end to never precede `base` (chosen, Stage 1)
In the pinned-leaf hot path, apply the leaf-block cap only when `base < cap`;
otherwise the next-bucket boundary (`successor_start`) is authoritative:

```rust
let end = if base < cap { next_base.min(cap) } else { next_base };
```

- Benefit: one-line, behavior-identical for every real vertex row (`base < cap`
  always holds), fixes the synthetic-access underflow at its source; no new state.
- Drawback: leaves the *policy* question (should an overflowing medium-degree
  bucket instead get a larger slab window via relocation?) to maintenance, which
  already handles it.
- Verdict: chosen — minimal correct fix for the demonstrated defect.

### B. Clamp at the consumer (`on_slab_edges_with_layout` saturating-sub)
Replace `checked_sub` with `saturating_sub` so a too-small window yields span 0.

- Drawback: hides the wrong window instead of computing the right one; span 0
  would mis-report on-slab edges as all-in-log, producing **wrong reads** (the
  failing bucket has 32 edges genuinely on the slab). Wrong, not just opaque.
- Verdict: rejected.

### C. Teach `LabelEdgeSpanAccess` to expose the bucket's true leaf
Carry the real `v_ord`/leaf so `slab_window_exclusive_end` reads the right cap.

- Drawback: larger surface change for no behavioral gain over A — the access
  already provides the tight, correct boundary via `successor_start`; the cap adds
  nothing for a single-bucket view.
- Verdict: rejected (over-engineered).

### D. Degree-driven hub edge storage (dedicated spans, then a B-tree tier)
The original Stage 1/Stage 2 idea: escalate skewed edge sets out of the shared
leaf into a dedicated span (medium degree), and above a higher threshold into a
B-tree tier (high degree, Terrace-style).

- Status: **does not fix this defect** (the wedge is a read-geometry bug, not a
  growth-capacity bug). It remains valuable for isolating skewed edge sets
  (memory locality, cheaper `DETACH DELETE` per ADR 0021) and is retained as a
  **deferred Stage 2** — not pursued now.
- **Granularity (decided): per labeled bucket `(vertex, label)`, not per vertex.**
  Skew in real graphs lives at the `(vertex, label)` level (e.g. one celebrity's
  `FOLLOWED_BY` bucket), so promoting the whole vertex would needlessly move its
  small buckets off the cache-friendly slab. Per-bucket also matches the existing
  iteration boundary — edges are already scanned bucket-by-bucket via
  `LabelEdgeSpanAccess`, and `LabelBucket` already carries a backing-store state
  machine (slab prefix → per-bucket overflow log). Both escalations add a *third*
  and *fourth* backing state to that same per-bucket descriptor (slab → log →
  dedicated span → B-tree), rather than a new vertex-level concept.
- **Gating (decided): benchmarks decide *both the necessity and the thresholds*
  of each tier — the dedicated-span tier included, not only the B-tree tier.**
  Neither escalation is assumed beneficial a priori; each is justified only by
  measured update/delete churn, `DETACH DELETE` cost, scan locality, and
  fragmentation. The promotion (and demotion) degree thresholds are tuned from the
  same measurements, with hysteresis to avoid promote/demote thrash.

## Decision

1. **Stage 1 (done): read-window clamp (Alternative A).** In
   `slab_window_exclusive_end`, gate the leaf-block cap on `base < cap` so the
   computed slab-window end can never precede `base`. This fixes the descending
   (and ascending) scan of any overflow-log labeled bucket whose edge bytes live
   past leaf-0's physical block.

2. **Stage 2 (deferred): degree-driven hub edge storage (Alternative D).**
   Escalate at **labeled-bucket granularity `(vertex, label)`**, in two tiers:
   - **Medium degree → dedicated span:** evacuate a hot bucket from the shared
     leaf block into its own PMA-managed span.
   - **High degree → B-tree tier:** above a higher threshold, store the bucket's
     edges in an ordered map tier.

   **Both tiers' necessity and degree thresholds are decided by benchmarks** — the
   dedicated-span tier is *not* assumed; it must earn its place on measured
   update/delete churn, `DETACH DELETE` cost, scan locality, and fragmentation,
   exactly like the B-tree tier. Thresholds carry hysteresis. Pursued only on
   benchmark evidence; via a separate ADR or amendment.

   Recorded design constraints for the eventual B-tree tier (contingent, not yet
   implemented):
   - **The edge order contract is CSR slot order = insertion (append) order, not
     target order** (`OutEdgeOrder::Ascending` = "CSR slots low→high", the stable
     materialization order; `insert_edge` appends). Worse, the **`slot_index` is an
     edge's stable identity**: `EdgeHandle { owner_vertex_id, label_id, slot_index }`
     keys the edge-property sidecar (`EDGE_PROPERTIES`), the undirected alias store
     (`EDGE_ALIASES`), and postings. A target-keyed map would both reorder edges
     *and* destroy this positional identity — so **the B-tree key must NOT be the
     target.** (This corrects the earlier `(VertexId, BucketLabelKey, target)`
     sketch.)
   - **Key shape `(VertexId, BucketLabelKey, seq)`** where `seq` is a monotonic,
     never-shifting per-bucket sequence id that *is* the `slot_index` analog.
     Forward range scan over the `(VertexId, BucketLabelKey)` prefix = seq ascending
     = `OutEdgeOrder::Ascending` (insertion order); reverse = `Descending`. This
     preserves both the order contract and stable edge identity. The dedicated-span
     tier needs no such care — it is still a CSR span, so slot order/identity carry
     over unchanged.
   - **Use storage-local `VertexId`, not the federation `LocalVertexId`.** Keep
     `LocalVertexId ↔ VertexId` translation above the LARA boundary so the general
     storage crate stays free of federation concepts.
   - **Target lives in the value, not the key.** A dedicated target struct holding
     only `{ remote flag, id }` (tombstone/payload bits stripped from `VertexRef`,
     `Ord` derived with an order matching the existing scan order, pinned by tests)
     is appropriate either as the value's target field or as the key of an optional
     secondary index `target → seq`. Tombstones are never keys — a B-tree delete
     removes the `seq` key (O(log d)), avoiding slab tombstone + compaction.
   - **Point lookup by target stays O(degree)** with a seq-keyed map — but that is
     exactly today's cost (`find_first_forward_handle_descending` already scans).
     The B-tree's real win is O(log d) delete-by-`seq` with no compaction and no
     O(degree) contiguous-span relocation (the ADR-0021 super-node pain). An
     optional `target → seq` secondary index buys O(log d) target lookup at ~2×
     entries; **whether that index is warranted is itself a benchmark decision.**
   - **One shared ordered map, not one map per vertex/bucket.** A literal
     per-vertex/per-bucket `StableBTreeMap` is infeasible (each instance needs its
     own `MemoryId`; the memory manager caps virtual memories at 255). "Per-bucket
     B-tree" is *logical*, realized by the `(VertexId, BucketLabelKey)` key prefix
     over a single map. Candidates: `ic-stable-structures::StableBTreeMap` or the
     in-repo `ic-stable-paged-ordered-map`.
   - **Value reuses the existing edge-payload representation / `EdgePayloadStore`
     indirection** (single source of truth for payload schema), not a new
     variable-length value format; large blobs stay referenced by handle.

## Consequences

### Positive
- The full-scale 256 × 64 friends-of-friends build succeeds; any graph with
  overflowing labeled buckets past the first edge-leaf can be scanned and have
  edges inserted.
- The fix is one branch in the single owner of slab-window geometry; no new state,
  module, or invariant.
- Behavior is provably unchanged for real vertex rows (`base < cap` invariant).

### Trade-offs
- Stage 1 does not change placement policy: a medium-degree bucket may transiently
  keep edges in the overflow log until maintenance relocates it. That is the
  existing ADR-0016 design and is now correctly readable.
- Per-bucket skew isolation (Stage 2: dedicated span and B-tree tiers) remains
  future work; each tier's necessity and thresholds are benchmark-gated.

## Migration

### Stage 1 — read-window clamp (done)
1. LARA reproduction added: `overflow_log_bucket_past_leaf0_cap_reads_descending`
   (`labeled/graph/compact.rs`) builds an overflow-log bucket whose base is past
   leaf-0's cap and asserts the descending read succeeds. Verified to fail with
   `Store(CollectAllocationOverflow)` before the fix and pass after.
2. Fix applied in `EdgeStore::slab_window_exclusive_end` (`lara/edge/row_layout.rs`).
3. Validated: full `ic-stable-lara` suite (321 tests) + `gleaph-graph` default
   suite (590 tests) + `bench::large::tests::large_friends_of_friends_setup_and_execute`
   at full 256 × 64 scale + `cargo clippy -p ic-stable-lara`.
4. Reverted the interim smoke-test downscale: restored
   `large_friends_of_friends_setup_and_execute` to 256 × 64 (undo `f4c93bc3`).

### Stage 2 — degree-driven hub edge storage (deferred; separate ADR on evidence)
Escalation is per labeled bucket `(vertex, label)`; benchmarks decide both the
necessity and the thresholds of *each* tier (the dedicated span included).

1. Benchmark shared-leaf vs dedicated-span vs B-tree backing for skewed buckets:
   insert/update/delete churn, point lookup, full-bucket scan locality,
   `DETACH DELETE` cost (ADR 0021), and slab fragmentation.
2. Decide, per tier, **whether it is warranted at all** and, if so, its promote /
   demote degree thresholds (with hysteresis).
3. If the dedicated-span tier is warranted: add the per-bucket pin→unpin
   transition (evacuate the bucket to a span, release leaf footprint, update
   `span_meta`/segment counts and the bucket descriptor's backing state).
4. If the B-tree tier is warranted: prototype the single shared ordered map keyed
   by `(VertexId, BucketLabelKey, seq)` — a stable monotonic sequence id, NOT the
   target, to preserve insertion order and the `slot_index` stable identity (see
   recorded design constraints in the Decision). Measure scan regression vs the
   span tiers, decide whether a `target → seq` secondary index is warranted, and
   wire the per-vertex scan to union slab/log/span/tree buckets in label order.
5. Capture the outcome in a separate ADR or an amendment here.

## Design Documentation Impact

| Document | Update | Status |
|----------|--------|--------|
| [adr/README.md](README.md) | Index ADR 0022 | this patch |
| [adr/0016-overflow-log-tombstones-and-src-fields.md](0016-overflow-log-tombstones-and-src-fields.md) | Note that labeled overflow-log buckets are scanned via the synthetic `LabelEdgeSpanAccess`, whose slab-window end is bounded by `successor_start` (not the leaf-0 cap) | this patch |
| [adr/0001-labeled-segment-slide.md](0001-labeled-segment-slide.md) | (Stage 2 only) degree-driven dedicated-span promotion as a leaf-pin escape | on Stage 2 |
| [adr/0021-resumable-supernode-detach-delete.md](0021-resumable-supernode-detach-delete.md) | (Stage 2 only) a B-tree hub tier would simplify resumable purge | on Stage 2 |

## Required Axes Impact (adr-review)

- **Encapsulation:** Stage 1 stays inside `EdgeStore::slab_window_exclusive_end`,
  the sole owner of slab-window geometry; no cross-API surface changes.
- **Separation of concerns:** read-geometry correctness is a storage-core concern;
  no planning/execution/index logic is introduced. Placement policy is untouched.
- **Invariants:** restores "a row's slab-window end is `>= base`"; for the
  synthetic single-bucket access the authoritative boundary is `successor_start`.
  No invariant ownership moves.
- **Consistency:** canonical state stays the edge slab + buckets + overflow log;
  the fix only corrects a derived read boundary, so reads now agree with the
  stored layout.
- **Fitness for purpose:** Stage 1 is the minimal correct fix for the demonstrated
  defect; the broader degree-driven storage redesign is explicitly deferred behind
  measurement (Stage 2) so we do not over-generalize before evidence. No
  Gleaph/ICP specifics leak into the general LARA crate.
