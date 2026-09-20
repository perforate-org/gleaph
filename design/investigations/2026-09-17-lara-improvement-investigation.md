# LARA improvement investigation from Orkut feedback (2026-09-17)

Date: 2026-09-17
Status: Investigation — no behavior change, no gates run, no code changed
Source: [Orkut feedback triage](2026-09-17-lara-orkut-feedback-triage.md) (from
`/Users/yota/dev/lara/docs/orkut-compare-2026-09-17/v1/feedback-to-gleaph.md`)
Baseline: `95d791087` (all canbench numbers below are quoted from the committed
`crates/ic-stable-lara/canbench_results.yml` at this HEAD; `canbench_results.yml`
itself is unmodified in the worktree)

## Cost-model premise (why DRAM ratios do not transfer)

IC-stable work is metered in instructions and stable-memory pages, not DRAM bytes.
The three memo findings must therefore be re-anchored to IC-measured costs before
any of them becomes a change:

- Labeled hub growth 0→4096 edges (Insertion policy, single vertex):
  `bench_labeled_stage2_hub_insert_grow_4096` = **29.74M ins total (~7.26K/edge)**.
- Core slab append, 1024 distinct vertices, no log, no rebalance:
  `bench_r_ed_st_si_1024` = **4.58M ins total (~4.47K/insert)**.
- Slab-only full scan: `bench_r_ed_st_oi_1024` = 157,671 ins for 1024 edges
  (**~154 ins/edge**); log-backed scan of 128 edges: `bench_r_ed_st_oi_lb_128` =
  35,379 ins (**~276 ins/edge**).
- Bypass→bucket promotion: `bench_l_bp_promo` = 115,888 ins total (114,486 in scope).
- Tree promotion (ADR 0088 Gate 3, plan evidence): 152.82K ins edge-only,
  1.16M ins at inline-property width 32 — i.e. **O(`T_promote` × (4 + w)) bytes**,
  not O(`T_promote`) edges.

## Finding A — hub growth drags its whole PMA leaf (memo item 1)

Production labeled geometry is segment16/quota1
(`labeled/graph/leaf_pin.rs:24-42`): each vertex starts with a 1-slot quota in a
16-slot pinned leaf block, and all buckets in the leaf share one 170-entry
overflow log (`DEFAULT_MAX_LOG_ENTRIES`). The insert loop recovers from
`SegmentLogFull` by folding/relocating the leaf and retrying
(`labeled/graph/insert.rs:602`).

The 4096-edge hub bench's scopes show exactly this loop, ~30 iterations:

| Scope (×calls) | Instructions |
| --- | ---: |
| `labeled_rebalance_leaf_cascade` ×30 | 4,491,239 |
| `labeled_relocate_leaf_physical_block` ×30 | 4,369,644 |
| `labeled_leaf_weighted_slide_commit` ×30 | 2,774,994 |
| `labeled_leaf_materialize_plans` ×17 | 1,350,438 |
| `labeled_leaf_stream_disjoint_relocation` ×13 | 983,910 |
| `labeled_leaf_try_expand_in_place` ×30 | 559,015 |
| `labeled_vertex_write_edge_runs` + `write_edge_run` ×30 | 841,708 |
| allocate/release footprint ×13, misc leaf scopes | ~700,000 |

Scoped leaf-level maintenance is ~15.9M of the 29.7M total; the remaining ~13.8M
(~3.4K/edge) is the unscoped scalar path (bucket lookup, descriptor read/write,
slot write, PMA accounting). So roughly **half of hub-growth cost is the hub
repeatedly relocating/sliding its entire leaf**, and each additional hub edge
re-pays leaf-scale work until promotion.

What this means for the memo's threshold question (`T=1024` effective in LARA-DRAM):

- The memo's mechanism (early hub isolation bounds rebalance scope) is confirmed
  on IC by the anatomy above — but its constant (1024) is **not** transferable.
  On IC, promotion itself transcribes O(`T_promote` × (4 + w)) bytes (152K–1.16M
  ins measured), and post-promotion growth must avoid the leaf-drag for the
  payback to hold. Neither side of that trade-off exists in the DRAM numbers.
- The in-flight tree-CSR work (ADR 0088) is already the structural answer for the
  relocate/cascade/slide share: blocks never relocate, values never enter the log
  in tree mode (root ids only, at 1/1024 the rate). Promotion at 4096 costs the
  equivalent of ~21–160 hub-growth edges — fast payback **if** post-promotion
  growth avoids the leaf-drag.
- Open measurement (gated on tree-mode landing): the current hub bench grows to
  exactly 4096 = `T_PROMOTE` and therefore likely never promotes
  (trigger is `stored_slots >= T_PROMOTE`, `labeled/graph/insert.rs:376`). A
  follow-up bench growing **past** the threshold with tree mode enabled measures
  the real post-promotion regime. No conclusion on 1024-vs-4096 until that exists.
- Making `T_PROMOTE` sweepable (1024/2048/4096) is itself a code slice: the
  constant threads through the dispatcher, cap checks, and LTB depth derivation.
  It is policy per the ADR 0088 constants registry, but not currently tunable.
  Recommend sequencing it **after** the post-promotion measurement, not before.

Core-tree reframing (Plan 0321): the core `EdgeStore` currently has **no**
rebalance, relocate, or fold implementation — all maintenance lives in
`labeled/graph/compact.rs` (`rebalance_labeled_leaf_weighted_slide`,
`relocate_labeled_leaf_physical_block`), and no non-labeled caller uses the core
store. "Core tree as a second instance" therefore presupposes a prior decision:
either port the maintenance machinery to core, or record that core stays a
primitive layer and hubs only exist in labeled. That ownership question
(architecture-integrity: which module owns the maintenance invariant) must be
answered before Plan 0321 work starts; it is not a small follow-up.

## Finding B — low-degree rows: write side only, attribution first (memo item 2)

Decomposition of the core ~4.47K ins/slab-append (distinct vertices, no log):

- Vertex get+set alone: ~713 ins (`bench_r_vx_gs_1024` = 730,341 / 1024).
- Raw slot write+read: ~344 ins (`bench_r_ed_swr_1024` = 352,470 / 1024).
- Remainder (~3.4K): successor-boundary read (`have_space_on_slab` →
  `slab_window_exclusive_end`), vertex rewrite, `num_edges` header write, and the
  counts-tree walk leaf→root (one read+write per level;
  `bump_counts_leaf_with_layout` in `lara/edge/row_layout.rs`).

Unmeasured hypothesis: the counts walk dominates the small-row insert. The memo's
tiny-row win (skip slab allocation/rebalance bookkeeping) maps on IC to
**skipping or aggregating per-insert PMA accounting and successor-boundary reads**
— but both are load-bearing today (density decisions; CSR window geometry), so
this is an ADR-scale trade-off, not a fast path.

What is already optimal: slab-only small-row scans (1 vertex read + 1 contiguous
read, ~154 ins/edge). There is **no read-side tiny win** to chase.

### Inline-tiny inside the `LabelBucket` descriptor (no new region)

Review of the draft above correctly notes the 29-byte descriptor
(`labeled/record.rs:55`; wire map in `write_to`/`try_read_from`) already carries
dead weight for a property-less, log-less bucket:

| Bytes | Field | Tiny need |
| --- | --- | --- |
| 0..8 | word: edge_start 36b + label 16b + log-head 8b + reserved 60–62 + tree bit 63 | label + 1 mode bit; edge_start/log-head unused |
| 8..12 | degree | needed |
| 12..16 | stored_slots | redundant (`stored == degree`) → repurposable |
| 16..20 | ipb_slab_slots | always 0 → repurposable |
| 20..25 | ipb_offset (u40) | always 0 → repurposable |
| 25..27 | ipb_width | always 0 → repurposable |
| 27 | ipb_log_byte | constant none → repurposable |
| 28 | ipb_log_len | constant 0 → repurposable |

Capacity math (4-byte targets): bytes 16..28 alone give 13B → K=3. Adding
stored_slots gives contiguous bytes 12..29 = 17B → **K=4** (16B + 1 spare).
K=5 would additionally need the word's edge_start bits (word surgery, invasive);
**K=8 (32B) does not fit any reading of the 29-byte descriptor.** The memo's K=8
is therefore not the achievable inline constant; the inline ceiling is K=4
(K=3 without touching stored_slots).

Why this is pattern-consistent: tree mode already repurposes byte 28 as the
depth marker under `TREE_MODE_BIT`, and bits 60–62 are free reserved bits for a
`TINY_MODE_BIT` (`labeled/slot_index.rs:24,80-88`). No wire-size change, no
MemoryId, no new reopen topology — validation extends the existing fail-closed
`try_read_from` pattern (zero/ignored-field enforcement per mode).

What inline-tiny (K=4, topology-only, width==0 fail-closed like the tree
carve-out) skips per row: leaf-quota span placement (including the possible
whole-leaf relocate on first-bucket pin, `labeled/graph/insert.rs:841-930`),
successor-boundary reads, overflow-log admission, per-insert `+1 actual` counts
bumps (`labeled/graph/insert.rs:495,814` — must explicitly skip, else tiny edges
inflate leaf density and trigger premature relocates), and all fold/slide/
relocate copying (zero leaf-block bytes). Scan becomes one 29B descriptor read;
delete compacts ≤4 slots inside the single descriptor write (dense prefix, no
tombstones). Promotion tiny→slab on the 5th edge reuses the existing quota path
with ≤4 slots to copy — bounded, synchronous, one-way initially (no slab→tiny
demotion, mirroring the deferred tree demotion).

Open design points (for the S4a slice, not decided here): dispatch enumeration
(insert/scan/delete/compaction/counterpart/batch classifier — third arm next to
slab/tree); recount-path agreement on the excluded `actual` (incremental bumps
skip vs any recompute-from-spans path); ADR 0088's "no mode branch in
rope/PMA/placement" holds only if tiny rows never touch slab paths.
Coverage caveat: K=4 at (vertex,label) granularity ≠ K=8 Orkut row shares —
finer granularity plausibly raises the bucket share, but that is unmeasured
(S3); the 13.6%/43.6% numbers justify nothing here.

Core (unlabeled) contrast: the 16-byte vertex row
(locator 8 + live 4 + slab 4) has zero dead space — every field is load-bearing
— so inline tiny is impossible without row growth (16→48B wastes 32B × every
vertex: rejected on fitness-for-purpose). Core tiny genuinely needs a new region
or stays out. The "new region" bar from the earlier draft applies to core,
not labeled.

Constraints if a tiny tier is ever pursued (all from current contracts):

- Pooled-chunk (memo-style) needs a new stable region: graph MemoryIds are
  densely allocated (see `design/storage/stable-memory-inventory.md`; LTB just
  took 53/54). A pooled chunk store needs inventory, composite all-or-nothing
  reopen, and free/chunk management with crash consistency — the memo's own
  caveat. The inline-descriptor alternative above avoids all of this for
  labeled; it does not exist for core (see inline-tiny section).
- Density-accounting interaction: rows outside PMA geometry change leaf density
  semantics; that is a labeled-maintenance contract change (ADR 0001/0020 area).
- The bypass mode is not a starting point: it is a label-homogeneity fast path,
  a different owning concept from row degree.
- Precedent exists in-repo: ADR 0022's lineage cites Terrace (in-place low tier,
  PMA mid tier, tree/B-tree high tier). A tiny tier would be Gleaph's low tier,
  but Terrace's split was validated against Terrace's cost model, not IC's.
- Missing input: no Gleaph workload degree distribution is cited anywhere. The
  Orkut tiny shares (13.6% of edges at 10M, 43.6% at 1M) are DRAM-layout facts;
  the IC question is the **instruction share** of low-degree inserts in a named
  target workload (social demo? bulk ingest?). Pick the workload before designing
  the tier.

Cheapest next measurement (bench-only, no code change to production paths):
attribute the core insert path with inner `bench_scope`s (successor read vs
counts walk vs header writes). That single attribution decides whether a tiny
tier has any leverage worth a new region.

## Finding C — log stays; manage its pressure (memo item 3)

No removal: the memo's two negative results are accepted as the reason.
Reframed as a review checklist for any future log-adjacent proposal, both
invariants already appear in Gleaph's contracts:

1. Proportional-gap placement guarantees no per-vertex minimum free slot
   (the 13,883rd-insert fixed point) — any "retry instead of log" proposal must
   state its per-row progress guarantee, or it is the same fixed point.
2. Single-row index edits violate segment-meta accounting
   (`total_space == capacity`) — any "surgical gap" proposal must name the
   invariant owner and update path, or it repeats the second failure.

Pressure, not existence, is the improvable surface: with quota1 + a shared
170-entry log, hub growth folds ~30× per 4096 edges, each fold leaf-wide.
Tree mode already relieves the hub side (root ids only). Remaining open
measurement: log-spill frequency of **non-tree buckets sharing a leaf with a
tree hub** (shared-log contention) — bench shape: one hub + N neighbors, count
folds. The persisted `bench_l_du_log_fold_mnt` (46.0M total) is **not** quoted
as a fold cost here: its scoped maintenance sums to only ~1.2M and the bulk of
its closure is unscoped workload, so a dedicated fold-cost bench is needed
before any claim.

## Proposed sequence (gated)

1. **S1 — core insert attribution (bench-only). DONE 2026-09-17, see S1 result:
   counts walk dominates (~56% of attributed).** Inner scopes for
   successor-boundary read vs counts walk vs header writes in the slab-append
   path. Decides whether a tiny tier has leverage. No production change.
2. **S2 — post-promotion hub regime (bench-only, gated on tree-mode landing).**
   Grow past `T_PROMOTE` with tree mode enabled; compare against the 29.74M
   slab baseline. Decides the tree payoff end-to-end.
3. **S3 — workload degree distribution (analysis). DONE 2026-09-17, see S3 result:
   K≤4 rows 44–70% (1M–10M Orkut prefixes); multi-label split raises coverage.
   Residual: property-width distribution.** Name the target workload
   and measure its low-degree insert instruction share. Required input for any
   tiny-tier ADR; S1 without S3 cannot justify a region.
4. **S4a — labeled inline-tiny proposal (design, only if S1+S3 justify K≤4
   coverage).** PROPOSED as [ADR 0096](../adr/0096-inline-tiny-mode-for-low-degree-label-buckets.md)
   2026-09-17 (K=3, wire map, dispatch table, gates G1–G6). Mode bit +
   bytes-12..29 repurposing, dispatch enumeration,
   density-accounting exclusion rule, one-way tiny→slab promotion, fail-closed
   validation matrix. No new region. Not an extension of bypass.
   **S4b — core tiny (design, much higher bar).** Requires row growth or a new
   region with inventory + reopen + free management; revisit only with core-side
   workload evidence.
5. **S5 — `T_PROMOTE` sweep (code+bench, only after S2).** Requires the
   tunable-threshold slice first; 1024 is a candidate arm, not a default.

## Threshold A/B run protocol (added 2026-09-17)

New benches in `crates/ic-stable-lara/src/labeled/bench.rs` (all 4-byte
`OneMTestEdge` production path; never `--persist` comparison runs):

- `thresh_hub_grow_8192` (M1): 0→8192 full-path inserts. Equal work both arms;
  exactly one promotion per arm (asserted tree at end). Isolates placement.
- `thresh_scan_2048` (M2a): full scan of a 2048-bucket; asserts regime flip
  (`is_tree == (T_PROMOTE <= 2048)`).
- `thresh_insert_2048` (M2b) / `thresh_delete_2048` (M2c): single-op on the
  flip-regime bucket; assert degree 2049/2047.
- `thresh_churn_roundtrip` (M4): 0→2T grow, delete to T_DEMOTE (mid-closure
  assert demoted — a no-op demote cannot pass), re-grow to T+1 (assert
  re-promoted, degree T+1).

Method: const-patch A/B (`T_PROMOTE` 4096→1024 in `labeled/graph.rs:52`;
`T_DEMOTE` follows as T/2). No tunability code — production threshold must stay
uniform (wire caps), and canbench runs no unit tests, so hardcoded test
literals are irrelevant to measurement. Both arms run on identical source
except the const; `canbench thresh_` focused patterns, no persist.

Guard audit (existing benches that reach degree ≥1024 change regime under the
patch — interpret, do not compare naively): `hub_insert_grow_4096/16384/65536`
(10B edges: slab-cap 4096→1024 probe = M6 wide-edge safety — must complete,
not trap); `ins_sb_1024`, `ins_last_1024`, `s2_det_hub_1024/4096` (check edge
type + max degree each before quoting); sub-1024 benches (`ins_fresh_256`,
`nt_bp_ins_*`, bypass/scan smalls) must be IDENTICAL across arms — regression
guard.

Adoption (only if numbers support): const change + hardcoded test/bench
updates (`T_PROMOTE_BENCH`, seed counts, demote messages) + ADR 0088 constants
registry amend + Gate 3 re-verification at T=1024 + full `canbench --persist`
per affected crate + PocketIC gates. Separate slice (S5).

Explicit non-goals: No-EL removal (rejected by evidence); core tree as a "small
second instance" (reframed as a maintenance-ownership decision first); adopting
any DRAM ratio as an IC target.

## S1 result — core insert attribution (measured 2026-09-17, worktree source)

Method: five sibling `bench_scope`s in `EdgeStore::insert_edge_inner`
(`lara/edge/insert.rs`; cfg-gated, zero production diff), run
`canbench bench_r_ed_st_si_1024` (1024 distinct-vertex slab appends, no log).
Totals are scope-overhead-inflated (13.48M vs committed 4.58M — the ~7.6M
remainder is ~5 scope open/close × 1024; `EdgeStore::header()` is already a
Cell mirror, so no header re-read hides there). Shares are the signal:

| Scope | ins total | /insert | share of attributed |
| --- | ---: | ---: | ---: |
| `lara_ins_acct` (num_edges write + counts leaf→root walk) | 3.30M | ~3,223 | **56%** |
| `lara_ins_vupdate` (grow + vertex rewrite) | 783.70K | ~766 | 13% |
| `lara_ins_window` (successor-boundary + span/counts reads) | 719.66K | ~703 | 12% |
| `lara_ins_slot_write` (capacity check + 1 slot write) | 548.24K | ~536 | 9% |
| `lara_ins_vread` (vertex row read) | 497.66K | ~486 | 8% |

Verdict: the hypothesis is confirmed directionally — **PMA counts accounting
is the dominant attributed cost** (~4.5× the next component even after
allowing ~300–400 scope overhead per scope). A tiny tier's leverage, if any,
lives primarily in skipping/aggregating the per-insert counts walk (with the
density-semantics design from Finding B), secondarily in successor-boundary
reads. Slot write + vertex rewrite are already near floor (consistent with the
~713 get+set and ~344 write+read micro-benches). Do NOT persist this run;
committed baselines stand.

## A result — HEAD isolation: latent production-path bug (2026-09-17)

Isolation worktree `/tmp/gleaph-head` (detached HEAD `95d791087`, throwaway
instrumentation only — main repo untouched): the full-path 4B growth 0→8192
fails **identically at HEAD** (target=5727, tree bucket, stored=degree=5728).
Verdict: **latent HEAD bug, not an in-flight regression.**

Root-cause evidence (throwaway eprintln tracing in the isolation worktree):

```text
release_span start=1063168 len=5
release_span start=1063168 len=5728
release FAILED start=1063168 len=5728: OverlapPrevious {
  previous: FreeSpan { start_slot: 1059545, len: 3628 },
  inserted: FreeSpan { start_slot: 1063168, len: 5728 } }
```

- The surfacing error `Store(RebalanceFailed(GrowFailed{0,0}))` is NOT built
  at `bucket_store.rs:478` (instrumented, never fired). It arrives via
  `impl From<GrowFailed> for LabeledOperationError` (`labeled/graph/error.rs:336`)
  from an **EDGE-slab** free-span failure (`lara/edge/span.rs` release path).
- A 5728-slot EDGE release (== current `stored_slots`) is issued from the tree
  growth regime and overlaps live free-span state by exactly the 5 slots of an
  earlier `len=5` release at the same start. Shape: double/overlapping release
  with `stored_slots`-scale length where a root-region-scale (or no) release
  belongs — or a leaf-footprint release sized by tree-inclusive accounting.
- Lead (confirmed and fixed by the owning slice): tree inserts kept feeding leaf
  PMA accounting that drives density, so after promotion every insert pushed the
  leaf to density ≥ 1.0 and fired `rebalance_cascade_after_labeled_mutation`;
  the cascading relocates then released an overlapping range. Both halves are
  now resolved (2026-09-20).

### A resolution (2026-09-20)

Two independent defects shared this trap, both in the same ownership:

1. **Release unit** — six resident-geometry paths sized releases from a logical
   cover where a physical width belongs, and one whole-cover release overlapped
   live free spans. Fixed by `bucket_physical_resident_slots` (compact.rs, SSOT:
   tiny 0 / slab `stored_slots` / tree `combined_span_region_len`) plus
   interval-only releases, then by delegating the two remaining whole-cover
   releases to `release_vertex_edge_span_slab` (skip-free-prefix + probe).
   Regression: `gap_tree_full_path_growth_past_5728_releases_only_owned_regions`.
   Error-mapping note above (`From<GrowFailed>` → `Store(RebalanceFailed)`) is
   what made the trap mislead; it stays a documented observability gap.
2. **Density numerator** — tree inserts bumped leaf `actual` (+1 per insert,
   −1 per remove) although tree rows live in LTB blocks and occupy only the root
   region; the promote path never subtracted the slab-era degree. Every insert
   after promotion therefore re-crossed the density trigger. Fixed by making
   `actual` mean *live edge records that occupy edge-slab slots*: tree
   insert/remove helpers no longer take a vertex id (structurally unable to
   count), promote subtracts the live degree, demote re-adds it, the leaf audit
   skips tree buckets, and the batch tree run skips its `actual` bump.
   Test: `tree_mode_leaf_actual_counts_slab_edges_only` (+ the tree-run
   assertion in `batch_run_admits_tree_mode_bucket_tail_fit`); wrong-impl
   probes (re-added insert bump / removed promote subtract / removed demote
   re-add / restored batch bump) each fail it.

Post-fix A/B re-measurement (same thresholds benches, unpersisted, one canbench
run per (arm, pattern); `T_PROMOTE` patched back to 4096 after measuring):

| Metric | T=4096 | T=1024 | Pre-fix (4096 / 1024) |
| --- | --- | --- | --- |
| M1 hub-grow 8192 (equal work) | 55.87M | **41.70M** | 82.30M / 92.89M |
| M2a scan 2048 | 74.30K | **40.09K** | 74.30K / 40.09K |
| M2b insert into 2048 (block boundary) | **8.36K** | 63.51K | 8,364 / 185.07K |
| M2b insert into 2050 (steady-state append) | 8.36K | **4.46K** | not measured |
| G5 skewed mix 256v/4520e (workload level) | 140.11M | **132.24M** | not measured |
| M2c delete 2048 | **4.84K** | 7.33K | 4,845 / 7,887 |
| M4 churn round-trip (threshold-relative sizing) | 117.65M | 30.77M | 147.54M / 38.64M |

Two readings changed materially: M1 improves in BOTH arms (82.30M→55.87M at
4096; 92.89M→41.70M at 1024 — the removed per-insert false cascade), and the
arm ordering **inverts** at workload scale (1024 now 25% cheaper on equal work).
M2b's remaining 7.6× is the tree-append cost at a block-boundary mint (stored
2048 → 2049 mints an LTB block, grows the root, reallocs the combined span), not
tree appends in general: seeded at 2050 (still inside the tail block) the same
append costs 4.46K in the tree arm vs 8.36K in the slab arm, so tree appends are
~1.9× cheaper in steady state. The insert/scan-dominated G5 workload mix agrees
(132.24M vs 140.11M, −5.6%). Every equal-work and workload-level metric now
favors 1024; only single deletes (7.33K vs 4.84K) and boundary mints favor 4096,
and the pre-fix 22× was the mint plus the false cascade. The recorded pre-fix verdict
("T_PROMOTE stays 4096", justified by M1) no longer follows from post-fix
evidence, so the constant was re-tuned to **`T_PROMOTE = 1024` / `T_DEMOTE = 512`**
(2026-09-20, after the overflow-log promotion fix cleared the gate): every
equal-work and workload-level metric favors it, and the ten threshold-coupled
tests now derive their sizes from `T_PROMOTE` so the suite is green at both
constants. Decision recorded in [implementation-gaps](../implementation-gaps.md).

Consequences (updated):

- S5 (threshold adoption) is no longer gated by this fix; it is gated by a
  deliberate threshold-flip slice, since the post-fix evidence now favors 1024
  on the equal-work workload metric while 4096 wins single boundary inserts and
  deletes.
- The standing regression bench (M1 shape) exists as `thresh_hub_grow_8192`
  (de-benched decision input) plus the compact.rs regression test, so the 4B
  full-path past-5.7K regime can no longer regress unexercised.

## S3 result — workload degree census (measured 2026-09-17)

Method: out-degree census over the recorded Orkut inputs
(`/Users/yota/dev/lara-runs/0039-v1-f22d4e993c15b24e/orkut-bin/mid-10M.input`,
sha256 `5a48a1fd…`, 10,000,001 lines = 1 header `3072627 10000000` + 10M shuffled
directed edges; shuffle = FNV-1a64(pair, seed=20260917) per the v1 phase1-input
record; order verified non-decreasing over the first 200K keys). Header excluded.
Single-label ⇒ buckets ≡ rows, so this is directly the `(vertex,label)`-bucket
census for single-label workloads.

| Prefix | K≤1 rows/edges | K≤2 | K≤3 | **K≤4** | K≤8 (memo ref) | distinct / maxdeg |
| --- | --- | --- | --- | --- | --- | --- |
| 1M | 32.4% / 6.2% | 50.4% / 13.0% | 61.7% / 19.5% | **69.6% / 25.4%** | 85.1% / 43.7% | 190,050 / 2023 |
| 10M | 16.8% / 1.3% | 28.3% / 3.0% | 37.1% / 5.0% | **44.1% / 7.2%** | 62.4% / 15.9% | 759,391 / 7428 |

Cross-checks: 1M K≤8 (161,772 rows / 436,702 edges) matches the memo's
161,469 / 435,637 within 0.2%, and maxdeg 2023 matches the memo's max hub
exactly — method validated. At 10M my K≤8 (473,529 / 1,588,524) exceeds the
memo's 420,136 / 1,361,358 by ~12%: input-regeneration or counting difference
on the memo side, unresolved and immaterial here — the memo never measured K≤4,
which is the decision input, and my definitions above are exact and reproducible.

Reading for S4a:

- K≤4 rows are 44–70% of ROWS but only 7–25% of EDGES. Row share drives
  span-management avoidance (quota/span/log/cascade participation per row);
  edge share bounds insert-path savings (each tiny insert skips counts+window
  per S1, plus unmeasured leaf-drag avoidance).
- Multi-label split is a strict refinement of rows ⇒ single-label coverage is a
  LOWER BOUND for multi-label tiny-bucket coverage (both row and edge shares
  rise under splitting; skewed hubs excepted).
- Residual unknowns (not measured): inline-property-width distribution in target
  production workloads (width>0 buckets are excluded from S4a tiny by design),
  and the IC instruction share (needs a tiny prototype or analytic bound — S4a).
- Social demo (88 vertices) is degenerate (~100% tiny) — cited, not decision-grade.

Verdict: coverage exists at both scales. PROCEED to S4a design (bench-gated
prototype follows the design, not precedes it). S1 leverage + S3 coverage jointly
satisfy the S4a entry gates from the sequence list.

## Review notes

- Skills consulted: `gleaph-architecture` (maintenance ownership stays in
  labeled; bypass vs tiny are distinct concepts), `architecture-integrity`
  (no existing concept covers a degree-based tiny tier; counts/successor reads
  are load-bearing — no duplication or shortcut proposed),
  `benchmark` (proxy discipline: DRAM ratios are not IC proxies; S1/S2 name the
  exact path each measures), `cost-aware-validation` (bench-only steps before
  any fixture-heavy work; no new fixtures proposed),
  `rust-workflow` (no commands run — investigation only).
- Design-sync: `lara.md`, `lara-dgap-contract.md`, ADR 0022, ADR 0088 all remain
  valid as written; the only new record is this note. If S4/S5 proceed they each
  need their own ADR amend; nothing here pre-approves them.
- LARA-DRAM degeneracy note: the memo's Orkut numbers describe row isolation
  (tree) and row exclusion (tiny) in a whole-graph PMA; Gleaph labeled already
  isolates differently (pinned leaf blocks + shared log + tree-CSR in flight),
  so even the mechanisms, not just the constants, differ.
