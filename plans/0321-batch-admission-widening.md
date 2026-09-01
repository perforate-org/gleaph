---
name: "Batch admission widening to tree-mode buckets — safety classification + tree-run admission"
overview: "Implement Plan 0321 per ADR 0088 §7 Batch: (1) SAFETY FIRST — the fail-closed tree-mode classification that ADR 0088 §7 records for the 'first slice' was never implemented: `batch_write.rs` has zero `is_tree_mode` references, and `preflight_run` (batch_write.rs:1952) plans a tree-mode bucket's run with slab geometry (`edge_start = root region offset`, `stored_slots` used as a slab slot count) — the computed `edge_start_slot = edge_start + stored_slots` points past the tiny root region, the Unordered hole-reuse branch (batch_write.rs:2025-2045) reads `stored_slots × E::BYTES` from the root region interpreting block ids as edges, and commit would write run edges into the root region and adjacent span regions. A batch insert reaching a tree-mode bucket corrupts the bucket today (same severity class as the Plan 0319 Step 3 rewrite hazard). A batch can reach a tree bucket through the public boundary `reserve_batch_orientations` / `reserve_one_orientation_batch` (bidirectional/deferred.rs:1121, bench callers) with no upstream guard. Plan 0321 therefore: (a) adds the fail-closed run-level classification `OneOrientationBatchError::TreeModeBucketRunUnsupported` in `preflight_run` (before the hole-reuse/tail-fit branches) so tree-targeted runs are cleanly rejected and callers fall back to the scalar path — restoring the ADR §7 promised semantics; (b) then WIDENS admission for the simple dominant case: a run whose target bucket is already tree-mode with room for the whole run without root growth beyond the in-window/log rules (tail-block room accounting per ADR 0088 §1) is admitted through the batch boundary: reserve mints the needed `kind = EdgeLeaf` blocks up front, commit bulk-writes them via `write_payload` (full blocks) + `write_payload_partial` (tail), and publish bumps `stored_slots`/`degree` in one descriptor write; root-region growth follows the scalar `tree_mode_insert_edge` root-growth rules (in-window append or overflow-log root-id append). Runs that would CROSS `T_PROMOTE` on a slab bucket keep today's behavior (batch writes past the threshold without promoting; the next scalar insert promotes) — recorded, not widened in this slice. Widening threshold-crossing runs (promote-then-batch) is a recorded follow-up. ADR 0088 §7 updated; validator final phase."
todos:
  - id: "audit-batch-boundary"
    content: "Audit the batch boundary and record findings in this plan doc BEFORE code: (a) who builds `OneOrientationBatchPlan` / calls `reserve_one_orientation_batch` / `insert_one_orientation_batch` in production (bidirectional/deferred.rs:1133 reserve/commit/rollback, bench.rs — the canbench benches are direct callers) and whether any upstream classification of tree buckets exists (grep says NO — verify); (b) the reachability of the corruption: `preflight_run` (batch_write.rs:1952) computes `edge_start_slot = bucket.edge_start() + stored_slots` for a tree bucket (root region + stored → garbage projection), the Unordered hole-reuse branch (batch_write.rs:2025-2045) reads `stored_slots × E::BYTES` from the root region, and commit writes run edges into the projected slab — confirm the write path (`BatchReservation::commit` → `insert_one_orientation_batch_with_locations`) would write through the same garbage geometry; (c) whether a run can even target a tree bucket today through the wrapper (which public APIs build runs — GQL/kernel layer out of crate, so the crate boundary must fail-closed); (d) the LTB bulk-write surface available for the widening: `write_payload` / `write_payload_partial` per block, `mint()` cost, tail-block room rule `stored_slots % B`, root-region append rules (in-window vs overflow-log root-id append) from `tree_mode_insert_edge` (tree_write.rs); (e) ADR 0045's batch boundary contract (reserve/commit/rollback ownership) as applied to tree runs. Record findings + the seam where the classification goes (recommended: `preflight_run` head + `prepare_batch_buckets`), then implement."
    status: pending
  - id: "tree-run-safety-classification"
    content: "Add the fail-closed classification FIRST (independent of the widening, committable alone): in `preflight_run` (batch_write.rs:1952), after the width-mismatch check and BEFORE any slab projection (`edge_start_slot` computation) or hole-reuse branch, reject runs whose bucket `is_tree_mode()` with a new typed `OneOrientationBatchError::TreeModeBucketRunUnsupported { owner_vertex_id: VertexId, label_id: BucketLabelKey }`. Also reject in `prepare_batch_buckets`? No — preparation only handles MISSING buckets (new buckets are slab by construction; a new-bucket run is fine). Also guard the `BatchReservation::commit` path defensively (a run whose bucket flipped to tree mode between reserve and commit — e.g. via a concurrent scalar insert promoting it — must fail closed at commit with the same typed error rather than writing slab geometry into a tree bucket; audit whether commit re-reads descriptors and add the check where the run's slot writes are applied). This restores the ADR §7 'first slice' semantics that were never implemented and closes the corruption hole. Unit tests: (i) tree-mode bucket + batch run → typed rejection, bucket untouched (edge set unchanged after failed batch), (ii) rejection happens in reserve (no canonical writes), (iii) a vertex whose OTHER bucket is tree-mode but whose targeted run bucket is slab → batch succeeds (guard is per-run, not per-vertex)."
    status: pending
  - id: "tree-run-batch-admission"
    content: "Widen batch admission: a run targeting an already-tree-mode bucket (4-byte edge, no inline property — tree invariants from ADR 0088 §1/§2) is admitted through the batch boundary. Design per ADR §7 ('reserve n blocks → write → publish'): (1) preflight_run classifies the tree run: tail room = `B - (stored_slots % B)` (B = 1024) when stored_slots > 0; if `run.edges.len() <= tail_room` the run fits the current tail block with NO new blocks; else it needs `ceil((run.edges.len() - tail_room) / B)` new blocks — compute the required new root entries (each new block appends one u32 root id, subject to the physical root capacity: if the root region cannot take the new ids in-window, route to the overflow-log root-id path exactly as scalar `tree_mode_insert_edge` does, or fail the run typed — pick from the audit of `tree_mode_insert_edge`'s root-growth logic and REUSE it where possible instead of reimplementing); (2) reserve: mint the new blocks up front (per-leaf block accounting mirrors the scalar tree insert); (3) commit writes the run's edges into the tail block + new blocks via `write_payload`/`write_payload_partial` in logical order (append-only, gap-0); (4) publish bumps `stored_slots += n` and `degree += n` in one descriptor write (tree insert is append-only — no tombstone interaction; `Unordered` placement on tree buckets: tree mode has no tombstone holes for slab reuse — the hole-reuse branch must be skipped for tree runs; `Unordered` placement on a tree run appends at the tail like `Insertion` — document this). Inline-property edges in tree runs stay rejected (`InlinePropertyBytesWidthMismatch` per the existing tree guard). Tests: (a) whole-run-fits-tail batch on a tree bucket (edges readable, order preserved, stored/degree correct); (b) run crossing a block boundary (tail room 5, run 300 → 1 new block minted, root region +1 entry); (c) run requiring multiple new blocks + root growth; (d) mixed plan (one slab run + one tree run) commits atomically; (e) Unordered placement on tree run appends (no hole reuse); (f) inline-property edge in a tree run stays typed-rejected; (g) rollback: reserve failure after block mint rolls the minted blocks back (free count restored); (h) reopen: batch-committed tree bucket reopens with correct descriptor (init validation)."
    status: pending
  - id: "threshold-crossing-runs"
    content: "Decide and record the threshold-crossing semantics: a run targeting a SLAB bucket that would push `stored_slots` past `T_PROMOTE` (4096). Today the batch path writes past the threshold without promoting (violating the 'no slab bucket exceeds T_promote' promotion invariant between operations — recovered only at the next scalar insert). For Plan 0321: keep batch writes allowed to exceed the threshold (scalar fallback at the NEXT scalar insert promotes; promotion transcribes any stored size — verify this with a test), OR classify threshold-crossing runs as Unsupported → the caller falls back to scalar inserts (ADR §7 first-slice wording). Pick ONE, implement the guard/test, and record the decision + rationale in the plan (recommendation: keep batch-past-threshold working — promotion handles any stored size — and add the regression test proving a post-batch scalar insert promotes correctly; do NOT add a new error)."
    status: pending
  - id: "wasm-budget-recheck"
    content: "Run `cargo build --release --target wasm32-unknown-unknown --features canbench` from `crates/ic-stable-lara/` and verify the exported-name budget stays ≤ 20,000 chars (current baseline 15,730). Full green-bar matrix per commit: plain `cargo check -p ic-stable-lara`, `cargo test -p ic-stable-lara --no-default-features` (613 baseline + new), `cargo test -p ic-stable-lara --features canbench` (0 failed), `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings`, `cargo fmt --check -p ic-stable-lara`, one canbench spot-run with yml restore. Record the wasm char count in the completion report."
    status: pending
  - id: "adr-0088-update-and-validate"
    content: "Update `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` §7 batch paragraph: mark the tree-run classification (fail-closed) and the widened tree-run admission (reserve n blocks → write → publish) as implemented in Plan 0321; record the threshold-crossing decision; keep LPB/inline-property batch admission fail-closed (Plan 0320 slab-form materialize does not extend to batch tree runs). Run `python3 ~/.agents/skills/plan/scripts/validate_plan.py plans/0321-batch-admission-widening.md --phase final` and confirm structurally-valid final-phase verdict before reporting completion."
    status: pending
isProject: false
---

# Batch admission widening to tree-mode buckets — safety classification + tree-run admission

## Objective

Close the batch-boundary gap of ADR 0088 §7: the promised first-slice classification ("tree-mode buckets and threshold-crossing runs → Unsupported → existing scalar fallback") was never implemented — `batch_write.rs` has zero tree-mode awareness, so a batch run targeting a tree-mode bucket is planned with slab geometry and **corrupts the bucket on commit** (root region misread as slab slots; writes through `edge_start + stored_slots` garbage projection). Plan 0321 ships the fail-closed classification, then widens batch admission to admit already-tree-mode runs (reserve blocks → write → publish), keeping threshold-crossing slab runs on their existing behavior with a regression test.

Success signal:

- Tree-mode bucket runs are admitted through the batch boundary with correct LTB block reservation / bulk write / descriptor publish, OR (if the audit shows the widening needs a bigger seam than this slice) fail-closed with a typed error + documented scalar fallback — the corruption hole is closed either way, with the decision recorded.
- Rollback semantics hold: a failed tree-run reserve/release leaves the LTB free list exactly as before.
- No regression to the four existing slab paths; full green-bar matrix green (plain check, 613+ tests, full canbench suite 0 failed, clippy, fmt, wasm ≤ 20,000 chars).
- ADR 0088 §7 updated; `validate_plan.py --phase final` structurally valid.

## Context

- Plan 0320 (merged, reworked at `b3c93f223`) closed the width-addition gap with the steppable materialize primitive (inline ≤ 4 KiB; deferred via `InlinePropertyMaterializeDeferredRequired` signal → wrapper enqueue → drain → retry contract).
- ADR 0088 §7 (batch, ADR 0045): "first slice classifies tree-mode buckets (and threshold-crossing runs) as `Unsupported` → existing scalar fallback; the scalar path performs promotion. Widening batch admission to tree buckets (reserve n blocks → write → publish) is a natural follow-up slice." — **the classification does not exist in the code**; `batch_write.rs` (5,724 lines) has no `is_tree_mode` reference. `preflight_run` computes `edge_start_slot = bucket.edge_start() + stored_slots` and the `Unordered` hole-reuse branch reads `stored_slots × E::BYTES` from `edge_start` — for a tree bucket both are garbage (root region is `root_len` u32 words, not `stored_slots` edge slots), and commit writes through the garbage projection.
- Batch entry points: `reserve_batch_orientations` / `commit_batch_orientations` / `rollback_batch_reservation` (bidirectional/deferred.rs:1121/1153/1578) and the one-orientation `reserve_one_orientation_batch` / `insert_one_orientation_batch` (batch_write.rs:1029/3928). Plan building happens above the crate (GQL/kernel layer) or in bench/test code.
- The scalar fallback for unsupported runs is the caller's existing per-edge insert path (which handles tree append + promotion correctly since Plan 0318/0319).
- Constants: `B = 1024` slots per LTB block, `T_PROMOTE = 4096`, `T_DEMOTE = 2048`, tree invariant: degree > T_DEMOTE between operations (Plan 0319).
- **Out of scope (recorded)**: threshold-crossing promotion-then-batch (admit slab runs past T_PROMOTE as today; promotion handles oversized stored — regression test required), inline-property edges in tree runs (LPB-in-tree), batch remove (out of scope — batch module is insert-only per ADR 0045), new canbench benches.
- **Green-bar discipline (per commit)**: plain `cargo check -p ic-stable-lara`, `cargo test -p ic-stable-lara --no-default-features` (613 baseline), `cargo test -p ic-stable-lara --features canbench` (full suite 0 failed), `cargo fmt --check`, `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings`, wasm build + `ic-wasm info` name-char count (≤ 20,000), `validate_plan.py --phase draft` after each commit.

## Scope

In: audit of the batch fallback flow, fail-closed tree-run classification, tree-run batch admission (reserve → bulk block write → publish), threshold-crossing semantics decision + test, test matrix, ADR/validator updates. Out: batch remove (batch boundary is insert-only per ADR 0045), inline-property tree runs, promote-then-batch in one reservation, new canbench benches, LPB-in-tree.

## Expected Change Surface

- `crates/ic-stable-lara/src/labeled/graph/batch_write.rs` — tree-run guard + tree-run admission path (preflight/reserve/commit) + tests (or batch_write_test.rs)
- `crates/ic-stable-lara/src/labeled/graph/error.rs` or batch error enum — new typed variant(s)
- `crates/ic-stable-lara/src/labeled/graph/tree_write.rs` — reusable root-growth helpers for the batch path (if the audit says reuse beats duplication)
- `crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs` — only if the boundary seam lands there
- `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` — §7 update
- `plans/0321-batch-admission-widening.md` — status/audit-findings updates

## Steps

### Step 0 — audit (record findings in this plan doc before implementing)

Map: (a) the production callers of the batch boundary and where a scalar fallback would live; (b) the exact corruption reachability for tree-mode runs today (preflight garbage projection + hole-reuse read + commit write); (c) the LTB bulk-write primitives available (write_payload on full blocks) and the root-growth seam shared with `tree_mode_insert_edge`; (d) whether threshold-crossing slab runs are admitted today (expected: yes, writes past T_PROMOTE without promoting) and confirm promotion handles them. Contradictions with this plan go back to the orchestrator, not silently reinterpreted.

#### Step 0 audit findings (2026-09-02, recorded before code)

**(a) Production callers of the batch boundary**:
- `DeferredLabeledLaraGraph::reserve_batch_orientations` →
  `forward.reserve_one_orientation_batch(&plan)` /
  `reverse.reserve_one_orientation_batch(&plan)`
  (bidirectional/deferred.rs:1133-1134)
- `bench.rs:243, 300, 402` (canbench benches; one is the
  `bench_l_s2_det_sat_4096` we already use as a spot-run gate)
- **GQL/kernel layer** is OUT of crate: `OneOrientationBucketPlan`
  is built above the crate. The crate boundary must be
  fail-closed for any tree bucket the GQL kernel might pass in.

**(b) Corruption reachability (confirmed)**:
- `batch_write.rs` grep for `is_tree_mode` returns ZERO matches
  (verified). The flag is set/cleared on `LabelBucket` but the
  batch planner never reads it.
- `preflight_run` (batch_write.rs:1987) computes
  `edge_start_slot = bucket.edge_start() + u64::from(bucket.stored_slots)`.
  For a tree bucket, `edge_start` is the root region offset and
  `stored_slots` is the **block id count** (one u32 per block in
  the root region), not a slot count. The sum is garbage.
- The `Unordered` hole-reuse branch (batch_write.rs:2025-2045)
  reads `stored_slots × E::BYTES` bytes from `edge_start` and
  treats the bytes as edge encodings. For a tree bucket those
  bytes are **u32 block ids**, not edge payloads; the
  `is_tombstone_edge` scan reads nonsense, the hole list is
  garbage, and commit's `write_payload` overwrites the root
  region.
- `commit` (line 3928 `insert_one_orientation_batch_with_locations`)
  writes through the same `edge_start_slot` garbage projection.
  A run of N edges writes N × 4 bytes starting at the projected
  garbage offset, corrupting both the root region and adjacent
  span regions.
- **Severity class matches Plan 0319 Step 3 rewrite hazard**:
  the planner misreads the geometry and the writer follows the
  planner. Closing this is the same urgency.

**(c) LTB bulk-write primitives available**:
- `ltb().mint()` allocates a fresh block (returns block id).
- `ltb().write_payload(block_id, &bytes)` writes the full block
  payload (B = 1024 rows = 4096 bytes for 4-byte edges).
- `ltb().write_payload_partial(block_id, &bytes)` writes a
  partial block (for the tail).
- `ltb().release(block_id)` returns the block to the free list
  (rollback path).
- `edges().allocate_span(n)` and `edges().read_slots_contiguous_bytes`
  / `write_slots_contiguous_bytes` are the root-region accessors.
- **Root-growth seam** (tree_write.rs:58, `tree_mode_insert_edge`):
  the scalar path computes `physical_root_len` from
  `tree_mode_physical_depth()` and `stored_slots`; refuses
  growth past `R_MAX = 1024` with `TreeRootCapacityReached`; on
  `tail_offset == 0` it (1) mints a new LTB block, (2) allocates
  a new root-region span (`new_root_len = old_root_len + 1`),
  (3) copies old block ids verbatim, (4) appends the new block
  id, (5) publishes the new descriptor. The **failure-rollback
  recipe** is: release the minted block and the new span on
  any post-mint error.
- **For tree-run batch admission**: the run needs `ceil((N -
  tail_room) / B)` new blocks. The new block ids go into the
  root region in order (block 0, block 1, …). Root growth is
  `old_root_len + n_new_blocks` (in the simple tail-fit case
  where the run crosses one or more block boundaries). If the
  total physical root length exceeds `R_MAX` after the grow,
  the run is rejected with `TreeRootCapacityReached` (same as
  scalar).

**(d) Threshold-crossing slab runs**:
- Today: `preflight_run` writes past `T_PROMOTE` without
  promoting. The next scalar insert triggers `promote.rs:96`'
  `is_tree_mode()` check on the stored bucket. If the bucket is
  at `stored_slots > T_PROMOTE` with `degree > T_DEMOTE` (true
  for an all-insert sequence), promotion succeeds (Plan 0318
  / 0319 verified; promotion handles any stored size).
- **Plan 0321 decision**: keep batch-past-threshold working.
  Add a regression test that asserts: (i) batch insert pushes
  `stored_slots` past `T_PROMOTE`; (ii) the next scalar insert
  promotes the bucket to tree mode; (iii) the tree bucket's
  contents are intact (all batch-inserted edges are reachable).
  Do NOT add a new typed error for threshold-crossing.

**(e) Chosen seam**:
- **Tree-run classification** in `preflight_run` at the head
  (after `find_bucket`, before `edge_start_slot` math), with
  per-run precision: a vertex with mixed slab + tree buckets
  has its slab run admitted and its tree run rejected in the
  same plan.
- **Commit re-validation** in `insert_one_orientation_batch_with_locations`:
  re-read the bucket descriptor and re-check `is_tree_mode()`
  before applying the run. Closes the TOCTOU window where a
  scalar insert in another batch context promotes the bucket
  between reserve and commit.
- **Typed error**:
  `OneOrientationBatchError::TreeModeBucketRunUnsupported {
  owner_vertex_id: VertexId, label_id: BucketLabelKey,
  current_mode_bucket_kind: BatchRunBucketKind }`. The
  `BatchRunBucketKind` enum is `{ Slab, Tree }` so callers can
  pattern-match on the failure (for the widening slice, callers
  will only see `Tree` after the widening lands).

### Step 1 — safety classification (fail-closed)

`preflight_run` rejects tree-mode bucket runs with a typed error before any geometry math; commit re-validates (mode flip between reserve and commit fails closed). See todo `tree-run-batch-admission` — wait, todo `tree-run-batch-admission` implements both the guard and the widening; Commit 1 = guard only (independently committable, closes the hole), Commit 2 = widening.

### Step 2 — tree-run batch admission

For a tree-mode target bucket: preflight computes tail-block room (`B - stored_slots % B`) and required new blocks; reserve mints blocks (per-leaf accounting like the existing leaf-expansion cursors); commit bulk-writes the run in logical order and publishes the descriptor (stored_slots += n, degree += n) in one write; root growth beyond `R_max` physical root length fails closed (`TreeRootCapacityReached` — same as scalar). Rollback releases minted blocks. See todo `tree-run-batch-admission`.

### Step 3 — threshold-crossing semantics + tests, wasm budget, ADR, validator

See todos `tree-run-batch-admission` (test list), `tree-run-safety-classification`, `threshold-crossing-semantics`, `wasm-budget-recheck`, `adr-0088-update-and-validate`.

## Validation

- **Unit tests**: guard rejection with clean rollback; tree-run batch insert (tail fit / block boundary / multi-block + root growth); mixed slab+tree run plans; Unordered placement on tree runs; rollback block reclamation; post-batch scalar insert on an over-threshold slab bucket (promotion still works); inline-property tree-run stays typed-rejected; reopen round-trip.
- **Green-bar matrix**: plain check / 613+ baseline / full canbench suite 0 failed / clippy -D warnings / fmt / wasm ≤ 20,000 chars.
- **Validator**: `validate_plan.py --phase final` structurally valid.

## Completion Criteria

- [ ] Tree-mode bucket runs fail closed at the batch boundary (`TreeModeBucketRunUnsupported` or the audit-selected seam) with per-run precision; a vertex mixing slab + tree buckets batches correctly.
- [ ] Already-tree bucket runs admitted via the batch boundary (reserve mints blocks → bulk write → one descriptor publish); rollback restores LTB free list exactly.
- [ ] Threshold-crossing slab runs: documented + tested decision (kept working with promotion-regression test, or typed fallback — recorded).
- [ ] Full green-bar matrix green; wasm exported-name count recorded and ≤ 20,000.
- [ ] ADR 0088 §7 updated; `validate_plan.py --phase final` PASS.

## Later Slices (recorded, not in this plan)

- **Threshold-crossing runs → promote-then-batch** (if kept Unsupported): scalar fallback per run is correct but O(n) per-edge; promoting first then batching the tree append is the eventual shape.
- **Batch remove for tree buckets** (tombstone batch): the batch boundary is insert-only; removal widening is unscoped.
- **1M-degree PocketIC sweep** (Plan 0324): unchanged; interior-level insert growth still blocks true 2^20+ reachability.