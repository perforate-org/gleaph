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

### Measured breakdown (2026-09-20, corrected)

Instrumenting `release()` with many `bench_scope`s inflated the numbers (each
scope entry costs ~2-3 K on a hot path), so the attribution was redone with a
native, uninstrumented micro-bench (`free_span/bench.rs`,
`fs_drain_release_pattern_1024`, de-benched) plus whole-`release()` ablations
(bench-only patches, reverted):

| Ablation | per release |
| --- | --- |
| A: dup/prev/next lookups only | ~0.6 K |
| B: A + skip the replace/insert writes (record relink kept) | ~27 K |
| C: B + skip the double-merge neighbour removal | ~13 K |
| full `release()` in the drain pattern | ~34 K |

So the cost is the free-span store's **stable-memory writes** (~1.3 K each, ~20
writes per release on the double-merge path that a high-to-low drain produces),
not the neighbor lookups. Two bounded improvements landed the same day:

- `release()` now needs **two** page-walking lookups instead of four: a new
  `predecessor_or_equal` on `ic-stable-paged-ordered-map` answers "duplicate
  start or predecessor" in one directory walk, and `successor` covers both the
  adjacent-next merge and the next-overlap check. The rewrite also fixes a
  latent hole where a free span strictly *inside* the released range was ignored
  whenever another span started exactly at the range end (the old shape merged
  over the inner span, producing two overlapping free spans); regression
  `release_prefers_inner_overlap_over_adjacent_merge` (wrong-impl probe: the old
  lookup shape returns `Ok` instead of `OverlapNext`).
- `write_record` writes the 48-byte record in **one** stable-memory write
  instead of six per-field writes (−5.5 % on the native pattern bench).

Effect on `bench_l_s2_det_hub_1024`: 51.31 M → 50.03 M (−2.5 % total; the scoped
removal 36.57 M → 35.38 M). The remaining ~32 K per release is store
bookkeeping, i.e. it does not shrink to zero by micro-optimisation.

### Scalable fix (A2, recommended)

The drain performs **one store release per emptied bucket**; each costs ~32 K
regardless of the bucket's span length. The fix is fewer releases per logical
operation: collect the emptied spans of one detached-vertex delete (they are
adjacent by construction once the neighbours' buckets are freed) and flush them
as pre-merged ranges in one store pass.

Constraints the flush must satisfy:

- **F1 reuse contract.** F1's regression
  (`emptied_bucket_releases_span_and_zeroes_vertex_cover`) asserts that the freed
  span becomes reusable; the flush must therefore complete before the delete
  operation returns, so the batch belongs to the drain boundary
  (`delete_vertex_deferred` / the resumable step), not to a lazy maintenance
  pass.
- **Cover sync stays per-delete.** `vertex.stored_slots` must shrink as each
  bucket empties (that part costs only ~4 K and keeps accounting honest); only
  the store insertion is deferred to the flush.
- **Double-free protection.** The flush reuses the GAP-2026-09-17-001
  protections (`release_vertex_edge_span_slab` semantics: skip free prefixes,
  never hand unowned ranges to the store).
- **Owner.** The batching needs a per-drain accumulator with a clear owner
  (either a parameter threaded through the delete path or a
  `DeleteContext`-style state); a global pending list would need a recovery
  contract, which is the A2-with-stable-state variant.

Acceptance: `bench_l_s2_det_hub_1024` back toward ~20-25 M with
`fs_drain_release_pattern_1024` recording the per-release store cost, and F1's
reuse regression plus the free-span suite green.

### Options (narrowed by the measurement)

- **A2 — drain-level batch flush (recommended; see above).** Fewer releases, F1
  contract preserved at the operation boundary.
- **A3 — store write-path refactor.** ~20 writes per release at ~1.3 K each;
  batching the header/summary/bin updates per release could cut this further
  (the record write is already batched). Complements A2 and benefits every
  release path.
- **A1 — blanket deferral of sub-cover frees** is **rejected**: it breaks F1's
  reuse regression (a freed span must be reusable when the delete returns).
- **A4 — coalesce within a drain** is subsumed by A2 (the flush pre-merges).

Recommended order: **A2 first (it is the only lever that removes whole
releases), then A3 for the residual per-release cost.**

## Finding B — tree-mode property reads resolve the property leaf per row

`visit_edges_with_inline_property` (tree path) already reads each **edge** block
once, but inside the per-slot loop it calls `read_property_value_at_slot`, which
re-resolves the property leaf (`resolve_property_leaf_block_id`: a LEG root read
plus offset math) and performs a separate 4-byte LTB read **per row**
(`tree_read.rs` ascending/descending loops). With w = 32 the property leaf holds
K = floor(4096 / 32) = 128 rows, so a 4 096-row scan pays 4 096 root reads +
4 096 partial reads instead of 32 block reads.

**Implemented 2026-09-20 (B1, `tree_read.rs` `PropertyLeafCache`):**
`tcsr_4096_property_read_w32` 4.83 M → 3.66 M at `T_promote = 1024` (slab
reference 3.22 M; the +50 % regression is now +13.7 %). Deterministic regression:
`tree_property_scan_reads_each_payload_block_once` counts LTB payload read calls
and requires exactly 4 edge blocks + 32 property leaves = 36 per scan in both
orders (pre-fix shape: 4100). Residual: one leaf resolution + one 4 KiB block
read per leaf, plus the per-row property value clone the visit API requires.

### Options (as designed)

- **B1 — leaf-streaming cursor (chosen).** Track `(property_leaf_index,
  block_id, payload_buf)` in the scan loop; resolve and read the property block
  once per leaf, serve rows from the buffer, fall back to
  `read_property_value_at_slot` at leaf boundaries. Contained to the two scan
  loops in `tree_read.rs`; no wire or descriptor change.
- **B2 — cache only the resolved leaf id** (skip the root read, keep the 4-byte
  reads): simpler, recovers roughly half the delta. Useful stepping stone if B1
  turns out to interact with the LPB-in-tree property-depth handling.
- **B3 — widen property leaves.** Wire change; rejected.

Acceptance metric (met): `tcsr_4096_property_read_w32` back to ≤ ~3.5 M at
`T_promote = 1024` (slab reference 3.22 M) — landed at 3.66 M with
`lpb_in_tree_read_round_trip_at_w_4_stored_4096` and the property-slot bound
tests green, plus the new payload-read-call test.

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
