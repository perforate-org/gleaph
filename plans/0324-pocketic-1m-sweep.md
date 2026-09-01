---
name: "PocketIC 1M-degree sweep — 2^20 reachability under the fail-closed cap"
overview: "Implement Plan 0324 per Plan 0318 §Later Slices: move the 1M-degree high-degree sweep from the `#[ignore]`d host prototype test to canbench benches on production `VirtualMemory` (PocketIC), and verify 2^20 = 1,048,576 slots/bucket is exactly reachable under the interim fail-closed `TreeRootCapacityReached` guard. The existing `tree_csr_high_degree_test.rs` (171 lines) measures the `TreeCsrBucket` PROTOTYPE (not the production `LabeledLaraGraph` tree path) via host wall-clock proxy values, and its `high_degree_*` tests cannot run on `VectorMemory` (raw-block `mint` grows ~4.1 GB of process heap at 1M blocks — Plan 0315 amend). The wasm export-name budget that forced the host-test approach in Plan 0313 is no longer binding: after the Plan 0318 bench cleanup the surface is at 15,730 / 20,000 chars (~4,270 headroom), so 2-3 new canbench bench functions (~150 chars) fit. Plan 0324 delivers: (a) a seeding + measurement bench at 1,048,576 edges on the production labeled tree path — full_scan_descending and random_ordinal_access at the cap (the Plan 0313 parity rows reachable at scale), with insert_grow measured just BELOW the cap; (b) a guard bench proving the 2^20+1-th insert fails closed with `TreeRootCapacityReached` and the bucket stays intact; (c) confirmation that the Plan 0319 demotion path reclaims an at-cap bucket (degree-driven demote → slab rebuild at 1M-scale footprint). The `#[ignore]`d prototype host tests are REMOVED (user decision 2026-09-01: unrunnable prototype benches are removed; git history + this plan keep the records) — they measure the prototype, not the production path, and can never run on `VectorMemory`. Numbers recorded in `design/implementation-gaps.md` (the high-degree anchor) and ADR 0088 §Status. Green-bar discipline maintained; validator final phase."
todos:
  - id: "audit-1m-feasibility"
    content: "Audit the 1M sweep feasibility and record findings in this plan doc BEFORE code: (a) seeding cost through the production `LabeledLaraGraph` scalar insert path — extrapolate from the Plan 0318 Gate 2 numbers (insert_grow 17,070 ins/edge at 4K; verify what that number included) and estimate total instructions for 1,048,576 inserts; confirm it fits the canbench runner's per-bench instruction limit (audit `canbench_rs` / the canbench CLI configuration — default PocketIC instruction limit per call and whether `.canbench/config` or canbench_results.yml carries a per-bench budget override); if seeding at 1M exceeds the limit, design the split (seed to 65K then... NO cross-bench state exists — instead measure the sweep in ONE bench that seeds outside `bench_scope` and measures inside, and verify PocketIC's wall-clock/instruction ceiling for a single call with the maintainer docs in `.agents/skills/benchmark/SKILL.md`); (b) which Plan 0313 parity rows are reachable on the production surface at 1M (full_scan_descending 41 ins/edge → ~43M total ✓; random_ordinal_access ~209K/call ✓; insert_grow must run below the cap — seed to `2^20 - 1024` then measure 1024 tail inserts); (c) the guard bench: seed to exactly 2^20, insert one more edge, assert `LabeledOperationError::TreeRootCapacityReached` surfaces (via the bench's expect pattern) and the bucket descriptor is unchanged; (d) memory: 1M slots = 1,024 LTB blocks (4 KiB each = 4.1 MB) + root region 1,024 × 4 bytes — trivial for stable memory; (e) confirm the Plan 0319 demote interplay at scale is covered by existing tests (not re-benched here). Write the findings + the final bench list into the plan before implementing."
    status: pending
  - id: "tcsr-1m-canbench-benches"
    content: "Add the 1M canbench benches to `crates/ic-stable-lara/src/labeled/bench.rs` (naming per the existing tcsr convention, e.g. `tcsr_1048576_full_scan_descending`, `tcsr_1048576_random_ordinal_access`, `tcsr_1048576_insert_grow_below_cap`, `tcsr_1048576_root_capacity_guard` — final names per the wasm-budget audit, all ≤ 20,000 chars total): (a) each bench seeds the production `LabeledLaraGraph` with a 4-byte-edge label bucket to 1,048,576 (or 2^20 - 1024 for insert_grow) OUTSIDE `bench_scope` (seeding is not measured — mirror the `inline_property_pressure_stats` seeding pattern), then measures the op inside `bench_scope`; (b) full_scan: `visit_edges` / `out_edges_collect` descending over 1M edges (expect ~41 ins/edge scale); (c) random_ordinal_access: 64 probes (expect ~209K ins/call scale); (d) insert_grow_below_cap: 1024 tail inserts at 2^20 - 1024 (tail-block growth path); (e) the guard bench `tcsr_1048576_root_capacity_reached`: seed to exactly 2^20, attempt one more insert INSIDE `bench_scope`, assert the typed `TreeRootCapacityReached` error surfaces and `stored_slots` is unchanged — the bench PASSING means the fail-closed guard held at the exact cap; (f) seeding loop must use the production scalar insert (`insert_edge_skip_leaf_cascade`) so promotion + tree growth are exercised at scale (this is the production-representative seeding the prototype test never did). Run each bench via `~/.cargo/bin/canbench <name>` against the wasm build and record ins/edge (and ins totals) in `canbench_results.yml` — this time the new entries are INTENTIONAL: commit the yml with the new 1M rows (do NOT restore the yml this slice; the 1M numbers are the deliverable). Wasm name-char budget: verify ≤ 20,000 after adding the benches (expect ~15,900)."
    status: pending
  - id: "record-numbers-and-cleanup"
    content: "Record the 1M numbers: (a) append a high-degree anchor section to `design/implementation-gaps.md` (the Plan 0313 note says the 1M numbers land there) with the measured ins/edge per row + the guard-at-cap verdict; (b) update ADR 0088 §Status with the 1M sweep results (2^20 exactly reachable, guard verified at cap+1, demote reclaim path available at scale); (c) REMOVE the `#[ignore]`d prototype host tests in `tree_csr_high_degree_test.rs` (all `high_degree_*` tests) — user decision 2026-09-01 (unrunnable prototype benches are removed; the file's remaining evidence value is superseded by the canbench 1M numbers). Keep `hub_tree_prototype.rs` / `tree_csr_prototype.rs` untouched (evidence-only, separately tracked). Unit test (host, cheap): a small-scale guard test `root_capacity_guard_fires_at_structural_boundary` mirroring the bench assertion but at a reduced degree is NOT possible (the guard is at 2^20 physical root length) — instead verify via the existing `tree_insert_fails_closed_at_root_capacity` unit test (Plan 0318) still passing and note the 1M guard is bench-verified only."
    status: pending
  - id: "wasm-budget-recheck"
    content: "Run `cargo build --release --target wasm32-unknown-unknown --features canbench` from `crates/ic-stable-lara/` and verify the exported-name budget stays ≤ 20,000 chars (baseline 15,730 + the new bench names — record the exact count). Full green-bar matrix: plain `cargo check -p ic-stable-lara`, `cargo test -p ic-stable-lara --no-default-features` (619 baseline; the removed prototype tests are `#[ignore]`d so counts may shift by the removed tests — record the exact delta), `cargo test -p ic-stable-lara --features canbench` (0 failed), `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings`, `cargo fmt --check -p ic-stable-lara`."
    status: pending
  - id: "adr-gaps-update-and-validate"
    content: "Run `python3 ~/.agents/skills/plan/scripts/validate_plan.py plans/0324-pocketic-1m-sweep.md --phase final` and confirm structurally-valid final-phase verdict. Ensure `design/implementation-gaps.md` + ADR 0088 §Status carry the 1M numbers and the guard verdict (this todo rides on the Step 2/3 commits; the final validation is the closing gate)."
    status: pending
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

Instruction limits (canbench runner + PocketIC per-call), seeding cost estimate, the reachable parity rows at 1M on the production surface, memory math. Decide the final bench list + seeding strategy from the numbers. Record in the plan doc.

### Step 1 — 1M benches + guard bench

Implement per todo `tcsr-1m-canbench-benches`. Run them; record numbers.

### Step 2 — remove the prototype host tests; record numbers in gaps doc + ADR

See todo `record-numbers-and-validate`.

## Validation

- **canbench**: the 1M benches run to completion with recorded ins/edge; the guard bench PASSes (typed error at cap+1); 4K/65K baselines unchanged (±1%).
- **Green-bar matrix**: plain check / 619±(removed tests) baseline / full canbench suite 0 failed / clippy -D warnings / fmt / wasm ≤ 20,000 chars with the new names.
- **Validator**: `validate_plan.py --phase final` structurally valid.

## Completion Criteria

- [ ] 1M benches implemented and run; numbers recorded in yml + `design/implementation-gaps.md` + ADR 0088 §Status.
- [ ] Guard-at-cap bench proves `TreeRootCapacityReached` at exactly 2^20+1 with the bucket intact.
- [ ] `#[ignore]`d prototype host tests removed (recorded in the plan; git history keeps them).
- [ ] Wasm exported-name count recorded and ≤ 20,000; full green-bar matrix green.
- [ ] `validate_plan.py --phase final` PASS.

## Later Slices (recorded, not in this plan)

- **Interior-level insert growth** (plan 0318 §Later Slices): lifts the 2^20 cap; the sweep's guard bench becomes the 2^30 boundary test afterward.
- **delete_half / compaction rows at 1M**: O(S)-class operations need stepped maintenance (ADR 0021); bench when the demote/reclaim path has telemetry.
- **Wave 2**: LPB-in-tree (reuses the 0320 primitive), crate slimming (drop the unlabeled graph implementation + `bench_r_*` benches).