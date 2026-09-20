# Regime-cost improvements surfaced by the T_promote re-tune (2026-09-20)

Status: **design record — no implementation in this slice.** Two measured cost
items came out of the `T_promote = 1024` adoption and its `canbench --persist`
run; this document attributes them, lists the design options, and names the
acceptance metric for each. One of the two is `T_promote`-caused; the other is
not, and the initial attribution in the persist commit message
(`da852423d`) was corrected here by measurement.

Method: temporary `bench_scope` probes inside the suspect paths
(uncommitted, removed after measuring), focused runs of the affected persisted
benches (`canbench <name>`, never `--persist`), and one A/B of the same bench at
`T_promote = 1024` vs `4096` to separate re-tune effects from pre-existing ones.

## Corrected attribution

| Regression (persisted bench) | Cause | Evidence | Threshold-dependent? |
| --- | --- | --- | --- |
| Hub drain `bench_l_s2_det_hub_1024` 20.72 M → 51.31 M, `…_4096` 85.49 M → 208.00 M | **F1's per-emptied-bucket span release** (`release_bucket_edge_span_on_empty`, commit `3fd14768b`, 2026-09-19) | identical total at both thresholds (57.09 M with probes); pre-F1 artifact value 20.72 M dated 2026-09-18 | no |
| `tcsr_4096_property_read_w32` 3.22 M → 4.83 M | **tree-mode property reads** resolve the property leaf per row | same bench, same 4 096 rows, same w = 32: 3.22 M at `T_promote = 4096` (slab) vs 4.83 M at 1024 (tree) | yes |
| `bench_remove_churn_*` scope +310% | scope attribution (the tree delete path now runs inside the scope) | bench totals move +2% | n/a |

Non-findings: the w = 0 insert/scan/growth wins (M1 −25%, M2a −46%, M2b steady
−47%, G5 −5.6%) are unaffected by either item; the once-per-B-rows block-boundary
mint stays a bounded re-tune cost.

## Finding A — emptied-bucket span release costs ~30 K per emptied bucket

Per-call breakdown for `bench_l_s2_det_hub_1024` (1024 counterpart removals, each
emptying a 1-edge slab bucket):

| Probe | instructions | per call |
| --- | --- | --- |
| `labeled_remove_edge_skip_leaf` (the whole scoped removal) | 42.27 M | 41.3 K |
| `tmp_release_empty` (`release_bucket_edge_span_on_empty`) | 30.88 M | 30.1 K |
| `tmp_fs_release` (the free-span store `release_span` call) | 26.73 M | 26.1 K |
| `tmp_cover_recompute` (vertex/bucket re-read + cover sync) | 4.41 M | 4.3 K |
| `tmp_counts_dec` (PMA counts decrement, ×2 per removal) | 9.28 M | 4.5 K |

So the cost is the **free-span store's `release()` insert** (~26 K), not the
scan or the cover sync. `release()` itself is small code: duplicate-start check,
`prev_span`/`next_span` predecessor/successor lookups, merge decision, then
`insert_span` → `alloc_record` + `write_active_record` + `record_max_candidate`
+ `adjust_summary_after_insert`. At ~26 K per insert the dominant term must be
page-level work inside those structures (a 4 KiB-page read-modify-write per
insert is ~8 K; two structures plus the heap candidate list explains the rest).
A store-level spike (probes inside `release()`) is required before choosing a
store-level fix.

Why it shows up in drains: `delete_vertex_deferred` drains the hub and then
removes the counterpart row at each neighbour; every neighbour's 1-edge bucket
empties, and F1 releases each emptied span individually. A 1024-edge detach
delete therefore performs 1024 such inserts (this bench), a 4096-edge one 4096.

### Options

- **A1 — arena-style deferral (recommended first experiment).** Stop inserting
  sub-cover frees into the free-span store during drains; keep the existing
  cover shrink (the vertex row already drops to the survivors' end) and reclaim
  whole abandoned regions at relocate/slide/fold time. Pre-F1 behaviour was
  effectively this (the artifact value 20.72 M is the no-release cost). Expected
  win on this bench: ~−26 M (−50 %). Risk to measure: without the release, leaf
  occupancy looks higher until the next relocate, which can trigger earlier
  relocates — run the leaf-pressure benches (`thresh_hub_grow_8192`-shaped
  growth, `tcsr_*_insert_grow`, G4/G5) in the same session to bound that.
  This is also the direction the ledger already records for the double-free
  family (`GAP-2026-09-17-001`, "arena rule"), now with a performance reason.
- **A2 — pending-release log.** Keep releasing, but append the range to a
  stable pending log that maintenance folds into the free-span store in one
  batched pass (amortises the ~26 K insert over many ranges). Needs a new stable
  owner + recovery contract; larger than A1.
- **A3 — store-level insert cost.** Probe `release()` internals; if a page
  read-modify-write or the max-candidate list dominates, cheapen it (e.g. avoid
  rewriting a whole page for a single insert, or defer heap maintenance).
  Complements A1/A2 and would also speed up every other release path.
- **A4 — coalesce within a drain.** Only helps when emptied spans are adjacent
  (not the case for a hub's neighbours) — not sufficient alone.

Recommended order: **A1 experiment (one temporary patch + the bench set), then
A3 spike to understand the 26 K, then decide A2 vs shipping A1.**

## Finding B — tree-mode property reads resolve the property leaf per row

`visit_edges_with_inline_property` (tree path) already reads each **edge** block
once, but inside the per-slot loop it calls `read_property_value_at_slot`, which
re-resolves the property leaf (`resolve_property_leaf_block_id`: a LEG root read
plus offset math) and performs a separate 4-byte LTB read **per row**
(`tree_read.rs` ascending/descending loops). With w = 32 the property leaf holds
K = floor(4096 / 32) = 128 rows, so a 4 096-row scan pays 4 096 root reads +
4 096 partial reads instead of 32 block reads.

### Options

- **B1 — leaf-streaming cursor (recommended).** Track `(property_leaf_index,
  block_id, payload_buf)` in the scan loop; resolve and read the property block
  once per leaf, serve rows from the buffer, fall back to
  `read_property_value_at_slot` at leaf boundaries. Contained to the two scan
  loops in `tree_read.rs`; no wire or descriptor change.
- **B2 — cache only the resolved leaf id** (skip the root read, keep the 4-byte
  reads): simpler, recovers roughly half the delta. Useful stepping stone if B1
  turns out to interact with the LPB-in-tree property-depth handling.
- **B3 — widen property leaves.** Wire change; rejected.

Acceptance metric: `tcsr_4096_property_read_w32` back to ≤ ~3.5 M at
`T_promote = 1024` (slab reference 3.22 M), with
`lpb_in_tree_read_round_trip_at_w_4_stored_4096` and the property-slot bound
tests green.

## Suggested slice order

1. **A1 experiment** (temporary patch, benches only): quantify the drain win and
   the relocate-frequency risk in one session. If the risk is bounded, land A1
   with the drain bench as the acceptance metric and record the reclaim owner.
2. **B1** (tree_read scan cursor): straight win, small diff, covers the
   property-scan cost that the re-tune introduced.
3. **A3 spike** afterwards, folding whatever it finds into either A1/A2 or the
   free-span store directly.

Validation for all three: `cargo test -p ic-stable-lara --lib` at both
`T_promote` values (the suite is threshold-agnostic since the re-tune),
`cargo fmt`/`clippy -D warnings` for the touched crate, focused `canbench` runs
for the named metrics, and a final unfiltered `canbench --persist` only when a
slice lands.
