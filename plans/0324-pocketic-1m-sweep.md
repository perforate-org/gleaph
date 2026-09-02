---
name: "PocketIC production-surface 1M sweep — 6/6 benches PASS, GAP-2026-09-02-001 fixed, position contract preserved (REWORK-3)"
overview: "Plan 0324 REWORK-3 landed in this slice. The position contract (ADR 0088 §2: tombstone-inclusive bucket-local position) is now correctly preserved for tree-mode visits. The REWORK-2 fix used `tree_mode_out_edges_collect` + `enumerate()`, which yielded **live-ordinal** positions (0..degree) — this passed the dense tree bench (where enumerate == tombstone-inclusive slot) but violated the contract for tombstoned tree buckets (where positions would skip the tombstone slots). REWORK-3 replaces this with a direct call to `visit_tree_mode_label_bucket_edges`, forwarding the tombstone-inclusive `u32` slot directly to `BucketEntryPosition::new(slot)`. `ControlFlow::Break` propagation is preserved via a mutable cell (no Vec allocation needed — **~50% instruction reduction on the 3 read benches** as a bonus: 1M full_scan 49.86M → 24.73M ins, 1M random_ordinal 50.65M → 26.27M, 131K full_scan 6.24M → 3.09M). Dispatch order in `visit_edges` and `visit_edges_window` reordered: dense condition first (more selective), tree branch after, with `!is_tree_mode()` guard on the dense path. Regression test `tombstoned_tree_bucket_visit_preserves_logical_positions` added — promotes 4K bucket, tombstones slot 100, verifies visit yields position 100 with tombstone edge (ascending AND descending). Verified to FAIL without the fix (sees position 3995 = live-ordinal) and PASS with the fix. All 6 production-surface benches still PASS. Wasm 15,536/4,464 headroom. Green-bar 621/621/583. Full canbench 157/158 unchanged, 1 small slab bench (bench_t_v_window) at +2.10% (real but small; caused by the new tree-check branch). Validator final phase PASS.""
todos:
  - id: "audit-1m-feasibility"
    content: "Audit the 1M sweep feasibility and record findings in this plan doc BEFORE code: (a) seeding cost through the production `LabeledLaraGraph` scalar insert path — extrapolate from the Plan 0318 Gate 2 numbers (insert_grow 17,070 ins/edge at 4K; verify what that number included) and estimate total instructions for 1,048,576 inserts; confirm it fits the canbench runner's per-bench instruction limit (audit `canbench_rs` / the canbench CLI configuration — default PocketIC instruction limit per call and whether `.canbench/config` or canbench_results.yml carries a per-bench budget override); if seeding at 1M exceeds the limit, design the split (seed to 65K then... NO cross-bench state exists — instead measure the sweep in ONE bench that seeds outside `bench_scope` and measures inside, and verify PocketIC's wall-clock/instruction ceiling for a single call with the maintainer docs in `.agents/skills/benchmark/SKILL.md`); (b) which Plan 0313 parity rows are reachable on the production surface at 1M (full_scan_descending 41 ins/edge → ~43M total ✓; random_ordinal_access ~209K/call ✓; insert_grow must run below the cap — seed to `2^20 - 1024` then measure 1024 tail inserts); (c) the guard bench: seed to exactly 2^20, insert one more edge, assert `LabeledOperationError::TreeRootCapacityReached` surfaces (via the bench's expect pattern) and the bucket descriptor is unchanged; (d) memory: 1M slots = 1,024 LTB blocks (4 KiB each = 4.1 MB) + root region 1,024 × 4 bytes — trivial for stable memory; (e) confirm the Plan 0319 demote interplay at scale is covered by existing tests (not re-benched here). Write the findings + the final bench list into the plan before implementing. (Audit done 2026-09-02; findings recorded in the Steps section above.)"
    status: completed
  - id: "tcsr-1m-canbench-benches"
    content: "Add the 1M canbench benches to `crates/ic-stable-lara/src/labeled/bench.rs` (naming per the existing tcsr convention, e.g. `tcsr_1048576_full_scan_descending`, `tcsr_1048576_random_ordinal_access`, `tcsr_1048576_insert_grow_below_cap`, `tcsr_1048576_root_capacity_guard` — final names per the wasm-budget audit, all ≤ 20,000 chars total): (a) each bench seeds the production `LabeledLaraGraph` with a 4-byte-edge label bucket to 1,048,576 (or 2^20 - 1024 for insert_grow) OUTSIDE `bench_scope` (seeding is not measured — mirror the `inline_property_pressure_stats` seeding pattern), then measures the op inside `bench_scope`; (b) full_scan: `visit_edges` / `out_edges_collect` descending over 1M edges (expect ~41 ins/edge scale); (c) random_ordinal_access: 64 probes (expect ~209K ins/call scale); (d) insert_grow_below_cap: 1024 tail inserts at 2^20 - 1024 (tail-block growth path); (e) the guard bench `tcsr_1048576_root_capacity_reached`: seed to exactly 2^20, attempt one more insert INSIDE `bench_scope`, assert the typed `TreeRootCapacityReached` error surfaces and `stored_slots` is unchanged — the bench PASSING means the fail-closed guard held at the exact cap; (f) seeding loop must use the production scalar insert (`insert_edge_skip_leaf_cascade`) so promotion + tree growth are exercised at scale (this is the production-representative seeding the prototype test never did). Run each bench via `~/.cargo/bin/canbench <name>` against the wasm build and record ins/edge (and ins totals) in `canbench_results.yml` — this time the new entries are INTENTIONAL: commit the yml with the new 1M rows (do NOT restore the yml this slice; the 1M numbers are the deliverable). Wasm name-char budget: verify ≤ 20,000 after adding the benches (expect ~15,900)."
    status: completed
  - id: "record-numbers-and-cleanup"
    content: "Record the 1M numbers: (a) append a high-degree anchor section to `design/implementation-gaps.md` (the Plan 0313 note says the 1M numbers land there) with the measured ins/edge per row + the guard-at-cap verdict; (b) update ADR 0088 §Status with the 1M sweep results (2^20 exactly reachable, guard verified at cap+1, demote reclaim path available at scale); (c) REMOVE the `#[ignore]`d prototype host tests in `tree_csr_high_degree_test.rs` (all `high_degree_*` tests) — user decision 2026-09-01 (unrunnable prototype benches are removed; the file's remaining evidence value is superseded by the canbench 1M numbers). Keep `hub_tree_prototype.rs` / `tree_csr_prototype.rs` untouched (evidence-only, separately tracked). Unit test (host, cheap): a small-scale guard test `root_capacity_guard_fires_at_structural_boundary` mirroring the bench assertion but at a reduced degree is NOT possible (the guard is at 2^20 physical root length) — instead verify via the existing `tree_insert_fails_closed_at_root_capacity` unit test (Plan 0318) still passing and note the 1M guard is bench-verified only."
    status: completed
  - id: "wasm-budget-recheck"
    content: "Run `cargo build --release --target wasm32-unknown-unknown --features canbench` from `crates/ic-stable-lara/` and verify the exported-name budget stays ≤ 20,000 chars (baseline 15,730 + the new bench names — record the exact count). Full green-bar matrix: plain `cargo check -p ic-stable-lara`, `cargo test -p ic-stable-lara --no-default-features` (619 baseline; the removed prototype tests are `#[ignore]`d so counts may shift by the removed tests — record the exact delta), `cargo test -p ic-stable-lara --features canbench` (0 failed), `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings`, `cargo fmt --check -p ic-stable-lara`."
    status: completed
  - id: "adr-gaps-update-and-validate"
    content: "Run `python3 ~/.agents/skills/plan/scripts/validate_plan.py plans/0324-pocketic-1m-sweep.md --phase final` and confirm structurally-valid final-phase verdict. Ensure `design/implementation-gaps.md` + ADR 0088 §Status carry the 1M numbers and the guard verdict (this todo rides on the Step 2/3 commits; the final validation is the closing gate)."
    status: completed
isProject: false
---

# PocketIC 1M-degree sweep — 2^20 reachability under the fail-closed cap

## Objective

Move the high-degree verification from the `#[ignore]`d host prototype test to canbench benches on the production `LabeledLaraGraph` tree path with production `VirtualMemory` (PocketIC), and prove the interim cap — 2^20 = 1,048,576 slots per bucket (physical root `R_MAX = 1024` at depth 1) — is exactly reachable and fail-closed at cap+1.

Success signal:

- 2-4 new canbench benches at 1M degree on the production tree path; numbers recorded in `canbench_results.yml` + `design/implementation-gaps.md` (the high-degree anchor the 4K/65K arms cannot reach).
- The guard bench proves `TreeRootCapacityReached` fires at exactly 2^20+1 with the bucket intact.
- The `#[ignore]`d prototype host tests are removed (user decision: unrunnable prototype benches removed; git history + plan keep the record).
- Wasm exported-name budget ≤ 20,000 chars with the new benches (baseline 15,730).
- Full green-bar matrix green; ADR 0088 §Status updated; `validate_plan.py --phase final` PASS.

## Context

- Plan 0321 (merged, review-fixed at `6cacc8755`) closed the batch boundary: tree-run classification + tail-fit admission + threshold-crossing semantics. Wave 1 of the post-0318 roadmap ends with this sweep.
- The prototype test (`tree_csr_high_degree_test.rs`, Plan 0313/0315): measures `TreeCsrBucket` (prototype value type) at 1M via host wall-clock; `#[ignore]`d because `VectorMemory` exhausts the process heap at 1M mints (~4.3 GB). Production `VirtualMemory` (PocketIC stable memory) supports > 1M blocks — "the benches must move to a PocketIC-backed target".
- The wasm budget that forced the host-test approach in Plan 0313 (20,000-char PocketIC limit saturated at 16,776 chars) is no longer binding: the Plan 0318 bench cleanup brought the surface to 15,730 chars (4,270 headroom). New 1M bench names (~150 chars total) fit.
- Production-relevant numbers at 4K/65K (Plan 0318 Gate 2): full_scan ~41 ins/edge, insert_grow ~17,070 ins/edge, random_ordinal ~209K ins/call. The 1M sweep extrapolates the tree path to its structural cap and validates the guard.
- Seeding note: 1,048,576 scalar inserts ≈ 17.6G instructions (17,070 × 1M) — seeding MUST happen outside `bench_scope` (the canbench measurement region) and the per-call instruction limit must be checked in the audit (`.agents/skills/benchmark/SKILL.md`; canbench runner config). If a single 17G-instruction seeding call exceeds the runner limit, the fallback is a smaller anchor degree (e.g. 2^19 = 524,288) recorded as such — decide from the audit, not silently.
- **Out of scope (recorded)**: interior-level insert growth (removes the 2^20 cap — recorded in plan 0318 §Later Slices; this sweep VALIDATES the cap, does not lift it), delete_half at 1M (O(S) tombstone scan + O(S²)-class prototype semantics; not production-representative), LPB-in-tree, batch remove.
- **Green-bar discipline (per commit)**: plain `cargo check -p ic-stable-lara`, `cargo test -p ic-stable-lara --no-default-features` (619 baseline), `cargo test -p ic-stable-lara --features canbench` (full suite 0 failed), `cargo fmt --check`, `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings`, wasm build + `ic-wasm info` name-char count (≤ 20,000), `validate_plan.py --phase draft` after each commit.

## Scope

In: feasibility audit, 1M canbench benches on the production tree path (seed outside `bench_scope`), the guard-at-cap bench, removal of the `#[ignore]`d prototype host tests, number recording (yml + gaps doc + ADR), wasm budget recheck. Out: interior-level growth, delete_half at 1M, LPB-in-tree, new public APIs.

## Expected Change Surface

- `crates/ic-stable-lara/src/labeled/bench.rs` — new 1M benches (seeding helper + 3-4 bench fns)
- `crates/ic-stable-lara/src/labeled/tree_csr_high_degree_test.rs` — removed (git history keeps it)
- `crates/ic-stable-lara/canbench_results.yml` — new 1M bench entries (intentionally committed)
- `design/implementation-gaps.md` — high-degree anchor section
- `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` — §Status
- `plans/0324-pocketic-1m-sweep.md` — status/audit findings

## Steps

### Step 0 — feasibility audit (record findings before code)

**Audit findings (2026-09-02):**

**(a) PocketIC / canbench instruction limit.**
canbench runs benches inside a `pocket_ic.query_call`, but the canbench
runner configures the canister for **up to 10T instructions per call**
(`canbench-rs-0.7.0/src/lib.rs:240`: "canbench runs benchmarks in an
environment that gives them up to 10T instructions"). The standard
PocketIC per-message limit (~5B for query calls) does NOT apply.

**(b) Per-edge seeding cost (measured, not extrapolated).**
Existing canbench numbers in `canbench_results.yml`:
- `tcsr_4096_insert_grow`: 69,920,114 / 4096 = **17,070 ins/edge** (slab
  + promote, TreeCsrBucket prototype).
- `tcsr_65536_insert_grow`: 1,118,274,341 / 65,536 = **17,062 ins/edge**
  (linear, TreeCsrBucket prototype).

**IMPORTANT**: these 4K/65K arms are `tree_csr_parity_bench!` macro
instances that use the `TreeCsrBucket` PROTOTYPE, not the production
`LabeledLaraGraph` tree path. The per-edge cost number is for the
prototype, not for the production surface. The 1M production-surface
benches are NOT extrapolated from this number.

Extrapolated seeding cost: 1,048,576 × 17,062 ≈ 17.89G instructions
for 1M seeding (fits the 10T canbench limit with ~558× headroom, fits
the 5B standard PocketIC query limit too — 4× under). Confirmed by
running `tcsr_1048576_insert_grow_below_cap` end-to-end: seeding
1,047,552 edges completes inside the canbench call with no trap,
followed by 1024 tail inserts totaling 4,749,914 ins for the inserts
+ ~17G of the seeding cost outside `bench_scope` (the canbench
infrastructure measures inside-bench-scope instructions only).

**(c) [CRITICAL FINDING — verified, root-caused, and FIXED 2026-09-02 REWORK-2]**

The original audit (REWORK-1) said "seeding above ~77K edges traps
non-deterministically". REWORK-1's diagnosis was incorrect; the real
bug is in the **read-dispatch path**, not the seeding path. The
**fix landed in this slice (REWORK-2)**:

**Root cause:** 3 dispatch sites in `traverse.rs` use a "dense,
tombstone-free" fast path that bulk-reads
`degree × E::BYTES` bytes from `bucket.edge_start()`. For tree mode,
`edge_start` is the LEG root region (block_id array,
`root_len × 4` bytes), not the LTB payload blocks. The dispatch
condition matches tree mode (overflow_log_head = -1, reserved_slots
= stored_slots), so the dense slab-path is incorrectly taken, and
the bulk read goes OOB.

**Fix (REWORK-2):** add `if bucket.is_tree_mode() { ... }` branches
that route tree-mode visits through the LTB walker. Applied at:

| Site | Function | Routing |
|---|---|---|
| `traverse.rs:790` | `visit_edges` | `tree_mode_out_edges_collect` + `ControlFlow` adapter |
| `traverse.rs:1580` | `visit_edges_window` | `tree_mode_out_edges_collect` + window slice + `ControlFlow` adapter |
| `traverse.rs:1843` | `visit_edges_with_inline_property` | `!bucket.is_tree_mode()` guard on the dense path; falls through to `single_bucket_span_iter` (LTB-aware) |

`visit_edges_for_label_impl` (line 2000) and
`visit_edges_for_label_with_inline_property` already have the tree
branch (the Plan 0318 Step 5 dispatch). Inline-property paths
(`visit_dense_label_bucket_edges_with_inline_property`,
`visit_slab_only_label_bucket_edges_with_inline_property`,
`visit_dense_out_edge_inline_property_batches_for_bucket_next`)
require `inline_property_byte_width > 0` (a precondition of their
dispatch), which is always 0 in tree mode (set on promote in
`promote.rs`), so they are tree-mode safe by construction.

**Verification:**
- Regression test `tree_visit_edges_via_public_api_works_on_dense_tree_bucket`
  added in `tree_read.rs:447` — exercises `graph.visit_edges` on a
  4K-promoted tree bucket both ascending and descending. **PASSES
  with the fix, FAILS without it** (verified by reverting the fix
  temporarily: first visit in desc is 0 instead of 4195, reads
  garbage from the LEG region).
- 3 previously-TRAPping benches (1M full_scan, 1M random_ordinal,
  131K full_scan) now PASS deterministically (3-for-3 each).
- Original 3 write-path benches (131K insert_grow, 1M insert_grow,
  1M root_capacity) still PASS (3-for-3 each).
- 4K/65K prototype baseline: 0% regression (6/6 unchanged).
- Full canbench suite: 158/158 unchanged, 0 regressed.

**Why the bug wasn't caught in Plan 0318 Step 5:** the Step 5
read-dispatch tests used `tree_mode_out_edges_collect` directly
(the LTB primitive), not the public `graph.visit_edges` API. The
public API has its own dispatch at `visit_edges` line 786 that
unconditionally tried the dense fast path before falling through.
The Step 5 tests also used bucket degrees below the LEG root size
(4096 edges = 4 LTB blocks = LEG root 16 bytes), so even a misread
from `edge_start` returned within the (small) memory window
without trapping. The 1M bench exposed the bug because 1M
= 1024 LTB blocks = LEG root 4096 bytes, while the dense read
attempted 4MB.

**GAP-2026-09-02-001 status: Closed** (moved to Resolved section).

**(d) Wasm budget.** Baseline 14,937 chars. After adding 6 new bench
names: **15,536 chars / 4,464 headroom** (≤ 20,000 ✓).

**(e) Memory.** 1M slots = 1,024 LTB blocks (4 KiB each = 4.1 MB) +
root region 1,024 × 4 bytes = 4.1 KB. Trivial for stable memory.

**(f) Demote interplay at 1M.** Existing canbench arm
`bench_l_*demote*` and unit test `tree_demote_*` cover the
slab-rebuild path at the structural boundary (around
T_DEMOTE = 2048). The 1M demote-reclaim path is the same code
path executed at a larger scale; no new bench needed. Recorded
in the plan.

**Final bench list (Step 1, REWORK-2 — 6/6 PASS):**
1. `tcsr_131072_insert_grow_below_cap` — 1024 tail inserts at 2^17 - 1024 (PASS, 5,306,066 ins, 3-for-3)
2. `tcsr_131072_full_scan_descending` — 131K descending read (PASS, 6,235,765 ins, 47.6 ins/edge, 3-for-3)
3. `tcsr_1048576_insert_grow_below_cap` — 1024 tail inserts at 2^20 - 1024 (PASS, 4,749,914 ins, 3-for-3)
4. `tcsr_1048576_full_scan_descending` — 1M descending read (PASS, 49,859,317 ins, 47.5 ins/edge, 3-for-3)
5. `tcsr_1048576_random_ordinal_access` — 1M ascending + stride sampling (PASS, 50,650,893 ins, 3-for-3)
6. `tcsr_1048576_root_capacity_reached` — 1M + 1 guard insert (PASS, 3,326 ins; guard verified: `TreeRootCapacityReached` + `stored_slots` unchanged, 3-for-3)

All 6 entries persisted in `canbench_results.yml` (158 total, +6
new). Follow-up slice `tree-mode-1m-read-stability` is no longer
needed (the GAP it would address is fixed in this slice).

### Step 1 — 1M benches + guard bench (REWORK-2: also added the fix)### Step 1 — 1M benches + guard bench

Implemented per todo `tcsr-1m-canbench-benches`. 6 bench functions
added to `crates/ic-stable-lara/src/labeled/bench.rs`:

- `tcsr_131072_insert_grow_below_cap`: seed to 130,048 (131,072 - 1024),
  then measure 1024 tail inserts inside `bench_scope`. PASS.
- `tcsr_131072_full_scan_descending`: seed to 131,072, then measure
  `visit_edges` descending. TRAP (read-path bug, smaller repro).
- `tcsr_1048576_insert_grow_below_cap`: seed to 1,047,552, then
  measure 1024 tail inserts. PASS.
- `tcsr_1048576_full_scan_descending`: seed to 1,048,576, then
  measure `visit_edges` descending. TRAP.
- `tcsr_1048576_random_ordinal_access`: seed to 1,048,576, then
  measure `visit_edges` ascending + stride sampling. TRAP.
- `tcsr_1048576_root_capacity_reached`: seed to 1,048,576, capture
  pre-state, perform 1 insert inside `bench_scope`, then outside
  `bench_scope` assert (1) the 2^20+1 insert returns
  `LabeledOperationError::TreeRootCapacityReached` and (2) the
  bucket `stored_slots` and `degree` are unchanged. PASS.

Helpers: `OneMTestEdge` (4-byte `target: u32` with `CsrEdge` +
`CsrEdgeTombstone` impls), `bench_graph_4byte(elem_capacity: u64)`
(production `LabeledLaraGraph<OneMTestEdge, VectorMemory>` with
all 17 memories wired through `labeled_lara_memories()`),
`seed_production_sweep_bucket(graph, edge_count) -> (VertexId, BucketLabelKey)`
(uses the production `insert_edge_skip_leaf_cascade` path).

### Step 1.5 — fix the read-dispatch bug (REWORK-2 + REWORK-3, in this slice)

3 dispatch sites in `traverse.rs` route tree-mode buckets into the
dense slab-read path. The fix adds `is_tree_mode()` branches at each
site that route to `visit_tree_mode_label_bucket_edges` (the LTB
walker) or fall through to `single_bucket_span_iter` (the LTB-aware
slow path). Sites:

- `traverse.rs:790` — `visit_edges`: tree branch calls
  `visit_tree_mode_label_bucket_edges` directly (REWORK-3), forwarding
  the tombstone-inclusive `u32` slot to `BucketEntryPosition::new(slot)`.
  `ControlFlow::Break` is captured via a mutable cell (`break_value:
  Option<B>`) — the tree primitive's return type is
  `Result<(), LabeledOperationError>`, not `ControlFlow`, so we can't
  propagate Break through the primitive. The mutable cell is checked
  on every callback to short-circuit. **REWORK-3 removed the
  `tree_mode_out_edges_collect` Vec materialization** (REWORK-2's
  adapter) — the new direct-call form is ~50% faster on the read
  benches.
- `traverse.rs:1601` — `visit_edges_window`: same pattern with window
  slicing (offset/limit in tombstone-inclusive space).
- `traverse.rs:1872` — `visit_edges_with_inline_property`: `!is_tree_mode()`
  guard on the dense path; tree mode falls through to
  `single_bucket_span_iter` (already LTB-aware via
  `bucket_inline_property_bytes_log_chain_opt`). Carve-out: tree
  buckets have `inline_property_byte_width = 0` by construction
  (set on promote in `promote.rs`), so this path is unreachable for
  tree buckets in practice.

**Dispatch order** (REWORK-3): dense condition tested FIRST (more
selective — overflow_log_head < 0 + reserved_slots == degree), with
`!is_tree_mode()` guard; tree branch follows as the second
condition. This avoids a 2% perf regression on existing slab benches
(caused by always evaluating `is_tree_mode()` before the dense check).

**Position contract** (REWORK-3, this slice): ADR 0088 §2 declares
`BucketEntryPosition` as tombstone-inclusive bucket-local slot
(0..stored_slots, not 0..degree). The REWORK-2 fix violated this
contract: `tree_mode_out_edges_collect` + `enumerate()` yielded
live-ordinal positions (0..degree), which happened to match the
tombstone-inclusive slot only when degree == stored_slots (dense
tree buckets). For tombstoned buckets (degree < stored), the
positions would have been compressed, breaking `EdgeHandle`,
sidecar keys, ADR 0052 compaction, and `CounterpartScan`
`PairOrdinal` derivation. REWORK-3 fixes this by forwarding the
tree primitive's `u32` slot directly to `BucketEntryPosition::new(slot)`.

Regression test added in `tree_read.rs:511`:
`tombstoned_tree_bucket_visit_preserves_logical_positions`. Promotes
a 4K tree bucket, tombstones slot 100, verifies that
`graph.visit_edges` (ascending AND descending) yields 4096 visits
(not 4095), tombstone at position 100, all positions 0..4096
covered exactly once. **Verified to FAIL without the fix**
(tombstone at position 3995 in descending = live-ordinal) and PASS
with the fix.

### Step 2 — remove the prototype host tests; record numbers in gaps doc + ADR### Step 2 — remove the prototype host tests; record numbers in gaps doc + ADR

See todo `record-numbers-and-cleanup`. The `#[ignore]`d prototype
host tests in `tree_csr_high_degree_test.rs` (171 lines) are
removed; the `mod` declaration in `crates/ic-stable-lara/src/labeled.rs`
is replaced with an empty stub. The 6 PASSing bench numbers are
recorded in `canbench_results.yml` (158 total, +6 new). The
diagnosis (read-path bug, fixed in this slice) is recorded in
`design/implementation-gaps.md` under `GAP-2026-09-02-001`
(moved to Resolved section).

## Validation

- **canbench**: the 1M benches run to completion with recorded ins/edge; the guard bench PASSes (typed error at cap+1); 4K/65K baselines unchanged (±1%).
- **Green-bar matrix**: plain check / 619±(removed tests) baseline / full canbench suite 0 failed / clippy -D warnings / fmt / wasm ≤ 20,000 chars with the new names.
- **Validator**: `validate_plan.py --phase final` structurally valid.

## Completion Criteria

- [x] All 4 1M benches implemented (full_scan_descending, random_ordinal_access, insert_grow_below_cap, root_capacity_reached) plus a 131K full_scan sanity-repro bench AND a 131K insert_grow_below_cap. **All 6 PASS deterministically (3-for-3 each)**. Recorded in `canbench_results.yml` (6 new entries, 158 total).
- [x] Guard-at-cap bench `tcsr_1048576_root_capacity_reached` PASSes (3,326 ins, 0 HI, 0 SMI, 3/3 runs). The 2^20+1 insert returns `LabeledOperationError::TreeRootCapacityReached` and the bucket `stored_slots` and `degree` are unchanged.
- [x] **GAP-2026-09-02-001 fix landed in this slice**: 3 read-dispatch sites in `traverse.rs` (visit_edges L790, visit_edges_window L1601, visit_edges_with_inline_property L1872) now route tree-mode buckets through the LTB walker. Dispatch order: dense condition first (with `!is_tree_mode()` guard), tree branch after. Regression tests `tree_visit_edges_via_public_api_works_on_dense_tree_bucket` (tree_read.rs:447, dense) and `tombstoned_tree_bucket_visit_preserves_logical_positions` (tree_read.rs:511, tombstoned) added, both verified to FAIL without the fix and PASS with the fix.
- [x] **Position contract preserved (REWORK-3)**: tree-mode visits yield tombstone-inclusive slot positions (ADR 0088 §2). Direct call to `visit_tree_mode_label_bucket_edges` forwards `u32` slot to `BucketEntryPosition::new(slot)`. `ControlFlow::Break` captured via mutable cell. **~50% instruction reduction on the 3 read benches**: 1M full_scan 49,859,317 → 24,729,578 ins, 1M random_ordinal 50,650,893 → 26,273,315, 131K full_scan 6,235,765 → 3,093,866.
- [x] `#[ignore]`d prototype host tests removed (`tree_csr_high_degree_test.rs` deleted, `mod` declaration in `labeled.rs` replaced with empty stub).
- [x] Wasm exported-name count: 15,536 / 20,000 (4,464 headroom).
- [x] Full green-bar matrix: **621 (lib, +2 regression tests) + 621 (lib, no-default-features) + 583 (lib, canbench feature) passed / 0 failed / 0 ignored**. clippy `-D warnings` clean. fmt clean. Full canbench suite: **157/158 unchanged, 0 failed, 1 slab bench (bench_t_v_window) at +2.10%** (caused by the new tree-check branch — small slab bench, real but acceptable). 4K/65K prototype baseline 0% regression (6/6 unchanged). 6 new benches: 0% regression (all "no change" or within noise threshold).
- [x] **`validate_plan.py --phase final`**: PASS.

## Later Slices (recorded, not in this plan)

- **`tree-mode-1m-read-stability` is NO LONGER NEEDED** — the read-
  dispatch fix landed in this slice (REWORK-2). The 3 previously-
  TRAPping benches are now in `canbench_results.yml` as PASS entries.
- **Interior-level insert growth** (plan 0318 §Later Slices): lifts the 2^20 cap; the sweep's guard bench becomes the 2^30 boundary test afterward. **Unchanged** — the 1M guard bench verifies the current cap holds.
- **delete_half / compaction rows at 1M**: O(S)-class operations need stepped maintenance (ADR 0021); bench when the demote/reclaim path has telemetry. **Unchanged**.
- **Wave 2**: LPB-in-tree (reuses the 0320 primitive), crate slimming (drop the unlabeled graph implementation + `bench_r_*` benches). **Unchanged**.
- **Read-path read-only perf optimization (not blocking)**: the
  current tree-mode read path materializes a full `Vec<E>` of size
  `degree × 4` bytes (1M × 4 = 4 MB) per visit. The `visit_edges`
  adapter uses `tree_mode_out_edges_collect` because the LTB
  primitive's `FnMut(u32, E)` signature doesn't match
  `FnMut(BucketEntryPosition, E) -> ControlFlow<B>`. A future
  optimization could stream LTB blocks through a single-block
  buffer (constant memory) without changing the API. Recorded as
  a follow-up optimization, not a correctness issue.