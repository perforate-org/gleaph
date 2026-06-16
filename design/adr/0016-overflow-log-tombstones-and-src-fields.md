# 0016. Overflow log tombstones and `src` field layout review

Date: 2026-06-15  
Status: accepted (phase 1 and phase 2 implemented)  
Last revised: 2026-06-16
Anchor timestamp: 2026-06-16 01:28:06 UTC +0000

## Context

LARA edge storage has two physical locations for a labeled edge row:

| Location | Owner | Delete representation (implemented) |
|----------|-------|-------------------------------------|
| Edge slab | `EdgeStore` / labeled bucket span | In-place tombstone edge payload |
| Edge overflow log | `LogStore` (`LLG`) | In-place tombstone edge payload; `prev` chain unchanged |

Payload bytes mirror the edge row order:

| Location | Owner | Current layout |
|----------|-------|----------------|
| Payload slab | `EdgePayloadStore` (`payload_slab`) | Byte CSR, indexed by the same logical edge slot |
| Payload overflow log | `PayloadLogStore` (`LVL`) | `prev: i32`, `src: i32`, `payload_cell: [u8; 8]` |
| Payload blobs | `payload_blobs` | Wide overflow payload body keyed by `(leaf_segment, entry_idx)` |

Current implementation facts:

- Edge overflow log entries store `prev_offset` (4 B), `src` (4 B), and edge bytes
  ([`edge/log.rs`](../../crates/ic-stable-lara/src/lara/edge/log.rs)).
- **Implemented (2026-06-15):** log-backed delete rewrites the target entry's edge payload to the
  tombstone contract and keeps the entry in the chain. Foreground delete no longer appends separate
  delete-target log entries. Scans skip tombstone edge rows in both slab and log locations.
- **Legacy read path:** `LOG_SRC_DEAD`, `DeleteTarget::Slab`, and `DeleteTarget::Log` encodings in
  the `src` word remain decodable for older log chains
  ([`edge/targets.rs`](../../crates/ic-stable-lara/src/lara/edge/targets.rs)).
- **Implemented (2026-06-16):** payload overflow log entries are 16 B (`LVL`, layout version 1):
  `prev` (4 B), `src` (4 B), and an untagged 8 B inline cell
  ([`edge_payload/log.rs`](../../crates/ic-stable-lara/src/lara/edge_payload/log.rs),
  [`edge_payload/cell.rs`](../../crates/ic-stable-lara/src/lara/edge_payload/cell.rs)).
  Inline vs blob on the log is derived from `LabelBucket::payload_byte_width`; wide bodies live in
  `payload_blobs` at `(leaf_segment, entry_idx)`.

DGAP stores a source-like field in its log entry (`u`) next to destination (`v`) and `prev_offset`,
but ordinary traversal is anchored from the owning vertex row and follows `prev_offset` while
emitting `v`. DGAP is therefore evidence that a source field can exist in the physical log format,
not proof that it must be scan-critical for every derived LARA layout.

## Problem

The current log delete model has two avoidable costs.

### 1. Delete information is represented as extra log history

For slab-backed edges, deletion can be represented directly by the edge slot's tombstone payload.
For log-backed edges, the current model can represent deletion as a separate log entry that points at
the deleted slab or log target.

That creates a second source of truth for delete state:

```text
edge slot payload tombstone
delete/dead metadata in log `src`
```

Scan, replay, payload-first traversal, and maintenance must then interpret both sources in the same
order. This widens the invariant surface and makes replay bugs easier to introduce.

### 2. `src` carries several concepts

The edge log `src` word currently carries:

- the source vertex id for live entries,
- dead entry state,
- delete target encoding for deferred deletes.

The payload log also stores a `src` word even though payload identity is tied to the same leaf and
entry index as the edge log, and blob identity is derived from `(leaf_segment, entry_idx)`.

This raises three layout questions:

1. Does the edge body log still need a physical `src` word after delete state moves into the edge
   payload tombstone contract?
2. Does the payload log need a full `src` word, or can that word become `src_and_tag` so the payload
   entry shrinks from 17 B to 16 B?
3. Does the payload log cell need per-entry inline/blob tags when the label bucket already declares
   `payload_byte_width`?

### 3. Payload log cells duplicated bucket schema (resolved)

Earlier design drafts stored inline/blob tags and duplicated blob width in the log cell even though
`LabelBucket::payload_byte_width` is already the schema for every slot in that bucket. The
implemented layout derives inline vs blob from bucket width on read and write; no per-cell tags.

## Existing Architecture Assessment

The existing storage domains can own this change. No new storage subsystem is required.

| Boundary | Owner | Source of truth after this ADR |
|----------|-------|--------------------------------|
| Edge liveness | Edge row payload | Slab and log entries both expose the same tombstone-edge contract |
| Edge slot identity | Labeled bucket scan order | Deleting one edge must not change surviving edge slot indices |
| Payload identity | Edge slot plus label bucket payload width | Payload log site mirrors the edge log site; blob body remains keyed by log site |
| Payload storage class | Label bucket schema (`payload_byte_width` + profile encoding) | Inline vs blob on the payload log is derived from bucket schema, not stored per cell |
| Log reclamation | Maintenance / rewrite path | Tombstoned log entries are reclaimed only by fold, rebalance, or compaction |
| Derived state | Graph mutation path | Edge aliases, postings, and payloads update from canonical edge delete once |

The critical invariant is:

```text
Deleting an edge must not change the slot index of any surviving edge.
```

Slot identity is observed outside the physical log by edge handles, reverse aliases, local edge
postings, payload-first phase-two lookups, and traversal cursors. Rewiring log chains to remove a
middle entry would change the ordinal of later log-backed edges unless slot identity were redefined
around physical log entry ids. This ADR does not choose that larger redesign.

## Decision

Adopt the following target policy for future implementation.

### 1. Do not model deletion as a separate delete log entry

Delete state belongs to the deleted edge row itself.

- If the edge body is on the slab, write the tombstone edge payload in that slab slot.
- If the edge body is in the overflow log, rewrite that log entry's edge payload to the tombstone
  value and keep the log entry in the chain.
- Do not rewire `prev` links merely to hide a deleted entry from traversal.
- Do not compact payload slab bytes as part of the foreground delete path.

Scans skip tombstone edge rows in both slab and log locations. Maintenance may later fold or reclaim
tombstoned rows while preserving the externally visible edge order for surviving rows.

### 2. Treat payload deletion as subordinate to edge liveness

Payload bytes are not the canonical liveness source.

- If the edge body is tombstoned, traversal must ignore that edge even if old payload bytes remain.
- If the payload body is in the payload log, mark or clear the payload log entry at the same logical
  log site without rewiring the payload log chain.
- If the payload body is in `payload_blobs`, drop the blob body for that log site.
- If the payload body is in the payload slab, foreground delete may leave bytes in place. Those bytes
  are unreachable because the edge row is tombstoned; maintenance can reclaim or rewrite them later.

This keeps edge body liveness as the single source of truth and avoids a second payload-specific
delete contract.

### 3. Review the necessity of edge log `src`

After delete state moves into the edge payload tombstone contract, the edge log `src` word should be
re-evaluated before keeping it as permanent layout.

The review must answer:

- Is `src` required by core `LaraGraph` APIs that scan a generic log without labeled-bucket context?
- Is `src` required for validation, diagnostics, reopen checks, or maintenance recovery?
- Can labeled edge logs derive owner context from the bucket/vertex chain and keep core LARA
  unchanged?
- Would removing or repurposing `src` create a second layout concept between core and labeled LARA
  that is harder to maintain than the bytes it saves?

Until that review lands, the safer implementation path is:

```text
first: move log-backed delete state to tombstone edge payloads
then: benchmark and review whether `src` can be removed or repurposed
```

### 4. Derive payload inline/blob from bucket schema, not per-cell tags

Do not store inline vs blob storage class in the payload slab or payload log cell.

**Schema source of truth:** `LabelBucket::payload_byte_width`, plus (when added) the label's
`EdgePayloadProfile.encoding` for variable-length payloads.

**Location-specific resolution:**

```text
on payload slab(slot):
  read payload_byte_width bytes at the slot byte offset

on payload log(leaf, entry_idx) with bucket context:
  if payload_byte_width == 0           → no payload
  if encoding is variable-length       → blob at (leaf, entry_idx)   [future]
  if payload_byte_width <= 8           → inline bytes in the 8 B cell
  else                                 → blob at (leaf, entry_idx)
```

Notes:

- The payload **slab** never uses the blob map; wide fixed-width payloads live directly in the byte
  CSR regardless of width.
- The payload **log** uses the blob map only when the fixed width exceeds the 8 B inline cell.
- Blob identity remains `(leaf_segment, entry_idx)`; blob body width comes from the bucket, not the
  cell.
- Foreground insert already rejects `edge_payload_byte_width != bucket.payload_byte_width`, so
  storage class does not vary per slot within one bucket.

Per-cell inline/blob tags and duplicated blob widths are not stored on the wire.

### 5. Payload log 16 B with an untagged 8 B cell (implemented)

The payload log entry (`LVL`, layout version 1) is:

```text
prev: i32
src: i32          // LOG_SRC_DEAD for dead payload log entries
payload_cell: [u8; 8]
```

Design constraints:

- `payload_cell` holds up to 8 B of inline payload when bucket schema says inline-on-log; it is
  otherwise ignored and the blob map owns the body.
- `src` carries dead/empty payload log entry state via `LOG_SRC_DEAD` (and any future small
  metadata). Edge liveness remains primary; payload dead bits are secondary for log-chain walks that
  lack edge context. A future `src_and_tag` rename is still open if the edge log `src` word is
  repurposed.
- Do not put inline/blob class bits in `payload_cell`; derive class from bucket schema at read time.
- Prefer tag bits in `src_and_tag`, not `prev`, because `prev` is the chain pointer and should remain
  a pure traversal primitive.

Variable-length payloads (not implemented in LARA storage as of 2026-06-16) require an additional
profile flag; when present, log-backed payloads always use the blob map regardless of
`payload_byte_width`.

## Benchmark Gate

Changes to log entry layout and scan replay affect storage, traversal, and payload-first execution.
Before accepting implementation of `src` removal or payload log 16 B compression, run focused
benchmarks that separate setup, mutation, scan, and payload attach costs.

Required benchmark coverage:

| Path | Benchmark signal |
|------|------------------|
| Same-label overflow insert | Whether smaller entries improve append-heavy log pressure |
| Same-label scan | Whether tombstone skipping and tag decoding affect hot traversal |
| Payload attach scan | Whether 16 B payload entries improve stable-memory IO enough to matter |
| Payload-first phase 1/2 | Whether cached replay and slot-to-log lookup stay neutral or faster |
| Tombstone-heavy delete/rewrite | Whether foreground delete stays cheap and maintenance cost remains bounded |

Existing candidate benches:

- `bench_labeled_mixed_label_hub_insert_33x50`
- `bench_labeled_mixed_label_hub_scan_33x50`
- `bench_labeled_mixed_label_hub_asc_iter_33x50`
- `bench_labeled_for_each_edges_for_label_48_x51`
- payload-first benches listed in `design/storage/payload-first-traversal.md`

Likely new focused benches (added 2026-06-16):

- `bench_labeled_payload_log_scan_8b_inline_overflow` — **implemented**
- `bench_labeled_payload_first_log_backed_selective_match` — **implemented** (`graph`: `bench_graph_payload_first_log_backed_selective_match`)
- `bench_labeled_tombstone_log_delete_then_scan` — **implemented**
- `bench_labeled_tombstone_log_rewrite_maintenance` — **implemented**

Benchmark acceptance should compare against the current implementation and must not disable
tombstone handling, payload blob cleanup, alias maintenance, or derived-state updates unless the
benchmark explicitly says it is measuring a lower-level isolated primitive.

**Status (2026-06-16):** focused benches below are implemented and baselined via canbench.
Edge log `src` removal remains gated on reviewing these results.

**Status (2026-06-16):** focused benches below are implemented and baselined via canbench.
Edge log `src` wire removal is **deferred** after review (see below); payload log 16 B compression
is accepted.

## Edge log `src` review (2026-06-16)

Benchmark gate complete. Code review of current `LLG` `src` word usage:

| Question | Finding |
| -------- | ------- |
| Required for core scan without labeled context? | **No for neighbor emission.** Scans anchor on the owning vertex row (`log_head`) and walk `prev`. `src` is decoded only for entry kind (`Live` / `Dead` / legacy `Delete`). |
| Required for validation or reopen? | **Partially.** Negative `src` encodes `LOG_SRC_DEAD` and legacy `DeleteTarget` replay. Labeled paths also treat `src < 0` as a fast dead check alongside edge-payload tombstones ([`traverse.rs`](../../crates/ic-stable-lara/src/labeled/graph/traverse.rs), [`remove.rs`](../../crates/ic-stable-lara/src/labeled/graph/remove.rs)). |
| Is live owner vertex id in `src` read on hot paths? | **No.** Live inserts write `log_owner` into `src` ([`log_mut.rs`](../../crates/ic-stable-lara/src/lara/edge/log_mut.rs)), but replay/scan never validates or uses that id after `decode_log_entry_kind` returns `Live`. |
| Can labeled derive owner without per-entry `src`? | **Yes** for current insert paths: `log_owner = vertices.log_leaf_vertex(vid)` at insert time; leaf segment is derived from the vertex row, not from log entry `src`. |
| Cost of removing the word now? | **Layout migration.** `LLG` stride is `8 + edge_stride`; dropping `src` needs a new layout version and dual-read or fresh-store policy. DGAP keeps a source-like `u` field; LARA mirrors it on wire today. |

**Decision:** keep the edge log `src` word for now.

- Foreground delete liveness is already on the edge payload tombstone; `src` is not a second
  liveness source for new chains.
- The word still carries `LOG_SRC_DEAD`, legacy delete-target replay, and (redundantly) live owner
  vertex id on write.
- Physical removal or `src_and_tag` repurposing is a **follow-up layout change**, not required for
  phase 1/2 correctness. Revisit only if a future `LLG` stride reduction benchmark shows material
  stable-memory win.

## Alternatives Considered

### A. Keep separate delete log entries

Rejected as the long-term model. It preserves the current implementation shape, but leaves delete
state split across edge payloads and log metadata. Replay and payload-first traversal must keep
interpreting historical delete entries correctly.

### B. Remove deleted log entries by rewiring `prev`

Rejected for foreground delete. It can make the log chain look cleaner, but it risks changing the
slot index of surviving log-backed edges. That would push updates into aliases, postings, cursors,
and payload slot resolution.

### C. Redefine log-backed slot identity as physical log entry id

Deferred. This could make chain rewiring possible, but it is a larger identity redesign. It would
need a separate ADR covering edge handles, reverse aliases, index postings, traversal order,
payload-first phase two, and maintenance rewrite semantics.

### D. Move only payload log tags to `prev`

Rejected unless later evidence proves `src_and_tag` is impossible. `prev` owns chain topology.
Packing unrelated state into `prev` would make chain walking and corruption checks harder to reason
about.

### E. Compress payload log immediately because the design doc already says 16 B

Rejected. The design doc was ahead of implementation. This ADR requires an explicit layout review
and benchmark gate before changing stable bytes.

### F. Keep per-cell inline/blob tags in `PayloadLogCell`

Rejected for the target layout. Tags duplicate bucket schema, force read paths to branch on cell
bytes instead of bucket context, and consume a byte that prevents 16 B entries. The write path
already derives inline vs blob from `payload_byte_width`; phase 2 aligns the read path and wire
layout with that model. Legacy tagged cells remain readable until layout version migration.

## Consequences

Positive effects:

- One liveness source: the edge row tombstone contract.
- Foreground delete no longer needs delete-target log history.
- Surviving edge slot indices stay stable after deletion.
- Payload bytes remain subordinate to edge liveness, reducing duplicate delete rules.
- Payload log 16 B compression has a clear path without mixing tag state into `prev`.
- One schema source for inline vs blob on the payload log: bucket `payload_byte_width` (+ profile).

Trade-offs:

- Tombstoned log entries remain in chains until maintenance folds or rewrites them.
- Scans must skip tombstone entries in both slab and log locations.
- Foreground deletes may leave unreachable payload slab bytes until maintenance.
- Edge log `src` removal is not decided here; keeping it may leave bytes on the table, while
  removing it too early could split core and labeled LARA layout concepts.
- Payload log reads require bucket context (or cached bucket width) to interpret log cells; low-level
  log walks without label context cannot infer inline vs blob from cell bytes alone.

## Implementation status (2026-06-16)

Phase 1 (implemented 2026-06-15):

1. Log-backed delete rewrites the target log entry as a tombstone edge payload (`rewrite_overflow_log_entry_tombstone`).
2. Slab-backed delete on log rows writes the slab tombstone directly (no delete-target append).
3. Scan/replay paths skip tombstone log entries; legacy delete-target replay remains for old chains.
4. Payload log chains stay aligned with edge log chains; payload bodies are cleared without rewiring.

Phase 2 (implemented 2026-06-16):

1. Payload log layout version 1: 16 B stride (`prev`, `src`, 8 B untagged cell).
2. Inline vs blob derived from `LabelBucket::payload_byte_width` on read and write; no per-cell tags.
3. Wide log-backed payloads store body in `payload_blobs` at `(leaf_segment, entry_idx)`; log cell is zero.

Benchmark gate (implemented 2026-06-16):

- `bench_labeled_payload_log_scan_8b_inline_overflow` — 4.67 M ix (hybrid payload attach)
- `bench_labeled_tombstone_log_delete_then_scan` — 3.89 M ix
- `bench_labeled_tombstone_log_rewrite_maintenance` — 40.64 M ix (edge overflow hub + incremental compact)
- `bench_graph_payload_first_log_backed_selective_match` — 698 K ix (48+24 overflow hub expand)

Edge log `src` review (2026-06-16): **complete; wire removal deferred** (see review section).

Deferred:

- Edge log `src` physical removal or `src_and_tag` repurposing (`LLG` layout version bump).
- Variable-length payload encoding flag (profile) → always blob on log; not in current storage.

Tests should cover:

- slab-backed edge delete,
- log-backed edge delete,
- payload-in-log delete,
- payload-in-slab delete,
- payload blob cleanup,
- payload-first traversal after log-backed delete,
- alias/posting stability when a middle log-backed edge is deleted.

## Design Documentation Impact

Documents to update when this ADR is implemented:

| Document | Required update |
|----------|-----------------|
| `design/storage/labeled-edge-payloads.md` | **Updated 2026-06-16:** `LVL` layout version 1, 16 B entry; bucket-derived storage class |
| `design/storage/lara-dgap-contract.md` | Record log tombstone policy and DGAP divergence |
| `design/storage/payload-first-traversal.md` | **Updated 2026-06-16:** bucket-derived log attach for sparse overflow |
| `design/storage/stable-memory-inventory.md` | Note `LVL` layout version 1 when revisiting region docs |

## Related

- [ADR 0001: Labeled edge physical layer uses PMA leaf segment slide](0001-labeled-segment-slide.md)
- [ADR 0007: Stable-memory layout policy and measured consolidation](0007-stable-memory-layout.md)
- [ADR 0008: Edge payload profile schema: router SSOT](0008-edge-payload-profile-router-ssot.md)
- [Labeled edge payload storage](../storage/labeled-edge-payloads.md)
- [LARA storage contract (DGAP alignment)](../storage/lara-dgap-contract.md)
- [Payload-first traversal](../storage/payload-first-traversal.md)
