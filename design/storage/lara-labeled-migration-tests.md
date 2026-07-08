# Labeled LARA migration: test plan

**Status:** accepted  
**ADR:** [0001-labeled-segment-slide.md](../adr/0001-labeled-segment-slide.md)  
**Contract:** [lara.md](./lara.md)  
**Crate:** `ic-stable-lara` (`labeled/graph/*`, `lara.rs`)

## Purpose

Define **verifiable completion criteria** for each migration phase (A–E). Tests are the contract boundary: a phase is done when its gate tests pass and the regression suite stays green.

## Non-goals

- Replacing `canbench` budgets (listed as secondary signals only).
- Payload-slab migration detail (mirror edge phases in a follow-up doc when edge bytes move).

## Principles

1. **Do not weaken** hub regressions (`mixed_label_hub_*`) to make a phase pass.
2. **Scan contract** tests must never assert on `span_meta` or `FreeSpanStore` in iterator code paths.
3. Each phase adds **at least one test that fails on the prior implementation** (or documents why an existing test becomes the gate).
4. Prefer **deterministic unit tests** in `compact.rs` / `lara.rs` over wall-clock-only checks; use `#[ignore]` + manual timing only as a last resort.
5. At phase **E**, delete or rewrite interim-only tests that encode per-vertex tail-append semantics.

---

## Regression suite (all phases)

These must pass after every phase PR.

| Test | File | Protects |
|------|------|----------|
| `mixed_label_hub_20_labels_500_edges_each` | `labeled/graph/compact.rs` | Large multi-label insert completes |
| `mixed_label_hub_33_labels_span_release_regression` | `compact.rs` | No span-release cliff (~33 labels × 50 edges) |
| `mixed_label_hub_parallel_edges_do_not_corrupt_overflow_log` | `compact.rs` | Log integrity under parallel edges |
| `labeled_insert_and_iter_by_label` | `insert.rs` | Scan matches materialized edges |
| `labeled_desc_and_asc_out_edges_iters_match_materialized_rows` | `traverse.rs` | Iterator contract |
| `labeled_out_edges_iter_advance_by_and_nth_match_scan` | `traverse.rs` | Iterator indexing |
| `unchecked_label_iteration_matches_checked_for_valid_vertices` | `traverse.rs` | Unchecked fast path |
| `labeled_vertex_wire_bytes_golden` | `labeled/record.rs` | On-disk row layout (update only if layout version bumps) |

**Secondary (bench):** `crates/graph/src/bench/mod.rs` — `expand_mixed_label_hub_*`, `expand_skewed_noise_50k_*`; `crates/ic-stable-lara/src/labeled/bench.rs` — `bench_labeled_mixed_label_hub_insert_33x50`, `bench_labeled_mixed_label_hub_scan_33x50`, `bench_labeled_mixed_label_hub_asc_iter_33x50`. Record in `canbench_results.yml` when behavior or budgets change intentionally.

---

## Phase A — Pin labeled edge bytes to leaf `span_meta.physical_start`

**Outcome:** Vertices in the same PMA leaf share one contiguous edge-slab reservation; bucket `edge_start` values lie inside that leaf block.

### New tests

| Test name (proposed) | Module | Assertions |
|----------------------|--------|------------|
| `labeled_leaf_vertices_share_span_meta_physical_start` | `compact.rs` or `bucket.rs` | Hub + N neighbors in one leaf; all labeled vertices' bucket ranges ⊆ `[physical_start, physical_start + leaf_total)` from `span_meta` |
| `labeled_leaf_physical_block_covers_all_label_buckets` | `compact.rs` | After inserts across 3+ labels on one vertex, min(bucket `edge_start`) ≥ leaf `physical_start`, max(bucket end) ≤ leaf end |
| `labeled_span_meta_assigned_on_first_leaf_edge_write` | `insert.rs` | First labeled insert on a fresh leaf sets `physical_start` ≠ `SPAN_PHYSICAL_UNASSIGNED` |
| `labeled_reopen_preserves_leaf_physical_pin` | `compact.rs` | Build hub, reopen graph, re-assert leaf pin + scan equality |

### Gate (phase complete when)

- All Phase A tests pass.
- Regression suite green.
- Spike note in PR: whether existing graphs need layout migration or can adopt pin lazily on next mutation.

### Existing tests (unchanged behavior)

- All scan/iter tests — degree and neighbor lists unchanged.
- `vertex_edge_span_*` tests may still pass during A (interim path coexists); no deletion yet.

---

## Phase B — Leaf PMA density drives labeled maintenance

**Outcome:** `segment_edges_actual / segment_edges_total` on the leaf triggers labeled rebalance; **not** isolated `VertexEdgeSpan` density as the primary trigger.

### New tests

| Test name (proposed) | Module | Assertions |
|----------------------|--------|------------|
| `labeled_leaf_density_triggers_rebalance_not_vertex_span_alone` | `bucket.rs` | Instrument or spy: when leaf `actual/total` crosses threshold, labeled maintenance runs **without** requiring `stored_slots` growth first |
| `labeled_insert_skip_leaf_cascade_does_not_rebalance` | `insert.rs` | `insert_edge_skip_leaf_cascade` leaves leaf counts unchanged (control) |
| `labeled_dense_leaf_triggers_leaf_rebalance` | `traverse.rs` | **Rewrite** `labeled_dense_leaf_triggers_slack_growth_cascade` — assert leaf `actual/total` crossed threshold and maintenance ran, not `stored_slots` growth |
| `labeled_leaf_rebalance_preserves_scan` | `bucket.rs` | Before/after materialized `(vid, label) → targets` identical |

### Gate

- Phase B tests pass.
- `labeled_dense_leaf_triggers_slack_growth_cascade` rewritten or superseded (old assertion on `stored_slots` growth is interim semantics).

### Deprecate at B

- Tests whose **only** assertion is `stored_slots` increase on cascade (unless bypass mode).

---

## Phase C — Leaf rebalance: slide + log fold + bucket `edge_start` rewrite

**Outcome:** Dense leaf maintenance performs in-window weighted slide (core LARA), folds per-leaf overflow logs, rewrites all affected `LabelBucket::edge_start` values.

### New tests

| Test name (proposed) | Module | Assertions |
|----------------------|--------|------------|
| `labeled_leaf_rebalance_folds_overflow_log` | `compact.rs` | Fill log for leaf, trigger rebalance, `log_segment_idx == 0`, edges on slab |
| `labeled_leaf_rebalance_rewrites_all_bucket_starts_in_leaf` | `compact.rs` | Multiple vertices with labels in same leaf; after rebalance, buckets contiguous within leaf block and scan-stable |
| `labeled_leaf_weighted_slide_preserves_total_live_edges` | `compact.rs` | Sum of live edges per leaf before == after; tombstones packed or cleared per contract |
| `labeled_leaf_rebalance_does_not_release_span` | `compact.rs` | `FreeSpanStore::len()` unchanged across in-window rebalance (mirror `lara` core) |
| `labeled_proportional_slack_by_label_degree_after_slide` | `compact.rs` | **Extend** `vertex_edge_span_rewrite_weights_slack_by_label_degree` — same ratio invariant after leaf slide |

### Reference tests to mirror (core LARA)

| Core test | File | Labeled analogue |
|-----------|------|------------------|
| `lara_resize_folds_log_edges_back_into_slab` | `lara.rs` | `labeled_leaf_rebalance_folds_overflow_log` |
| `lara_reopen_preserves_rebalanced_layout_and_counts` | `lara.rs` | `labeled_reopen_preserves_leaf_physical_pin` + leaf counts |

### Gate

- Phase C tests pass.
- `mixed_label_hub_33_labels_span_release_regression` still completes in bounded time (no per-vertex peel on in-window path).

---

## Phase D — Single segment `release_span` on relocate

**Outcome:** When a leaf physical block moves, exactly **one** `release_span(old_start, old_len)` for the leaf footprint after commit; no per-vertex `release_vertex_edge_span_footprint` on relocate.

### New tests

| Test name (proposed) | Module | Assertions |
|----------------------|--------|------------|
| `labeled_segment_relocate_releases_single_footprint` | `compact.rs` | Count `release_span` calls (test hook or wrapper): == 1 per leaf relocate; `old_len == leaf segment_edges_total` |
| `labeled_segment_relocate_does_not_call_vertex_span_release` | `compact.rs` | `release_vertex_edge_span_footprint` not invoked on relocate path |
| `labeled_rewrite_within_pinned_leaf_does_not_release_vertex_footprint` | `compact.rs` | **Implemented** — rewrite/compact/slide inside pinned leaf keeps slack in-block |
| `labeled_relocate_commit_order` | `compact.rs` | After relocate, all bucket `edge_start` valid before free span contains old range (use store peek, not scan) |
| `labeled_segment_relocate_reuses_free_span` | `compact.rs` | Mirror `lara_local_relocation_reuses_prior_free_span` for labeled leaf |
| `labeled_segment_slide_coalesces_adjacent_free` | `compact.rs` | **Implemented** — leaf relocate releases footprint; adjacent free spans coalesce |

### Gate

- Phase D tests pass.
- `vertex_edge_span_retire_intervals_*` still valid for **interim cleanup** if any peel remains, or marked `#[deprecated]` in test name with sunset date.

---

## Phase E — Remove per-vertex tail append for normal labeled rows

**Status:** done (2026-06-05) — rewrite and rebalance paths use leaf relocate when pinned; no steady-state per-vertex tail append.

**Outcome:** Steady-state labeled insert does not append new physical span at `elem_capacity` for normal (non-bypass) rows; growth is leaf slide / leaf relocate only.

### New tests

| Test name (proposed) | Module | Assertions |
|----------------------|--------|------------|
| `labeled_insert_does_not_grow_elem_capacity_for_hub_growth` | `insert.rs` | 33-label hub insert sequence; `elem_capacity` unchanged OR grows only via global resize escalation, not per-vertex tail |
| `labeled_hub_33_labels_bounded_insert_time` | `compact.rs` | Same workload as regression; optional `std::time` ceiling (e.g. < 30s local) — gate for cliff class |
| `labeled_bypass_still_uses_core_vertex_path` | `insert.rs` | Default-label bypass row can still use core `Vertex` relocate semantics |
| `labeled_no_vertex_edge_span_rewrite_on_routine_insert` | `insert.rs` | Hook: routine `insert_edge` does not call `rewrite_vertex_edge_span` |

### Remove or rewrite at E

| Test | Action |
|------|--------|
| `insert_beyond_initial_label_edge_span_capacity_relocates_vertex_edge_span` | Rewrite to assert leaf relocate, not vertex span rewrite |
| `vertex_edge_span_retire_intervals_cover_interior_gaps_and_tail` | Delete if `vertex_edge_span_retire_intervals` removed |
| `compact_vertex_edge_span_*` (interim compaction) | Delete or move to `legacy/` module if API removed |
| `incremental_vertex_edge_span_compact_*` | Delete when per-vertex compaction removed |

### Gate

- Phase E tests pass.
- ADR 0001 migration complete; `labeled.rs` interim section updated to **implemented**.
- Full regression green (`cargo test -p ic-stable-lara --lib`, 275 tests incl. `mixed_label_hub_50_labels_1000_edges_each`).
- `expand_mixed_label_hub_10kscan_500match` and `expand_mixed_label_hub_50kscan_1kmatch` canbench pass (2026-06-07).

---

## Cross-cutting invariant tests (add once, run forever)

| Test name (proposed) | Module | Assertions |
|----------------------|--------|------------|
| `labeled_scan_never_reads_span_meta` | `traverse.rs` | **Implemented** — `ScanPathGuard` on hub scan paths; zero `span_meta` reads |
| `labeled_scan_never_reads_free_span_store` | `traverse.rs` | **Implemented** — `ScanPathGuard` on hub scan paths; zero edge free-span reads |
| `labeled_hub_materialized_matches_all_scan_iters` | `traverse.rs` | **Implemented** — hub fixture; materialized label targets match scan iterators |
| `labeled_payload_edge_order_matches_edge_slab_order` | `values.rs` | **Implemented** — after rewrite/compact, payload slots and dense offsets follow asc edge slab order |

---

## Test utilities (shared harness)

Add to `crates/ic-stable-lara/src/test_support.rs` (or `labeled/test_support.rs`):

| Helper | Use |
|--------|-----|
| `labeled_leaf_physical_range(graph, vid) -> (start, total)` | Phases A–D |
| `materialized_labeled_edges(graph, vid) -> Vec<(Label, Vec<Target>>)` | **Implemented** in `labeled/graph/test_support.rs` |
| `leaf_segment_counts_for_vid` (expose in tests) | Phase B |
| `count_free_spans(graph) -> usize` | **Implemented** in `labeled/graph/test_support.rs` |
| `exercise_labeled_hub_scan_paths(graph, hub)` | **Implemented** — shared scan-path exerciser for guard tests |
| `build_mixed_label_hub(labels, edges_per_label) -> (graph, hub, dst)` | **Implemented** in `labeled/graph/test_support.rs` |

---

## PR checklist (per phase)

```markdown
- [ ] Phase gate tests added and passing
- [ ] Regression suite (`cargo test -p ic-stable-lara --lib`) green
- [ ] No scan-path regression (iter tests)
- [ ] design/storage/lara-labeled-migration-tests.md phase section marked done
- [ ] canbench updated if bench labels/budgets changed
```

---

## Phase completion tracker

| Phase | Gate tests | Status |
|-------|------------|--------|
| A | `labeled_leaf_vertices_share_span_meta_physical_start`, … | **done** |
| B | `labeled_dense_leaf_triggers_leaf_rebalance`, … | **done** |
| C | `labeled_leaf_rebalance_folds_overflow_log`, … | **done** |
| D | `labeled_segment_relocate_releases_single_footprint`, … | **done** |
| E | `labeled_insert_does_not_grow_elem_capacity_for_hub_growth`, … | not started |

---

## Related documents

- [lara.md](./lara.md)
- [lara-dgap-contract.md](./lara-dgap-contract.md)
- [adr/0001-labeled-segment-slide.md](../adr/0001-labeled-segment-slide.md)
- [labeled-edge-inline-values.md](./labeled-edge-inline-values.md)
