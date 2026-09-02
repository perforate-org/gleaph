---
name: "Interior-level insert growth — right-spine cascade lifts the tree cap from 2^20 to 2^30 (TREE_STRUCTURAL_CAP), MAX_DEPTH=3 fail-closed preserved"
overview: "Replace the interim `TreeRootCapacityReached` fail-closed guard (Plan 0318 §Step 7 amend) with the ADR 0088 §4 right-spine insert cascade. The depth-generic infrastructure already exists and is verified: `resolve_leaf_block_id` (mixed-radix descent, physical depth, synthetic depth-2 test), `collect_leaf_block_ids`, demote Phase-5b interior release, `bucket_span_region_len` physical-depth match, `tree_mode_deepen`/`tree_mode_flatten` (level-generic, reserve/commit/publish). What is missing is ONLY the insert-side wiring: (1) when the physical root is full (`physical_root_len == R_MAX`) and deepening is permitted, call `tree_mode_deepen`, re-read the (stale) descriptor, then proceed; (2) at physical depth ≥ 2 the new leaf block_id must go into its home interior (`l / K`), NOT the root array — root grows only when a new interior is minted (`l % K == 0`); (3) the fail-closed boundary moves to the ADR cap: root full at `TREE_STRUCTURAL_CAP = 2^30` (= exact depth-2 coverage: 1024 interiors × 1024 leaves × 1024 slots) or `MAX_DEPTH` reached — production depth never exceeds 2. Two 1M canbench benches change: `tcsr_1048576_root_capacity_reached` (guard at 2^20+1) is replaced by `tcsr_1048576_deepen_beyond_r_max` (insert at 2^20+1 now SUCCEEDS via deepen) and `tcsr_1048576_deepen_then_interior_grow` (1024 inserts crossing 2^20). Existing depth-1 benches must stay 0% regression. Synthetic-layout unit tests cover the 2^30 fail-closed boundary (canbench cannot seed 2^30: 17.9T ins > 10T limit). Wasm ≤ 20,000 chars. Green-bar + validator."
todos:
  - id: "audit-depth-assumptions"
    content: "Before code, audit and record in this plan: every site that reads or writes the tree root region or derives root length, classified by (a) already depth-generic (via `tree_mode_physical_depth` / `resolve_leaf_block_id` / `collect_leaf_block_ids` / `bucket_span_region_len`) vs (b) depth-1-only (the `tail_offset == 0` append branch in `tree_mode_insert_edge` at tree_write.rs:113-175 which appends the new LEAF id to the root array and sizes the span with `derived_root_len`). Confirm: demote Phase 5b releases interior blocks; batch tree branch (batch_write.rs:2119+) behaves at depth ≥ 2 (tail_offset==0 → TreeRunExceedsTailBlock → scalar fallback → tree_mode_insert_edge cascade); remove-path tombstone rewrites resolve through `resolve_leaf_block_id`; compaction `bucket_span_region_len` is physical-depth-generic. List the two 0318 unit tests that assert the 2^20 guard (`tree_insert_fails_closed_at_root_capacity`, `production_insert_path_fails_closed_at_root_capacity`) as must-update."
    status: completed
  - id: "interior-append-helpers"
    content: "Add `resolve_interior_block_id(graph, bucket, interior_index)` (mixed-radix hop chain stopping at the interior level just above leaves — depth 2: `root[idx]`; depth 3: `root[idx/K]` then mid-level interior `[(idx%K)]`; mirror resolve_leaf_block_id's physical-depth loop). Restructure the `tail_offset == 0` branch into a depth-aware append: depth 1 keeps the existing mint-leaf + root-span-realloc + append-leaf-id code path byte-identical; depth ≥ 2 appends leaf `l = ceil(stored/B)`: if `l % K != 0` write the leaf id into interior `l/K` at row `l % K` via `ltb().write_payload_partial` (no root change; publish stored/degree); if `l % K == 0` mint leaf + interior, grow the root (existing span realloc, appending the new INTERIOR id), write the leaf id into interior row 0. LIFO rollback on every failure (mirror deepen's reserve/commit/publish). Unit tests with the synthetic-layout pattern (tree_write.rs:1853)."
    status: completed
  - id: "cascade-wiring"
    content: "Wire the cascade in `tree_mode_insert_edge`: when `tail_offset == 0` and `physical_root_len >= R_MAX`: if `depth >= MAX_DEPTH` or `next_stored > TREE_STRUCTURAL_CAP` return the typed `TreeRootCapacityReached` (cap semantics per ADR §4: 2^30 = exact depth-2 coverage); else call `tree_mode_deepen`, RE-READ the bucket descriptor (stale-descriptor hazard — deepen published a new edge_start/depth), then continue into the depth-aware append. Assert/debug-assert invariants: deepen strictly reduces physical root length; after deepen the root has room. Update the two 0318 guard tests to the synthetic depth-2-root-full 2^30 layout. Add the full unit matrix: cascade at exactly 2^20+1, interior-row append, new-interior mint, 2^30 fail-closed, demote from synthetic depth-2, batch tail-fit at depth 2, public read accessors over a synthetic depth-2 bucket."
    status: completed
  - id: "bench-semantics"
    content: "canbench surface: REPLACE `tcsr_1048576_root_capacity_reached` with `tcsr_1048576_deepen_beyond_r_max` (seed 2^20 outside bench_scope; inside: 1 insert — SUCCEEDS via deepen; assert depth == 2, physical root len == 2, stored == 2^20+1, edge readable). ADD `tcsr_1048576_deepen_then_interior_grow` (seed 2^20; inside: 1024 inserts crossing R_MAX — 1 deepen + interior-row writes + 1 new-interior mint; assert depth 2 / root_len 2 / stored 2^20+1024; record ins/edge). Re-run the touched 1M benches with plain `canbench <name>` (--persist FORBIDDEN); `tcsr_1048576_insert_grow_below_cap` and both full_scan/random_ordinal benches must be 0%-regression (depth checks are negligible). Wasm name chars ≤ 20,000 (baseline 15,536)."
    status: completed
  - id: "docs-and-validate"
    content: "ADR 0088: §4 cap semantics (2^20 interim → 2^30 effective, enforced; MAX_DEPTH = 3 remains the deepen/flatten primitive safety bound, production depth ≤ 2), §7 later-slices row closed, §Status updated. plan 0318 Later Slices: cascade shipped. design/implementation-gaps.md: `tree-mode-interior-level-insert-growth` follow-up → resolved with the design notes. plan 0324: note the guard bench was superseded by the deepen bench. validate_plan.py --phase final. Full green-bar matrix."
    status: completed
isProject: false
---

# Interior-level insert growth (right-spine cascade)

## Objective

Lift the tree-mode per-bucket cap from the interim 2^20 (Plan 0318 §Step 7 amend fail-closed guard) to the ADR-documented `TREE_STRUCTURAL_CAP = 2^30` by wiring the right-spine insert cascade: root full → deepen (depth d → d+1) → depth-aware leaf append. Production depth never exceeds 2; `MAX_DEPTH = 3` remains the deepen/flatten primitive safety bound (`TreeDepthLimitReached`).

Success signal:

- Insert at 2^20 + 1 succeeds via deepen (depth 1 → 2); 1M-scale canbench benches prove it with production `VirtualMemory`.
- Fail-closed moves to 2^30 (synthetic-layout unit proof; canbench cannot seed 2^30 — 17.9T ins > 10T limit).
- Depth-1 bench numbers 0% regression; demote / batch / compaction / reads verified at depth 2.
- Wasm ≤ 20,000 chars; green-bar; validator final PASS.

## Context

- Wave 2 slice 1 (roadmap 2026-09-02). Main HEAD: `609b96965` (Plan 0324 final).
- The interim guard (0318 §Step 7 amend, `d5657eb5f`): `tree_mode_insert_edge` returns `TreeRootCapacityReached` when the physical root length would exceed `R_MAX = 1024` at depth 1 — effective cap 2^20.
- Depth-generic infrastructure already shipped and unit-verified (0318 Step 7): `tree_mode_deepen` (level-generic reserve/commit/publish, packs the root into `EdgeInterior` blocks, publishes new `edge_start` + `tree_mode_physical_depth`), `tree_mode_flatten`, `resolve_leaf_block_id` (mixed-radix physical-depth descent, test `resolve_leaf_block_id_walks_synthetic_depth2_layout` at tree_write.rs:1853), `collect_leaf_block_ids`, demote Phase-5b interior release, compaction `bucket_span_region_len` (physical-depth match).
- **Structural vs physical depth**: `derive_depth(stored_slots)` / `derived_root_len` compute the minimal-depth shape (depth 1 covers 2^20). A deepened bucket at stored = 2^20 has structural depth 1 but PHYSICAL depth 2 with a 1-entry root. All cascade code must use `tree_mode_physical_depth()` and the physical ceil-chain formula (the same one the guard and `bucket_span_region_len` already use). `resolve_leaf_block_id` documents this exact hazard.
- **Cap semantics (ADR 0088 §4)**: `TREE_STRUCTURAL_CAP = 1 << 30` = exact depth-2 coverage (1024 interiors × 1024 leaves × 1024 slots). The cascade grows depth 1 → 2 at stored = 2^20 and fail-closes at 2^30. Lifting the cap to 2^40 (depth 3 in production) would require an ADR amend — explicitly out of scope.
- canbench limits (Plan 0324 audit): per-bench 10T ins budget; 2^20 seeding = 17.9G ins (558× headroom ✓); 2^30 seeding = 17.9T — over budget ⇒ the 2^30 fail-closed boundary is unit-proven via synthetic layout only.

## Steps

### Step 1 — Audit (todo `audit-depth-assumptions`)

Record the depth-classification table (generic vs depth-1-only) with file:line refs. Known going in (verify, don't re-derive):

| Site | Status | file:line |
|---|---|---|
| `resolve_leaf_block_id` | depth-generic ✓ (physical depth, mixed-radix) | tree_write.rs:516 |
| `collect_leaf_block_ids` | depth-generic via resolver | tree_write.rs:586 |
| `tree_mode_deepen` | level-generic (reserve/commit/publish, all depth-generic) | tree_write.rs:632 |
| `tree_mode_flatten` | depth-2 → depth-1, structural depth match | tree_write.rs:830 |
| demote Phase 5b interior release | releases interior blocks ✓ | tree_write.rs:1025-1058 |
| compaction `bucket_span_region_len` | physical-depth match ✓ | compact.rs:234 |
| remove-path tombstone rewrite | via `resolve_leaf_block_id` ✓ | tree_write.rs:323-360 |
| batch tree branch `commit_with_location_mode` | resolver-based; `tail_offset==0` → `TreeRunExceedsTailBlock` → scalar fallback → `tree_mode_insert_edge` cascade ✓ | batch_write.rs:2119, 2165 |
| **`tree_mode_insert_edge` `tail_offset == 0` branch** | **depth-1-ONLY: appends the new LEAF id to the root array; span sized via `derived_root_len` (which under-walks post-deepen physical-depth); `physical_root_len >= R_MAX` returns `TreeRootCapacityReached` at 2^20 (interim guard)** | tree_write.rs:113-175 |
| 0318 guard tests `tree_insert_fails_closed_at_root_capacity`, `production_insert_path_fails_closed_at_root_capacity` | assert 2^20 guard; both **must be rewritten** to synthetic depth-2 root-full layout exercising the 2^30 fail-closed boundary | tree_write.rs:2121, 2193 |

**Structural vs physical depth hazard** (called out in
`resolve_leaf_block_id` doc comment at tree_write.rs:491-507):
- `derive_depth(stored_slots)` / `derived_root_len(stored_slots)` compute the minimal-depth shape; they assume the bucket is **not** manually deepened.
- A deepened bucket at `stored = 1,048,576` has `derive_depth = 1` but `tree_mode_physical_depth = 2` with a 1-entry root.
- The `tail_offset == 0` branch currently uses `derived_root_len(stored_slots)` to size the new root region — this would under-size after deepen (post-deepen root has 1 entry, but `derived_root_len(2^20) = 1024`). Must use **physical** ceil-chain (the same one the guard at L121-137 and `bucket_span_region_len` use).

### Step 2 — Interior append helpers + depth-aware tail branch (todo `interior-append-helpers`)

New `resolve_interior_block_id(graph, bucket, interior_index)` — the resolver's hop chain truncated at the interior level (mirror the mixed-radix loop, one level short). Depth-aware append in `tree_mode_insert_edge`:

- depth 1 (root not full): existing path UNCHANGED (mint leaf → span realloc → append leaf id → publish).
- depth ≥ 2, leaf index `l = ceil(stored/B)`:
  - `l % K != 0`: leaf id → interior `l/K` row `l % K` (`write_payload_partial`); descriptor publish (stored/degree; edge_start unchanged).
  - `l % K == 0`: mint leaf + interior (LIFO rollback), root grows by the span-realloc path appending the new INTERIOR id, leaf id → interior row 0, publish.
- Root-region sizing at depth ≥ 2 uses the PHYSICAL ceil-chain (never `derived_root_len` — structural mismatch after deepen).

### Step 3 — Cascade wiring (todo `cascade-wiring`)

Guard becomes: root full ⇒ (`depth >= MAX_DEPTH` || `next_stored > TREE_STRUCTURAL_CAP`) → `TreeRootCapacityReached`; else `tree_mode_deepen(...)` → **re-read the descriptor** (deepen published new edge_start/depth — the caller's `bucket` copy is stale) → continue into the append (post-deepen root has room: ceil(R_MAX/K) = 1 interior).

Unit test matrix (synthetic layout, host `VectorMemory`, cheap): cascade at 2^20+1; interior-row append (no root change); new-interior mint (root grow); 2^30 fail-closed; demote from depth-2; batch tail-fit at depth-2; public read accessors over depth-2.

### Step 4 — Bench semantics (todo `bench-semantics`)

- `tcsr_1048576_root_capacity_reached` → REPLACED by `tcsr_1048576_deepen_beyond_r_max` (2^20 + 1 insert SUCCEEDS; assert depth 2 / root 2 / readable).
- NEW `tcsr_1048576_deepen_then_interior_grow` (2^20 → 2^20 + 1024: deepen + interior-row writes + 1 root grow).
- Plain `canbench <name>` runs only (`--persist` forbidden); yml committed with the new/updated rows.
- Depth-1 benches (`tcsr_1048576_insert_grow_below_cap`, full_scan, random_ordinal, 131K set): 0% regression target (the cascade only touches the `tail_offset == 0` root-full path).

### Step 5 — Docs + validation (todo `docs-update`)

ADR 0088 §4/§7/§Status (2^20 interim → 2^30 effective; MAX_DEPTH = 3 = primitive-level safety), plan 0318 Later Slices, gaps entry → resolved, plan 0324 superseded-guard note. `validate_plan.py --phase final`.

## Completion Criteria

- [x] Insert at 2^20 + 1 succeeds via deepen on the production path (canbench-verified, deterministic ×3).
- [x] `tcsr_1048576_deepen_then_interior_grow` recorded in `canbench_results.yml`; depth-1 benches 0% regression.
- [x] Fail-closed proven at 2^30 via synthetic depth-2 root-full unit test (typed `TreeRootCapacityReached`; bucket unchanged).
- [x] Demote / batch / read accessors verified at depth ≥ 2 (unit).
- [x] ADR 0088 §4/§7/§Status + gaps + plan 0318/0324 cross-references updated.
- [x] Green-bar: plain check, 621 baseline, full canbench 0 failed, clippy -D warnings, fmt, wasm ≤ 20,000 chars.
- [x] `validate_plan.py --phase final` PASS.

## Scope

- IN: insert-side right-spine cascade (deepen wiring + depth-aware leaf append), guard relocation to the 2^30 structural boundary, `resolve_interior_block_id` helper, synthetic depth-2 unit matrix, 1M canbench bench semantics update, ADR/plan/gaps docs.
- OUT: LPB-in-tree (next slice), depth-3 production growth (ADR amend candidate), tombstone-reuse, batch root-minting, read-path restructuring (already depth-generic).

## Expected Change Surface

| File | Change |
|---|---|
| `crates/ic-stable-lara/src/labeled/graph/tree_write.rs` | Depth-aware tail-append in `tree_mode_insert_edge`; new `resolve_interior_block_id`; cascade wiring + relocated guard; updated 0318 guard tests; new synthetic depth-2 unit matrix |
| `crates/ic-stable-lara/src/labeled/bench.rs` | Guard bench → deepen bench; new `tcsr_1048576_deepen_then_interior_grow` |
| `crates/ic-stable-lara/canbench_results.yml` | Replaced/updated 1M rows (plain single-bench runs only) |
| `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` | §4 cap semantics, §7 later-slices, §Status |
| `design/implementation-gaps.md` | `tree-mode-interior-level-insert-growth` → resolved |
| `plans/0318-tree-csr-implementation.md`, `plans/0324-pocketic-1m-sweep.md` | Cross-reference notes |
| `plans/0325-interior-level-insert-growth.md` | This file |

## Validation

- `cargo check -p ic-stable-lara` (plain, production cfg)
- `cargo test -p ic-stable-lara --lib --no-default-features` (621 baseline; delta = new tests − updated guard tests, record exact counts)
- `cargo test -p ic-stable-lara --lib --features canbench` (0 failed)
- `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings`
- `cargo fmt --check -p ic-stable-lara`
- wasm exported-name chars ≤ 20,000 (canonical python normalization; baseline 15,536)
- `~/.cargo/bin/canbench <name>` per new/updated bench (plain runs; `--persist` forbidden)
- `validate_plan.py plans/0325-interior-level-insert-growth.md --phase final`

## Later Slices

- LPB-in-tree (0320 primitive reuse; removes the w > 0 promotion carve-out).
- Crate slimming (unlabeled graph removal — user decision).
- Tree read polish (visit_edges_with_inline_property tree branch; bench_t_v_window +2.10%; dense window contract).
- Tombstone-reuse in LTB blocks (shared occupancy map with GAP-2026-07-25-002).
- ADR amend candidate: lift the cap to 2^40 (depth 3 in production) — only if a real workload demands > 2^30 slots/bucket.