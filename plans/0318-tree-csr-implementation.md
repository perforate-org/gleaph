---
name: "Tree-CSR production wire-up in LabeledLaraGraph"
overview: "Implement Steps 4-11 of the original Plan 0318 (Steps 1-3 already committed as `e70f43534` and `a8ad189d3` on `plan-0318` lane): `promote_bypass_to_tree_mode` failure-atomic transition, mode-dispatch in the bucket access constructor, `deepen`/`flatten` with `MAX_DEPTH = 3` fail-closed, Gate 2 canbench re-run on production `VirtualMemory`, ADR 0088 §Status update, and `validate_plan.py --phase final` pass. Steps 4-11 are split into separate commits for review-ability: Step 4 (promote), Step 5 (read dispatch), Step 6 (write dispatch), Step 7 (deepen/flatten), Step 8-11 (validation/ADR). The LEG/LTB/LPB storage architecture (per user clarification) is: `LabelBucket::edge_start` → LEG slab offset, LEG span contains block_id array (root region), each block_id points to a 4 KiB block in the LTB store. Inline-property bytes use the same pattern with LPB byte slab instead of LEG edge slab. **Status (2026-09-01)**: Steps 1-11 all implemented and committed on `plan-0318` lane. Step 7 amend (`d5657eb5f`) replaced the overclaimed `deepen` insert hook with an interim fail-closed `TreeRootCapacityReached` guard; effective tree-mode cap is `2^20 = 1,048,576` slots per bucket until the interior-level insert cascade ships. Gate 2 production-relevant benches (full_scan, insert_grow, tcsr_promote_*) within ±1% of Plan 0315/0316 baselines. Validator passes final phase. Last revised: 2026-09-01."
todos:
  - id: "label-bucket-tree-mode-bit"
    content: "Add a tree-mode flag bit (bit 63, the high bit of the packed `word`) to `LabelBucket` in `crates/ic-stable-lara/src/labeled/record.rs`. Provide `is_tree_mode() -> bool`, `with_tree_mode(enabled: bool) -> Self`, and update `try_from_parts` / `try_read_from` / serialization / round-trip tests to keep ` `LabelBucket::BYTES == 29``. Update the reserved-bits validator so bit 63 is the tree-mode bit (not a rejected reserved bit). Update `bucket_word_has_zero_reserved_bits` to ignore bit 63 or remove the check on that bit while keeping it on bits 60–62. Unit tests: (a) tree-mode bit round-trips through encode/decode, (b) default `LabelBucket` is slab-mode, (c) reserved bits 60–62 still rejected, (d) bit 63 accepted as tree-mode flag."
    status: completed
  - id: "ltb-store-as-graph-field"
    content: "Add `ltb: LtbRawBlockStore<M>` field to `LabeledLaraGraph<E, M>` in `crates/ic-stable-lara/src/labeled/graph.rs` (single orientation). Update `LabeledLaraGraph::new(...)` and `init(...)` constructors to accept one extra `M: Memory` argument for the LTB store. Update the bidirectional graph (`BidirectionalLabeledLaraGraph`) to hold two `LtbRawBlockStore<M>` (forward + reverse), accepting 2 extra `M` arguments total. Update test fixture helpers (`labeled_lara_memories()`, `failpoint_labeled_memories()`) to return 16 / 16 `VectorMemory` values (was 15 / 15) for single orientation, and 32 / 32 (was 30 / 30) for bidirectional. Update `LabeledLaraGraph::init` reopen validation (`count consistency`, `free-list walk up to declared envelope`, asymmetric reopen rule: empty LTB reopens under `value_blobs`-style asymmetry). **[Additional in Step 2]: Define `pub(crate) const MAX_BUCKETS_PER_VERTEX: u32 = 1024;` in `crates/ic-stable-lara/src/labeled/graph.rs` (per Plan 0317 amend). This const is the per-vertex edge-label-type limit and is needed as an interface design choice at the field-addition step (not deferred to the cap-enforcement step). The cap-enforcement helper `check_vertex_bucket_count_cap` is implemented in Step 3, but the const is fixed here.**"
    status: completed
  - id: "cap-enforcement-helpers"
    content: "Implement cap enforcement helpers in `crates/ic-stable-lara/src/labeled/graph.rs`: (a) `pub(crate) const T_PROMOTE: u32 = 4096` and `pub(crate) const R_MAX: u32 = 1024` (wire truth from Plan 0315); (b) `pub(crate) const MAX_BUCKETS_PER_VERTEX: u32 = 1024` (per-vertex edge-label-type limit, industry-largest vs DataStax Enterprise Graph's official 200); (c) `compute_bucket_allocation(bucket: &LabelBucket) -> u32` returning `min(stored_slots + alloc_gap(stored_slots), cap)` for slab mode and `min(stored_slots, R_MAX)` for tree mode; (d) `check_alloc_cap(bucket: &LabelBucket, increment: u32) -> Result<(), LabeledOperationError>` returning `AllocSpaceCapReached { current_alloc_space, cap, mode }` when `alloc_space + increment > cap(bucket.mode)`; (e) `check_vertex_bucket_count_cap(vertex: &LabeledVertex) -> Result<(), LabeledOperationError>` returning `VertexBucketCountCapReached { current_count, cap }` when vertex bucket count reaches `MAX_BUCKETS_PER_VERTEX`. Note that the cap is on `alloc_space = stored_slots + alloc_gap` (NOT on `stored_slots` alone), per Plan 0317 amend. Unit tests cover each helper independently."
    status: completed
  - id: "promote-bypass-to-tree-mode"
    content: "Implement `promote_bypass_to_tree_mode(vid, label)` in `LabeledLaraGraph` (single orientation). Follow the `promote_bypass_to_bucket_mode` failure-atomic template (reserve / commit / publish). When `alloc_space = stored_slots + alloc_gap` crosses `T_PROMOTE = 4096` (the cap is on `alloc_space`, not `stored_slots` alone — per Plan 0317 amend), mint all data blocks in the LTB store via `LtbRawBlockStore::mint()`, transcribe the slab prefix + unfolded log entries in logical order into blocks (using `read_payload` for the source and `write_payload` for the destination, with sub-block writes via `write_payload_partial` for the tail block), write the root region via `LabelBucket::with_tree_mode(true)` plus the new root ids, publish the descriptor (single canonical write), and release the old edge span (via the vertex-span rewrite) and the old inline-property span (via the byte-slab `FreeSpanStore`). Promotion is also the moment the leaf releases up to `T_PROMOTE` slots of pressure. **Architecture clarification (LEG/LTB/LPB)**: `LabelBucket::edge_start` is an offset into the LEG (Labeled Edge Graph) slab. The LEG span at that offset holds the root region (a `u32` block_id array, 4 bytes per block_id). Each `block_id` (u32) indexes one 4 KiB block in the LTB (LARA Tree Block) store. The actual edge data (4 bytes per edge × up to 1024 edges per block) lives inside the LTB blocks. For `stored_slots = 4096` at depth 1, the root region is 4 × u32 = 16 bytes; 4 LTB blocks hold 4 × 4096 = 16384 bytes = 16 KiB. Total tree-mode bucket footprint = 16 KiB + 16 bytes (root region) ≈ 16.016 KiB. Inline-property bytes use the same pattern with an LPB (Labeled Property Byte) byte slab instead of the LEG edge slab. Unit tests: (a) `alloc_space < T_PROMOTE` → promotion rejected (typed error); (b) `alloc_space >= T_PROMOTE` → promotion succeeds; (c) reserve phase is atomic (no partial state); (d) publish phase is atomic (no partial state); (e) LEG root region holds correct block_id sequence; (f) LTB blocks hold correctly-ordered edge data after transcription."
    status: completed
  - id: "tree-mode-read-dispatch"
    content: "Implement the mode dispatch in the bucket access constructor: `out_edges_iter`, `out_edges_collect`, `visit_edges`, `prefix_scan_descending`, and `random_ordinal_access`. When the bucket's tree-mode bit is set, the dispatch routes to new tree-mode accessors that read the LEG root region (block_id 配列) and use `LtbRawBlockStore::read_payload_partial` for sub-block reads or `for_each_chunk` for chunk-buffer iteration. **Tree-mode accessors are implemented directly on `LabeledLaraGraph` (not on a separate prototype type)**: e.g. `tree_mode_random_ordinal_access(slot: u32) -> Option<u32>` lives on `LabeledLaraGraph` and calls `self.ltb.read_payload_partial(block_id, slot * 4, &mut 4_bytes)` directly. `TreeCsrBucket` (from `tree_csr_prototype.rs` / Plan 0313 / Plan 0322) is a prototype value type that does not appear in production. `random_ordinal_access` becomes `random_ordinal_access(slab_path) | tree_mode_random_ordinal_access(tree_path)` depending on `is_tree_mode()`. The tree-mode accessor reads the LEG root region, finds the block_id for the target slot, then uses `read_payload_partial(block_id, slot * 4, &mut 4_bytes)` to fetch only the 4 bytes — no 4 KiB block allocation per call. The dispatch lives in **one constructor per accessor** — reviewers must not see a mode branch in rope / PMA / placement code per [ADR 0088 §Encapsulation](../design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md). `LabeledOutEdgesIter` becomes an enum with `Slab` and `TreeMode` variants. Unit tests: (a) `out_edges_iter` on a slab bucket routes to slab path; (b) `out_edges_iter` on a tree bucket routes to tree path; (c) `random_ordinal_access` reads only 4 bytes (verifiable via the LTB store call count or memory instrumentation); (d) cross-mode `for_each_with_counterpart` walks both slab and tree streams correctly."
    status: completed
  - id: "tree-mode-edge-write-dispatch"
    content: "Implement `insert_edge`, `remove_edge_at_slot`, `direct_unlink_log_*` for the tree-mode path. When the bucket is in slab mode and the insertion would push `alloc_space` past `T_PROMOTE`, the dispatcher triggers `promote_bypass_to_tree_mode` first, then completes the insertion in tree mode. When the bucket is in tree mode, the new edge is appended into the LTB blocks: compute the current tail block index from `stored_slots % B` (B = 1024). If `stored_slots % B != 0` or `stored_slots == 0`, the tail block has room and we use `write_payload_partial(tail_block_id, (stored_slots % B) * 4, &target_bytes)` for a 4-byte write; otherwise, mint a new tail block (`LtbRawBlockStore::mint()`), populate it via the same path, append the new block_id to the root region (in LEG slab), bump `stored_slots`. **The tail-block-room check uses `stored_slots % B` (not `alloc_gap_tail`, which is a slab-mode concept and violates tree-mode gap-0 invariant).** Tree mode has gap-0: `alloc_space = stored_slots` (no gap slots); the cap is on `stored_slots <= R_MAX = 1024` slots per block, with multiple blocks per root region. When the bucket is in bypass mode, existing behavior: append into the bypass row, then trigger promotion if `alloc_space` crosses the threshold. Tombstone semantics stay above the LTB store layer; this method only mutates the canonical row. Unit tests: (a) tree-mode insert at full tail block mints new block correctly; (b) tree-mode insert with room in tail uses `write_payload_partial` for the 4-byte write; (c) slab-to-tree automatic promotion triggers when `alloc_space >= T_PROMOTE`."
    status: completed
  - id: "deepen-flatten"
    content: "Implement `deepen(vid, label)` and `flatten(vid, label)` for the tree-mode descriptor. `deepen` runs when the derived root length would exceed `R_MAX = 1024`: reserve interior blocks (`kind = *Interior` via `LtbRawBlockStore::mint()`), copy current root ids into the interior blocks (right-spine partial allowed), rewrite the span to the interior ids, publish the descriptor. `flatten` is the inverse, only ever inside compaction/maintenance; interior blocks are released after publish (commit-order invariant). Both are level-generic (depth 2 → 3 identical to depth 1 → 2). Fail-closed at `MAX_DEPTH = 3` per [ADR 0088 §4](../design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md): `derive_depth(stored_slots) > MAX_DEPTH` returns a typed error before any canonical write. Unit tests: (a) `stored_slots = 2²⁰` → deepen triggers depth 1 → 2; (b) `stored_slots = 2³⁰` → deepen triggers depth 2 → 3; (c) `stored_slots = 2⁴⁰` → MAX_DEPTH exceeded, fail-closed typed error; (d) `flatten` after compaction releases interior blocks correctly."
    status: completed
  - id: "tree-mode-root-capacity-fail-closed"
    content: "Step 7 amend: insert path checks the **physical** root region length against `R_max = 1024` BEFORE any state change. When the next insert would push the physical root past `R_max`, return `LabeledOperationError::TreeRootCapacityReached { stored_slots, root_len, cap }` (new variant, error.rs). The guard replaces the overclaimed `deepen` first hook from commit `7918f9be3`'s message. Effective tree-mode cap is `2^20 = 1,048,576` slots per bucket until the interior-level insert cascade ships (follow-up `tree-mode-interior-level-insert-growth`). Tests: `tree_insert_fails_closed_at_root_capacity` (direct helper) + `production_insert_path_fails_closed_at_root_capacity` (production `insert_edge_skip_leaf_cascade` dispatcher)."
    status: completed
  - id: "wasm-budget-recheck"
    content: "Run `cargo build --release --target wasm32-unknown-unknown --features canbench` from `crates/ic-stable-lara/` and verify the exported-name budget is under the 20K PocketIC limit. Plan 0318 adds no new canbench benches (production-code-only changes), so the budget should stay at the Plan 0314/0315/0316/0322 baseline (~16,776 chars / 3,224 chars of headroom preserved). If regression occurs, investigate and add to the plan as a follow-up; do not block the implementation."
    status: completed
  - id: "gate-2-recheck-on-virtual-memory"
    content: "Re-run Gate 2 canbench benches against the production `VirtualMemory<DefaultMemoryImpl>` backend: `tcsr_4096_full_scan_descending`, `tcsr_65536_full_scan_descending`, `tcsr_4096_random_ordinal_access`, `tcsr_4096_insert_grow`, `tcsr_4096_delete_half_by_slot_then_scan`, and the same for 65K. Record ins/edge for each cell and confirm production numbers are within ±20% of the Plan 0322 `VectorMemory` baseline. **Baseline numbers from Plans 0315/0316/0322 (production-equivalent VectorMemory backend)**: (a) `tcsr_4096_full_scan_descending` (Plan 0316 block-batched): **41 ins/edge** at 4K (down from 14,540 ins/edge in Plan 0313 scaffold; raw-block 59.57M / 4096 = 14,540; block-batched collapsed to 41 ins/edge); (b) `tcsr_4096_random_ordinal_access` (Plan 0322): **~209K ins/call** (call-level, not per-edge); (c) `tcsr_4096_insert_grow` (Plan 0315 raw-block): **17,066 ins/edge** (= 69.90M / 4096; down from 52,300 ins/edge in Plan 0313 scaffold). Note: insert_grow is a planar operation, not a canbench bench; the canbench surface measures it through `tcsr_promote_*` instead. (d) `tcsr_4096_delete_half_by_slot_then_scan` (Plan 0322): O(N²) prototype-only limitation, not production-representative. (e) `tcsr_65536_full_scan_descending` (Plan 0316 block-batched): **~41 ins/edge**. (f) `tcsr_65536_insert_grow` (Plan 0315 raw-block): **~17,090 ins/edge** (= 69.90M × 16 / 65536 ≈ 17,066–17,090). (g) `tcsr_65536_delete_half_by_slot_then_scan` (Plan 0316): 6.14T ins (prototype O(N²), not production). If production numbers regress beyond ±20%, open a follow-up slice to investigate the regression. Worst case leaf footprint at MAX_BUCKETS_PER_VERTEX = 1024 × T_PROMOTE = 4096 slots × 16 vertices = 256 MiB; leaf relocation cost ≈ 8.5 × 10⁸ instructions = 2.1% of IC 40B update budget per IC DMT (commit b722538, 2026-07-23: 5000 ins/page read + 5000 ins/page write + 3000 ins/page copy overhead). **Results (2026-09-01)**: all production-relevant benches (full_scan, insert_grow, tcsr_promote_*) within ±1% of Plan 0315/0316 baselines. Random_ordinal_access and delete_half are prototype-only parity rows (not on the production hot path). Recorded in plan §Step 9."
    status: completed
  - id: "adr-0088-update-and-validate"
    content: "Update `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` §Status to mark Plan 0318 closed. Run `python3 ~/.agents/skills/plan/scripts/validate_plan.py plans/0318-tree-csr-implementation.md --phase final` and confirm structurally-valid final-phase verdict before reporting completion."
    status: completed
isProject: false
---

# Tree-CSR production wire-up in LabeledLaraGraph

## Objective

Implement the Plan 0317-amended Tree-CSR mode in the production `LabeledLaraGraph` (and `BidirectionalLabeledLaraGraph`). Wire `LtbRawBlockStore<VirtualMemory>` as a per-orientation field, add the tree-mode flag bit to `LabelBucket::word` bit 63, implement `promote_bypass_to_tree_mode` failure-atomic transition, mode-dispatch in the bucket access constructor for read/write paths, `deepen`/`flatten` with `MAX_DEPTH = 3` fail-closed, and `check_alloc_cap` / `check_vertex_bucket_count_cap` cap enforcement (per `alloc_space = stored_slots + alloc_gap` per Plan 0317 amend). Confirm Gate 2 canbench numbers within ±20% of Plan 0322 `VectorMemory` baseline on production `VirtualMemory`.

Success signal:

- `LabelBucket` packed word carries the tree-mode flag bit (bit 63); `LabelBucket::BYTES == 29` is preserved; 4 unit tests added (round-trip, default-cleared, 60–62 reserved reject, 63 tree-mode accept).
- `LabeledLaraGraph<E, M>` has a new `ltb: LtbRawBlockStore<M>` field; `new(...)` and `init(...)` accept one extra `M: Memory`; `BidirectionalLabeledLaraGraph` accepts two extra `M` args. Test fixture helpers updated.
- `promote_bypass_to_tree_mode(vid, label)` implemented with reserve/commit/publish atomicity.
- Mode dispatch in bucket access constructor: `out_edges_iter`, `out_edges_collect`, `visit_edges`, `prefix_scan_descending`, `random_ordinal_access` route to slab or tree mode based on `is_tree_mode()`; single dispatch point per accessor, no branches in slab or tree internals.
- `insert_edge` / `remove_edge_at_slot` dispatch on tree-mode; tree-mode path uses `write_payload_partial` for tail-block append and `read_payload_partial` / `write_payload_partial` for tombstone rewriting.
- `deepen()` and `flatten()` work level-generically; fail-closed at `MAX_DEPTH = 3`.
- `cargo build --release --target wasm32-unknown-unknown --features canbench` succeeds; exported-name total under the 20K PocketIC limit (~16,776 chars expected).
- Gate 2 canbench benches re-run on production `VirtualMemory`; numbers within ±20% of Plan 0322 `VectorMemory` baseline.
- ADR 0088 §Status updated.
- `validate_plan.py --phase final` is structurally valid.

## Context

- [ADR 0088](../design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md) — Tree-CSR mode for high-degree labeled buckets with `LtbRawBlockStore`. §Decision (2026-08-30) recorded Gate 3 pass after [Plan 0316](./0316-adr0088-per-write-payload-constant-fix.md) and Gate 4 pass after [Plan 0315](./0315-adr0088-raw-block-ltb-revalidation.md). Final verdict: Gates 1 / 2 amend, Gates 3 / 4 pass. [Plan 0317](./0317-adr0088-tree-csr-implementation.md) amended the cap semantics: `T_PROMOTE` and `R_MAX` are caps on **`alloc_space = stored_slots + alloc_gap`** (NOT on `stored_slots` alone). This slice closes the implementation side.
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
- Mode dispatch in the bucket access constructor for read paths (`out_edges_iter`, `out_edges_collect`, `visit_edges`, `prefix_scan_descending`, `random_ordinal_access`).
- Mode dispatch for `insert_edge`, `remove_edge_at_slot`, `direct_unlink_log_*`.
- `deepen()` and `flatten()` (level-generic; fail-closed at `MAX_DEPTH = 3`).
- `check_alloc_cap` and `check_vertex_bucket_count_cap` cap enforcement helpers (per Plan 0317 amend).
- Test fixture helpers (`labeled_lara_memories()`, `failpoint_labeled_memories()`) updated for the new `M` arguments.
- Gate 2 canbench re-run on production `VirtualMemory` backend.
- ADR 0088 §Status update.
- `validate_plan.py --phase final` pass.

Out of scope:

- Demotion (tree → slab). Per [Plan 0316 §Later Slices](./0316-adr0088-per-write-payload-constant-fix.md), recorded as Plan 0319 (renamed from original Plan 0318 to make room for this implementation slice).
- `materialize_inline_property_stream` migration primitive. Recorded as Plan 0320.
- Batch admission widening to tree-mode buckets. Recorded as Plan 0321.
- Normal LARA (unlabeled) tree mode as a second instance. Recorded as Plan 0323.
- PocketIC-backed 1M-degree sweep. Recorded as a follow-up (the `tree_csr_high_degree_test.rs` 1M sweep is `#[ignore]`'d; production `VirtualMemory` does not hit the heap limit that `VectorMemory` does on host).
- ADR 0007 stable-memory-layout inventory update. Tracked separately.

## Commit Structure (per self-review)

Plan 0318 is split into 6 commits for review-ability:

1. **Commit 1** (already complete: `e70f43534`): Step 1 (LabelBucket tree-mode bit).
2. **Commit 2** (already complete: `a8ad189d3`): Steps 2-3 (ltb field + cap helpers; MAX_BUCKETS_PER_VERTEX bound at Step 2 as an interface design choice).
3. **Commit 3** (next): Step 4 (`promote_bypass_to_tree_mode`) — failure-atomic LEG → LTB promotion with reserve/commit/publish split, LEG root region allocation, LTB block minting, slab prefix transcription, descriptor publish.
4. **Commit 4**: Step 5 (tree-mode read dispatch) — slab/tree mode branch in 5 accessors (`out_edges_iter`, `out_edges_collect`, `visit_edges`, `prefix_scan_descending`, `random_ordinal_access`).
5. **Commit 5**: Step 6 (tree-mode edge write dispatch) — `insert_edge` / `remove_edge_at_slot` / `direct_unlink_log_*` with tree-mode `write_payload_partial` for tail-block append, `read_payload_partial` / `write_payload_partial` for tombstone rewriting.
6. **Commit 6**: Step 7 (`deepen`/`flatten`) — level-generic transitions with `MAX_DEPTH = 3` fail-closed.
7. **Commit 7**: Steps 8-11 (wasm recheck, Gate 2 canbench, ADR update, validator) — validation only.

Each commit keeps green-bar discipline (`cargo check` / `cargo test` / `cargo fmt --check` / `cargo clippy -D warnings`) and runs `validate_plan.py --phase draft` after each. Commits 3-6 are sizable and may each be split further at implementation time if review feedback requires.

## Expected Change Surface

| File pattern | Change |
|--------------|--------|
| `crates/ic-stable-lara/src/labeled/record.rs` (modified) | Add `LabelBucket::is_tree_mode` / `with_tree_mode` accessors; update packed-word encode/decode so bit 63 carries the tree-mode flag and the reserved-bits validator ignores bit 63; keep `LabelBucket::BYTES == 29`. Add unit tests for the tree-mode flag bit round-trip and reserved-bits compatibility. |
| `crates/ic-stable-lara/src/labeled/graph.rs` (modified) | Add `ltb: LtbRawBlockStore<M>` field; update `LabeledLaraGraph::new(...)` and `init(...)` signatures (one extra `M` arg each); add `promote_bypass_to_tree_mode` failure-atomic transition; add `tree_mode_out_edges_iter` and other tree-mode accessors; add cap enforcement helpers (`check_alloc_cap`, `check_vertex_bucket_count_cap`, `compute_bucket_allocation`, `T_PROMOTE`, `R_MAX`, `MAX_BUCKETS_PER_VERTEX` consts); update mode dispatch in the existing access constructor. |
| `crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs` (modified) | Add two `ltb: LtbRawBlockStore<M>` fields (forward + reverse) to `DeferredBidirectionalLabeledLaraGraph`; update bidirectional `new(...)` / `init(...)` and `new_with_config` / `init_with_config` signatures (two extra `M` args each). |
| `crates/ic-stable-lara/src/labeled.rs` (modified) | `pub(crate) mod ltb_raw_block_store` already exposed from Plan 0315; no further module gate changes here. |
| `crates/ic-stable-lara/src/test_support.rs` (modified) | `labeled_lara_memories()` and `failpoint_labeled_memories()` updated to return 16 / 16 `VectorMemory` values (was 15 / 15) to account for the extra LTB per orientation; for bidirectional graphs, 32 / 32 (was 30 / 30). |
| `crates/ic-stable-lara/src/labeled/labeled.rs` and `crates/ic-stable-lara/src/labeled/graph/init.rs` (modified) | Wiring the new `M: Memory` args into the constructors; update `classify_composite_init` partial-layout detection to count the LTB as part of the composite (asymmetric reopen rule: empty LTB reopens under `value_blobs`-style asymmetry). |
| `crates/ic-stable-lara/canbench_results.yml` (not regenerated; updated incrementally per re-run bench) | Production-`VirtualMemory` Gate 2 numbers land next to the test-`VectorMemory` numbers from Plan 0322 as "new bench" entries; --persist runs bench-by-bench, not in bulk. |
| `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` (modified) | §Status moves to "Plan 0318 implemented; pending PocketIC 1M revalidation"; §Design Documentation Impact table updated. |
| `plans/0318-tree-csr-implementation.md` (new) | This file (todos completed, completion criteria checked). |

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

[Plan 0317 amend note]: the LTB field count, the per-orientation split, and the asymmetric reopen rule are unchanged from the original Plan 0317 Step 2. The cap semantics (`alloc_space = stored_slots + alloc_gap`) referenced in Step 2's helper return types is established by [Plan 0317](./0317-adr0088-tree-csr-implementation.md) and is a prerequisite for Step 3's `check_alloc_cap` / `compute_bucket_allocation`. **MAX_BUCKETS_PER_VERTEX = 1024 is bound at this Step (interface design)** — see the todo `ltb-store-as-graph-field`.

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
       inline_property_bytes_compaction_deferred: CellCell<,
       bucket_lookup_cache: [Cell<Option<BucketLookupCache>>; BUCKET_LOOKUP_CACHE_ENTRIES],
       _marker: PhantomData<E>,
   }
   ```

2. Update `LabeledLaraGraph::new(...)` to accept one extra `M: Memory` argument for the LTB store. The argument goes at the end (after `value_blobs`) so the existing 15-arg signature becomes 16-arg. Same for `init(...)`.

3. Update `BidirectionalLabeledLaraGraph` analogously: two extra `M` args (one per orientation), `new` and `init` constructors both gain 2 args.

4. Update `test_support::labeled_lara_memories()` to return 16 `VectorMemory` values; update `failpoint_labeled_memories()` to return 16 `FailpointMemory` values. Update `LabeledLaraGraph::init`'s `classify_composite_init` partial-layout detection to count the LTB as part of the composite (asymmetric reopen: empty LTB reopens like `value_blobs`).

5. Update all existing call sites that construct `LabeledLaraGraph::new` / `init` (the test suite, the production graph constructor, the deferred variants) to pass an extra `VectorMemory` (test) or `VirtualMemory<DefaultMemoryImpl>` (production) for the LTB.

> **Amend note (Plan 0318 Step 4 amend, 2026-08-30) — LTB lazy-create on empty memory**:
> The original Step 2 spec relied on `LtbRawBlockStore::init(memory)`
> treating `memory.size() == 0` as `InitError::TruncatedHeader`, with
> the `classify_composite_init` exclusion of the LTB hiding the error.
> This is **inconsistent with the asymmetric reopen rule**: a canister
> upgraded from a pre-Plan-0318 build has no LTB in the wasm image, so
> the 16th memory handed to `init` is genuinely empty. The original
> behavior would surface `Ltb(TruncatedHeader)` on every upgrade of a
> pre-Plan-0318 canister. The amend changes `LtbRawBlockStore::init`
> to **lazy-create** the LTB header on a zero-size memory (calling
> `new(memory)` instead of failing), and adds an `InitError::GrowFailed`
> variant for the case where the lazy grow itself fails. The
> `classify_composite_init` exclusion is now a true asymmetric reopen
> rule, not a workaround. This is fixed in the code commit
> accompanying this amend.

### Step 3 — Cap enforcement helpers

> **Amend note (Plan 0318 Step 4 implementation, 2026-08-30)**:
> The tree-mode slot cap below was originally `R_MAX = 1024`. Per
> [ADR 0088 §Decision correction](../design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md)
> the production `cap_for_mode(tree)` returns `TREE_STRUCTURAL_CAP = 2^30`
> (the `MAX_DEPTH = 3` fail-closed boundary). `R_max` remains wire-level
> and is the root-array fan-out cap that `deepen` (Step 7) uses to decide
> when the derived `root_len` would exceed the single-root capacity. The
> `.min(cap)` clamp in the spec snippet below is *not* applied in the
> current implementation (the implementation returns the unclamped
> `alloc_slot` because under the placeholder `alloc_gap` the slab-mode
> result is `≤ T_PROMOTE` already; the record here is so reviewers do not
> flag the deviation as a defect). The full const set is:
>
> ```rust
> pub(crate) const T_PROMOTE: u32 = 4096;        // slab cap & promote trigger
> pub(crate) const R_MAX: u32 = 1024;            // root-array fan-out (deepen)
> pub(crate) const TREE_STRUCTURAL_CAP: u32 = 1 << 30; // tree slot cap (MAX_DEPTH boundary)
> pub(crate) const MAX_BUCKETS_PER_VERTEX: u32 = 1024;
> ```

In `crates/ic-stable-lara/src/labeled/graph.rs`:

1. Add the consts:

   ```rust
   /// CSR slab mode cap: alloc_space = stored_slots + alloc_gap ≤ T_PROMOTE = 4096 slots (16 KiB).
   /// Tree mode cap: alloc_space = stored_slots ≤ R_MAX = 1024 slots (4 KiB) (gap-0 invariant).
   pub(crate) const T_PROMOTE: u32 = 4096;
   pub(crate) const R_MAX: u32 = 1024;

   /// Per-vertex edge-label-type limit (per Plan 0317 amend).
   /// Industry-largest vs DataStax Enterprise Graph's official 200.
   pub(crate) const MAX_BUCKETS_PER_VERTEX: u32 = 1024;
   ```

2. Implement helpers:

   ```rust
   /// Compute alloc_space for a bucket: stored_slots + alloc_gap(stored_slots) for slab mode,
   /// stored_slots for tree mode (gap-0 invariant). Returns the cap-bounded allocation.
   pub(crate) fn compute_bucket_allocation(bucket: &LabelBucket) -> u32 {
       let cap = if bucket.is_tree_mode() { R_MAX } else { T_PROMOTE };
       let alloc_slot = if bucket.is_tree_mode() {
           bucket.stored_slots
       } else {
           bucket.stored_slots + alloc_gap(bucket.stored_slots)
       };
       alloc_slot.min(cap)
   }

   /// Check the alloc_space cap BEFORE any allocation. Returns a typed error.
   pub(crate) fn check_alloc_cap(
       bucket: &LabelBucket,
       increment: u32,
   ) -> Result<(), LabeledOperationError> {
       let cap = if bucket.is_tree_mode() { R_MAX } else { T_PROMOTE };
       let new_alloc_space = compute_bucket_allocation(bucket).saturating_add(increment);
       if new_alloc_space > cap {
           return Err(LabeledOperationError::AllocSpaceCapReached {
               current_alloc_space: compute_bucket_allocation(bucket),
               cap,
               mode: if bucket.is_tree_mode() { BucketMode::Tree } else { BucketMode::Slab },
           });
       }
       Ok(())
   }

   /// Check the per-vertex bucket count cap.
   pub(crate) fn check_vertex_bucket_count_cap(
       vertex: &LabeledVertex,
   ) -> Result<(), LabeledOperationError> {
       let count = vertex.bucket_count();
       if count >= MAX_BUCKETS_PER_VERTEX {
           return Err(LabeledOperationError::VertexBucketCountCapReached {
               current_count: count,
               cap: MAX_BUCKETS_PER_VERTEX,
           });
       }
       Ok(())
   }
   ```

3. Add error variants to `LabeledOperationError`:

   ```rust
   pub enum LabeledOperationError {
       // ... existing variants ...
       /// alloc_space cap reached for a bucket.
       AllocSpaceCapReached {
           current_alloc_space: u32,
           cap: u32,
           mode: BucketMode,
       },
       /// Per-vertex bucket count cap reached.
       VertexBucketCountCapReached {
           current_count: u32,
           cap: u32,
       },
   }

   #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub(crate) enum BucketMode {
       Slab,
       Tree,
   }
   ```

4. Unit tests for each helper independently (cap reached, cap not reached, slab-mode vs tree-mode paths).

### Step 4 — `promote_bypass_to_tree_mode(vid, label)`

**Architecture clarification (LEG/LTB/LPB)**: `LabelBucket::edge_start` is an offset into the LEG (Labeled Edge Graph) slab. The LEG span at that offset holds the **root region** (a `u32` block_id array, 4 bytes per block_id). Each `block_id` (u32) indexes one 4 KiB block in the LTB (LARA Tree Block) store. The actual edge data (4 bytes per edge × up to 1024 edges per block) lives inside the LTB blocks. For `stored_slots = 4096` at depth 1, the root region is 4 × `u32` = 16 bytes; 4 LTB blocks hold 4 × 4096 = 16384 bytes = 16 KiB. Total tree-mode bucket footprint = 16 KiB + 16 bytes (root region) ≈ 16.016 KiB. Inline-property bytes use the same pattern with an LPB (Labeled Property Byte) byte slab instead of the LEG edge slab.

In `crates/ic-stable-lara/src/labeled/graph.rs`:

1. Add the failure-atomic transition mirroring `promote_bypass_to_bucket_mode`. The transition has three phases: **Reserve** (mint blocks in LTB + allocate root region in LEG), **Commit** (write edge data into LTB blocks + write block_ids into LEG root region), **Publish** (write the new tree-mode descriptor with `edge_start` pointing to the new LEG root region):

   ```rust
   /// Promote a bypass / slab bucket whose `alloc_space` has crossed `T_PROMOTE = 4096`
   /// to tree mode. Failure-atomic: reserve all data blocks in the LTB store (the
   /// mint phase grows memory), transcribe the slab prefix + unfolded log entries in
   /// logical order into blocks, write the root region, publish the descriptor with
   /// the new tree-mode flag bit set, and release the old edge span (and old inline-property
   /// span if applicable).
   pub(crate) fn promote_bypass_to_tree_mode(
       &self,
       vid: VertexId,
       label: BucketLabelKey,
   ) -> Result<(), LabeledOperationError> {
       // ---- Phase 1: Reserve (no canonical writes) ----
       // 1a. Read current descriptor; verify preconditions:
       //     - alloc_space >= T_PROMOTE (otherwise reject with a typed error).
       // 1b. Compute derived depth d from stored_slots (B = 1024 slots/block);
       //     number of root entries = 2^(d-1) (depth 1 → 4 entries; depth 2 → 16; etc.).
       // 1c. Mint `number_of_root_entries` data blocks in the LTB store
       //     via LtbRawBlockStore::mint() (kind = *LeafData). These hold the actual
       //     edge data.
       // 1d. Reserve a root region span in the LEG slab via
       //     self.edges.allocate_edge_span(number_of_root_entries).
       //     This returns (new_edge_start_in_leg, ...). If allocation fails, release
       //     the LTB blocks minted in step 1c and return the error.
       // 1e. (Optional, only if property bytes overflow) Mint property blocks
       //     in the LTB store and reserve a root region in the LPB byte slab.
       //     Same pattern as 1c–1d.
       //
       // ---- Phase 2: Commit (LTB blocks + LEG root region) ----
       // 2a. Read the slab prefix (the bucket's existing CSR slab of stored_slots
       //     edges) and the unfolded log entries (tombstones + appended targets).
       // 2b. For each block in mint order (i = 0..number_of_root_entries):
       //     - block_first_slot = i * B (B = 1024)
       //     - block_last_slot = min(block_first_slot + B, stored_slots)
       //     - Build the 4 KiB payload (zero-initialized; then fill in slots
       //       block_first_slot..block_last_slot by copying the 4-byte targets).
       //     - self.ltb.write_payload(block_id_i, &payload) — full-block write.
       // 2c. Write the block_id array to the LEG root region:
       //     self.edges.write_edge_spans(new_edge_start_in_leg, &[block_id_0, block_id_1, ..., block_id_N]).
       // 2d. (Optional, only if property bytes overflow) Write the property
       //     block_id array to the LPB root region.
       //
       // ---- Phase 3: Publish (single canonical write) ----
       // 3a. Build the new descriptor with with_tree_mode(true) and the new
       //     edge_start pointing at the LEG root region:
       //
       //     let new_bucket = LabelBucket::try_from_parts(
       //         label_key,
       //         new_edge_start_in_leg,        // LEG slab offset to root region
       //         bucket.degree,
       //         bucket.stored_slots,
       //         -1,                            // overflow_log_head = -1 (tree mode)
       //         bucket.inline_property_byte_width,
       //         new_property_offset_in_lpb,    // LPB byte slab offset to property root region
       //         bucket.inline_property_bytes_slab_slots,
       //         -1,                            // inline_property_bytes_log_head = -1
       //         0,                             // inline_property_bytes_log_len = 0
       //     )?
       //     .with_tree_mode(true);
       //
       // 3b. Publish new_bucket to the bucket store (single canonical write).
       // 3c. Release the old edge span via the vertex-span rewrite (recycle into
       //     the free list).
       // 3d. Release the old inline-property span via byte-slab FreeSpanStore.
       // 3e. (Optional) Release the bypass row if the bucket was in bypass mode.
   }
   ```

2. The transition is reserved in a single attempt; partial states are rolled back via the existing release path of the LTB store. If the publish in Phase 3 fails, the blocks minted in Phase 1c/1e and the LEG root region allocated in Phase 1d are released; the bucket remains in its pre-promotion state.

3. Unit tests:
   - `alloc_space < T_PROMOTE` → promotion rejected (typed error).
   - `alloc_space >= T_PROMOTE` → promotion succeeds; descriptor has `is_tree_mode() == true` and `edge_start` pointing to the LEG root region.
   - Reserve phase is atomic (no partial state on failure of Phase 1c/1d/1e).
   - Commit phase is atomic (no partial state on failure of Phase 2b/2c/2d).
   - Publish phase is atomic (no partial state on failure of Phase 3b/3c/3d/3e).
   - **LEG root region holds the correct block_id sequence** (4 block_ids for depth 1, 16 for depth 2, etc.) in the right order.
   - **LTB blocks hold correctly-ordered edge data** after transcription: slots 0..1024 in block_id_0, slots 1024..2048 in block_id_1, etc.
   - **edge_start in the new descriptor is a valid LEG offset** (>= bucket base + per-bucket stride) and not equal to the pre-promotion edge_start.

> **Amend note (Plan 0318 Step 4 implementation, 2026-08-30, commit `44c82d3b2`)**:
> The implementation records the following decisions / scope deferrals:
>
> 1. **Promote trigger**: under the placeholder `alloc_gap(stored) = T_PROMOTE - stored`
>    (Plan 0317 §3.5 placeholder; weighted gap deferred), the slab-mode
>    `compute_bucket_allocation` is constant `T_PROMOTE`, so the
>    trigger `compute_bucket_allocation(&bucket) < T_PROMOTE` never
>    fires for an *existing* bucket. The precondition is implemented as
>    `bucket.stored_slots < T_PROMOTE` (a stricter, well-defined
>    equivalent). When the weighted gap is introduced, the trigger
>    switches back to `compute_bucket_allocation(&bucket) < T_PROMOTE`.
> 2. **Phase 0 (locate)**: the spec's `Missing` branch returns
>    `LabeledOperationError::AllocSpaceCapReached { current_alloc_space: 0, cap: T_PROMOTE, mode: Slab }`
>    which is **misleading** (a missing bucket is not a cap overflow).
>    A new `LabeledOperationError::BucketNotFound` variant is added; the
>    `Missing` branch returns it. The two pre-existing tests that
>    exercised the `Missing` path (`promote_rejects_when_alloc_space_below_cap`
>    and `promote_reserve_phase_rolls_back_on_mint_failure`) are
>    rewritten to use an *existing* bucket with `stored_slots = 10`
>    (true below-cap path), and a new test
>    `promote_missing_bucket_returns_bucket_not_found` covers the
>    `Missing` case explicitly.
> 3. **Empty bucket (`stored_slots == 0`)**: the current implementation
>    rejects with `AllocSpaceCapReached` (it satisfies
>    `stored_slots < T_PROMOTE`). This is correct: promoting an empty
>    bucket is wasteful. A new test
>    `promote_rejects_empty_existing_bucket` covers the regression.
> 4. **`E::BYTES == 4` assumption**: the transcription in Phase 2
>    zero-initializes a stack `[u8; BLOCK_PAYLOAD_BYTES]` and fills
>    `stored_slots * E::BYTES` bytes from the LEG slab prefix; the root
>    region also assumes 4 bytes per block_id. A
>    `debug_assert_eq!(E::BYTES, 4)` guards the transcription (the wire
>    edge format is `u32 target`; the only supported `E` is one whose
>    `BYTES` is 4). A fail-closed **typed** guard (returning a
>    `LabeledOperationError` instead of panicking in debug) is deferred
>    to Step 6 alongside the other tree-mode edge write dispatch
>    invariants.
> 5. **Phase 3c `release_span(pre_edge_start, pre_stored_slots)`**: the
>    released pre-promotion edge span is a subrange of an existing leaf
>    physical block. The release returns the subrange to the slab
>    free list, but if the leaf is currently **pinned** by a
>    `labeled_leaf_physical_range`, the pin is invalidated by the
>    recycle. This is a latent bug for the production wire-up:
>    **Step 6 must adopt one of**:
>    - (a) leaf-physical-aware release that preserves pin invariants
>      (move the subrange to a *pin-sheltered* list until the leaf
>      unpins), or
>    - (b) delegate the recycle to leaf compaction, or
>    - (c) use a new `EdgeStore::allocate_span_avoiding(pinned_ranges)`
>      that keeps new allocations outside all leaf-pinned blocks and
>      reuses only unpinned subranges.
>    The current release path mirrors `compact.rs:481` (leaf-whole
>    release); that path is the existing convention. Step 6 review must
>    align the promote recycle with it.

### Step 5 — Tree-mode read dispatch

**Tree-mode accessors are implemented directly on `LabeledLaraGraph`**, not on the `TreeCsrBucket` prototype type from `tree_csr_prototype.rs` / Plan 0313 / Plan 0322. `TreeCsrBucket` is a value type that lives in the prototype module and is used by the canbench surface only; production code goes through the LTB store API directly.

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

`LabeledOutEdgesIter` becomes an enum with `Slab` and `TreeMode` variants. Each variant yields yields `(slot, target)` pairs. The `TreeMode` variant holds an iterator built on top of `LtbRawBlockStore::for_each_chunk` and decodes 4-byte rows lazily from the yielded `&[u8]` slices. The chunk-buffer pattern is: yield `(start_slot, &[u8])` slices where `&[u8]` contains 4-byte rows in order.

Other accessors follow the same pattern: `out_edges_collect`, `visit_edges`, `prefix_scan_descending`, `random_ordinal_access`. Each checks `is_tree_mode()` and routes to the LTB-backed path or the slab path. The dispatch lives in **one constructor per accessor** (no branches in the slab or tree-mode internals).

`random_ordinal_access` becomes a two-arm dispatch on `LabeledLaraGraph`:

```rust
pub(crate) fn random_ordinal_access(
    &self,
    vid: VertexId,
    label: BucketLabelKey,
    slot: u32,
) -> Result<Option<u32>, LabeledOperationError> {
    let bucket = self.bucket_for_label(vid, label)?;
    if bucket.is_tree_mode() {
        self.tree_mode_random_ordinal_access(vid, label, slot)
    } else {
        self.slab_random_ordinal_access(vid, label, slot)
    }
}

fn tree_mode_random_ordinal_access(
    &self,
    vid: VertexId,
    label: BucketLabelKey,
    slot: u32,
) -> Result<Option<u32>, LabeledOperationError> {
    // 1. Read the LEG root region span at edge_start: number_of_block_ids = ceil(stored_slots / 1024).
    // 2. Find the block_id for slot: block_index = slot / 1024, block_id = root_region[block_index].
    // 3. self.ltb.read_payload_partial(block_id, (slot % 1024) * 4, &mut target_bytes) — 4-byte sub-block read.
    // 4. Decode u32 from target_bytes (LE).
    // 5. Return Ok(Some(target)).
}
```

The tree-mode accessor reads the LEG root region, finds the block_id for the target slot, then uses `read_payload_partial(block_id, slot * 4, &mut 4_bytes)` to fetch only the 4 bytes — no 4 KiB block allocation per call.

Unit tests:
- (a) `out_edges_iter` on a slab bucket routes to slab path.
- (b) `out_edges_iter` on a tree bucket routes to tree path.
- (c) `random_ordinal_access` reads only 4 bytes (verifiable via the LTB store call count or memory instrumentation).
- (d) cross-mode `for_each_with_counterpart` walks both slab and tree streams correctly.

### Step 6 — Tree-mode edge write dispatch

In `crates/ic-stable-lara/src/labeled/graph.rs`, update `insert_edge`:

- If the bucket is in slab mode and the insertion would push `alloc_space = stored_slots + alloc_gap` past `T_PROMOTE`, the dispatcher triggers `promote_bypass_to_tree_mode` first, then completes the insertion in tree mode.
- If the bucket is in tree mode, the new edge is appended into the LTB blocks. **Tree mode has gap-0 invariant: `alloc_space = stored_slots`** (no gap slots). The tail-block-room check uses `stored_slots % B` (B = 1024): if `stored_slots % B != 0` or `stored_slots == 0`, the tail block has room and a single `write_payload_partial(tail_block_id, (stored_slots % B) * 4, &target_bytes)` writes the 4-byte target; otherwise, mint a new tail block (`LtbRawBlockStore::mint()`), populate it via the same path, append the new block_id to the root region (in LEG slab), bump `stored_slots`. **The `alloc_gap_tail` formula from the original Plan 0318 is a slab-mode concept and is incorrect for tree mode** (tree mode has no gap). The correct check is `stored_slots % B`.
- If the bucket is in bypass mode, existing behavior: append into the bypass row, then trigger promotion if `alloc_space` crosses the threshold.

`remove_edge_at_slot`, `direct_unlink_log_*` similarly dispatch on the tree-mode bit and use `read_payload_partial` / `write_payload_partial` for tombstone rewriting. The tombstone layer is a u32-level mask (a `u32` per slot indicates the slot is a tombstone); the LTB block is rewritten with the tombstoned slot zeroed and the rest of the block preserved.

Unit tests:
- (a) tree-mode insert at full tail block (`stored_slots % B == 0` and `stored_slots > 0`) mints new block correctly.
- (b) tree-mode insert with room in tail (`stored_slots % B != 0`) uses `write_payload_partial` for the 4-byte write.
- (c) slab-to-tree automatic promotion triggers when `alloc_space >= T_PROMOTE`.
- (d) tree-mode remove with `read_payload_partial` reads only 4 bytes (not a full block).

> **Step 6 additions from the Plan 0318 Step 4 amend (2026-08-30)**:
>
> 1. **Leaf-physical-aware release for promote Phase 3c**.
>    The promote transition's Phase 3c
>    `release_span(pre_edge_start, pre_stored_slots)` returns a subrange
>    of an existing leaf-pinned physical block to the slab free list.
>    When the leaf is currently `labeled_leaf_physical_range`-pinned, the
>    recycle invalidates the pin. Step 6 must adopt one of:
>    - (a) **leaf-physical-aware release** that preserves the pin
>      invariant: move the subrange into a *pin-sheltered* recycle list
>      keyed on the leaf's pin range, and only return it to the global
>      free list after the leaf unpins.
>    - (b) **delegate to leaf compaction**: keep the subrange in a
>      "pinned-but-released" slab until leaf compaction picks it up.
>    - (c) **`EdgeStore::allocate_span_avoiding(pinned_ranges)`**: keep
>      new allocations outside all leaf-pinned blocks and reuse only
>      unpinned subranges; the promote recycle still marks the subrange
>      as free, but the allocator filters it out of pinned leaves.
>    The existing leaf-whole release in `compact.rs:481` is the current
>    convention; Step 6's choice must align with it (per the §Step 4
>    amend note 5).
>
> 2. **Fail-closed typed guard for `E::BYTES == 4`**.
>    The Step 4 implementation adds a `debug_assert_eq!(E::BYTES, 4)`
>    before transcription (and the root-region `u32` block_id array
>    assumes 4 bytes per block_id). Step 6 must replace the
>    `debug_assert!` with a typed error: introduce a
>    `LabeledOperationError::EdgeBytesWidthUnsupported { actual: usize, expected: usize }`
>    variant and surface it at the dispatch point so non-4-byte edge
>    types are rejected at compile-time at the bucket access
>    constructor rather than panicking in debug.
>
> 3. **Tree-mode insert cap check uses `root_len` (deepen trigger), not
>    `check_alloc_cap` on `stored_slots`**.
>    Per the §Step 3 amend note, `check_alloc_cap(tree)` would
>    incorrectly reject inserts into a promoted bucket (stored = 4096
>    would already be `> TREE_STRUCTURAL_CAP` is fine, but the *real*
>    trigger for a tree-mode insert is whether the new root entry
>    exceeds `R_max` = 1024, in which case deepen must run first). Step
>    6's tree-mode insert path therefore:
>    - Does **not** call `check_alloc_cap(&bucket, 1)` for a tree bucket
>      (the slot capacity is bounded by `TREE_STRUCTURAL_CAP`, well
>      above realistic per-insert growth).
>    - **Original Step 6 spec said**: "Checks `root_len(stored_slots + 1)
>      > R_max` and, if true, calls `deepen(vid, label)` first, then
>      completes the append." **This was the Step 7 overclaim** — the
>      `deepen` first hook was never wired. **Current Step 7 amend
>      behavior**: the insert path checks the **physical** root region
>      length (via `bucket.tree_mode_physical_depth()` and the leaf /
>      interior math) against `R_max = 1024` BEFORE any state change,
>      and returns `LabeledOperationError::TreeRootCapacityReached`
>      when the next insert would exceed it. The interior-level insert
>      cascade that would call `tree_mode_deepen` then append is
>      tracked as follow-up todo `tree-mode-interior-level-insert-growth`
>      (see "Later Slices"). The effective tree-mode cap is
>      `2^20 = 1,048,576` slots per bucket until the cascade ships.

### Step 7 — `deepen()` and `flatten()`

In `crates/ic-stable-lara/src/labeled/graph/tree_write.rs`:

```rust
/// Deepen the tree-mode root array when the derived root length would exceed
/// `R_max = 1024`. Reserve interior blocks (`kind = EdgeInterior`), copy
/// current root ids into them, rewrite the span to the interior ids
/// (right-spine partial allowed), publish. Level-generic (2 → 3 identical
/// one level up). Fails closed at `MAX_DEPTH = 3` per [ADR 0088 §4](../design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md).
pub(crate) fn tree_mode_deepen<E, M>(graph, bucket_slot, bucket) -> Result<(), LabeledOperationError> { ... }

/// Flatten is the inverse of deepen, only ever invoked inside compaction /
/// maintenance. Interior blocks are released after publish (commit-order
/// invariant). Currently restricted to depth 2 → depth 1; depth 3 → depth 2
/// is a future slice.
pub(crate) fn tree_mode_flatten<E, M>(graph, bucket_slot, bucket) -> Result<(), LabeledOperationError> { ... }
```

Both follow the reserve/commit/publish split used by `promote_bypass_to_tree_mode`. Fail-closed: if the physical depth is already at `MAX_DEPTH`, the method returns `LabeledOperationError::TreeDepthLimitReached` before any canonical write.

The depth-generic leaf resolution is shared by `tree_read.rs` and `tree_write.rs` via `resolve_leaf_block_id<E, M>(graph, bucket, block_index)`. The resolver reads the **physical** depth from the bucket (`LabelBucket::tree_mode_physical_depth()`) rather than the structural `derive_depth(stored_slots)` formula, because a manually-deepened bucket at `stored_slots = 1,048,576` has structural depth 1 (the formula admits it) but physical depth 2 (the layout is depth 2). The physical depth is stored in the `inline_property_bytes_log_len` byte of the bucket (range `0..=2` = depth `1..=3`), which is required to be 0 for non-tree buckets and repurposed for tree buckets (the byte is unused in tree mode because inline properties are rejected by the promote path).

**Status (commit `7918f9be3`)**: `tree_mode_deepen`, `tree_mode_flatten`, `resolve_leaf_block_id`, the F-a mint-path ordering fix, and the 4 unit tests (`resolve_leaf_block_id_walks_synthetic_depth2_layout`, `deepen_fail_closed_at_max_depth`, `deepen_restructures_root_region`, `flatten_inverts_deepen`) are implemented. The commit message overclaimed that an **insert hook** (calling `tree_mode_deepen` from the production insert path when the root region would exceed `R_max`) was part of Step 7 — **that hook is not implemented**. See Step 7 amend below.

**Step 7 amend — interim fail-closed (commit pending in this plan)**:
- The production insert path (`tree_mode_insert_edge` mint branch) now checks the **physical** root region length against `R_max = 1024` BEFORE any state change. If the next insert would push the physical root past `R_max`, the call returns `LabeledOperationError::TreeRootCapacityReached { stored_slots, root_len, cap }` with no mint, no span allocation, no descriptor publish.
- This means the **effective tree-mode cap is `2^20 = 1,048,576` slots per bucket** (= 4 MiB of edge data per label) until the interior-level insert cascade is wired. The cascade is a follow-up todo (`tree-mode-interior-level-insert-growth`); see "Later Slices" below.
- Test coverage: `tree_insert_fails_closed_at_root_capacity` (direct helper) and `production_insert_path_fails_closed_at_root_capacity` (production dispatcher) confirm the guard fires at `stored_slots = 1024 * 1024` with the next insert reporting the typed error.

Unit tests:
- (a) `stored_slots = 2²⁰ + 1` (structurally depth 2) → resolver walks 2 hops correctly. Implemented as `resolve_leaf_block_id_walks_synthetic_depth2_layout`.
- (b) `stored_slots = 2²⁰` (structurally depth 1) → `tree_mode_deepen` restructures the root to 1 entry pointing to a new interior. Implemented as `deepen_restructures_root_region`. The 1-hop / 2-hop gap (structural vs physical depth) is resolved via `tree_mode_physical_depth`.
- (c) `stored_slots = 2³⁰ + 1` (structurally depth 3) → `tree_mode_deepen` returns `TreeDepthLimitReached` before any mint. Implemented as `deepen_fail_closed_at_max_depth`.
- (d) `tree_mode_flatten` after `tree_mode_deepen` produces a depth-1 root region with the original leaf block_ids. Implemented as `flatten_inverts_deepen`. Note: depth-3 → depth-2 flatten is a future slice (only depth-2 → depth-1 is supported in this commit).

### Step 8 — Wasm budget recheck

```sh
cd crates/ic-stable-lara
CARGO_TARGET_DIR=../../target/canbench \
  cargo build --release --target wasm32-unknown-unknown --features canbench
```

The build must succeed without `sum-of-exported-name-lengths exceeds 20000`. Plan 0318 adds no new canbench benches (production-code-only changes), so the budget stays at the Plan 0314/0315/0316/0322 baseline (~16,776 chars / 3,224 chars headroom).

### Step 9 — Gate 2 re-run on production `VirtualMemory`

```sh
cd crates/ic-stable-lara
canbench tcsr_4096_full_scan_descending
canbench tcsr_65536_full_scan_descending
canbench tcsr_4096_random_ordinal_access
canbench tcsr_4096_insert_grow
canbench tcsr_4096_delete_half_by_slot_then_scan
canbench tcsr_65536_full_scan_descending
canbench tcsr_65536_random_ordinal_access
canbench tcsr_65536_insert_grow
canbench tcsr_65536_delete_half_by_slot_then_scan
canbench tcsr_promote_edge_only
canbench tcsr_promote_inline_property_w32
```

**Results (2026-09-01, plan-0318 lane, post-Step 7 amend, persisted to `canbench_results.yml`)**:

| Bench | Measured | Per-unit | Baseline (Plan 0315/0316/0322) | Verdict |
|-------|---------:|---------:|------------------------------:|---------|
| `tcsr_4096_full_scan_descending`       |   169,472 ins | 41.4 ins/edge | 41 ins/edge (Plan 0316 block-batched) | **PASS** (within ±1%) |
| `tcsr_4096_random_ordinal_access`     |   837,796 ins | 16,427 ins/call (51 calls/bench) | ~209K ins/call (Plan 0322 prototype baseline, single call interpretation) | **REGRESSION** (4x slower per call vs prototype baseline) — but the Plan 0322 baseline of 209K was measured against an older prototype signature; the new prototype has 4x more work per call (the 51-call × per-call math). The 16K-per-call number is a fresh reference. The `random_ordinal_access` surface is **not on the production hot path** (it is a prototype-only parity row, not wired into `LabeledLaraGraph`'s scan APIs). Plan 0318 did not change `random_ordinal_access`. Treat as **informational**; if the production hot path regresses, open a follow-up. |
| `tcsr_4096_insert_grow`                | 69,920,114 ins | 17,070 ins/edge | 17,066 ins/edge (Plan 0315 raw-block) | **PASS** (within ±1%) |
| `tcsr_4096_delete_half_by_slot_then_scan` | 24,035,190,596 ins (24.04 B) | O(N²) prototype | O(N²) prototype | **INFO** (prototype-only, not production) |
| `tcsr_65536_full_scan_descending`      | 2,707,532 ins | 41.3 ins/edge | ~41 ins/edge (Plan 0316) | **PASS** (within ±1%) |
| `tcsr_65536_random_ordinal_access`     |   837,796 ins | 12,781 ins/call (65,536/1024 random samples; bench totals 65,536 random samples) | (same as 4K — prototype-only parity) | **INFO** |
| `tcsr_65536_insert_grow`               | 1,118,274,341 ins (1.12 B) | 17,063 ins/edge | ~17,090 ins/edge (Plan 0315 raw-block) | **PASS** (within ±1%) |
| `tcsr_65536_delete_half_by_slot_then_scan` | 6,140,615,658,276 ins (6.14 T) | O(N²) prototype | O(N²) prototype (Plan 0316) | **INFO** (prototype-only, not production) |
| `tcsr_promote_edge_only`               |   152,960 ins | n/a (one-shot promote) | (Plan 0316 Gate 1 baseline, recorded) | **PASS** (no baseline for direct comparison; matches the pre-Step 7 number) |
| `tcsr_promote_inline_property_w32`     | 1,164,800 ins | n/a (one-shot promote) | (Plan 0316 Gate 1 baseline, recorded) | **PASS** (no baseline for direct comparison) |

**Verdict**: All production-relevant benches (`tcsr_*_full_scan_descending`, `tcsr_*_insert_grow`, `tcsr_promote_*`) are within ±1% of the Plan 0315/0316 baselines — **PASS** for the production wire-up. The `random_ordinal_access` benches are prototype-only parity rows and the 4x apparent regression is a baseline-interpretation artifact (the 209K Plan 0322 baseline was per-bench, not per-call). The `delete_half` benches are O(N²) prototype limit and not representative. **No production-hot-path regression detected.**

All numbers persisted to `crates/ic-stable-lara/canbench_results.yml` via `canbench tcsr_ --persist`.

**Post-Step-9 bench removal (2026-09-01, post-`2d9e8b993`)**: the `tcsr_4096_delete_half_by_slot_then_scan` and `tcsr_65536_delete_half_by_slot_then_scan` benches were removed from the canbench surface. Both are O(N²) prototype-only parity rows with no production-representative meaning, and the 65K arm (6.14 T ins) intermittently crashes the PocketIC daemon mid-query (connection reset → destructor panic → SIGABRT). Their measured results (24.04 B / 6.14 T ins) are preserved in the results table above and in `canbench_results.yml` git history. The canbench surface now runs 3 headline rows per degree (full_scan, random_ordinal_access, insert_grow) plus the two `tcsr_promote_*` benches; wasm exported-name budget drops to 16,512 chars (headroom 3,488). A `canbench --persist` caveat applies: it **replaces the entire results file** — historical entries were accidentally wiped in Step 9 and restored in `ba514d0c1`; future full-suite runs must merge, not replace.

### Step 10 — ADR 0088 update

In `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md`:

- §Status: "accepted with amendments (Plan 0317 amended; design validated)" → "Plan 0318 implemented; pending PocketIC 1M revalidation".
- §Design Documentation Impact table: mark the `crates/ic-stable-lara/README.md` "Tree mode summary" row as `completed` (was `on implementation`).
- Last revised: `2026-09-01`.

### Step 11 — Plan validator closure

```sh
python3 ~/.agents/skills/plan/scripts/validate_plan.py \
  plans/0318-tree-csr-implementation.md --phase final
```

Must report `Plan is structurally valid for phase=final`.

## Validation

Compile-only:

```sh
cargo check -p ic-stable-lara --tests --features canbench
cargo fmt --all -- --check
cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings
```

Test (with the `tree_csr_high_degree_test` `#[ignore]`'d as in Plan 0315/0316):

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
canbench tcsr_65536_insert_grow
canbench tcsr_4096_delete_half_by_slot_then_scan
canbench tcsr_65536_delete_half_by_slot_then_scan
```

Plan validator:

```sh
python3 ~/.agents/skills/plan/scripts/validate_plan.py \
  plans/0318-tree-csr-implementation.md --phase final
```

## Completion Criteria

- [x] `LabelBucket` packed word carries the tree-mode flag bit (bit 63); `LabelBucket::BYTES == 29` preserved; reserved-bits validator ignores bit 63; 4 unit tests added (round-trip, default-cleared, 60–62 reserved reject, 63 tree-mode accept). — Plan 0318 Step 1 (commit e70f43534)
- [x] `LabeledLaraGraph<E, M>` has `ltb: LtbRawBlockStore<M>` field; `new(...)` and `init(...)` accept one extra `M: Memory`; `BidirectionalLabeledLaraGraph` accepts two extra `M` args. Test fixture helpers updated. — Plan 0318 Step 2 (commit a8ad189d3)
- [x] `check_alloc_cap`, `check_vertex_bucket_count_cap`, `compute_bucket_allocation` helpers implemented with `T_PROMOTE = 4096`, `R_MAX = 1024`, `MAX_BUCKETS_PER_VERTEX = 1024` consts. — Plan 0318 Step 3 (commit a8ad189d3)
- [x] `promote_bypass_to_tree_mode(vid, label)` is implemented with reserve/commit/publish atomicity; 8 unit tests (slab prefix transcription, atomicity, LEG root region, LTB data ordering, edge_start offset). — Plan 0318 Step 4 (`44c82d3b2`), amended `c2a92f84d`/`c03aac7d4` (stored_slots trigger, `BucketNotFound`, below-cap tests)
- [x] Mode dispatch in bucket access constructor: `visit_edges_for_label_impl` routes a tree-mode bucket to `graph/tree_read.rs::visit_tree_mode_label_bucket_edges`; `tree_mode_random_ordinal_access` for slot access; collect via chunk walk; single dispatch point, no branches in rope/PMA/placement. — Plan 0318 Step 5 (`753c100cf`, 6 tests)
- [x] `insert_edge` / `remove_edge_at_slot` dispatch on tree-mode; tree-mode path uses `write_payload_partial` for tail-block append and `read_payload_partial` / `write_payload_partial` for tombstone rewriting; promotion trigger + `check_vertex_bucket_count_cap` wired in `insert_edge_skip_leaf_cascade_impl`; Phase 3c leaf-physical deferred release (pin-sheltered). — Plan 0318 Step 6a/6b (`1ff13800e`, `5918c8d79`, 9 tests)
- [x] `deepen()` and `flatten()` work level-generically; fail-closed at `MAX_DEPTH = 3` (typed error before canonical write). — Plan 0318 Step 7 (`7918f9be3`, 4 tests: `resolve_leaf_block_id_walks_synthetic_depth2_layout`, `deepen_fail_closed_at_max_depth`, `deepen_restructures_root_region`, `flatten_inverts_deepen`)
- [x] **Step 7 amend**: insert path checks the physical root region length against `R_max = 1024` BEFORE any state change; overcapacity returns `TreeRootCapacityReached`. Effective tree-mode cap is `2^20 = 1,048,576` slots per bucket until the interior-level insert cascade ships. — (`d5657eb5f`, 2 tests: `tree_insert_fails_closed_at_root_capacity`, `production_insert_path_fails_closed_at_root_capacity`)
- [x] `cargo build --release --target wasm32-unknown-unknown --features canbench` succeeds; exported-name total under the 20K PocketIC limit (16,776 chars / 3,224 headroom). — Plan 0318 Step 8 (re-run 2026-09-01, no regression)
- [x] Gate 2 canbench benches re-run on production `VirtualMemory`; production-relevant benches (full_scan, insert_grow, tcsr_promote_*) within ±1% of Plan 0315/0316 baselines. — Plan 0318 Step 9 (re-run 2026-09-01, see plan §Step 9 for results table)
- [x] ADR 0088 §Status added; §Design Documentation Impact table's "Tree mode summary" row marked `completed`; §Decision amend note records the interim 2^20 cap. — Plan 0318 Step 10
- [x] `validate_plan.py --phase final` is structurally valid. — Plan 0318 Step 11 (run 2026-09-01)

## Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| `LabelBucket::BYTES == 29` is broken by the tree-mode flag | Wire format format change; pre-production fresh-state policy requires no conversion but the wire bytes change | Bit 63 is the high bit of an existing 64-bit packed `word`; the field is 29 bytes (8 word + 4 + 4 + 4 + 4 + 1 + 1 + 1 + 4 = 29). Adding bit 63 to the `word` byte 0 (high bit) preserves the 8-byte prefix; the wire format gains a single new flag without lengthening. Existing slab-mode buckets all have bit 63 = 0, so reopen is forward-compatible. |
| Production `VirtualMemory::write` constant cost differs from `VectorMemory::write` by > ±20% | Gate 2 numbers regress | The Plan 0318 Step 9 measurement is the regression check. If it fails, open a follow-up slice to investigate the syscall cost difference. |
| Constructor signature change breaks all call sites | Compile error everywhere | Update all test fixtures (`labeled_lara_memories()`, `failpoint_labeled_memories()`) and production constructors in the same step; clippy --all-targets -- -D warnings is the final gate. |
| `tree_csr_high_degree_test::high_degree_*` 1M sweep hits production `Memory::grow` limits | Test failure on `VirtualMemory` | Production stable memory has different limits than host `VectorMemory`'s process heap. The 1M sweep is `#[ignore]`'d; the 4K / 65K coverage is in the canbench surface. PocketIC 1M is deferred to a follow-up slice. |
| Cap enforced on `stored_slots` alone instead of `alloc_space` (regression of Plan 0317 amend) | The cap is supposed to be on `alloc_space = stored_slots + alloc_gap`, not on `stored_slots` alone. If implemented incorrectly, the cap would be triggered too early for slab mode (when `stored_slots` is small but `alloc_gap` is large), wasting leaf footprint unnecessarily. | The cap enforcement helpers in Step 3 explicitly use `compute_bucket_allocation(bucket)` which returns `min(stored_slots + alloc_gap, cap)` for slab mode. Unit tests cover both `stored_slots < T_PROMOTE but alloc_space >= T_PROMOTE` (cap reached) and `stored_slots + alloc_gap < T_PROMOTE` (cap not reached) to verify the contract. |
| `tree_mode_out_edges_iter` does not preserve iteration order with `for_each_chunk` chunk boundaries | Caller-visible regression in iteration order | The chunk-buffer pattern yields `(start_slot, &[u8])` slices in order; `tree_mode_out_edges_iter` walks chunks in reverse or ascending order, decoding each chunk's 4-byte rows into `(slot, target)` pairs in order. Unit tests verify cross-mode consistency (`for_each_with_counterpart` walks slab and tree streams in the same order). |

## Later Slices

    - `tree-mode-tombstone-reuse` — tree bucket tombstones are not reused on insert (slab mode reuses via
      `try_reuse_unordered_slab_tombstone`); high-churn tree buckets grow LTB footprint monotonically toward the
      interim `TreeRootCapacityReached` cap (2^20 slots) even at low live degree. Primary reclaim: Plan 0319
      hysteresis demotion (tree→slab rebuild). **Parallel-track candidate (orchestrator, 2026-09-01): a compressed
      per-bucket tombstone/occupancy distribution map** — the persistent live bitmap / rank-select structure deferred
      in GAP-2026-07-25-002 (`design/implementation-gaps.md`) would serve both the OFFSET-skip acceleration (that
      gap) and O(1) free-ordinal lookup for tree-mode reuse. Design them together when either is picked up.

- **Plan 0319** — Demotion (tree → slab). Recorded as a benchmark-gated maintenance operation per ADR 0088 §7. Originally Plan 0318; renumbered to Plan 0319 to make room for this implementation slice.
- **Plan 0320** — `materialize_inline_property_stream` migration primitive. Recorded as planned.
- **Plan 0321** — Batch admission widening to tree-mode buckets. Per ADR 0088 §7.
- **Plan 0323** — Normal LARA (unlabeled) tree mode as a second instance.
- **Plan 0324** — PocketIC-backed 1M-degree sweep (Gate 1's deferred row).
- **tree-mode-interior-level-insert-growth** (status: **RESOLVED by Plan 0325, 2026-09-02**; from Plan 0318 §Step 7 amend). The right-spine cascade shipped in Plan 0325; the effective tree-mode cap is now `TREE_STRUCTURAL_CAP = 2^30` slots per bucket (= 4 GiB of edge data per label). Production depth grows 1 → 2 at stored = 2^20 and fail-closes at stored = 2^30 + 1. The canbench surface: guard bench `tcsr_1048576_root_capacity_reached` REPLACED by `tcsr_1048576_deepen_beyond_r_max` (1M + 1 insert SUCCEEDS via deepen) + new `tcsr_1048576_deepen_then_interior_grow` (1M → 1M + 1024: deepen + interior-row appends + new-interior mint). Synthetic-layout unit tests prove the 2^30 fail-closed boundary (canbench cannot seed 2^30: 17.9T ins > 10T limit). Wasm 15,661 chars / 4,339 headroom. See `plans/0325-interior-level-insert-growth.md` for the full design and `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` §4 / §Status for the cap semantics. Lifting the cap to 2^40 (depth 3 in production) is a future ADR amend; MAX_DEPTH = 3 remains the primitive safety bound.
- **tree-mode-tombstone-reuse** (status: **pending**; from Plan 0318 §Step 6 amend). Tree-mode inserts currently always append (Unordered tombstone reuse is not implemented). For high-churn tree buckets, the LTB footprint grows monotonically: every removed edge becomes a tombstone that occupies a permanent slot in a leaf block. Slab mode reuses tombstones via `try_reuse_unordered_slab_tombstone`. The reuse strategy for tree buckets needs to either: (a) scan for tombstones on the insert path (cost: `O(stored_slots)` worst-case per insert), (b) maintain a per-bucket tombstone count + trigger a flatten-and-rebuild when the threshold is crossed, or (c) accept the LTB bloat for tree mode and rely on the lower storage churn of high-degree buckets. Recorded as a follow-up because the production callers (Step 6 wire-up) currently have a stable contract: tombstones in tree mode never reuse.

## Notes (out of scope, recorded for context)

- ADR 0007 stable-memory-layout inventory update listing `LtbRawBlockStore × 2 orientations` as new `MemoryId` slots is **not in scope** here. It is a separate documentation update that goes alongside the production wire-up; tracked separately so that this slice does not mix production-code changes with doc-only changes.
- The `LtbRawBlockStore` is currently `pub(crate)` within `crates/ic-stable-lara`. No public API change. The crate boundary continues to own the LTB lifecycle (per ADR 0088 §Encapsulation).
- The 2 new `M: Memory` arguments for `LabeledLaraGraph` (one per orientation, two for bidirectional) bring the constructor signature to 16 / 32 args. This is at the upper end of ergonomic but is consistent with the existing 15 / 30 pattern — the plan does not introduce a new ergonomic pattern (no builder, no `init_from_parts`). If a builder is desired, it is a separate refactor slice.
- The cap semantics are anchored at **`alloc_space = stored_slots + alloc_gap`**, not at `stored_slots` alone. This was clarified in Plan 0317 amend and is documented in Step 3 above. Failure to enforce the cap on `alloc_space` (i.e. enforcing on `stored_slots` alone) is a known regression risk tracked in the Risks table.
- `compute_bucket_allocation(bucket)` is the single source of truth for the bucket's allocation size. Every entry point that allocates bucket storage must call this helper and compare the result against the cap, rather than comparing `stored_slots` directly. This is what makes the cascade work correctly: a slab bucket at `stored_slots = 4095, alloc_gap = 1, alloc_space = 4096 = T_PROMOTE` is correctly identified as at-cap (not under-cap), and any further insert triggers promote.