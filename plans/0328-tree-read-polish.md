---
name: "tree read polish (D) — unify visit_edges_window to tombstone-inclusive positions + bench_t_v_window attribution"
overview: "Two recorded leftovers. D-1: re-measure bench_t_v_window at the 0327 HEAD; the +2.10% dispatch regression from Plan 0324 REWORK-3 was already absorbed by the 0326 dense-first dispatch reorder (10,574 ins, unchanged) — closed as solved. D-2 (main): visit_edges_window applied offset/limit in live-ordinal space on slab paths (bypass/dense/sparse) but tombstone-inclusive positions on the tree path; align all paths to the ADR 0088 §2 position contract (window cuts request-order positions over the tombstone-inclusive extent; visitor receives live edges only). Consumer audit: zero uses of TraversalWindow / edges_window outside ic-stable-lara (grep over crates/graph, crates/gql) — semantic change is consumer-safe."
todos:
  - id: "remeasure-bench-t-v-window"
    content: "canbench bench_t_v_window (plain) at 0327 HEAD vs yml 10,574. Result: unchanged → D-1 closed as solved-by-0326."
    status: completed
  - id: "unify-window-contract"
    content: "traverse.rs visit_edges_window: bypass path (extent = vertex.stored_slots), dense path (tombstone-free: position == slot), sparse path (extent = stored_slots + overflow-log len via overflow_log_chain_len; descending position = extent-1-slot) all cut the window in request-order position space and yield live edges only; tree path gains the live-only yield filter (position counting already position-space). Update the method doc."
    status: completed
  - id: "tests-and-benches"
    content: "Retarget the tombstoned sparse window test to the position contract (visit_edges_window_cuts_tombstone_inclusive_positions_when_tombstones_make_bucket_sparse); add slab/tree parity tests (traverse.rs + tree_read.rs visit_edges_window_tree_matches_slab_tombstone_positions). Re-encode the t_off_* bench expectations and bench_t_v_window to the position contract (truth_position_window). Full-suite canbench --persist (142 entries, 0 regression; bench_t_v_window 10,574 → 9,669 ins, −8.6%)."
    status: completed
  - id: "validate-and-commit"
    content: "Green bar (check / lib 548 / canbench lib 492 / clippy -D warnings / fmt --check) + wasm metrics (4,292,418 bytes, exports 286, name chars 39,078) + ADR 0088 polish entry + commit."
    status: completed
isProject: false
---

# Plan 0328 — ic-stable-lara tree read polish (D)

## Objective

Close the two recorded leftovers: D-1 (`bench_t_v_window` +2.10%
regression attribution) and D-2 (dense/sparse window contract
asymmetry vs the tree path).

## Context

- Handoff: /tmp/handoff-tree-read-polish.md. HEAD `bebbdc1ed`
  (Plan 0327). Baseline: lib 546 / 490, wasm 4,299,187 bytes, yml 142.
- Asymmetry detail: slab scan iterators skip tombstoned slots, so the
  sparse path physically cannot yield tombstone edges; the unified
  contract is therefore "window cuts tombstone-inclusive positions,
  visitor receives live edges only" — the tree path gains the
  live-only yield filter while keeping its position-space counting.

## Scope

- IN: `visit_edges_window` contract unification (bypass / dense /
  sparse / tree), test + bench re-encoding, ADR recording.
- OUT: the crate-root trait-level `visit_edges_window` default
  (src/traverse.rs:233) and
  `visit_edges_with_inline_property_window` — those count live edges
  per their own documented contract and have no tree/slab asymmetry.

## Expected Change Surface

| File | Change |
|---|---|
| `crates/ic-stable-lara/src/labeled/graph/traverse.rs` | Position-space window cut in bypass/dense/sparse paths; tree path live-only yield filter; doc |
| `crates/ic-stable-lara/src/labeled/graph/tree_read.rs` | Tree/slab parity test |
| `crates/ic-stable-lara/src/labeled/graph/traverse/bench.rs` | `truth_position_window` re-encoding of the t_off_* / v_window expectations |
| `crates/ic-stable-lara/canbench_results.yml` | Full-suite `--persist` (142 entries) |
| `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` | Polish entry |

## Steps

1. Re-measure `bench_t_v_window` (plain) → D-1 closed.
2. Unify the window contract in `visit_edges_window` + parity tests.
3. Re-encode bench expectations; full-suite `--persist`; green bar; commit.

## Consumer audit (D-2 safety)

`TraversalWindow` / `edges_window` / `visit_edges_window` grep over
`crates/graph`, `crates/gql`, and the SDK crates: zero hits. The window
API is `pub(crate)` to ic-stable-lara; in-crate consumers are the
Traversal impl (traverse.rs:3967), unit tests, and the t_off_*/v_window
benches — all updated to the unified contract.

## Validation

- `cargo check -p ic-stable-lara` — clean.
- `cargo test -p ic-stable-lara --lib --no-default-features` — 548 pass (baseline 546).
- `cargo test -p ic-stable-lara --lib --features canbench` — 492 pass (baseline 490).
- `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings` — clean.
- `cargo fmt --check -p ic-stable-lara` — clean.
- `canbench --persist` full suite — 142 entries, 0 regression; yml intact.
- wasm: 4,299,187 → 4,292,418 bytes (−6,769); exports 286 unchanged; name chars 39,078.

## Completion Criteria

- [x] D-1 closed: `bench_t_v_window` re-measured at 10,574 ins (yml
      unchanged) — the Plan 0324 dispatch regression was absorbed by
      the 0326 dense-first reorder.
- [x] All `visit_edges_window` paths cut the window in request-order
      tombstone-inclusive positions and yield live edges only;
      tombstoned positions consume window space on slab and tree alike.
      (Unit tests: `visit_edges_window_cuts_tombstone_inclusive_positions_when_tombstones_make_bucket_sparse`,
      `visit_edges_window_tree_matches_slab_tombstone_positions`,
      `visit_edges_window_applies_offset_limit_and_preserves_breaks`.)
- [x] Benches re-encoded; full-suite `--persist` (142 entries,
      0 regression); `bench_t_v_window` improved −8.6% (10,574 →
      9,669 ins) from replacing `try_advance_by` buffering with direct
      position cuts in the sparse path.
- [x] Green bar: check clean; lib 548 / 0 failed (baseline 546);
      canbench lib 492 / 0 failed (baseline 490); clippy `-D warnings`
      clean; `cargo fmt --check` clean. Wasm 4,292,418 bytes
      (−6,769), exports 286 unchanged, name chars 39,078.
