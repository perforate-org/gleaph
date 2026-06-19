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
   - **Status-quo (shared-leaf) baselines landed** in `labeled/bench.rs` for a
     1024-edge single `(vertex, label)` hub (canbench, persisted):
     `bench_labeled_stage2_hub_delete_half_then_compact_1024` (~308.66M ins;
     ~597K ins/delete via `remove_edge_skip_leaf` — the O(degree) delete the
     B-tree tier targets), `bench_labeled_stage2_hub_point_lookup_descending_1024`
     and `bench_labeled_stage2_hub_scan_descending_1024` (~16.61M ins each; point
     lookup == full scan, confirming O(degree) target lookup).
   - **Dedicated-span warrant probe landed** (`..._saturated_leaf_grow_one_256`
     vs `..._isolated_vertex_grow_one_256`): growing one vertex to 256 edges costs
     ~4.90M ins inside a saturated leaf vs ~3.23M ins isolated (leaf-mates empty ≈
     a dedicated span). The ~1.67M-ins delta (~6.5K ins/insert, driven by
     `rebalance_leaf_cascade`) is the cost a dedicated span would recover.
   - **B-tree (2b) prototype landed** (`labeled/hub_tree_prototype.rs`, evidence-only,
     not wired into the graph; benches `bench_labeled_stage2b_btree_hub_*`). A shared
     `StableBTreeMap` keyed by `(vertex, label, seq)` holds one 1024-edge hub bucket
     and is exercised on the same three operations as the slab baselines (canbench,
     persisted): delete-half-by-`seq` + survivor scan **45.65M ins** (vs slab
     308.66M, **6.76× faster**); point lookup descending **103.76M ins** and full
     descending scan **104.65M ins** (vs slab ~16.61M each, **~6.3× slower**,
     ≈2004 ins/edge vs ≈318 ins/edge on the slab).
   - **Value-layout experiment landed** (`bench_labeled_stage2b_narrow_*` and
     `..._scan_descending_keyonly_*`), testing whether the read regression is a
     value-width problem. The persisted edge row is 4 bytes (`Edge::BYTES`; payloads
     already externalized to `EdgePayloadStore`/`EdgePropertyStore`), so the value is
     never variable-length. Results: shrinking the B-tree value 10B → 4B changes
     nothing (scan 104.40M → 104.96M; delete 45.56M → 44.70M; lookup 103.51M →
     104.12M, all within noise). Reading **only the key** (value never deserialized,
     i.e. the best case of moving `target` into the key) cuts scan to 71.74M — still
     **~4.3× the slab** (≈1373 vs ≈318 ins/edge). Value-deser is ≈626 ins/edge;
     B-tree node traversal + key-deser is the dominant, irreducible-by-layout cost.
   - **Fair delete-by-handle baseline + insert costs landed**, and they change the
     conclusion. The original `..._delete_half_then_compact_1024` (~308.66M) deletes
     by `remove_edge_matching`, which pays an O(degree) **find-scan** production never
     pays — edges are deleted by `EdgeHandle{owner,label,slot_index}` via
     `remove_edge_at_slot` (O(1) slab tombstone for prefix slots; O(chain) only for
     overflow-log slots). The honest baseline
     `bench_labeled_stage2_hub_delete_half_by_slot_then_compact_1024` is **36.93M**
     (compaction scopes total only ~0.25M; the rest is overflow-log chain walks). And
     insert on the paid path: slab `insert_edge` grow 0→1024 = **15.53M**
     (`bench_labeled_stage2_hub_insert_grow_1024`) vs B-tree **56.37M**
     (`bench_labeled_stage2b_narrow_hub_insert_1024`), i.e. the B-tree insert is
     **3.6× slower**.
   - **Still to measure:** crossover degree where the slab's O(degree) overflow-log
     delete loses to B-tree O(log d); facade-level `DETACH DELETE` cost; slab
     fragmentation.

   **Warrant verdict (2026-06-19):** the dedicated-span tier (2a) shows only a
   *modest* benefit — ~6.5K ins/insert of recovered growth contention — while the
   B-tree tier (2b) addresses the *dominant* cost, ~597K ins/**delete** (≈90×
   larger per operation). Evidence therefore prioritizes **2b over 2a**; the
   dedicated-span tier is not warranted on growth contention alone (its remaining
   case is fragmentation/locality, still unmeasured). The high-value, high-risk
   2b prototype is the next implementation candidate, not 2a.

   **2b prototype verdict (2026-06-19):** the B-tree tier is a **conditional**, not
   a free, win. It cuts delete churn ~6.76× (the dominant cost), but a naive
   whole-bucket move **regresses the read paths ~6.3×** (descending scan and point
   lookup), because the slab scans contiguous bytes (~318 ins/edge) while the B-tree
   pays node traversal + per-entry key/value deserialization (~2004 ins/edge). So 2b
   is only net-positive for **delete/insert-churn-dominated** hubs; for read-heavy
   (traversal-scan) hubs it loses. This means the promotion trigger must be
   **churn-aware, not degree-alone**, and the tier must keep scans cheap before it is
   production-warranted. A `target → seq` index would help lookup but not full scans.

   **Value-layout experiment verdict (2026-06-19):** the read regression is **not** a
   value-width problem and is **not fixable by value layout**. Shrinking the value to
   4 bytes (the real `Edge` width) or splitting target/payload into separate trees
   gives no scan benefit, because production edge payloads are already external (the
   slab row is target-only). Moving `target` into the key (value-free scan) is the
   only layout change that helps, and only by ~31% (to ~4.3× the slab); B-tree node
   traversal + key deserialization dominate. Therefore do **not** pursue the
   two-tree / payload-split idea for the hub tier, and do not expect a packed value to
   rescue scans. The viable path is to keep the contiguous slab as the read path and
   confine the B-tree (or another structure) to churn-heavy, scan-light hubs under a
   churn-aware trigger — or to reconsider whether a non-B-tree structure (e.g. a slab
   "target column" plus a small free/index map) better fits the
   delete-win-without-scan-loss goal.

   **IC cost-structure verdict (2026-06-19) — supersedes the "6.76× delete win"
   above.** Weighing the tiers in Internet Computer economics (query calls: free,
   5B-instruction limit; update calls: metered at ~$0.00137/1B, 40B limit) flips the
   conclusion, because only **update** (mutating) work costs money and the B-tree's
   regressions land mostly on **free** reads:
   - **Paid update path, degree 1024** (the only dollar-bearing ops): the slab wins
     **both** mutations once delete is measured fairly by handle. Insert grow 0→1024:
     slab **15.53M** vs B-tree **56.37M** (B-tree **3.6× more expensive**).
     Delete-half by handle + compact + scan: slab **36.93M** vs B-tree **44.70M**
     (B-tree **~1.2× more expensive**). The prior 6.76× "win" was entirely an
     artifact of charging the slab an O(degree) find-scan (`remove_edge_matching`)
     that production never pays.
   - **Free query path**: the B-tree's 6.3× scan/lookup regression costs **no money**
     and stays well within the 5B/call limit at hub scales (a single 1024-edge
     descending scan is ~2.0M ins ≈ 0.04% of 5B; the slab is ~0.0065%). The slab also
     keeps ~6× more single-call scan headroom before the 5B limit bites (~15.7M vs
     ~2.5M edges/call), but neither is a practical constraint at realistic degrees.
   - **Conclusion: the Stage 2b B-tree tier is NOT warranted at hub degrees up to
     ~1024.** It increases metered cost on every mutation and worsens (free) reads.
     The remaining open question is the **crossover degree**: the slab's overflow-log
     delete is O(degree) per log-resident edge (chain walk), so at degrees ≫1024 the
     slab delete eventually loses to B-tree O(log d) — but the slab would still win
     insert and (free) scan there, so even then 2b is at best a narrow,
     degree-and-churn-gated optimization. The higher-leverage fix is the slab's
     unindexed **overflow log** itself (index/segment the log so log deletes and
     scans stop being O(degree)), which preserves cheap inserts and free-query scans
     instead of paying the B-tree tax on every write.
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
