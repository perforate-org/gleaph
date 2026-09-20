# Discovered Implementation Gaps

Last updated: 2026-09-20
Anchor timestamp: 2026-08-25 22:49:39 UTC +0000

## Status

**Active tracking document** — this ledger records implementation defects, missing product
capabilities, and contract mismatches discovered while implementing another slice when they cannot
be resolved safely in that slice.

It is not a second roadmap or design source of truth. Each entry names the owning module and links
the active design contract. Once an architectural decision is accepted, the owning design document
or ADR remains authoritative and this ledger points to it.

## Disposition rule

Every counterpartrial gap discovered during implementation, review, validation, or demo integration must
receive one disposition before the current work is committed:

1. **Fix now** when it is a correctness or security defect, blocks the current contract, has a clear
   owner, and can be repaired without obscuring the current slice.
2. **Prerequisite slice** when it blocks the current work but needs independent implementation,
   review, validation, or commit history.
3. **Record here** when it is real but non-blocking, its design is unresolved, or fixing it would
   expand the current slice counterpartrially.
4. **Reject as not a gap** only with evidence that the observed behavior matches an existing active
   contract.

Do not leave a gap only in terminal scrollback, a temporary report, an ignored plan file, or a final
chat summary.

## Entry requirements

Each open entry must state:

- **Observed behavior:** reproducible fact, not a proposed solution;
- **Expected or needed behavior:** the contract or product need that exposes the gap;
- **Owner:** module/domain that owns the violated invariant or missing API surface;
- **Evidence:** test, command, source path, or design section;
- **Impact:** what remains unsafe, impossible, misleading, or inefficient;
- **Next decision:** the smallest question or slice that can resolve it;
- **Status:** `Open`, `Planned`, `In progress`, `Resolved`, or `Not a gap`.

Resolved entries remain in the ledger with the fixing commit and owning test. This prevents the same
defect from being rediscovered without its prior reasoning.

## Open gaps

### GAP-2026-09-20-006 — Committed-space audit: eagerly grown overflow-log capacity and per-vertex descriptor slack

- **Status:** Open, measured 2026-09-20 while auditing for the same "wasted data area" class as
  GAP-2026-09-20-005 (the promotion's dead prefix). Numbers are page counts from
  `Memory::size()` (whole 64 KiB WebAssembly pages) plus the graph's own accounting.
- **Owner:** `ic-stable-lara` per-leaf overflow log (`lara/edge/log.rs`) and per-vertex bucket-row
  slack (`labeled/bucket_store.rs`).
- **Observed behavior (5000 vertices, one edge each, single label, segment 16):**
  | | value |
  |---|---|
  | committed pages, empty graph | **30 pages ≈ 1.9 MB** (16 memories' 1-page minimums + the stores' initial growth) |
  | committed pages, after 5000 tiny edges | **66 pages ≈ 4.2 MB** (buckets 15, log 11, edges 5, vertices 2 pages) |
  | actual edge data | 5000 × 4 B = **20 KB** |
  | bucket descriptor rows reserved | **25,000** for 5,000 buckets (5 rows per vertex: 1 live + 4 slack = 116 B/vertex, physically committed) |
  | overflow-log capacity declared | **87,040 entries = 696 KB** for a workload that used **0 entries** |
  | edge-slab cover / resident | 0 / 0 (tiny truly occupies no edge slab) |
- **Two scaling wastes:**
  1. **Per-leaf overflow-log capacity is eagerly committed.** `DEFAULT_MAX_LOG_ENTRIES = 170` per leaf
     (`log.rs`), stride `4 + E::BYTES` = 8 B, and `grow_segment_count_to` ensures the *whole*
     `segment_count × 170 × 8 B` region is backed at init/reopen. At segment 16 that is
     **1,360 B per 16 vertices = 85 B/vertex**: 1M vertices → ~85 MB, 10M → ~850 MB, committed
     whether or not any leaf ever spills. The log is only needed for leaves that actually overflow
     the slab window, and the earlier log-pressure measurement (Finding C, ~1.1K instructions per
     append, folds ~43K) gives no evidence that the capacity must be reserved graph-wide.
  2. **Per-vertex bucket-row slack commits 4 extra 29-byte rows (116 B) per vertex with any label.**
     The slack is consumed by later label inserts without an in-segment rewrite (the reason it
     exists: `insert_label_bucket_at`'s fast path), so the fix is a policy/tuning question, not a
     delete: measure the descriptor-rewrite cost it saves (the vertex-segment rewrite path) against
     the 116 B/vertex.
- **Also noted (fixed-cost, likely inherent):** every one of the 16 stable memories costs a 1-page
  minimum (≈1 MB at init), and the edge/lb stores each grow to ~320 KB before any payload; merging
  small meta stores or lowering the initial growth is a layout-table decision, not a local change.
- **Expected or needed behavior:** grow the log's entry region lazily (per touched leaf, or with a
  high-water counter) so tiny/slab-only graphs commit no log entries; re-tune the bucket-row slack
  with a measured insert-vs-rewrite trade; then re-run the audit probe
  (`/tmp/space_audit_probe.rs`) to confirm the delta.
- **Next decision:** decide whether to fold this into the same space-first slice as
  GAP-2026-09-20-005 (both are "committed bytes we never use") or take it as its own slice with the
  bench for the slack trade.

### Incident 2026-09-20 — `git stash` + `drop` in the shared worktree (recovered)

Whlie diagnosing GAP-2026-09-20-005 a `git stash` was used to bisect and then dropped, which swept up
**twelve uncommitted files belonging to other workstreams** (`crates/graph/src/facade/**`,
`crates/graph/src/index/canonical_export.rs`, `Cargo.lock`, the `deferred.rs` / `iter.rs` comment
hunks, `pocket-ic-tests/tests/adr0059_index_build_lifecycle.rs`, `design/adr/0050` / `0059`). All of
them were restored from the still-reachable dangling stash commit `c2fae9e5b` (`git fsck
--no-reflogs --unreachable`), verified back in place, and the suite is green again (611/0). No
foreign content had been committed: the one shared file committed here (`design/adr/README.md`)
contains only this workstream's ADR 0096 row.
**Discipline for this worktree:** never `git stash` (or any tree-wide state operation) while other
agents have uncommitted work; bisect with file copies instead (`/tmp/...`), as the rest of this
session did.

### GAP-2026-09-20-005 — Log-fold span growth can extend a vertex cover over a leaf mate (K=4 exposes it)

- **Status:** **Partially fixed 2026-09-20 (`7ad12b230`)** — the fold-prelude path is closed with a
  regression test; a second path is still open (details below). Discovered while auditing
  production capacity/latency balance;
  regression-adjacent to ADR 0096 §3b Phase 2 (`91575bc29`), which made it reachable in a
  production-shaped skew where the K=3 wire was not.
- **Severity:** P0 correctness (cover overlap ⇒ a later release/density decision can act on a
  mate's slots; the debug-only guard `assert_no_labeled_leaf_mate_overlap` is the only detector,
  so **release builds run on silently.**
- **Owner:** overflow-log fold / span-growth path (`compact.rs` fold + cover growth; the same
  class the promotion path already guards with its `mates_free` check).
- **Observed behavior:** after a hub's span fills, the per-leaf overflow log fills and folds into
  the bucket span. The fold grows `stored_slots` **in place** without checking the leaf's other
  covers, so the grown span walks over mates that were placed after it.
- **Minimal reproduction** (deterministic, single label, Insertion policy, production
  `segment_size = 16`, `InitialCapacities::uniform(256)`, 16 vertices in one leaf, `TestEdge`
  4-byte): degrees `[8, 8, 8, 8, 64, 64, 64, 64, 64, 64, 64, 64, 512, 512, 512, 2048]`, mates
  inserted first, then the 2048-degree hub. The hub promotes at its 5th insert into the 8-slot
  gap `[356, 364)` (legal), then the fold grows it to 175 slots in place:
  `vid VertexId(15) reserves up to 531 but vid VertexId(5) starts at 364`
  (`leaf_pin.rs` guard, leaf 0, block `(256, 2288)`).
- **K=3/K=4 split (measured):** the same probe on `1c277d95c` (R4, K=3) **passes**; on
  `91575bc29` (K=4) it fails. The missing mate check is not new, but the K=4 promotion shape
  (span 5 instead of 4, promotion at the 5th insert instead of the 4th) changes which free run
  the hub lands in, turning a latent hazard into a reachable one.
- **Expected or needed behavior:** the fold's span growth must be mate-disjoint (check
  `labeled_leaf_occupied_spans` for `[base, base + grown)`), and a collision must relocate/grow
  the leaf block and retry — the same bounded-retry pattern as
  `promote_tiny_to_slab_with_growth`. Promote the debug guard to a checked error on the growth
  path (fail closed) so release builds cannot proceed on an overlapping cover.
- **Impact:** data-loss class (an overlapping cover can free a mate's slots on delete/relocate),
  plus density/audit misfires. Also blocks any honest capacity-vs-latency tuning of the tiny/slab
  boundary, since the geometry it would tune is currently not always valid.
- **Fold-path fix (`7ad12b230`):** `prepare_vertex_edge_span_for_overflow_log_fold` now requires
  the wider range to be mates-disjoint (reusing `try_labeled_vertex_edge_base_in_pinned_leaf`),
  relocates the leaf block on a collision, and fails closed otherwise. Regression
  `fold_growth_stays_mate_disjoint_across_span_growth` (the minimal reproduction above) fails on
  the block-only check and passes with the fix.
- **Second path still open (diagnosed 2026-09-20 on the same commit):** the wider G5-shaped skew
  (256 vertices, degrees 1..2048, segment 16) still trips the guard in leaf 15:
  `vid 255 reserves up to 1568 but vid 240 starts at 400`.
  The writer is the **tree promotion**, not the fold:
  `promote_bucket_if_needed` -> `promote_bypass_to_tree_mode[_impl]` publishes the new tree
  descriptor with `edge_start = allocate_span(combined_root_len)` (a fresh, globally allocated
  LEG root region — slot 256 in this run, i.e. *outside* the leaf block `(400, 3664)`), and the
  vertex row's `stored_slots` keeps its **slab-era cover** (1312). `tree_mode_insert_edge` then
  writes the same stale cover with `degree = stored = 1151`. The audit reads `vertex.stored_slots`
  as the reservation, so the stale slab cover looks like a span reaching from 256 across the
  leaf's re-tiled mates.
  Two facts fall out, and ADR 0088 §3 already prescribes which way to fix them — "the
  rope/PMA/placement layers treat the root span exactly like a small slab bucket ... and stay
  mode-blind":
  1. `promote_bypass_to_tree_mode_impl` never re-bases the vertex cover on the resident-geometry
     SSOT (`bucket_physical_resident_slots(bucket)` = `combined_span_region_len` = `root_len`
     slots for tree, not `degree`). The published cover therefore describes a region the tree
     bucket does not own.
  2. The leaf-cover model itself (one `(base, cover)` interval per vertex, used by the audit, the
     slide, and release math) cannot represent a vertex whose tree root was allocated outside the
     leaf block while sibling slab buckets stay inside it. Per ADR 0088 §3 the root span is *not*
     a special case: it is an ordinary vertex-local span, so it must be reserved through the same
     vertex/leaf tiling the slab path uses, and the cover re-based on the resident length —
     rather than teaching the cover model an out-of-block representation.
  Also verified while tracing: the relocation slide re-tiles this leaf correctly
  (`TMPSLIDE`: hub `old2752+688 -> 2749+979`, later `-> 2752+1312`, inside the block), so the
  slide is not the writer; the guard's span is the *stale vertex cover*, not a moved span.
- **Minimal variants tried and rejected by measurement (2026-09-20, same commit):**
  (a) *re-base the vertex cover on `bucket_physical_resident_slots` only* — breaks two existing
  invariants, measured: `gap_tree_full_path_growth_past_5728_releases_only_owned_regions` and
  `tree_mode_leaf_actual_counts_slab_edges_only` fail with `leaf 0: PMA total mismatch
  (store vs labeled geometry) left: 1040 right: 16`, and
  `single_label_log_fold_reserves_edge_only_tail_headroom` fails on
  `vertex.stored_slots >= segment_size`. A cover is a vertex's *share of the leaf block* (resident
  plus distributed slack), not the bare resident sum, so shrinking it to the root region breaks
  `Σ covers == leaf total` and the headroom floor.
  (b) *relocate the leaf once after the promotion* (pinned or not) — with the shared
  position helper made tree-aware it fixes the reproduction, but the slide itself can fail
  (`CollectAllocationOverflow`) on other shapes (`directed_inline_property_adjacent_reverse_hub_stays_writable_after_skew`)
  and it writes the new block start before the per-vertex commit, so swallowing the error could
  leave a half-moved leaf; propagating it after the descriptor commit turns a successful promotion
  into an error. Not acceptable as the minimal variant.
  Conclusion: no small fix exists. The root must be reserved **inside the vertex's span** from the
  start (so the cover, the block tiling, and the release math all stay consistent by
  construction), which is the ADR-prescribed route below.
- **In-span reservation attempt (2026-09-20, implemented then reverted):** Phase 1 was rewritten to
  reserve the combined root region *inside* the vertex span (property sizing first; then
  `rewrite_vertex_edge_span(vid, Some(bucket_index), combined_root_len, …)`; root base =
  `bucket.edge_start() + stored_slots` after re-reading the bucket; every span-release rollback
  dropped, LTB releases kept). Outcome: the reproduction passes (the target defect is fixed), but
  three interactions remain, so the attempt was reverted to keep the tree green:
  1. the re-laid vertex span can straddle the leaf block end
     (`fold_growth_stays_mate_disjoint_across_span_growth`: hub cover `[3758, 5070)` against block
     `(256, …3920)`) — the vertex-span rewrite's out-of-block escape hatch
     (`tail_append_labeled_edge_base` while a relocation is in progress) or its sizing can exceed
     the block, which the cover/tiling invariants forbid;
  2. `batch_plan_with_mixed_slab_and_tree_runs_rejects_only_tree_run` fails with
     `CollectAllocationOverflow` (the pre-transcription rewrite can fail on batch-shaped fixtures);
  3. `demote_atomic_on_failure` trips `LtbRawBlockStore::release(0): block is already Free` —
     **resolved** (`d80504c2e`, test-only): the fixture's premise (the property restore reads an
     unminted id from whatever follows the edge root) was placement-dependent; the test now writes an
     explicit unminted sentinel and asserts the descriptor is byte-identical after the failed demote.
     Details in the original narrowing below, kept for the record.
     Narrowed by instrumentation (2026-09-20): `physical_depth` is 1, so the depth-2 interior walk
     is skipped; the two releases of block 0 come from the test's **two demote calls on a synthetic
     state** (`w = 4` patched without minting the matching LPB, bucket relocated to
     `edge_start = 4352`, `stored = 4096`). With the in-span change the *first* call gets past the
     property visit and completes Phase 5 (releases the leaf ids), so the second call re-releases
     them — i.e. the test's premise ("the first call fails before any release") is
     placement-dependent, not an invariant. Next attempt: decide whether the fixture should build a
     legitimate LPB (or patch the depth/width consistently) so demote atomicity is pinned on a state
     that does not depend on where the promotion placed the root.
  Re-run after `d80504c2e`: interaction 3 is gone; the remaining failures with the in-span change are
  (1) and (2), with (1) narrowed further — the straddling cover observed in
  `fold_growth_stays_mate_disjoint_across_span_growth` (`base=3758 end=5070 leaf=(256, 3664)`) still
  carries the **pre-promotion slab width** (1312), so it is not the `tail_append_labeled_edge_base`
  escape hatch. Instrumenting `commit_vertex_edge_span_layout` (the slide/rewrite commit) shows the
  hub is **not** among the vertices that path republishes in that workload (only vids 0..11 appear,
  their spans summing 612 slots inside the block), so the 1312 cover is written by another site.
  Instrumentation then cleared the fold-prelude suspect and found the real shape (2026-09-20):
  - `prepare_vertex_edge_span_for_overflow_log_fold`'s relocate branches are fine — the relocation's
    slide *does* republish the cover, and the printed state is consistent
    (`after-relocate vid=15 stored=1312 base=Some(2608) leaf=Some((256, 3664))`, i.e. the span ends
    exactly at the block end).
  - The inconsistency is **tree-bucket sizing inside the vertex-span layout**. After the promotion,
    `edge_start` is the LEG *root* offset (3758) while the vertex cover still carries the vertex's
    block share (1312), because the layout helpers size a tree bucket by its *logical* degree:
    `read_and_plan`'s `total_live` and `calculate_label_edge_span_positions_by_resident_slots`'s
    `resident`, plus the materialization slice length at `compact.rs:984` (a tree bucket has no slab
    rows there — reading `stored`-many rows panics with `range end index 4600 out of range for slice
    of length 8`). ADR 0088 §3 names the intended shape: promotion publishes the descriptor and then
    "releases the old edge span (via the vertex-span rewrite)", so the vertex is re-laid with the
    tree bucket contributing `bucket_physical_resident_slots` (its root region) — no Phase-1
    restructuring needed.
  - Working files from this pass: `/tmp/promote_postpublish_attempt.rs` (post-publish rewrite) and
    `/tmp/compact_treeprep_attempt.rs` (tree-aware planning + positions, which then hits the
    materialization panic).
- **Class-level reframing (2026-09-20) and the structural step:** every ①/② failure above is one bug
  class — *a mode-specific field (`degree`, `stored_slots`, `edge_start`) read inside the mode-blind
  geometry layer* (ADR 0088 §3 keeps that layer mode-blind; ADR 0096 §7 keeps mode decisions at the
  dispatch points and transitions). The structural fix is one SSOT, `bucket_resident_region()`
  (tiny → `None`, slab → `(edge_start, stored_slots_raw)`, tree → `(edge_start,
  combined_span_region_len)`), with `bucket_physical_resident_slots()` delegating to it, and the
  geometry sites expressed through it so the wrong read cannot be written: `read_and_plan`'s
  `total_live`, `calculate_label_edge_span_positions_by_resident_slots`'s `resident`,
  `label_buckets_allow_contiguous_slab_copy` ("every bucket's region *is* its slab prefix"), and the
  `try_labeled_vertex_edge_base_in_pinned_leaf` block-bound check (which was missing its *lower*
  bound). Implemented and verified green in isolation.
  Remaining piece, pinned precisely: `rewrite_vertex_edge_span`'s inline copy path (the one built
  from `per_bucket`/`buf`, not the `per_bucket_raw` path) cannot move a tree bucket's LEG root array,
  so the post-promotion re-lay still fails with `CollectAllocationOverflow` after four relocations.
  Next attempt: give that path the materializer's tree branch (move the root bytes by anchor), then
  add the promotion's Phase 3e (re-base the cover on the SSOT + `rewrite_vertex_edge_span(..., force_slack_grow = true)`)
  and the two regression tests. Working copies: `/tmp/ssot_{compact,promote,bucket,leaf_pin,graph}.rs`.
- **Space-first attempt (2026-09-20, C = reserve the root inside the vertex span):** Phase 1 now
  reserves `combined_root_len` through
  `rewrite_vertex_edge_span(vid, Some(bucket_index), combined_root_len, …)` and writes the root at
  `bucket.edge_start() + stored_slots`; the old prefix is released by the existing Phase 3c, so no
  space is forfeited (unlike the reuse variant). With the SSOT migrations, the tree-aware copy arms
  in both inline loops, and the lower bound in `try_labeled_vertex_edge_base_in_pinned_leaf`, the
  **skewed-leaf regression passes with C alone**.
  What C alone does not do: the vertex cover still measures the vertex's share from the *released
  prefix base*, while the tree bucket's `edge_start` is now the root — and the debug guard (and the
  layout) reconstruct the vertex's span from the first non-tiny bucket, so the reconstructed span
  straddles the block end (`base=3758 end=5070 leaf=(256, 3664)`). So C needs the cover normalized
  too; the model has nowhere to record the vertex's span start (the 29-byte descriptor has no spare
  field, and the row's reserved bits are too narrow).
  **Copy-free refinement (2026-09-20):** the prefix never needs to be *moved* — it is dead after the
  transcription, so the root can be written over the prefix's **head** (`edge_start` unchanged), which
  also keeps the vertex's span start (and therefore the cover arithmetic) exactly where it was; the
  single post-promotion layout pass then only *shrinks* the vertex's share to shed the dead tail
  (root bytes move by anchor, the vertex's other buckets shift down). Implemented that way
  (`/tmp/d_promote.rs`, `/tmp/d_compact.rs`): Phase 1 allocates nothing, Phase 3e re-bases the cover
  on the resident SSOT and re-lays.
  **Root cause of that failure (found 2026-09-20):** `calculate_label_edge_span_positions` (the
  degree-weighted positions helper used by the *planning* path) computes its slot budget as
  `Σ bucket.degree()`, so a promoted bucket demanded its full logical count of a span sized for its
  root region — `gaps = span_slots - effective_live` underflowed into `CollectAllocationOverflow`.
  With `effective_live` taken from the resident-region SSOT (weights stay label-degree-based, which
  `vertex_edge_span_rewrite_weights_slack_by_label_degree` pins), **both regression tests pass** with
  the copy-free shape.
  **Blast radius measured (13 tests, classified):**
  - *design contract change* (expected, re-express): `promote_edge_start_points_to_leg_offset`,
    `promote_pre_and_post_edge_start_differ`, `promote_publish_phase_atomic_descriptor_write`,
    `promote_succeeds_when_alloc_space_at_cap` (the root now reuses the prefix head, so
    `edge_start` is unchanged and no span is allocated — the "fresh LEG offset" proxies no longer
    hold).
  - *audit model* (needs the leaf-level slide, see below): `gap_tree_full_path_growth_past_5728_releases_only_owned_regions`
    and `tree_mode_leaf_actual_counts_slab_edges_only` fail with `leaf 0: PMA total mismatch (store
    vs labeled geometry) left: 1040` — shedding the prefix leaves its slots unassigned in the leaf,
    while the audit requires `Σ covers == leaf total`. The honest fix is to re-tile the **leaf**
    (slide / relocate) in Phase 3e instead of only the vertex, so the freed slots are redistributed
    to the leaf's other vertices.
  - *expectation shifts* (policy preserved, numbers move): `labeled_segment_relocate_reuses_free_span`,
    `vertex_edge_span_rewrite_weights_slack_by_label_degree`,
    `single_label_log_fold_reserves_edge_only_tail_headroom`.
  - *suspected real bugs* (data assertions, investigate before updating anything):
    `default_bypass_conversion_clears_vertex_edge_span_allocation` (`left: [TestEdge { target: 0 },
    TestEdge { target: 0 }]` — bypass rows read back as zeros) and
    `edge_inline_propertys_survive_rewrite_with_tombstones` (`left: [(0, 20), (0, 30)]` — property
    values zeroed); plus `cascade_at_2_20_plus_1_deepens` (`promote: CollectAllocationOverflow`) and
    the known ② `batch_plan_with_mixed_slab_and_tree_runs_rejects_only_tree_run`.
  Working copies of the passing shape: `/tmp/final_*.rs` (superseding `/tmp/d_*`).
  **Leaf-slide variant works (2026-09-20):** an earlier attempt at this looked broken because a
  script splice had landed Phase 3e *inside* Phase 1 (so the slide ran before the descriptor flip and
  the promotion looked like a no-op); rebuilding the function from the known-good shape
  (`/tmp/c_promote.rs`) with Phase 3e at the end — Phase 1 root-in-place, transcription, flip, release,
  then `rebalance_labeled_leaf_weighted_slide(vid)` — makes **both regression tests pass** and drops
  the suite to **603 passed / 9 failed** (from 13). The leaf re-tile absorbs the shed prefix slots into
  the neighbours' shares, which is exactly what fixes the audit-model pair
  (`gap_tree_full_path_growth_past_5728_…`, `tree_mode_leaf_actual_counts_slab_edges_only`), plus
  `cascade_at_2_20_plus_1_deepens` and `single_label_log_fold_reserves_edge_only_tail_headroom`.
  Remaining 9, classified: (i) *design contract, re-express* —
  `promote_edge_start_points_to_leg_offset`, `promote_pre_and_post_edge_start_differ`,
  `promote_publish_phase_atomic_descriptor_write`, `promote_succeeds_when_alloc_space_at_cap` (no fresh
  root span: `edge_start` is the prefix head, nothing is allocated); (ii) *policy/expectation review* —
  `labeled_segment_relocate_reuses_free_span`, `vertex_edge_span_rewrite_weights_slack_by_label_degree`;
  (iii) *data-assertion failures* — `default_bypass_conversion_clears_vertex_edge_span_allocation`
  (bypass rows read back as `TestEdge { target: 0 }`),
  `edge_inline_propertys_survive_rewrite_with_tombstones` (property values read back as zeros),
  `directed_inline_property_adjacent_reverse_hub_stays_writable_after_skew`
  (`CollectAllocationOverflow` in a schema path).
  **Bisect result (2026-09-20):** these three are *not* independent defects.
  Root cause pinned by three bisect steps: `promote.rs` alone is fine, `bucket.rs` alone is fine,
  `compact.rs` is the culprit, and within it the single change that reproduces
  `default_bypass_conversion_clears_vertex_edge_span_allocation` is `read_and_plan`'s `total_live`
  switching from `degree` to the resident SSOT. The reason is a missing dimension in the SSOT: a
  **compacting** rewrite materializes only the *live* rows (tombstones are packed away), so its span
  budget must stay `degree` for slab buckets — while a promoted bucket still needs its LEG root
  region. The SSOT as applied returns the tombstone-inclusive width, which over-reserves slab spans
  and shifts the published layout. Next step: make the budget compaction-aware
  (`bucket_rewrite_content_slots(bucket, compact)`: compact → slab `degree` / tree root / tiny 0;
  non-compact → the resident region) and re-run these three. Applying only the
  resident-region SSOT + planning positions fix (`79ca06e24`) keeps the whole suite green (611/0) and
  all three pass; they fail only once the promotion/leaf-tiling rework is applied on top, so they
  belong to that rework's blast radius and must be explained (or the rework corrected) rather than
  patched in place. Working copies of the rework: `/tmp/best_*.rs`; the SSOT increment is now
  committed.
  **Compaction-aware budget applied (2026-09-20):** `bucket_rewrite_content_slots(bucket, compact)`
  (compact → slab `degree` / tree root / tiny 0; non-compact → the resident region) next to the
  resident-region SSOT, wired into the planning sizing and the planning positions helper (weights
  stay label-degree-based). The rework's suite moves from 9 to **8 failures**:
  `default_bypass_conversion_clears_vertex_edge_span_allocation` is fixed. The two remaining
  data-assertion tests (`edge_inline_propertys_survive_rewrite_with_tombstones`,
  `directed_inline_property_adjacent_reverse_hub_stays_writable_after_skew`) are the rework's last
  unexplained piece; the other six are the four promotion contract tests and the two
  expectation/policy tests. Working copies: `/tmp/best2_*.rs`.
  Narrowed further (2026-09-20): in `edge_inline_propertys_survive_rewrite_with_tombstones` the
  **property values read back correctly** (20, 30) while the **edge targets read back as 0** — so the
  values slab is fine and the *edge* copy is what breaks. The call takes the **disjoint** branch
  (`moved && old_alloc > 0 && new_base != old_base`, `slab_only_bulk == false` because the bucket has a
  tombstone), i.e. the `per_bucket`-collecting loop: the collected edges are written at the published
  positions, so the next instrumentation target is the `collect_out_edges_slot_order` source against
  the positions the plan published (the pair that used to be derived from the same `degree` width; the
  compaction-aware budget changed one side of that contract).
  **Duplication removed (2026-09-20):** `rewrite_vertex_edge_span` had three copies of the
  collect-and-publish logic (two inline fast paths plus `commit_vertex_edge_span_layout`), which is
  what kept drifting. Replacing both inline paths with the materializer + commit pair
  (`materialize_labeled_vertex_edge_plan` → `commit_vertex_edge_span_layout`, the same code the leaf
  slide uses) deletes 268 lines and fixes `edge_inline_propertys_survive_rewrite_with_tombstones`
  outright. Combined with the compaction-aware budget and *without* the promotion rework the suite is
  **609 passed / 2 failed**, and both failures are the sizing/expectation pair
  (`labeled_segment_relocate_reuses_free_span`, `vertex_edge_span_rewrite_weights_slack_by_label_degree`):
  the unified path moves the bucket's whole slab prefix, so the span budget must cover the prefix
  (`stored`) rather than the live count (`degree`) — the old `degree` budget under-reserved, and those
  two tests pin the old numbers. Next step: re-derive those two expectations (with that justification)
  **But the two remaining failures are not expectation shifts** (checked 2026-09-20): with the unified
  code on HEAD, `labeled_segment_relocate_reuses_free_span` fails `assert_eq!(new_start, old_start)`
  with `left: 1280, right: 256` — the in-place leaf expansion no longer consumes the adjacent released
  span, so the relocation took a fresh block — and
  `vertex_edge_span_rewrite_weights_slack_by_label_degree` fails
  `cold_capacity >= cold_bucket.stored_slots()`, i.e. the published tiling gives the cold bucket less
  room than its own stored width. Both are invariants, so the unified path has a real behavioural
  delta to diagnose (candidates: the plan's compaction-aware budget versus the commit's
  resident-slot position helper disagreeing now that the inline fast paths are gone; or the
  materializer's `raw` for a log-free slab bucket being the *prefix* while the plan budgeted the
  packed run). Do **not** re-derive those two expectations before that is explained. Copies:
  `/tmp/unified_compact.rs` (unified layout code to pair with HEAD's other files).
  **Last duplicate found (2026-09-20):** `rebalance_vertex_edge_span` (~2487-2660) is a *fourth* span
  layout implementation — its own snapshot loop (`stored_slots_raw` per bucket), its own
  `calculate_label_edge_span_positions_by_resident_slots` call, its own publish loop
  (`write_label_bucket_row_adaptive`) and its own vertex-cover write (`with_stored_slots(new_alloc)`).
  Test 2 calls exactly this path, which is why instrumenting `commit_vertex_edge_span_layout` printed
  nothing for it. Consolidation therefore has one more step beyond the rewrite: route
  `rebalance_vertex_edge_span` through the same plan + materialize + commit pair (or delete it in
  favour of the rewrite it duplicates), then re-check the two invariants — only then is the
  expectation question meaningful. Copies: `/tmp/unified_compact.rs`.
  **Delegation attempt (2026-09-20):** replacing `rebalance_vertex_edge_span`'s body with a call to
  `rewrite_vertex_edge_span` (deleting 137 more lines) breaks **24** tests — the two functions do not
  share semantics yet: the rebalance path must not release the vertex footprint
  (`labeled_leaf_rebalance_does_not_release_span`), it folds overflow logs
  (`labeled_leaf_rebalance_folds_overflow_log`), and several batch/deferred/inline-property tests
  depend on its ordering. So the last consolidation needs an explicit semantics decision per caller
  (footprint release, log folding, ordering) with that 24-test list as the worklist; the direction is
  still right (one layout implementation), but it is a slice of its own. Copies:
  `/tmp/delegated_compact.rs`.
  (`old_alloc=2 new_alloc=18 old_base=2608 new_base=2608 moved=true leaf=(256, 3664)`, all in-block),
  and the error is raised **before `commit_vertex_edge_span_layout` reaches its positions step** —
  i.e. inside `rewrite_vertex_edge_span`'s non-disjoint inline branch (the one that builds
  `per_bucket`/`raw`). Next instrumentation target is that branch (its `slab_only_bulk` predicate
  evaluation, `per_bucket` collection, and `raw` sizing), not the planning or the commit. Working copies: `/tmp/c_{compact,promote,bucket,leaf_pin,graph}.rs` (C + normalization) and
  the same files with C alone is `/tmp/ssot_*`.
  For reference, the only *self-consistent and complete* variant today is the root-at-prefix-base
  reuse (B): it passes every geometry test, but holds up to `T_PROMOTE` slots (4 KiB) per promoted
  bucket as claimed slack until the leaf is relocated, which is exactly the space cost this
  requirement rejects.
  Working copy saved at `/tmp/promote_inspan_attempt.rs`; regression test at
  `/tmp/tree_promo_test.rs` (`tree_promotion_leaves_the_vertex_cover_tiled_with_its_leaf`, fails on
  HEAD with the exact guard message).
- **Prescribed fix (ADR 0088 §3, no design decision left):** the tree promotion must reserve the
  combined root region through the vertex/leaf tiling (the same `rewrite_vertex_edge_span` /
  leaf-placement path slab growth uses) instead of a bare `edges.allocate_span`, and must then
  publish the vertex cover from the resident length (`bucket_physical_resident_slots`) so the
  vertex's `(base, cover)` describes exactly the region the tree bucket owns. Watch items while
  implementing: the promotion's rollback path currently releases the root region with
  `release_span(new_edge_start, combined_root_len)` (must follow the tiling's owner), the
  in-pinned-leaf release caveat documented at Phase 3c, and `force_tree_mode_for_test`'s
  `force_bucket_to_stored_slots` fixture which prepares the slab prefix.
  Then add the skewed-leaf regression (256 vertices, degrees 1..2048, segment 16) — the reverted
  attempt already has one at `/tmp/tree_promo_test.rs` — re-run the skewed-leaf audit, and resume
  the production balance evaluation. Budget the three interactions listed above as part of the
  slice: they are the reason this is not a drive-by patch. Until then, treat the
  K=4 boundary flip (`91575bc29`) as needing this follow-up before release.

### GAP-2026-09-20-004 — PMA counts tree maintained eagerly with no runtime reader

- **Status:** Fixed 2026-09-20 (commit `f9b276b6e`) — recorded the same day while
  looking for the next lever after the drain/property fixes. Not a threshold issue:
  it affects every slab insert/remove in every arm.
- **Observed behavior (confirmed):** `bump_counts_leaf_with_layout` propagated each
  `actual`/`total` delta up the segment tree with one read + write per level
  (~2.3 K instructions per insert on a 15-level tree). The S1 probe had already
  measured this walk at **56 % of the attributed labeled append cost**
  ([investigation §S1](investigations/2026-09-17-lara-improvement-investigation.md)),
  but every runtime consumer reads the **leaf** row only: the density decision
  (`leaf_segment_counts_for_vid`), the layout audit (`assert_labeled_edge_store_pma_counts`),
  and the leaf cascade. The internal rows had no reader outside the walk itself and
  the segment-growth rebuild — verified by enumerating every `counts.get`/`counts_store().get`
  site (all leaf-indexed except the walk's own read-modify-write, the growth
  migration, and two test seed helpers that recompute the tree inline).
- **Fix:** leaf rows are canonical. The hot path applies its delta to the leaf row
  only; `EdgeStore::rebuild_counts_internal_nodes` (exposed on the labeled graph,
  body shared with the segment-growth migration) recomputes the internal rows from
  the leaves where an aggregate is needed.
- **Tests:** `counts_internal_nodes_are_derived_and_repaired_on_demand` — leaves stay
  exact, the internal root is *stale* before a rebuild (the wrong-impl probe: eager
  maintenance would already match), the rebuild restores every internal row from its
  children, and a further mutation leaves the root stale again.
- **Measured (T_promote = 1024, artifact pre/post of the same persist run):**
  `bench_r_ed_st_si_1024` (1,024 scattered slab appends) 4.58 M → 2.17 M (−52.6 %);
  `bench_l_s2_det_hub_1024` 25.01 M → 17.88 M (−28.5 %, now below both the pre-F1
  20.72 M and the pre-re-tune slab reading); `bench_l_nt_bp_ins_4096` 415.6 K →
  327.9 K (−21.1 %); `bench_l_s2_det_hub_st_4096` −16.8 %;
  `tiny_workload_skewed_mix` (G5) 122.11 M → 115.49 M (−5.4 %). The persist run
  reported 13 improved / 0 regressed / 178 unchanged. Single-leaf growth shapes
  (M1) are unaffected — their walk hits one shallow path, so the total does not
  move. Note: G5's 132.24 M reading in the earlier A/B session predates the drain
  batch and the property cursor, and cross-build canbench comparisons carry a few
  percent of code-layout variance, so only same-run numbers are quoted as deltas.
- **Contract:** [lara-dgap-contract](storage/lara-dgap-contract.md) and
  [lara](storage/lara.md) now state leaf-canonical counts with on-demand internal
  repair (DGAP maintains the tree eagerly; the density semantics are unchanged).

### GAP-2026-09-20-002 — Emptied-bucket span release costs ~26K per call (drain paths)

- **Status:** Fixed 2026-09-20 (partials `914734443`, A2 `a021a309c`) — measured
  performance defect, recorded 2026-09-20. Found while attributing the
  `canbench --persist` verdict for the `T_promote = 1024` re-tune; it is **not**
  threshold-dependent (identical bench totals at 1024 and 4096) and predates the
  re-tune.
- **Observed behavior (confirmed):** F1 (`3fd14768b`) releases an emptied slab
  bucket's span immediately (`release_bucket_edge_span_on_empty`). A detach-delete
  drain empties every neighbour's 1-edge bucket, so `bench_l_s2_det_hub_1024`
  performs 1024 such releases. The pre-F1 artifact value for the same bench is
  20.72 M (no per-bucket release); the value at measurement time was 51.31 M.
  Uninstrumented attribution (native pattern bench
  `fs_drain_release_pattern_1024` + whole-`release()` ablations; the first,
  heavily-scoped probe run over-reported the lookups and is superseded): a
  drain-shaped release costs ~34 K, of which the dup/prev/next lookups are only
  ~0.6 K and the rest is the free-span store's stable-memory writes (~1.3 K per
  write, ~20 writes on the double-merge path), plus ~4 K for the cover
  re-read/sync in the labeled path.
- **Evidence:** [regime-cost investigation](investigations/2026-09-20-tree-regime-cost-improvements.md)
  §Finding A (probe table: `tmp_release_empty` 30.88 M / 1025 calls,
  `tmp_fs_release` 26.73 M, `tmp_cover_recompute` 4.41 M, `tmp_counts_dec`
  9.28 M over 2048 calls); bench A/B at both thresholds (57.09 M each) rules out
  the re-tune; `git merge-base` shows the artifact baseline predates F1.
- **Impact:** every drain that empties many buckets (detach delete, vertex
  delete, churn) pays ~26 K per emptied bucket inside the free-span store; the
  cost is per release, so a 4096-edge hub pays ~4× the 1024-edge one. It also
  explains why the re-tune looked responsible for the drain regression.
- **Owner:** labeled delete path (`labeled/graph/remove.rs`,
  `release_bucket_edge_span_on_empty`) plus the free-span store
  (`lara/edge/free_span.rs`, `release`/`insert_span`). The reclaim owner in the
  replacement design must keep the F1 guarantee (no phantom occupancy after a
  delete) and the GAP-2026-09-17-001 double-free protection.
- **Landed partials (2026-09-20, commit `914734443`):** `release()` needs two
  page-walking lookups instead of four (`predecessor_or_equal` added to
  `ic-stable-paged-ordered-map`; `successor` now covers adjacency *and*
  next-overlap, which also closes a latent hole where a span strictly inside the
  released range was ignored when another span started exactly at the range end
  — regression `release_prefers_inner_overlap_over_adjacent_merge`), and
  `write_record` writes the 48-byte record in one write instead of six.
  Measured: native pattern bench 35.34 M → 33.41 M; `bench_l_s2_det_hub_1024`
  51.31 M → 50.03 M (scoped removal 36.57 M → 35.38 M).
- **A2 landed (2026-09-20, commit `a021a309c`):** the synchronous detach delete opens
  one span-release batch per orientation (`begin_span_release_batch`), emptied
  buckets record their ranges (the per-delete cover sync still runs), and
  `flush_span_release_batch` releases them as merged runs before the delete
  returns — so F1's reuse contract holds at the operation boundary and the store
  sees one insert per contiguous group instead of one per emptied bucket. Tiny and
  tree buckets stay excluded (no span / LTB-addressed). Regression:
  `detach_delete_flushes_emptied_spans_as_merged_runs` (asserts flush runs equal
  the pre-computed merged-run count and that every freed range is allocatable
  afterwards; wrong-impl probe: disabling the batch yields 0 vs 8 runs). Measured:
  `bench_l_s2_det_hub_1024` 51.31 M → 25.01 M (−51 %), `..._4096` 208.00 M →
  102.59 M (−51 %), stepped variant unchanged (−2 %, one release per step by
  design), `..._sat_4096` unchanged.
- **Residual evaluated, not pursued (A3, 2026-09-20):** with the batch in place
  the drain bench sits 4.3 M above the pre-F1 20.72 M, and that difference
  matches F1's *necessary* per-delete cover sync (~4.2 K × 1024 emptied buckets) —
  the per-flushed-run store write cost is only ~1 M of it. Store-level write
  batching therefore has a ceiling of roughly 4 % on this shape; kept as
  `fs_drain_release_pattern_1024` (de-benched) if a release-heavy workload ever
  needs it. The drain is now threshold-independent: both `bench_l_s2_det_hub_*`
  benches measure identically at `T_promote` 1024 and 4096.

### GAP-2026-09-20-003 — Tree property reads resolve the property leaf per row

- **Status:** Fixed 2026-09-20 (commit `cfd389cad`) — measured performance
  defect introduced as a *cost* by the `T_promote = 1024` re-tune
  (property-bearing buckets above 1024 rows are tree now). Fix: the property
  walk caches the leaf's LTB block (`PropertyLeafCache`), resolving and reading
  it once per leaf and serving rows from the buffer in both walk orders
  (`tree_read.rs`); `tcsr_4096_property_read_w32` 4.83 M → 3.66 M at
  `T_promote = 1024` (slab reference 3.22 M, so the residual tree overhead is
  +13.7 % instead of +50 %). Pinned deterministically by
  `tree_property_scan_reads_each_payload_block_once`, which counts LTB payload
  read calls over a promoted 4096-row w = 32 bucket and requires exactly
  `4 + 32 = 36` (edge blocks + property leaves) in both orders — the pre-fix
  shape fails it with 4100.
- **Observed behavior (confirmed):** `visit_edges_with_inline_property`'s tree
  path reads each edge block once but calls `read_property_value_at_slot` per
  row, which re-resolves the property leaf (`resolve_property_leaf_block_id`:
  a LEG root read plus offset math) and issues a separate 4-byte LTB read per
  row. At w = 32 (K = 128 rows per property leaf) a 4096-row scan pays 4096 root
  reads + 4096 partial reads instead of 32 block reads:
  `tcsr_4096_property_read_w32` 3.22 M (slab) → 4.83 M (tree), +50 %.
- **Evidence:** [regime-cost investigation](investigations/2026-09-20-tree-regime-cost-improvements.md)
  §Finding B; same-bench A/B at `T_promote` 4096 vs 1024 with all other code held
  constant.
- **Impact:** property-bearing scans over buckets above `T_promote` rows pay the
  per-row resolution; w = 0 scans are unaffected (M2a improves in tree mode).
- **Owner:** `labeled/graph/tree_read.rs` property-bearing scan loops
  (ascending/descending) + `read_property_value_at_slot`.
- **Next decision:** none for the per-row resolution itself. The residual
  +0.44 M over the slab reference is the tree property indirection (one leaf
  resolution plus one 4 KiB block read per leaf, per-row closure value clone);
  revisit only if a property-scan-heavy workload makes it material. The
  deterministic test also gives the LTB store a reusable payload-read counter
  (`ltb_payload_read_calls`) for future block-granularity contracts.

### GAP-2026-09-20-001 — Promote silently drops overflow-log rows (tree mode has no log)

- **Status:** Fixed 2026-09-20 (commit `164e27cfa`) — recorded the same day while
  triaging the `T_PROMOTE = 1024` flip (found by classifying the 11 failures that
  appear at the lower threshold; see GAP-2026-09-17-001 for the density-accounting
  context). Fix: promotion folds the slab overflow log into the prefix first
  (`ensure_label_bucket_folded_to_slab`, with the vertex-span rewrite retry), then
  re-locates the descriptor and transcribes the folded prefix; a fold that cannot
  make room fails closed and leaves the slab bucket and its log intact. This is
  the route ADR 0088 §7's transcription clause allows and §6 ("promotion always
  folds") names; the rejected alternative was interleaving log entries directly
  during transcription (larger, and duplicates the fold's ordering rules).
  `assert_labeled_layout_invariants` now enforces the tree wire rules
  (`overflow_log_head < 0`, `degree <= stored_slots`) and sizes a tree bucket's
  on-slab range from `bucket_physical_resident_slots` (the logical width had
  false-positived against the LEG capacity). Cost: `tcsr_4096_insert_grow` 69.92M
  instructions, no change vs the persisted artifact (the fold runs only when a log
  is active).
- **Observed behavior (confirmed):** `promote_bypass_to_tree_mode` transcribes
  exactly `pre_stored_slots` prefix slots into the LTB blocks and publishes
  `overflow_log_head = -1` ("the log was orphaned by promote"). Any live row the
  slab **overflow log** holds at the trigger is therefore dropped — tree mode has
  no log (ADR 0088 §2) — and the row becomes unreachable. Post-promotion the
  bucket reports `degree = stored_slots + N` (N = log depth at the trigger) while
  every scan (forward and reverse) returns `stored_slots` rows. The inserts that
  admitted those rows returned `Ok` and `num_edges` counts them: fail-open.
- **Evidence / repro:** temporary probe (removed after diagnosis) seeding a
  width-2 `InlinePropertyTestEdge` hub through the production
  `insert_directed_edge` path on `valued_bidirectional_graph()`:
  - `T_PROMOTE = 1024`: promotion fires at insert 1191 →
    `stored 1191, degree 1192, out/in scans 1191`.
  - `T_PROMOTE = 4096`: promotion fires at insert 4251 →
    `stored 4251, degree 4252, out/in scans 4251` → **threshold-independent**.
  - width-0 control: exact (`stored == degree == scanned` at 4097/5000/8000);
    the trigger is a *non-empty overflow log*, which the width>0 path reliably
    produces because the edge prefix plateaus (`stored 4080` while `degree 4200`,
    `log_head 119`) while the property stream keeps growing.
  - Regression: `gap_promote_with_active_overflow_log_keeps_every_row` (fails
    before the fix with `left: 4999, right: 5000`; with the audit swapped in it
    fails with `vertex 2 bucket 256: tree degree 5000 exceeds stored width 4999`).
    The lower-threshold test `directed_inline_property_adjacent_reverse_hub_stays_writable_after_skew`
    (`in_edges_for_label(..).len() == 2000` → 1999) exposed it originally.
  - No detector fires today: `assert_labeled_edge_store_pma_counts` compares
    `actual` against bucket degrees (both agree on the inflated degree), and the
    tree block-header parity helper is test-local.
- **Impact:** silent adjacency loss for any promoted bucket that carried
  log-resident rows. Promotion frequency rises as the threshold falls, so the
  `T_PROMOTE = 1024` flip multiplies the exposure; the flip is gated on this fix.
- **Owner:** tree promotion transcription (`labeled/graph/promote.rs` Phase 2/3)
  plus overflow-log ownership in the labeled insert path
  (`labeled/graph/insert.rs`; the slab path's fold helper
  `fold_label_bucket_*_log_to_slab` already exists). Contracts: ADR 0088 §2
  (tree mode has no overflow log), ADR 0096 §5 (log rows are PMA edge records).
- **Next decision:** smallest fix — fold the overflow log into the prefix before
  promoting (fold → re-check trigger → promote, reusing the existing fold path),
  versus a typed fail-closed precondition on `overflow_log_head() >= 0` that makes
  the caller fold-then-retry, versus transcribing prefix+log rows into the tree
  (larger). Regression: promote a bucket with a non-empty log, then assert
  `degree == stored == scanned` for both directions plus payload identity.

### GAP-2026-09-17-001 — Tree-mode full-path 4-byte bucket growth traps past ~5.7K edges on an overlapping edge free-span release

- **Status:** Fixed 2026-09-19 (commits `c664d8f73`, `c779ced27`, `0e2ac456a`,
  `a86ff0f50`; density-accounting half `8a6d73925` + audit coverage `795754dca`) —
  root cause was a unit confusion shared by SIX paths, not one call site.
  Every resident-geometry computation sized tree buckets on the logical
  `stored_slots` edge count while the physical LEG span is only the root region
  (`combined_span_region_len`):
  (1) `release_vertex_edge_span_footprint`'s monolithic whole-cover
  `release_span(span_start, span_len)` handed unowned ranges to the free store
  (the observed `OverlapPrevious` trap); (2) `plan_labeled_leaf_relocation`
  over-allocated the leaf block (5728 slots for a 6-slot root), which is what made
  the whole-cover release overlap live free spans; (3) slide tiling, (4) rebalance
  planning, and (5) fold/grow-footprint planning shared the same logical-width
  sizing; (6) `materialize_labeled_vertex_edge_plan` shared it on the READ side,
  snapshotting tree buckets as `stored_slots` slab slots from `edge_start`,
  walking past the root region into live ranges and republishing those bytes as
  the bucket's new span on relocate (corruption: post-relocate scan returned 1
  edge instead of 8192 — no trap, so fail-closed never fired). The fix introduces
  `bucket_physical_resident_slots` (compact.rs) as the single source of truth —
  tiny 0, slab `stored_slots`, tree `combined_span_region_len` (log-chain slots
  still added by callers) — routes all six paths through it (plus the commit
  rebuild, which preserves tree descriptors' logical width: root bytes move by
  anchor only), removes the whole-cover release so footprints retire as mode-aware
  bucket regions plus remainder only, and unifies the retire-interval filter/width
  and live-guard on the same helper (commit `0e2ac456a`; value-identical, no
  behavior change). Standing regression:
  `gap_tree_full_path_growth_past_5728_releases_only_owned_regions` (compact.rs
  tests; M1 shape 0→8192 full-path inserts, asserts tree mode, root-region width,
  + full adjacency; bisect checkpoints at every 512 inserts all 1:1).
  Threshold A/B completed 2026-09-19 on the fixed path (unpersisted runs, both
  arms; T_PROMOTE reverted to 4096 after measuring — production unchanged):
  M1 hub-grow 82.30M (4096) vs 92.89M (1024, ~13% worse); M2a scan 74.30K slab vs
  40.09K tree (~1.9x tree-favored); M2b insert 8,364 slab vs 185.07K tree (~22x
  tree append cost); M2c delete 4,845 vs 7,887; M4 churn 147.54M vs 38.64M
  (threshold-relative sizing artifact — the 1024 round-trip does less work by
  definition, not evidence of threshold merit). Verdict at the time: the M2a
  tree-scan win is outweighed at workload scale (M1); **T_PROMOTE stays 4096**
  (evidence-backed, not freeze-by-default). **Superseded** by the
  density-accounting fix below: post-fix M1 inverts (1024 25% cheaper on equal
  work).
- **G4/G5 follow-up (2026-09-19, commit `a5cefbf69`):** G4 (`tiny_relocate_mixed_leaf`)
  and G5 (`tiny_workload_skewed_mix`) benches landed; both green unpersisted (G4 ~594K
  ins, HI=0/SMI=0 with payload-identity + per-neighbor scan asserts; G5 ~140M ins for
  256v/4520e with exact census assert). Two discoveries: (1) a tree bucket co-resident
  with pinned mates on one vertex is unseedable in quota-1 leaf geometry BOTH ways
  (tree-first stalls mate tiles at the 8th edge via the leaf-mate overlap assertion;
  mates-first stalls the tree growth cascade at ~4703-4895 edges) — tree-under-relocate
  stays covered by the M1 regression instead; (2) the G4 probe exposed a stale ADR
  expectation: tiny anchors ADVANCE on relocate (running-boundary stamps per the
  Successor-chain row), so G4 asserts payload identity, not anchor identity.
- **T=1024 follow-up (2026-09-19, commit `a86ff0f50`):** the threshold A/B's T=1024
  arm trapped M1 at edge 2063 with `DuplicateStart` on `release_span(1048579, 2064)`
  — a SECOND double-free of the same family, at a different site: the tree
  combined-span realloc (w==0, no avoid) re-took a freed head `[1048579,2)` for the
  live root, and the next leaf relocate released the stale whole cover over it
  (plus the symmetric tree step-7 old-span release). Both whole-cover releases now
  delegate to `release_vertex_edge_span_slab` (skip-free-prefix + probe): live
  covers release whole (identical behavior), recycled heads release only the owned
  remainder. Verified: T=1024 diagnostic 0→8192 full-path growth green with full
  adjacency. Rejected alternatives: `allocate_span_avoiding` re-release removal
  (leaks the taken span as unaccounted padding — the range stays reserved but
  untracked); free-store-overlap guard at the leaf-release choke point (wrong layer:
  legitimately-abandoned covers routinely overlap recycled ranges after slide
  republishes spans — the guard would convert every such release into a leak);
  old/new-block overlap check at the relocate call site (necessary but insufficient:
  the overlap is with the NEW block's recycled free tail, not the old cover).
- **Second-stage hardening (evaluated 2026-09-19, NOT pursued):** a `SpanExtent`
  enum (slab/tree unit separation at the type level) was scored highest (8/10) but
  rejected on implementation review — the three release call sites correctly pass a
  *logical cover* (mixed-mode vertex width), so forcing physical widths at the call
  boundary contradicts the valid calling convention; the remaining divergence was
  mechanical and is now unified (see above). NOTE (precision, 2026-09-19): the
  SpanExtent score applies to the ORIGINAL gap (logical/physical unit confusion)
  only — it does not address the T=1024 follow-up (temporal ownership overlap),
  which no type can capture. A dedicated `TreeSpanLengthMismatch`
  error variant was prototyped and reverted: the code no longer constructs the
  failure condition, so the variant would be unreachable (YAGNI); the free-store
  `OverlapPrevious` tripwire already covers this class (proven: it caught the
  original trap). The dual meaning of `vertex.stored_slots` (logical cover vs
  physical width) remains and is *not* what plan 0361's R4 addressed: R4
  privatized the **bucket** descriptor's `stored_slots` (mode-aware accessor,
  landed 2026-09-20 `15719602c`); the vertex-level cover/resident split is owned by
  the resident-geometry SSOT introduced here (`bucket_physical_resident_slots`)
  and stays a documented convention rather than a type split.
- **Future direction (proportional trigger: a THIRD same-family double-free):**
  an arena rule — while a leaf block is pinned, defer per-bucket sub-releases
  inside it and release only whole abandoned blocks — would make recurrence
  structurally impossible (E1=5) instead of guarded per site (current: E1=4).
  Cost: pin-liveness checks on every sub-release path, new state + invariant
  design (separate slice). The current slab-fallback delegation bounds the
  residual hole (stale covers skip free ranges; live spans are republished
  before old-cover release per post-slide invariant, pinned by M1/G4/G5/suite).
  Do NOT pursue before a third firing — disproportionate until then.
- **Density-accounting follow-up (2026-09-20, this slice):** the second half of the
  same trap. Tree inserts bumped leaf `actual` (+1 per insert, −1 per remove) even
  though tree rows live in LTB blocks and occupy only the root region, and the
  promote did not subtract the slab-era degree; the leaf audit
  (`expected_vertex_pma_contribution`) counted tree `degree()` too, so the
  accounting stayed self-consistent while driving density ≥ 1.0 after every
  promotion — each post-promotion insert fired
  `rebalance_cascade_after_labeled_mutation` (M2b: 185.07K vs 8.36K; M1: 92.89M
  vs 82.30M at T=1024). Fix: `actual` now means *live edge records that occupy
  edge-slab slots*. Tree insert/remove helpers no longer take a vertex id
  (structurally unable to touch per-vertex counts); `promote_bypass_to_tree_mode`
  subtracts the live degree (`−degree`, LARA parity `segment_actual[leaf] -=
  live`); `tree_mode_demote_to_slab` re-adds it (`+degree`, mirroring
  `promote_tiny_to_slab`); the leaf audit and the batch `RunDestination::Tree`
  commit skip the bump. Tests:
  `tree_mode_leaf_actual_counts_slab_edges_only` (production insert/remove/
  promote/demote paths, exact leaf counts + audit at every step) and the tree-run
  assertion in `batch_run_admits_tree_mode_bucket_tail_fit`;
  `force_bucket_to_stored_slots` now seeds the matching leaf count so fixture
  states audit cleanly. The M1 growth regression
  (`gap_tree_full_path_growth_past_5728_releases_only_owned_regions`) now
  also asserts leaf `actual == 0` after 8192 full-path inserts and runs the
  leaf audit, so the rule is pinned across every relocate/slide/rebuild step
  of the growth chain, not only at the transition sites. Wrong-impl probes (re-added insert bump, removed promote
  subtract, removed demote re-add, restored batch bump) each fail those tests.
  ADR 0096 §5 updated; ADR 0088 §3 clarified (root *span* residency stays
  mode-blind; tree *edge rows* never count).
- **Threshold verdict — SUPERSEDED by the density-accounting fix (same session,
  2026-09-20).** Re-measured both arms with the same unpersisted `thresh_*`
  benches after the fix (`T_PROMOTE` reverted to 4096 after measuring; production
  constant unchanged):

  | Metric | T=4096 | T=1024 | Pre-fix |
  | --- | --- | --- | --- |
  | M1 hub-grow 8192 (equal work) | 55.87M | **41.70M** | 82.30M / 92.89M |
  | M2a scan 2048 | 74.30K | **40.09K** | 74.30K / 40.09K |
  | M2b insert into 2048 (block-boundary mint) | **8.36K** | 63.51K | 8,364 / 185.07K |
  | M2b insert into 2050 (steady-state append) | 8.36K | **4.46K** | not measured |
  | M2c delete 2048 | **4.84K** | 7.33K | 4,845 / 7,887 |
  | M4 churn round-trip (threshold-relative sizing) | 117.65M | 30.77M | 147.54M / 38.64M |
  | G5 skewed mix 256v/4520e (workload level) | 140.11M | **132.24M** | not measured |

  Both arms improved on M1 (the false cascade is gone) and the **ordering
  inverts**: 1024 is now 25% cheaper for equal work (8192 full-path inserts), so
  the pre-fix rationale for keeping 4096 no longer holds. M2b's residual 7.6× is
  a block-boundary mint (2048 → 2049 mints an LTB block, grows the root, reallocs
  the combined span), not a cascade: with the seed moved off the block boundary
  (2050) the same append costs 4.46K in the tree arm vs 8.36K in the slab arm, so
  tree appends are ~1.9x cheaper in steady state and the earlier 7.6x was the
  one-off mint. The insert/scan-dominated G5 workload mix also favors 1024
  (132.24M vs 140.11M, -5.6%). Every equal-work and workload-level metric now
  favors 1024; only single deletes (7.33K vs 4.84K) and boundary mints favor
  4096, and hub-delete streams are not a workload the Orkut comparison
  exercises. **Gated:** the flip is also blocked by
  [GAP-2026-09-20-001](#gap-2026-09-20-001--promote-silently-drops-overflow-log-rows-tree-mode-has-no-log)
  (promotion silently drops overflow-log rows, and a lower threshold multiplies
  the number of promotions). The drain batches landed later the same day
  (`a021a309c`) make the delete side threshold-independent, so the cost record
  is now property-scan indirection (+13.7 %) and the bounded boundary mint only.
  **Decision taken (2026-09-20):** the flip was executed
  once the gate cleared — `T_PROMOTE = 1024` / `T_DEMOTE = 512` (commit `2b417059f`),
  with the ten threshold-coupled tests migrated to derive their sizes from
  `T_PROMOTE` so the suite is green at both constants (602/0 each way). The density
  fix is threshold-agnostic; the flip only changes where promotion happens.
  Persisted artifact re-measured the same day (unfiltered `canbench --persist`,
  191 benches, 11 regressed / 5 improved / 3 new). Attribution was corrected by
  measurement (2026-09-20, temporary scopes; see the
  [regime-cost investigation](investigations/2026-09-20-tree-regime-cost-improvements.md)):
  only the property-bearing scan `tcsr_4096_property_read_w32` 3.22M → 4.83M and
  the once-per-B-rows block-boundary mint are re-tune costs. The hub-drain
  regression (`bench_l_s2_det_hub_1024` 20.72M → 51.31M, `..._4096`
  85.49M → 208.00M) is **threshold-independent** and comes from F1's
  per-emptied-bucket span release (`3fd14768b`) — see GAP-2026-09-20-002 — so the
  persist commit `da852423d` message misattributes it. `bench_remove_churn_*`
  scope growth is attribution only (totals +2%). Revisit trigger: a
  property-scan-heavy target workload re-opens the threshold (2,048 keeps
  4,096-row property buckets on the slab side).
- **Observed behavior (confirmed):** full-path `insert_edge` (impl + dense-check +
  cascade) on a single-vertex/single-label `LabeledLaraGraph` with 4-byte edges
  (Insertion policy), growing 0 → 8192, traps deterministically at the 5728th edge:
  `Store(RebalanceFailed(GrowFailed { current_size: 0, delta: 0 }))` with the bucket
  tree-mode, `stored = degree = 5728`, `log_head = -1`. Isolation worktree at detached
  HEAD `95d791087` (no in-flight changes) fails identically — latent HEAD bug, not an
  in-flight regression. Throwaway tracing in the isolation worktree shows an EDGE-slab
  `release_span start=1063168 len=5728` rejected with `OverlapPrevious { previous:
  FreeSpan { start_slot: 1059545, len: 3628 }, inserted: FreeSpan { start_slot: 1063168,
  len: 5728 } }` — overlapping exactly the 5 slots of an earlier `len=5` release at the
  same start. A `stored_slots`-scale (5728) release is issued from the tree growth
  regime where a root-region-scale (or no) release belongs.
- **Observability note:** the surfacing error misdirects. `impl From<GrowFailed> for
  LabeledOperationError` (`crates/ic-stable-lara/src/labeled/graph/error.rs:336`)
  wraps ANY `GrowFailed` as `Store(RebalanceFailed(..))`; instrumenting the only
  direct `RebalanceFailed` constructor (`labeled/bucket_store.rs:478`) never fired.
  The fault is in the edge-slab free-span path (`lara/edge/span.rs`), reached via the
  generic `From` conversion. Consider distinct error mapping as a follow-up; not part
  of this gap's fix.
- **Expected or needed behavior:** tree growth to the structural cap (2^30, ADR 0088)
  via the production path; release lengths in root-region units, never
  `stored_slots`-scale overlapping ranges.
- **Owner:** labeled tree growth + PMA accounting: `tree_write.rs` release sites,
  `compact.rs` leaf-relocate footprint sizing, per-insert counts bumps in
  `labeled/graph/insert.rs:495,814` (the same ownership the Tree-CSR workstream
  had; that workstream landed in `main` before this entry was closed).
- **Evidence:** [2026-09-17 investigation, §A result](../investigations/2026-09-17-lara-improvement-investigation.md);
  repro shape `thresh_hub_grow_8192` (M1 bench, worktree `labeled/bench.rs`);
  native repro `head_repro_full_path_growth` in the kept isolation worktree
  `/tmp/gleaph-head` (detached HEAD + throwaway eprintln tracing in
  `bucket_store.rs`/`span.rs`): `cargo test -p ic-stable-lara --features canbench
  --lib head_repro`. Unit-level promote paths are healthy (23/23 promote tests pass) —
  the failure is steady-growth + cascade interplay, untested until now (4B benches
  seed via `skip_leaf_cascade`; full-path growth benches are 10B-only and never promote).
- **Impact:** tree mode is unusable past ~5.7K edges via the production insert path;
  threshold A/B (1024 vs 4096) cannot run; S5 adoption is blocked.
- **Next decision:** answered (2026-09-20) — tree-mode inserts did bump leaf
  `actual`, and `release_labeled_leaf_physical_footprint` was sized by a
  slab-unit footprint; both halves are fixed (see the follow-up bullets above).
  Remaining decision tracked there: whether to adopt `T_PROMOTE = 1024` now that
  the post-fix evidence favors it at workload scale.

### GAP-2026-09-12-001 — Exact-vertex bulk updates do not support EXISTS-chain policies

- **Status:** Open — deferred capability, recorded 2026-09-12.
- **Observed behavior:** Fixed-vertex SET/REMOVE admits a labeled NodeScan and property residuals.
  EXISTS policy lowering needs SemiApply/reverse-seed joins, outside that Graph input contract.
  Router deliberately rejects applicable target-label chain policies before scalar reservation
  or Graph dispatch with `NotImplemented("bulk vertex updates with EXISTS policies are not supported")`.
  Property-only conditional grants, including indexed equality, are supported. The policy is never
  ignored, and the rejected row leaves no scalar dispatch for Abort to settle.
- **Owner:** Router `policy_pushdown.rs` / `gql::lower_for_execution`; Graph
  `plan_wire_guard::validate_mutation_target_plan` owns exact-input admission.
- **Evidence:** `pure_exists_row_lowers_to_one_bounded_semi_apply_probe` checks ordinary chain
  lowering and the bound-input rejection; `bound_vertex_keeps_indexed_policy_as_a_full_residual`
  checks the supported property form. These are native tests, not an EXISTS bulk E2E claim.
- **Impact / needed behavior:** A non-tenant whose target-label visibility needs a chain cannot
  perform bulk vertex updates under that policy. Ordinary GQL policy execution is unchanged.
- **Next decision:** Specify bounded chain evaluation over the one saved vertex and its replicated
  execution path before extending the exact-input shape. Do not substitute an index-selected target
  or drop residual authorization checks.
- **Contract:** [ADR 0057](adr/0057-router-operation-api-and-durable-bulk-load.md), update-lane
  policy and exact-target constraints; [plan format](gql/plan-format.md), exact vertex mutation input.

### GAP-2026-09-11-004 — Bulk-load edge property update needs replicated resolution and retry-safe target validity

- **Status:** Open — prerequisite slice. Recorded 2026-09-11 after the edge-property SET review
  round withdrew the implemented surface (wire variant, CLI mode, Router path, docs section) from
  its slice rather than shipping a weakening contract. Edge-property SET through bulk load is
  **not implemented**. The vertex update lane's journal-first resume/abort work in the same round
  did not re-add any edge surface, identity, or lock substrate.
- **Owner:** Router bulk-load workflow (`crates/router/src/bulk_load.rs`) owns admission and
  replay. Graph owns canonical adjacency, edge-target validity, and any Router→Graph resolution
  API (`crates/graph/src/lib.rs`, GraphStore, and the underlying LARA mutation boundaries).
- **Observed behavior (confirmed):** Router ingress handlers execute in replicated mode. The
  withdrawn Router→Graph pre-read of *edge state* used the composite query
  `execute_plan_query` (`#[query(composite = true, guard = "guard_router_canister")]`), which a
  replicated-mode handler cannot call. A PocketIC run of the withdrawn edge-update lifecycle
  failed with `InvalidArgument("graph execute_plan_query call failed: call rejected: 5 - IC0527:
  Composite query cannot be called in replicated mode")`, so an `Append` handler could not
  evaluate the required exactly-one count before admission.
- **Observed behavior (what already exists, confirmed):** Edge property indexes and Router-side
  edge index lookup are implemented: `indexed_catalog::active_edge_physical_index`,
  `router/src/index_lookup.rs` `collect_edge_equal_hits_paged` (`lookup_edge_equal_page`) and
  `collect_edge_range_hits_paged` (`lookup_edge_range_page`), the Router wire helpers
  `lookup_edge_equal_wires`/`lookup_edge_range_wires` (`router/src/gql.rs`), and the planner's
  `PlanOp::EdgeIndexScan` (`gql-planner/src/plan.rs`). The earlier statement that no edge property
  index exists was wrong.
- **Replicated-call finding (2026-09-12, source/specification evidence):** Edge index paging
  endpoints are ordinary, non-composite queries. The [IC interface specification](https://docs.internetcomputer.org/references/ic-interface-spec/)
  permits update execution to call update and ordinary query methods. The earlier call-mode
  uncertainty is resolved at the protocol level, not by a new bulk-edge runtime test. These APIs
  select property values and return `EdgePostingHit { shard_id, owner_vertex_id, label_id,
  slot_index }`, not endpoint-pair adjacency or a destination/generation witness. Posting count
  alone therefore cannot establish canonical multiplicity for `(from, label, to)`. Graph's
  `execute_plan_update` is not a read-only substitute: its wire guard rejects read-only plans.
- **Target-validity finding (2026-09-12, source/test inspection):** `GlobalEdgeId` is a 16-byte
  query-time physical handle `(shard, owner, label, slot)`, not a lifetime occurrence ID. LARA's
  `unordered_scalar_insert_reuses_interior_slab_tombstone` deletes target 20 at slot 1 and inserts
  target 21 into the same slot. Graph maintenance also moves slots and relocates sidecars/postings.
  Saving the handle does not prevent an unstarted row from updating a replacement edge; checking
  the destination still cannot distinguish delete/reinsert with identical endpoints. The latter
  is an identity-model counterexample, not a newly executed test. Repairing the read API alone
  cannot establish the retry contract.
- **Selected direction (2026-09-12; planned, not implemented):** Preserve multi-edges; select and
  update all eligible matches at each input row's first Graph execution, rather than pinning a
  singleton edge before admission. Zero matches succeeds with target count zero. Exceeding bounded
  work/mutation/byte limits rejects before writes. Selection, canonical SET and durable outcome
  must complete locally without an intervening external call; retries replay recorded outcomes.
  This replaces the withdrawn zero/many-rejection requirement for edge updates, not the existing
  vertex bulk contract. See [selected direction and storage accounting](investigations/2026-09-12-bounded-edge-membership-witnesses.md#selected-direction-and-storage-accounting).
- **Evidence:** `crates/graph/src/lib.rs:80` (composite-query graph read),
  `crates/router/src/graph_client.rs:100-119` (mode→method mapping),
  `crates/router/src/gql_search.rs:1637` (query-mode dispatch),
  `crates/router/src/index_lookup.rs:185,216` (edge equality/range lookup),
  `crates/router/src/facade/stable/indexed_catalog.rs:267` (`active_edge_physical_index`),
  `crates/gql-planner/src/plan.rs:414` (`PlanOp::EdgeIndexScan`),
  `crates/graph/src/facade/store/handle.rs:10` (internal edge handle), and the failing PocketIC
  run recorded in this review round.
- **Research:** [Bulk edge SET prerequisites](investigations/2026-09-12-bulk-edge-set-prerequisites.md)
  records the API/identity evidence, existing journal/receipt extension points, alternatives, and
  validation gates against `95d791087`. No implementation or new runtime validation was performed.
- **Candidate design (2026-09-12):** [Sparse edge-reference tokens](investigations/2026-09-12-sparse-edge-reference-tokens.md)
  explores a bounded Graph registration map and non-reusing allocator, without widening every
  vertex or edge. Internal LARA materialize/sibling drains use no-op observers, so existing Graph
  callbacks alone do not establish complete invalidation coverage. Bounded resolution and the
  availability cost of capacity-driven invalidation also remain gates. No design approval,
  MemoryId allocation, implementation, or runtime validation is claimed.
- **Alternative research (2026-09-12):** [Primary-source comparison](investigations/2026-09-12-edge-reference-alternatives.md)
  identifies occurrence IDs with bounded source/label lookup as an alternative that requires no
  global ID-to-location directory. Kuzu's inspected update code uses this lookup organization;
  applying it to Gleaph would require wider edge records and identity-preserving rewrites.
  The all-edge ID proposal was declined on storage/performance grounds; no measured throughput
  regression is claimed.
- **Fixed-budget research (2026-09-12):** [Logical-membership witnesses](investigations/2026-09-12-bounded-edge-membership-witnesses.md)
  combines canonical singleton selection with a fixed-size hashed revision array for edge
  insertion/deletion. Adjacency rows stay unchanged; pure movement need not notify the witness,
  and an unusable saved slot rejects rather than starting another scan. Complete membership-write
  coverage, storage-mode/work bounds, metadata-write cost, and bulk-level hash false conflicts
  remain unclosed gates. No implementation or runtime validation was performed.
- **Bounded-read prerequisite (2026-09-12 23:34:39 UTC +0000):** Implemented tree visitor
  short-circuiting in LARA; [ADR 0088](adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#tree-visitor-short-circuit-implemented)
  owns the contract and measurements. Previously, the topology and inline-property adapters saved
  a callback's `Break` but the underlying walkers continued through later blocks/properties.
  `tree_visit_break_stops_topology_reads` and `tree_visit_break_stops_property_reads` reproduced
  both defects and now prove physical stopping in both orders. This is not complete selection:
  `read_label_bucket_placement_info` counts the overflow chain, and selected-slot readers can
  reconstruct chains/prefetch tables outside the emitted-row count. Those existing APIs are not
  a whole-row work/byte bound; no new validity registry is needed.
- **Complete-topology prerequisite (2026-09-13 00:22:26 UTC +0000):** LARA now exposes
  `collect_edge_topology_bounded` and its forward adapter; [ADR 0050](adr/0050-lara-traverse-read-api.md#bounded-complete-topology-collection-implemented)
  owns the contract. A physical-slot allowance includes tombstones and linked overflow entries;
  exhaustion returns an error, never a successful prefix. No property stream or whole-leaf log
  table is read. Workspace and topology payload are bounded from the slot allowance and concrete
  edge width; this is not an independent byte-budget API. Six native contracts and focused
  canbench cover the storage capability, not bulk SET or policy/mutation atomicity.
- **Inline-read prerequisite (2026-09-13 01:26:43 UTC +0000):** LARA exposes a complete topology plus
  `InlinePropertyBytes` collector with physical-slot and value-body allowances; [ADR 0050](adr/0050-lara-traverse-read-api.md#bounded-complete-inline-property-collection-implemented)
  owns the bounds. Value admission precedes property reads, reserves wide-blob lookahead, validates
  exact property-log exhaustion, and preserves slab live ordinals/tree physical slots. Six native
  contracts and focused canbench validate this read capability, not policy/SET atomicity. The tree
  property helper's out-of-range block-zero sentinel was replaced with a typed rejection.
- **Sidecar pre-read blocker (2026-09-13, source inspection):** `facade/stable/edge_properties.rs`
  stores unbounded `StoredPropertyValue`s. The ic-stable-structures 0.7.2 B-tree getter materializes
  the whole encoded value before its `Storable::from_bytes` call; `ensure_persistable` only checks
  binary encoding. A size check after the ordinary getter cannot establish a pre-read byte cap.
  No sidecar format, length map, dependency fork or whole-row mutation preflight was introduced.
- **Sidecar node-discovery evidence (2026-09-13 01:52:13 UTC +0000):** A native 0.7.2 B-tree probe
  with fixed 14-byte keys and unbounded raw values confirms that a ten-byte lookup, a missing-key
  lookup and key-only iteration all perform more metadata reads as an unselected neighbor grows.
  `Node::load_v2` reconstructs the whole node overflow list before key search, so exposing only a
  value length would not close the work bound. The [investigation and reproducer](investigations/2026-09-13-sidecar-bounded-read-options.md)
  compare a dependency-owner extension, a global property cap and out-of-line storage. That
  investigation recommended an extension as the minimum layout-preserving change; the design
  direction below supersedes that priority. This is primitive read evidence, not Graph codec,
  write-budget, policy or bulk SET validation.
- **Property-value separation design (2026-09-13 03:57:44 UTC +0000):** The
  [design](investigations/2026-09-13-property-value-separation-design.md) keeps per-property
  keys and bounded small cells in the existing vertex/edge directories, with independent large
  bodies owned by each PropertyStore. A reclaimable fixed-block allocator is an experiment, not
  an accepted production layout. Threshold, fragmentation, common-path cost, rekey ownership and
  whole-row preflight remain validation gates. No production Rust, MemoryId, persisted codec,
  dependency or public API changed; no extra adjacency IDs or reference journal are proposed.
- **Value-separation experiment (2026-09-13 06:23:23 UTC +0000):** Two native tests over 24
  configurations passed, including body admission before reads, zero-write rejection, ownership
  transfer and persistent block reuse. The [results](investigations/2026-09-13-property-value-separation-design.md#8-probe-results-and-disposition)
  demonstrate removal of unselected-body read/write coupling. They do not select the fixed-block/
  replacement-body candidate: short-body waste, retained high-water and ordinary-path preparation
  cost require further design. A partial-pair reopen defect was found and fixed in the new probe,
  not in production Graph. No Graph codec, Wasm instruction, policy, whole-row SET or IC rollback
  proof is claimed; the existing storage and public capability remain unchanged.
- **Comparison-first follow-up (2026-09-13 09:36:59 UTC +0000):** The
  [B-tree body comparison](investigations/2026-09-13-property-body-btree-comparison.md) measures five
  whole/chunk configurations, with two native tests and 188 I/O rows. Default-geometry 4 KiB chunks
  reduce the small external-body neighbor rewrite from 1048708 to 16582 bytes, but rewrite a 1 MiB
  body with about 5.30× the whole-body write volume. Declared 4 KiB bounds occupy 27 native pages
  for the tested 256 short bodies versus one with default geometry; shrink can also allocate during
  tree mutation. These are diagnostic observations, not Graph instruction or workload acceptance.
  The [extent proposal](investigations/2026-09-13-property-body-extent-design.md) was not measured
  by this B-tree slice; its later native checkpoint is recorded below, without production selection. Body-key/format/move ownership, decoder and
  total-work admission, whole-row co-write and IC lifecycle remain open; no production region,
  allocator, dependency or public capability changed.
- **IC body-lane comparison (2026-09-13 11:08:59 UTC +0000):** The
  [stable-memory follow-up](investigations/2026-09-13-property-body-btree-comparison.md#ic-stable-memory-follow-up)
  completed 32 canbench cases using direct Ic0StableMemory and the shared native body algorithm.
  Default-geometry 4 KiB chunks reduce a small external-body neighbor rewrite from 2.62M to 0.188M
  instructions, but a full 1 MiB replacement costs 28.32M versus 6.94M, and shrink 38.27M versus
  5.06M. Ordinary Whole point-get is also retained as a faster read control than range/assembly.
  All measured heap/stable page deltas are zero after setup; this does not mean zero allocation.
  The two native tests and all 188 I/O rows remain unchanged. Query benchmarking is not Graph
  commit/rollback or workload acceptance; no production layout or custom allocator is selected.
- **Workload relevance (2026-09-13 11:35:15 UTC +0000):** The
  [codec-based input profile](investigations/2026-09-13-property-body-workload-profile.md) covers
  Knowledge and Social 1x1/5x20 typed load artifacts. Maximum Value size is 143 bytes; none exceeds
  4 KiB. Social scaling repeats 71 post texts, not a sampled large-body tail. At hypothetical T=128,
  300/35932 scaled vertex-property occurrences and 0/94460 edge-property occurrences remain external,
  but counts are not access frequencies (all Social prepared queries project Post.body). No runtime
  read/rewrite/shrink history is available from these inputs; large-row test fixtures are synthetic.
  This does not select a cap/threshold or justify custom allocation; representative old/new sizes
  and operation counts remain necessary for workload-weighted acceptance.
- **Chunk-size sensitivity (2026-09-13 12:30:53 UTC +0000):** The
  [wider-chunk comparison](investigations/2026-09-13-property-body-chunk-size-sensitivity.md) extends
  the same body algorithm to 16/64 KiB default-geometry caps: two native tests, 260 I/O rows and
  48 IC query benchmarks pass; the original 188 native rows/32 IC counters are unchanged. Wider
  chunks improve some costs versus 4 KiB, but 1 MiB replacement still costs 3.44×/3.85× Whole and
  shrink 5.28×/5.12×. Neighbor isolation worsens; the 16 KiB shrink adds one stable page. This is
  size sensitivity, not workload acceptance, a production cap or custom-allocation authorization.
- **Native extent checkpoint (2026-09-13 14:02:33 UTC +0000):** After explicit authorization, the
  [allocator-only comparison](investigations/2026-09-13-property-body-extent-comparison.md) passed
  six native tests/36 extent rows; all 260 B-tree reference rows remain unchanged. Existing 0.7.2
  exposes no selected-node batch replacement/removal API; no fork was introduced. Contiguous
  1 MiB replacement writes 1048592 bytes versus 5559316 with 4 KiB chunks. Repeated hot shrink
  writes 260 bytes but still reads the full old 1 MiB value. Exact partition/index/owner oracles,
  pre-effect rejection, post-allocation retirement and nine paired reopens pass. The extra index
  doubles the raw short-body floor (two pages versus one); fragmentation and relocation can retain
  more capacity. These are native I/O results, not IC instructions, rollback/upgrade, directory
  atomicity or whole-row work bounds. The managed IC follow-up is recorded below; production
  selection remains pending.
- **Managed IC extent checkpoint (2026-09-13 20:50:20 UTC +0000):** The
  [91-case comparison](investigations/2026-09-13-property-body-extent-canbench.md) uses the same
  four-page variable-manager policy for one-region B-trees and two-region extents. All91 query
  cases pass; six native tests and296 numeric rows remain unchanged. Extent1MiB replacement/shrink
  costs3.15M/2.11M instructions versus Whole7.36M/5.86M and chunks26.71–32.31M/29.54–44.75M.
  Adverse results remain: Whole point-get1.43M beats extent read2.10M; allocation from already-
  coalesced space atF128 costs68.65K versus38.08–38.12K. Extent retains four extra physical pages
  in these fixtures (ten versus six small;26 versus22 large). Five initial grows add16 physical
  pages each; three also grow heap. Query measurement/local collection reload is not persistent-
  directory atomicity, Graph work admission or IC update rollback/upgrade proof. A separately
  reviewed integration design is justified, not production activation or a workload-weighted win.
- **Native PropertyStore checkpoint (2026-09-14 01:10:06 UTC +0000):** The
  [comparison](investigations/2026-09-13-property-store-native-comparison.md) implements actual
  8/14-byte keys, Graph Value codec, directory ownership and single-property moves for Direct,
  directory+B-tree4/16/64KiB and directory+custom extents. Ten tests pass;40 full stores produce
  2640 region rows, with all260+36 raw rows preserved. Candidates pre-admit selected bytes; Direct
  remains an unbounded admission reference. Small-directory isolation helps all candidates, but
  small mutations add reads and eager lanes retain backing. Chunk writes/moves amplify I/O;
  extent relocation retains holes/high-water, while B-tree deletion can also grow backing.
  These native counts do not establish IC speed or total work bounds. The separate managed
  full-store measurement follows below; frozen0357/0358 sources preserve historical provenance.
  Fine bucket/zero-fill/free-lookup optimizations remain deferred. No production format, whole-row
  safety, durable edge outcomes or bulk edge capability is selected.
- **Managed PropertyStore checkpoint (2026-09-17 22:24:02 UTC +0000):** The
  [full-store IC comparison](investigations/2026-09-17-property-store-managed-comparison.md) passes
  all210 full-store cases plus91 reused body controls. Actual keys, Value validation/codec,
  previous values and all regions are included. For edge keys, Direct ordinary1MiB get/replacement
  costs5.36M/9.85M instructions versus extent6.04M/10.25M; extent scalar insert/replace is about
  37%/60% more expensive. Extent large moves improve26.55M→12.11M, and small-neighbor isolation
  helps every separated candidate. Extent coalesced allocation costs118.36K versus Direct46.81K.
  Small physical totals are Direct6/chunks10/extent14 pages;1MiB point totals22/26/30. These are
  independently reserved capacities, not per-edge metadata or automatically wasted bytes. C64
  large move/deletion each grows four physical pages; extent relocation retains high-water.
  Native2640+296 rows reproduce. No production integration, total-work/whole-row proof,
  update/upgrade lifecycle validation or workload-weighted winner follows.
- **Impact:** Bulk-load edge SET remains unavailable. The selected direction needs bounded local
  selection/SET and durable terminal replay, not a cross-message edge-reference registry. Bulk-load
  edge inserts and vertex SET/REMOVE contracts are unchanged; ordinary GQL edge `SET` remains
  available. The tree, topology and inline-property read prerequisites do not expose a bulk operation.
- **Next decision:** Specify endpoint/policy semantics, local work/mutation/byte limits, exact
  positive/zero/rejection outcome representation, input-row versus matched-edge counts, and
  retention/Abort behavior before implementation. The existing exact-vertex seed and receipt
  contract do not represent this behavior as-is. No per-edge field or generation table is selected;
  recovery state belongs in the existing scalar journals and chunk receipts. Include actual
  serialized record/outbox/high-water measurements in the bounded validation plan.

### GAP-2026-09-11-001 — `PERCENTILE_CONT`/`PERCENTILE_DISC` fail closed on non-constant, NULL, or out-of-range fraction

- **Status:** Open — non-blocking follow-up recorded 2026-09-11 during the GQL-execution
  Unsupported-path survey. Fail-closed only; no wrong results. Low priority; a docs-only
  disposition is acceptable.
- **Owner:** Graph aggregate executor (`crates/graph/src/plan/query/aggregate.rs`).
- **Observed behavior:** Three error paths are reachable through grammatical queries:
  (1) `aggregate.rs:591` "aggregate: percentile fraction must be constant per group"
  for `PERCENTILE_CONT(x, <row-varying expr>)` such as `PERCENTILE_CONT(x, n.k)`;
  (2) `aggregate.rs:719` "aggregate: percentile fraction missing" for
  `PERCENTILE_CONT(x, NULL)` (fraction NULL on every row of a non-empty group);
  (3) `percentile_fraction_from_value` (`aggregate.rs:215-227`)
  `InvalidExpressionValue` for non-finite or out-of-`[0, 1]` fractions such as
  `PERCENTILE_CONT(x, 1.5)`. The constant-fraction requirement itself is SQL-consistent;
  the contract is simply undocumented.
- **Evidence:** source paths above; aggregate happy-path tests (`percentile_cont_median`
  et al.) cover no error path; no `percentile` mention in
  `design/gql/extension-syntax.md` or this ledger.
- **Expected or needed behavior:** The fraction contract (constant per group, non-NULL,
  finite, within `[0, 1]`) stated in `extension-syntax.md`, or error-path unit tests
  locking the three fail-closed arms.
- **Impact:** Users hitting these shapes get an execution error against an undocumented
  contract; nothing unsafe or misleading is returned.
- **Next decision:** Document the fraction contract in `extension-syntax.md` (smallest);
  add the three error-path unit tests only if the contract is ever relaxed.

### GAP-2026-09-11-002 — Cross-shard traversal fails closed on remote-vertex expand (federated scan limitation)

- **Status:** Open — non-blocking; single-shard execution (the only exercised topology)
  is unaffected. Recorded 2026-09-11 as one bundled entry during the GQL-execution
  Unsupported-path survey.
- **Owner:** Graph federation expand (`crates/graph/src/federation/expand.rs`) together
  with the streaming expand and WCOJ paths.
- **Observed behavior:** A traversal touching a vertex placed on another shard fails
  closed: `UnsupportedOp("cross-shard expand (remote vertex binding)")`
  (`federation/expand.rs:93`; sibling arms `:38,43,64,97,122`),
  `UnsupportedOp("Expand.var_len.remote")`
  (`plan/query/executor/scan/streaming.rs:570`), the limited-streaming remote-source
  arm (`streaming.rs:525`), and `WorstCaseOptimalJoin.remote_vertex` / `.var_len`
  (`plan/query/executor/wcoj.rs:21,154`). Relation to GAP-2026-08-25-003: that entry
  covers cross-shard *ordering* (concatenated sorted fragments); this entry covers
  cross-shard *traversal* (no remote-vertex resolution). Both stem from shard-local
  execution without a cross-shard fan-out.
- **Evidence:** source paths above; PocketIC fixtures are single-shard, so no E2E
  coverage exists for any of these arms.
- **Expected or needed behavior:** Either a remote-vertex resolution fan-out or an
  explicit documented federated-traversal limitation.
- **Impact:** Multi-shard deployments cannot traverse shard boundaries; queries fail
  with an explicit error rather than silently dropping rows.
- **Next decision:** Resolve inside the slice that introduces cross-shard resolution,
  if ever demanded; until then no action.

### GAP-2026-09-11-003 — Federated aggregate merge errors on all-NULL partials instead of yielding NULL/skip

- **Status:** Open — non-blocking, low priority. Fail-closed error direction (never a
  wrong result); single-shard execution unaffected; `Avg` excluded (already
  non-mergeable UnionRows). Recorded 2026-09-11 during the GQL-execution
  Unsupported-path survey.
- **Owner:** Router federation aggregate merge
  (`crates/router/src/federation/aggregate_merge.rs`).
- **Observed behavior:** For mergeable `COUNT`/`SUM`/`MIN`/`MAX`, when the
  first-ingested shard partial for a group is `Null` — e.g. grouped `SUM(x)` where one
  shard holds the group's rows but every value is NULL, so the Graph partial is Null —
  `merge_add_values` (`:345`) / `merge_extreme_value` (`:381`) return
  `Err("aggregate merge on null")` instead of skipping the NULL partial. All shards
  empty produces no rows and does not trigger.
- **Evidence:** source paths above; merge tests (`merge_aggregate_blobs_*`) cover valued
  partials only.
- **Expected or needed behavior:** SQL-consistent outcome: skip NULL partials and emit
  NULL when every partial is NULL.
- **Impact:** A rare multi-shard-only query error; the error direction is safe.
- **Next decision:** Smallest fix is "skip NULL partials, emit NULL when all partials
  are NULL" plus a two-shard unit test; open only on demand.

### GAP-2026-08-25-003 — Cross-shard global order for ordered-delivery and plain ORDER BY results (merge-aware union deferred)

- **Status:** Open — non-blocking follow-up recorded while landing ADR 0081 Slice A
  (2026-08-25). Not a regression introduced by that slice: Federation v1 already merged
  shard-local `ORDER BY` fragments by concatenation.
- **Owner:** Router federation merge (`crates/router/src/federation/merge.rs`
  `FederatedMergeMode::UnionRows` → `GqlWireRows::merge_optional_batch_blobs`)
  together with the future per-shard cursor-stream fan-out described in
  [ADR 0081 §Decision 3](../adr/0081-index-ordered-order-by-delivery.md).
- **Observed behavior:** Every current index read path delivers one globally ordered
  hit stream (native single posting map; single index canister per logical graph via
  `RouterIndexLookup::require_single_target`), so ADR 0081's R-way merge degenerates to
  identity and shards return ascending fragments. When a query fans out over multiple
  graph shards, the router concatenates those sorted fragments (`UnionRows`), so global
  cross-shard ordering is not guaranteed — both for the new ordered-delivery plans and,
  unchanged, for every pre-existing `Sort`/`TopK` plan executed per shard.
- **Evidence:** `design/gql/plan-format.md` §"IndexScan ordered-delivery intent";
  `crates/router/src/index_lookup.rs:315` (`require_single_target`);
  `crates/router/src/federation/merge.rs` UnionRows branch.
- **Expected or needed behavior:** When per-shard cursor streams exist, the federation
  bind boundary should run an R-way heap merge of the ascending streams (ADR 0081 §3)
  or, at the router, a merge-aware union for batches whose plans carry
  `IndexScan { ordered_by_sort }`.
- **Impact:** Multi-shard deployments cannot rely on router-level result ordering for
  `ORDER BY` queries; single-shard execution (the only mode exercised by native stores
  and current canister topologies) is unaffected.
- **Next decision:** Fold merge-aware union into the slice that introduces per-shard
  cursor fan-out; until then no action.
- **Status detail:** Blocked on the cursor-stream fan-out prerequisite; revisit when
  sharded postings land.

### GAP-2026-08-24-008 - RESOLVED: ELEMENT_ID projections need property-level READ grants (plan 0303 diagnosis; no Router defect)

- **Status:** Resolved (plan 0303, 2026-08-24). Root cause: the knowledge demo's grant
  surface lacked **property-level READ rows**. ELEMENT_ID-bearing projections demand
  `ReadProperty(label, property)` per projected key (`authz.rs:479-513`
  `require_vertex_scan_rows`; see also `labeled_scan_projection_contract`), which bare
  label-level `GRANT READ ... NODES <label>` does **not** cover (it lowers to the READ vertex
  row plus ReadProperty rows only for explicitly enumerated properties,
  `gql_grants.rs:413 resolve_property_ids`). Once brace-form
  `GRANT READ ... NODES Concept { name } TO PUBLIC` rows are applied, non-owner execution
  of `variable-length-reach` succeeds end-to-end (**7 rows**, verified live).
- **Owner:** demo grant surface (`demo/knowledge/scripts/apply-public-grants.sh`); walker
  semantics in `crates/router/src/authz.rs` confirmed correct (see
  `labeled_scan_projection_contract` test).
- **Observed behavior (final-state matrix, all ops freshly registered + published on a
  pristine demo-local network, canonical order):**

  | projection / shape | dev (non-owner) |
  | --- | --- |
  | RETURN 1 AS x (no graph) | PASS |
  | MATCH (n:Concept) RETURN n.name [AS name] [LIMIT n] | PASS (3 rows) |
  | MATCH (n:Concept) RETURN 1 AS x LIMIT 3 | PASS |
  | var-length incoming + DISTINCT, no ELEMENT_ID | PASS |
  | single-hop incoming + DISTINCT + ELEMENT_ID | DENY Forbidden |
  | var-length incoming + DISTINCT + ELEMENT_ID (= variable-length-reach) | DENY Forbidden, then PASS (7 rows) after property-level READ grant |

  Owner executes every variant via implicit-root tenancy bypass.
- **Mechanism:** ELEMENT_ID projections demand `ReadProperty(label, property)` rows; bare
  label-level READ grants do not emit those rows (`resolve_property_ids` enumerates only
  the statement's property list), so hydrated plans deny for non-tenants until a
  brace-form property-list READ grant is applied. No unattributed row is involved and no
  Router code change is required.
- **Expected or needed behavior:** published scenarios execute for arbitrary visitors once
  the PUBLIC surface covers match/traverse/read including per-property reads.
- **Evidence:** artifacts sections 13-14 in
  `design/investigations/artifacts/0296-knowledge-demo-bringup-evidence.txt`; probe scripts
  `scripts/register-probe.sh`, `scripts/direct-grant-probe.sh`,
  `scripts/apply-public-grants.sh` (all removed 2026-08-28 — superseded by
  `gleaph grants apply` and the grants policy file); contract-locking test
  `element_id_projection_demands_are_coverable_property_reads`.
- **Impact:** resolved for vertex scenarios. Two residual follow-ups: (a) RESOLVED by the
  plan 0306 diagnosis (2026-08-26) — this follow-up's original framing was a
  misattribution: requirement extraction on the current tree shows an edge `ELEMENT_ID(e)`
  projection adds no demand beyond the traversal row its label fact already covers, so
  citation-reach executes for PUBLIC once the brace-form property READ rows exist (contract
  test `edge_element_id_projection_demands_stay_attributed`; probe artifact
  `design/investigations/artifacts/0306-edge-element-id-demand-probe.txt`; no edge-property
  grant resource needed for element-id reads; runtime execution verified
  2026-08-26 only after GAP-2026-08-26-001 resolved the group element-id read);
  (b) OPEN — shortest-path currently hits
  IndexScan(no index client) under active sibling index/planner development (observation,
  not diagnosed here).
- **Fix direction:** none required in the walker. Demo-side resolution landed via
  apply-public-grants.sh. Optional future enhancement: let READ without a property list
  expand to all catalogued properties of the label.
- **Detection:** w1:p0 during plan 0296 quickstart; escalated by admin pane w1:pQ as P1.

### GAP-2026-08-24-009 — `ILIKE` evaluation deliberately ships without SQL LIKE wildcard semantics

- **Status:** Resolved — SQL LIKE slice A landed (LIKE new kind + ILIKE fold-then-match upgrade); ESCAPE clause landed as the follow-up below; LIKE prefix-index fusion landed separately (vertex + edge)
- **Follow-up (landed): `ESCAPE <char>` clause (2026-09-11, uncommitted slice):** standard SQL `x LIKE pattern ESCAPE char` / `x ILIKE pattern ESCAPE char` with an optional single-character escape replacing the default backslash (omitted clause keeps `\`; an explicit escape makes `\` ordinary). AST `StringPredicate.escape: Option<Box<Expr>>` (parser accepts the clause for LIKE/ILIKE only — `CONTAINS`/`STARTS WITH ... ESCAPE` fail closed at parse); type check warns on empty/multi-scalar literal escapes (strict mode → `TypeError`); execution resolves per-row (`NULL` escape → UNKNOWN, non-Text/non-single-scalar — including `$param` — → `IncomparableValues` fail-closed; ILIKE folds the escape with both operands, fold-collapse → fail-closed). Prefix fusion resolves the prefix with the static escape (`like_literal_prefix(pattern, escape)`); `$param`/bad-literal escapes stay residual-only. `NOT LIKE ... ESCAPE` rides the existing `negated` flag. This resolves the GQL `\%` spelling pain: `LIKE 'a#%b%' ESCAPE '#'` matches literal `a%b…` with no backslash doubling beyond GQL string rules. `$param` patterns stay non-fused (unchanged).
- **Owner:** `crates/graph/src/plan/expr_evaluator.rs` (`eval_string_predicate_expr` + `sql_like_match`) owns the runtime semantics
- **Resolution:** `StringPredicateKind::Like` added (AST tail variant, parser `eat_keyword("LIKE")`, `NOT LIKE` via the existing `negated` flag); `Like` runs SQL wildcard semantics over Unicode scalars (`%` any run, `_` one scalar, `\` escapes the next pattern scalar, trailing `\` literal); `ILike` folds both operands with `str::to_lowercase` and runs the same matcher. LIKE/ILIKE never fuse into an index anchor — residual filter only (`STARTS WITH` keeps the prefix-index path; planner regression `match_negated_or_other_string_predicates_never_anchor_and_stay_residual` extended with LIKE cases). Documented as a SQL-compat dialect extension in `design/gql/extension-syntax.md` (GQL core has no LIKE/ILIKE keywords).
- **Migration contract (breaking for `%`/`_` ILIKE users):** `ILIKE` previously implemented whole-string equality with literal `%`/`_` (tested contract). Those patterns are wildcards now: `'100%'` matches the `100` prefix, `'a_b'` matches any one-scalar middle. Match literals with `'100\%'` / `'a\_b'`. `LIKE` itself is new (non-breaking). The old literal-semantics test (`ilike_treats_percent_and_underscore_as_literals`) was rewritten to the wildcard contract (`like_and_ilike_apply_sql_wildcard_semantics`); no dual-path code remains.
- **Observed behavior (superseded):** Phase 1 string-predicate execution (commit introducing
  `ExprKind::StringPredicate` evaluation) implements `ILIKE` as plain Unicode case-insensitive
  whole-string equality: both operands are folded with `str::to_lowercase` and compared for exact
  equality. `%` and `_` are literal characters, not wildcards.
- **Contract basis:** No existing contract defined ILIKE semantics. Evidence surveyed:
  parser comment (`crates/gql/src/parser/expr.rs:1344`, "[NOT] CONTAINS, [NOT] ILIKE"), AST doc
  comment (`crates/gql/src/ast/expr.rs:145`), type-check constraint
  (`check_string_predicate` in `crates/gql/src/type_check/infer.rs:836`, which only requires
  string-compatible operands), and `ast/tests.rs`. No LIKE kind exists in the AST and no
  `%`/`_` handling exists anywhere in value evaluation, so the pre-production default applies:
  naive case-insensitive match with literal metacharacters. This complements the family:
  `STARTS WITH` / `ENDS WITH` / `CONTAINS` are case-sensitive positional matches, `ILIKE` is the
  case-insensitive whole-string match.
- **Impact:** Queries written with SQL-LIKE-style patterns (`'Ada%'`) will not match prefix-style;
  they require literal `%` in the data until a SQL LIKE feature defines wildcard semantics.
  Non-negated `STARTS WITH` TEXT-index pushdown (Phase 2) must preserve exactly these residual
  semantics for the non-indexed remainder.
- **Next decision (decided):** `LIKE` absorbed the wildcard grammar and `ILIKE` became its case-insensitive counterpart (fold-then-match); plain case-insensitive equality did not survive — see migration contract above.

### GAP-2026-08-24-010 — Edge-side `STARTS WITH` TEXT-index pushdown (vertex-only slice)

- **Status:** Resolved — edge-symmetric extension landed in the same slice family as the vertex
  side (fix: `feat(planner,graph,router)` commit `c8f60f894`, 2026-08-24; regression
  coverage listed below)
- **Owner:** Edge scan family: planner (`crates/gql-planner/src/planner/match_plan/path/filters.rs`
  edge fusion + `EdgeIndexScan` emission), executor
  (`crates/graph/src/plan/query/executor/scan/edge_index.rs`), Router seed extraction
  (`crates/router/src/seed.rs` edge anchor arms), and wire/explain/cost sync
- **Observed behavior:** The vertex slice fuses non-negated
  `v.prop STARTS WITH <Text literal | $param>` into `ScanValue::TextPrefix`
  (`AnchorSource::PropertyPrefix`, `IndexAnchor::Prefix` seeds) over range-indexed vertex
  properties. Edge properties had no symmetric path: `-[e:R WHERE e.prop STARTS WITH 'x']->`
  stayed fully residual even when `e.prop` has an ordered edge index.
- **Resolution:** `EdgeFilterFusion.indexed_prefix` fuses after equality → IN → range and keeps the
  predicate residual; the leading bound chain lowers to
  `PlanOp::EdgeIndexScan { value: TextPrefix, cmp: Eq }`; eligibility flows through
  `has_indexed_edge_bound`; executor intercepts TextPrefix before equality dispatch and derives the
  shared `text_prefix_range_bounds` interval (`lookup_edge_range_local`, Null pattern binds no rows,
  non-TEXT resolved pattern falls back to the canonical superset); Router extracts
  `IndexAnchor::EdgePrefix(EdgePrefixSeedProbe)` at all three edge extraction sites via
  `edge_prefix_anchor` (reusing `resolve_edge_equal_seed_context`) and executes it through
  `lookup_edge_range_wires`. Landing this slice exposed and fixed the filter-pushdown stale-index
  defect recorded as GAP-2026-08-24-011. Regression coverage: planner edge prefix matrix,
  exact-Between-bytes mock test, missing-parameter / Null / non-TEXT contracts, native e2e boundary
  row sets with red-proof, Router extraction + execution harness tests.
- **Contract basis:** Same shape as GAP-2026-08-24-002 (edge IN-list symmetry): the interval
  primitive exists (`PostingRangeRequest::Between` via `lookup_edge_range`, used by edge range
  anchors since GAP-2026-07-29-003); bound derivation reuses
  `text_prefix_range_bounds(_for_encoded_key)` from `crates/gql/src/value_index_key.rs`; no new
  key-encoding knowledge outside that file.

### GAP-2026-08-24-011 — Filter-pushdown applied index moves sequentially, dragging filters before their producer

- **Status:** Resolved — fixed in the same slice that discovered it (fix:
  `feat(planner,graph,router)` commit `c8f60f894`, 2026-08-24)
- **Owner:** `crates/gql-planner/src/pushdown/filter.rs` (`apply_filter_pushdown`)
- **Observed behavior:** With two or more PropertyFilter moves in one pass, `(from, to)` pairs were
  applied via sequential `remove`/`insert`. The first insertion shifts every later index, so a
  second move addressed a stale position and dragged an unrelated filter across a producer op.
  Concrete repro on main before the fix: any leading one-sided range edge anchor with a path-level
  WHERE residual — `[EdgeIndexScan{Ge}, bind(a,b), PF(IsLabeled b), PF(IsLabeled a),
  PF(e.weight >= 7)]` reordered to `[scan, PF(residual), PF(IsLabeled b), bind, PF(IsLabeled a)]`,
  executing `MATCH (a)-[e:R]->(b) WHERE e.weight >= 7 RETURN b` failed with
  `MissingBinding { variable: "b" }`. Equality/IN-list anchors masked the defect because their fused
  predicates are removed from residuals, leaving only one move per pass.
- **Impact:** Silent wrong plans (execution errors or, worse, filters evaluated against unbound
  variables) for multi-move pushdown passes; blocked every non-equality edge pushdown e2e.
- **Resolution:** Moves are now queued per anchor (`to - 1`, which is always an unmoved producer op)
  and the op list is rebuilt in one pass against original positions; relative order of same-anchor
  moves follows original positions. Regression tests:
  `filter_pushdown_never_moves_filters_before_their_producer`
  (planner unit invariant) and `parsed_edge_range_path_level_where_binds_projected_rows`
  (native e2e through the shared leading-edge path).
- **Next decision:** None open; if future passes adopt positional moves again, they must rebuild
  rather than remove/insert incrementally.

### GAP-2026-08-24-005 — ADR 0074 grant statements cannot address hyphenated prepared-query names

- **Status:** Resolved 2026-09-08 (uncommitted working-tree fix; no commit per primary-owns-commits) — `Parser::expect_prepared_query_name` (helpers.rs) joins bare `Ident (- segment)*` with `Int`/`BigInt` digit-led segments plus span-adjacent tail glue, and keeps the exact `QuotedIdent` arm; GRANT/REVOKE `PREPARED QUERY` plus `EXPLAIN AUTHORIZATION FOR PREPARED QUERY` share the production. Previously blocked `GRANT EXECUTE ON PREPARED QUERY <name> TO PUBLIC` for every hyphenated prepared operation (ADR 0061 names are `[a-z][a-z0-9-]*`, so most real names, e.g. the knowledge demo's `variable-length-reach`, were affected)
  **Live-reproduction + red-test addendum (2026-08-24, plan 0296 quickstart):** the rejection is
  no longer PocketIC-only. On a real demo-local network, `gleaph prepared publish citation-reach`
  fails with `InvalidArgument("parse error: expected 'TO', got '-'")` (Router wasm built from the
  current tree minutes earlier — not a stale-artifact symptom), and the tree's own
  `grant_parser_tests` under `#[cfg(feature = "gleaph")]`
  (`crates/gql/src/parser/statement.rs:2010`) are red on main:
  `parse_grant_execute_on_prepared_query_to_public_and_principal` and
  `parse_revoke_execute_on_prepared_query_mirrors_grant` both panic with the same lexer error.
  `statement.rs` sits in the owning pane's uncommitted edit set. **Operational workaround until
  this fix (retained as the canonical CLI spelling):** emit the op name as a double-quoted identifier — the lexer produces
  `Token::QuotedIdent` (`crates/gql/src/lexer.rs:628`) and grant-target `expect_ident` accepts it
  (`crates/gql/src/parser/helpers.rs:232`) — adopted by the CLI's
  `prepared::publication_statement` with parser-acceptance unit tests.
- **Owner:** `crates/gql` authorization-statement lexer/parser (`parse_grant_statement` →
  `expect_ident`; hyphen lexes as minus, not part of an unquoted identifier)
- **Merged from the removed duplicate entry (plan 0306 sync, 2026-08-26):** underscore names
  (`cli_run_tag`) fail CLI-side kebab-case validation instead, so no publishable spelling
  exists for them today; `prepare` registration and CLI name validation both accept hyphenated
  names while the grant statement rejects them, and parser tests assert `find-users` parses —
  a conflict with the runtime lexer behavior to reconcile before fixing. Resolution options
  recorded there: accept ADR 0061 prepared-name syntax for the PREPARED QUERY target (hyphenated
  or quoted identifiers), or restrict prepared names to one grammar shared by both sides; a
  lexer extension needs an ADR 0074 §5 amendment note. No correctness gap; default-deny stays
  intact.
  **Resolution 2026-09-08 (option A, parser-only, no lexer change):** `Parser::expect_prepared_query_name`
  accepts the ADR 0061 spelling in the three `PREPARED QUERY` positions (GRANT `TO`, REVOKE `FROM`,
  EXPLAIN AUTHORIZATION name + `BY PRINCIPAL` tail unchanged); full charset validation stays with
  prepared registration/Router resolution. Regression: `parse_prepared_query_name_accepts_bare_kebab_digits_and_quoted`
  (`variable-length-reach`, `phase-2-rollout`, `op-2fa`, `find-to` terminator guard, quoted `find-users`,
  EXPLAIN bare + BY PRINCIPAL). Validation: `gleaph-gql --features gleaph --lib` 567 passed / 0 failed,
  default `--lib` 524 passed / 0 failed, `cargo fmt -p gleaph-gql -- --check` clean, `git diff --check` clean.
  The previously-red `parse_grant_execute_on_prepared_query_to_public_and_principal` and
  `parse_revoke_execute_on_prepared_query_mirrors_grant` pass unmodified (no test weakening).

### GAP-2026-08-24-007 — Committed HEAD does not build `gleaph-router`: `auth::require_admin` callers landed before their auth definitions

- **Status:** Open — window is green only while the owning pane's uncommitted WIP stays in the
  tree; resolution owned by the ADR 0075 stream (w1:pT, notified 2026-08-24)
- **Owner:** `crates/auth/src/lib.rs` (missing definitions) vs `0d9f58421`/`73d4b3737`/`e9159938b`
  (router-side callers: `facade/store/catalogs.rs`, `idempotency.rs`, …)
- **Observed behavior:** An isolated worktree at `5a8e6dc8a` (current main HEAD at detection time)
  fails to compile `gleaph-router --lib` with `E0425 cannot find function require_admin in module
  auth` (multiple sites). The main working tree passes (`951 passed / 0 failed`) because the
  uncommitted auth-side changes of the same workstream are still present.
- **Expected or needed behavior:** Every commit on main must build standalone. The auth-side
  definitions (or an equivalent refactor of the callers) must land in the same push that lands the
  callers. Same failure mode as the `8e392c127` half-committed `edge_properties.rs` incident.
- **Evidence:** `git show 5a8e6dc8a:crates/router/src/facade/store/catalogs.rs | rg require_admin`
  vs absent definition in `git show 5a8e6dc8a:crates/auth/src/lib.rs`; isolated-worktree build log.
- **Impact:** Bisect over this range reports false positives; any fresh clone/worktree at these
  commits cannot run router tests.
- **Detection:** w1:pQ during edge IN-list slice verification (Phase 2 final gate), 2026-08-24.

### GAP-2026-08-24-006 — Pure-CLI platform bring-up fails on the Gleaph-owned launcher network

- **Status:** Fully Resolved (2026-08-26). (a) operator tier + CLI deploy, (b) daemonization,
  and (c) the Provision init/grant model refinement (below) all landed; `gleaph identity new
  dev && gleaph network start -d` completes end to end (Account/Provision deploy + account
  auto-registration) on a Gleaph-owned launcher network; `gleaph-operator grant upsert`
  verified against the live network
- **Owner:** `crates/cli/src/network.rs` (launcher lifecycle, management-canister transport)
  and the launcher's HTTP gateway contract (`icp-cli-network-launcher` v15.0.0);
  Account/Provision deployment flow in `network::start`
- **Observed behavior:** Two independent failures, each reproduced twice on 2026-08-24
  (macOS arm64, launcher v15.0.0):
  1. *Management canister updates are rejected by the launcher gateway.* After a successful
     launch (root-key fetch over `http://localhost:8000/api/v2/...` succeeds), the first
     management-canister update call fails:
     `update create_canister: The replica returned an HTTP Error: Http Error: status 400 Bad Request, content type "text/plain; charset=utf-8", content: error: canister_not_found details: The specified canister does not exist.`
     The read-side (`fetch_root_key`, i.e. `read_state` on `aaaaa-aa`) reaches the replica,
     so the network is up — the rejection is specific to update calls targeting
     `aaaaa-aa`. Because `create_canister` is exactly how `network start` deploys
     Account/Provision (`deploy_canister` → `install_canister`), no canister is deployed
     and the mapping file is never written.
  2. *The launched network dies with the parent CLI process.* In both runs the status file
     `$TMPDIR/gleaph-local-status/status.json` and the gateway listener on port 8000 were
     gone seconds after `gleaph network start -d …` exited. `-d/--background` detaches the
     child's stdio but does not setsid it, so the launcher (and its PocketIC process chain)
     does not survive the CLI session that started it.
- **Expected or needed behavior:** `gleaph identity new dev && gleaph network start -d …`
  followed by data-plane commands must work with only a clean checkout and Rust toolchain —
  per the plan 0296 quickstart contract — including management-canister deployment of
  Account/Provision from local wasm artifacts and a launcher that persists independently of
  the starting process.
- **Evidence:** `/tmp/gq-netstart2.log` through `/tmp/gq-netstart4.log` in the 0296 slice
  session; fix for the related staging defect (v15 tarballs nest the binaries under a
  versioned directory) landed as `normalize_extracted_launcher` in `crates/cli/src/network.rs`
  with unit tests (`normalize_stages_nested_launcher_and_companion`,
  `normalize_is_idempotent_when_flat_binary_already_cached`,
  `normalize_fails_closed_when_the_archive_has_no_launcher`).
- **Impact:** The knowledge-demo quickstart cannot use the pure-CLI bring-up path; it runs
  against an icp-cli managed network with a directly deployed platform instead (see plan
  0296 revision 3). No data-plane or authorization impact.
- **Next decision:** Future slice "pure-CLI bring-up": (a) determine whether the launcher
  gateway can route management-canister updates (or whether Account/Provision must be
  installed via a different entrypoint), (b) daemonize/supervise the launcher chain so `-d`
  outlives the CLI (setsid + optional supervisor), (c) then revisit whether lazy issuance
  plus provisioned topology completes without catalog-upload plumbing (ADR 0036 has no CLI
  upload surface today). Decided 2026-08-26 ([ADR 0087](adr/0087-wasm-ingestion-operations-model.md)):
  the catalog-upload surface lives in `gleaph-operator` over a shared ingestion client library,
  not the developer CLI; pure-CLI bring-up seeds the local catalog through that library.

  **(a) resolved 2026-08-26.** The launcher gateway *can* route management-canister updates;
  the two observed failures were operator-side effective-canister-id routing, not a gateway
  limitation. `gleaph-operator bootstrap deploy` now (1) uses `provisional_create_canister_with_cycles`
  for local/PocketIC endpoints (ingress-level `create_canister` has no derived-effective-id
  routing there), and (2) sets the effective canister id per call: the target canister for
  `upload_chunk`/`install_chunked_code`/`stop`/`start`/`canister_status`, and the network's
  default effective canister id (read from the `/_/topology` endpoint, the same source dfx's
  `dfx info default-effective-canister-id` uses) for the provisional create, whose response
  certification requires the effective id to fall within the target subnet's canister ranges.
  Verified end to end against a locally launched launcher network: create → chunked install →
  stop → upgrade → start with exact module-hash match. (b) daemonization remains open.

  **(b) resolved 2026-08-26.** `spawn_launcher` now calls `setsid()` (via `Command::pre_exec`)
  in background mode, creating a new session so the launcher (and its PocketIC process chain)
  survives the CLI's exit and a terminal close. Verified: `gleaph network start -d` run inside a
  pseudo-terminal leaves the launcher alive after the pty closes (previously it died on SIGHUP).

  **Remaining: CLI `network start` deploy still fails.** The operator's bootstrap tier is fixed,
  but `crates/cli/src/network.rs` `deploy_canister` still calls `create_canister` (no
  `provisional_create_canister_with_cycles`, no per-call effective canister id), so the
  pure-CLI bring-up deploy rejects with `canister_not_found` on the launcher network. The same
  GAP-2026-08-24-006(a) fix must be applied to the CLI's `RemoteTransport` management calls
  before `gleaph network start` completes.

  **Resolved 2026-08-27 — CLI deploy + Provision grant-model refinement (c).**
  Two changes completed the pure-CLI bring-up:

  1. *CLI deploy:* `RemoteTransport` mirrors the operator fix — local deploys go through
     `provisional_create_canister_with_cycles` with per-call effective canister id (target
     canister for install, `/_/topology` default for the provisional create). Additionally the
     third launcher-start failure surfaced and was fixed: a reused, incomplete PocketIC state
     dir panicked the launcher at startup (`nonblocking.rs: The state of subnet … is
     incomplete`); `spawn_launcher` now removes the state dir before each start (the official
     icp-cli pattern), redirects stdout/stderr to `launcher.stdout.log`/`launcher.stderr.log`,
     and detects premature launcher exit, surfacing the stderr tail. It also refuses to start
     a second launcher while one is alive (`gleaph network stop` first).

  2. *Provision trust-model redesign (deployment grants).* The per-deployment
     `DeploymentBinding` (router/governance/bootstrap principals + binding_version) is replaced
     by a single set of authorized issuers:

     - `DeploymentTrustStore` → `DeploymentGrantStore` = `StableBTreeSet<Principal>`; the set
       holds principals authorized to request issuance. `admin_install_deployment_binding` →
       `upsert_deployment_grant` (governance-only, idempotent; `BootstrapAuthAction::Upsert`).
     - `DeploymentBinding.router_principal/governance_principal/bootstrap_principal/binding_version`
       and the `complete_bootstrap` handover are all removed: deployment_id was a map key
       duplicated in the value, binding_version was never read for logic, and the bootstrap
       handover existed only to break a circularity that the set model dissolves. Each deploy is
       independent — the account (via the Account canister) issues the first Router; the issued
       Router is auto-granted at install time and issues graph resources; no transition concept.
     - `ProvisionInitArgs` = `{ governance_principal }` (the only authority established at
       init; grants are seeded per issuer afterwards). The CLI `network start` passes its
       session principal as the governance authority.
     - Envelope auth is set membership (`caller ∈ grants`); the deployment_id in an envelope is
       data (the Account canister issues under the user's account principal, the Router under
       its own). Authorization never depended on the deployment id again.
     - Operator CLI `binding install` → `grant upsert <ISSUER>`; init-args JSON is
       `{"governance_principal": "…"}`.

     Verified: provision (110 unit tests), router (1043), operator (40), cli (142), account,
     and the PocketIC E2E suite for the provision-affected targets (adr0035 callable
     endpoints/outbound, adr0068 deployment/issuance, adr0087 bootstrap tier/operator
     ingestion, text_index_provisioning, adr0070 fixture family 1). Known pre-existing E2E
     failures unrelated to this change: adr0070 fixture 2 (`NotFound("graph context")` — RBAC
     gate precedes the catalog-DDL path), adr0034 (vector stream WIP), router_gql_query
     barrier test (ADR 0029 query/update export mismatch).

### GAP-2026-08-24-002 — Edge-index anchors do not accept `ScanValue::InList` (symmetric extension of the vertex IN-list anchor)

- **Status:** Resolved (this commit)
- **Owner:** `gleaph-gql-planner` (`EdgeFilterFusion`, `parse_edge_var_property_equality`) +
  `gleaph-graph` executor (`execute_edge_index_scan`) + Router seed lowering (`edge_equal_anchor`)
- **Observed behavior:** The IN-list anchor slice (commit series ending with the vertex IN union)
  lowers only vertex predicates (`WHERE v.<indexed-prop> IN […]`) into `ScanValue::InList` union
  point probes. An edge predicate `WHERE e.<indexed-prop> IN […]` keeps its single-value
  `indexed_equality` path and evaluates the list as an ordinary residual filter.
- **Expected or needed behavior:** Symmetric with the vertex anchor: a scannable non-negated edge
  IN list could probe each element against the edge property index and union the postings.
- **Evidence:** `crates/gql-planner/src/planner/match_plan/path/filters.rs` (vertex lowering),
  `crates/router/src/seed.rs` (`IndexAnchor::EqualUnion`, edge arms unchanged).
- **Impact:** Edge IN queries stay correct but scan more incident edges than necessary; no
  correctness gap.
- **Next decision:** Whether edge equality fusion should reuse `ScanValue::InList` directly or add
  an edge-specific multi-probe shape that also carries the label/direction subset rule.
- **Resolution (this commit):** Reused `ScanValue::InList` directly — no new wire or planner
  shapes. The planner fuses a scannable non-negated edge IN conjunct through the existing
  `EdgeFilterFusion.indexed_equality` slot (`parse_edge_var_property_inlist` + shared
  `find_first_indexed_edge_bound_in_conjunctions`; every equality bound wins before any IN fusion,
  NOT IN never anchors, and the fused bound is removed from residuals exactly like fused equality).
  All three executor consumers of `indexed_edge_equality` gained per-element probe unions
  (`execute_edge_index_scan`, `edge_equality_stream_filter`, `expand_candidates_via_equality_index`;
  Null elements contribute no probe, missing parameters fail closed, duplicate list elements
  deduplicate on edge identity, `cmp != Eq` stays fail-closed). The Router extracts
  `IndexAnchor::EdgeEqualUnion` in all three seed-extraction sites with per-probe ADR 0012
  wire-label subsets and global `(shard, owner, label, slot)` deduplication; `resolve_scan_value`'s
  InList arm remains a malformed-plan rejection because unions expand before resolution. Owning
  tests: `crates/gql-planner/tests/planner_tests.rs::match_edge_*inlist*`,
  `crates/graph/src/plan/query/executor/scan/tests.rs::{executes_edge_inlist_*,edge_inlist_*,indexed_edge_inlist_queries_end_to_end_match_equality_semantics}`,
  `crates/router/src/seed.rs::tests::edge_inlist_index_scan_extracts_edge_equal_union_with_per_element_probes`.

### GAP-2026-08-24-003 — Cypher bracket-form `IN […]` fails to parse when `sql-compat` is enabled

- **Status:** Resolved — one-token opener lookahead in the sql-compat arm; fix commit
  `37d59c7f9`, owning regression tests
  `gql::parser_tests::in_list_combined_build_serves_both_list_forms`,
  `in_list_is_rejected_in_the_default_build` (default cell),
  `in_list_cypher_only_build_serves_both_list_forms` (cypher-only cell),
  `in_list_sql_compat_only_build_serves_paren_and_rejects_bracket` (sql-compat-only cell), and the
  combined-build graph e2e
  `parsed_bracket_in_list_returns_matching_rows_under_combined_dialects`
- **Owner:** `gleaph-gql` parser (`crates/gql/src/parser/expr.rs`, `IN` predicate arms)
- **Observed behavior:** With both `cypher` and `sql-compat` features enabled, `expr IN [v1, v2]`
  failed with `expected '(', got '['`. The `sql-compat` arm ran first and unconditionally required
  `(` after `IN`; its comment claimed precedence "keeps behavior unchanged", but it aborted parsing
  instead of falling through to the cypher arm's bracket form.
- **Fix:** The sql-compat arm now claims the `IN` predicate only when the token after `IN` is `(`
  (`peek_ahead` off the same keyword test that decides the `IN`/`NOT IN` shape). A non-`(` opener
  falls through untouched: in combined builds the cypher bracket grammar serves it; in a
  sql-compat-only build no arm claims it and the query still fails as a parse error. Single-feature
  and default configurations keep their exact acceptance sets; only the error text for a rejected
  bracket form under sql-compat-only shifts from "expected '('" to the statement-level leftover
  token error.
- **Evidence:** Before the fix, `in_list_combined_build_serves_both_list_forms` failed at parse;
  with the fix all four feature combinations pass their pinned cells
  (`cargo test -p gleaph-gql [features] in_list`: default 1, cypher 5, sql-compat 3,
  `cypher,sql-compat` 7) plus the combined-build e2e returning rows for
  `WHERE p.age IN [5, 7]`.
- **Impact:** Bracket-form IN queries parse and execute under combined dialect builds; feature
  combinations no longer diverge on `IN […]`. Default canister dialect unaffected.

### GAP-2026-08-24-004 — Endpoint property projection on `EdgeBindEndpoints` breaks trailing `IsLabeled` filters

- **Status:** Resolved — planner-side entity-use veto; fix commit `0cade11a8`,
  owning regression tests
  `gql-planner::property_projection::tests::is_labeled_residual_keeps_bind_endpoint_slot_full`,
  `planner_tests::trailing_is_labeled_residual_keeps_edge_bind_endpoint_vertex_binding` (+ near,
  positive-control, and cypher/entity variants), and graph e2e
  `parsed_is_labeled_residual_with_projected_far_endpoint_returns_matching_rows`
- **Owner:** planner late-projection pushdown (`crates/gql-planner/src/property_projection.rs`)
  exclusively; executor binding layout and the fail-closed non-Vertex `IsLabeled` semantics are
  unchanged
- **Observed behavior:** For a parsed leading-edge-anchor query whose projection touches an
  endpoint property (for example `MATCH (a:X)-[e:R WHERE e.weight = 5]->(b:Y) RETURN b.name`),
  the planner pushed `far_property_projection = ["name"]` into `EdgeBindEndpoints` (late
  projection) and still emitted `PropertyFilter(IsLabeled(b))` after it. Execution bound `b` to a
  projected `PlanBinding::Value(Record)` (`vertex_binding_for_projection`, `Some(props)` arm), so
  the subsequent `IsLabeled(b)` check no longer saw a vertex binding and dropped **every** row.
  Root cause: the pushdown veto (`must_remain_vertex`) only saw structural uses (SEARCH bindings,
  traversal sources); expression-level entity use was invisible because `var_used_as_non_property_receiver`
  short-circuited `ExprKind::IsLabeled` to `false`, so the RETURN-side `PropertyAccess(b, name)`
  alone decided the projection.
- **Fix:** Added a third downstream collector, `collect_entity_used_bindings`, which walks the
  suffix expressions and records every `Variable(v)` occurrence except direct `PropertyAccess`
  subjects, and unioned it into `must_remain_vertex`. All scan/expand/bind slots now veto to
  `ScanProjectionPatch::FullProperties` while any entity-level use remains downstream — including
  `expand_edge`, which previously ran its inference without any identity veto. Over-veto is
  conservative (full hydration = pre-pushdown behavior), never changes results.
- **Evidence:** Before the fix, the e2e regression test returned 0 rows for the projected query
  with the unprojected whole-variable twin returning 1; after the vetoes, both return the matching
  row. Planner-level repro pinned at plan level: `far_property_projection` stayed `Some(["name"])`
  with a trailing `PropertyFilter(IsLabeled(b))` before the fix, `None` after.
- **Impact:** Property-level projections over labeled endpoints now execute correctly for anchored
  edge scans; only queries mixing endpoint property reads with entity-level use of the same
  variable lose the projection optimization (correct results either way). Whole-variable
  projections were never affected.

### GAP-2026-08-24-001 — Two stale router lib fixtures surfaced on the suite's first full run after the 8e392c127→ac380c4bb build-broken window

- **Status:** Resolved (this commit)
- **Severity:** P3 test fixture — the fail-closed verification logic was correct in both cases; no
  production defect
- **Owner:** `gleaph-router` native unit-test fixtures (`facade/stable/indexed_catalog.rs`,
  `vector_sync.rs`)
- **Observed behavior:** Once GAP-2026-08-23-001's ic0-context deferral unblocked
  `cargo test -p gleaph-router --lib` on this host, the first clean full run failed 3 of 937 tests,
  all stale fixtures rather than regressions:
  1. `facade::stable::indexed_catalog::tests::maintenance_projection_preserves_namespace_phase_and_epoch`
     asserted `vertex_indexes.len() == 1` but observed 0. Since
     `8f3f5c1da fix(index): fail closed on inconsistent vertex leaf identities`, the catalog projection
     requires `vertex_leaf_projection(graph_id, property_id)` to resolve the reverse property name and
     omits whole rows it cannot resolve; the fixture inserted the physical-101 Active Vertex record with
     property id 5 without interning any reverse name, so the row was (correctly) omitted.
  2. `vector_sync::tests::frontier_response_loss_retains_exact_marker_snapshot` expected the publisher to
     observe `(Principal([1;29]), ShardId(2), 52)` but saw no call. Since
     `128940f51 feat(router): publish markerless vector lane frontiers`, frontier publication is
     catalog-driven (`run_recovery_pass` → `run_catalog_frontier_pass` →
     `graph_catalog::scan_attached_vector_lane`, which reads `ROUTER_SHARDS`); the test seeded only outbox
     markers with no attached lane, so `publish_router_frontier` was unreachable. The third failure,
     `resolved_rows_transition_to_awaiting_frontier_before_publish`, was lock-poison cascade from item 2's panic.
- **Expected or needed behavior:** Fixtures must satisfy the current contracts without weakening any
  assertion. Both verifications are intended fail-closed behavior: an unresolvable leaf identity omits the
  catalog row whole instead of flattening it, and frontier publication requires a catalog-attached lane.
- **Fix:** Item 1 interns the Active Vertex leaf's flat property name through the module's existing
  `intern_property` helper and passes the returned id into the fixture record; the assertions key on
  `physical_index_id`/`catalog_epoch` so they are property-id agnostic. Preparing/Aborting rows need no
  name because `maintenance_phase` excludes them before projection, and the Building/Sealing rows are
  Edge-kind. Item 2 adds a local `attach_catalog_lanes` helper in the vector_sync tests module (isomorphic
  to the outbox-side fixture) registering exactly the `(Principal([1;29]), ShardId(2))` lane that
  `intent()` targets; `clear_for_test()` already resets `FRONTIER_CATALOG_CURSOR`/`FRONTIER_LANE_PROGRESS`.
  No assertion or verification path changed.
- **Owning tests:** the two named tests plus the de-poisoned
  `vector_sync::tests::resolved_rows_transition_to_awaiting_frontier_before_publish`; full
  `cargo test -p gleaph-router --lib` green (937 passed).

### GAP-2026-08-23-004 — Router canister wasm exceeds the IC code-section limit once the ADR 0074 slice 2a grant grammar is linked

- **Status:** Closed (2026-08-23) via prerequisite plan `0288` (router wasm budget recovery).
  Decision: build-path unification plus release-profile levers instead of source-level
  slimming. Every Gleaph canister build path (root `icp.yaml` script adapters,
  `scripts/deploy-demo-local.sh`'s instrumented router, the PocketIC fixture build,
  `scripts/check-codegen-local-e2e.sh`) now runs one shared post-processing entry point,
  `scripts/postprocess-canister-wasm.sh` (`ic-wasm` metadata insertion → candid extraction →
  `ic-wasm shrink --keep-name-section`; the name section stays kept deliberately — it is a
  custom section and does not count against the code-section limit), and `[profile.release]`
  gained `lto = "fat"` + `codegen-units = 1`. Binaryen `wasm-opt` was not needed.
- **Observed behavior:** `cargo test -p gleaph-pocket-ic-tests` fails every suite with
  `Wasm module code section size of 12615453 exceeds the maximum allowed size of 12582912`
  during Router install (PocketIC enforces the mainnet limit). Measured code sections of
  `gleaph_router.wasm` (wasm64, release, E2E features): HEAD + concurrent-session overlay
  without slice 2a = **11,942,769**; with slice 2a = **12,123,923** (+181,154); six-package
  feature-unified E2E build = **12,615,455** (+32,543 over the limit).
- **Attribution evidence:** name-section diff of the two single-package binaries shows the
  largest genuinely new linkage is `data_encoding` decode machinery (**+61,772 bytes**, 19 → 49
  functions), introduced by the first production use of `candid::Principal::from_text` in
  `crates/router/src/gql_grants.rs::bind_subject` (all pre-existing `from_text` call sites are
  test-only; `to_text` was already linked via provisioning `deployment_id`). An isolated
  experiment replacing `from_text` with `from_slice` shrinks the binary by 76,068 bytes,
  confirming the attribution; the remaining ~105KB of delta is inlining-budget churn spread
  across unrelated shared symbols (growth and shrinkage nearly cancel: +1.87 MB / −1.87 MB).
- **Needed behavior:** ADR 0074 §5 requires the integration layer to bind `PRINCIPAL <literal>`
  subjects to identities, which requires principal-text decoding in the Router; the IC
  install-time code-section ceiling is hard on both mainnet and PocketIC.
- **Owner:** Router canister build composition (`crates/router`, workspace release/wasm64
  profile, `crates/pocket-ic-tests/build.rs`). The principal-text contract itself stays owned
  by `candid`; a hand-rolled base32/CRC decoder in the Router was considered and rejected as a
  duplication of candid's identity-validation logic. Resolution adds
  `scripts/postprocess-canister-wasm.sh` and the workspace `[profile.release]` as the owning
  size-budget surfaces.
- **Impact:** ~~all PocketIC E2E suites are blocked until the Router drops ≥33 KB below the
  current footprint~~ resolved: see resolution evidence below; plan `0287` todo
  `pocketic-lifecycle-tests` is unblocked.
- **Resolution evidence (2026-08-23, plan `0288`; limit 12,582,912 code-section bytes,
  measured with `scripts/wasm-code-section-size.py`):**
  - PocketIC fixture path (six-package feature-unified wasm64+SIMD release), isolated levers:
    raw cargo output **12,123,923** → + `[profile.release] lto="fat", codegen-units=1`
    **10,689,428** (−1,434,495) → + shared post-processing **9,634,923** (−1,054,505).
    Headroom vs the limit: **2,947,989 bytes (~2.81 MiB)**, ~11× the required 256 KiB.
    Today's pre-change reproduction measures lower than the recorded failing build
    (nightly `c656540d6 2026-08-21` and/or concurrent-session overlay churn; not bisected —
    applying the same measured deltas to the recorded 12,615,455 footprint estimates ≈10.13 MB,
    still ≥2.4 MiB under).
  - Per-path router code sections after both levers: deploy (`icp build gleaph-router`)
    **9,597,708**; demo instrumented (`batch-instr-log`, feature-guard grep verified)
    **9,614,992**; PocketIC fixture **9,634,923** — all paths installable with ≥2.8 MiB margin.
  - Other canisters strictly smaller than before on every path (PocketIC fixture code
    sections, before → after): graph 7,405,381→6,094,339; graph-index 2,098,383→1,744,550;
    vector 1,890,704→1,512,047; account 973,145→805,582; provision 1,677,761→1,382,074.
  - Blocked validations rerun green: `smoke` 1/1, `adr0074_auth_ingress_walk` 1/1,
    `adr0074_grant_grammar` 4/4 (first-ever execution exposed a self-contradictory hardcoded
    subject expectation in the suite's own `expect_grant_summary` helper — it asserted `Public`
    while its callers grant to `PRINCIPAL` and assert the principal subject; the helper now
    takes the expected subject, no canister-side change was needed, stored rows were correct).
- **Next decision:** none (closed). Standing watch: cross-nightly rebuild drift is real
  (~±500 KB observed); keep the ≥256 KiB headroom check per canister when touching canister
  features or toolchain versions, measured with `scripts/wasm-code-section-size.py`.

### GAP-2026-08-23-003 — ForceAtlas2 equilibrium rests below the rendered node diameter once nodes are world-sized

- **Status:** Closed (2026-08-23). Decision: recalibrate the repulsion-scaling
  default so the force balance itself rests outside marker overlap
  (`scaling = 432`, derived from `d* = (S·m²)^{1/3}` with mass 2 pairs equaling
  the canonical 12-unit node diameter), documented in DESIGN.md §15.1. A hard
  pairwise separation floor was prototyped first and rejected: dense topologies
  make it unsatisfiable (hub/256 leaf ring spacing ~1.2 units against a 12-unit
  floor), so the constraint fights the force law forever and the layout never
  settles. Residual limitation, accepted as capacity physics rather than a
  defect: leaves around a massive hub still rest partially overlapped
  (~9 units apart at hub/256) because their packing density exceeds what any
  spacing floor allows — matching reference FA2 behavior. If overview-zoom
  readability of such dense hubs ever matters, revisit render-side marker LOD
  as a separate slice.
- **Resolution evidence:** undilated headless probe over the example and
  tolerance fixtures (24-node demo, 12-node team start, hub/256 ring, grid/20x20@40):
  average relaxed edge length moved from 4.8 / 3.9 / 10.6 / 9.0 world units
  (default scaling 1) to 35.8 / 29.7 / 73.8 / 33.3 (scaling 432) — every family
  at or beyond ~2.5× the rendered diameter except capacity-limited hub leaves;
  settle iterations stayed far inside the contract budget (156 / 109 / 128 /
  471 against < 1500); `cargo test -p gpui-graph --lib` 217 passed including
  `settles_within_iteration_budget` and `slow_down_scales_per_iteration_motion`.
- **Follow-up (2026-08-23, same day):** the canonical `node_radius` default was
  halved to 3 (diameter 6 world units); by the same pair-equilibrium derivation
  the FA2 scaling default moved 432 → 54 (`d* = (54·2²)^(1/3)` = 6). Fixture
  ratios were preserved (demo/24 ratio 3.01 / team 2.50 / hub 6.45 / grid 3.47;
  settle iterations 204 / 66 / 195 / 528), confirming the calibration is
  scale-free. DESIGN.md §15.1 records the current numbers.
- **Severity:** P2 visual quality — demo-scale graphs relax into overlapping node blobs over
  tens of seconds; static and freshly-opened views are unaffected.
- **Owner:** `crates/gpui-graph/src/layout/force_atlas2.rs` force balance, against the
  world-sized node contract (`3a52ee395`, DESIGN.md §26.2) and the default placement contract
  (DESIGN.md §13).
- **Observed behavior:** Headless probe mirroring the `force_atlas2` / `interactive` examples
  (24-node random hub graph, 12-node start graph, FA2 defaults, one iteration per frame): from
  the ±200-unit spread start, the average edge length falls from ~205–233 world units at frame
  0 to **4.7–6.5 by settle** (frame ~1400 undilated pace) — i.e. below half the rendered node
  diameter (`2 * node_radius` = 12 world units), with a minimum pairwise node distance of
  ~2.3 units. Before world-sized nodes this contraction was invisible (nodes drew at a fixed
  6 px); now the settled state renders as heavily overlapping discs.
- **Mechanism:** linear attraction grows with distance while repulsion falls off as
  `1/d²`, so the pairwise equilibrium sits near `scaling^(1/3)` times small mass factors —
  well inside one node diameter at the default `scaling = 1`. A universal hard separation
  floor was prototyped and rejected: dense topologies make the floor unsatisfiable (hub/256
  leaves around their center equilibrate at ring spacing ~1.2 units; enforcing ≥12 units per
  pair puts the constraint in permanent conflict with the force law, so the layout never
  settles — `layout::force_atlas2::tests::settles_within_iteration_budget` fails at budget),
  and raising attraction-side softening or `scaling` alone either misses the floor or breaks
  convergence contracts elsewhere.
- **Impact:** relaxed layouts eventually render overlapping markers in every app using FA2
  defaults with the canonical style; hit-testing ambiguity follows visually.
- **Next decision:** pick one contract before coding: (a) a size-aware FA2 parameter with a
  capacity-aware relaxation that tolerates density where the floor cannot hold (Gephi-style
  no-overlap is approximate for exactly this reason); (b) render-side marker LOD that shrinks
  or fades markers when local density exceeds what world-sized discs allow; or (c) document
  per-app tuning of `with_scaling`/`node_radius` pairs and accept blobbing for dense hubs.
  Record the decision in DESIGN.md §11/§26.2 and close this entry in the fixing commit.

### GAP-2026-08-22-001 — Migration-driven directed edge builds reject at graph-index seeding: wire vs catalog label identity divergence

- **Status:** Closed (2026-08-23; identity rule decided and implemented per the Next decision
  below — plan 0282). Owning evidence: graph-index unit contract green
  (`cargo test -p gleaph-graph-index --lib` 153 passed / 0 failed, including
  `directed_wire_fact_seeds_under_catalog_target_and_catalog_sieve_finds_it`,
  `any_direction_target_accepts_both_bucket_packings_of_its_catalog_label`,
  `facts_from_a_different_catalog_label_are_rejected_and_store_nothing`, and
  `dml_build_subjects_carry_wire_labels_against_catalog_targets`); cross-canister closing proof
  green (`cargo test -p gleaph-pocket-ic-tests --test adr0059_index_build_lifecycle`
  **5 passed / 0 failed** with both previously-ignored inline scenarios un-ignored and passing:
  `edge_inline_create_index_migration_converges_active_with_complete_postings` converges a
  directed ROAD + undirected LINK migration over pre-existing two-shard inline values with
  posting-level shard/canonical-owner assertions, and
  `edge_inline_same_wasm_upgrade_mid_build_resumes_and_converges` preserves watermarks across a
  same-wasm mid-build upgrade of all five federation canisters); regression guards green
  (`router_gql_query` 24 passed / 0 failed including every standalone edge lifecycle target).
- **Severity:** P0 index correctness — blocks the edge half of ADR 0059's lifecycle and therefore
  the GAP-2026-07-29-001 E2E closure
- **Owner:** The Router→Graph→graph-index edge build identity contract: Router registration
  (`crates/router/src/index_catalog.rs::resolve_index_definition`,
  `facade/store/schema_migration/index.rs::step_request`), Graph export fact emission
  (`crates/graph/src/index/canonical_export.rs::export_edge_sidecar_page` /
  `export_edge_inline_page`), and graph-index identity enforcement
  (`crates/graph-index/src/facade/store/build_state.rs::prepare_fact_posting`)
- **Observed behavior:** Both new PocketIC scenarios in
  `crates/pocket-ic-tests/tests/adr0059_index_build_lifecycle.rs`
  (`edge_inline_create_index_migration_converges_active_with_complete_postings`,
  `edge_inline_same_wasm_upgrade_mid_build_resumes_and_converges`, currently `#[ignore]`d) drive a
  combined `CREATE INDEX ... FOR ()-[e:ROAD]-() ON (e.distance)` + undirected LINK migration
  through `apply_schema_migration` over a two-shard federation with pre-existing inline edge
  values. The first (directed ROAD) sub-build registers and reaches Building, then terminates with
  `SchemaMigrationApplyStatus::Failed(TargetRejected)`; in the upgrade variant
  `IndexBuildProgress.seeded_items` stays 0 until the round budget exhausts. Vertex builds are
  unaffected (their facts carry no label).
- **Mechanism (static chain, each link read directly on main `306f469c1`):**
  1. The Router registers the build target with the untagged catalog label id:
     `resolve_index_definition` uses `lookup_edge_label_id(...).raw()` and
     `IndexDefRecord.label_id` persists it; `step_request` copies it into
     `IndexBuildTarget::Edge { label_id }`.
  2. Graph storage keys edges by tagged wire labels (`BUCKET_LABEL_DIRECTED_BIT = 0x8000`;
     directed bucket key = catalog id | 0x8000, undirected = catalog id). Both edge export pages
     emit facts whose `label_id` is that storage value (`key.label_id()` / `edge.label_id()`).
  3. graph-index requires exact equality between fact label and registered target label
     (`target_label == label_id`) and returns `InvalidIndexBuildTarget` otherwise.
  4. `InvalidIndexBuildTarget` is not retryable, so the driver classifies it as
     `MigrationFailureCode::TargetRejected`.
  For a DIRECTED edge, wire (`id | 0x8000`) never equals the registered catalog id, so every seed
  page rejects.
- **Expected or needed behavior:** One owner must define the posting label identity end-to-end so
  migration-seeded postings, DML-maintained postings, and read-side binding agree for both
  directedness buckets. Note the same divergence family exists on the Active DML/read path today:
  DML maintenance inserts postings under wire labels
  (`crates/graph/src/index/edge_pending.rs::push_edge_index_op` receives the canonical wire label)
  while expand lookups sieve with resolved catalog ids
  (`plan/query/executor/expand/candidates.rs`), so directed-edge equality probes through
  graph-index return zero hits and silently degrade to scan fallbacks
  (`expand_candidates_via_equality_index` treats empty postings as "index did not own the lookup").
  Single-shard lifecycle tests pass because of that fallback, masking the divergence.
- **Evidence:** Scenario run 2026-08-22: inventory precondition `router_gql_query` 24 passed /
  0 failed; `adr0059_index_build_lifecycle` 3 passed / 2 failed with
  `Failed(TargetRejected)` and a stuck-at-zero `seeded_items`. Unit coverage gap: graph-index's
  build-state tests use one arbitrary label id (9) for both target and facts, so the divergence
  has no unit-level signal.
- **Impact:** No index over a directed edge property can be created through the migration
  lifecycle (terminal failure); GAP-2026-07-29-001 cannot close. Undirected-only registrations
  happen to satisfy the numeric identity check but then bind read handles from catalog-id
  postings, so fixing only the register side without owning the identity decision would leave
  read binding broken for one bucket packing.
- **Next decision:** Decided (2026-08-22, plan 0282 survey): **edge postings are stored and
  transported in wire-tagged space (`catalog id | directed MSB` directed, bare catalog id
  undirected); registrations, memberships, and lookup sieves speak catalog ids.** Rationale:
  every physical producer already emits wire labels (DML dispatch from canonical handles,
  repair journal, operator backfill export, migration seed facts) and read binding resolves
  `EdgeHandle`s from posting labels against LARA bucket keys, so wire storage needs zero
  producer churn and keeps handle binding unambiguous; the catalog-space alternative would
  force a translation onto every Graph producer and make handle binding direction-ambiguous.
  The single translation owner is `gleaph_graph_kernel::entry::label` — `EdgeLabelId`
  (catalog) ↔ `TaggedEdgeLabelId` (wire) via `pack()` / `label_index()`; no ad-hoc `0x8000`
  arithmetic outside it. Conforming seams: (S1) graph-index `prepare_fact_posting` accepts a
  fact when its wire label's catalog index equals the registered catalog label and its bucket
  is covered by the registration direction, storing under the wire tag (`EdgePostingKey`
  layout and Candid types unchanged); (S2) `ensure_subject_matches_target` applies the same
  rule to DML build subjects; (S3) graph-index `lookup_edge_{equal,range}_page` treat
  request labels as catalog ids and scan both packings; (S4) Graph's
  `lookup_edge_{equal,range}_local` resolve memberships by direct catalog equality instead of
  feeding a catalog id into the wire-expecting registration matcher. Router registration,
  export scopes/walks, planner stats, and all Graph producers stay unchanged.

### GAP-2026-08-21-002 — Graph registration completion held bootstrap intent locks across a deployment's graph bootstraps

- **Status:** Resolved 2026-08-22 by `3ef8fe9d53f503b9663e1e5fe324741a16005559`
- **Severity:** P2 provisioning lifecycle
- **Owner:** Router Graph provisioning convergence and Provision's versionless `complete_graph_registration` endpoint
- **Historical observed behavior (before the 2026-08-22 working-tree fix):** After a successful `CREATE GRAPH` bootstrap, the Router-side request record stayed `AwaitingAck` and its `(deployment_id, GraphShard(0))` intent lock stayed held because no Provision→Router ack arrived. A second `CREATE GRAPH` by the same caller principal failed with `Conflict("provisioning intent already locked")` indefinitely (`adr0070_create_graph_provisioning.rs` first exercised this as an infinite retry).
- **Expected or needed behavior:** Once Router has reconciled the exact Graph topology, its versionless completion call must mark the Provision job `Completed` and release only that request's Map 2/3 and Map 47 rows, so sequential bootstraps converge.
- **Evidence:** Fixing commit `3ef8fe9d53f503b9663e1e5fe324741a16005559`. Exact Router owner
  tests cover retry-before-early-return, partial registration, lost-response replay, byte-exact reopen,
  and owned Map 47 release. The Provision owner lifecycle test drives the production accept and
  completion handlers across an actual Map 1/2/3 reopen, preserves exact created resources, effect
  count, and immutable-envelope digest on admission replay, and proves completed replay leaves a later
  request's Map 2/3 rows unchanged. On 2026-08-22 UTC, both focused PocketIC runtime targets passed:
  `create_graph_provisions_shard_and_sets_home_graph` proved same-caller consecutive graphs, and
  `router_graph_bootstrap_registration_ack_crosses_real_candid_boundary` proved the real
  Router→Provision Candid completion boundary (1 passed, 0 failed for each target).
- **Impact:** The exact `GraphShard(0)` path no longer has a one-graph-per-deployment restriction.
  Property- and Vector-index completion remain deferred to their owner-specific slices.

### GAP-2026-08-21-001 — Anchored multi-DML roll-forward saga fails to converge on both shards

- **Status:** Resolved 2026-08-22 (fix implemented on top of `7a75e9fd6` and validated there; commit pending at time of update)
- **Severity:** P1 DML correctness signal
- **Owner:** Graph vertex-index membership resolution (`crates/graph/src/index/catalog_context.rs::vertex_index_memberships_for_labels`), shared by single DML, bulk insert, and backfill; E2E fixture catalog guards.
- **Observed behavior:** In
  `crates/pocket-ic-tests/tests/router_gql_query.rs`, three consolidated contracts failed:
  `router_runs_anchored_multi_dml_bundle_across_shards_as_roll_forward_saga`,
  `router_recovers_anchored_multi_dml_roll_forward_saga_via_idempotent_retry`,
  `router_recovers_non_terminal_federated_saga_via_idempotent_retry`. The two-shard bundle reports
  `Completed`, but the post-commit read `MATCH (n {age: 6}) RETURN n` returned 0 rows instead of the
  expected 2, so the SET did not survive on either shard despite the completed phase.
- **Mechanism (probe-verified on `7a75e9fd6`):** Nothing removed the old posting. Fixture inserts
  posted through `e2e_legacy_unlabeled_vertex_catalog_guard`, which rewrote every vertex
  membership's `label_id` to 0 so unlabeled vertices matched the label-scoped index; bundle-time
  SET resolved memberships against the true Router catalog, where the same unlabeled vertex
  (`labels == []`) matched nothing, so neither remove-old nor add-new was ever enqueued
  (`pending_min == None` because the queue was never populated). Post-bundle reads still seeded
  from the stale posting (`lookup_equal` hits=2) but the shard-side residual equality filter
  dropped the rows because canonical age had changed. The labeled variant failed admission for a
  second reason: its fixture posted into a fabricated physical namespace (101) instead of the
  Router-allocated namespace (1), so anchor lookups saw no hits.
- **Expected or needed behavior:** A `Completed` roll-forward saga must leave both shards'
  anchor vertices updated; the read-back equality anchor must observe both.
- **Resolution:** `vertex_index_memberships_for_labels` now owns the maintenance rule in one
  place: a vertex with no labels maintains every namespace indexing the property (legacy-unlabeled
  contract, mirroring the Router's label-less superset anchor reads); labeled vertices maintain
  wildcard plus exact-label memberships as before; a missing vertex row dispatches nothing. The
  fixture-only wildcard rewrite and fabricated namespaces were deleted — fixtures fetch the real
  Router catalog via `e2e_router_catalog_guard`. Regression origin diff-triaged to `04575f280`
  (strict label-scoped DML membership resolution), not to the five commits named in plan 0269
  Step 2; no bisect was needed.
- **Owning regression tests:** graph-unit `unlabeled_legacy_set_reposts_remove_and_insert_into_label_scoped_membership`,
  `labeled_set_reposts_only_into_its_own_label_membership`,
  `remove_repost_removes_exactly_the_old_value_posting`,
  `multi_label_vertex_sharing_one_namespace_pushes_no_duplicate_ops`,
  `missing_vertex_row_dispatches_no_postings` (`catalog_context.rs` tests); PocketIC
  `router_labeled_insert_set_repost_converges_across_shards` plus the restored saga trio in
  `router_gql_query.rs` (20 passed / 0 failed at the fix).
- **Evidence:** Probe instrumentation over a pinned worktree of `7a75e9fd6` (no working-tree
  changes): fixture-phase `resolved=[(1,1)]` + `Insert` posting vs bundle-phase `resolved=[]`
  twice per shard; zero index-canister vertex-posting mutations during the bundle; post-bundle
  seed hits=2 sieved to 0 rows by the residual filter; labeled variant pre-bundle eq(5)=0 with
  `ix batch phys=101`.

### GAP-2026-08-20-001 — AtLeast graph-index barrier ignores pending first-delivery outbox work

- **Status:** Resolved (commit pending; fix is in this patch)
- **Severity:** P0 read consistency
- **Owner:** Router ReadMode::AtLeast barrier and the Graph-owned durable derived-index work boundary
- **Observed behavior (before fix):** GraphStore::index_pending_min_mutation_id read only the
  failure-only RepairJournal, while DerivedIndexOutbox was a separate durable FIFO whose entries
  retain their originating mutation_id. Router's AtLeast barrier therefore could consult only the
  repair owner.
- **Expected or needed behavior:** An index-backed AtLeast(M) read must fail closed until every
  derived posting at or below M has reached its target, including first-delivery and failed-flush
  work.
- **Resolution:** Graph MemoryId 52 now owns one fixed key for every qualifying source row in
  DerivedIndexOutbox and RepairJournal. GraphStore prevalidates and synchronously co-updates the
  source row and its exact floor key; zero-id and `IndexBuildDml` rows are excluded, quarantine
  preserves its key, and acknowledgements remove only exact applied rows. Router's existing barrier
  and wire shape are unchanged.
- **Evidence:** `fixed_keys_order_by_mutation_owner_and_sequence`,
  `fresh_and_reopen_preserve_exact_floor`,
  `co_updates_outbox_and_repair_without_zero_or_build_rows`,
  `quarantine_and_partial_ack_preserve_exact_multiplicity`,
  `prevalidation_failure_leaves_owners_and_floor_unchanged`, and PocketIC
  `first_delivery_outbox_survives_graph_upgrade_and_atleast_fails_closed_until_drain`.
  ADR 0029 §5 remains the AtLeast projection-barrier authority.
- **Validation status:** The five exact Graph owner tests, layout/stats tests, focused floor and
  reopen canbenches, Graph check/clippy, and stopped-index PocketIC lifecycle pass. Final unfiltered
  Graph benchmark persistence and final plan/scope gates are recorded in Plan 0263.
- **Impact:** A token can be treated as graph-index satisfied while its first delivery remains
  durable but unapplied, allowing an index-backed read to miss canonical state.
- **Next decision:** Keep the separate Vector Index acknowledgement/watermark/tombstone-retention contract and
  index-build generation visibility contract in their later slices; do not infer either from this
  ordinary Graph-owned floor.

### GAP-2026-08-20-002 — Router direct vector ingestion durable intent ownership

- **Status:** Resolved (2026-08-22; catalog-driven markerless frontier implementation and focused
  runtime/benchmark gates pass; the persisted Router artifact has all 41 terminal benchmark entries,
  including `markerless_frontier_catalog` total 52,080,138 and scope 52,079,143 instructions;
  targeted affected-crate format passes while the workspace-wide format check is blocked by unrelated
  dirty paths and never passed; finite-time liveness, global subject-map growth, and finite-time GC
  remain explicitly deferred)
- **Severity:** P0 durable derived-state contract
- **Owner:** Router direct vector-ingestion API and MemoryId 53 lifecycle
- **Contract:** Every allocated direct-ingestion mutation ID is synchronously co-written with one
  exact durable intent before the first Graph await. `Pending` is valid only while that intent is
  `AwaitingGraph`, `AwaitingVector`, or `AwaitingFrontier` in the sole MemoryId 53 lifecycle. The
  Router row durably owns pending/retry payload bytes until exact marker retirement; Vector owns the
  indexed embedding bytes after delivery.
- **Implementation:** `admit_awaiting_graph` validates the full batch, live shard, exact Graph and
  Vector targets, immutable definition, capacity, and encoded size before changing state. It then
  co-writes the final `ROUTER_MUTATION_COUNTER` value and every canonical `AwaitingGraph` row with no
  intervening await. Exact Graph acceptance changes only the matching row to `AwaitingVector`;
  exact logical rejection changes only that row to `AwaitingFrontier`; transport/decode failure and
  response loss retain the current phase. Vector typed-prefix acknowledgement changes only applied
  `AwaitingVector` rows to `AwaitingFrontier`. The canonical shard catalog enumerates only fully
  attached, non-anonymous `(Vector target, shard)` lanes. For the selected lane, Router derives the
  frontier from the oldest unresolved exact-lane intent minus one, or the durable allocation ceiling
  when none remains, and may publish an empty marker snapshot. Unresolved direct intents in another
  lane do not cross-block. The recovery cursor advances before await and attempts one lane per tick,
  so a failed lane yields later lanes a turn before retry after lap rotation. The Router-only Vector
  endpoint rechecks Router ownership and exact shard attachment, applies MemoryId 15 with `max`, and
  runs one bounded GC step in the same no-await update; its cutoff remains
  `min(graph_watermark, router_watermark)`. Recovery uses persisted targets for direct-ingestion rows
  and catalog discovery only for markerless lane enumeration.
  MemoryId 2 now stores the public shard projection and a Router-private monotonic Vector attach
  epoch in one canonical row. Unregister advances the epoch while clearing both readiness bits
  before the first detach await and returns the exact retained Vector target plus epoch as its
  continuation claim. Router revalidates that claim and both cleared bits before each outbound
  detach and atomically before final removal. Successful existing-row index re-registration
  advances the epoch once while clearing the old target; an already completed duplicate is a no-op.
  A new explicit same-target attach advances the epoch again, so every pre-unregister finalizer and
  unregister continuation exact-conflicts and only the new claim may publish readiness or retain
  the row.
- **Evidence:** `crates/router/src/facade/stable/vector_ingest_outbox.rs::{admit_awaiting_graph,
  observe_graph_accept,observe_graph_reject,apply_outcome,run_recovery_pass}`,
  `crates/router/src/facade/stable/graph_catalog.rs::scan_attached_vector_lane`, and
  `crates/router/src/api/control.rs::ingest_vertex_embeddings`. Owner-local tests cover admission
  atomicity, phase persistence, exact compare-before-transition, stale callbacks, reopen, Vector
  prefix retention, empty/non-empty frontier snapshots, catalog enumeration, one-lane rotation,
  malformed outcomes, and recovery scheduling. The focused PocketIC test
  `graph_only_markerless_lane_advances_frontier_on_autonomous_timer` passes exactly one test with
  seven filtered tests, one `PocketIc`, one federation bootstrap, and four canister installs (Router,
  Property Index, Graph, Vector); it covers ordinary timer dispatch, applied response loss, upgrade
  rediscovery, exact retry, and physical collection under the Vector
  `min(graph_watermark, router_watermark)` cutoff. The persisted Router
  `crates/router/canbench_results.yml` artifact has all 41 terminal benchmark entries;
  `markerless_frontier_catalog` records exactly 52,080,138 total instructions and 52,079,143
  instructions in its benchmark scope. The dated Plan 0277 focused/persisted Vector
  `bench_router_frontier_gc_budget` baseline records 2,110,300,092 total and 2,110,299,087 scoped
  instructions; the later live artifact currently records 2,084,327,695 total and 2,084,326,690
  scoped instructions from unrelated work and is not this slice's evidence. Nine exact Router
  owner unit tests pass:
  `stale_vector_attach_finalizer_after_unregister_reregister_requires_new_attach`,
  `same_target_vector_reattach_claim_fences_delayed_pre_unregister_finalizer`,
  `unregister_start_response_loss_then_register_requires_explicit_vector_reattach`,
  `second_graph_same_vector_target_and_shard_cannot_duplicate_lane_ownership`,
  `markerless_failure_rotates_to_next_lane_and_wraps_without_starvation`,
  `vector_attach_rejects_cross_page_duplicate_before_claiming_candidate`,
  `unregister_rejects_exact_pending_vector_work_without_lifecycle_mutation`,
  `unregister_shard_with_vector_target_removes_vector_readiness_row`, and
  `delayed_unregister_final_commit_cannot_remove_new_same_target_ownership_after_vector_detach`.
  The separate regression
  `delayed_unregister_cannot_detach_or_remove_new_same_target_ownership` also passes and protects
  the post-await unregister claim fence. Router check passes. Router all-target/all-feature clippy
  is blocked only by unrelated unused imports and one needless borrow
  in `crates/router/src/prepared.rs`; production-library strict clippy passes with no markerless
  diagnostic. The targeted affected-crate format check passes
  while the workspace-wide format check is blocked by unrelated dirty paths and never passed. The
  Quint refinement passes 15/15 deterministic tests and both 5,000-sample depth-35 safe-policy runs
  with all requested witnesses; `quint verify` remains unrun and non-required. This bounded model
  and the focused runtime evidence are not production proof of finite-time liveness or global
  subject-map growth.
- **Impact:** Restart or response loss cannot leave an allocated direct-ingestion stamp without a
  durable owner. Fully attached catalog lanes, including Graph-only lanes with no MemoryId 53 row,
  can now advance the Router frontier and use the existing
  `min(graph_watermark, router_watermark)` cutoff. The public operation remains at-least-once
  retry/convergence with no finite-time or client-level exactly-once guarantee, and no global
  subject-map growth or finite-time GC bound is claimed.
- **Next decision:** No remaining prerequisite exists for this bounded durable-ownership and
  markerless eligibility slice. Keep finite-time liveness, global subject-map growth, and finite-time
  GC as separate future evidence; do not infer those claims from the bounded Quint model or the
  one-lane runtime gate.

### GAP-2026-08-20-003 — CanonicalPending retry does not reconcile a completed Graph receipt

- **Status:** Resolved in `d700331c33a8cdae7524e76bcb4e9ddcd5cdb600`
- **Severity:** P1 Router exact-replay recovery
- **Owner:** Router ordered atomic_insert lifecycle and exact Graph journal reconciliation
- **Observed behavior (before fix):** ADR 0049 requires a same-key retry after a Graph success /
  Router callback loss to query the exact Graph journal and either record its completed receipt or
  resend the exact stored request. Ordered edge, vertex, and mixed recovery left CanonicalPending
  with an explicit-retry diagnostic, and an existing same-key request returned its Router record
  without reconciliation.
- **Expected or needed behavior:** If the exact Graph journal contains a completed receipt, retry
  must advance the Router record without canonical redispatch. Only an explicit exact retry may
  redispatch an unambiguously absent stored target; background recovery must remain query-only and
  fail closed.
- **Resolution:** Existing ordered CanonicalPending admission now reaches Router's private
  trigger-aware reconciliation boundary before fresh routing or catalog resolution. It reads the
  persisted Graph target and accepts only exact active, completed journal evidence to record the
  matching receipt without an ordered execution. Explicit same-key retry may redispatch only after
  an unambiguous Absent result and only with that immutable stored request. Background recovery is
  query-only: it may adopt exact completed evidence, while Absent, invalid, retired,
  not-applicable, non-completed, and identity/version/fingerprint/count/allocation mismatches keep
  CanonicalPending with only the bounded diagnostic change and no ordered execution.
- **Evidence:** Router tests
  canonical_pending_reconciliation_uses_stored_target_before_fresh_resolution,
  canonical_pending_reconciliation_adopts_exact_completed_receipts_without_dispatch,
  canonical_pending_reconciliation_rejects_nonexact_evidence_without_dispatch, and
  canonical_pending_reconciliation_handles_absent_by_trigger; PocketIC tests
  atomic_insert_canonical_pending_reconciliation_same_key_retry_does_not_redispatch_completed_journal
  and
  atomic_insert_canonical_pending_reconciliation_timer_after_router_upgrade_does_not_redispatch_completed_journal.
  The final focused Router filter reported 4 passed / 792 filtered. The focused PocketIC filter
  reported 2 passed / 13 filtered after the test-only target_shard expectation correction. Router
  library clippy, Graph pocket-ic-e2e clippy, and the PocketIC target clippy passed.
- **Supplemental runtime evidence:** After the latest Router reservation fix, the full
  adr0057_atomic_insert target ran once and reported 14 passed / 1 failed. Both reconciliation
  tests passed. This target is not counted as a pass: the unrelated
  atomic_insert_rejects_missing_catalog_name_before_reservation test failed because PocketIC HTTP
  adapter startup timed out and reqwest reported IncompleteMessage.
- **Close-gate status:** Global `cargo fmt --all -- --check` passed. The final Plan 0266 validator
  passed. The final exact 7-path scope review passed with the staged-empty/unrelated manifest
  matched. Both scoped Plan tests passed. The full `adr0057_atomic_insert` target remains
  incomplete at 14 passed / 1 failed because the unrelated
  `atomic_insert_rejects_missing_catalog_name_before_reservation` test hit a PocketIC HTTP
  adapter startup timeout (`IncompleteMessage`). No benchmark was run because this slice does not
  change a performance-sensitive algorithm. The recovery fix was committed as
  `d700331c33a8cdae7524e76bcb4e9ddcd5cdb600`.
- **Impact (before fix):** A committed canonical effect could remain non-terminal despite the
  exact durable Graph receipt needed to reconcile it.
- **Next decision:** None for this recovery contract. Plan 0260 remains a later, non-blocking
  formalization slice.

### GAP-2026-08-20-004 — Rust application client SDK and shared Router wire contract

- **Status:** Resolved
- **Severity:** P2 product gap; generated-code quality gap for the Rust client profile
- **Owner:** `gleaph-codegen` rust client profile, `gleaph-cdk`, and the SDK boundary
- **Observed behavior:** The Rust application-client codegen profile (`generate_rust`) emitted a
  provisional `PreparedExecutor` / `PreparedQueries` facade whose transport and response decoding
  were a runtime scaffold, and it did not mirror the mature `PreparedExt` profile used by
  `gleaph-cdk`. There was no supported Rust application client; the Router data-plane wire contract
  lived only in `gleaph-cdk::types`, which depends on `ic-cdk` and is not a valid application-client
  dependency.
- **Resolution (ADR 0069, 2026-08-20):** introduced `gleaph-router-wire` (`crates/router-wire`) as a
  neutral owner of the Router data-plane wire contract (`GqlQueryResult`, `ReadMode`,
  `MutationToken`, `RouterError`, bulk-load family, row-decode helpers); `gleaph-cdk` re-exports it.
  Introduced `gleaph-sdk` (`sdk/client/rust`), an `ic-agent` application client mirroring the
  `GleaphClient<Prepared>` surface with a `GleaphTransport` trait, `IcAgentTransport`, and caller
  identity injection through the agent identity. Unified the Rust codegen profiles onto a shared
  renderer (`crates/codegen/src/rust/shared.rs`) parameterized by runtime path; the client profile
  now emits the same `PreparedExt` boundary as the canister profile, removing `PreparedExecutor` /
  `PreparedQueries` and the provisional client envelope/structs.
- **Evidence:** `crates/router-wire`, `sdk/client/rust`, `crates/codegen/src/rust/shared.rs`;
  updated fixture at `crates/codegen/fixtures/rust-client-basic/src/lib.rs`; codegen tests in
  `crates/codegen/src/lib.rs`.
- **Impact:** Resolved: application clients get a typed, canister-parity client with identity
  injection, and both Rust profiles share one generated `PreparedExt` shape and one wire contract.
- **Next decision:** None — ADR 0069 accepted; the generated Rust client output changed from the
  `PreparedExecutor` scaffold to `PreparedExt`, so any scaffold consumer must regenerate.

### GAP-2026-08-20-006 — Router shard identity has no incarnation or lifecycle fence

- **Status:** Resolved (2026-08-22)
- **Severity:** P0 identity safety
- **Owner:** Router graph catalog lifecycle, ShardId allocation, and Graph/Router wire identity
- **Resolution:** `ROUTER_GRAPH_RUNTIME_CONFIG.next_shard_id` is the canonical per-graph high-water.
  Registration accepts only that never-issued id and advances the high-water; unregister and
  failed-attach rollback never rewind it. Exhaustion fails before any registry mutation.
- **Evidence:** `graph_runtime_config_reopen_preserves_shard_high_water`,
  `unregister_shard_never_reuses_the_retired_graph_local_id`,
  `exhausted_graph_local_shard_allocator_rejects_without_registry_mutation`, and
  `registry_invariants_reject_active_shard_at_allocator_high_water`.
- **Impact:** Delayed work and public identifiers from a retired shard cannot alias a later shard
  within the same graph, without widening `GlobalVertexId` or cross-canister protocols.
- **Next decision:** None for shard identity. Frontier publication remains a separate Vector GC
  protocol decision.

### GAP-2026-08-20-005 — Property DROP INDEX has no durable per-PhysicalIndexId retirement lifecycle

- **Status:** Resolved (2026-08-22; fix is in this patch)
- **Severity:** P1 derived-index cleanup
- **Owner:** Router index catalog and graph-index posting purge lifecycle
- **Observed behavior:** Router removes the catalog row before remote purge and keeps purge progress
  only in the active call. A retry after response loss cannot recover the removed catalog identity.
  Multiple physical namespaces can exist for one logical property, while the remaining-reference
  decision can skip or target only one namespace: dropping one of several indexes on a shared
  property skipped the purge entirely, orphaning the dropped namespace immediately, and the final
  drop purged only its own namespace.
- **Expected or needed behavior:** Every unreferenced PhysicalIndexId must have a durable,
  resumable purge identity and cursor until graph-index confirms cleanup; a shared logical property
  must not leak a distinct physical namespace after its final reference is removed.
- **Resolution:** `DROP INDEX` now co-writes the catalog removal with a durable Router-owned
  retirement record keyed by `PhysicalIndexId` (`ROUTER_INDEX_RETIRED`, MemoryId 54) that freezes
  the drain target set resolved before the destructive mutation and persists one resume cursor per
  pending target. Posting scopes are disjoint per physical namespace and unreachable once the
  catalog row dies, so every dropped definition retires unconditionally — the wrong
  remaining-reference gate is deleted. The inline fast path drains bounded rounds inside the DDL;
  holds defer to a new recovery-timer lane (`index_retirement.rs`, Driver 4) advancing one bounded
  step per pending target per tick; records delete exactly when the last frozen target confirms
  done. `DROP INDEX` returns success after the co-write and never fails on purge transport errors.
- **Owning regression tests:** PocketIC
  `dropping_one_of_two_indexes_on_shared_property_does_not_leak_its_namespace` (per-namespace leak,
  plus sibling-survival guard) alongside the restored `drop_index_purges_postings_from_graph_index`
  and timer-compaction contracts in `adr0023_index_store_consistency.rs` (6 passed); router unit
  tests in `router/src/index_retirement.rs`: response-loss hold-and-converge, cross-pass cursor
  persistence, per-target partial completion, empty-pending retirement, scan pagination; memory
  layout inventory updated to 55 regions.
- **Evidence:** Failure modes F1–F5 traced through `router::drop_index` → `drop_named_index`
  (catalog row removed first) → call-local resume loop over `graph_index_lookup_targets`; probe
  verified postings are keyed per `(physical_index_id, property_id)` so per-namespace retirement is
  complete by construction. The Quint model named in the next decision is unnecessary: the
  retirement record is a single-owner monotonic lifecycle (enqueue → per-target drain → delete)
  with no concurrency beyond the idempotent bounded steps, which the unit regressions cover.

### GAP-2026-08-11-004 — Current V1 metadata boundary leaves too little extension space

- **Status:** Resolved (obsolete)
- **Severity:** P2 / Persisted-layout capacity
- **Owner:** `ic-stable-clustered-hash-map` header and entry-address invariants
- **Observed behavior:** The current V1 metadata fields occupy bytes 0 through 62 and the table
  entry boundary is now defined at byte 128. Fresh layout creation clears the complete metadata
  extension.
- **Expected or needed behavior:** Future persisted metadata must fit within the 128-byte current
  V1 prefix without moving entries again.
- **Evidence:** `header.rs::HEADER_SIZE`, `header.rs::DATA_OFFSET`, and the test
  `current_v1_header_uses_128_byte_data_boundary` establish the boundary and initialization.
- **Impact:** The map has space for additional persisted state while retaining the V1 version
  number and fixed field offsets. The extra prefix is allocated once per map and does not change
  entry stride or mutation algorithms.
- **Next decision:** **Obsolete.** The `ic-stable-clustered-hash-map` crate is retired; its last
  consumer (`VECTOR_PARTITION_HEADS`) migrated to `ic-stable-linear-hash-map` (ADR 0067) and the
  crate was removed from the workspace. This gap no longer applies.

### GAP-2026-08-11-002 — Active-remap overflow can force a full remap drain in one insert

- **Status:** Resolved (obsolete)
- **Severity:** P1 / High bounded-execution risk
- **Owner:** `ic-stable-clustered-hash-map` capacity, incremental-remap, and mutation-atomicity
  invariants
- **Observed behavior:** The header stores logical capacity at offset 29. `size_up()` accepts only a
  settled table, preserves the existing tail reserve while doubling buckets, and contains no
  active-remap drain. Normal and remap relocation preflight the full chain and extend a cleared
  64-slot tail before the first destructive write. `REMAP_BATCH = 64` counts every examined
  position. Public `insert` and `remove` keep the direct path when the final `REMAP_BATCH + 1` slots
  are empty, which bounds all maintenance relocations plus the requested insert without growth.
  Other growth-capable operations write directly through a block-granular undo transaction. It
  snapshots each overwritten block below the operation's initial logical capacity at most once,
  delegates exact growth and writes to the backing memory, and restores those disjoint blocks if the
  operation returns an error. Writes in a newly grown logical tail need no snapshot because restoring
  the original header makes them unreachable.
- **Expected or needed behavior:** No public mutation should perform a table-sized remap fallback.
  Capacity pressure must be handled before relocation writes begin, preserve the current two-mapping
  (`N` / `N-1`) mixed-lookup contract, and leave the key set and header recoverable if stable-memory
  growth fails. A failed public mutation must return the exact maintenance error with the whole
  operation's logical bytes, header, length, capacity, remap boundary, and key set unchanged.
- **Evidence:** `header.rs::CAPACITY_OFFSET` and `map.rs::capacity` establish the persisted source of
  truth. `extend_tail`, `insert_and_relocate`, `remap_position`, and settled-only `size_up` enforce
  grow -> clear -> publish ordering and preserve N/N-1 lookup.
  `multi_boundary_active_remap_insert_oom_is_operation_atomic_and_retry_succeeds` and
  `multi_boundary_active_remap_remove_oom_is_operation_atomic_and_retry_succeeds` prove exact-byte,
  header, key-set, reopen, and successful-retry behavior when a later remap boundary needs growth.
  The preceding persisted unfiltered `canbench` run uses `DefaultMemoryImpl` and measures the
  generic clustered insert at 67.62M instructions on the wasm32 canbench target; the focused
  active-remap fixtures also complete under the target-selected stable-memory backend. A
  post-change non-persist check on 2026-08-11 measured 75.26M for the generic insert, 37.88K for
  the active batch, and 48.43K for active tail extension; the checked-in artifact was not rewritten
  by that check. These values are not directly comparable with the former explicit-`VectorMemory`
  artifact.
- **Impact:** The table-sized active-remap fallback remains removed. A public `insert` or `remove`
  now either commits its bounded maintenance and requested mutation together or returns
  `OutOfMemory`/`CapacityOverflow` without changing logical map state. Stable-memory page count is
  physical allocation and need not shrink if an earlier grow succeeded before a later failure. The
  undo transaction covers returned errors; traps rely on the IC message rollback boundary, and the
  map adds no standalone write-ahead journal for process-crash recovery outside that boundary.
- **Next decision:** **Obsolete.** The `ic-stable-clustered-hash-map` crate is retired; its last
  consumer (`VECTOR_PARTITION_HEADS`) migrated to `ic-stable-linear-hash-map` (ADR 0067) and the
  crate was removed from the workspace. This gap no longer applies.

### GAP-2026-08-11-003 — Settled `size_up` clears a capacity-scale region in one mutation

- **Status:** Resolved (obsolete)
- **Severity:** P1 / High bounded-execution risk
- **Owner:** `ic-stable-clustered-hash-map` settled resize initialization and persisted-capacity
  invariants
- **Observed behavior:** Active remap maintenance is bounded to `REMAP_BATCH = 64` positions and
  production no longer drains a whole active remap. A settled threshold insert now persists a
  target capacity and clear cursor, grows/clears only a bounded prefix, and retains the old N until
  publication. Physical growth is also extended incrementally as the cursor advances. Settled
  inserts now guard lookup/relocation at 4,096 occupied entries; an over-budget request advances
  one persisted resize/remap step and returns `InsertError::RelocationBudgetExceeded` before the
  request mutates the table.
- **Expected or needed behavior:** The public mutation path must have a per-call initialization bound
  independent of table capacity. New-region initialization is resumable and persisted: each
  operation clears only a bounded chunk, reopen resumes from the cursor, the new bucket mapping is
  not published before the entire region is initialized, and every pending mutation advances the
  cursor or completes the resize.
- **Evidence:** The owning paths are `header.rs::ResizeState`, `map.rs::advance_resize_initialization`,
  `map.rs::publish_resize`, and `map.rs::clear_region`. The deterministic tests
  `threshold_resize_clears_a_bounded_prefix_and_reopens`,
  `pending_resize_clear_oom_rolls_back_cursor_and_reopens`,
  `publishing_resize_marker_reopens_and_finishes_metadata_commit`, and
  `clear_new_aborts_pending_resize_and_reopens_settled` cover cursor progress, reopen, OOM rollback,
  metadata recovery, and reset behavior. The updated N=13/N=16/N=20/N=23 threshold fixtures keep
  setup outside the timed closure and each measures 17,635 scoped instructions for the bounded
  64-slot prefix; the exact persisted values are in
  `crates/ic-stable-clustered-hash-map/canbench_results.yml`. The added settled collision-chain
  sweep measures exact relocation of 4, 64, 256, 1,024, and 4,096 occupied slots. The subject-width
  model (13-byte key plus 41-byte value) measures 600,576 / 2,390,784 / 9,551,635 instructions at
  256 / 1,024 / 4,096 slots, respectively, with an empirical fit of approximately `2,331 * C +
  3,837`. The fixture is setup outside the timed closure and verifies every resident after the
  insert; it is a map-algorithm model, not a vector-canister call-path benchmark. The regression
  tests `settled_chain_budget_starts_persisted_resize_and_retry_advances` and
  `settled_chain_budget_recovery_oom_is_atomic` cover persisted progress, reopen, and exact-byte
  rollback at the new boundary. The post-change non-persist threshold check measured 18.10K scoped
  instructions for each N=13/N=16/N=20/N=23 fixture; the persisted artifact remains the preceding
  17.635K baseline until an explicit final persist run. The bounded insert path now reuses the
  cluster-end boundaries collected by its preflight. A new 4,096-entry single-cluster fixture
  measured 950.56K scoped instructions with the plan versus 2.05M in a controlled plan-disabled
  run (approximately 53.6% lower); this exploratory benchmark is not persisted yet.
- **Impact:** The capacity-scale `clear_region` write is no longer performed in one public mutation,
  and the target mapping is not advertised until the cursor reaches the target. The scale benchmark
  still includes target-selected stable-memory growth and transaction overhead. Settled and active
  relocation attempts now have a deterministic 4,096-occupied-entry guard; the returned error
  commits bounded persisted maintenance while the request key/value remains the caller's
  responsibility. Active remap persists a lower scan cursor when a lower range can be processed,
  but a blocked boundary is retained rather than skipped. Stable-memory pages grown before a later
  returned error remain physical backing outside the logical rollback contract.
- **Next decision:** **Obsolete.** The `ic-stable-clustered-hash-map` crate is retired; its last
  consumer (`VECTOR_PARTITION_HEADS`) migrated to `ic-stable-linear-hash-map` (ADR 0067) and the
  crate was removed from the workspace. This gap no longer applies.

### GAP-2026-08-11-001 — Inner resize leaves the pending insert distance stale

- **Status:** Resolved by commit `1efac368d` (crate retired; see ADR 0067)
- **Severity:** P1 / High
- **Owner:** `ic-stable-clustered-hash-map` inner insert resize and pending-entry distance
  invariant
- **Observed behavior:** In the defective inner `insert_and_relocate` resize branch,
  `size_up()` recalculates the pending entry's bucket and insertion position under the grown
  mapping but leaves `entry.distance` from the prior mapping. The public insert returns `Ok`,
  `len` increases, and iteration contains the entry, while `get` and `contains_key` fail both
  immediately and after re-open.
- **Expected or needed behavior:** After an inner resize, the pending entry's stored distance
  must be the checked distance from its recomputed bucket to its recomputed position before
  relocation or the terminal write continues.
- **Evidence:** `crates/ic-stable-clustered-hash-map/src/map.rs::insert_and_relocate` is the
  owning branch. The repair adds
  `entry.distance = checked_distance(position - b)` immediately after the recomputed bucket and
  position. The focused regression target
  `map::tests::inner_resize_recomputes_relocated_entry_distance` covers successful insert,
  `len`/iterator presence, and immediate plus `init`/re-open point lookup. The repair and
  regression passed the focused Rust test, full crate library tests, format/check/clippy gates,
  and independent review. The repair is recorded in commit `1efac368d`.
- **Impact:** A successful public insert can persist an entry that iteration exposes but point
  lookup cannot reach, and the inconsistency survives re-open.
- **Next decision:** None for this defect; retain the focused regression with the owning map
  implementation.

### GAP-2026-08-10-001 — Future row-copy migration must carry the I8 per-row scale

- **Status:** Open
- **Severity:** P2 (latent; no code path today)
- **Owner:** vector canister slab page store / migration primitive
- **Observed behavior:** With `VectorEncoding::I8` implemented (Slice 0242), stored rows are an i8
  payload plus a per-row f32 scale in row-meta aux. The design's row-copy migration primitive
  (`export_vector_rows` / `import_vector_rows`) is **not implemented**; grep confirms no such code
  exists yet.
- **Expected or needed behavior:** A future export/import that copies rows across canisters (shard
  split, ADR 0031 multi-canister) must carry the per-row aux (scale) alongside the payload, or the
  imported I8 rows would be decoded with a wrong/zero scale.
- **Owner:** vector canister `PAGE_STORE` / `export_vector_rows` future slice.
- **Evidence:** `crates/vector-canister/src/facade/stable/page_store.rs` (`read_row_bytes` returns
  `(vertex_id, bytes, aux)`); `design/index/vector-index.md` §Multi-canister readiness.
- **Impact:** Latent until migration is built; adding it without carrying aux would silently corrupt
  I8 distances on the target canister.
- **Next decision:** When the migration primitive slice is planned, require `export_vector_rows` /
  `import_vector_rows` to round-trip the row aux.

### GAP-2026-08-10-002 — `size_up` under-allocates the canonical entry stride

- **Status:** Resolved by commit `c1dc31db7` (crate retired; see ADR 0067)
- **Severity:** P1 / High
- **Owner:** `ic-stable-clustered-hash-map` entry allocation/layout and `size_up`
- **Observed behavior:** Before the repair, `entry_stride()` used the canonical
  `key_size + value_size + 4` layout, but `size_up` computed its growth target with a stale
  `key_size + value_size + 2` stride.
  At the `n = 13 → 14` capacity of 16,398, the stale `+ 2` target under-requests the
  canonical allocation by `2 * capacity = 2 * 16,398 = 32,796 bytes`.
- **Expected or needed behavior:** Every growth target must cover the canonical allocation
  used by `entry_offset` / `entry_stride`, before clearing or writing the new region.
- **Evidence:** `crates/ic-stable-clustered-hash-map/src/map.rs::size_up` now derives the growth
  target from `self.entry_stride()`. The
  `load_threshold_resize_allocates_the_canonical_entry_stride` regression starts from fresh
  minimal-page backing, deterministically seeds a collision-free `n = 13` table at the normal
  75% load threshold, triggers growth through public `insert`, and asserts that the backing covers
  `DATA_OFFSET + capacity * entry_stride`. The focused test failed before the repair with
  `VectorMemory` `write: out of bounds` and passes after it.
- **Impact:** Before the repair, pre-grown backing could mask the physical out-of-bounds access,
  and collisions could trigger the stale-stride path earlier. Under a conforming `Memory`
  implementation whose out-of-bounds access traps or panics, the failure does not silently corrupt
  the pre-existing old-table bytes; this conclusion does not cover non-conforming memory
  implementations. The repaired path allocates from the canonical stride before clearing or
  writing the grown region.
- **Next decision:** None for the implementation. Record the primary-owned fixing commit after it
  is created.

### GAP-2026-08-04-002 — Rust canister bindings for record parameters lack a Candid form

- **Status:** Resolved
- **Severity:** P2 generated-code compile gap for `Record` parameters only
- **Owner:** `gleaph-codegen` rust-canister profile and `gleaph-cdk` record parameter type
- **Observed behavior:** The rust-canister profile always derived `CandidType` on generated
  `*Params` structs. Record-typed parameters use `gleaph_cdk::GqlRecord` (= `Vec<(String,
GqlValue)>`), whose `GqlValue` element type is candid-free by design in `gleaph-gql-value`,
  so a manifest with a Record parameter emitted bindings that failed to compile with
  `E0277: the trait bound GqlValue: CandidType is not satisfied`.
- **Resolution:** The rust-canister profile now emits `*Params` structs with only
  `#[derive(Clone, Debug)]` — no `CandidType`, no serde. Parameters never cross a candid or
  serde boundary; the generated `into_gql_params()` converts them to ordered logical GQL
  parameters at call time, which also allows raw `f128` / `f256::f256` fields for
  `Float128` / `Float256` parameters. Generated `*Row` structs and `PreparedResponse` derive
  `CandidType` + serde, with exotic row fields bound through the cdk row-binding wrappers
  (`GqlInt256`, `GqlUint256`, `GqlDecimal`, `GqlFloat16`, `Float128`, `Float256` in
  `gleaph-gql-ic-wire`, each with candid + serde impls; `Principal` is candid-native), so a
  canister can return prepared rows directly over its candid interface.
- **Evidence:** `crates/codegen/src/rust/canister.rs` (`canister_param_rust_type` emits
  `gleaph_cdk::GqlRecord`); scratch compile at `/tmp/gleaph-exotic-check` with the
  `exotic_manifest.json` (Int256/Uint256/Decimal/Principal/Float16/Float256 params and rows)
  compiles and round-trips a `PreparedResponse<FindExoticRow>` through `candid::encode_one` /
  `candid::decode_one`.
- **Impact:** Resolved for all parameter and column types; `Record` parameters compile because
  `*Params` no longer requires `CandidType`.
- **Next decision:** None — resolved in the real-type row-binding refactor.

### GAP-2026-07-25-002 — Tombstone-heavy OFFSET scans lack a persistent skip structure

- **Status:** Resolved for the tree regime by [ADR 0094](adr/0094-ltb-block-tombstone-count-level1-offset.md)
  + Plan 0338 (`73ba30e01` field/maintenance, `ec52f28a8` read path, 2026-09-07): the LTB block
  header carries a per-block `u16 tombstone_count` maintained by the single tree-mode remove
  funnel, and `visit_edges_window`'s tree arm resolves only the blocks overlapping the window's
  position range (S1, arithmetic under the Plan 0327 tombstone-inclusive position contract)
  skipping fully-dead in-window blocks via the header count (S2, fail-closed scan
  self-verification). Measured attribution (ADR 0094 implementation-status appendix): the
  dead-prefix paging case reclaims ≈ 99.97% of the tree walk (39.79M → 14,399 instructions,
  S1-dominated); a window spanning 512 fully-dead blocks removes their payload reads at ≈ 29,151
  instructions per skip (S1+S2 = 40.92M vs S0 64.29M, S2-dominated). **Slab/bypass regimes remain
  intentionally status-quo** (research doc §5.5): slab extent is capped at `T_PROMOTE = 4,096`, the
  worst-case re-anchored overshoot is ~109K instructions/query (Plan 0337 slab leg, 1.09× above
  the declared 100K bar — deferred-with-evidence, not opened), the compact-or-promote insert-path
  cycle answers churn, and the remove-side maintenance-admission trigger for Insertion-policy slab
  buckets and bypass rows is recorded as a separate deferred improvement (research doc §5.5).
  Level 2 (a dedicated contiguous per-bucket directory region) stays evidence-gated (§5.2) —
  Level 1 already reclaims ~98% of the measured tree gap.
- **Severity:** P2 traversal performance and stable-layout research gap (closed for tree;
  slab-side residual recorded as deferred-with-evidence)
- **Owner:** `ic-stable-lara` traversal and LARA logical-slot metadata
- **Observed behavior:** `TraversalWindow.offset` can jump directly only for a proven dense bucket.
  Sparse or tombstone-bearing buckets inspect logical rows to count live matches before delivering
  the requested window. No persistent interval summary, live bitmap, or rank/select structure is
  currently part of the LARA layout.
- **Expected or needed behavior:** A future implementation may skip tombstone-only regions and
  resolve the requested live ordinal without inspecting every preceding logical slot, while
  preserving logical-slot identity, exact forward/reverse ordering, overflow semantics, and
  fail-closed corruption handling.
- **Evidence:** [ADR 0050](adr/0050-lara-traverse-read-api.md) § “Known gap: tombstone-aware offset
  acceleration”; `TraversalWindow` and the labeled sparse traversal implementation. The current
  dense fast path is intentionally limited to `live_degree == logical_extent`. Plan 0283 measured
  872,634 instructions at extent 8,192 / 87.5% tombstones / OFFSET 960 / LIMIT 32, versus 38,051
  at the same density and offset 0 and 27,146 for the dense OFFSET control. Fair benchmark-only
  end-to-end probes performed canonical stable-row decoding, liveness checks, slot reconstruction,
  and identical visitor work: fixed-block counts measured 35,620 / 65,776 instructions at 75% /
  87.5%, while triggered two-level counts measured 32,138 / 59,210. The exact temporary patch and
  raw canbench CSV are retained under `design/investigations/artifacts/`. The bounded
  production-owned maintenance drain took 831,216,051 instructions across 1,025 one-work-item
  calls, then restored the query to 27,095 instructions. The sign-corrected optimistic query-only
  crossover is `Q_upper = 831,216,051 / (59,210 - 27,095) = 25,882.49 queries`. This upper bound
  omits candidate build, mutation, update, validation, framing, and repair costs, while compaction
  retains independent storage obligations; a positive crossover alone does not establish
  substitution or workload benefit. The stale predecessor wording was accidentally included in
  unrelated commit `b787ac389`; this hunk records corrected evidence without rewriting that commit.
- **Impact:** Large sparse buckets can spend instructions scanning tombstones for OFFSET/LIMIT.
  Introducing durable metadata now would expand the storage format and add mutation,
  compaction/rebuild, reopen, and benchmark obligations before the design is understood.
- **Next decision:** None for the tree regime — resolved by ADR 0094 + Plan 0338 (S1+S2, measured,
  attribution recorded in the ADR implementation-status appendix). The Plan 0283-era numbers below
  are historical: the extent-8,192 slab fixture predates ADR 0088's `T_PROMOTE` slab cap (legal
  ceiling 4,096 today), and the candidate-C reactive design was rejected by Plan 0336's fixed gate.
  Slab-side residual: status-quo + deferred-with-evidence (research doc §5.5); Level 2 escalation
  evidence-gated. The 2026-08-23 deferred decision text is preserved below as the historical
  record of that slice's scope boundary.

  **Historical record (2026-08-23, Plan 0283 decision at that time):** Retain the exact sparse
  scan and current compaction ownership; do not open an ADR from that slice. Revisit only if
  integrated candidate build/mutation/update/validation costs and a demonstrated workload
  establish a substitution benefit beyond compaction's query and storage obligations. Any
  adoption remains a separate ADR/storage slice.

### GAP-2026-07-25-001 — Paired canonical edge writes expose recoverable post-write errors

- **Status:** Resolved by Plan 0182
- **Severity:** P1 canonical consistency risk
- **Owner:** `ic-stable-lara` bidirectional owner and GraphStore paired mutation boundary
- **Observed behavior:** The owner writes paired halves sequentially. The implementation traps on
  reverse-half and post-write maintenance-admission failures, and dirty-key admission restores its
  bitmap bit when queue append fails. Deterministic failure-injection tests cover directed,
  undirected, and maintenance-queue paths.
- **Expected or needed behavior:** After the first canonical half is written, the supported
  paired-mutation boundary must either complete both halves and all required mutation intent, or
  trap/rollback the whole message segment. A recoverable `Err` must not expose a partially updated
  bidirectional graph to a caller that handles the result without trapping.
- **Evidence:** `crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs`
  (`insert_directed_edge_with_locations`, `insert_undirected_deferred_with_locations`) and
  `design/adr/0029-shard-local-atomicity-and-cross-canister-consistency.md` § “Shard-local
  preflight and commit discipline”. Graph reverse-adjacency tests intentionally inject and repair
  forward-only and reverse-only states, proving that the state is observable when the paired
  boundary is bypassed.
- **Impact:** A lower-level caller or a handler that returns an ordinary error could leave forward
  and reverse canonical adjacency out of sync, while the counterpart sidecar is already invalidated.
- **Next decision:** Keep exact counterpart validation for repair and published lookup. Any future
  mutation path must preserve the same preflight-then-trap boundary; a new recoverable post-write
  error requires a separate atomicity design review.

### GAP-2026-07-11-005 — `FreeSpanStore` reopen validation has no production-scale cost bound

- **Status:** Resolved (2026-08-22; declared reopen envelope with measured scale probes — up to
  262,144 non-coalescing active spans per store must validate within 1.0B wasm instructions and
  ≤8 MiB transient heap; measured 774.92M instructions at the declared maximum, essentially linear
  at ~2,956 instr/span, so the existing fail-closed sorted-merge validator is accepted unchanged
  and no ADR 0007/0039 amendment is required. Owning benchmarks:
  `bench_lara_free_span_store_reopen_16384/65536/262144` in
  `crates/ic-stable-lara/src/lara/edge/free_span/bench.rs`, thresholds documented in the bench
  header and [storage/lara.md](storage/lara.md) "Reopen integrity". Fragmentation beyond the
  declared level is outside the supported upgrade envelope rather than trapped.)
- **Severity:** P2 operational scalability risk
- **Owner:** `ic-stable-lara` free-span persistence and Graph upgrade preflight
- **Observed behavior:** `FreeSpanStore::init` validates the records/bin ↔ `by_start` bijection by
  collecting all active `(start_slot, id)` pairs, sorting them in heap, and comparing them with the
  ordered index. The algorithm is `O(active log active)` with `O(active)` transient heap. Existing
  canbench coverage measures at most 4,096 active spans.
- **Expected or needed behavior:** before production durable upgrades are claimed, the owner must
  define and measure a maximum supported fragmentation level, reopen instruction ceiling, and
  transient-heap ceiling. Reopen must remain fail-closed; performance work must not weaken the
  bin/index bijection.
- **Evidence:** `crates/ic-stable-lara/src/lara/edge/free_span.rs::validate`;
  `crates/ic-stable-lara/src/lara/edge/free_span/bench.rs::bench_reopen`;
  [storage/lara.md](storage/lara.md) “Reopen integrity”; and ADR 0039 “Upgrade preflight” plus
  “Performance and capacity gates”.
- **Impact:** a highly fragmented long-lived graph could make `post_upgrade` exceed its instruction
  or heap budget even though its stable layout is valid, preventing the new Wasm from serving.
- **Next decision:** first add scale-probing benchmarks beyond 4,096 spans and predeclare acceptance
  limits. If the current validator exceeds them, compare bounded incremental validation, persisted
  validation summaries with generation fencing, and an explicit hard fragmentation cap. Any change
  to the fail-closed validation contract requires an ADR 0007/0039 amendment.
- **Related contracts:** [ADR 0007](adr/0007-stable-memory-layout.md),
  [ADR 0039](adr/0039-production-stable-memory-evolution-and-upgrade-safety.md),
  [storage/lara.md](storage/lara.md)

### GAP-2026-07-11-004 — Non-tail homogeneous bypass insertion rewrites successor origins in `O(V)`

- **Status:** Resolved (2026-08-22; eager promotion — the labeled insert dispatcher promotes a
  non-tail default-bypass row to bucket mode via the existing preflighted
  `promote_bypass_to_bucket_mode` transition before its next same-label insert, so slab-region
  extension and the successor-origin bump only ever run while the row is the tail. Measured scale
  probes `bench_labeled_non_tail_bypass_insert_256/1024/4096`: pre-fix 1.18M / 4.40M / 17.25M
  instructions for 16 inserts (linear, ~263 instr/successor-row), post-fix 343K / 363K / 382K
  (flat; +11% for 16× successors, bounded by the owning PMA leaf). Scan semantics reuse the
  established promotion transition; no persisted row meaning, scan geometry, or PMA ownership
  change, so no dedicated ADR is required. Owning tests:
  `same_label_insert_into_non_tail_bypass_row_promotes_to_bucket_mode` (fails if the promotion
  guard is removed — row would stay default-edge-labeled) and
  `same_label_insert_into_tail_bypass_row_stays_in_bypass_mode`; contract documented in
  [storage/lara.md](storage/lara.md) "Labeled LARA". A `debug_assert` on
  `may_use_homogeneous_bypass` now guards `insert_homogeneous_bypass_edge` directly.)
- **Severity:** P2 performance risk
- **Owner:** `ic-stable-lara` labeled adjacency geometry
- **Observed behavior:** after a homogeneous bypass edge insert,
  `bump_successor_origins_after_bypass_end` scans every later vertex row and rewrites each later
  bypass origin that falls before the new region end. A bypass vertex that ceases to be the tail as
  more vertices are appended can therefore make one later edge insert proportional to the number of
  successor vertices.
- **Expected or needed behavior:** insertion cost must remain bounded by the owning PMA leaf/segment
  or the system must explicitly prohibit or promote non-tail bypass rows before they enter this hot
  path. Scan semantics and the direct vertex-row lookup contract must remain unchanged unless a
  reviewed representation decision replaces them.
- **Evidence:** `crates/ic-stable-lara/src/labeled/graph/bypass.rs::insert_homogeneous_bypass_edge`
  and `bump_successor_origins_after_bypass_end`; existing bypass canbench coverage does not isolate
  repeated inserts into an old bypass vertex across increasing successor counts.
- **Impact:** repeated insertion into an early bypass vertex can grow toward `O(EV)` work and stable
  row writes, creating an instruction-limit cliff not represented by current benchmark gates.
- **Next decision:** add vertex-count scaling benchmarks first. Prefer an existing leaf-owned
  geometry update or eager promotion if it meets the measured bound. Write a dedicated adjacency
  representation ADR only if the chosen fix changes persisted row meaning, scan geometry, or PMA
  ownership; a local bounded optimization needs only a plan plus design/benchmark sync.
- **Related contracts:** [ADR 0001](adr/0001-labeled-segment-slide.md),
  [ADR 0022](adr/0022-degree-driven-hub-edge-storage.md), [storage/lara.md](storage/lara.md)

### GAP-2026-07-31-001 - Prepared sort variants are not retained in the heap cache

- **Status:** Resolved (2026-08-22; bounded heap cache
  `PREPARED_SORTED_CACHE` keyed by `(PreparedPlanKey, PreparedSortSignature)` — the signature is
  validated once by `normalize_prepared_sort` (the single source of truth shared with AST
  injection): direction text canonicalized case-insensitively, allowed keys exact-matched and
  deduplicated, term order preserved because `ORDER BY a, b` and `ORDER BY b, a` are different
  plans. Cap 128 entries with smallest-key eviction on insert; invalidated on upsert replacement,
  `drop_prepared`, and post-upgrade rebuild (variants rebuild lazily). Stable storage keeps only
  query source + metadata per ADR 0053's heap-cache principle, so no stable-layout change.
  Measured `bench_prepared_sorted_variant_rebuild` 318.12K vs
  `bench_prepared_sorted_variant_cache_hit` 29.50K instructions (~10.8x reduction).
  Owning tests: hit-path mutation discriminator (`sorted_variant_cache_hits_normalized_signature`
  fails if the wrapper re-plans), distinct term-order/direction entries, upsert/drop/upgrade
  invalidation, cap eviction, and unchanged validation-error contracts. Router unfiltered
  `canbench --persist` deferred while the crate's benchmark artifact carries a parallel stream's
  in-flight measurements; resume with `cd crates/router && canbench --persist`.)
- **Severity:** P2 prepared-query performance gap
- **Owner:** Router prepared-query heap cache
- **Observed behavior:** The Router keeps the unsorted prepared plan in the heap cache. When a caller supplies a non-empty sort specification, the Router validates the metadata and rebuilds a derived plan for that invocation; equivalent sort specifications are not cached as separate heap entries.
- **Expected or needed behavior:** Repeated executions of the same prepared operation with the same normalized sort specification should reuse a heap-cached derived plan while stable storage continues to retain only the query source and metadata.
- **Evidence:** crates/router/src/prepared.rs::prepare_sorted_cache and the prepared query local E2E in crates/codegen/e2e/test.mjs.
- **Impact:** Sort-enabled prepared queries pay the planning and plan-encoding cost on every execution. This preserves correctness and upgrade safety but can increase latency and instruction usage for frequently reused sort variants.
- **Next decision:** Define a bounded cache key and eviction policy, including direction normalization, key ordering, metadata changes, and post-upgrade invalidation, then add hit/miss tests and a focused benchmark before introducing durable or unbounded derived state.
- **Related contracts:** ADR 0053 and crates/prepared-api/src/lib.rs

### GAP-2026-07-04-001 — Prepared execution still requires graph visibility

- **Status:** Closed by ADR 0063 (2026-08-06)
- **Severity:** P2 product gap; P1 if a public frontend must call Router prepared queries directly
- **Owner:** Router prepared catalog resolution and graph authorization
- **Observed behavior:** `authorize_prepared_execute` permitted the default Router `Executor` role,
  including an anonymous caller, but prepared-plan resolution searched only graphs visible to the
  caller. A principal that is not the graph owner or in the graph `admins` set therefore could not
  resolve the prepared plan. The social demo test had to add its application caller to the graph
  administrators while leaving its Router role at `Executor`.
- **Resolution (ADR 0063, 2026-08-06):** prepared queries are addressed by global name and bound to
  a graph at registration time; `authorize_prepared_execute` and `resolve_prepared_graph_id` were
  removed, so any caller can execute an administrator-registered prepared query. Caller-aware
  authorization (public vs visible graphs, per-query permissions) is deferred to a future
  access-control ADR.
- **Historical evidence:** `crates/router/src/rbac.rs::authorize_prepared_execute`,
  `crates/router/src/prepared.rs::resolve_prepared_graph_id` (both removed by ADR 0063), and the
  shared `install_single_shard_federation_with_graph_admins` fixture in
  `crates/pocket-ic-tests/src/lib.rs`.
- **Historical workaround (removed 2026-08-06):** `crates/social-demo-gateway` was an
  application-owned canister with a fixed scenario enum. The Gateway principal was registered as a
  graph administrator so Router could resolve the prepared plan, but it remained a default Router
  Executor with no ad-hoc `Read` role. Anonymous callers executed the fixed scenarios through the
  Gateway; Router observed the Gateway principal, not the original caller. This was an
  application-layer trusted-deputy pattern, not a product change to Router prepared-query
  authorization. The crate was removed together with the SDK-direct frontend; the frontend now
  calls Router `prepared_query` by name through `@gleaph/sdk`.
- **Related contracts:** [security/rbac-and-prepared.md](security/rbac-and-prepared.md),
  [demo/social-graph-rag.md](demo/social-graph-rag.md)

### GAP-2026-08-01-001 — Social-demo load artifacts were not reproducible from build-config.mjs

- **Status:** Resolved by Plan 0204 on 2026-08-02 UTC
- **Severity:** P2 demo-tooling drift; blocks trusting `pnpm run build:config` as an idempotent
  regeneration step
- **Owner:** `demo/social/scripts/build-config.mjs` and the committed seed/avatar
  artifacts
- **Resolution:** The generator now emits one ordered NDJSON pair (`seeds/vertices.jsonl` +
  `seeds/edges.jsonl`) directly from the canonical
  node/edge model. Its typed properties contain no execution-time values, application endpoint keys
  remain explicit, and exact UTC timestamps are deterministic. Focused contracts cover source-ID
  closure and byte-stable generation inputs; the durable loader uses the full artifact SHA-256 plus
  exact Router chunk replay rather than parsing generated mutation text.

### GAP-2026-08-03-001 — Router seal activation crash window can strand an already-Active Graph export scope

- **Status:** Resolved by ADR 0059 seal-step tolerance (commit in the same patch)
- **Severity:** P2 crash-window recovery gap (narrow, non-blocking for first rollout)
- **Owner:** `crates/router/src/facade/store/schema_migration/driver.rs` seal composition plus Graph
  `canonical_export` Active-scope semantics
- **Observed behavior:** In one seal drive the driver publishes every Graph export scope
  (`admin_activate_index_export_scope`) as its final remote step. If the Router message traps or
  rolls back after those activations but before persisting the `Converged` target, a re-drive's
  step-1 `admin_seal_index_export_scope` returned `InvalidPhase` because the scope is already
  `Active`; the driver classified that as terminal, the router entered `Aborting`, and Graph
  rejected `admin_abort_index_export_scope`/`admin_remove_index_export_scope` for an `Active`
  scope, so cleanup could not progress.
- **Resolution:** `seal_scope` now treats an already-`Active` scope under the exact same frozen
  identity and lifecycle epoch as an exact replay and returns the durable status instead of
  `InvalidPhase`; `activate_scope` already replays an `Active` scope whose proof matches. The
  re-drive therefore re-seals (replay), re-reads the graph-index seal proof (idempotent),
  drains (already converged), and re-activates (idempotent) without error, so the Router
  persists `Converged` and completes normally. A different epoch or identity still fails closed,
  and `Active` remains deliberately non-abortable and non-removable.
- **Evidence:** `crates/router/src/facade/store/schema_migration/driver.rs` `drive_seal`;
  `crates/graph/src/index/canonical_export.rs` `seal_scope`/`activate_scope`/`abort_scope`/`remove_scope`;
  ADR 0059 seal ordering.
- **Regression tests:** `seal_scope_replays_already_active_scope_after_activation_crash_window`
  (graph lib) and `driver_seal_resumes_after_activation_crash_window` (router driver) prove the
  re-drive converges and that a wrong `InvalidPhase` return for the same identity/epoch would fail.

### GAP-2026-08-04-001 — `ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` is defined but never consumed

- **Status:** Resolved (2026-08-22; option (b) implemented. The between-chunk loop in
  `execute_prepared_mutation` now runs the ADR 0042 between-wave check before every chunk via
  `graph_batch_chunk_within_update_budget`, composing the crate's generalized `should_cutoff`
  predicate with ceiling `MAX_UPDATE_CALL_INSTRUCTIONS` (40B), the new
  `ROUTER_BATCH_CHUNK_WORK_INSTRUCTION_ESTIMATE` (50M: measured worst per-chunk Router work
  37.34M from GAP-2026-07-17-001 evidence plus ~34% margin), and the previously-dead
  `ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` (4B) as the finalization reserve — so the constant
  is consumed by the boundary it claims to protect, and the Router provably stops dispatching
  rather than trapping at 40B regardless of Graph behavior. When the guard trips, deferred
  operations surface as per-operation errors ("router update instruction budget guard stopped
  chunk dispatch") through the same recovery path as chunk failures; resubmission completes
  them idempotently via the mutation journal. The live counter comes from the now-ungated
  `current_instruction_counter` (host builds return 0, so native tests exercise the untripped
  path). Owning tests: `chunk_budget_allows_a_fresh_message`,
  `chunk_budget_trips_at_and_past_the_exact_boundary` (pins the exact `>=` trip point against
  dropped/reordered terms or a flipped comparison),
  `chunk_budget_trips_when_only_headroom_remains`. Residual risk: loop wiring itself is not
  natively testable without an inter-canister seam; the pure guard is fully pinned.)
- **Severity:** P2 router budget-integrity gap (no current runtime risk; the shared headroom covers the
  path)
- **Owner:** `gleaph-instruction-budget` constants and the Router chunk-dispatch loop
  (`crates/router/src/gql.rs` `execute_prepared_mutation` / `graph_batch_chunk_len_for_dispatches`)
- **Observed behavior:** `ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` (4B) is defined in
  `gleaph-instruction-budget` and re-exported from `gleaph-graph-kernel`, but no code path consumes
  it: the Router chunk decision caps at `max_operation_count(500M, 35B)` = 70 operations per chunk
  using the shared `MAX_DYNAMIC_UPDATE_INSTRUCTIONS`, and the between-chunk loop
  (`execute_prepared_mutation`) has no Router-local call-context cutoff. The 4B constant appears only
  in the definition, the re-export, the compile-time assertion, and the GAP acceptance bench comment.
- **Expected or needed behavior:** every defined headroom must either be consumed by the owning
  boundary or be removed. If the Router is to guarantee it never crosses the 40B limit on its own
  account (distinct from the Graph-side 35B budget inside each dispatched batch), the chunk loop
  needs the ADR 0042 original between-wave check: read the Router call-context instruction counter
  and stop before starting another chunk when `used + next-chunk estimate + reserve ≥ 40B`.
- **Evidence:** `crates/instruction-budget/src/lib.rs` (definition + assert),
  `crates/graph-kernel/src/lib.rs` (re-export), `crates/router/src/gql.rs`
  (`graph_batch_chunk_len_for_dispatches` uses only the shared budget); measured per-chunk decision
  cost 6.01M / 37.34M instructions and the ≤2 MiB inter-canister sizing bound
  (`crates/router/src/bench.rs`, `crates/router/canbench_results.yml`, GAP-2026-07-17-001).
- **Impact:** no current runtime risk: the Router's actual safety is the shared 5B
  `UPDATE_CALL_INSTRUCTION_HEADROOM` (35B dynamic budget), and the measured per-chunk work
  (decision ≤ 37.34M + ≤2 MiB encode + response) is ≪ 1% of it. The gap is integrity and
  future-proofing: the dead constant invites a false sense of protection, a future Router-side
  bounded loop cannot reference a consumed budget, and the GAP acceptance table lists a value that
  is not wired to the path it claims to reserve.
- **Next decision:** choose one of (a) remove the constant and the ADR 0060 §1 list entry, noting
  in GAP-2026-07-17-001 that the shared 5B covers the Router chunk path; or (b) add the
  between-chunk call-context cutoff to `execute_prepared_mutation` and re-derive the constant from
  measurement (≈1B suffices: decision 37.34M + response at ~7× margin). Option (b) is preferred so
  the Router itself provably never traps at 40B regardless of Graph behavior.

### GAP-2026-08-23-001 — Native `gleaph-router` lib suite has pre-existing ic0-context failures that poison unrelated tests

- **Status:** Fully resolved (2026-08-23 build breakage, B1, adr0034 split-out, and ic0-context
  admission deferral; 2026-08-24 var_len group-variable access)
- **Severity:** P3 test-harness (the ic0/build items below were P1 while present)
- **Owner:** `gleaph-router` native unit test harness; `gleaph-graph` expand executor and adr0034 fixture own their respective items

#### Resolved 2026-08-23

1. **Workspace build breakage (8e392c127 → ac380c4bb).** `8e392c127 feat(index): wire edge range pushdown end to end`
   added `collect_edges_matching_indexed_property_where` call sites in
   `crates/graph/src/index/edge_lookup.rs` but omitted the collector's definition in
   `crates/graph/src/facade/store/edge_properties.rs`, leaving every commit from 8e392c127 through
   463478b44 unable to build `gleaph-graph`. Fixed by `ac380c4bb fix(graph): add the predicate-scanned
   edge property collector`.
2. **Directed equality-index candidates lose inline bytes (`b787ac389` → 317517074).**
   `b787ac389 fix(index): own edge posting label identity in wire space` let directed postings reach
   the equality-index candidate path for the first time, but that path read slots through
   `read_out_edge_slots_for_label`, which restores topology only — scanned edges carried empty inline
   property bytes, failing projected inline reads. The LARA slot-targeted reader is topology-only by
   design (`read_edge_state_at_slot` restores no payload); byte restoration lives in the batch walk.
   Fixed by `317517074 fix(graph): restore inline bytes in equality-index candidates`: the PointingRight
   arm now walks the labeled adjacency once with batch-restored bytes and keeps the posted slots,
   mirroring the PointingLeft arm. Owning test: `indexed_edge_equality_expand_return_inline_property`.

3. **var_len group-variable property access under default-enabled cypher (`b13d427ca`) — resolved 2026-08-24.**
   `gql_var_len_where_inline_property_filters_on_last_hop_edge` ("property access on group edge variable
   'e.distance' requires element indexing") and `gql_var_len_return_inline_property_decodes_indexed_last_hop_edge`
   (decodes to Null) were newly *exposed*, not regressed, by the cypher dialect gate (green at
   da665a70b only because the tests did not compile in). Two executor gaps: `e[-1].prop` resolved the
   indexed element through the technical whole-edge record (`edge_to_value`), which carries no decoded
   inline properties, and `Compare` over a direct group-property access had no defined quantifier.
   Fixed by defining both semantics in `QueryExprEvaluator`: indexed elements read their property
   exactly like a single-edge binding (inline decode → sidecar fallback), and a residual group-property
   comparison quantifies over **every** hop — mirroring the schema-fused `edge_inline_property_predicate`
   contract so fused and residual plans stay result-equivalent. Owning tests:
   `gql_var_len_return_inline_property_decodes_indexed_last_hop_edge`,
   `gql_var_len_where_inline_property_filters_on_last_hop_edge`, and the all-hop discriminating
   `gql_var_len_where_group_property_requires_every_hop_to_match`. Contract recorded in
   [group-variables.md](../design/execution/group-variables.md).

#### Resolved 2026-08-23 (ic0 admission deferral)

4. **ic0-context panics poisoned the router lib suite (this host) — resolved 2026-08-23 late UTC by
   deferring the self-principal read.** `cargo test -p gleaph-router --lib` failed three dev-mode
   fail-closed tests in this macOS host environment:
   `facade::store::schema_migration::tests::unregistered_create_graph_migration_fails_closed_without_provisioner`,
   `provisioning::graph::tests::admission_fails_closed_for_unregistered_name_without_provisioner`,
   and `provisioning::graph::tests::admission_short_circuits_registered_name_without_provisioner`,
   all panicking with `canister_self_size should only be called inside canisters` (`ic0` system call
   reached outside canister execution), in full runs and in isolation. Root cause: the
   `create_graph_admission` wrapper evaluated `ic_cdk::api::canister_self()` eagerly as an argument
   to `create_graph_admission_with`, so the ic0 read fired before the provisioner-absent rejection;
   each panic also poisoned the shared outbox/registry test lock, cascading into
   `vector_sync::tests::{frontier_response_loss_retains_exact_marker_snapshot,
   resolved_rows_transition_to_awaiting_frontier_before_publish}`, which pass when run alone.
   Fixed in this entry's closing commit: `create_graph_admission_with` now takes the Router
   self-principal as an `impl FnOnce() -> Principal` and evaluates it exactly once, only after every
   fail-closed rejection has returned; the wrapper passes the `ic_cdk::api::canister_self` function
   path itself. No endpoint check was relaxed and no ic0 stub abstraction was introduced. Owning
   tests: the three admission/migration tests above plus the de-poisoned vector_sync pair.

The adr0034 fixture item that formerly appeared here as "Remaining open" item 5 now lives in its own
entry, [GAP-2026-08-23-002](#gap-2026-08-23-002--adr0034-e2e-fixture-predates-typed-schema-endpoint-enforcement).

#### Evidence summary

- Build breakage and B1: baseline reproduction with working-tree changes stashed at HEAD 463478b44;
  worktree bisect across 8e392c127 / cb156b0b7 / b787ac389 / b6fe10612 with the missing collector copied
  forward; candidate-path disable experiment turned B1 green, confirming the reader as the defect site.
- ic0 panics reproduce on clean 463478b44; isolated vector_sync runs are green.
- adr0034 failure persists with the slice-4 planner changes reverted, so planner anchor selection is not
  involved; see GAP-2026-08-23-002 for its owning entry.

### GAP-2026-08-23-002 — ADR 0034 E2E fixture predates typed-schema endpoint enforcement

- **Status:** Resolved (2026-08-23) — both `adr0034` read fixtures were reproduced against the
  typed-schema endpoint gate, aligned with their declared graph types without weakening validation,
  and rerun green (struct in `8f3f5c1da`; scalar in the closing commit).
- **Severity:** P1 E2E correctness-gate blocker (keeps the adr0034 PocketIC target red); not a
  production storage/query defect.
- **Owner:** the stale `adr0034` read fixtures, which insert vertices whose labels contradict their
  declared graph types. The typed-schema endpoint check promoted at Router ingress is correct
  fail-closed behavior and must not be weakened. Planner anchor code is not involved.

#### Observed behavior

`cargo test -p gleaph-pocket-ic-tests --test adr0034_inline_edge_struct_read_access` failed at HEAD
`9a4b94892` (and on the clean slices 1–3 baseline `463478b44`) with:

```
gql_query rejected: InvalidArgument("validation error: edge `:AFFINITY` cannot connect :ProjectionSource to (unlabeled) (schema constraint violation)")
```

The DDL declares `NODE User AS user ... CONNECTING (user -> user)`, but scenarios inserted sources
labeled `ProjectionSource`/`FilterSource`/… and unlabeled targets, so every MATCH violated the
declared endpoint set. `cargo test -p gleaph-pocket-ic-tests --test
adr0034_inline_edge_scalar_access` fails identically for `:ROAD` over `road_type`.

#### Fix evidence (2026-08-23)

`crates/pocket-ic-tests/tests/adr0034_inline_edge_struct_read_access.rs` was aligned with its
declared graph type: vertices now carry the declared `User` label, and per-scenario independence
moved from ad-hoc source labels to unique `updated_at` windows scoped through WHERE clauses. The
endpoint validator itself is unchanged. Terminal rerun:
`cargo test -p gleaph-pocket-ic-tests --test adr0034_inline_edge_struct_read_access` → **1 passed /
0 failed**, one single-shard federation constructor, three canister installs.

The sibling scalar fixture received the same alignment in the closing commit: vertices carry the
declared `City` label, per-scenario independence moved from ad-hoc source labels to disjoint
`distance` bands (7 / 17·19 / 27·29 / 37) scoped through WHERE equality and band-range predicates,
and every row-count/order assertion keeps its original intent. The scalar expectations were also
corrected from the never-exercised `Uint64` decode to the declared-type `Uint16` wire value,
mirroring the struct fixture's `Float64` → `Float32` correction in `8f3f5c1da`. Terminal rerun:
`cargo test -p gleaph-pocket-ic-tests --test adr0034_inline_edge_scalar_access` → green after a
fresh re-run immediately before judgment (shared-target contention with parallel agents produced
one transient unrelated build failure).

#### Next decision

None — closed. Both adr0034 read fixtures now conform to their declared graph types, and the
typed-schema endpoint gate remains unchanged and fail-closed.

## Resolved gaps

### GAP-2026-09-02-001 — RESOLVED: production `LabeledLaraGraph::visit_edges` dispatches tree-mode buckets into the dense slab read path

- **Status:** Resolved (2026-09-02, Plan 0324 REWORK-2).
- **Severity:** P0 production-correctness defect (read of stable
  memory garbage / trap with `VectorMemory::read: out of bounds`).
  Tree mode is the primary high-degree storage mode; any production
  canister that visits a high-degree tree bucket would trap.
- **Root cause:** 3 dispatch sites in `traverse.rs` used a "dense,
  tombstone-free" fast path that bulk-reads
  `degree × E::BYTES` bytes from `bucket.edge_start()`. For tree
  mode, `edge_start` is the LEG root region (block_id array,
  `root_len × 4` bytes), not the LTB payload blocks. The dispatch
  condition matched tree mode (overflow_log_head = -1,
  reserved_slots = stored_slots), so the dense slab-path was
  incorrectly taken and the bulk read went OOB.
- **Initial fix (REWORK-2)**: tree branches routed through
  `tree_mode_out_edges_collect` (Vec materialization) + `enumerate()`,
  yielding live-ordinal positions. Worked for dense tree buckets
  (where enumerate == tombstone-inclusive slot) but violated the
  position contract (ADR 0088 §2) for tombstoned buckets.
- **Final fix (REWORK-3, this slice's design)**: tree branches call
  `visit_tree_mode_label_bucket_edges` directly, forwarding the
  tombstone-inclusive `u32` slot to `BucketEntryPosition::new(slot)`.
  `ControlFlow::Break` is captured via a mutable cell. Dispatch
  order reordered: dense condition first (more selective, with
  `!is_tree_mode()` guard), tree branch after. **~50% instruction
  reduction on the 3 read benches** as a bonus.
- **Fix (REWORK-3):** added `if bucket.is_tree_mode() { ... }` branches
  at 3 dispatch sites:

  | Site | Function | Routing |
  |---|---|---|
  | `traverse.rs:790` | `visit_edges` | `tree_mode_out_edges_collect` + `ControlFlow` adapter |
  | `traverse.rs:1580` | `visit_edges_window` | `tree_mode_out_edges_collect` + window slice + `ControlFlow` adapter |
  | `traverse.rs:1843` | `visit_edges_with_inline_property` | `!is_tree_mode()` guard on dense path; falls through to `single_bucket_span_iter` (LTB-aware) |

  `visit_edges_for_label_impl` (line 2000) and
  `visit_edges_for_label_with_inline_property` already had the
  tree branch (Plan 0318 Step 5). Other read paths require
  `inline_property_byte_width > 0` for dispatch (a precondition),
  which is always 0 in tree mode (set on promote), so they are
  tree-mode safe by construction.

- **Regression tests:**
  - `tree_visit_edges_via_public_api_works_on_dense_tree_bucket` in
    `tree_read.rs:447` (dense tree bucket: 4096 slots, no tombstone).
    Verified to FAIL without the fix and PASS with the fix.
  - `tombstoned_tree_bucket_visit_preserves_logical_positions` in
    `tree_read.rs:511` (REWORK-3): promotes 4K tree bucket,
    tombstones slot 100, verifies `graph.visit_edges` yields 4096
    visits (not 4095) with tombstone at position 100, all positions
    0..4096 covered exactly once, ascending AND descending.
    Verified to FAIL with the REWORK-2 fix (enumeration gives
    live-ordinal; tombstone at position 3995 in descending) and
    PASS with the REWORK-3 fix (tombstone at position 100 in both
    orders).

- **Bench verification:** the 3 previously-TRAPping benches
  (`tcsr_1048576_full_scan_descending`,
  `tcsr_1048576_random_ordinal_access`,
  `tcsr_131072_full_scan_descending`) now PASS deterministically
  (3-for-3 each) and are persisted in `canbench_results.yml`.
  Original 3 write-path benches (131K insert_grow, 1M insert_grow,
  1M root_capacity) still PASS. 4K/65K prototype baseline 0%
  regression (6/6 unchanged). Full canbench suite 158/158
  unchanged, 0 regressed, 0 failed.

- **Why the bug wasn't caught in Plan 0318 Step 5:** the Step 5
  read-dispatch tests used `tree_mode_out_edges_collect` directly
  (the LTB primitive), not the public `graph.visit_edges` API.
  The public API has its own dispatch at `visit_edges` line 786
  that unconditionally tried the dense fast path before falling
  through. The Step 5 tests also used bucket degrees below the
  LEG root size (4096 edges = 4 LTB blocks = LEG root 16 bytes),
  so even a misread from `edge_start` returned within the (small)
  memory window without trapping. The 1M bench exposed the bug
  because 1M = 1024 LTB blocks = LEG root 4096 bytes, while the
  dense read attempted 4MB.

- **Diagnostic history:** the original (REWORK-1) audit
  misdiagnosed the bug as "seeding above ~77K edges traps non-
  deterministically" and proposed reducing the bench anchor to
  131K. The REWORK-1 audit was internally inconsistent (the
  131K insert_grow bench worked, requiring seeding past the
  alleged 77K boundary). The REWORK-2 audit identified the
  read-dispatch as the actual cause; REWORK-2 verified
  determinism (3-for-3 trap) and re-root-caused the OOB.

### GAP-2026-09-02-002 — RESOLVED: `tree-mode-interior-level-insert-growth` (right-spine cascade) and the 2^30 fail-closed boundary

- **Status:** Resolved (2026-09-02, Plan 0325).
- **Severity:** P1 production-correctness defect (effective tree-mode
  cap of 2^20 per bucket was 4 KiB per label per vertex — too tight
  for any non-trivial high-degree use case).
- **Owner:** `tree_mode_insert_edge` (`tree_write.rs:58-280`),
  `tree_mode_deepen` (already shipped, level-generic), depth-generic
  resolver `resolve_leaf_block_id` (already shipped, physical depth).
- **Observed behavior:** the Plan 0318 §Step 7 amend shipped an
  interim fail-closed `TreeRootCapacityReached` guard at 2^20
  (`stored_slots + 1 > R_MAX × B = 1,048,576` in a depth-1 tree
  bucket). Production tree-mode inserts above 2^20 returned
  `Err(TreeRootCapacityReached)`. The depth-generic infrastructure
  needed for the cascade was already shipped and unit-verified
  (mixed-radix resolver, `tree_mode_deepen`, `tree_mode_flatten`,
  demote Phase 5b interior release, `bucket_span_region_len`
  physical-depth match) — only the insert-side wiring was missing.
- **Why it matters (design invariant):** ADR 0088 §4 documents
  `TREE_STRUCTURAL_CAP = 2^30` (depth 1 → 2 at 2^20, depth 2 → 3 at
  2^30, MAX_DEPTH = 3 primitive safety). The interim guard
  contradicted the documented cap by 2^10. Production canisters
  promoting high-degree label buckets to tree mode would hit the
  guard at 2^20 with no way to grow further.
- **Fix (Plan 0325):** rewrote `tree_mode_insert_edge` to wire
  the right-spine cascade:
  1. The `tail_offset == 0` branch now consults the
     **physical** root length (ceil-chain, NOT
     `derived_root_len` — `derive_depth` returns 1 for a deepened
     bucket at stored = 2^20, but the physical root has 1 entry,
     not 1024).
  2. When the physical root is at `R_MAX`, the cascade fires:
     - if `depth >= MAX_DEPTH` OR `next_stored > TREE_STRUCTURAL_CAP` →
       typed `TreeRootCapacityReached` (fail-closed at the 2^30
       structural boundary);
     - else → `tree_mode_deepen` + **re-read the bucket
       descriptor** (the caller's `bucket` copy is stale after
       deepen publishes a new `edge_start` /
       `tree_mode_physical_depth`).
  3. After the cascade, the dispatch routes to
     `tree_mode_tail_append_depth1` (depth 1) or
     `tree_mode_tail_append_depth_ge2` (depth ≥ 2). The depth ≥ 2
     path appends the new leaf id to its **home interior** (not
     the root); the root grows only when a new interior is minted
     (`l % K == 0`).
  4. New helper `resolve_interior_block_id` — the
     `resolve_leaf_block_id` hop chain truncated one level short.
     Used by `tree_mode_tail_append_depth_ge2` to find the
     home interior.

- **Audit of depth-generic infrastructure (file:line):**
  - `resolve_leaf_block_id` (tree_write.rs:516) — depth-generic ✓
  - `collect_leaf_block_ids` (tree_write.rs:586) — depth-generic ✓
  - `tree_mode_deepen` (tree_write.rs:632) — level-generic ✓
  - `tree_mode_flatten` (tree_write.rs:830) — depth-2 → depth-1 ✓
  - demote Phase 5b (tree_write.rs:1025) — interior release ✓
  - `bucket_span_region_len` (compact.rs:234) — physical-depth match ✓
  - `tree_mode_random_ordinal_access` (tree_read.rs:54) — DEPTH-1
    DEBUG_ASSERT was relaxed (the prior `block_root_index < root_len`
    debug_assert used structural root_len; relaxed to
    `block_root_index < leaf_count` for depth-2 safety)
  - `visit_tree_mode_label_bucket_edges` (tree_read.rs:115) —
    DEPTH-1 DEBUG_ASSERT was relaxed (the prior
    `debug_assert_eq!(leaf_count, root_len)` failed for depth ≥ 2;
    relaxed to `leaf_count >= root_len` invariant — actual walk
    uses `resolve_leaf_block_id` for the descent)
  - `tree_mode_insert_edge` `tail_offset == 0` branch
    (tree_write.rs:113-280) — **DEPTH-1-ONLY** (replaced by the
    cascade + depth-aware append)

- **Regression tests** (synthetic layout, host `VectorMemory`,
  cheap):
  - `tree_insert_fails_closed_at_2_30_cap`: 2^30 fail-closed at
    depth 2 + root_len = R_MAX.
  - `production_insert_path_fails_closed_at_2_30_cap`: same,
    via the public `insert_edge_skip_leaf_cascade` API.
  - `cascade_at_2_20_plus_1_deepens`: 2^20 + 1 insert SUCCEEDS,
    depth 1 → 2, stored 2^20 + 1, visit yields 2^20 + 1 slots.
  - `interior_row_append_keeps_root_constant`: depth 2 with
    2 interiors + 1 leaf in the 2nd; next insert at row 1 of the
    2nd interior (l = 1025, l % K = 1) — root unchanged.
  - `new_interior_mint_grows_root`: depth 2 with 2 full interiors
    + tail_offset == 0; next insert at l = 2048, l % K = 0 —
    root grows from 2 to 3.
  - `public_read_accessors_over_depth_2`: `tree_mode_out_edges_collect`
    walks 1,048,577 slots for stored = 2^20 + 1, depth 2.
  - Replaces the obsolete 0318 §Step 7 tests
    `tree_insert_fails_closed_at_root_capacity` and
    `production_insert_path_fails_closed_at_root_capacity` (the
    2^20 guard was superseded by the cascade + 2^30 boundary).

- **Bench surface:** REPLACED `tcsr_1048576_root_capacity_reached`
  (2^20 + 1 insert previously FAIL-CLOSED) with
  `tcsr_1048576_deepen_beyond_r_max` (2^20 + 1 insert now
  SUCCEEDS via deepen; verified PASS ×3 with depth 2 / root_len 2 /
  stored 2^20 + 1 / edge readable at slot 2^20). ADDED
  `tcsr_1048576_deepen_then_interior_grow` (2^20 → 2^20 + 1024:
  1 deepen + 1023 interior-row appends + 1 new-interior mint;
  5,111,272 ins, 4994 ins/insert).
  - Existing depth-1 benches (5 of them) 0% regression
    (insert_grow / full_scan / random_ordinal / 131K insert_grow
    / 131K full_scan): "no change" or within noise threshold
    (≤ 1% on insert_grow, 0% on read benches).
  - Full canbench 158/158 unchanged, 1 regressed
    (`bench_t_v_window` +2.10%, pre-existing from Plan 0324
    REWORK-3; small slab bench, 220 ins regression, acceptable).
  - Wasm 15,661 chars / 4,339 headroom (was 15,536; +125 for
    2 new bench names).

- **Why depth 3 is not exercised (deliberate):** ADR 0088 §4
  documents `MAX_DEPTH = 3` as a primitive-level safety bound
  (`TreeDepthLimitReached`); the production cap of 2^30 binds
  first. Depth 3 coverage (2^40 slots, 1 TiB per bucket) is
  structurally wired (the resolver handles it, the canbench
  surface cannot seed it: 17.9T ins > 10T per-bench limit). Lifting
  the cap to 2^40 in production requires an ADR amend (out of
  scope for this slice).

### GAP-2026-08-28-001 — RESOLVED: index-anchored scans are unattributed in authz requirement extraction (grants silently stop working when an index exists)

- **Status:** Resolved (2026-08-28). Fixing commit `0aebf4513`; Router suite 1045 → 1056 green.
- **Severity:** P1 authorization-semantics defect (availability + plan-dependence)
- **Owner:** Router authz requirement walker (`crates/router/src/authz.rs` `walk_op`,
  `PlanCatalogView`, `RouterCatalogView`) with the index catalog seam
  (`crates/router/src/facade/stable/indexed_catalog.rs`)
- **Observed behavior:** On the demo network — the one environment with an active property
  index (`document_title`, demo migration 000002) — every index-anchored read was denied to
  non-tenants regardless of grants: `MATCH (d:Document {title: 'X'}) RETURN 1` → uniform
  `Forbidden` for anonymous, while the identical shape over the deliberately unindexed
  `Concept.name` executed, and the registry owner (tenancy root) always passed. The walker's
  `PlanOp::IndexScan` arm deferred the anchor's property read without noting any label fact;
  an index anchor never receives a later `NodeScan`, so the deferred read resolved with
  `unique_vertex_label == None` → `require_unattributed` → fail-closed tenancy-only. The
  same gap existed in the `IndexIntersection` arm and the `ConditionalIndexScan` candidate
  path (only `fallback_label` was noted). Every extraction contract test ran indexless
  fixtures (ADR 0054 indexless bootstrap), so the planner always emitted `NodeScan` and the
  `IndexScan` arms had zero walker coverage — the bug was invisible to the suite and only
  the live demo network surfaced it.
- **Why it matters (design invariant):** authorization demands must not depend on which
  physical plan the planner chooses. Before the fix, creating an index silently changed who
  could run an existing prepared query — physical layout leaking into logical access
  control. Removing the demo's index would have hidden the defect, not fixed it.
- **Implemented behavior:** `PlanCatalogView` gained a catalog-driven label resolution for
  the unique active vertex property index covering a property (same unique/fail-closed
  semantics as the executor's `active_vertex_physical_index`); the `IndexScan`,
  `IndexIntersection`, and `ConditionalIndexScan` walker arms now note the resolved label
  fact and demand the same scan-side rows an equivalent `NodeScan` would (`Match` +
  `Read`/`ReadProperty` via `require_vertex_scan_rows`). Ambiguous or inactive index
  resolution stays unattributed (fail-closed, unchanged). The PocketIC citation-reach flow
  fixture now creates `document_title` and drains maintenance to active, pinning the
  index-anchored path end-to-end: a non-owner executes the op after the same PUBLIC grant
  surface.
- **Evidence:** live bisection on the demo network (indexed `Document.title` filter denies,
  unindexed `Concept.name` filter passes; plain scans, traversal, quantified paths, and
  `ELEMENT_ID` projections all pass) + indexless-vs-indexed extraction tests in
  `crates/router/src/gql_grants.rs` and walker unit tests in `crates/router/src/authz.rs`.
- **Next decision:** none open for the walker. Long-term, the planner could carry the label
  in `PlanOp::IndexScan` (self-describing plan) — planner-stream territory, deferred until
  the next plan-wire revision; the catalog-driven resolution matches executor semantics and
  needs no wire change.

### GAP-2026-08-26-001 — RESOLVED: `ELEMENT_ID` over quantified-path group variables fail-closed as "requires element indexing"

- **Status:** Resolved (plan 0307 slice A, 2026-08-26) — executor completeness for GQL
  group variables, not a demo workaround.
- **Severity:** P2 demo-blocking executor gap
- **Owner:** graph executor expression evaluation
  (`crates/graph/src/plan/query/executor/eval.rs`, `eval_element_id`)
- **Observed behavior:** With the PUBLIC grant surface applied, the knowledge demo's
  `citation-reach` op (`ELEMENT_ID(e)` over `-[e:CITES]->{1,3}`) failed at execution for
  ANY caller (owner included) with `InvalidArgument("invalid query expression value for
  'ELEMENT_ID(e) on a group edge variable requires element indexing'")`. Quantified-path
  variables bind as groups (`PlanBinding::EdgeGroup` hop trail / `VertexGroup`), and
  `eval_element_id` rejected all three group binding kinds since commits `51ba273c9` /
  `c7a6eb01a` (2026-06-08). Authorization was never the blocker: requirement extraction
  adds no demand for element-id reads (plan 0306 contract test
  `edge_element_id_projection_demands_stay_attributed`).
- **Expected or needed behavior:** GQL group variables are lists; an element-identity read
  over them returns a list of element ids in group order (empty group → empty list), so
  published ops projecting edge identity execute end-to-end for authorized callers.
- **Resolution:** `eval_element_id` now evaluates `EdgeGroup` → `Value::List` of edge-id
  bytes in traversal order and `VertexGroup` → list of vertex-id bytes in group order,
  reusing the singleton-arm encodings verbatim; empty groups yield empty lists;
  `PathGroup` stays fail-closed with an actionable message (paths are not elements; use
  `CARDINALITY(p)` or path element access). No planner, wire, rkyv, or authz changes.
  Tests: host-level `element_id_on_edge_group_lists_hop_ids_in_traversal_order`,
  `element_id_on_vertex_group_preserves_order_and_empty_group_yields_empty_list`,
  `element_id_on_path_group_stays_fail_closed_with_guidance`; PocketIC
  `knowledge_demo_citation_reach_flow.rs` upgraded to full row-content assertions
  (default-deny leg preserved; non-owner execution with per-row hop-count-pinned
  `cite_edge_id` lists).
- **Evidence:** detection run during brief #2 PocketIC validation (w1:p8, 2026-08-26);
  root cause read of `eval.rs` / `expand/var_len.rs`
  [group-variables.md](execution/group-variables.md); post-fix green runs recorded in the
  Validation Transcript of `plans/0307-group-element-id.md`.
- **Related contracts:** GAP-2026-08-24-008 Impact follow-up (a) — citation-reach runtime
  execution is now actually proven; the 2026-08-26 plan 0306 note was extraction-correct
  but execution remained blocked until this slice.
- **Detection:** w1:p8 during brief #2 validation (2026-08-26); resolved same day by w1:p8
  under brief #3.

### GAP-2026-08-25-004 — Committed HEAD fails `gleaph-router` lib-test standalone: `GqlQueryResult.truncated` constructor updates left uncommitted

- **Status:** Resolved by commit 5cb98eb4c (2026-08-25; landed by w1:pM after the owning pane
  could not be identified and the fix was routed through the operator)
- **Severity:** P1 repository integrity
- **Owner:** `crates/router/src/federation/aggregate_index_fast_path.rs` constructor call sites
  for the `truncated` field added to `GqlQueryResult` by kernel commit `49f10d461` (ADR 0078)
- **Observed behavior:** An isolated worktree at `d31909605` — and equally at then-HEAD —
  failed `cargo test -p gleaph-router --lib` with
  `E0063: missing field 'truncated' in initializer of GqlQueryResult`
  (`aggregate_index_fast_path.rs:120/139/254`). The main tree compiled only while the owning
  stream's uncommitted WIP stayed present, so the breakage was invisible locally.
- **Expected or needed behavior:** Kernel-side field additions and their constructor-site
  updates land in the same commit (GAP-2026-08-24-007 precedent).
- **Resolution:** All three aggregate fast-path result constructors initialize
  `truncated: None` (commit `5cb98eb4c`). Isolated-worktree verification at the fixing commit:
  `cargo test -p gleaph-router --lib` → 980 passed / 0 failed, including the ADR 0081 Slice A
  router-side ordered-range seed fail-closed surface.
- **Evidence:** `git log -S "truncated" -- crates/graph-kernel/src/plan_exec.rs` → `49f10d461`;
  isolated-worktree build log; fixing commit `5cb98eb4c`.
- **Related contracts:** [ADR 0078](adr/0078-authz-aware-vector-search.md)
- **Detection:** w1:pM during ADR 0081 Slice A verification (2026-08-25).

### GAP-2026-07-14-001 — Ordered incremental slab compaction repeatedly scans packed prefixes

- **Status:** Resolved by Plan 0201 (ADR 0052 Slice 6; commit in the same patch)
- **Severity:** P2 maintenance-performance risk
- **Owner:** `ic-stable-lara` labeled edge-slab compaction and deferred-maintenance cursor state
- **Observed behavior:** `compact_vertex_edge_span_one_step` preserves edge scan order and emits at
  most one `EdgeSlotMove` per queue pop, but `first_edge_slot_move_in_bucket` restarts at slot zero
  after every move. Alternating tombstones therefore make full bucket compaction quadratic in the
  resident slab width. After edge-log and inline-property maintenance were separated, canbench measured
  `bench_labeled_stage2_hub_delete_half_by_slot_then_compact_1024` at 73.36M instructions versus
  4.14M previously; the existing 4,096- and 16,384-edge cases were already dominated by the same
  quadratic shape.
- **Expected or needed behavior:** preserve ascending/descending edge scan order and immediate
  per-edge sidecar/index re-keying while carrying enough progress state that successive bounded
  maintenance steps do not rescan the already-packed prefix.
- **Resolution:** `CompactVertexEdgeSpan` (v1 work item) carries a `resume_slot_index` cursor;
  `compact_vertex_edge_span_one_step` and both finders scan from the cursor with `next_live` seeded
  from it (packed-prefix invariant), `EdgeMoved` re-enqueues carry the advanced cursor,
  `AdvanceBucket` resets it, and a cursor-scan miss triggers a full re-scan from slot zero before
  `stored_slots = degree`, so an interleaved delete inside the already-packed prefix can never
  truncate a live edge. `bench_labeled_stage2_hub_delete_half_by_slot_then_compact_1024` moves off
  the quadratic shape.
- **Evidence:** `crates/ic-stable-lara/src/labeled/graph/compact.rs::first_edge_slot_move_in_bucket`
  (cursor parameter), `compact_vertex_edge_span_one_step`; regression tests
  `compaction_cursor_packs_alternating_tombstones_across_steps` and
  `compaction_cursor_survives_interleaved_delete_in_packed_prefix` in
  `labeled/bidirectional/deferred.rs`; ADR 0020's one-`EdgeMoved`-per-pop contract.
- **Related contracts:** [ADR 0001](adr/0001-labeled-segment-slide.md),
  [ADR 0020](adr/0020-deferred-maintenance-timer-drain.md),
  [ADR 0052](adr/0052-per-label-adjacency-order-and-tombstone-reuse.md) Slice 6

### GAP-2026-07-04-003 — No application-facing vertex-embedding ingestion boundary

- **Status:** Resolved by plan 0048 implementation
- **Severity:** P1 product gap
- **Owner:** Router authorization/resolution + Graph validation boundary + Vector durable store
- **Observed behavior:** before plan 0048, there was no canister API for an application or deployment
  tool to write a canonical vertex embedding. Vector-index fixtures and demos had to seed the
  derived `graph-vector-index` canister directly, bypassing Graph canonical ownership and the
  Router embedding-name catalog.
- **Expected or needed behavior:** an authorized caller should submit only graph name, opaque encoded
  vertex id, registered embedding name, and finite F32 values to the typed Router endpoint; Router
  should validate the value dimension/finiteness before stamp allocation and the Graph await, Graph
  should validate vertex/tombstone/label and payload-independent embedding metadata, and every
  allocated Router stamp should have one exact durable owner before the first Graph await.
- **Resolution:** The typed Router `ingest_vertex_embeddings` flow validates the encoded id, live
  shard, registered
  vector definition, value dimension, and finiteness, then synchronously co-writes the allocated
  stamps and exact `AwaitingGraph` intents before the first Graph await. Graph `stamp_embedding`
  validates existence/tombstone state, required labels, and payload-independent embedding
  metadata/encoding, then returns the Router-issued stamp without embedding-byte or journal writes.
  Exact acceptance transitions the row to `AwaitingVector`; exact rejection changes the row to
  `AwaitingFrontier`; an observed Vector prefix also changes only the applied rows to
  `AwaitingFrontier`. Unknown Graph, Vector, or frontier outcomes retain the applicable phase. No
  finite-time or client-level exactly-once guarantee is implied.
- **Evidence:** `crates/router/src/api/control.rs::ingest_vertex_embeddings`;
  `crates/graph/src/canister/handlers.rs::stamp_embedding`; and the focused Router/Vector unit
  coverage. The named PocketIC gate `graph_only_markerless_lane_advances_frontier_on_autonomous_timer`
  passes exactly one test with seven filtered, one `PocketIc`, one federation, and four canister
  installs; it advances the ordinary timer with an empty MemoryId 53 lane and exercises bounded GC.
  The persisted Router `crates/router/canbench_results.yml` artifact has all 41 terminal benchmark
  entries; `markerless_frontier_catalog` records exactly 52,080,138 total and 52,079,143 scoped
  instructions. The dated Plan 0277 focused/persisted Vector `bench_router_frontier_gc_budget`
  baseline records 2,110,300,092 total and 2,110,299,087 scoped instructions. The later live
  artifact currently records 2,084,327,695 total and 2,084,326,690 scoped instructions from
  unrelated work and is not Plan 0277 evidence. The targeted
  affected-crate format check passes, while the
  workspace-wide format check is blocked by unrelated dirty paths and never passed. The bounded
  Quint model has fifteen deterministic tests and 5,000 sampled traces with all requested witnesses;
  `quint verify` remains unrun and non-required, so this is not production proof or a finite-time
  liveness claim.
- **Related contracts:** [ADR 0031](./adr/0031-vertex-embedding-store-and-derived-vector-index.md),
  [design/index/vector-index.md](./index/vector-index.md),
  [design/execution/pipeline.md](./execution/pipeline.md)

### GAP-2026-07-04-002 — `NEXT INSERT` lost edge endpoint identity

- **Status:** Resolved by commit `27e993ae`
- **Severity:** P1 correctness defect
- **Owner:** GQL block planning and Graph projection/mutation execution
- **Observed behavior:** a `MATCH ... RETURN ... NEXT INSERT (a)-[:L]->(b)` mutation reported
  success, but a later traversal observed disconnected/`NULL` endpoints. Separate seed operations
  could not build a shared-vertex social graph.
- **Resolution:** no-YIELD `NEXT` boundaries now preserve typed graph bindings; already-bound node
  variables are not planned as new vertices; plain-variable projections retain `PlanBinding`
  identity through native and wire execution.
- **Evidence:**
  `gql_run::tests::{block_match_next_insert_edge_keeps_endpoints,wire_block_match_next_insert_edge_keeps_endpoints,block_match_next_insert_edge_shares_source}`.
- **Related contracts:** [gql/plan-format.md](gql/plan-format.md),
  [execution/pipeline.md](execution/pipeline.md)

### GAP-2026-07-17-001 — Dynamic instruction-budget headrooms lack measured acceptance criteria

- **Status:** Resolved by measured canbench + PocketIC acceptance (2026-08-03/2026-08-04; commits
  `test(graph): measure plan-batch tail costs for headroom acceptance`, `test(graph): verify
plan-batch drain boundary end to end`, `test(graph-index): measure posting_batch loop costs for
headroom acceptance`, `test(vector-index): measure vector_sync_batch loop costs for headroom
acceptance`, `test(graph-index): measure batched page-answer lookahead tail`, `test(router):
measure graph batch chunk decision costs for headroom acceptance`, `test(graph): measure timer
maintenance tick and per-step costs`)
- **Severity:** P1 availability and resumability risk
- **Owner:** Router, Graph, graph-index, and graph-vector-index dynamic batch boundaries
- **Observed behavior:** Multiple canister paths stop before the 40B update-call limit using
  independently chosen values, but no repository benchmark or acceptance table establishes that
  each headroom covers its owning path's final operation, response encoding, post-operation drain,
  and cross-canister call overhead. Current examples include Router's 5B headroom, Graph's pending
  5B headroom for `execute_plan_update_batch`, graph-index and graph-vector-index batch ceilings at
  32B with 100M per-loop reserves, graph-index's 1B update / 0.5B query lookahead, and Graph timer
  maintenance at 32B with a 100M reserve. These values have different ownership and semantics and
  must not be treated as interchangeable.
- **Resolution:** every dynamic batch / bounded maintenance loop in the GAP now has measured,
  path-specific instruction-ceiling and headroom acceptance evidence (plan-batch, posting_batch,
  vector_sync_batch, page-answer lookahead, Router chunk decision, timer maintenance):
  - **Graph plan-batch** (`execute_plan_update_batch`, `GRAPH_BATCH_FINAL_BOOKKEEPING_INSTRUCTION_HEADROOM`
    2B + 500M drain estimate): adversarial single operation 32.98M < `MIN_OP_INSTRUCTION_ESTIMATE`
    (50M) and far below the ~37.5B trap threshold; response tail ≤ 581K vs the 2.5B reserve; the
    inter-canister drain is covered by the PocketIC boundary test
    (`adr0060_plan_batch_instruction_boundary.rs`) which completes a 300-operation NEXT-chained
    mutation on a converged index without trapping and serves the drained posting.
  - **graph-index `posting_batch`** (32B ceiling, 100M reserve): max single op ≈1.92M (4096-byte
    value) + response 38K, two orders below the reserve; ceiling cuts off only after ~16.6K
    max-value postings. Fully local; no boundary test needed.
  - **graph-vector-index `vector_sync_batch`** (32B ceiling, 100M reserve): max single op ≈1.12M
    (flat in dims) + response 38K, two orders below the reserve; ceiling cuts off after ~29K
    upserts. Fully local; no boundary test needed.
  - **graph-index page-answer lookahead** (500M query lookahead): max per-check work (worst page
    ≈206.4M + encode ≤ 316K) ≈206.7M, 2.4× below the lookahead. The 1B update lookahead has no
    caller and needs no evidence until used.
  - **Router mutation-batching chunk decision** (`ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` 4B):
    decision cost 6.01M nominal / 37.34M adversarial (≈0.15% / ≈0.9% of the reserve); the
    remaining per-chunk tail is bounded by the inter-canister sizing policy (≤2 MiB) and the
    dispatch count is bounded by live shard count, so no dedicated boundary test was added.
  - **Graph timer maintenance** (`MAX_TIMER_MAINTENANCE_INSTRUCTIONS` 32B,
    `TIMER_MAINTENANCE_INSTRUCTION_HEADROOM` 100M): single work item 61.60K and whole tick on a
    dense 2048-edge hub 1.33M, so the worst per-check work (≤ whole tick) is ≈1.3% of the reserve
    and a realistic backlog drains in ≈0.004% of the cap. Fully local; no boundary test needed.
    Follow-up refactor (`relocate_edge_properties_for_move`): the inline-property posting rekey,
    previously a post-pass loop outside the budget check, now runs inside the per-move observer
    through the unified property relocation, so the entire edge-move consequence is covered by
    the per-item cutoff; the measured tick is unchanged (1.33M, re-persisted 2026-08-04).
- **Evidence:** canbench targets and persisted results: `crates/graph/src/bench/plan_batch.rs`,
  `crates/graph/src/bench/timer_maintenance.rs`, `crates/graph-index/src/bench.rs`,
  `crates/graph-vector-index/src/bench.rs`, `crates/router/src/bench.rs`, with the matching
  `canbench_results.yml` per crate; the plan-batch PocketIC boundary test
  `crates/pocket-ic-tests/tests/adr0060_plan_batch_instruction_boundary.rs`. No headroom was
  justified by copying another canister's constant: each path carries its own measured worst-case
  and tail numbers.
- **Related contracts:** [ADR 0041](adr/0041-router-graph-batch-mutation-dispatch.md),
  [ADR 0042](adr/0042-router-dynamic-instruction-budget-batching.md),
  [ADR 0020](adr/0020-deferred-maintenance-timer-drain.md),
  [design/index/property-index.md](index/property-index.md),
  [design/index/vector-index.md](index/vector-index.md)

### GAP-2026-08-07-001 — Newer-incarnation replacement can tombstone the old row before a fallible append

- **Status:** Resolved by a commit-before-tombstone reorder in `vector_upsert` (Plan 0233).
- **Severity:** P2 correctness edge (only reachable on stable-memory OOM during a subject-map commit)
- **Owner:** graph-vector-index (`VectorCanisterStore::vector_upsert`)
- **Observed behavior:** In the `mutation_id > stamp` (newer-incarnation) branch, the superseded old
  active/shadow slots were tombstoned _before_ the fallible `VECTOR_SUBJECT_TO_ID` commit. If that
  commit failed (`StableGrowFailed`, stable-memory OOM), the retained old subject entry pointed at a
  now-tombstoned row, ghosting the subject from search.
- **Resolution:** The branch now commits the subject map (pointing at the new slot) before tombstoning
  the old slots; on a commit failure it tombstones only the just-appended rows, leaving the old slot
  live and the retained entry consistent. A regression test
  (`newer_stamp_upsert_commit_failure_keeps_old_slot_live`) injects a subject-map insert failure via
  a test seam (`arm_subject_insert_failure`) and asserts the old slot stays live and `live_len` is
  unchanged. The resurrection case is likewise fixed: `unmark_deleted` now runs only after a
  successful commit.
- **Evidence:** `crates/vector-canister/src/facade/store/mutation.rs` (reorder + seam),
  `crates/vector-canister/src/facade/store/tests.rs`
  (`newer_stamp_upsert_commit_failure_keeps_old_slot_live`).
- **Related contracts:** [ADR 0032](adr/0032-vector-index-slab-page-store.md)

## Property and index capability gaps

The following property/index status was verified against the implementation at 2026-08-10 00:28:05 UTC +0000.
The canonical property-index contract remains [design/index/property-index.md](index/property-index.md);
this section records only the remaining gaps and the boundary at which each one is owned.
[ADR 0059](adr/0059-create-index-migration-backfill.md) is the accepted, partially implemented
source of truth for the migration-driven `CREATE INDEX` backfill lifecycle; this ledger does not
duplicate its state machine or ownership rules.

### Priority order

| Priority | Work item                                                                                  | Current status                                                                               |
| -------- | ------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------- |
| P0       | Backfill existing vertex, sidecar, and INLINE values before advertising an index as active | Closed — vertex/sidecar closed 2026-08-22 via GAP-2026-07-29-006; edge INLINE closed 2026-08-23 via GAP-2026-08-22-001 + GAP-2026-07-29-001 (identity rule + un-ignored cross-canister scenarios, 5/5) |
| P1       | Define INLINE removal/`NULL` transitions and complete vertex `MATCH` range planner wiring  | Closed 2026-08-22 — GAP-2026-07-29-004 resolved by contract-pinning tests (see entry); GAP-2026-07-29-002 closed 2026-08-21                                |
| P1       | Add edge range postings, Router seed planning, and execution support                       | Closed 2026-08-22 — GAP-2026-07-29-003 (see entry)                                           |
| P1       | Restore anchored multi-DML roll-forward saga convergence on both shards                    | Closed 2026-08-22 — GAP-2026-08-21-001 (see entry)                                           |
| P2       | Add vertex nested-record field indexes with a canonical dotted-path contract               | Open for slice-4 acceptance — GAP-2026-07-29-005 (ADR 0073 slices 1–3 implemented and validated; slice 4 landed on `39746f7b3`, acceptance pending Plan 0285 validation) |
| P2       | Add record/list index semantics and tests, after the scalar/leaf contract is fixed         | Planned                                                                                      |
| P2       | Extend edge-index anchors to accept `ScanValue::InList` union probes (symmetric with the vertex IN-list anchor landed 2026-08-24) | Resolved — GAP-2026-08-24-002 (`ScanValue::InList` reused end to end; planner fusion, three executor probe-union consumers, Router `EdgeEqualUnion` seeds) |
| P2       | Cypher bracket-form `IN […]` fails to parse under combined `cypher` + `sql-compat` builds (sql-compat arm requires `(` unconditionally) | Resolved — GAP-2026-08-24-003 (sql-compat arm claims only `(`-openers; per-combo feature matrix pinned) |
| P1       | Endpoint property projection on `EdgeBindEndpoints` replaces the vertex binding, so trailing `IsLabeled` filters drop every row of anchored edge scans with property-level projections | Resolved — GAP-2026-08-24-004 (planner entity-use veto; whole-variable projections were unaffected) |
| P2       | Extend edge-index anchors to accept `ScanValue::TextPrefix` prefix intervals (symmetric with the vertex STARTS WITH anchor landed 2026-08-24) | Resolved — GAP-2026-08-24-010 (edge symmetric extension landed 2026-08-24) |
| P3       | Decide edge-property uniqueness enforcement and multi-canister index sharding axes         | Planned                                                                                      |
| P3       | Cache resolved `GraphTypePropertySchema` beside the definition heap cache (steady-state O(1) catalog resolve; candidate from the 2026-08-25 canbench attribution review) | Open — GAP-2026-08-25-002 (catalog owner decision)                                           |
| P2       | ADR 0081 index-ordered ORDER BY delivery: Slice A (single-key ASC, equality/IN/range/prefix anchors, wire intent + executor tie-boundary TopK) landed; Slice B DESC and merge-aware cross-shard union deferred | Resolved 2026-08-25 — Slice A fix `7a7484eaf` + hardening `bench`/tests (cross-shard union tracked as GAP-2026-08-25-003)                                    |

The P0 item is a prerequisite for trusting any newly created index. The range premise is narrower:
the ordered scan primitive already exists through `StableBTreeMap::range()`; the remaining work is
to expose its capability at every required planner and entity boundary. Range support must not be
reimplemented as a full-bucket materialization.

### Range index status

Range traversal itself is **implemented**, not missing: `graph-index` converts the encoded bounds to
a half-open `StableBTreeMap::range()` scan, and paginated, label-sieved vertex range endpoints are
available. Router `SEARCH ... WHERE` also uses those endpoints for the supported numeric range
shapes. The missing pieces are planner coverage and edge symmetry, not the basic range scan.

### GAP-2026-07-29-001 — Existing INLINE values are not included by property-index backfill

- **Status:** Closed (2026-08-23). The Graph edge-property export enumerates both canonical
  value domains under one opaque cursor: sidecar `EDGE_PROPERTIES` rows first, then canonical
  edges carrying indexed inline values, decoded through the same `inline_index_values`
  membership resolution mutations use. The cursor carries a domain tag byte
  (`0x00` sidecar key, `0x01` inline `(wire label, owner)` position); untagged legacy cursors
  are rejected cleanly. Inline enumeration visits outgoing rows only and keeps the exact
  mutation identity — directed forward rows always, undirected max-endpoint rows only — so no
  mirror double-posting and every later Remove can address what backfill inserted. A vertex
  started within budget is collected fully (resume advances between vertices), so resume never
  skips or replays an identity; `done` is true only after both domains exhaust.
  ADR 0059's one-opaque-export consequence is now implemented as written. Owning unit tests:
  `backfill_enumerates_indexed_inline_scalar_values`,
  `backfill_emits_only_the_canonical_undirected_owner`,
  `backfill_resume_walks_both_domains_without_duplicate_identities`,
  `unversioned_backfill_cursor_is_rejected`. The deferred cross-canister create-index lifecycle
  proof (plans/0281 scenarios) initially exposed GAP-2026-08-22-001; after that identity fix
  landed (plan 0282), both un-ignored scenarios pass —
  `edge_inline_create_index_migration_converges_active_with_complete_postings` and
  `edge_inline_same_wasm_upgrade_mid_build_resumes_and_converges` in
  `crates/pocket-ic-tests/tests/adr0059_index_build_lifecycle.rs`, 5 passed / 0 failed at the
  closing run — completing this entry's migration-path validation over inline data, including
  equality/range completeness, per-shard coverage, multiplicity, undirected canonical-owner
  uniqueness, and same-wasm upgrade reopen.)
- **Severity:** P0 index correctness
- **Owner:** Router backfill orchestration and Graph inline-property backfill boundary
- **Observed behavior:** The edge-property backfill scans canonical `EDGE_PROPERTIES` only. Existing
  values stored in the edge inline-property region are therefore not enumerated by that backfill,
  although subsequent mutations can maintain an indexed inline property when the router supplies
  the active catalog.
- **Expected or needed behavior:** Creating or enabling an index on an eligible INLINE property must
  converge existing inline values before the index is advertised as active; the backfill cursor and
  posting owner must cover the inline storage domain as well as the sidecar domain.
- **Evidence:** `crates/graph/src/index/edge_property_backfill.rs` calls
  `scan_edge_properties_batch`; inline storage is owned by `crates/graph/src/edge_inline_property_schema.rs`
  and the edge store helpers. `design/storage/labeled-edge-inline-properties.md` describes the
  inline bytes as canonical for the declared inline property.
- **Impact:** A newly created index can miss pre-existing inline values and return incomplete
  equality/range candidates until those values are rewritten.
- **Next decision:** The [ADR 0059](adr/0059-create-index-migration-backfill.md) lifecycle (one
  Graph-owned opaque export, graph-index pull, touched-first exact outbox mutation, base-seed guard,
  and Active-only transition) is implemented with its cross-canister PocketIC proof complete
  (GAP-2026-07-29-006, closed 2026-08-22); this entry's remaining work is the edge `INLINE`
  enumeration itself. The existing operator cursor endpoints remain separate from that lifecycle.

### GAP-2026-07-29-002 — Normal `MATCH` planning does not select vertex range indexes

- **Status:** Closed — implemented (2026-08-21)
- **Severity:** P1 query capability
- **Owner:** Router catalog projection and GQL planner statistics boundary
- **Observed behavior:** The graph-index range API and `SEARCH` range path are present, but
  `RouterGraphStats::is_vertex_property_range_indexed` returned `false` unconditionally, so the
  normal `MATCH ... WHERE` planner could not use a vertex range index through its
  planner-statistics contract. Fixing only the stats gate surfaced the second half: the Router
  seed-anchor model had no range variant, so a leading non-Eq `IndexScan` reached graph shards
  with no index client and failed at runtime.
- **Implemented behavior:** Both halves are wired. The Router projects range capability from the
  same Active vertex catalog membership as equality (`planner_stats.rs`, fail-closed for
  unindexed properties); anchor selection lowers one-sided MATCH range predicates to
  `IndexScan` with the original predicate retained as a residual `PropertyFilter`; the Router
  seed model gained `IndexAnchor::Range` (`seed.rs::RangeSeedProbe` with `SeedRangeBound`)
  extracted from leading non-Eq `IndexScan` ops and resolved through the new
  `IndexLookup::lookup_range` paginated collector; Graph already skips any seeded leading scan op.
- **Evidence:** `crates/router/src/planner_stats.rs`, `crates/router/src/seed.rs`,
  `crates/router/src/index_lookup.rs`; planner contracts in
  `crates/gql-planner/tests/planner_tests.rs::match_range_anchor_*`; runtime contract
  `crates/pocket-ic-tests/tests/router_gql_query.rs::single_shard_vertex_index_match_range`.
- **Next decision:** None open for vertex MATCH ranges; two-sided bounds ride the same anchor via
  residual conjunction, and cross-type overscan clamping is a later performance refinement.

### GAP-2026-07-29-003 — Edge range index has no query/planner path

- **Status:** Closed — implemented (2026-08-22)
- **Severity:** P1 query capability
- **Owner:** Graph-index edge posting API, Router edge seed planning, and Graph edge candidate execution
- **Observed behavior:** Edge postings support equality lookup and paged equality lookup, but no
  `lookup_edge_range` endpoint, edge range request path, or edge range planner-statistics capability
  exists. The implemented `PostingRangeRequest` path is vertex-posting oriented.
- **Expected or needed behavior:** An edge property declared indexable should support the same
  encoded ordering contract for bounded range predicates, including label/direction scoping and
  resumable pages, before the planner emits an edge range scan.
- **Implemented behavior:** The ledger next-decision was adopted. graph-index gained the paginated
  ordered endpoint `lookup_edge_range_page`
  (`crates/graph-index/src/facade/store/edge_postings.rs`, key bounds derived in
  `crates/graph-index/src/posting_range.rs::edge_posting_key_half_open_range`) preserving the full
  `(physical_index_id, property_id, value, label_id, shard_id, owner_vertex_id, slot_index)` order
  with an in-canister wire-label sieve and the existing direction subset rule; kernel request type
  `LookupEdgeRangePageRequest` reuses `PostingRangeRequest` and the `EdgePostingCursor`. The Router
  projects edge range capability from the same Active edge catalog membership as equality
  (`RouterGraphStats::is_edge_property_range_indexed_for`, fail-closed), lowers one-sided leading
  range predicates to `IndexAnchor::EdgeRange(EdgeRangeSeedProbe)` and executes them drain+clamp:
  pages are drained within the 0270 comparison-domain interval (`gql::range_bounds_for_encoded_key`)
  and residual predicates stay as filters — no cursor state survives the call. The planner emits
  one-sided `PlanOp::EdgeIndexScan{cmp}` via the leading-edge fusion path (equality wins first),
  retaining the original predicate as a residual filter. The single-shard executor clamps through
  the shared helper and falls back to a canonical `EDGE_PROPERTIES` filtered superset scan for
  unsupported comparison domains.
- **Evidence:** storage contracts
  `crates/graph-index/src/facade/store/tests.rs::lookup_edge_range_page_*`; router contracts
  `crates/router/src/seed.rs::edge_range_scan_anchor_*`,
  `crates/router/src/gql.rs::edge_range_seed_probe_*`; planner contracts
  `crates/gql-planner/tests/planner_tests.rs::match_edge_range_anchor_*`; executor contract
  `crates/graph/src/plan/query/executor/scan/tests.rs::executes_edge_range_index_scan_with_domain_clamped_between`;
  runtime `crates/pocket-ic-tests/tests/router_gql_query.rs::single_shard_edge_index_match_range`
  and `federated_edge_index_match_range_with_domain_clamp`.
- **Next decision:** None open; `!=` complement policy remains Slice D.

### GAP-2026-07-29-004 — INLINE removal/NULL transitions need explicit posting semantics

- **Status:** Resolved (2026-08-22; contract pinned by owning tests — the implementation already
  satisfied the intended old-key/new-key semantics. Executor level: SET of top-level `NULL`
  rejects with `NullInlineProperty`, struct records missing a schema field or carrying a `NULL`
  leaf reject with `InvalidInlinePropertyValue`, and every rejection leaves the stored bytes
  untouched, so no stale posting can be produced. Store level: accepted updates dispatch exactly
  Remove(prev)+Insert(next) per membership, equal-value updates dispatch nothing, a leaf-scoped
  membership transitions only its own leaf key, and a width-mismatched update rejects before any
  posting or row change. Contract recorded in
  `design/storage/labeled-edge-inline-properties.md` §Mutation write semantics. Owning tests:
  `inline_edge_scalar_set_null_aborts_without_write`,
  `inline_edge_struct_set_missing_field_aborts_without_write`,
  `inline_edge_struct_set_null_leaf_aborts_without_write` (executor),
  `updating_indexed_inline_scalar_swaps_posting_old_for_new`,
  `updating_indexed_inline_scalar_to_same_value_emits_no_posting`,
  `updating_indexed_inline_struct_leaf_replaces_only_that_leaf_posting`,
  `updating_indexed_inline_bytes_with_wrong_width_rejects_before_write` (store). No code-path
  change, so no benchmark artifact update is required.)
- **Severity:** P1 index consistency
- **Owner:** Graph inline-property mutation and index posting dispatch
- **Observed behavior:** Inline index maintenance now supports eligible scalar values and struct leaf
  paths, but the contract for removing a field, assigning `NULL`, and changing a struct shape is not
  yet recorded as a complete old-key/new-key transition.
- **Expected or needed behavior:** Every mutation must remove the old sortable posting when the
  indexed value disappears or becomes non-indexable, and insert the new posting only after the
  inline bytes and decoded value agree. A missing/NULL value must never remain queryable as a stale
  posting.
- **Evidence:** `crates/graph/src/property/inline_dispatch.rs`,
  `crates/graph/src/facade/store/edge_profiles.rs`, and the inline-property contract in
  `design/storage/labeled-edge-inline-properties.md`.
- **Impact:** Deletes and shape changes can leave false-positive index candidates even when scalar
  replacement is correct.
- **Next decision:** Add mutation-contract tests for remove, NULL, missing nested field, and
  non-indexable replacement before widening inline index coverage.

### GAP-2026-07-29-005 — Vertex nested-record indexes are not yet symmetrical with edge INLINE fields

- **Status:** Resolved 2026-09-08 (slice-4 acceptance closed; uncommitted working-tree change, no commit per primary-owns-commits). Slices 1–3 are implemented and
  terminal-validated (focused unit gates, graph/graph-index all-target check/clippy, focused
  canbenches, posting-level PocketIC lifecycle, and an independent detached-worktree run at the
  `aa7fd9124` baseline plus only the remediation patch); domain decision recorded in
  [ADR 0073](adr/0073-vertex-nested-property-indexes-share-dotted-path-domain.md), 2026-08-22.
  Slices 1–3 landed: kernel `IndexedVertexMembership` gained required `field_path` +
  `ancestor_property_id` (Router-interned leaf identity plus the ancestor record property Graph
  dispatches by); Router DDL interns leaf and ancestor identities and validates bounded depth
  and typed leaf kind (`MAX_VERTEX_NESTED_INDEX_SEGMENTS`, container leaves rejected at declare
  time); vertex mutation dispatch, delete planning, the index-build fence, bulk writes, and the
  repair backfill all expand through one owner
  (`crates/graph/src/property/index_dispatch.rs::vertex_posting_transitions`) over the shared
  dotted-path resolver (`crates/graph/src/property/dotted_path.rs`), riding the existing
  old-key/new-key transition contract; migration builds export nested facts through
  `CanonicalExportTarget::Vertex.record_source`. Owning tests: router DDL interning/validation
  (`nested_vertex_ddl_*` in `index_catalog.rs`), GAP-004-grade store contracts for the record
  domain (`nested_record_*` in `catalog_context.rs`), record-walking backfill/export units, and
  the cross-canister lifecycle scenario
  `vertex_nested_create_index_migration_converges_active_with_complete_postings`
  (Building→Sealing→Active over pre-existing records on both shards including the three absence
  shapes, equality multiplicity, label scoping, and post-Active rewrite swaps).
  Slice 4 is present on main since `39746f7b3` (2026-08-23) but its acceptance stays pending
  Plan 0285 validation: the planner's property-access extractor lowers a nested chain to
  the canonical dotted path so equality/range anchors select interned leaf identities
  (`crates/gql-planner/src/anchor.rs::extract_property_access`, shared by equality/range/
  intersection anchor selection and inline-WHERE collection), the node-pattern value lookup uses
  the same extractor (`match_plan/path/filters.rs::find_equality_value_in_where`), and Router seed
  probes resolve dotted names through the ordinary catalog/namespace path. Plan 0284 removed the
  slice-4 capability test `planner_stats.rs::active_catalog_projects_nested_leaf_as_indexed_and_
  range_indexed`; Plan 0285 re-adds it and runs its bounded contracts, including planner tests
  `planner_tests.rs::match_nested_leaf_*`, router contracts
  `seed.rs::vertex_{equality,range}_anchor_resolves_nested_leaf_property`, and the
  cross-shard GQL proof
  `crates/pocket-ic-tests/tests/router_gql_query.rs::federated_vertex_nested_leaf_index_match_equality_and_range`
  — success itself is the anchor proof because unseeded leading index scans are rejected on graph
  shards. **Slice-4 acceptance closed 2026-09-08:** re-added `planner_stats.rs::active_catalog_projects_nested_leaf_as_indexed_and_range_indexed` (Active dotted-leaf projection for equality+range with ancestor/bare-segment/missing fail-closed plus catalog `field_path` membership assertion). Bounded contracts green: gql-planner `match_nested_leaf_*` + `match_unindexed_nested_leaf_range_does_not_emit_index_scan` (4 passed), router `seed::vertex_{equality,range}_anchor_resolves_nested_leaf_property` (2 passed), `planner_stats::` suite (7 passed), PocketIC `federated_vertex_nested_leaf_index_match_equality_and_range --exact` (1 passed / 24 filtered, 12.74s), router lib clippy clean, fmt + diff-check clean, pocket-ic `router_gql_query` target compiles.
- **Severity:** P2 query capability
- **Owner:** Planner property-path resolution; decision owned by ADR 0073 slice 4
- **Observed behavior:** Nested leaf postings were maintained and backfilled end-to-end, but the
  planner did not select a nested index as an anchor (`COST BY v.stats.field` parsed;
  equality/range planning over `n.stats.score` did not bind to the interned leaf namespace).
- **Expected or needed behavior:** The planner must seed anchors and range/equality candidates
  against interned leaf identities under the same contract as every other indexed property.
- **Evidence:** `crates/router/src/planner_stats.rs` (membership projection carries
  `field_path`/`ancestor_property_id`), `design/index/property-index.md` "Nested record leaf
  domain", and the ADR 0073 implementation slices.
- **Impact:** Vertex nested-field range/equality planning cannot rely on the same index contract
  as edge inline fields until the anchor lands.
- **Next decision:** Implement ADR 0073 slice 4 (planner anchors) in a follow-up plan.

### GAP-2026-07-29-006 — Index activation convergence gate lacks production driver/E2E completion

- **Status:** Closed 2026-08-22
- **Severity:** P0 index correctness
- **Owner:** Router index catalog and backfill lifecycle
- **Observed behavior:** Router now persists hidden Preparing/Building/Sealing/Aborting lifecycle
  rows and derives planner membership only from Active rows. Graph owns exact canonical export
  scopes, graph-index owns resumable build state, the production Router cross-canister driver
  composes register/advance/seal/cleanup with unit coverage, and the CLI exact-replays one immutable
  artifact. Graph label gain/loss admission is now implemented in the Graph coordinator: exact
  `(label_id, property_id)` memberships are selected from the canonical label set, all affected
  namespaces are preflighted before mutation, Building emits the exact build envelope, and Sealing
  rejects before the canonical label change. The boundary is covered by
  `building_label_gain_emits_exact_property_insert`,
  `building_label_loss_emits_exact_property_remove_once`,
  `sealing_label_gain_rejects_before_any_mutation`,
  `sealing_label_loss_rejects_before_any_mutation`,
  `public_mutation_id_zero_label_wrappers_preserve_index_build_admission`, and
  `delete_internal_label_clear_does_not_emit_second_property_removal`. The cross-canister PocketIC
  proof is now complete (`crates/pocket-ic-tests/tests/adr0059_index_build_lifecycle.rs`, three
  scenarios green at HEAD `3c75d2fb5`; inventory precondition `router_gql_query` 24 passed / 0
  failed): (a) one single-statement `CREATE INDEX` migration converges
  Preparing→Building→Sealing→Active over a two-shard federation with pre-existing vertex and edge
  sidecar data — equality/range reads stay correct throughout (label-scan fallback pre-Active,
  index-served post-Active, non-Person decoy excluded) and the router catalog projects
  Building→Sealing→Active in order; (b) while Building, an eligibility label gain is admitted and
  its pre-existing property value converges into postings, while Sealing an eligibility label
  loss rejects before canonical mutation (E2E mirror of the six Graph fence regressions); (c)
  upgrading all five federation canisters same-wasm mid-build preserves graph-index's registered
  build watermarks and resumes to Active with complete postings. Edge-INLINE backfill completeness
  stays owned by GAP-2026-07-29-001 and is deliberately not asserted. The "retryably fail-closed"
  posture resolved as documentation-only: no code-level gate exists — `apply_schema_migration`
  composes `real_index_migration_driver()` unconditionally and caller-driven exact replay (CLI
  `MAX_INDEX_APPLY_ROUNDS_PER_MIGRATION`) is the resumption contract.
- **Expected or needed behavior:** A newly declared index must remain pending until sidecar and
  INLINE backfill has completed for every attached shard, or the query planner must explicitly treat
  it as incomplete and fail closed. During Building, every label membership transition that changes
  eligibility for an already-indexed property must durably emit the exact per-physical-index build
  operation. During Sealing, that transition must reject before canonical state changes.
  Rebuild/drop must use the same lifecycle rule.
- **Evidence:** `crates/router/src/facade/store/backfill.rs`, `crates/router/src/planner_stats.rs`,
  `crates/graph/src/facade/store/labels.rs`, `crates/graph/src/index/catalog_context.rs`, and
  `design/index/property-index.md`'s router-owned active catalog description.
- **Impact:** A query can observe an index that is structurally present but incomplete, producing
  false negatives rather than merely falling back to a slower plan.
- **Confirmed implementation boundary:** Graph label mutation uses the ordinary label-pending path
  for label postings and the same index-build admission/fence owner as property transitions for
  exact label-scoped property eligibility. ADR 0059's touched-first BuildDml admission and
  pre-mutation Sealing rejection are covered by the six Graph regressions named above.
- **Resolution:** Catalog-epoch seal behavior and per-shard watermark convergence validated in
  PocketIC, including upgrade reopen; the migration endpoint's resumption contract is documented
  rather than gated. [ADR 0059](adr/0059-create-index-migration-backfill.md) remains the source of
  truth.
- **Follow-up observation (2026-09-11):** the executor-side guard for the same transient —
  `IndexScan` / `ConditionalIndexScan` / `IndexIntersection (no unique physical index
  namespace)` (`crates/graph/src/plan/query/executor/scan/index.rs:250,336,384`) — fails
  closed when more than one physical index is active for a property. Left as an observation
  under this closed entry; promote to its own GAP only if the transient is observed live.
  No wrong results (fail-closed); no error-path test exists.

### GAP-2026-07-29-007 — Edge uniqueness and index-canister sharding remain design work

- **Status:** Planned
- **Severity:** P3 product/capacity capability
- **Owner:** Router DDL/catalog for uniqueness; graph-index deployment and routing for sharding
- **Observed behavior:** Edge property indexes provide equality/range candidate postings but do not
  enforce uniqueness. Per-graph index clusters exist, while subject/range split axes across multiple
  index canisters remain planned.
- **Expected or needed behavior:** If uniqueness is exposed in DDL, the owning mutation boundary
  must reserve/check the key atomically with the canonical write. If an index is split, Router
  routing must preserve graph, subject, label, and range completeness without changing posting
  ordering semantics.
- **Evidence:** `design/index/property-index.md` Phase E and Non-goals; [ADR 0010](adr/0010-index-sharding-extensibility.md);
  current edge posting APIs in `crates/graph-index/src/facade/store/edge_postings.rs`.
- **Impact:** Declaring these capabilities prematurely would either allow duplicate values or make
  range/equality results incomplete across index canisters.
- **Next decision:** Keep both capabilities out of the public contract until their owner boundary,
  atomicity, routing, and rebuild semantics receive separate design decisions.

### GAP-2026-08-01-001 — gleaph-gql test harness: default-feature integration build broken, all-features suite hangs, clippy test lint fails

- **Status:** Resolved 2026-08-01 (commit pending; fixes in the same patch as the resolution)
- **Severity:** P3 test-harness
- **Owner:** `gleaph-gql` crate test harness
- **Observed behavior:**
  1. `cargo test -p gleaph-gql` (default features) fails to compile the `tests/parser_tests.rs`
     integration binary: `create_graph_type_nested_record_inline_property_ast` reads
     `PropertyDef.inline`, a `#[cfg(feature = "gleaph")]` field, without gating the test
     (parser_tests.rs L1378-1402).
  2. `cargo test -p gleaph-gql --all-features` does not terminate within the observation budget
     (10 minutes); `cargo test -p gleaph-gql --features cypher --lib` alone reproduces the hang,
     so the `cypher` feature's tests are the trigger.
  3. `cargo clippy -p gleaph-gql --all-targets --all-features -- -D warnings` fails on
     `clippy::items_after_test_module` in `type_check/phase_b.rs` (a `mod parameter_inference_tests`
     placed mid-file, followed by `pub fn infer_linear_query_binding_kinds`).
- **Root cause of the hang (2):** the deeply nested `VALUE { RETURN ...` input recursed through
  `Parser::recurse` until `MAX_RECURSION_DEPTH` (64), but the `cypher` build's enlarged
  expression/parse frames exhausted the native stack first — `EXC_BAD_ACCESS` (stack overflow) on
  the test worker thread, which the harness then waited on forever. Root cause confirmed by
  lowering the limit (guard fires, test passes) and by lldb (`EXC_BAD_ACCESS` at a stack address).
- **Resolution:**
  1. Gate `create_graph_type_nested_record_inline_property_ast` behind
     `#[cfg(feature = "gleaph")]` (the sibling `create_graph_type_edge_inline_property_ast` was
     already gated).
  2. Lower `Parser::MAX_RECURSION_DEPTH` from 64 to 32 with a doc note: the guard must fire at
     roughly half the heaviest feature build's stack cost; legitimate queries nest far below the
     bound. `cargo test -p gleaph-gql --all-features` now completes (all binaries pass, including
     the cypher-gated 746-test lib suite).
  3. Move `mod parameter_inference_tests` to the end of `type_check/phase_b.rs`;
     `clippy --all-targets --all-features -- -D warnings` now passes.
- **Evidence:** the three commands above before and after the fixes; `crates/gql/tests/parser_tests.rs`
  L1378-1402; `crates/gql/src/type_check/phase_b.rs`; `crates/gql/src/parser/helpers.rs`
  `MAX_RECURSION_DEPTH`; lldb backtrace showing `EXC_BAD_ACCESS (code=2)` at a stack address.
- **Impact (resolved):** default-feature test builds compile; the all-features suite terminates;
  test-target clippy gates with `-D warnings`.
- **Regression coverage:** the existing `deeply_nested_subqueries_are_rejected_not_overflowing`,
  `deeply_nested_parentheses_are_rejected_not_overflowing`, and
  `deeply_nested_not_chain_is_rejected_not_overflowing` tests run under both default and
  all-features builds and assert the depth error path.

### GAP-2026-08-25-001 — Pre-existing gleaph-gql parser failures: dashed prepared-query names fail `expect_ident` in EXECUTE publication forms

- **Status:** Resolved 2026-09-08 (same uncommitted fix as GAP-2026-08-24-005; discovered 2026-08-25 during plan 0304 / ADR 0080 implementation; not caused by that slice — reproduced on a pristine HEAD worktree).
- **Owner:** `crates/gql` tokenizer/identifier grammar and prepared-query naming contract.
- **Observed behavior:** `cargo test -p gleaph-gql --features gleaph --lib` fails
  `parse_grant_execute_on_prepared_query_to_public_and_principal` and
  `parse_revoke_execute_on_prepared_query_mirrors_grant`: parsing
  `GRANT EXECUTE ON PREPARED QUERY find-users TO PUBLIC` errors with
  `expected 'TO', got '-'`, i.e. the lexer splits dashed identifiers, so prepared names
  containing `-` cannot be granted/revoked through the publication form. All other grant
  tests pass; ADR 0080's new metadata forms are unaffected.
- **Impact:** Prepared operations with dashed canonical names cannot be published or revoked
  via GQL; the two failing tests document the intended contract (`find-users`), so the suite
  is red independent of any in-flight slice. Blocks a green `gleaph-gql` test run for every
  concurrent session.
- **Needed behavior:** Met 2026-09-08 — the `PREPARED QUERY` positions accept bare ADR 0061 kebab-case
  (`expect_prepared_query_name`) alongside the quoted form; no registration-side charset change.
- **Next decision:** None — both previously-red tests pass unmodified plus the new
  `parse_prepared_query_name_accepts_bare_kebab_digits_and_quoted` regression; same validation as
  GAP-2026-08-24-005 (567 gleaph / 524 default passed, fmt + diff-check clean).

### GAP-2026-08-25-002 — Catalog steady-state schema resolve rebuilds the full property schema on every call (heap-cache candidate)

- **Status:** Open — P3 optimization candidate recorded 2026-08-25 from the canbench
  re-baseline attribution review; catalog owner decision pending (not a defect: current
  behavior is correct, only redundant work)
- **Owner:** `crates/graph-catalog` (`GraphCatalog::try_property_schema_for_graph_id`,
  `lib.rs` ~L306) + callers that resolve per planning/validation
  (`router/src/facade/stable/graph_type_catalog.rs:352`)
- **Observed behavior:** Every call takes the `binding_cache` hit path but then performs
  `definition.clone()` plus `GraphTypePropertySchema::try_from_definition(&def)` — a full
  schema rebuild — before returning. Introduced by `8709ffe8c` (2026-07-30 heap-cache
  refactor); definitions themselves grew under `5396d15f3` (2026-07-29 inline edge
  properties). Net effect: `bench_catalog_resolve_inline_medium` roughly doubled against its
  stale artifact, 120,465 → 229,580 IC (+90.6%, absorbed into re-baseline `53e01eb26`).
- **Evidence:**
  - Profile ruled out experimentally: HEAD code with the pre-`d9a4f4b3e` codegen settings
    (`lto="off"`, `codegen-units=16`) in an isolated worktree measures 250.92K IC — +9.3%
    over the landed fat-LTO 229.58K. Fat LTO improves this bench; neither lto/cgu nor any
    2026-08-24/25 index slice contributes to the delta.
  - Artifact history: the prior value 120,465 IC was recorded at `72516cf84` (2026-06-19)
    and was never refreshed through the 07-29 → 08-03 graph-catalog evolution window
    (`5396d15f3`, `8709ffe8c`, `cee561df3`, `a7a361c66`).
  - Measurement hygiene for reproduction: isolated worktree checkout of HEAD; single-bench
    pattern run (`canbench resolve_inline` from `crates/graph-catalog`); no contact with
    other panes' uncommitted trees or shared-target contention.
- **Expected or needed behavior:** Cache the resolved `GraphTypePropertySchema` beside the
  definition/binding caches so steady-state resolves are O(1) map hits; invalidate on the
  same boundaries that evict the definition caches (`OR REPLACE`, migration apply,
  upgrade rebuild via `rebuild_caches_after_upgrade`).
- **Impact:** Steady-state read-path cost proportional to definition size paid on every
  planning/validation resolve; grows as graph types get richer (records, inline edges).

### GAP-2026-08-26-002 — No privileged authorization diagnosis surface; uniform Forbidden leaves "why denied" to trial-and-error

- **Status:** Resolved (2026-08-26). `EXPLAIN AUTHORIZATION FOR PREPARED QUERY` shipped per
  [ADR 0084](adr/0084-explain-authorization-diagnosis.md) (status: implemented): statement
  grammar behind the `gleaph` gate with zero-flag classification, Router report mode reusing
  `extract_live`/`requirements_cover` (stored-primary + live-drift fallback), visibility-only
  authority gates with indistinguishable `NotFound`, per-mode redaction, and the
  `adr0084_explain_authorization` PocketIC suite including an invariant-1 execution-bytes
  regression guard. Fixing commit: `cc80c5327` (suite) with grammar `7d6cf5083`, report mode
  `1ffce1888`.
- **Owner:** Router authorization seam (`crates/router/src/authz.rs`
  `enforce_data_plane_authorization`, requirement walker `walk_ops`/`requirements_cover`)
  together with the grammar surface in `crates/gql`; introspection precedent in
  `crates/router/src/gql_grants.rs` (`list_graph_grants`).
- **Observed behavior:** Every uncovered demand fails with the uniform non-disclosing
  `Forbidden` that never names the missing privilege or resource ([ADR 0074] §4). The only
  diagnosis paths today are reading code/tests plus owner-side `list_graph_grants`
  introspection; the question "why can't caller X run program P on graph G" has no direct
  answer anywhere in the system.
- **Expected or needed behavior:** A privileged-only diagnostic statement (working name
  `EXPLAIN AUTHORIZATION`, reserved by [ADR 0074] §4 and its Consequences trade-off) that
  renders one program's requirement set joined with coverage evaluation: per requirement,
  which effective row/root satisfies it or that it is uncovered — emitted only within the
  asker's visibility scope.
- **Evidence:** [ADR 0074] §4 ("diagnosis is a job for a future privileged-only
  `EXPLAIN AUTHORIZATION`") and §Consequences ("support/debugging of 'cannot see my own
  data' incidents requires either temporary explicit grants or the future
  `EXPLAIN AUTHORIZATION`"); reusable machinery: `RequirementSet` extraction and
  `requirements_cover` already exist as enforcement internals.
- **Impact:** Operability only — non-disclosure itself is intact by design. Developers and
  operators debug deny-by-default surprises via trial-and-error with temporary grants;
  shared-graph incident support stays script-level as multi-principal usage grows.
- **Next decision:** resolved by [ADR 0084] §1/§5: (a) self mode plus owner mode (`BY`)
  restricted to tenancy of every touched graph; (b) self-diagnosis may name uncovered
  resources on graphs whose visibility the asker passed — invisible graphs abort before any
  rendering; (c) full coverage detail with source-class redaction in self mode and full row
  identities in owner mode.

### GAP-2026-08-26-003 — main HEAD does not compile standalone: committed provisioning code imports `TextIndexId` defined only in uncommitted graph-kernel edits (half-commit recurrence)

- **Status:** Resolved (2026-08-26). Root cause: the text-index track landed referencing
  commits before their defining edits; fixed by landing the definitions as a
  single-purpose commit.
- **Resolution:** definitions landed in `31d716246`
  (`crates/graph-kernel/src/federation/shard_id.rs` +87 incl. `TextIndexId`,
  `federation.rs` re-export). Owner-side verification: check/clippy `-D warnings` /
  `test -p gleaph-graph-kernel --lib` 180/180. Coordinator independent verification:
  fresh detached worktree at `31d716246`, shared target dir,
  `cargo check -p gleaph-graph-kernel --lib` → `Finished dev profile … 15.41s`, zero
  errors/warnings — HEAD builds standalone again.
- **Owner:** the text-index/canister track session holding the dirty defining edits
  (`crates/graph-kernel/src/federation.rs`, `crates/graph-kernel/src/federation/shard_id.rs`
  — `TextIndexId` at `shard_id.rs:246` with its `federation.rs:54` re-export). The
  referencing side landed first: committed
  `crates/graph-kernel/src/provisioning/{mod.rs,tests.rs}` import the symbol via
  `crate::federation::{… TextIndexId …}`.
- **Observed behavior:** a clean detached worktree at HEAD fails
  `cargo check -p gleaph-graph-kernel --lib`:
  `error[E0432]: unresolved import crate::federation::TextIndexId` at
  `provisioning/mod.rs:12:50` ("no TextIndexId in federation"). Main-tree
  `gleaph-router --lib` test targets additionally fail on text-index/index-migration WIP
  per the w1:p8 report.
- **Expected or needed behavior:** every commit leaves main buildable standalone: a fresh
  clone/worktree compiles kernel lib and router lib tests without any other session's
  uncommitted state.
- **Evidence:** coordinator reproduction (2026-08-26): `git worktree add … --detach HEAD;
  CARGO_TARGET_DIR=<shared> cargo check -p gleaph-graph-kernel --lib` → E0432 above;
  w1:p8 working log; `plans/0307-group-element-id.md` Validation Transcript. Note HEAD
  advanced during verification (`35c18f85c` → `e24bb57c5` via text-track commits
  `c97dc08c5`, `23b23249c`) and the breakage reproduces on current HEAD — live incident,
  not history.
- **Impact:** bisect false positives across the whole workspace; fresh clone/worktree
  cannot compile kernel or run router tests; any per-commit validation gate is blind.
- **Next decision:** the owning text-track pane lands its graph-kernel defining edits
  immediately (simultaneous-landing resolution), or a coordinator-approved minimal
  extraction commit moves the definitions in; afterwards re-run the clean-worktree check
  to close this entry.

### GAP-2026-08-26-004 — ANY-SHORTEST-style prepared ops are undispatchable: label-only multi-variable anchor prefixes dispatch without seeds and the federated wire guard rejects them

- **Status:** Resolved (2026-08-26). The original index-servability framing (plan 0309
  draft, briefs #5/#6) is superseded by the root cause below; both halves are fixed and
  pinned.
- **Owner:** Router seeding (`crates/router/src/seed.rs` `SeedAnchorSet::from_plans`, the
  ADR 0046 Phase 1 label-only fallback) against the graph wire guard
  (`crates/graph/src/plan_wire_guard.rs::ensure_federated_seeds_for_index_anchors`); the
  apply-time index-anchor half lives in `crates/router/src/prepared.rs`
  (`validate_leading_index_anchor_servability`). gql/gql-planner untouched throughout.
- **Observed behavior:** plans planned with ≥2 leading labeled endpoint scans —
  structurally every `ANY SHORTEST` between two labeled endpoints — were rejected at run
  time for every caller on federation-configured shards with
  `unsupported plan query operator: IndexScan(no index client)`: the label-only
  multi-variable fallback dispatched them seed-less while the guard requires effective
  seeds exactly for labeled-scan-led plans. First recorded as the knowledge-demo
  `shortest-path` failure (0296 bring-up evidence; 2026-08-24 matrix row), mis-framed
  through plans 0309/0310 as an index-catalog mismatch until the PocketIC probe in 0310
  falsified that reading.
- **Expected or needed behavior:** label-only multi-variable prefixes must be seedable on
  the topology that can serve them (standalone), stay fail-closed where they cannot
  (multi-shard), and unservable index anchors must fail at apply time instead of run time.
- **Resolution:** (a) `SeedAnchorSet::from_plans` keeps pure label-only multi-variable
  prefixes on single-live-shard topologies; anchors resolve through the existing
  label-posting lookup (`lookup_label`, ADR 0004) and the sole shard executes remaining
  labeled scans locally. Multi-shard keeps fail-closed `None` pending the
  federated-traversal restoration ADR ([federation-target.md](sharding/federation-target.md)).
  (b) prepared apply rejects leading index anchors without a unique Active posting
  namespace (migration-000002 hazard class). Fixing commits: standalone seeding + E2E
  (`feat(router): seed label-only anchors in standalone topology`) and the apply-time
  guard (`feat(router): guard prepared apply against unservable index anchors`),
  2026-08-26.
- **Pinned contracts:** `with_hits_encodes_seed_bindings_blob_for_single_shard_dispatch`,
  `from_plans_keeps_multi_variable_label_only_prefix_on_standalone`,
  `from_plans_multi_shard_still_drops_label_only_multi_variable_prefix`,
  `wave_4_multi_anchor_seed_extracts_two_variables` (router);
  `shortest_path_demo_plan_leading_ops_diagnostic_dump` (router, plan 0309);
  `index_scan_requires_index_client` (graph); PocketIC
  `knowledge_demo_shortest_path_flow.rs` (default-deny then non-owner row-content
  execution after publication).
- **Next decision:** none open. Multi-shard label-only-multi seeding returns with the
  federated-traversal restoration ADR.

### GAP-2026-08-28-002 — HEAD does not compile `gleaph-cli` standalone: committed `prepared.rs` matches `ReturnBody::NoBindings` (a `cfg(cypher)` variant) while the `cli/Cargo.toml` cypher-feature line is uncommitted (half-commit recurrence)

- **Status:** Open (2026-08-28) — same class as GAP-2026-08-26-003, different crate pair.
- **Severity:** P1 implementation-integrity recurrence
- **Owner:** cli stream (the pane holding the uncommitted `crates/cli/Cargo.toml` +
  `crates/cli/src/identity.rs` dependency-migration work)
- **Observed behavior:** A clean worktree at HEAD (2026-08-28, post `00c94208e`) fails to
  compile `gleaph-cli`: `crates/cli/src/prepared.rs:1077` matches
  `ReturnBody::NoBindings`, which exists only under gleaph-gql's `cfg(feature = "cypher")`,
  and the committed `cli/Cargo.toml` declares `gleaph-gql` features `["gleaph", "serde"]`.
  The main working tree compiles only because the stream's uncommitted Cargo.toml WIP adds
  the `cypher` feature (plus a `gleaph-gql-planner` cypher feature and the k256/rand
  dependency migration). Isolated-worktree probes (e.g. `git worktree add` for a baseline
  build) therefore cannot build the cli at HEAD.
- **Expected or needed behavior:** the cli stream lands its Cargo.toml feature lines in the
  same commit window as any prepared.rs AST matching; until then, per-commit validation
  gates on clean checkouts stay blind for the cli crate.
- **Next decision:** the owning stream commits the feature lines (the workspace comment in
  the WIP already explains why `cypher` must stay enabled unconditionally); afterwards a
  clean-worktree build check closes this entry.

### GAP-2026-08-26-005 — Real Provision upgrades wipe the bootstrap authority and active release: eager `StableCell::new` constructors clobber durable cells on every process restart

- **Status:** Resolved (2026-08-26, slice "Provision upgrade durability"). The two eager
  constructors now use the read-or-create form `StableCell::init(memory, default)`
  (`crates/provision/src/stable/bootstrap_auth.rs` `init_bootstrap_auth_cell`, MemoryId 4;
  `crates/provision/src/stable/memory.rs` `init_active_release`, MemoryId 10), matching the safe
  precedent of `init_artifact_storage_id`. `init` decodes an existing durable cell and only writes
  the default when the region is empty (fresh install), so a process restart no longer rewrites
  state before any handler runs. No other store logic changed; `post_upgrade` remains a no-op.
- **Owner:** `crates/provision/src/stable/bootstrap_auth.rs:26-38` (`BOOTSTRAP_AUTH`
  thread-local, MemoryId 4) and `crates/provision/src/stable/memory.rs:111-116`
  (`init_active_release`, MemoryId 10).
- **Observed behavior:** after a real chunked upgrade of a deployed Provision (PocketIC
  replica ingress through the `gleaph-operator` transport), governance readback that
  succeeded pre-upgrade returns `Unauthorized` because
  `ProvisionBootstrapAuthStore::get_authority()` reads `None`. Stable memory is preserved
  byte-for-byte across the upgrade (25,231,360 bytes unchanged, measured via canister
  status), so the loss happens in the store layer, not the replica.
- **Root cause:** `StableCell::new(memory, value)` writes on construction
  (`ic-stable-structures` cell.rs:174-176). Both cells are built inside eager
  thread_local initializers with `None` defaults, so every new process (install or
  upgrade) rewrites the durable cells empty before any handler runs. The same module set
  already uses the safe read-or-create form elsewhere
  (`init_artifact_storage_id`, `stable/memory.rs:100-105`), and the hazard is described in
  `bootstrap_auth.rs:58-60`'s own comment. The unit test
  `bootstrap_authority_singleton_survives_upgrade` passes because it never restarts the
  process, so the writing constructors never run again.
- **Expected or needed behavior:** an upgraded Provision must retain its seeded authority
  and active release without re-running `init`; a fresh install must still start empty.
  Blast radius if unfixed: a post-upgrade Provision rejects every catalog write as
  unauthorized and aborts every issuance at the no-active-release guard while remaining
  live.
- **Pinned tests:** unit — `stable::store::tests::bootstrap_auth_constructor_rerun_preserves_seeded_authority`
  and `active_release_constructor_rerun_preserves_active_release` rebuild each cell through its
  constructor over the same MemoryId (process-restart simulation via new test-only reopen helpers);
  both were red-proofed against the write-on-build form. E2E —
  `adr0087_bootstrap_tier::bootstrap_tier_deploys_and_upgrades_provision_end_to_end` now publishes
  five placeholder artifacts plus a release before upgrading and asserts post-upgrade: governance
  `artifact_audit_history` authorized with the pre-upgrade successful activation row intact,
  anonymous still unauthorized, and `release_get_active` equal to the pre-upgrade release id.

### GAP-2026-08-29-001 — RESOLVED: `ELEMENT_ID(e)` / `GraphPathEdgeId` bytes collide across edge labels under the same owner (router/storage edge-identity bug)

- **Status:** Resolved (plan 0312 / [ADR 0090](adr/0090-edge-element-id-label-attribution.md),
  2026-08-29). Identity fix; no data loss.
- **Severity:** P2 explorer display / cross-caller identity collision; not data loss.
- **Owner:** `crates/graph-kernel/src/federation/{global_edge_id,encoded}.rs` and
  `crates/graph-kernel/src/path.rs` (canonical identity + Feistel encoder/decoder);
  `crates/graph/src/plan/query/executor/path/materialize.rs` and `eval.rs` (call sites).
- **Observed behavior:** Knowledge demo seed has 37 unique edges. `MATCH (a)-[e:L]->(b)
  RETURN element_id(e)` returned 25 unique element_ids, not 37. The native graph explorer
  dedups by element_id and rendered 25 edges, missing 12. No data loss: storage is
  correct; the wire id is not. The collision is masked in graphs where every owner has at
  most one label (e.g. simple seeds), so the bug only surfaced against a multi-label
  source vertex (the demo's alice has `AUTHORED_BY slot0`, `BELONGS_TO slot0`, `OWNS slot0`,
  `ROUTED_VIA slot0` — all four encode to the same `(shard, alice, 0)`).
- **Expected or needed behavior:** `ELEMENT_ID(e)` / `PathElement::Edge` bytes must be
  unique per edge under the per-graph `ElementIdEncodingKey`. The CLI stderr warning
  (introduced 2026-08-25) already states that edge ids are unstable physical handles
  unstable across compaction; this bug is the same kind of instability in a different
  dimension (cross-label collision at the same compaction), so the warning covers the
  fix surface; no new warning is needed.
- **Resolution:** `GlobalEdgeId` extended from `(shard, owner, slot)` to
  `(shard, owner, label, slot)` with the label widened from `u16` to `u32` for 4-byte
  alignment and bijection. `EncodedEdgeId` wire layout grew from 12 to 16 bytes. The
  Feistel encoder/decoder is updated to a 16-byte canonical form (head: 8-byte Feistel-4,
  tail: 8-byte XOR under a key-derived mask that mixes the encoded head and the per-graph
  key tail). `GraphPathEdgeId::new` is now 5-arg and takes the label; the 3-arg form is
  removed. Caller sites in `materialize.rs` (encoder + path materialization) and `eval.rs`
  (singleton + `EdgeGroup` + host test helper) read the label from `EdgeHandle.label_id`
  (already populated at insert time in `edge_insert.rs:203,331`). A new pure helper
  `catalog_label_from_lara` translates the LARA bucket label to the catalog label space
  for the wire encoder. The remote-edge default-label `0` matches the existing
  label-free reverse-edge pattern.
- **Pinned tests:** host — `gleaph-graph` lib tests
  `edge_element_id_distinguishes_per_label_same_owner_slot`,
  `edge_element_id_distinguishes_within_same_label`,
  `edge_element_id_distinguishes_per_owner_same_label_slot`,
  `edge_element_id_encoded_length_is_16_bytes`,
  `edge_element_id_encoded_differs_from_canonical`,
  `edge_element_id_label_zero_is_distinct_from_label_nonzero`;
  `gleaph-graph-kernel` lib tests `global_edge_id::round_trip`,
  `global_edge_id::label_widening_is_lossless_for_catalog_range`,
  `encoded::edge_encode_decode_roundtrip`,
  `encoded::edge_tail_changes_when_head_unchanged`,
  `path::edge_path_id_roundtrips`,
  `path::edge_path_id_distinguishes_per_label`. The plan 0307 group-binding
  executor tests (`element_id_on_edge_group_lists_hop_ids_in_traversal_order`,
  `element_id_on_vertex_group_preserves_order_and_empty_group_yields_empty_list`,
  `element_id_on_path_group_stays_fail_closed_with_guidance`) still pass; the per-hop
  list form inherits the fix automatically because the singleton and list arms share
  the encoder.
- **Evidence:** detection run during knowledge demo validation against the native graph
  explorer (2026-08-29); root cause read of
  `crates/graph/src/plan/query/executor/path/materialize.rs:116,145`,
  `crates/graph/src/plan/query/executor/eval.rs:1010,1024,2214`,
  `crates/graph-kernel/src/federation/global_edge_id.rs`,
  `crates/graph-kernel/src/federation/encoded.rs`. Post-fix validation: `cargo test -p
  gleaph-graph --lib` (1124 passed, 0 failed), `cargo test -p gleaph-graph-kernel --lib`
  (184 passed, 0 failed), `cargo clippy -p gleaph-graph --lib --all-targets -- -D
  warnings` and the same for `gleaph-graph-kernel` (exit 0).
- **Related contracts:** [ADR 0090](adr/0090-edge-element-id-label-attribution.md) (new);
  [ADR 0005](adr/0005-vertex-identity.md) (amended — 12 → 16 bytes for `GlobalEdgeId` /
  `EncodedEdgeId`, 12-byte form marked SUPERSEDED); [ADR 0006](adr/0006-pre-federation-foundation.md)
  § 4 (amended); [ADR 0019](adr/0019-graph-local-shard-id-and-index-clusters.md) § 3
  (amended); [glossary](glossary.md) rows for `Global edge id` and `Encoded edge id`
  (amended); [group-variables](execution/group-variables.md) ELEMENT_ID rules row
  (amended with cross-link to ADR 0090). Distinct from GAP-2026-08-26-001 (plan 0307,
  group-binding executor implementation): the 0307 fix made `ELEMENT_ID(e)` over a
  quantified-path group variable execute, but the underlying identity collision was not
  in scope and is resolved here.

### GAP-2026-08-29-007 — RESOLVED: Canonical mutation segment enforcement was not path-independent

- **Status:** Resolved (2026-08-29; implementation slice landed; status updated alongside the
  slice commit)
- **Severity:** P1 latent invariant enforcement gap
- **Owner:** Graph (`crates/graph/src/gql_run.rs::apply_canonical_mutation_segment`)
- **Observed behavior:** The canonical mutation segment relied on two narrow structural
  properties to keep inter-canister calls out of the segment: (a) it takes no
  `PropertyIndexLookup` handle, and (b) the only `CALL` procedures it executes are
  synchronous. These are not path-independent — a second inter-canister chokepoint (peer-shard
  client, subgraph client, Phase 6 cross-shard coordination) added inside the segment would
  silently extend the critical section across a commit point. ADR 0029 §8 explicitly named this
  as a deferred guard.
- **Expected or needed behavior:** A new inter-canister chokepoint added inside the segment must
  fail loudly at its acquisition boundary. The canonical segment must trap and roll back the
  whole message (Property 5) when a chokepoint is reached from inside the segment.
- **Resolution (design):** Adopted [`CanonicalSegmentGuard`](../adr/0091-path-independent-canonical-segment-guard.md)
  (RAII thread-local depth counter + Drop balance check + chokepoint-side
  `assert_no_canonical_segment(...)` trap). The guard is entered at the first statement of
  `apply_canonical_mutation_segment`; the existing `ExecuteCtx::new` (referred to as
  `ExecutorContext::new` in ADR 0091) chokepoint for `PropertyIndexLookup` acquisition calls
  `assert_no_canonical_segment("executor_context_new")`. The original "no `PropertyIndexLookup`
  handle" / "synchronous `CALL`" guarantees are retained as defense-in-depth.
- **Resolution (implementation):** Landed. New module
  `crates/graph/src/facade/canonical_segment.rs` (thread-local `Cell<u32>` depth counter,
  `CanonicalSegmentGuard::enter()`, `Drop`-balance trap, `assert_no_canonical_segment(...)`,
  `canonical_segment_depth()` accessor). One-line `enter()` at the first statement of
  `apply_canonical_mutation_segment` (`crates/graph/src/gql_run.rs`). One-line
  `assert_no_canonical_segment("executor_context_new")` at the top of
  `ExecuteCtx::new` (`crates/graph/src/plan/query/executor/context.rs`). Re-exports in
  `crates/graph/src/facade.rs`. Test-only seam (`pocket-ic-e2e` only) in `canonical_segment.rs`
  plus a feature-gated `GLEAPH.E2E_SIMULATE_INTER_CANISTER_CALL` CALL procedure dispatch in
  `crates/graph/src/plan/mutation/gleaph_finalize.rs`.
- **Evidence (post-implementation):** New host unit tests in
  `crates/graph/src/facade/canonical_segment.rs` cover depth-balance, outside-guard pass,
  inside-guard trap, nested-enter depth, and a contrived `Drop`-balance wrong-impl test
  (`double_drop_traps_when_depth_already_zero`). New PocketIC test file
  `crates/pocket-ic-tests/tests/adr0091_path_independent_guard.rs` covers whole-message
  rollback when the trap fires, the happy path, and the read-side chokepoint pass. Existing
  tests are unaffected (`canonical_segment_trap_rolls_back_whole_message` passes without
  modification because Property 5 guarantees message-wide rollback on trap).
- **Impact:** The canonical segment atomicity boundary is now formally defensible from any new
  inter-canister chokepoint by a single line of defense at the chokepoint boundary, ahead of
  the second-chokepoint trigger named in ADR 0029 §8. ADR 0091 Decision 4 (PR-review
  checklist) remains the only manual guard for future drift; the runtime guard is the
  single-line `assert_no_canonical_segment(...)` at the chokepoint acquisition boundary.
- **Related contracts:** [ADR 0091](../adr/0091-path-independent-canonical-segment-guard.md)
  (new); [ADR 0029 §8](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md)
  (referenced from ADR 0091); [ACID roadmap Phase 1](../architecture/acid-roadmap.md) exit
  criteria (updated to cite ADR 0091); [`MutationToken` /
  `ReadMode::AtLeast` doc comments](../../crates/graph-kernel/src/plan_exec.rs) (clarified
  scope; see `gleaph-mvcc-and-ic-atomicity.md` and `gleaph-mvcc-design-review.md` for the
  underlying review).

## Review cadence

- The primary agent checks this ledger before final approval of a meaningful slice.
- A slice that resolves an entry updates its status in the same commit as the fix.
- Open entries should be converted to an implementation plan when their prerequisite arrives or
  when they become the highest-impact blocker.
- If an entry duplicates an existing roadmap or ADR item, replace its detailed proposal with a link
  to that authoritative contract rather than maintaining both descriptions.

### GAP-2026-09-02-003 — RESOLVED: LPB-in-tree split-brain layout (Plan 0326 REWORK)

- **Status:** Resolved (2026-09-02, Plan 0326 REWORK).
- **Severity:** P0 split-brain — read path and write path disagreed on
  the property root location. Production correctness + benchmark
  measurement validity.
- **Owner:** `promote_bypass_to_tree_mode` (promote.rs:300-465), 
  `tree_mode_property_leaf_append` (tree_write.rs:421-567), depth-1
  tail append (tree_write.rs:611-794), depth-2 tail append
  (tree_write.rs:856-1196), `tree_mode_demote_to_slab` Phase 5e
  (tree_write.rs:2260-2300), `bucket_span_region_len` and
  `property_root_region_len` (compact.rs:234-300).
- **Observed behavior:** the Plan 0326 first cycle shipped with a
  read path that derived the property root as `edge_start +
  bucket_span_region_len(bucket)` (contiguous layout, ADR §2) and a
  write path that allocated the property root as a separate LEG
  span, recorded in the descriptor's `inline_property_bytes_offset`
  field. The two layouts agreed only when the allocator happened to
  place the property root span adjacent to the edge root span.
  Synthetic bench seeds (in `seed_w32_sweep_bucket`) constructed
  both spans manually, so the split was hidden. The canbench
  reported 540 ins for both new benches (4K and 65K), which was
  actually a no-op: the bench seed left the vertex's `bucket_count`
  at 0, so `find_bucket` returned `BucketSearch::Missing` and the
  visit callback was never invoked. The unit test
  `lpb_in_tree_demote_round_trip_at_w_4_stored_4096` passed only
  because the synthetic seed's property root span happened to
  land at `edge_start + edge_root_len` by allocator luck.
- **Why it matters:** the actual production deployment would have
  hit the read-after-write inconsistency on the first w>0 tree-mode
  bucket promote: property reads via the production visit path
  would return bytes from the wrong LPB blocks. The bench numbers
  reported in the yml were 0% noise; downstream regression budgets
  (Plan 0324's Gate 3) compared against these bogus values.
- **Fix (REWORK)**: single combined LEG span `[edge root | property root]`
  (gap 0, ADR §2). All write sites (promote Phase 2, depth-1 tail
  append, depth-2 tail append, depth-2 new-interior-mint) do a
  single `allocate_span_avoiding(combined_len, avoid=old_combined_span)`
  and copy both halves. The descriptor's `inline_property_bytes_offset`
  is set to 0 in tree mode (unused; the property root location is
  derived from `edge_start + bucket_span_region_len(bucket)`). The
  read path is unchanged (was already correct per ADR §2). The
  demote's Phase 5e reads the property root from `edge_start +
  bucket_span_region_len(bucket)` and releases the property half of
  the combined span (the edge half is covered by the edge-half
  release in Phase 5c). The compact helper `property_root_region_len`
  returns `ceil(S / K)` (no tail buffer; the legacy `+ 1` was a
  writing-side convenience that never affected the read path). A new
  compact helper `combined_span_region_len` returns
  `edge_root_len + property_root_len` for the compaction rewrite
  path's per-vertex span intervals.
- **F-2 cap guard**: a new `LabeledOperationError::PropertyTreeRootCapacityReached`
  variant fires when the property root would exceed `R_MAX = 1024`.
  Property-tree deepen (with `BlockKind::InlinePropertyInterior`) is
  recorded as a follow-up slice per ADR 0088 §7.
- **F-3 bench honesty**: `tcsr_4096_property_read_w32` and
  `tcsr_65536_property_read_w32` now use the production insert path
  (`insert_edge_skip_leaf_cascade`) with the bucket schema pre-declared
  via `ensure_label_bucket_inline_property_byte_width`. The two bogus
  540-ins entries (which measured nothing) are removed from
  `canbench_results.yml`. New honest measurements: 4K = 3,160,000 ins,
  65K = 5,630,000 ins. `--persist` is forbidden going forward
  (single-bench `canbench <name>` runs only; yml updates are manual).
- **F-4 w=0 hot path**: the depth-2 interior-row-append path for w=0
  no longer reallocates the edge root (the edge root doesn't grow
  on interior-row append; only on new-interior-mint does the edge
  root grow). The depth-1 / depth-2 combined-span allocation uses
  `allocate_span` (no avoid) for the w=0 path to avoid the avoid-scan
  cost. The +24.81% regression on `tcsr_1048576_deepen_beyond_r_max`
  is fully restored to 0%.
- **Tests**: 630 lib tests pass (+4 from 627: 
  `lpb_in_tree_rework_combined_span_round_trip` exercises the full
  promote → read round-trip via the production path; 
  `lpb_in_tree_rework_f2_cap_guard` exercises the synthetic
  R_MAX-1 setup; plus 2 from the foundation). 583 canbench tests pass.
  clippy -D warnings clean. fmt clean. validator final phase: PASS.
- **F-5 wasm chars**: canbench wasm builds and runs; exported name
  count within the 20,000 char limit. Baseline 15,684 → 15,684 (no
  new bench surface). 159 → 161 entries (+2 honest benches; the
  partial slice's bogus 540-ins entries were removed).

### GAP-2026-09-07-001 — RESOLVED: Delete-only workloads accumulate tombstones indefinitely on Insertion-policy slab buckets and default-label bypass rows

- **Status:** Resolved (2026-09-07, Plan 0339 slab side + Plan 0341 bypass side). The
  slab-bucket half is closed by the Plan 0339 remove-side hysteresis admission trigger
  (`DeferredBidirectionalLabeledLaraGraph::maybe_enqueue_remove_side_compaction`, deferred.rs):
  after a successful removal, if post-removal tombstones exceed half the stored width on an
  Insertion-policy slab bucket, the existing `CompactVertexEdgeSpanV1` work item is enqueued and
  the same message's drain left-packs the span. Regression tests: `remove_side_compaction_*`
  (deferred.rs) and `delete_past_hysteresis_enqueues_span_compaction_and_left_packs` /
  `delete_below_hysteresis_leaves_tombstones_for_append` (facade/store/tests.rs). The bypass-row
  half is closed by Plan 0341: the bypass-origin geometry contract
  (`design/storage/lara.md`) plus the new `compact_default_bypass_row` left-pack step
  (compact.rs), the `CompactDefaultBypassRowV1` work item, and the remove-side admission wired
  into the deferred wrapper's bypass remove paths (deferred.rs). Regression tests:
  `bypass_tombstones_are_left_packed_at_hysteresis` / `bypass_compaction_below_hysteresis_no_fire`
  (deferred.rs) and `bypass_compact_step_*` (bypass.rs). The slab trigger + this step together
  close the full gap.
- **Severity:** P2 maintenance-coverage gap (slow scan/memory degradation, no correctness risk;
  extent-bounded per row)
- **Owner:** `ic-stable-lara` deferred maintenance admission (`deferred.rs`) + bypass row
  compaction coverage (`compact.rs` / `bypass.rs`)
- **Observed behavior:** The insert path admits post-write maintenance — dense-leaf compaction
  when `labeled_leaf_segment_is_dense(src)` and inline-property-bytes slab compaction
  (`deferred.rs:2172-2194`, admission failures trap). The remove path admits nothing: production
  `remove.rs` contains no enqueue/mark_compact/maintenance call, and the sole remove-side trigger
  is the tree-mode demote check (`degree <= T_DEMOTE`, Plan 0319, best-effort inline). A
  delete-only workload therefore accumulates tombstones indefinitely on Insertion-policy slab
  buckets and default-label bypass rows until a later insert touches the same vertex.
  `Unordered` labels are excluded from the gap by policy: they self-heal on insert (in-slab
  tombstone reuse before append, `insert.rs:420`) and swap-compact (ADR 0052 §7).
- **Plan 0339 survey findings (2026-09-07, no code changed):**
  1. **Insertion-policy slab buckets** are closable with an existing work item:
     `CompactVertexEdgeSpanV1` (enqueued via `mark_compact_vertex_edge_span`, deferred.rs:1542)
     drains through `compact_vertex_edge_span_one_step` (compact.rs:1923), which left-packs a
     tombstoned bucket span. The planned trigger design (hysteresis
     `tombstones = stored − degree ≥ stored/2` post-removal, Insertion-only policy gate via
     `maintenance_policy_for_label`, tree/Unordered exclusions, trap-on-admission-failure per the
     insert-side convention, O(1) descriptor-arithmetic gate) is implementable as written for
     this regime.
  2. **Default-label bypass rows are NOT closable today**: every existing edge-span compaction
     path short-circuits on `is_default_edge_labeled()` (`compact_vertex_edge_span_one_step`
     compact.rs:1939, `rewrite_vertex_edge_span` compact.rs:739, leaf-span helpers), and
     `CompactVertexValueSpanV1` drains as a no-op. Enqueuing any existing work item for a bypass
     row is a drain no-op. The structural reason: bypass rows are vertex-level spans whose
     left-pack would interact with the bypass-origin geometry (`base_slot_start` updates of
     later tail rows, `bump_successor_origins_after_bypass_end`) — a real compaction capability
     that does not exist, not a missing enqueue.
  3. The planned test update (`bypass_accumulates_many_slab_tombstones_without_promotion`,
     bypass.rs:617) exercises the unidirectional `LabeledLaraGraph` remove path, not the
     deferred-wrapper remove side where the trigger would live — the test relocation/rename is
     part of the bypass work, not the trigger work.
- **Expected or needed behavior:** (a) a remove-side hysteresis admission trigger for
  Insertion-policy slab buckets (design fixed in research doc §5.5 and validated by the 0339
  survey); (b) a bypass-row left-pack compaction capability (new compact step or remove-path
  maintenance in `compact.rs`/`remove.rs`/`bypass.rs`, respecting the bypass-origin geometry and
  the ADR 0022 recorded descending-fallback behavior), with the bypass regression test moved to
  the layer that owns the trigger.
- **Impact:** Delete-only workloads degrade scan cost and pin memory on affected rows
  (extent-bounded; the dense fast path re-engages once compaction eventually fires via a later
  insert). No correctness risk; the 0337 slab rule (slab OFFSET overshoot ~109K at the legal
  cap) remains deferred-with-evidence, and this trigger is the recorded lever that would also
  reclaim those tombstones.
- **Next decision:** Split delivery — (1) a small plan for the slab-bucket trigger
  (`CompactVertexEdgeSpanV1`, design already fixed), then (2) a separate bypass-row compaction
  plan (new compact step; needs the bypass-origin geometry contract written down first).
  Update this entry to Resolved in the same patches as each half lands.
  **Both halves landed (2026-09-07):** Plan 0339 (slab side) and Plan 0341 (bypass side).
  The bypass v1 boundary (overflow-log-backed rows defer to the existing fold mechanism) and
  the undirected/vertex-purge remove paths (not wired; a bypass row being purged needs no
  compaction) are recorded in the Plan 0341 report.
