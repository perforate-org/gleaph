---
name: "ADR 0088 Tree-CSR amend (cap semantics)"
overview: "Amend ADR 0088 to clarify that `T_PROMOTE` and `R_MAX` are caps on `alloc_space = stored_slots + alloc_gap` (NOT on `stored_slots` alone). The amend was added after the canbench Gate 2/3/4 re-validation in Plan 0315/0316/0322, and is a prerequisite for Plan 0318 (Tree-CSR production wire-up) which enforces this cap semantics in production code. **This plan is documentation-only** (no production code changes). The actual production wire-up of the LTB store, tree-mode flag bit, mode dispatch, etc. lives in Plan 0318 (was originally folded in here but split out for scope manageability)."
todos:
  - id: "cap-semantics-amend"
    content: "**Documentation-only amend of ADR 0088 §Decision**: clarify that `T_PROMOTE = 4096` and `R_MAX = 1024` are caps on `alloc_space = stored_slots + alloc_gap` (NOT on `stored_slots` alone). This is a documentation change only; no production code changes are made in this plan. The production code that enforces this cap semantics lives in [Plan 0318](./0318-tree-csr-implementation.md) Step 3 (`check_alloc_cap` / `check_vertex_bucket_count_cap` / `compute_bucket_allocation` helpers) and the cap helpers' integration into `insert_edge` / `remove_edge_at_slot` in Plan 0318 Steps 5-6. The amend was added after the canbench Gate 2/3/4 re-validation in Plan 0315/0316/0322 surfaced a subtle gap: the prototype's `compute_bucket_allocation` was implemented as `stored_slots + alloc_gap` for slab mode but the cap check was on `stored_slots` alone. The amend unifies the two. Cap semantics: (a) `T_PROMOTE = 4096` slots = 16 KiB for CSR slab mode; `alloc_space = stored_slots + alloc_gap ≤ T_PROMOTE`. (b) `R_MAX = 1024` slots = 4 KiB for tree mode; `alloc_space = stored_slots ≤ R_MAX` (gap-0 invariant). (c) LARA rebalance naturally keeps `alloc_space = stored_slots + alloc_gap` bounded by `T_PROMOTE` via weighted gap allocation. (d) `MAX_BUCKETS_PER_VERTEX = 1024` is a per-vertex edge-label-type limit (independent of the 16-bit label id space which is wire-level). (e) Industry comparison: DataStax Enterprise Graph imposes an official cap of 200 edge labels per graph — Gleaph's 1024 is 5× larger. (f) Worst case leaf footprint: 16 vertices × 1024 buckets × 16 KiB = 256 MiB. (g) IC deterministic memory tracker cost (commit b722538, 2026-07-23): 5000 ins per 4 KiB page read + 5000 ins per page write + 3000 ins per page copy; per-slot (4 bytes) relocation = ~12.7 ins; worst case 256 MiB leaf relocation = 8.5 × 10⁸ ins = 2.1% of 40B update budget — well within safety."
    status: completed
  - id: "label-bucket-tree-mode-bit"
    content: "**Moved to Plan 0318 Step 1 (commit `e70f43534`).** Add a tree-mode flag bit (bit 63, the high bit of the packed `word`) to `LabelBucket`. Provide `is_tree_mode() -> bool`, `with_tree_mode(enabled: bool) -> Self`, and update `try_from_parts` / `try_read_from` / serialization / round-trip tests to keep `LabelBucket::BYTES == 29`. Update the reserved-bits validator so bit 63 is the tree-mode bit (not a rejected reserved bit). Update `bucket_word_has_zero_reserved_bits` to ignore bit 63 or remove the check on that bit while keeping it on bits 60–62."
    status: completed
  - id: "ltb-store-as-graph-field"
    content: "**Moved to Plan 0318 Step 2 (commit `a8ad189d3`).** Add `ltb: LtbRawBlockStore<M>` field to `LabeledLaraGraph<E, M>` (single orientation). Update the `LabeledLaraGraph::new(...)` and `init(...)` constructors to accept one extra `M: Memory` argument for the LTB store. Update the bidirectional graph (`BidirectionalLabeledLaraGraph`) to hold two `LtbRawBlockStore<M>` (forward + reverse), accepting 2 extra `M` arguments total. Update the test fixture helpers (`labeled_lara_memories()`, `failpoint_labeled_memories()`) and `LabeledLaraGraph::init` reopen validation (count consistency, free-list walk up to declared envelope)."
    status: completed
  - id: "promote-bypass-to-tree-mode"
    content: "**Moved to Plan 0318 Step 4 (pending).** Implement `promote_bypass_to_tree_mode(vid, label)` in `LabeledLaraGraph` (single orientation). Follow the `promote_bypass_to_bucket_mode` failure-atomic template (reserve / commit / publish). When `alloc_space = stored_slots + alloc_gap` crosses `T_PROMOTE = 4096` (the cap is on `alloc_space`, not `stored_slots` alone — see Step 3.5 for the cascade), mint all data blocks in the LTB store, transcribe the slab prefix + unfolded log entries in logical order into blocks, write the root region, publish the descriptor with the new tree-mode flag bit set, and release the old edge span (and old inline-property span if applicable)."
    status: pending
  - id: "alloc-space-cap-enforcement-cascade"
    content: "Enforce `alloc_space ≤ cap` via cascade from bucket to vertex layer. Gleaph operates as an IC actor model graph database with multi-canister federation; label id space is16-bit (65536) shared federation-wide — independent of MAX_BUCKETS_PER_VERTEX. The cap on bucket count per vertex is1024 (the number of distinct edge-label types incident to one vertex). **Important: `T_PROMOTE` and `R_MAX` are caps on `alloc_space = stored_slots + alloc_gap` (NOT on `stored_slots` alone). For CSR slab mode, `alloc_space = stored_slots + alloc_gap ≤ T_PROMOTE = 4096 slots (16 KiB)`. For tree mode, `alloc_space = stored_slots ≤ R_MAX = 1024 slots (4 KiB)` (gap-0 invariant). LARA rebalance naturally keeps `alloc_space = stored_slots + alloc_gap` bounded by `T_PROMOTE` via weighted gap allocation.** Vertex cap: MAX_BUCKETS_PER_VERTEX = 1024 (per-vertex edge-label-type limit). Industry comparison: DataStax Enterprise Graph imposes an official cap of200 edge labels per graph — Gleaph's 1024 is5x larger, the largest among major graph DB products. Typical graph schemas use< 100 edge labels per vertex; 1024 covers all practical workloads (social graphs, EC, knowledge graphs, IoT, finance) with10-200× schema evolution headroom. Worst case leaf footprint: 16 vertices × 1024 buckets ×16 KiB = 256 MiB. IC deterministic memory tracker (commit b722538,2026-07-23) charges5000 instructions per4 KiB page read AND5000 per page write, plus3000 per page copy overhead. Per-slot (4 bytes) relocation cost = ~12.7 instructions. Worst case 256 MiB leaf relocation = 8.5 ×10⁸ instructions = 2.1% of40B update budget — well within safety. Implement `check_alloc_cap(bucket, increment)` returning `AllocSpaceCapReached { current_alloc_space: stored_slots + alloc_gap, cap, mode }` when `alloc_space + increment > cap(bucket.mode)`. Implement `check_vertex_bucket_count_cap(vertex)` returning `VertexBucketCountCapReached { current_count, cap: MAX_BUCKETS_PER_VERTEX }` when vertex bucket count reaches the cap. Use `compute_bucket_allocation(bucket) = alloc_slot.min(cap)` where `alloc_slot = stored_slots + alloc_gap(stored_slots)` for slab mode and `alloc_slot = stored_slots` for tree mode. Audit LARA rebalance / leaf-pin paths. Unit tests: (a) CSR slab bucket at alloc_space = T_PROMOTE -1 → trigger promote; (b) tree mode bucket at alloc_space = R_MAX - 1 → trigger deepen; (c) vertex with MAX_BUCKETS_PER_VERTEX buckets → next insert triggers VertexBucketCountCapReached; (d) cascade: bucket alloc_space cap → vertex cap enforced; (e) bypass mode has its own promotion path; (f) worst case 256 MiB leaf relocation cost ≈ 8.5 × 10⁸ instructions (2.1% of 40B budget); (g) typical workloads with< 100 buckets/vertex are well within cap."
    status: pending
  - id: "tree-mode-read-dispatch"
    content: "**Moved to Plan 0318 Step 5 (pending).** Implement the mode dispatch in the bucket access constructor: `out_edges_iter`, `out_edges_collect`, `visit_edges`, `prefix_scan`, and `random_ordinal_access` (production paths that today always go through `EdgeStore`). When the bucket's tree-mode bit is set, the dispatch routes to a new `tree_mode_out_edges_iter` (using `LtbRawBlockStore::for_each_chunk` for the chunk-buffer pattern). When the bit is clear, the existing slab path runs unchanged. The dispatch lives in one constructor — reviewers must not see a mode branch in rope / PMA / placement code. **Tree-mode accessors are implemented directly on `LabeledLaraGraph` (not on the `TreeCsrBucket` prototype type).**"
    status: pending
  - id: "tree-mode-edge-write-dispatch"
    content: "**Moved to Plan 0318 Step 6 (pending).** Implement `insert_edge` (and `remove_edge_at_slot`, `direct_unlink_log_*`) for the tree-mode path: when the bucket is in tree mode, the new edge is appended into the LTB blocks (single `write_payload_partial` at the tail offset if the tail block has room; otherwise mint a new tail block and append at `tail_first_slot`). Tombstone semantics stay above the LTB store layer; this method only mutates the canonical row. **Tree mode has gap-0 invariant: tail-block-room check uses `stored_slots % B` (not `alloc_gap_tail`, which is a slab-mode concept).**"
    status: pending
  - id: "deepen-flatten"
    content: "**Moved to Plan 0318 Step 7 (pending).** Implement `deepen()` and `flatten()` for the tree-mode descriptor. `deepen` runs when the derived root length would exceed `R_max = 1024`: reserve interior blocks, copy current root ids into the interior blocks, rewrite the span to the interior ids (right-spine partial allowed), and publish. `flatten` is the inverse, only ever inside compaction/maintenance. Both are level-generic (2→3 identical one level up). Fail-closed at `MAX_DEPTH = 3` per ADR 0088 §4."
    status: pending
  - id: "wasm-budget-recheck"
    content: "**Moved to Plan 0318 Step 8 (pending).** Run `cargo build --release --target wasm32-unknown-unknown --features canbench` from `crates/ic-stable-lara/`. The exported-name budget must still be under the 20K PocketIC limit. Plan 0318 adds no new canbench benches (it only changes production code paths); expected: ~16,776 chars / 3,224 chars headroom preserved."
    status: pending
  - id: "gate-2-recheck-on-virtual-memory"
    content: "**Moved to Plan 0318 Step 9 (pending).** Re-run Gate 2 canbench benches against the production `VirtualMemory<DefaultMemoryImpl>` backend: `tcsr_4096_full_scan_descending`, `tcsr_65536_full_scan_descending`, `tcsr_4096_random_ordinal_access`, `tcsr_4096_insert_grow`, etc. Record ins/edge for each row and confirm production backend is within ±20% of the `VectorMemory` test numbers from Plan 0322 (~41 ins/edge at 4K scan). If production numbers regress beyond ±20%, open a follow-up slice to investigate the regression. **Baseline numbers from Plans 0315/0316/0322 (production-equivalent VectorMemory backend)**: 41 ins/edge at 4K full scan (Plan 0316 block-batched, down from 14,540 raw-block / 52,300 scaffold); 17,066 ins/edge for insert_grow (Plan 0315 raw-block); 209K ins/call for random_ordinal_access (Plan 0322)."
    status: pending
  - id: "plan-validator"
    content: "**Moved to Plan 0318 Step 11 (pending).** Run `python3 ~/.agents/skills/plan/scripts/validate_plan.py plans/0318-tree-csr-implementation.md --phase final` and confirm structurally-valid final-phase verdict before reporting completion. Plan 0317 is documentation-only; its validator is `python3 ~/.agents/skills/plan/scripts/validate_plan.py plans/0317-adr0088-tree-csr-implementation.md --phase final` on the cap-semantics-amend body only."
    status: pending
isProject: false
---

# ADR 0088 Tree-CSR amend (cap semantics)

> **Note**: This plan is **documentation-only**. The original Plan 0317 covered the full Tree-CSR production wire-up (LtbRawBlockStore as a graph field, mode dispatch, promote, deepen/flatten, Gate 2 canbench re-run). That production wire-up was split out into [Plan 0318](./0318-tree-csr-implementation.md) for scope manageability. Plan 0317 now contains only the cap-semantics amend (the `T_PROMOTE` / `R_MAX` cap-on-`alloc_space` clarification) which is a documentation change to ADR 0088 and a prerequisite for the production wire-up. The actual `check_alloc_cap` / `check_vertex_bucket_count_cap` / `compute_bucket_allocation` helpers and their integration into the bucket access constructor live in Plan 0318 Step 3.

## Objective

Amend [ADR 0088](../design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md) §Decision to clarify that `T_PROMOTE` and `R_MAX` are caps on `alloc_space = stored_slots + alloc_gap` (NOT on `stored_slots` alone). This is a documentation-only change: no production code is modified in this plan. The amend unifies the prototype's `compute_bucket_allocation` (which was `stored_slots + alloc_gap` for slab mode) with the cap check (which was on `stored_slots` alone before the amend).

Success signal:

- **ADR 0088 §Decision** explicitly states the cap is on `alloc_space = stored_slots + alloc_gap`, not on `stored_slots` alone. The cascade from bucket cap to vertex cap of `MAX_BUCKETS_PER_VERTEX = 1024` is documented. The worst-case leaf footprint (16 vertices × 1024 buckets × 16 KiB = 256 MiB) and IC DMT cost (8.5 × 10⁸ ins = 2.1% of 40B update budget) are documented.
- **Plan 0318** (the production wire-up) uses this cap semantics in its `check_alloc_cap` / `check_vertex_bucket_count_cap` / `compute_bucket_allocation` helpers and treats the cap as authoritative. The cap enforcement in Plan 0318 Step 3 and the integration in Plan 0318 Steps 5-6 are the production-code counterpart of this amend.
- `validate_plan.py --phase final` is structurally valid for the documentation-only cap-semantics-amend body of this plan.
- `validate_plan.py --phase final` is structurally valid.

## Context

- [ADR 0088](../design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md) — Tree-CSR mode for high-degree labeled buckets with `LtbRawBlockStore`. §Decision (2026-08-30) recorded Gate 3 pass after [Plan 0316](./0316-adr0088-per-write-payload-constant-fix.md) and Gate 4 pass after [Plan 0315](./0315-adr0088-raw-block-ltb-revalidation.md). Final verdict: Gates 1 / 2 amend, Gates 3 / 4 pass. Implementation slice (Plan 0317) is unblocked from the cost side.
- [Plan 0322](./0322-adr0088-partial-read-and-chunk-iter.md) closed the partial-read side. The `LtbRawBlockStore` API surface is now production-shaped:
  - `read_payload(id, &mut [u8; 4096])` — block-aligned read
  - `read_payload_partial(id, offset, &mut [u8])` — sub-block read
  - `write_payload(id, &[u8; 4096])` — block-aligned write
  - `write_payload_partial(id, offset, src)` — sub-block write
  - `for_each_chunk<F>(&self, f)` — chunk-buffer iterator (CSR-slab leaf-chunk-buffer pattern)
  - `mint()`, `release()`, `walk_free_list()`
- The prototype `tree_csr_prototype.rs` carries the same API contract but operates on an in-memory `TreeCsrBucket<M>` value type. This slice replaces the prototype-only code path with a production path that integrates with the existing `LabeledLaraGraph` slab machinery through a single mode-dispatch constructor.
- ADR 0088 §Encapsulation: "rope/PMA/placement stay mode-blind and operate on spans; block lifecycle is owned by the LTB store; logical-position contracts stay where they are." Concretely: the mode dispatch lives at the bucket access constructor (`out_edges_iter`, `insert_edge`, etc.). Rope, PMA, and placement code see only one of the two backings; they are unaware of the mode bit.
- `LabelBucket::BYTES == 29`. ADR 0088 §1: "the descriptor stays 29 bytes" because the tree-mode flag occupies one of the existing reserved bits (`bits 60–63`) of the packed `word`. Bit 63 is the natural choice (high bit, used as a sign in many flag conventions; matches the existing `inline_property_bytes_log_state_mismatch` etc. style of bit-packing the wire layout).
- Existing ADR 0007 stable-memory-layout inventory needs a follow-up update listing `LtbRawBlockStore × 2 orientations` (forward + reverse) as new `MemoryId` slots. That update is tracked separately and not in scope here (per Plan 0317's own ADR §Decision table, the inventory update is a follow-up slice).

## Scope

In scope:

- `LabelBucket` packed word tree-mode flag bit + serialization update.
- `LabeledLaraGraph::new` / `init` constructor signature update (one extra `M: Memory` arg each).
- `BidirectionalLabeledLaraGraph::new` / `init` constructor signature update (two extra `M: Memory` args each).
- `promote_bypass_to_tree_mode(vid, label)` failure-atomic transition.
- Mode dispatch in the bucket access constructor for read paths (`out_edges_iter`, `out_edges_collect`, `visit_edges`, `prefix_scan`, `random_ordinal_access`).
- Mode dispatch for `insert_edge`, `remove_edge_at_slot`, `direct_unlink_log_*`.
- `deepen()` and `flatten()` (level-generic; fail-closed at `MAX_DEPTH = 3`).
- Test fixture helpers (`labeled_lara_memories()`, `failpoint_labeled_memories()`) updated for the new `M` arguments.
- Gate 2 canbench re-run on production `VirtualMemory` backend.
- `validate_plan.py --phase final` pass.

Out of scope:

- Demotion (tree → slab). Per [Plan 0316 §Later Slices](./0316-adr0088-per-write-payload-constant-fix.md), recorded as Plan 0318 (a benchmark-gated maintenance operation).
- `materialize_inline_property_stream` migration primitive. Recorded as Plan 0319.
- Batch admission widening to tree-mode buckets. Recorded as Plan 0320.
- Normal LARA (unlabeled) tree mode as a second instance. Recorded as Plan 0321.
- PocketIC-backed 1M-degree sweep. Recorded as a follow-up (the `tree_csr_high_degree_test.rs` 1M sweep is `#[ignore]`'d; production `VirtualMemory` does not hit the heap limit that `VectorMemory` does on host).
- ADR 0007 stable-memory-layout inventory update. Tracked separately.

## Expected Change Surface

| File pattern | Change |
|--------------|--------|
| `crates/ic-stable-lara/src/labeled/record.rs` (modified) | Add `LabelBucket::is_tree_mode` / `with_tree_mode` accessors; update packed-word encode/decode so bit 63 carries the tree-mode flag and the reserved-bits validator ignores bit 63; keep `LabelBucket::BYTES == 29`. Add unit tests for the tree-mode flag bit round-trip and reserved-bits compatibility. |
| `crates/ic-stable-lara/src/labeled/graph.rs` (modified) | Add `ltb: LtbRawBlockStore<M>` field; update `LabeledLaraGraph::new(...)` and `init(...)` signatures (one extra `M` arg each); add `promote_bypass_to_tree_mode` failure-atomic transition; add `tree_mode_out_edges_iter` and other tree-mode accessors; update mode dispatch in the existing access constructor. |
| `crates/ic-stable-lara/src/labeled/bidirectional.rs` (modified) | Add two `ltb: LtbRawBlockStore<M>` fields (forward + reverse); update bidirectional `new(...)` / `init(...)` signatures (two extra `M` args each). |
| `crates/ic-stable-lara/src/labeled.rs` (modified) | `pub(crate) mod ltb_raw_block_store` already exposed from Plan 0315; no further module gate changes here. |
| `crates/ic-stable-lara/src/test_support.rs` (modified) | `labeled_lara_memories()` and `failpoint_labeled_memories()` updated to return 16 / 16 `VectorMemory` values (was 15 / 15) to account for the extra LTB per orientation; for bidirectional graphs, 32 / 32 (was 30 / 30). |
| `crates/ic-stable-lara/src/labeled/labeled.rs` and `crates/ic-stable-lara/src/labeled/graph/init.rs` (modified) | Wiring the new `M: Memory` args into the constructors; update `classify_composite_init` partial-layout detection to count the LTB as part of the composite (asymmetric reopen rule: empty LTB reopens under `value_blobs`-style asymmetry). |
| `crates/ic-stable-lara/canbench_results.yml` (not regenerated; updated incrementally per re-run bench) | Production-`VirtualMemory` Gate 2 numbers land next to the test-`VectorMemory` numbers from Plan 0322 as "new bench" entries; --persist runs bench-by-bench, not in bulk. |
| `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` (modified) | §Decision line updated to mark Plan 0317 closed; §Status line moves from "design validated" to "implemented; pending PocketIC revalidation"; §Design Documentation Impact table updated. |
| `plans/0317-adr0088-tree-csr-implementation.md` (new) | This file (todos completed, completion criteria checked). |

## Steps

### Step 1 — `LabelBucket` tree-mode flag bit

In `crates/ic-stable-lara/src/labeled/record.rs`:

1. Identify the existing reserved-bit validator (`bucket_word_has_zero_reserved_bits`) and the range it rejects. Currently bits 60–63 are reserved and must be zero per `ReservedBitsSet` error.

2. Update the validator (and the unit tests `label_bucket_rejects_each_set_reserved_bit`) so that:
   - **Bit 63 is the tree-mode flag.** It is set on tree-mode buckets; it is zero on slab-mode buckets. Existing slab-mode buckets (which always have bit 63 = 0) reopen without change.
   - **Bits 60–62 remain reserved and must be zero** for forward compatibility.
   - `ReservedBitsSet` errors continue to reject only bits 60–62, not bit 63.

3. Add accessors:

   ```rust
   impl LabelBucket {
       /// Bit 63 of the packed word: 1 = tree mode (LTB-backed), 0 = slab mode.
       pub(crate) const TREE_MODE_BIT: u64 = 1u64 << 63;

       /// Returns `true` if this bucket is in tree mode (LTB-backed).
       #[inline]
       pub(crate) fn is_tree_mode(&self) -> bool {
           (self.word & Self::TREE_MODE_BIT) != 0
       }

       /// Returns a copy of this bucket with the tree-mode flag set or cleared.
       #[inline]
       pub(crate) fn with_tree_mode(mut self, enabled: bool) -> Self {
           if enabled {
               self.word |= Self::TREE_MODE_BIT;
           } else {
               self.word &= !Self::TREE_MODE_BIT;
           }
           self
       }
   }
   ```

4. The `try_from_parts` constructor and `try_read_from` deserializer do not need new fields (the bit is part of `word`); the existing round-trip path handles it transparently.

5. Unit tests:
   - `label_bucket_tree_mode_bit_round_trips`: encode → decode → assert `is_tree_mode() == true`.
   - `label_bucket_tree_mode_bit_cleared_default`: `LabelBucket::default()` is slab-mode (bit 63 = 0).
   - `label_bucket_rejects_reserved_bits_60_62`: encode with bit 60 set, decode, expect `ReservedBitsSet` error.
   - `label_bucket_accepts_tree_mode_bit_63`: encode with bit 63 set, decode, expect success and `is_tree_mode() == true`.

### Step 2 — `LtbRawBlockStore` as a graph field

In `crates/ic-stable-lara/src/labeled/graph.rs`:

1. Add the field:

   ```rust
   pub struct LabeledLaraGraph<E, M>
   where
       E: CsrEdge,
       M: Memory,
   {
       vertices: VertexStore<LabeledVertex, M>,
       buckets: LabelBucketStore<M>,
       edges: EdgeStore<E, M>,
       values: EdgeInlinePropertyBytesStore<M>,
       ltb: LtbRawBlockStore<M>,                       // <-- new
       default_label: BucketLabelKey,
       last_bucket_lookup: Cell<Option<BucketLookupCache>>,
       inline_property_bytes_compaction_deferred: Cell<bool>,
       bucket_lookup_cache: [Cell<Option<BucketLookupCache>>; BUCKET_LOOKUP_CACHE_ENTRIES],
       _marker: PhantomData<E>,
   }
   ```

2. Update `LabeledLaraGraph::new(...)` to accept one extra `M: Memory` argument for the LTB store. The argument goes at the end (after `value_blobs`) so the existing 15-arg signature becomes 16-arg. Same for `init(...)`.

3. Update `BidirectionalLabeledLaraGraph` analogously: two extra `M` args (one per orientation), `new` and `init` constructors both gain 2 args.

4. Update `test_support::labeled_lara_memories()` to return 16 `VectorMemory` values; update `failpoint_labeled_memories()` to return 16 `FailpointMemory` values. Update `LabeledLaraGraph::init`'s `classify_composite_init` partial-layout detection to count the LTB as part of the composite (asymmetric reopen: empty LTB reopens like `value_blobs`).

5. Update all existing call sites that construct `LabeledLaraGraph::new` / `init` (the test suite, the production graph constructor, the deferred variants) to pass an extra `VectorMemory` (test) or `VirtualMemory<DefaultMemoryImpl>` (production) for the LTB.

### Step 3 — `promote_bypass_to_tree_mode(vid, label)`

In `crates/ic-stable-lara/src/labeled/graph.rs`:

1. Add the failure-atomic transition mirroring `promote_bypass_to_bucket_mode`:

   ```rust
   /// Promote a bypass / slab bucket whose `alloc_space` has crossed
   /// `T_promote = 4096` into tree mode. `alloc_space = stored_slots + alloc_gap`,
   /// NOT `stored_slots` alone — for slab mode, the LARA rebalance may reserve
   /// gap slots up to the cap, so the trigger is on the *total* allocation.
   /// Reserve all data blocks in the LTB store, transcribe the slab prefix +
   /// unfolded log entries in logical order into blocks, write the root region,
   /// publish the descriptor with the new tree-mode flag bit set, and release
   /// the old edge span (and old inline-property span if applicable).
   pub(crate) fn promote_bypass_to_tree_mode(
       &self,
       vid: VertexId,
       label: BucketLabelKey,
   ) -> Result<(), LabeledOperationError> {
       // 1. Read current descriptor; verify preconditions
       //    (alloc_space = stored_slots + alloc_gap >= T_promote).
       // 2. Compute derived depth from stored_slots; reserve blocks (mint).
       // 3. Transcribe slab prefix + log entries into blocks (no public API
       //    call site yet; the LTB store's write_payload is used for full-block
       //    commits and write_payload_partial for the tail block).
       // 4. Set tree-mode bit on the descriptor; bump stored_slots and root_len
       //    consistently.
       // 5. Release old edge span; release old inline-property span if w > 0.
       // 6. Publish the new descriptor (single canonical write).
   }
   ```

2. The transition is reserved in a single attempt; partial states are rolled back via the existing release path of the LTB store.

### Step 3.5 — `alloc_space` cap enforcement cascade (bucket → vertex)

**Cap semantics.** The promotion caps `T_PROMOTE` (slab → tree) and `R_MAX` (tree depth limit) are enforced on `alloc_space = stored_slots + alloc_gap`, NOT on `stored_slots` alone. Two reasons:

1. **LARA rebalance reserves gap slots up to the cap.** A bucket's edge span in slab mode is `alloc_gap(stored_slots) = some weighted function of stored_slots`; the rebalance may over-allocate to amortize future inserts. If the cap is enforced on `stored_slots` alone, a bucket can be at `stored_slots = 3000` with `alloc_gap = 2000` already at the cap, but a write to slot 3001 would silently push `alloc_space` to 5001 — past `T_PROMOTE = 4096`. This violates the per-slot count invariant.
2. **Tree mode's gap-0 invariant** means `alloc_space = stored_slots` for tree buckets; the cap reduces to the simpler form, but for slab buckets the gap must be included.

The cap is a function of the bucket's mode:
- **CSR slab mode**: `alloc_space = stored_slots + alloc_gap ≤ T_PROMOTE = 4096 slots (16 KiB)`.
- **Tree mode**: `alloc_space = stored_slots ≤ R_MAX = 1024 slots (4 KiB)` (gap-0 invariant; `alloc_gap = 0`).

**Vertex cap cascade.** Per vertex, the bucket count is capped at `MAX_BUCKETS_PER_VERTEX = 1024` (the number of distinct edge-label types incident to one vertex). This is independent of the 16-bit (65536) federation-wide label id space — the 1024 limit is the per-vertex incident-edge-label-type count, not a label-id encoding range. The 16-bit label id is the wire encoding; the 1024 per-vertex cap is the access pattern.

**Why 1024 per vertex.** Industry comparison: DataStax Enterprise Graph imposes an official cap of 200 edge labels per graph; Gleaph's 1024 is 5× larger, the largest among major graph DB products. Typical graph schemas use < 100 edge labels per vertex; 1024 covers all practical workloads (social graphs, e-commerce, knowledge graphs, IoT, finance) with 10-200× schema evolution headroom.

**Worst case leaf footprint.** 16 vertices × 1024 buckets × 16 KiB = 256 MiB. The IC deterministic memory tracker (commit b722538, 2026-07-23) charges 5000 instructions per 4 KiB page read AND 5000 per page write, plus 3000 per page copy overhead. Per-slot (4-byte) relocation cost is therefore `5000 (page write) + 5000 (page read) + 3000 (copy)` ÷ `(4096 / 4)` = ~12.7 instructions per slot. Worst case 256 MiB leaf relocation = 8.5 × 10⁸ instructions = 2.1% of the 40 B update budget — well within safety.

**Implementation.** In `crates/ic-stable-lara/src/labeled/graph.rs`:

```rust
/// Cap on per-vertex bucket count (distinct incident edge-label types).
pub(crate) const MAX_BUCKETS_PER_VERTEX: u32 = 1024;

/// Slab mode cap on `alloc_space = stored_slots + alloc_gap`.
pub(crate) const T_PROMOTE: u32 = 4096;

/// Tree mode cap on `alloc_space = stored_slots` (gap-0 invariant).
pub(crate) const R_MAX: u32 = 1024;

/// Returns the bucket's effective allocation in slots (includes gap for slab
/// mode, equals `stored_slots` for tree mode).
#[inline]
pub(crate) fn compute_bucket_allocation(bucket: &LabelBucket) -> u32 {
    if bucket.is_tree_mode() {
        bucket.stored_slots
    } else {
        bucket.stored_slots.saturating_add(alloc_gap(bucket.stored_slots))
    }
}

/// Cap for the given bucket mode.
#[inline]
pub(crate) fn cap_for_mode(bucket: &LabelBucket) -> u32 {
    if bucket.is_tree_mode() { R_MAX } else { T_PROMOTE }
}

/// Returns `Err(AllocSpaceCapReached)` if `increment` would push the bucket's
/// `alloc_space` past the mode cap. Enforced at every insert path.
pub(crate) fn check_alloc_cap(
    bucket: &LabelBucket,
    increment: u32,
) -> Result<(), LabeledOperationError> {
    let current = compute_bucket_allocation(bucket);
    let cap = cap_for_mode(bucket);
    if current.saturating_add(increment) > cap {
        return Err(LabeledOperationError::AllocSpaceCapReached {
            current_alloc_space: current,
            cap,
            mode: bucket.mode(),
        });
    }
    Ok(())
}

/// Returns `Err(VertexBucketCountCapReached)` if the vertex already holds
/// `MAX_BUCKETS_PER_VERTEX` buckets. Enforced at the bucket-mint path.
pub(crate) fn check_vertex_bucket_count_cap(
    current_count: u32,
) -> Result<(), LabeledOperationError> {
    if current_count >= MAX_BUCKETS_PER_VERTEX {
        return Err(LabeledOperationError::VertexBucketCountCapReached {
            current_count,
            cap: MAX_BUCKETS_PER_VERTEX,
        });
    }
    Ok(())
}
```

**Error variants to add to `LabeledOperationError`:**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabeledOperationError {
    // ... existing variants ...
    /// Bucket's `alloc_space` (stored_slots + alloc_gap) reached the mode cap.
    /// Caller must trigger `promote_bypass_to_tree_mode` (slab mode) or
    /// `deepen` (tree mode) before retrying.
    AllocSpaceCapReached {
        current_alloc_space: u32,
        cap: u32,
        mode: BucketMode,
    },
    /// Vertex already holds MAX_BUCKETS_PER_VERTEX distinct edge-label-type
    /// buckets. Caller must re-classify this vertex (split, federate, or fail).
    VertexBucketCountCapReached {
        current_count: u32,
        cap: u32,
    },
}
```

**Audit trail.** Walk every path that mutates `stored_slots` (insert_edge, remove_edge_at_slot, direct_unlink_log_*, promote_bypass_to_bucket_mode, promote_bypass_to_tree_mode, deepen, flatten, LARA rebalance, leaf-pin relocation). Each must call `check_alloc_cap` before committing the canonical write. Bypass mode has its own promotion path (`promote_bypass_to_bucket_mode`); the cascade does not need to enforce the alloc-space cap on the bypass row itself, only on the post-promotion target (slab or tree).

**Unit tests for Step 3.5:**

| Test | Scenario |
|------|----------|
| `alloc_cap_triggers_promote_at_threshold` | CSR slab bucket at `alloc_space = T_PROMOTE - 1` → next insert triggers `promote_bypass_to_tree_mode`. |
| `alloc_cap_triggers_deepen_at_threshold` | Tree mode bucket at `alloc_space = R_MAX - 1` → next insert triggers `deepen` (or `AllocSpaceCapReached` if `MAX_DEPTH` already reached). |
| `vertex_bucket_count_cap_at_max` | Vertex with `MAX_BUCKETS_PER_VERTEX` buckets → next insert returns `VertexBucketCountCapReached`. |
| `alloc_cap_cascade_bucket_then_vertex` | Bucket `alloc_space` cap fires before vertex bucket-count cap; the cascade order is enforced (bucket cap → vertex cap → return). |
| `bypass_mode_promotion_path_independent` | Bypass mode does NOT enforce `alloc_space` cap on the bypass row itself; promotion to slab or tree mode is the bypass-mode-specific path. |
| `worst_case_256_mib_leaf_relocation_cost` | 256 MiB leaf relocation = 8.5 × 10⁸ instructions; assert this is ≤ 2.5% of 40 B update budget (with 0.4 pp safety margin over the 2.1% estimate). |
| `typical_workload_under_caps` | Workloads with < 100 buckets per vertex are well under `MAX_BUCKETS_PER_VERTEX`; assert typical schemas do not approach the cap. |

### Step 4 — Tree-mode read dispatch

In `crates/ic-stable-lara/src/labeled/graph.rs`, update the existing bucket access constructor to dispatch on the tree-mode bit:

```rust
pub fn out_edges_iter(
    &self,
    vid: VertexId,
    label: BucketLabelKey,
    order: OutEdgeOrder,
) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
    let bucket = self.bucket_for_label(vid, label)?;
    if bucket.is_tree_mode() {
        Ok(LabeledOutEdgesIter::TreeMode(self.tree_mode_out_edges_iter(vid, label, order)?))
    } else {
        Ok(LabeledOutEdgesIter::Slab(self.slab_out_edges_iter(vid, label, order)?))
    }
}
```

`LabeledOutEdgesIter` becomes an enum with `Slab` and `TreeMode` variants. Each variant yields `(slot, target)` pairs. The `TreeMode` variant holds an iterator built on top of `LtbRawBlockStore::for_each_chunk` and decodes 4-byte rows lazily from the yielded `&[u8]` slices.

Other accessors follow the same pattern: `out_edges_collect`, `visit_edges`, `prefix_scan_descending`, `random_ordinal_access`. Each checks `is_tree_mode()` and routes to the LTB-backed path or the slab path. The dispatch lives in **one constructor per accessor** (no branches in the slab or tree-mode internals).

### Step 5 — Tree-mode edge write dispatch

In `crates/ic-stable-lara/src/labeled/graph.rs`, update `insert_edge`:

- If the bucket is in slab mode and the insertion would push `alloc_space = stored_slots + alloc_gap` past `T_promote = 4096`, the dispatch triggers `promote_bypass_to_tree_mode` first, then completes the insertion in tree mode. (See §Step 3.5 for the cap enforcement cascade.)
- If the bucket is in tree mode, the new edge is appended into the LTB blocks: if the tail block has room (`stored_slots - tail_first_slot < B = 1024`), a single `write_payload_partial` writes the 4-byte target at `tail_first_slot_offset + (stored_slots - tail_first_slot) * 4`. Otherwise, mint a new tail block (`mint()`), populate it via the same path, append the root array, bump `stored_slots`.
- If the bucket is in bypass mode, existing behavior: append into the bypass row, then trigger promotion if `stored_slots` crosses the threshold.

`remove_edge_at_slot`, `direct_unlink_log_*` similarly dispatch on the tree-mode bit and use `read_payload_partial` / `write_payload_partial` for tombstone rewriting.

### Step 6 — `deepen()` and `flatten()`

In `crates/ic-stable-lara/src/labeled/graph.rs`:

```rust
/// Deepen the tree-mode root array when the derived root length would exceed
/// `R_max = 1024`. Reserve interior blocks (`kind = *Interior`), copy current
/// root ids into them, rewrite the span to the interior ids (right-spine
/// partial allowed), publish. Level-generic (2→3 identical one level up).
/// Fails closed at `MAX_DEPTH = 3`.
pub(crate) fn deepen(&self, vid: VertexId, label: BucketLabelKey) -> Result<(), ...> { ... }

/// Flatten is the inverse of deepen, only ever invoked inside compaction /
/// maintenance. Interior blocks are released after publish (commit-order
/// invariant).
pub(crate) fn flatten(&self, vid: VertexId, label: BucketLabelKey) -> Result<(), ...> { ... }
```

Both follow the reserve/commit/publish split used by `promote_bypass_to_tree_mode`. Fail-closed: if `derive_depth(stored_slots) > MAX_DEPTH`, the method returns an error before any canonical write.

### Step 7 — Wasm budget recheck

```sh
cd crates/ic-stable-lara
CARGO_TARGET_DIR=../../target/canbench \
  cargo build --release --target wasm32-unknown-unknown --features canbench
```

The build must succeed without `sum-of-exported-name-lengths exceeds 20000`. Plan 0317 adds no new canbench benches (production-code-only changes), so the budget stays at the Plan 0314/0315/0316/0322 baseline (~16,776 chars / 3,224 chars headroom).

### Step 8 — Gate 2 re-run on production `VirtualMemory`

```sh
cd crates/ic-stable-lara
canbench tcsr_4096_full_scan_descending
canbench tcsr_65536_full_scan_descending
canbench tcsr_4096_random_ordinal_access
canbench tcsr_4096_insert_grow
```

For each bench, record the measured ins/edge and compare to the Plan 0322 `VectorMemory` baseline (~41 ins/edge at 4K full scan). If production numbers regress beyond ±20%, open a follow-up slice to investigate the regression; otherwise record pass.

### Step 9 — ADR 0088 update

In `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md`:

- §Status: "accepted with amendments (Gate 3 / 4 pass; design validated)" → "accepted with amendments (Plan 0317 implemented; Gate 3 / 4 pass; production wiring pending PocketIC 1M revalidation)".
- §Decision (Final verdict): append a paragraph noting Plan 0317 closed, with file paths.
- §Design Documentation Impact: mark the `crates/ic-stable-lara/README.md` "Tree mode summary" row as `completed` (was `on implementation`).
- Last revised: `2026-08-30` (no change; this slice did not flip any verdict).

### Step 10 — Plan validator closure

```sh
python3 ~/.agents/skills/plan/scripts/validate_plan.py \
  plans/0317-adr0088-tree-csr-implementation.md --phase final
```

Must report `Plan is structurally valid for phase=final`.

## Validation

Compile-only:

```sh
cargo check -p ic-stable-lara --tests --features canbench
cargo fmt --all -- --check
cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings
```

Test (with the `tree_csr_high_degree_test` `#[ignore]`d as in Plan 0315/0316):

```sh
cargo test -p ic-stable-lara --lib --features canbench -- --skip tree_csr_high_degree
```

Production wasm build:

```sh
cd crates/ic-stable-lara
CARGO_TARGET_DIR=../../target/canbench \
  cargo build --release --target wasm32-unknown-unknown --features canbench
```

Gate 2 benches on production `VirtualMemory`:

```sh
cd crates/ic-stable-lara
canbench tcsr_4096_full_scan_descending
canbench tcsr_65536_full_scan_descending
canbench tcsr_4096_random_ordinal_access
canbench tcsr_4096_insert_grow
```

Plan validator:

```sh
python3 ~/.agents/skills/plan/scripts/validate_plan.py \
  plans/0317-adr0088-tree-csr-implementation.md --phase final
```

## Completion Criteria

- [ ] `LabelBucket` packed word carries the tree-mode flag bit (bit 63); `LabelBucket::BYTES == 29` preserved; reserved-bits validator ignores bit 63; 4 unit tests added (round-trip, default-cleared, 60–62 reserved reject, 63 tree-mode accept).
- [ ] `LabeledLaraGraph<E, M>` has `ltb: LtbRawBlockStore<M>` field; `new(...)` and `init(...)` accept one extra `M: Memory`; `BidirectionalLabeledLaraGraph` accepts two extra `M` args. Test fixture helpers updated.
- [ ] `promote_bypass_to_tree_mode(vid, label)` is implemented with reserve/commit/publish atomicity; at least 2 unit tests (slab prefix transcription, log-entry transcription).
- [ ] `alloc_space = stored_slots + alloc_gap` cap cascade implemented: `check_alloc_cap` and `check_vertex_bucket_count_cap` enforce the cap at every insert path; 7 unit tests cover the cascade (slab promote trigger, tree deepen trigger, vertex bucket cap, cascade order, bypass-mode independence, 256 MiB worst-case cost bound, typical-workload headroom).
- [ ] Mode dispatch in bucket access constructor: `out_edges_iter`, `out_edges_collect`, `visit_edges`, `prefix_scan_descending`, `random_ordinal_access` route to slab or tree mode based on `is_tree_mode()`; single dispatch point per accessor, no branches in slab or tree internals.
- [ ] `insert_edge` / `remove_edge_at_slot` dispatch on tree-mode; tree-mode path uses `write_payload_partial` for tail-block append and `read_payload_partial` / `write_payload_partial` for tombstone rewriting.
- [ ] `deepen()` and `flatten()` work level-generically; fail-closed at `MAX_DEPTH = 3` (typed error before canonical write).
- [ ] `cargo build --release --target wasm32-unknown-unknown --features canbench` succeeds; exported-name total under the 20K PocketIC limit (~16,776 chars expected).
- [ ] Gate 2 canbench benches re-run on production `VirtualMemory`; numbers within ±20% of Plan 0322 `VectorMemory` baseline.
- [ ] ADR 0088 §Status and §Decision updated; §Design Documentation Impact table's "Tree mode summary" row marked `completed`.
- [ ] `validate_plan.py --phase final` is structurally valid.

## Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| `LabelBucket::BYTES == 29` is broken by the tree-mode flag | Wire format change; pre-production fresh-state policy requires no conversion but the wire bytes change | Bit 63 is the high bit of an existing 64-bit packed `word`; the field is 29 bytes (8 word + 4 + 4 + 4 + 4 + 1 + 1 + 1 + 4 = 29). Adding bit 63 to the `word` byte 0 (high bit) preserves the 8-byte prefix; the wire format gains a single new flag without lengthening. Existing slab-mode buckets all have bit 63 = 0, so reopen is forward-compatible. |
| Production `VirtualMemory::write` constant cost differs from `VectorMemory::write` by > ±20% | Gate 2 numbers regress | The Plan 0317 Step 8 measurement is the regression check. If it fails, open a follow-up slice to investigate the syscall cost difference. |
| Constructor signature change breaks all call sites | Compile error everywhere | Update all test fixtures (`labeled_lara_memories()`, `failpoint_labeled_memories()`) and production constructors in the same step; clippy --all-targets -- -D warnings is the final gate. |
| `tree_csr_high_degree_test::high_degree_*` 1M sweep hits production `Memory::grow` limits | Test failure on `VirtualMemory` | Production stable memory has different limits than host `VectorMemory`'s process heap. The 1M sweep is `#[ignore]`'d; the 4K / 65K coverage is in the canbench surface. PocketIC 1M is deferred to a follow-up slice. |
| Cap enforced on `stored_slots` alone instead of `alloc_space = stored_slots + alloc_gap` | LARA rebalance can over-allocate past `T_PROMOTE`; invariant violation | Step 3.5 enforces the cap on `alloc_space`, not `stored_slots`. `compute_bucket_allocation(bucket)` and `check_alloc_cap(bucket, increment)` are the canonical accessors. Audit all insert paths and LARA rebalance to use these accessors. |

## Later Slices

- Plan 0318 — Demotion (tree → slab). Per Plan 0316 §Later Slices. The reverse transition that runs when `alloc_space = stored_slots + alloc_gap` (or just `stored_slots` for tree mode) shrinks below `T_promote / 2` (or another threshold). For tree mode, the demotion threshold is a separate hysteresis value; the tree-mode `alloc_gap` is 0 so the demotion triggers purely on `stored_slots` crossings.
- Plan 0319 — `materialize_inline_property_stream` migration primitive. Recorded as planned in ADR 0088 §5.
- Plan 0320 — Batch admission widening to tree-mode buckets. Per ADR 0088 §7 (batch scalar path is currently `Unsupported` for tree-mode).
- Plan 0321 — Normal LARA (unlabeled) tree mode as a second instance.
- Plan 0323 — PocketIC-backed 1M-degree sweep (Gate 1's deferred row).

## Notes (out of scope, recorded for context)

- ADR 0007 stable-memory-layout inventory update listing `LtbRawBlockStore × 2 orientations` as new `MemoryId` slots is **not in scope** here. It is a separate documentation update that goes alongside the production wire-up; tracked separately so that this slice does not mix production-code changes with doc-only changes.
- The `LtbRawBlockStore` is currently `pub(crate)` within `crates/ic-stable-lara`. No public API change. The crate boundary continues to own the LTB lifecycle (per ADR 0088 §Encapsulation).
- The 2 new `M: Memory` arguments for `LabeledLaraGraph` (one per orientation, two for bidirectional) bring the constructor signature to 16 / 32 args. This is at the upper end of ergonomic but is consistent with the existing 15 / 30 pattern — the plan does not introduce a new ergonomic pattern (no builder, no `init_from_parts`). If a builder is desired, it is a separate refactor slice.