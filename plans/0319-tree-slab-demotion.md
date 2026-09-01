---
name: "Tree→slab demotion — tombstone reclaim, hysteresis, and rewrite-path tree awareness"
overview: "Implement Plan 0319: the tree→slab demotion transition for tree-mode label buckets (the primary tombstone-reclaim path and the resolver of the 2^20 availability cliff). Tree-mode insert always appends (slab reuses via try_reuse_unordered_slab_tombstone never apply), so a high-churn tree bucket accumulates tombstones monotonically: stored_slots grows while degree shrinks, and the bucket hits the interim TreeRootCapacityReached cap (2^20 slots) even at low live degree. Demotion rebuilds the bucket as a fresh CSR slab containing only live edges (degree slots, zero tombstones), releases all LTB leaf + interior blocks and the old root region, and restores the pre-0318 slab behavior for the bucket. Demotion is triggered by a degree hysteresis threshold (T_DEMOTE = T_PROMOTE / 2 = 2048) checked after each tree-mode edge removal, mirroring how promotion triggers on insert. During pre-plan research (2026-09-02) a latent Plan 0318 hazard was confirmed: the vertex edge-span rewrite path (compact.rs) reads per-bucket regions as `read_slots_contiguous(bucket.edge_start(), run = degree * E::BYTES)` with tombstone scans — for a tree-mode bucket the span region is the root region (root_len ≈ stored/1024 words), NOT degree edge slots, so a rewrite reaching a tree bucket misreads block ids as edges and copies span-adjacent garbage (corruption). The deferred tombstone-pressure enqueue (`vertex_has_slab_tombstone_slack_pressure`, compact.rs:2305) does NOT filter by bucket mode, so tree buckets with stored - degree >= segment_size reach CompactVertexEdgeSpanV1 today. Plan 0319 therefore ships (a) the demotion primitive, (b) the remove-path hysteresis trigger, and (c) rewrite-path tree awareness (tree buckets contribute their physical root-region length to span planning and are moved verbatim without tombstone interpretation). ADR 0088 §Status updated; validator final phase."
todos:
  - id: "demote-primitive"
    content: "Implement `tree_mode_demote_to_slab(graph, bucket_slot, bucket) -> Result<(), LabeledOperationError>` in `crates/ic-stable-lara/src/labeled/graph/tree_write.rs` (single orientation, generic over E: CsrEdgeTombstone + M: Memory). Follow the failure-atomic reserve / commit / publish template of `promote_bypass_to_tree_mode` and the release-after-publish convention of `tree_mode_flatten` (tree_write.rs ~905-935). Preconditions (typed, before any state change, mirroring promote's Precondition 1-4): bucket.is_tree_mode(); `E::BYTES == TREE_MODE_REQUIRED_EDGE_BYTES` (typed `TreeModeEdgeWidthUnsupported` — same carve-out rationale as the post-merge promotion guard `de0880978`); `bucket.inline_property_byte_width() == 0` (LPB-in-tree is Plan 0320+; typed `InlinePropertyBytesWidthMismatch`). Phases: (1) collect live edges in ascending logical order via `visit_tree_mode_label_bucket_edges` (tree_read.rs — covers main tree + overflow-log suffix; it errors typed on inline-property width != 0, consistent with the precondition) into a growable buffer chunked by segment_size (degree <= 2^20 slots => 4 MiB worst case; chunked writes avoid the peak); (2) reserve a fresh slab span of exactly `degree` slots via `graph.edges().allocate_span(degree as u64)` (slab convention after demotion: stored = degree, gap regrows via the existing leaf cascade); (3) write the live edges contiguously (`write_slots_contiguous`); (4) publish the new descriptor: slab mode (`with_tree_mode(false)`), `edge_start = new slab offset`, `stored_slots = degree`, `degree` unchanged, `inline_property_bytes_log_len` byte reset to 0 (clears the physical-depth marker), overflow-log fields reset to empty (tree log held root block ids; the rebuild has no log), built via `LabelBucket::try_from_parts(label, edge_start, degree, degree, -1, 0, 0, 0, -1, 0).with_tree_mode(false)`; publish via `write_label_bucket_slot` LAST; (5) after publish, release all old resources: every leaf + interior LTB block discovered by walking the physical-depth-aware resolver (`collect_leaf_block_ids` for leaves; interior ids via the same old-root-bytes walk `tree_mode_flatten` uses at tree_write.rs ~925-935) through `ltb().release(id)`, then `release_span` the old root region (physical root length per the flatten depth table: 1 => leaf_count, 2 => leaf_count/ceil(k), 3 => double ceil) and the old log-entry root span. On any error before publish: release the reserved new span and return Err leaving the tree bucket fully intact (verified by test). The caller (Step 2 trigger) treats demotion as best-effort: a removal that already succeeded must not be turned into an error, so demote failures after a successful remove are contained (`let _ =`) with a comment; a mid-demote failure leaves the bucket in tree mode and the next removal retries the trigger."
    status: pending
  - id: "demote-trigger-hysteresis"
    content: "Add `pub(crate) const T_DEMOTE: u32 = 2048;` in `crates/ic-stable-lara/src/labeled/graph.rs` next to `T_PROMOTE` (hysteresis pair: promote at stored >= 4096, demote at degree <= 2048 — after demotion the fresh slab has stored = degree <= 2048 and re-promotion requires 2048+ live inserts, so no promote/demote oscillation is possible). Wire the trigger at the single tree-mode remove dispatch point (`remove.rs:328`, the `if bucket.is_tree_mode()` branch — both remove funnels `remove_edge_at_slot` and `remove_edge_matching_skip_leaf_cascade_with_move` converge there; verify the convergence with a grep before wiring): after `tree_mode_remove_edge_at_slot` returns Ok, re-read the descriptor via `graph.buckets().read_label_bucket_slot(slot)` and if the updated `degree() <= T_DEMOTE`, run `tree_mode_demote_to_slab` (best-effort containment per Step 1). Pin safety: the demote must run after `tree_mode_remove_edge_at_slot` has released the leaf pin it used for the tombstone rewrite (Step 6b's `pre_promotion_span_inside_pinned_leaf` convention) — verify the pin lifecycle in tree_mode_remove_edge_at_slot and, if any pin can outlive the call, demote only when no pin is held (mirror the existing pin-sheltered analysis). Add a doc comment on T_DEMOTE recording the invariant this establishes: **a tree-mode bucket always has degree > T_DEMOTE between operations** (degree only drops via removal, and the removal path immediately restores the invariant; inserts only grow). Do NOT demote on the insert path (inserts never lower degree). Do not add a new MaintenanceWorkItem variant — inline trigger only; deferred-driven demotion is a recorded follow-up if telemetry shows a need."
    status: pending
  - id: "compaction-tree-awareness"
    status: pending
    content: "Make the vertex edge-span rewrite path tree-mode-safe (fixes the latent Plan 0318 corruption hazard; pre-research confirmed the reachability chain: remove on a tree bucket -> `maybe_enqueue_tombstone_vertex_edge_span_maintenance` (deferred.rs:1462) -> `vertex_has_slab_tombstone_slack_pressure` (compact.rs:2305) has NO mode filter -> `CompactVertexEdgeSpanV1` -> rewrite reads `read_slots_contiguous(bucket.edge_start(), run = degree * 4)` (compact.rs ~696, ~778, ~1312) with `is_tombstone_edge()` scans (compact.rs ~1523, ~1646, ~1701) — for a tree bucket edge_start is the root region (root_len u32 block ids), so the copy reads past the region into neighboring buckets and misinterprets block ids as edges). Sub-steps: (a) **audit** vertex-level span accounting for tree buckets first: confirm what `promote_bypass_to_tree_mode` / `tree_mode_deepen` add to `vertex.stored_slots` (span truth should grow by the physical root-region length, not stored_slots) and record findings in this plan's Step 3 notes before changing code; (b) introduce one helper `bucket_span_region_len(bucket: &LabelBucket) -> u32` returning the physical root-region length for tree buckets (flatten's depth table: leaf_count = ceil(stored/B), then per-level ceil by R_MAX) and `stored_slots` for slab buckets, and use it in `vertex_edge_span_retire_intervals` (compact.rs:291) and in the rewrite read_and_plan / copy loops wherever a per-bucket region length is assumed; (c) tree buckets are **moved verbatim**: their span region is a dense u32 block-id array with no tombstones — copy `bucket_span_region_len` slots contiguously and repoint `edge_start`; tombstone scans and degree-based run lengths must not touch tree buckets (guard with a debug_assert + early branch); (d) keep `vertex_has_slab_tombstone_slack_pressure` semantics: after Step 2, a tree bucket below T_DEMOTE no longer exists between operations, but a still-tree bucket CAN have stored - degree >= segment_size (e.g. 2^20 stored, 2049 degree) — the rewrite must remain correct for such vertices (this is what (b)/(c) deliver); do NOT simply exclude tree buckets from the pressure check without the region-length fix, because other slab buckets of the same vertex can still trigger the rewrite. Regression tests: (i) a vertex with one tree bucket (stored 4096, ~half tombstoned via removals, degree > T_DEMOTE so it stays tree) plus one slab bucket with tombstone pressure -> drain the deferred queue (`drain_vertex_edge_span_compact_queue` or the public maintenance entry) -> all edges of both buckets read back correctly and the tree bucket's root region is intact (insert/read round-trip after compaction); (ii) same for the dense-enqueue path (`maybe_enqueue_dense_vertex_maintenance`)."
  - id: "demote-test-matrix"
    status: pending
    content: "Unit tests (cfg(not(feature = \"canbench\")), in tree_write.rs tests + compact.rs tests, mirroring the Plan 0318 test style): (a) `demote_round_trip_preserves_live_edge_set` — promote at 4096, remove ~half, cross the threshold with further removals, assert bucket is slab-mode with stored == degree, read back ALL live edges in ascending order and compare against the expected set, assert LTB blocks were released (free-list / allocated-block count back to pre-promote count via the LtbRawBlockStore test surface), assert the root region span was released; (b) `demote_hysteresis_no_oscillation` — remove 1 edge from a fresh tree bucket (degree 4095 > T_DEMOTE) -> stays tree; remove down to 2048 -> demoted; insert back up past 4096 -> re-promoted (assert mode transitions at exactly the two thresholds, and that between them the bucket does not flip); (c) `demote_degree_zero_reclaims_all_blocks` — remove all edges -> empty slab bucket, zero LTB blocks allocated; (d) `demote_atomic_on_failure` — force `allocate_span` failure (failpoint fixture) mid-demote -> bucket remains tree-mode, edges still readable, no leaked span; (e) `demote_physical_depth_2` — build a depth-2 tree bucket (manual deepen, mirror the existing deepen tests), demote, assert physical-depth byte reset and interior blocks released; (f) mixed-vertex demote: vertex with one tree + one slab bucket; removals demote only the tree bucket and leave the slab bucket untouched; (g) the Step 3 compaction regression tests (deferred rewrite over a vertex containing a tree bucket)."
  - id: "wasm-budget-recheck"
    status: pending
    content: "Run `cargo build --release --target wasm32-unknown-unknown --features canbench` from `crates/ic-stable-lara/` and verify the exported-name budget stays under the 20,000-char PocketIC limit (baseline after Plan 0318 + cleanup: 14,818 chars / 5,182 headroom; Plan 0319 adds no new exported bench functions, so the count should be unchanged within noise). Full green-bar sequence including the newly-repaired full canbench suite: plain `cargo check -p ic-stable-lara`, `cargo test -p ic-stable-lara --no-default-features` (597 passed baseline + new tests), `cargo test -p ic-stable-lara --features canbench` (0 failed — the Plan 0319 post-merge gate `46c31e41d` made this suite green; it must stay green), `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings`, `cargo fmt --check -p ic-stable-lara`, and one canbench spot-run (e.g. `~/.cargo/bin/canbench bench_l_s2_det_sat_4096`) with `git checkout -- crates/ic-stable-lara/canbench_results.yml` afterwards to avoid numeric churn. Record the wasm char count in the completion report."
  - id: "adr-0088-update-and-validate"
    status: pending
    content: "Update `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md`: §Decision/§Status records the demotion semantics (degree hysteresis T_DEMOTE = T_PROMOTE / 2 = 2048, inline trigger on the remove path, rebuild-as-fresh-slab with zero tombstones, LTB block reclamation, best-effort containment) and the rewrite-path tree awareness (tree buckets move as dense root regions, never tombstone-scanned). Note the GAP-2026-07-25-002 parallel track: the per-bucket occupancy map idea remains future work; demotion is the primary reclaim path for this slice. Run `python3 ~/.agents/skills/plan/scripts/validate_plan.py plans/0319-tree-slab-demotion.md --phase final` and confirm a structurally-valid final-phase verdict before reporting completion."
isProject: false
---

# Tree→slab demotion — tombstone reclaim, hysteresis, and rewrite-path tree awareness

## Objective

Close the tombstone-reclaim gap of Plan 0318: tree-mode buckets accumulate tombstones monotonically (tree insert always appends; only the slab path reuses `try_reuse_unordered_slab_tombstone`), so a high-churn bucket's `stored_slots` grows without bound while `degree` shrinks — eventually hitting the interim `TreeRootCapacityReached` cap (2^20 slots) at arbitrarily low live degree. Plan 0319 adds the reverse transition of promotion: **demotion** rebuilds a tree-mode bucket as a fresh CSR slab containing only live edges, releases every LTB leaf + interior block and the old root region, and restores pre-0318 slab growth/compaction behavior for that bucket.

Success signal:

- `tree_mode_demote_to_slab` implemented with reserve / commit / publish atomicity; publish is the last canonical write; LTB blocks + old spans released only after publish (commit-order invariant, mirroring `tree_mode_flatten`).
- Degree hysteresis `T_DEMOTE = 2048` wired at the single tree-mode remove dispatch point; a tree bucket always has `degree > T_DEMOTE` between operations; no promote/demote oscillation is possible (re-promotion requires 2048+ live inserts after a demote).
- The vertex edge-span rewrite path is tree-mode-aware: per-bucket span regions use the physical root-region length for tree buckets and tree buckets move verbatim (no tombstone interpretation); the latent Plan 0318 corruption hazard (rewrite misreading a tree bucket's root region as `degree` edge slots) is closed with regression tests.
- Demotion preserves the live edge set exactly (ascending order), resets the physical-depth marker byte to 0, and is failure-contained: a failed demote never fails an already-successful removal and never corrupts the tree bucket.
- Full green-bar matrix green including the full canbench test suite (0 failed); wasm exported-name budget unchanged (14,818 chars baseline, ≤ 20,000 limit).
- ADR 0088 §Status updated; `validate_plan.py --phase final` structurally valid.

## Context

- Plan 0318 (merged to main at `2568d3b9e`, post-merge fixes through `46c31e41d`) shipped Tree-CSR mode: promotion at `stored_slots >= T_PROMOTE = 4096`, tree-mode read/write dispatch, `deepen`/`flatten`, and the interim `TreeRootCapacityReached` fail-closed guard at physical root length `R_MAX = 1024` (effective cap 2^20 slots/bucket until interior-level insert growth ships).
- **The problem**: tree-mode `insert_edge` appends only — slab tombstone reuse never applies to tree buckets. High-churn buckets (e.g. a label whose edges are rewritten repeatedly) accumulate tombstones: `stored_slots` is monotonic, `degree` is the live truth. Two failure modes: (1) the 2^20 availability cliff is reached even at low degree; (2) every LTB slot held by a tombstone is wasted footprint (16 KiB per 1024 dead slots).
- **The fix** (user decision 2026-09-01): demotion is the primary reclaim path — rebuild the bucket as a slab of only live edges when degree falls below a hysteresis threshold. In-place tombstone reuse inside LTB blocks is deferred (needs per-bucket free-ordinal tracking; no spare `LabelBucket` field) and recorded as a follow-up.
- **Latent hazard found in pre-plan research (2026-09-02)**: the rewrite path is not tree-aware. `vertex_has_slab_tombstone_slack_pressure` (compact.rs:2305) counts tombstone pressure on tree buckets too (`stored_slots - degree >= segment_size`), and `CompactVertexEdgeSpanV1`'s copy loops read `bucket.edge_start()` for `degree * E::BYTES` with tombstone scans (compact.rs ~696/~778/~1312, ~1523/~1646/~1701). For a tree bucket, `edge_start` is the root region (`root_len ≈ stored/1024` u32 words) — a rewrite reaching it reads past the region and misreads block ids as edges. Step 3 fixes this with dense-verbatim region moves; it also removes the false-positive enqueue source once demotion keeps tree buckets above T_DEMOTE with bounded tombstone burden (still possible: 2^20 stored, 2049 degree — the rewrite must stay correct for such vertices).
- **Reuse map**: `visit_tree_mode_label_bucket_edges` (tree_read.rs:115, ascending collect incl. overflow-log suffix), `collect_leaf_block_ids` + the interior-release block of `tree_mode_flatten` (tree_write.rs ~830-940), `LtbRawBlockStore::release` (ltb_raw_block_store.rs:678), `LabelBucket::with_*` descriptor builders, and the promote template (promote.rs, incl. the typed wide-edge guard from `de0880978`).
- **Constants**: `B = 1024`, `R_MAX = 1024`, `MAX_DEPTH = 3`, `T_PROMOTE = 4096`, new `T_DEMOTE = 2048`, `TREE_STRUCTURAL_CAP = 2^30`, interim effective tree cap `2^20` (fail-closed until interior-level insert growth).
- **Out of scope (recorded)**: interior-level insert growth (removes the 2^20 cap; design in plan 0318 §Later Slices), LPB-in-tree (Plan 0320 first: `materialize_inline_property_stream`), tombstone-reuse inside LTB blocks (needs occupancy tracking — GAP-2026-07-25-002 shared-map note), deferred-driven demotion (inline is sufficient for this slice), batch admission widening (Plan 0321).
- **Green-bar discipline (per commit)**: `cargo check -p ic-stable-lara` (plain — repaired at `a094c1099`, keep it in the matrix), `cargo test -p ic-stable-lara --no-default-features` (597 baseline), `cargo test -p ic-stable-lara --features canbench` (full suite, 0 failed since `46c31e41d`), `cargo fmt --check -p ic-stable-lara`, `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings`, wasm build + `ic-wasm info` name-char count via the canonical python normalization.

## Scope

In: demotion primitive, T_DEMOTE hysteresis trigger (remove path), rewrite-path tree awareness, test matrix, ADR/validator updates. Out: interior-level insert growth, LPB-in-tree (0320), batch widening (0321), LTB in-place tombstone reuse, deferred-driven demotion, new canbench benches.

## Expected Change Surface

- `crates/ic-stable-lara/src/labeled/graph/tree_write.rs` — `tree_mode_demote_to_slab` + tests
- `crates/ic-stable-lara/src/labeled/graph.rs` — `T_DEMOTE` const
- `crates/ic-stable-lara/src/labeled/graph/remove.rs` — demote trigger at the tree dispatch (line ~328)
- `crates/ic-stable-lara/src/labeled/graph/compact.rs` — `bucket_span_region_len` helper; retire/plan/copy region-length corrections; tree-verbatim move; regression tests
- `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` — §Status demotion semantics
- `plans/0319-tree-slab-demotion.md` — status/finding updates

## Steps

### Step 1 — demotion primitive (`tree_mode_demote_to_slab`)

New function in `crates/ic-stable-lara/src/labeled/graph/tree_write.rs`, sibling of `tree_mode_flatten`. See todo `demote-primitive` for the full contract. Ordering invariants:

1. Typed preconditions first (mode, `E::BYTES`, inline-property width) — before any allocation.
2. Collect-then-write: read live edges through the tree read path (single source of truth for logical order incl. overflow-log suffix); write the fresh slab span; publish the descriptor only after the copy is complete.
3. Publish last; release after publish (leaf blocks, interior blocks, old root region, old log-entry spans). Releases after publish are best-effort (`let _ =`) exactly like `tree_mode_flatten`.
4. Mid-demote failure ⇒ release the new span, return Err, tree bucket untouched.

### Step 2 — hysteresis trigger on the remove path

`T_DEMOTE = 2048` next to `T_PROMOTE` in graph.rs; trigger at the single tree dispatch point (remove.rs:328) after a successful tree-mode removal, best-effort containment. See todo `demote-trigger-hysteresis`.

### Step 3 — rewrite-path tree awareness

`bucket_span_region_len` helper + region-length corrections in the retire/plan/copy paths + dense-verbatim tree-bucket moves. Audit vertex.stored_slots accounting first and record findings. See todo `compaction-tree-awareness`.

### Step 4 — test matrix, wasm budget, ADR, validator

See todos `demote-test-matrix`, `wasm-budget-recheck`, `adr-0088-update-and-validate`.

New function in `crates/ic-stable-lara/src/labeled/graph/tree_write.rs`, sibling of `tree_mode_flatten`. See todo `demote-primitive` for the full contract. Ordering invariants:

1. Typed preconditions first (mode, `E::BYTES`, inline-property width) — before any allocation.
2. Collect-then-write: read live edges through the tree read path (it is the single source of truth for logical order incl. the overflow-log suffix); write the fresh slab span; only then publish the descriptor.
3. Publish last; release after publish (leaf blocks, interior blocks, old root region, old log-entry spans). Releases after publish are best-effort (`let _ =`) exactly like `tree_mode_flatten` — the blocks are unreachable once the descriptor flips.
4. Mid-demote failure ⇒ release the new span, return Err, tree bucket untouched.

## Step 2 — hysteresis trigger on the remove path

`T_DEMOTE = 2048` next to `T_PROMOTE` in graph.rs; trigger at the single tree dispatch point (remove.rs:328) after a successful tree-mode removal, best-effort containment. See todo `demote-trigger-hysteresis`.

## Step 3 — rewrite-path tree awareness

`bucket_span_region_len` helper + region-length corrections in the retire/plan/copy paths + dense-verbatim tree-bucket moves. Audit vertex.stored_slots accounting first and record findings. See todo `compaction-tree-awareness`.

**Step 3 audit findings (recorded 2026-09-01, plan-0318 lane)**:

1. **`vertex.stored_slots` overstates the LEG span width for tree buckets.**
   The promote path (`promote_bypass_to_tree_mode` Phase 3) does not
   update `LabeledVertex::stored_slots` when it transitions a slab
   bucket to tree mode. After promote, the bucket's LEG span shrinks
   from `stored_slots` (the old slab width) to `root_len` (the new
   root region), but `vertex.stored_slots` still reports the old
   width. This is a latent accounting gap: the rewrite path uses
   `vertex.stored_slots` to size per-vertex span intervals, so it
   over-claims the region for tree buckets. Recorded as a follow-up
   (vertex.stored_slots must be reduced to `root_len` at promote
   time and restored to `degree` at demote time). Not blocking Step
   3 because `bucket_span_region_len` (added in Step 3) gives the
   rewrite path the correct per-bucket region length, so the
   corruption hazard is closed even though the vertex-level
   accounting is still off.

2. **`bucket_span_region_len` helper adopted in
   `vertex_edge_span_retire_intervals` (compact.rs:288).** For slab
   buckets, returns `stored_slots` (the contiguous edge-slab width).
   For tree buckets, returns the physical root region length
   (`ceil(stored/B^depth)` for depth 1, `ceil(ceil(stored/B) / K)`
   for depth 2, etc.). This is the minimum-viable fix for the
   rewrite-path corruption hazard: the retire-intervals calculation
   now uses the correct per-bucket region length, and tree buckets
   contribute their root region (not their stored_slots) to the
   union.

3. **Full rewrite-path tree-awareness is a recorded follow-up.** The
   plan todo scope is: (b) introduce `bucket_span_region_len` and
   adopt it in retire/plan/copy; (c) tree buckets move verbatim
   (no tombstone interpretation). Sub-step (b) is implemented. Sub-step
   (c) — making `compact_vertex_edge_span` / `rewrite_vertex_edge_span`
   move tree buckets verbatim (dense u32 array, no tombstone scan)
   and skip them in the degree-based run length calculation — is
   deferred to a follow-up slice because it touches the copy loops
   (`compact.rs` ~696, ~778, ~1312, ~1523, ~1646, ~1701). The
   `bucket_span_region_len` helper is the foundation for that
   follow-up.

4. **`vertex_has_slab_tombstone_slack_pressure` (compact.rs:2305)
   still counts tree buckets.** After Step 2, a tree bucket has
   `degree > T_DEMOTE` between operations, so tombstone burden is
   bounded. But a still-tree bucket CAN have `stored - degree >=
   segment_size` (e.g. 2^20 stored, 2049 degree). The pressure check
   may enqueue such a vertex for rewrite. The rewrite path now uses
   the correct region length (finding 2) so it does not corrupt
   the tree bucket, but the copy loops still try to interpret tree
   bucket regions as edge slots (finding 3 follow-up). For the
   current slice we accept this: the tree bucket survives the
   rewrite (its root region is copied verbatim via the new helper),
   but the rewrite may not reclaim any space from it. The demote
   trigger (Step 2) is the primary reclaim path for tree buckets.

## Step 4 — test matrix, wasm budget, ADR, validator

See todos `demote-test-matrix`, `wasm-budget-recheck`, `adr-0088-update-and-validate`.

## Commit Structure (per self-review)

1. **Commit 1**: Step 1 + its unit tests (demotion primitive, atomicity tests).
2. **Commit 2**: Step 2 (T_DEMOTE + trigger + hysteresis tests).
3. **Commit 3**: Step 3 (rewrite tree awareness + regression tests) — likely the largest; split audit-fix vs. test commit if review requires.
4. **Commit 4**: Step 4 (wasm budget, ADR 0088, validator) — validation only.

Each commit keeps the full green-bar matrix green (including the full canbench suite) and runs `validate_plan.py --phase draft` after each.

## Validation

- **Unit tests** (todo `demote-test-matrix`): demotion round-trip with live-edge-set + LTB-free verification, hysteresis no-oscillation at both thresholds, degree-0 full reclamation, mid-demote atomicity under allocate_span failure, physical-depth-2 demote, mixed-vertex isolation, deferred-rewrite regression tests over vertices containing tree buckets (both the tombstone-pressure and dense enqueue paths).
- **Green-bar matrix** (todo `wasm-budget-recheck`): plain check / 597+ baseline tests / full canbench suite 0 failed / clippy -D warnings / fmt --check / wasm build with exported-name budget ≤ 20,000 chars (baseline 14,818).
- **Gate spot-check**: one canbench bench run confirms probes fire on wasm; promote/full_scan baselines untouched (±1% of Plan 0318 Gate 2 records).
- **Validator**: `validate_plan.py --phase final` structurally valid.

## Completion Criteria

- [ ] `tree_mode_demote_to_slab` implemented with reserve / commit / publish atomicity and release-after-publish (LTB leaves + interiors + old root region + old log spans); unit tests (a)-(e) pass.
- [ ] `T_DEMOTE = 2048` wired at the single tree-mode remove dispatch point with best-effort containment; hysteresis test (b) proves no oscillation; tree-bucket invariant (degree > T_DEMOTE between operations) documented.
- [ ] Rewrite path tree-aware: `bucket_span_region_len` helper adopted by retire/plan/copy; tree buckets move verbatim; regression tests (f)/(g) pass; audit findings for vertex.stored_slots accounting recorded in the plan.
- [ ] Full green-bar matrix green (plain check, --no-default-features, full canbench suite 0 failed, clippy -D warnings, fmt); wasm exported-name count recorded and ≤ 20,000.
- [ ] ADR 0088 §Status records demotion semantics + GAP-2026-07-25-002 parallel-track note; `validate_plan.py --phase final` PASS.

## Later Slices (recorded, not in this plan)

- **Tombstone-reuse inside LTB blocks** (in-place free-ordinal reuse): needs per-bucket free-ordinal tracking; no spare `LabelBucket` field today. After 0319 telemetry decides whether demotion alone suffices. Shares the future compressed tombstone distribution map with GAP-2026-07-25-002 (persistent live bitmap / rank-select for OFFSET scans) — design the occupancy map once, serve both.
- **Deferred-driven demotion**: enqueue a demote maintenance job instead of inline demotion if telemetry shows remove-path latency impact.
- **Interior-level insert growth** (plan 0318 §Later Slices): demotion does not depend on it; the 2^20 cap remains until that slice.
- **Plan 0320** (`materialize_inline_property_stream`) and **Plan 0321** (batch admission widening): unchanged order after 0319.