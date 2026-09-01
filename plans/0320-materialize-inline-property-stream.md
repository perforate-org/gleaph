---
name: "materialize_inline_property_stream — steppable width-addition primitive"
overview: "Implement Plan 0320 per ADR 0088 §5: the `materialize_inline_property_stream(bucket, w, fill)` migration primitive and its first production consumer, the width-addition transition (w = 0 → w) for label buckets that already hold edges. Today every width transition is fail-closed: `ensure_bucket_inline_property_byte_width_on_slot` (values.rs:793) allows 0→w only when the bucket is completely empty (schema_unset), and `ensure_bucket_inline_property_schema_for_insert` (values.rs:624) rejects ALL mismatches, so a label bucket's property width is frozen at bucket creation and users must recreate buckets to attach properties. The primitive materializes a dense tombstone-inclusive w-byte-per-position property stream (position i ↔ edge position i, 1:1) for a non-empty slab bucket: reserve the byte-slab span (S × w) once through the existing byte-slab allocator (with `inline_property_bytes_compaction_needed` interplay for fragmentation), then fill live positions with the `fill` value via a resumable stepped cursor (ADR 0021 stepped/resumable maintenance — same pattern as the CompactVertexEdgeSpanV1 resume cursors, GAP-2026-07-14-001) so the O(S × w) work is split across calls instead of one instruction-budget-exceeding step; publish the descriptor (width + offset + slab_slots) atomically on completion; reset log fields (fresh dense stream, no log). Tree-mode form (property root + ceil(S/K) LPB blocks, K = floor(payload/w)) is NOT in this slice — tree buckets still reject property edges (insert.rs:303) and promotion keeps its inline-property carve-out; the tree form is the LPB-in-tree follow-up that reuses this primitive's fill/cursor machinery. w1 → w2 re-encoding and w → 0 teardown remain deferred fail-closed. ADR 0088 §Status updated; validator final phase."
todos:
  - id: "audit-inline-property-representation"
    content: "Audit the current inline-property-bytes representation and record findings in this plan's Step 1 notes BEFORE writing code: (a) byte-slab store layout (`self.values`, FreeSpanStore allocator, `retire_byte_span` / `write_bytes`), (b) the per-bucket descriptor fields (`inline_property_bytes_offset`, `inline_property_bytes_slab_slots`, `inline_property_bytes_log_byte`, `inline_property_bytes_log_len`, `inline_property_bytes_log_head`) and their invariants, (c) the log path (`write_edge_inline_property_to_log`, values.rs:539) and how compaction folds it (`compact_inline_property_bytes_slab`, values.rs:61), (d) the read paths (`read_bucket_inline_property_bytes_for_slot`, `read_bucket_inline_property_bytes_span`, `collect_bucket_inline_property_bytes_asc_order`, values.rs:435-539) — what shape do they expect (dense slab prefix + log suffix?), (e) `bucket_live_ordinal_at_edge_slot` (values.rs:664) — how live ordinals map to property positions (tombstone-inclusive vs live-only: ADR 0088 §5 says position i ↔ edge position i tombstone-inclusive for tree mode; confirm what slab mode does today and keep it consistent), (f) the vertex-level `inline_property_bytes_allocated_bytes` accounting (values.rs:652-658) that must be updated on materialize, (g) ADR 0021's resumable-maintenance contract and the CompactVertexEdgeSpanV1 cursor pattern (deferred.rs:31-38) as the template for the stepped fill. Write the findings into this plan doc (## Step 1 findings) before implementing."
    status: pending
  - id: "materialize-primitive"
    content: "Implement `materialize_inline_property_stream(bucket_slot, w, fill) -> Result<(), LabeledOperationError>` on `LabeledLaraGraph` in `crates/ic-stable-lara/src/labeled/graph/values.rs` (single orientation; generic E: CsrEdgeTombstone + M: Memory). Contract (ADR 0088 §5, fail-closed where recorded): preconditions — bucket is slab-mode (tree buckets keep the typed rejection; the tree form is LPB-in-tree later), `w >= 1`, `w <= payload constraint per ADR §5` (reject w > the slab representation's bound with the existing typed error; record the bound chosen from the audit), bucket width == 0 (w1→w2 and w→0 stay deferred fail-closed — typed `InlinePropertyBytesWidthMismatch`), and the bucket must have stored_slots > 0 (the schema_unset fast path already handles the empty case). Phases: (1) RESERVE — one `S × w` byte-slab reservation through the existing allocator (use `inline_property_bytes_compaction_needed` first and run `compact_inline_property_bytes_slab` when the allocator is fragmented, mirroring the existing insert-path compaction interplay); (2) FILL (stepped, ADR 0021) — write the `fill` byte pattern to each live position via a resumable cursor (`(bucket_slot, next_position)`); the cursor state lives in a new `MaintenanceWorkItem` variant (e.g. `MaterializeInlinePropertyStreamV1 { vid, bucket_slot, width, fill_byte, resume_position }` — stable tag per the existing enum conventions, deferred.rs:23) enqueued by the trigger when `S × w` exceeds a single-step threshold (constant, e.g. 1 MiB of fill work per step — pick from the audit); small buckets take an inline fast path (single call, no queue); fill only LIVE positions is NOT required — tombstone-inclusive 1:1 means all S positions get `fill` (live positions are then backfilled by the caller above LARA via the existing per-slot write API); (3) PUBLISH — one descriptor write setting `inline_property_byte_width = w`, `inline_property_bytes_offset`, `inline_property_bytes_slab_slots = S × w` slot accounting per the audit findings, log fields reset; update the vertex-level `inline_property_bytes_allocated_bytes` in the same publish window (values.rs accounting invariant). Failure containment: any error before publish retires the reserved span and leaves the bucket at w=0 untouched (atomic, mirror demotion's containment)."
    status: pending
  - id: "width-addition-wiring"
    content: "Wire width addition 0→w through the production surface: in `ensure_bucket_inline_property_schema_for_insert` (values.rs:624) and `ensure_bucket_inline_property_byte_width_on_slot` (values.rs:793), replace the fail-closed rejection for `bucket_width == 0 && edge_inline_property_width == w && stored_slots > 0` with a call to `materialize_inline_property_stream` (inline fast path when small, deferred enqueue when large — the insert path returns Ok after enqueueing the step job; the insert itself completes AFTER materialization completes, so the insert that triggered the transition must be re-attempted by the caller once the stepped fill finishes — audit how deferred insert completion works today and, if a synchronous contract is required, gate the deferred path to the explicit maintenance entry instead and keep the insert-path materialize synchronous with the single-step threshold; record the decision). Keep fail-closed: width mismatch w1→w2, w→0, and tree-mode buckets with property edges (insert.rs:303 tree guard unchanged — LPB-in-tree is a later slice). Also wire `materialize_inline_property_stream` as a pub(crate) maintenance entry so the deferred worker can drive the stepped fill (`drain_vertex_edge_span_compact_queue`-style pop handling in deferred.rs:210's worker). Document in ADR 0088 §5 that slab width addition is now supported via materialize and tree-mode promotion keeps its carve-out."
    status: pending
  - id: "test-matrix"
    content: "Unit tests (cfg(not(feature = \"canbench\")), values.rs tests + deferred tests, mirroring existing style): (a) `materialize_zero_to_w_preserves_positions` — bucket with S edges incl. tombstones → materialize w=3 fill=0xAA → every live position reads 0xAA via `read_bucket_inline_property_bytes_for_slot`, tombstone positions are tombstone-inclusive (no shift), width/offset/slab_slots published correctly; (b) `materialize_stepped_resume` — force the stepped path (S × w above the threshold) → drive the queue to completion across multiple pops → identical final state to the inline fast path (property stream equality); (c) `materialize_atomic_on_failure` — failpoint on a byte-slab write mid-fill → bucket stays w=0, reserved span retired, no allocated-bytes drift (vertex accounting); (d) `materialize_fragmented_allocator_compacts_first` — fragment the allocator so the S×w request needs compaction → materialize triggers compaction then succeeds; (e) `materialize_rejects_w1_to_w2_and_teardown` — w≠0 source stays fail-closed; (f) `materialize_empty_bucket_fast_path` — schema_unset empty bucket keeps the existing direct width-set path (no stream job); (g) `width_addition_via_insert` — end-to-end: insert N edges at w=0, then insert an edge carrying w-byte inline property → materialize runs → the new edge's property bytes are readable and prior edges read `fill`; (h) `tree_bucket_property_insert_still_fail_closed` — the LPB-in-tree boundary holds. Deferred-worker test: the new MaintenanceWorkItem variant drains via the existing queue machinery (mirror the CompactVertexEdgeSpanV1 drain test pattern, deferred.rs:210)."
    status: pending
  - id: "wasm-budget-recheck"
    content: "Run `cargo build --release --target wasm32-unknown-unknown --features canbench` from `crates/ic-stable-lara/` and verify the exported-name budget stays ≤ 20,000 chars (baseline 14,818 / 5,182 headroom). Full green-bar matrix per commit: plain `cargo check -p ic-stable-lara`, `cargo test -p ic-stable-lara --no-default-features` (604 baseline + new), `cargo test -p ic-stable-lara --features canbench` (0 failed), `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings`, `cargo fmt --check -p ic-stable-lara`, one canbench spot-run with yml restore. Record the wasm char count in the completion report."
    status: pending
  - id: "adr-0088-update-and-validate"
    content: "Update `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` §5: mark `materialize_inline_property_stream` as implemented (slab form, stepped fill per ADR 0021; tree form deferred to LPB-in-tree), record the width-addition support (0→w on non-empty buckets) and the still-deferred transitions (w1→w2, w→0, tree-form). Run `python3 ~/.agents/skills/plan/scripts/validate_plan.py plans/0320-materialize-inline-property-stream.md --phase final` and confirm structurally-valid final-phase verdict before reporting completion."
    status: pending
isProject: false
---

# materialize_inline_property_stream — steppable width-addition primitive

## Objective

Implement the `materialize_inline_property_stream(bucket, w, fill)` primitive recorded in ADR 0088 §5 and wire its first production consumer: width addition (0 → w) for label buckets that already hold edges. Today a bucket's property width is frozen at creation (`ensure_bucket_inline_property_byte_width_on_slot` only transitions an entirely-empty schema_unset bucket) — attaching properties to an existing populated label requires recreating the bucket. The primitive materializes a dense tombstone-inclusive w-byte-per-position stream with a stepped, resumable fill (ADR 0021) instead of one un-steppable `S × w` write, and publishes the descriptor atomically.

Success signal:

- `materialize_inline_property_stream` implemented with reserve / stepped fill / publish atomicity; a resumable cursor (new `MaintenanceWorkItem` variant) splits the `O(S × w)` fill across queue pops for large buckets; small buckets take an inline fast path.
- Width addition 0→w works end-to-end through the insert path: existing edges keep their positions (tombstone-inclusive 1:1), new property reads return `fill` until backfilled above LARA.
- Still fail-closed: w1→w2, w→0, tree-mode property inserts (LPB-in-tree boundary), and `w` beyond the representation bound.
- Full green-bar matrix green (plain check, 604+ tests, full canbench suite 0 failed, clippy, fmt, wasm ≤ 20,000 chars).
- ADR 0088 §5/§Status updated; `validate_plan.py --phase final` structurally valid.

## Context

- Plan 0319 (merged, `4db8f5bd8`) closed the tombstone-reclaim gap: demotion with `T_DEMOTE` hysteresis + rewrite-path tree awareness. Main HEAD is post-0319, all-green.
- ADR 0088 §5 records the primitive: "A future `materialize_inline_property_stream(bucket, w, fill)` primitive (reserve `ceil(S/K)` blocks; commit fill + root extension + one descriptor republish) is recorded as planned: the derived geometry makes later width addition an incremental, steppable operation instead of one `S × w` contiguous allocation. Value backfill semantics stay above LARA (ADR 0008 profile SSOT). `w1 → w2` re-encoding and `w → 0` teardown are separate deferred transitions."
- Current width-transition surface: `ensure_bucket_inline_property_schema_for_insert` (values.rs:624, insert path, all mismatches fail-closed) and `ensure_bucket_inline_property_byte_width_on_slot` (values.rs:793, allows 0→w ONLY on a fully-empty bucket). The insert path calls the former before the slab/tree dispatch (insert.rs:287-292); the tree branch then rejects property edges outright (insert.rs:296-303).
- Slab representation: byte-slab allocator (`self.values`) addressed by `inline_property_bytes_offset` + `inline_property_bytes_slab_slots`, with an append log (`inline_property_bytes_log_*`) folded by `compact_inline_property_bytes_slab`; read paths in values.rs:435-539; fragmentation handled by `inline_property_bytes_compaction_needed` (values.rs:246).
- The tree-form of this primitive (property root + LPB `kind = InlineProperty` blocks, `K = floor(4096/w)`, residue dead bytes) is the LPB-in-tree follow-up; 0320 delivers the primitive's slab-form materialization + stepped-cursor machinery that the tree form will reuse.
- Steppable pattern precedent: `CompactVertexEdgeSpanV1` resume cursors (deferred.rs MaintenanceWorkItem, GAP-2026-07-14-001) and ADR 0021 stepped/resumable maintenance.
- **Out of scope (recorded)**: tree-form materialization / LPB-in-tree (promotion keeps its inline-property carve-out), w1→w2 re-encoding, w→0 teardown, value backfill semantics (stays above LARA per ADR 0008), batch admission widening (Plan 0321), interior-level insert growth.
- **Green-bar discipline (per commit)**: plain `cargo check -p ic-stable-lara`, `cargo test -p ic-stable-lara --no-default-features` (604 baseline), `cargo test -p ic-stable-lara --features canbench` (full suite 0 failed), `cargo fmt --check`, `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings`, wasm build + `ic-wasm info` name-char count (canonical python normalization), `validate_plan.py --phase draft` after each commit.

## Scope

In: the materialize primitive (slab form, stepped fill, atomic publish), width-addition wiring (0→w) through the two ensure_* functions, deferred stepped-fill work item, test matrix, ADR/validator updates, audit findings. Out: LPB-in-tree (tree-form primitive + promotion carve-out removal), w1→w2, w→0, backfill semantics, new canbench benches.

## Expected Change Surface

- `crates/ic-stable-lara/src/labeled/graph/values.rs` — `materialize_inline_property_stream` + stepped-fill plumbing + width-addition wiring in the two ensure_* fns + tests
- `crates/ic-stable-lara/src/labeled/deferred.rs` — new `MaintenanceWorkItem` variant + drain handling
- `crates/ic-stable-lara/src/labeled/graph/insert.rs` — no logic change expected (ensure_* call sites already funnel; verify)
- `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` — §5/§Status update
- `plans/0320-materialize-inline-property-stream.md` — status/audit-findings updates

## Steps

### Step 0 — audit (record findings in this plan doc before implementing)

Confirm and write down: the slab byte-stream layout the read paths expect (dense prefix + log suffix? exact semantics of `bucket_live_ordinal_at_edge_slot` — tombstone-inclusive vs live-ordinal), the byte-slab allocator's reservation/retire API shape, the compaction interplay order, the ADR 0021 stepped pattern (cursor carrier, queue pop contract, crash-safety), and the payload/width bound the slab representation implies for the typed rejection. Any contradiction with this plan's contract goes back to the orchestrator, not silently reinterpreted.

### Step 1 — primitive (`materialize_inline_property_stream`)

Reserve (compaction-aware) → stepped fill (inline fast path below the step threshold, deferred `MaintenanceWorkItem::MaterializeInlinePropertyStreamV1` cursor above it) → atomic publish (descriptor + vertex allocated-bytes accounting). Failure containment: retire the reserved span, bucket untouched. See todo `materialize-primitive`.

### Step 2 — width-addition wiring

0→w on non-empty buckets via the primitive from `ensure_bucket_inline_property_schema_for_insert` / `ensure_bucket_inline_property_byte_width_on_slot`; all other transitions stay fail-closed; tree guard unchanged. See todo `width-addition-wiring`.

### Step 3 — tests, wasm budget, ADR, validator

See todos `test-matrix`, `wasm-budget-recheck`, `adr-0088-update-and-validate`.

## Validation

- **Unit tests** (todo `test-matrix`): position preservation under tombstones, stepped resume equality with the inline path, atomicity under failpoints, allocator-fragmentation interplay, fail-closed boundaries (w1→w2, w→0, tree), schema_unset fast path, end-to-end width addition via insert, deferred drain.
- **Green-bar matrix** (todo `wasm-budget-recheck`): plain check / 604+ baseline / full canbench suite 0 failed / clippy -D warnings / fmt / wasm ≤ 20,000 chars (baseline 14,818).
- **Validator**: `validate_plan.py --phase final` structurally valid.

## Completion Criteria

- [ ] `materialize_inline_property_stream` implemented (slab form, stepped fill, atomic publish) with the audit findings recorded in the plan.
- [ ] Width addition 0→w supported for non-empty buckets through the insert-path ensure_* hooks; w1→w2 / w→0 / tree-mode property inserts remain fail-closed.
- [ ] New `MaintenanceWorkItem` variant drains via the existing queue machinery; stepped and inline paths produce identical streams.
- [ ] Full green-bar matrix green; wasm exported-name count recorded and ≤ 20,000.
- [ ] ADR 0088 §5/§Status updated; `validate_plan.py --phase final` PASS.

## Later Slices (recorded, not in this plan)

- **LPB-in-tree**: tree-form materialization (property root + `ceil(S/K)` LPB blocks, `K = floor(4096/w)`) + removal of the promotion inline-property carve-out — reuses this slice's fill/cursor machinery; 6-8h estimate stands.
- **w1 → w2 re-encoding** and **w → 0 teardown**: separate deferred transitions per ADR 0088 §5.
- **Value backfill UX** above LARA (ADR 0008 profile SSOT; ADR 0058/0059 flavor).
- **Plan 0321** (batch admission widening) and the 1M PocketIC sweep: unchanged order.