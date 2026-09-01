---
name: "materialize_inline_property_stream — steppable width-addition primitive"
overview: "Implement Plan 0320 per ADR 0088 §5: the `materialize_inline_property_stream(bucket, w, fill)` migration primitive and its first production consumer, the width-addition transition (w = 0 → w) for label buckets that already hold edges. Today every width transition is fail-closed: `ensure_bucket_inline_property_byte_width_on_slot` (values.rs:793) allows 0→w only when the bucket is completely empty (schema_unset), and `ensure_bucket_inline_property_schema_for_insert` (values.rs:624) rejects ALL mismatches, so a label bucket's property width is frozen at bucket creation and users must recreate buckets to attach properties. The primitive materializes a dense tombstone-inclusive w-byte-per-position property stream (position i ↔ edge position i, 1:1) for a non-empty slab bucket: reserve the byte-slab span (S × w) once through the existing byte-slab allocator (with `inline_property_bytes_compaction_needed` interplay for fragmentation), then fill live positions with the `fill` value via a resumable stepped cursor (ADR 0021 stepped/resumable maintenance — same pattern as the CompactVertexEdgeSpanV1 resume cursors, GAP-2026-07-14-001) so the O(S × w) work is split across calls instead of one instruction-budget-exceeding step; publish the descriptor (width + offset + slab_slots) atomically on completion; reset log fields (fresh dense stream, no log). Tree-mode form (property root + ceil(S/K) LPB blocks, K = floor(payload/w)) is NOT in this slice — tree buckets still reject property edges (insert.rs:303) and promotion keeps its inline-property carve-out; the tree form is the LPB-in-tree follow-up that reuses this primitive's fill/cursor machinery. w1 → w2 re-encoding and w → 0 teardown remain deferred fail-closed. ADR 0088 §Status updated; validator final phase."
todos:
  - id: "audit-inline-property-representation"
    content: "Audit the current inline-property-bytes representation and record findings in this plan's Step 1 notes BEFORE writing code: (a) byte-slab store layout (`self.values`, FreeSpanStore allocator, `retire_byte_span` / `write_bytes`), (b) the per-bucket descriptor fields (`inline_property_bytes_offset`, `inline_property_bytes_slab_slots`, `inline_property_bytes_log_byte`, `inline_property_bytes_log_len`, `inline_property_bytes_log_head`) and their invariants, (c) the log path (`write_edge_inline_property_to_log`, values.rs:539) and how compaction folds it (`compact_inline_property_bytes_slab`, values.rs:61), (d) the read paths (`read_bucket_inline_property_bytes_for_slot`, `read_bucket_inline_property_bytes_span`, `collect_bucket_inline_property_bytes_asc_order`, values.rs:435-539) — what shape do they expect (dense slab prefix + log suffix?), (e) `bucket_live_ordinal_at_edge_slot` (values.rs:664) — how live ordinals map to property positions (tombstone-inclusive vs live-only: ADR 0088 §5 says position i ↔ edge position i tombstone-inclusive for tree mode; confirm what slab mode does today and keep it consistent), (f) the vertex-level `inline_property_bytes_allocated_bytes` accounting (values.rs:652-658) that must be updated on materialize, (g) ADR 0021's resumable-maintenance contract and the CompactVertexEdgeSpanV1 cursor pattern (deferred.rs:31-38) as the template for the stepped fill. Write the findings into this plan doc (## Step 1 findings) before implementing."
    status: completed
  - id: "materialize-primitive"
    content: "Implement `materialize_inline_property_stream(bucket_slot, w, fill) -> Result<(), LabeledOperationError>` on `LabeledLaraGraph` in `crates/ic-stable-lara/src/labeled/graph/values.rs` (single orientation; generic E: CsrEdgeTombstone + M: Memory). Contract (ADR 0088 §5, fail-closed where recorded): preconditions — bucket is slab-mode (tree buckets keep the typed rejection; the tree form is LPB-in-tree later), `w >= 1`, `w <= payload constraint per ADR §5` (reject w > the slab representation's bound with the existing typed error; record the bound chosen from the audit), bucket width == 0 (w1→w2 and w→0 stay deferred fail-closed — typed `InlinePropertyBytesWidthMismatch`), and the bucket must have stored_slots > 0 (the schema_unset fast path already handles the empty case). Phases: (1) RESERVE — one `S × w` byte-slab reservation through the existing allocator (use `inline_property_bytes_compaction_needed` first and run `compact_inline_property_bytes_slab` when the allocator is fragmented, mirroring the existing insert-path compaction interplay); (2) FILL (stepped, ADR 0021) — write the `fill` byte pattern to each live position via a resumable cursor (`(bucket_slot, next_position)`); the cursor state lives in a new `MaintenanceWorkItem` variant (e.g. `MaterializeInlinePropertyStreamV1 { vid, bucket_slot, width, fill_byte, resume_position }` — stable tag per the existing enum conventions, deferred.rs:23) enqueued by the trigger when `S × w` exceeds a single-step threshold (constant, e.g. 1 MiB of fill work per step — pick from the audit); small buckets take an inline fast path (single call, no queue); fill only LIVE positions is NOT required — tombstone-inclusive 1:1 means all S positions get `fill` (live positions are then backfilled by the caller above LARA via the existing per-slot write API); (3) PUBLISH — one descriptor write setting `inline_property_byte_width = w`, `inline_property_bytes_offset`, `inline_property_bytes_slab_slots = S × w` slot accounting per the audit findings, log fields reset; update the vertex-level `inline_property_bytes_allocated_bytes` in the same publish window (values.rs accounting invariant). Failure containment: any error before publish retires the reserved span and leaves the bucket at w=0 untouched (atomic, mirror demotion's containment)."
    status: completed
  - id: "width-addition-wiring"
    content: "Wire width addition 0→w through the production surface: in `ensure_bucket_inline_property_schema_for_insert` (values.rs:624) and `ensure_bucket_inline_property_byte_width_on_slot` (values.rs:793), replace the fail-closed rejection for `bucket_width == 0 && edge_inline_property_width == w && stored_slots > 0` with a call to `materialize_inline_property_stream` (inline fast path when small, deferred enqueue when large — the insert path returns Ok after enqueueing the step job; the insert itself completes AFTER materialization completes, so the insert that triggered the transition must be re-attempted by the caller once the stepped fill finishes — audit how deferred insert completion works today and, if a synchronous contract is required, gate the deferred path to the explicit maintenance entry instead and keep the insert-path materialize synchronous with the single-step threshold; record the decision). Keep fail-closed: width mismatch w1→w2, w→0, and tree-mode buckets with property edges (insert.rs:303 tree guard unchanged — LPB-in-tree is a later slice). Also wire `materialize_inline_property_stream` as a pub(crate) maintenance entry so the deferred worker can drive the stepped fill (`drain_vertex_edge_span_compact_queue`-style pop handling in deferred.rs:210's worker). Document in ADR 0088 §5 that slab width addition is now supported via materialize and tree-mode promotion keeps its carve-out."
    status: completed
  - id: "test-matrix"
    content: "Unit tests (cfg(not(feature = \"canbench\")), values.rs tests + deferred tests, mirroring existing style): (a) `materialize_zero_to_w_preserves_positions` — bucket with S edges incl. tombstones → materialize w=3 fill=0xAA → every live position reads 0xAA via `read_bucket_inline_property_bytes_for_slot`, tombstone positions are tombstone-inclusive (no shift), width/offset/slab_slots published correctly; (b) `materialize_stepped_resume` — force the stepped path (S × w above the threshold) → drive the queue to completion across multiple pops → identical final state to the inline fast path (property stream equality); (c) `materialize_atomic_on_failure` — failpoint on a byte-slab write mid-fill → bucket stays w=0, reserved span retired, no allocated-bytes drift (vertex accounting); (d) `materialize_fragmented_allocator_compacts_first` — fragment the allocator so the S×w request needs compaction → materialize triggers compaction then succeeds; (e) `materialize_rejects_w1_to_w2_and_teardown` — w≠0 source stays fail-closed; (f) `materialize_empty_bucket_fast_path` — schema_unset empty bucket keeps the existing direct width-set path (no stream job); (g) `width_addition_via_insert` — end-to-end: insert N edges at w=0, then insert an edge carrying w-byte inline property → materialize runs → the new edge's property bytes are readable and prior edges read `fill`; (h) `tree_bucket_property_insert_still_fail_closed` — the LPB-in-tree boundary holds. Deferred-worker test: the new MaintenanceWorkItem variant drains via the existing queue machinery (mirror the CompactVertexEdgeSpanV1 drain test pattern, deferred.rs:210)."
    status: completed
  - id: "wasm-budget-recheck"
    content: "Run `cargo build --release --target wasm32-unknown-unknown --features canbench` from `crates/ic-stable-lara/` and verify the exported-name budget stays ≤ 20,000 chars (baseline 14,818 / 5,182 headroom). Full green-bar matrix per commit: plain `cargo check -p ic-stable-lara`, `cargo test -p ic-stable-lara --no-default-features` (604 baseline + new), `cargo test -p ic-stable-lara --features canbench` (0 failed), `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings`, `cargo fmt --check -p ic-stable-lara`, one canbench spot-run with yml restore. Record the wasm char count in the completion report."
    status: completed
  - id: "adr-0088-update-and-validate"
    content: "Update `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` §5: mark `materialize_inline_property_stream` as implemented (slab form, stepped fill per ADR 0021; tree form deferred to LPB-in-tree), record the width-addition support (0→w on non-empty buckets) and the still-deferred transitions (w1→w2, w→0, tree-form). Run `python3 ~/.agents/skills/plan/scripts/validate_plan.py plans/0320-materialize-inline-property-stream.md --phase final` and confirm structurally-valid final-phase verdict before reporting completion."
    status: completed
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

- [x] `materialize_inline_property_stream` implemented (slab form, stepped fill, atomic publish) with the audit findings recorded in the plan.
- [x] Width addition 0→w supported for non-empty buckets through the insert-path ensure_* hooks; w1→w2 / w→0 / tree-mode property inserts remain fail-closed.
- [x] New `MaintenanceWorkItem` variant drains via the existing queue machinery; stepped and inline paths produce identical streams.
- [x] Full green-bar matrix green; wasm exported-name count recorded and ≤ 20,000.
- [x] ADR 0088 §5/§Status updated; `validate_plan.py --phase final` PASS.

## Later Slices (recorded, not in this plan)

- **LPB-in-tree**: tree-form materialization (property root + `ceil(S/K)` LPB blocks, `K = floor(4096/w)`) + removal of the promotion inline-property carve-out — reuses this slice's fill/cursor machinery; 6-8h estimate stands.
- **w1 → w2 re-encoding** and **w → 0 teardown**: separate deferred transitions per ADR 0088 §5.
- **Value backfill UX** above LARA (ADR 0008 profile SSOT; ADR 0058/0059 flavor).

## Step 0 — audit findings (2026-09-01, recorded BEFORE code)

Investigation of the byte-slab allocator (`self.values`,
`edge_inline_property.rs`), the per-bucket descriptor fields
(`LabelBucket`), the read paths (`values.rs:435-539`), the
`bucket_live_ordinal_at_edge_slot` mapping (values.rs:664), the
vertex-level accounting (values.rs:652-658, invariants.rs:73-91),
the insert-path hooks (values.rs:624, 793), and the existing
stepped maintenance pattern (`CompactVertexEdgeSpanV1`,
deferred.rs:23-87 / 1615-1650). All findings consistent with the
plan contract; no silent reinterpretation required.

### 0.1 Slab byte-stream layout (the read paths’ expectation)

The per-bucket inline property bytes stream is **not** a flat
singleton array. It has two regions:

- **Dense slab prefix** at `inline_property_bytes_offset` (u64),
  length = `inline_property_bytes_slab_slots × width` bytes. For
  position `i < slab_slots` the byte at slot `i` is at
  `offset + i × width` (values.rs:495-501, using
  `inline_property_bytes_byte_offset_at_slot`).
- **Append-log suffix** at the per-source inline-property-bytes
  log leaf (`inline_property_bytes_log_leaf(src)`). For position
  `i >= slab_slots` the byte is at the **ascending** log index
  `i - slab_slots` (values.rs:508-525, using
  `read_inline_property_bytes_log_asc_index`). The log chain head
  is `bucket.inline_property_bytes_log_head()`; the log length
  lives in `inline_property_bytes_log_len`.

**Implication for `materialize_inline_property_stream`**: on
publish, the bucket must be put in the **dense prefix only** state
(`inline_property_bytes_slab_slots = S`, `log_head = -1` /
`OVERFLOW_LOG_NONE`, `log_len = 0`). This is the same state that
`write_edge_inline_property_after_insert` (values.rs:543-573)
prefers (it only takes the log path when log_len > 0 or the span
is not at the slab tail). The "fresh dense stream, no log"
invariant in the plan contract is consistent with this.

### 0.2 The `bucket_live_ordinal_at_edge_slot` mapping

values.rs:664-693: for dense buckets
(`reserved_edge_slots == degree`), the mapping is the identity
(`edge_slot_index < degree` → `Some(edge_slot_index)`). For
sparse buckets it does a rank-resolution traversal.

**Implication for `materialize_inline_property_stream`**: after
materialize, the stream has `S = bucket.stored_slots` positions
matching edge positions 1:1 (tombstone-inclusive). For the dense
case (no tombstones) this is the identity; for the sparse case
the rank resolution at read time maps edge slot → ordinal, and the
stream at ordinal `i` corresponds to edge position `i` (the rank
of the i-th live edge in the bucket). Either way the position
contract holds: position i in the stream is the i-th live (or
slot-index-i-th edge) entry.

The `materialize` primitive only needs to write `S × w` bytes into
a single dense slab region; tombstone-aware read backfill stays
above LARA per ADR 0008 (the plan's "value backfill semantics
stay above LARA" line). Tombstone entries have their property
stream position filled with the `fill` byte — **not skipped** —
because the read path (`read_bucket_inline_property_bytes_for_slot`)
reads every position 1:1 with the bucket's edge slot order (dense
case).

### 0.3 The byte-slab allocator API

`InlinePropertyBytesSlab` (edge_inline_property.rs):

- `allocate_byte_span(len) -> Result<u64, GrowFailed>` (line 918):
  best-fit from the free list, falls through to `cap` (bump
  occupied tail). `allocate_byte_span_with_origin` (line 926)
  returns `(offset, used_free_origin: bool)`.
- `retire_byte_span(offset, len) -> Result<(), GrowFailed>` (line
  1025): insert into the free list (coalesces with adjacent
  free spans, may be deferred on fragmentation).
- `write_bytes(offset, bytes) -> Result<(), GrowFailed>` (line
  382): underlying `read/write_pages` path; `Memory::grow` on
  first write past `byte_capacity` (set by `set_byte_capacity`).
- `read_bytes(offset, out: &mut [u8])` (line 377): read path.
- `grow_byte_span_in_place(offset, old_len, new_len) -> Result<bool, GrowFailed>`:
  fast path for span-at-tail extension (used by the insert path,
  values.rs:898-906).
- `reserve_retired_byte_spans(additional) -> Result<(), GrowFailed>`:
  grow the free-span store (preflight before retiring).
- `append_byte_span(len) -> Result<u64, GrowFailed>`: bump
  occupied tail (used for first span of a bucket, values.rs:879).

**Implication for `materialize_inline_property_stream`**: the
primitive uses `append_byte_span(needed_bytes)` for the **first**
reservation (mirroring `ensure_bucket_inline_property_bytes_span`
values.rs:875-883), then `write_bytes(offset, &fill_buffer)` for
each fill step. The reservation is retried after
`compact_inline_property_bytes_slab` if the allocator reports
fragmentation (see 0.4).

### 0.4 The compaction interplay order

`inline_property_bytes_compaction_needed(requested_bytes)` (values.rs:246)
returns `true` when the allocator has enough aggregate free bytes
but no single retired span can satisfy the request. The insert
path (values.rs:876-883) calls `compact_inline_property_bytes_slab`
**once** when this returns `true` and a `inline_property_bytes_compaction_deferred`
flag is unset, then proceeds with `append_byte_span`.

**Implication for `materialize_inline_property_stream`**: mirror
this pattern. Before `append_byte_span(needed_bytes)`, check
`inline_property_bytes_compaction_needed(needed_bytes)`; if
`true` (and not deferred), call `compact_inline_property_bytes_slab()`
once and proceed. The compaction pass moves existing bucket
spans into earlier retired slots; **it does not change the
endpoint caller’s reservation** — `append_byte_span` runs after
compaction and may still need to bump the occupied tail.

### 0.5 The payload/width bound

The slab allocator addresses bytes via u64; the per-bucket
`inline_property_bytes_slab_slots` is a u32 and
`inline_property_byte_width` is a u16. The maximum byte count
per bucket is therefore `u32::MAX × u16::MAX = ~2^48` bytes,
which is well above any practical slab capacity. There is no
explicit "w must be ≤ W_MAX" typed bound in the representation
today; the only bound the plan contract needs to surface is
**the upper end of u16** (`w <= u16::MAX`, 65,535 bytes per
position). Larger widths are not representable and the existing
insert path would have already rejected them at the typed edge
boundary. The plan's "w <= payload constraint per ADR §5" maps to
**`u16::MAX`**, surfaced as `InlinePropertyBytesWidthMismatch`
when `w > u16::MAX` (typed; consistent with the existing reject
on tree-mode property edges). This is the new typed bound the
primitive must enforce.

### 0.6 The vertex-level accounting invariant

`LabeledVertex::inline_property_bytes_allocated_bytes` is the sum
of `bucket_resident_inline_property_bytes(bucket)` over the
vertex's buckets (invariants.rs:73-91 = `slab_slots × width`).
The sum is reconciled by
`reconcile_vertex_inline_property_bytes_allocated_bytes`
(values.rs:309-341), which is called after compact / retire paths.

**Implication for `materialize_inline_property_stream`**: on
publish, the vertex accounting must be increased by `S × w` bytes
(via `try_with_inline_property_bytes_allocated_bytes`). On
failure containment, the reserved span is retired and the
accounting is **not** changed (the reservation never published).
The existing `release_bucket_inline_property_bytes_span`
(values.rs:649-664) is the template for the saturating-subtract
pattern.

### 0.7 The stepped maintenance contract (ADR 0021)

`CompactVertexEdgeSpanV1 { vid, anchor_bucket_index, resume_bucket_index, resume_slot_index }`
(deferred.rs:23-37) is the canonical stepped cursor. The work
item carries `(vid, resume_position)` and is re-enqueued by the
worker (deferred.rs:1620-1651) with an advanced cursor until the
operation finishes. The queue entry is `pop_next` (deferred.rs:1292);
re-enqueue uses `requeue` (deferred.rs:1303) at the same or
`priority::RETRY` level.

**Implication for the new variant**:
`MaterializeInlinePropertyStreamV1 { vid, bucket_slot, width, fill_byte, resume_position }`.
`bucket_slot` validates that the work item is still relevant
(bucket hasn't been re-allocated). `resume_position` is the next
byte position to write (0..S×w). The worker pops, writes one
step's worth of fill bytes, advances `resume_position`, and
re-enqueues if not finished. On final step the worker publishes
the descriptor (width, offset, slab_slots, log reset) and
updates the vertex accounting, then drops the work item.

**Step threshold constant**: choose from audit. The bytecode for
`canbench` spot-run reports the existing
`CompactVertexEdgeSpanV1` does ~one slot step per pop. For
`materialize`, the per-step cost is `step_bytes` `write_bytes`
calls. A reasonable threshold: **the inline fast path is for
`S × w <= 4096` bytes (one VirtualMemory page)**; above that,
defer. This keeps the synchronous insert-path cost bounded
under the WASM instruction limit and matches the existing
`grow_byte_span_in_place` "tail-extend" fast-path granularity
(values.rs:898-906 uses a similar single-segment cost).

### 0.8 The insert-path synchronization contract

`ensure_bucket_inline_property_schema_for_insert`
(values.rs:624-636) is called from `insert.rs:287-292` BEFORE the
slab/tree dispatch (line 296-303). Today it rejects all
mismatches with `InlinePropertyBytesWidthMismatch`. To wire
width addition:

- For `bucket_width == 0 && edge_width == w && stored_slots > 0`
  (the materialize-eligible case), call
  `materialize_inline_property_stream(bucket_slot, w, fill_byte)`.
  The `fill_byte` is `0u8` for this contract (caller has not
  provided per-position values yet; backfill is above LARA).
- The insert path must see the bucket updated **before** the
  edge is written. Two design options:
  - **Synchronous materialize** (S × w ≤ threshold): call
    `materialize_inline_property_stream` inline, return the
    updated bucket, then proceed. The insert completes in one
    call. ✅ Keeps the existing insert contract (synchronous
    success/failure, no deferred retry needed by the caller).
  - **Deferred materialize** (S × w > threshold): call
    `materialize_inline_property_stream` which enqueues a
    `MaterializeInlinePropertyStreamV1` work item. The primitive
    returns `Ok(())` but the bucket descriptor is **not yet
    published** (it stays at `width=0`). The insert that
    triggered the transition must be re-attempted after the
    stepped fill completes — but the insert API today has no
    "re-try me after this work item finishes" mechanism.

**Decision recorded in todo `width-addition-wiring`**: the
synchronous fast-path is the only path. For the deferred (large)
case, the **insert that triggered the transition fails with the
typed `InlinePropertyBytesWidthMismatch` error** (same as today).
The caller can then drive `materialize_inline_property_stream`
via the explicit maintenance entry (the new pub(crate) entry
exposed in `deferred.rs`), wait for the work item to drain, and
retry the insert. This keeps the insert-path contract
synchronous and does not introduce a deferred-insert completion
mechanism. The primitive is still the *single source of truth*
for the byte-slab allocation + fill + publish — the only
difference is who calls it (insert path inline for small,
caller for large).

This is the "gate the deferred path to the explicit maintenance
entry" branch of the plan's "if a synchronous contract is
required" sentence. Decision is recorded here so Step 2
implements it without ambiguity.

### 0.9 The tree-mode carve-out (LPB-in-tree boundary)

`insert.rs:296-303` already rejects property edges on tree-mode
buckets with `InlinePropertyBytesWidthMismatch`. The plan
preserves this. The primitive's first precondition (slab-mode
bucket) keeps the carve-out: a tree bucket calling
`materialize_inline_property_stream` returns
`InlinePropertyBytesWidthMismatch` (or a new dedicated typed
error; reusing the existing one keeps the production error
surface unchanged). The LPB-in-tree follow-up will introduce
the tree-form primitive variant; this plan does not.

### 0.10 The read-after-publish contract

After publish, `read_bucket_inline_property_bytes_for_slot` (line 435)
reads from the dense slab prefix (log_head < 0 branch, line 491-501).
All `S` positions return `fill_byte` (set by the primitive).
Live positions are then backfilled by the caller above LARA via
the existing per-slot write API (`write_edge_inline_property_to_log`
for log-backed writes, or `write_edge_inline_property_at_slot` for
tail-slab writes, values.rs:567-573). The primitive does **not**
need to call any per-slot write API — it just writes the
sentinel `fill_byte` everywhere in the reserved region.

### 0.11 What the plan does NOT cover (confirmed)

- `w1 → w2` re-encoding: out of scope (typed
  `InlinePropertyBytesWidthMismatch` on non-zero source width).
- `w → 0` teardown: out of scope (typed
  `InlinePropertyBytesWidthMismatch` on target width 0).
- Tree-form materialization: out of scope (LPB-in-tree).
- Value backfill semantics: out of scope (above LARA).
- Per-position backfill above LARA: out of scope (caller
  responsibility; the primitive only writes the `fill` sentinel).

### 0.12 The deferred worker entry (where to wire the stepped
fill in `deferred.rs`)

The `process_maintenance_step` dispatch is the single place
where work items are turned into `Ok(Some(next))` (re-enqueue)
or `Ok(None)` (drop). The new variant gets one arm:

- `MaterializeInlinePropertyStreamV1 { vid, bucket_slot, width, fill_byte, resume_position }`:
  - Re-read the bucket from `bucket_slot`; if missing, drop
    (work item stale; wid re-allocated).
  - Compute `total_bytes = S × w`. If `resume_position >= total_bytes`,
    publish the descriptor (`width`, `offset = reserved`, `slab_slots = S`,
    `log_head = -1`, `log_len = 0`), update vertex accounting
    (`+ total_bytes`), and drop the work item.
  - Otherwise, write a step's worth of `fill_byte` to
    `reserved + resume_position..min(resume_position + STEP_BYTES, total_bytes)`,
    advance `resume_position`, re-enqueue at `Retry` priority
    (same pattern as `CompactVertexEdgeSpanV1`).
  - On write error, set `stalled = true` and re-enqueue (ADR 0020
    retry-never-drop).

The `STEP_BYTES` constant is the same as the inline fast-path
threshold (4096 bytes, from 0.7). This keeps the worker
behavior identical to a continuation of the inline path.

### 0.13 Stepped vs inline divergence risk

The two paths must produce **byte-identical** streams. The risk
is: stepped path writes one step at a time using `write_bytes`,
inline path writes the whole `S × w` block in one `write_bytes`.
Both target the same `offset` and fill the same bytes; the only
observable difference would be a partial-write failure mid-step
(handled by the step's `?` propagation and `stalled = true`).
The test (b) `materialize_stepped_resume` enforces equality by
running the inline path on bucket A and the stepped path on
bucket B with the same inputs and comparing the two streams.

### 0.14 The `width` validation order

`materialize_inline_property_stream` must check in this order
(fail-closed at the first failure):

1. `w > 0` (else `InlinePropertyBytesWidthMismatch` — but really
   this is "w must be > 0", the typed error is fine).
2. `w <= u16::MAX` (always true for u16; we type it as u16).
3. `bucket.is_tree_mode() == false` (typed
   `InlinePropertyBytesWidthMismatch` reusing the existing
   typed error).
4. `bucket.inline_property_byte_width() == 0` (typed
   `InlinePropertyBytesWidthMismatch` — w1→w2 fail-closed).
5. `bucket.stored_slots > 0` (typed `InlinePropertyBytesWidthMismatch`
   — empty bucket is the schema_unset fast path; the caller should
   use `ensure_bucket_inline_property_byte_width_on_slot` for
   that case, not `materialize`).

Then the reserve + fill + publish phases.

### 0.15 Open question deferred to Step 2

The decision in 0.8 (insert-path synchronous-only) means a
large-bucket insert with a new property width is rejected with
the typed error. The caller is expected to drain the maintenance
queue and retry. Should the typed error be the existing
`InlinePropertyBytesWidthMismatch` or a new
`MaterializeInlinePropertyStreamDeferred` variant?

**Decision**: keep the existing typed error. The error semantics
("the requested width cannot be applied synchronously") is
covered by the message; the caller can introspect the bucket
state after the maintenance drain to confirm the materialize
completed, then retry the insert. Adding a new error variant
would expand the public error surface for a path the caller is
expected to handle out of band; the current surface already
covers the contract.

---

## Status

**Plan 0320 implemented (Steps 1-3, 2026-09-01).** Commits:
`772960947` (Step 1, primitive + 6 unit tests), `c5ae29a97` (Step 2,
width-addition wiring + new `MaintenanceWorkItem::MaterializeInlinePropertyStreamV1`
+ drain test). Step 3 docs/ADR/validator: completed. wasm exported-name
chars 14,818 (no change from Step 1); full green-bar matrix
maintained through every commit.
- **Plan 0321** (batch admission widening) and the 1M PocketIC sweep: unchanged order.