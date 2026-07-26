# 0016. Overflow log tombstones and `src` field layout review

Date: 2026-06-15  
Status: accepted (phases 1–3 implemented)  
Last revised: 2026-07-14
Anchor timestamp: 2026-07-14 02:55:40 UTC +0000

Inline-property-bytes-liveness portions of this ADR are amended by [ADR 0001](./0001-labeled-segment-slide.md): edge and inline property bytes physical slots/logs are independent, and inline property bytes deletion now removes the bucket-local live ordinal rather than relying on a paired edge-log entry.

## Context

LARA edge storage has two physical locations for a labeled edge row:

| Location | Owner | Delete representation (implemented) |
|----------|-------|-------------------------------------|
| Edge slab | `EdgeStore` / labeled bucket span | In-place tombstone edge inline property |
| Edge overflow log | `LogStore` (`LLG`) | Tombstone-free direct unlink; the `prev` chain preserves newest-to-oldest scan order |

Inline property bytes preserve bucket-local live order through an independent physical layout:

| Location | Owner | Current layout |
|----------|-------|----------------|
| Inline property bytes slab | `EdgeInlinePropertyBytesStore` (`inline_property_bytes_slab`) | Dense live-value sequence with its own slab-slot count |
| Payload overflow log | `InlinePropertyBytesLogStore` (`LVL`) | `prev: i32`, `inline_property_bytes_cell: [u8; 8]` |
| Inline property bytes blobs | `inline_property_bytes_blobs` | Wide overflow inline property bytes body keyed by `(leaf_segment, entry_idx)` |

Current implementation facts:

- Edge overflow log entries store `prev` (4 B) and edge bytes only
  ([`edge/log.rs`](../../crates/ic-stable-lara/src/lara/edge/log.rs)). Liveness is encoded in the
  edge inline property tombstone contract; there is no per-entry `src` word on `LLG`.
- **Superseded on 2026-07-14:** labeled log-backed delete no longer writes a tombstone. It advances
  the bucket head or rewrites the one newer entry whose `prev` points to the target. Existing stable
  tombstones remain readable and are reclaimed by maintenance for compatibility.
- **Implemented (2026-06-16):** inline property bytes overflow log entries are 12 B (`LVL`, layout version 1):
  `prev` (4 B) and an untagged 8 B inline cell
  ([`edge_inline_property/log.rs`](../../crates/ic-stable-lara/src/lara/edge_inline_property/log.rs),
  [`edge_inline_property/cell.rs`](../../crates/ic-stable-lara/src/lara/edge_inline_property/cell.rs)).
  Inline vs blob on the log is derived from `LabelBucket::inline_property_byte_width`; wide bodies live in
  `inline_property_bytes_blobs` at `(leaf_segment, entry_idx)`. The inline property bytes log is an independently maintained
  ordered suffix; there is no per-entry `src` word on `LVL`.

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

Scan, replay, inline-property-bytes-first traversal, and maintenance must then interpret both sources in the same
order. This widens the invariant surface and makes replay bugs easier to introduce.

### 2. `src` carries several concepts

The edge log `src` word currently carries:

- the source vertex id for live entries,
- dead entry state,
- delete target encoding for deferred deletes.

The inline property bytes log also stores a `src` word even though inline property bytes identity is tied to the same leaf and
entry index as the edge log, and blob identity is derived from `(leaf_segment, entry_idx)`.

This raises three layout questions:

1. Does the edge body log still need a physical `src` word after delete state moves into the edge
   inline property bytes tombstone contract?
2. Does the inline property bytes log need a full `src` word, or can that word become `src_and_tag` so the inline property bytes
   entry shrinks from 17 B to 16 B?
3. Does the inline property bytes log cell need per-entry inline/blob tags when the label bucket already declares
   `inline_property_byte_width`?

### 3. Inline property bytes log cells duplicated bucket schema (resolved)

Earlier design drafts stored inline/blob tags and duplicated blob width in the log cell even though
`LabelBucket::inline_property_byte_width` is already the schema for every slot in that bucket. The
implemented layout derives inline vs blob from bucket width on read and write; no per-cell tags.

## Existing Architecture Assessment

The existing storage domains can own this change. No new storage subsystem is required.

| Boundary | Owner | Source of truth after this ADR |
|----------|-------|--------------------------------|
| Edge liveness | Edge row payload | Slab and log entries both expose the same tombstone-edge contract |
| Edge slot identity | Labeled bucket scan order | Direct unlink shifts only newer suffix ordinals and reports every resulting move |
| Inline property bytes identity | Bucket-local live ordinal plus label bucket inline property byte width | Inline property bytes slab/log maintain an independent ordered sequence; blob body remains keyed by inline property bytes log site |
| Inline property bytes storage class | Label bucket schema (`inline_property_byte_width` + profile encoding) | Inline vs blob on the inline property bytes log is derived from bucket schema, not stored per cell |
| Log reclamation | Foreground delete plus maintenance | New log deletes unlink immediately; maintenance only folds live suffixes and removes legacy tombstones |
| Derived state | Graph mutation path | Edge aliases, postings, and inline property bytes update from canonical edge delete once |

The critical invariant is:

```text
Deleting a log-backed edge may shift only the newer overflow suffix, and every move is reported
before the mutation completes.
```

Slot identity is observed outside the physical log by edge handles, reverse aliases, local edge
postings, inline-property-bytes-first phase-two lookups, and traversal cursors. Middle-node unlink renumbers
the newer suffix by one. `EdgeRemoval::moves` carries that bounded move batch to Graph sidecars and
index postings; its size is at most the leaf log capacity minus one.

## Decision

Adopt the following target policy for future implementation.

### 1. Do not model deletion as a separate delete log entry

Delete state belongs to the deleted edge row itself.

- If the edge body is on the slab, write the tombstone edge inline property in that slab slot.
- If the edge body is the overflow head, update the bucket head to `head.prev`.
- Otherwise rewrite the one newer entry whose `prev` points to the target so it points to
  `target.prev`.
- Return one `EdgeSlotMove` for each live entry newer than the target; each slot shifts down by one.
- Do not compact inline property bytes slab bytes as part of the foreground delete path.

Overflow deletion is therefore O(chain lookup + one fixed-width link-owner write), leaves no new log
tombstone, and preserves newest-to-oldest scan order. Move notification cost is bounded by the
170-entry shared leaf log rather than vertex degree. Rebalance, resize, and relocation may fold the
remaining live chain. Existing slab tombstones and legacy log tombstones are compacted by maintenance.

### 2. Keep edge liveness canonical while maintaining inline property bytes order independently

Inline property bytes are not the canonical liveness source.

- Resolve the edge physical slot to its bucket-local live ordinal before the tombstone commit.
- If inline property bytes exist, fold the inline property bytes log when necessary and remove the same live ordinal while
  shifting the newer inline property bytes suffix, preserving edge/value scan order.
- Inline property bytes slab/log capacity and maintenance remain independent from edge slab/log capacity and
  maintenance. Either maintenance order must preserve observed edge/value pairs.
- Width-zero labels allocate no inline property bytes slab or log entries.

This keeps edge body liveness canonical without making inline property bytes physical consistency depend on edge
log residency.

### 3. Review the necessity of edge log `src`

After delete state moves into the edge inline property tombstone contract, the edge log `src` word should be
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
first: move log-backed delete state to tombstone edge inline propertys
then: benchmark and review whether `src` can be removed or repurposed
```

### 4. Derive inline property bytes inline/blob from bucket schema, not per-cell tags

Do not store inline vs blob storage class in the inline property bytes slab or inline property bytes log cell.

**Schema source of truth:** `LabelBucket::inline_property_byte_width`, plus (when added) the label's
`EdgeInlinePropertyProfile.encoding` for variable-length inline property bytes.

**Location-specific resolution:**

```text
on inline property bytes slab(slot):
  read inline_property_byte_width bytes at the slot byte offset

on inline property bytes log(leaf, entry_idx) with bucket context:
  if inline_property_byte_width == 0           → no inline property bytes
  if encoding is variable-length       → blob at (leaf, entry_idx)   [future]
  if inline_property_byte_width <= 8           → inline bytes in the 8 B cell
  else                                 → blob at (leaf, entry_idx)
```

Notes:

- The inline property bytes **slab** never uses the blob map; wide fixed-width inline property bytes live directly in the byte
  CSR regardless of width.
- The inline property bytes **log** uses the blob map only when the fixed width exceeds the 8 B inline cell.
- Blob identity remains `(leaf_segment, entry_idx)`; blob body width comes from the bucket, not the
  cell.
- Foreground insert already rejects `edge_inline_property_byte_width != bucket.inline_property_byte_width`, so
  storage class does not vary per slot within one bucket.

Per-cell inline/blob tags and duplicated blob widths are not stored on the wire.

### 5. Inline property bytes log 12 B with an untagged 8 B cell (implemented)

The inline property bytes log entry (`LVL`, layout version 1) is:

```text
prev: i32
inline_property_bytes_cell: [u8; 8]
```

Design constraints:

- `inline_property_bytes_cell` holds up to 8 B of inline property bytes when bucket schema says inline-on-log; it is
  otherwise ignored and the blob map owns the body.
- Liveness on the inline property bytes log is **not** stored in the log entry. The inline property bytes sequence follows
  bucket-local live ordinals independently from edge slab/log residency. Foreground delete resolves
  and removes the inline property bytes ordinal before tombstoning the canonical edge; unreachable log/blob bytes
  may remain until inline property bytes maintenance.
- Do not put inline/blob class bits in `inline_property_bytes_cell`; derive class from bucket schema at read time.
- `prev` remains the chain pointer only.

Variable-length inline property bytes (not implemented in LARA storage as of 2026-06-16) require an additional
profile flag; when present, log-backed inline property bytes always use the blob map regardless of
`inline_property_byte_width`.

## Benchmark Gate

Changes to log entry layout and scan replay affect storage, traversal, and inline-property-bytes-first execution.
Before accepting implementation of `src` removal or inline property bytes log 12 B compression, run focused
benchmarks that separate setup, mutation, scan, and inline property bytes attach costs.

Required benchmark coverage:

| Path | Benchmark signal |
|------|------------------|
| Same-label overflow insert | Whether smaller entries improve append-heavy log pressure |
| Same-label scan | Whether tombstone skipping and tag decoding affect hot traversal |
| Inline property bytes attach scan | Whether 12 B inline property bytes entries improve stable-memory IO enough to matter |
| Inline-property-first phase 1/2 | Whether cached replay and slot-to-log lookup stay neutral or faster |
| Tombstone-heavy delete/rewrite | Whether foreground delete stays cheap and maintenance cost remains bounded |

Existing candidate benches:

- `bench_labeled_mixed_label_hub_insert_33x50`
- `bench_labeled_mixed_label_hub_scan_33x50`
- `bench_labeled_mixed_label_hub_asc_iter_33x50`
- `bench_labeled_for_each_edges_for_label_48_x51`
- inline-property-bytes-first benches listed in `design/storage/inline-property-bytes-first-traversal.md`

Likely new focused benches (added 2026-06-16):

- `bench_labeled_inline_property_bytes_log_scan_8b_inline_overflow` — **implemented**
- `bench_labeled_inline_property_bytes_first_log_backed_selective_match` — **implemented** (`graph`: `bench_graph_inline_property_bytes_first_log_backed_selective_match`)
- `bench_labeled_tombstone_log_delete_then_scan` — **implemented**
- `bench_labeled_tombstone_log_rewrite_maintenance` — **implemented**

Benchmark acceptance should compare against the current implementation and must not disable
tombstone handling, inline property bytes blob cleanup, alias maintenance, or derived-state updates unless the
benchmark explicitly says it is measuring a lower-level isolated primitive.

**Status (2026-06-16):** focused benches below are implemented and baselined via canbench.
Edge log `src` wire removal is **implemented** (see review section).

## Edge log `src` review (2026-06-16)

Benchmark gate complete. Code review of prior `LLG` `src` word usage:

| Question | Finding |
| -------- | ------- |
| Required for core scan without labeled context? | **No for neighbor emission.** Scans anchor on the owning vertex row (`log_head`) and walk `prev`. `src` was decoded only for entry kind (`Live` / `Dead` / legacy `Delete`). |
| Required for validation or reopen? | **No after tombstone-only delete.** Tombstone edge inline propertys subsume `LOG_SRC_DEAD` and legacy `DeleteTarget` replay on the edge log. |
| Is live owner vertex id in `src` read on hot paths? | **No.** Live inserts wrote `log_owner` into `src`, but replay/scan never validated or used that id. |
| Can labeled derive owner without per-entry `src`? | **Yes:** `log_owner = vertices.log_leaf_vertex(vid)` at insert time; leaf segment is derived from the vertex row. |

**Decision (2026-06-16):** remove the edge log `src` word.

- `LLG` stride is `4 + edge_stride` (`prev` + edge bytes). Layout version stays **1**; development
  stores are recreated rather than migrated.
- Replay and scan skip tombstone edge inline propertys only; no `decode_log_entry_kind` on the edge log.

## Inline property bytes log `src` review (2026-06-16)

Benchmark gate and edge-log `src` removal are complete. Inline property bytes log review:

| Question | Finding |
| -------- | ------- |
| Separate inline property bytes dead marker required? | **No.** Slab inline property bytes already have no tombstone; traversal gates on edge tombstone only. |
| Can log-backed inline property bytes mirror slab? | **Yes, by live ordinal.** Edge and inline property bytes logs have independent entry indices and maintenance timing; the inline property bytes chain stores the same live-value order, not paired edge-log sites. |
| Does `LOG_SRC_DEAD` add information? | **No** after foreground delete writes only the edge tombstone. It duplicated edge liveness and forced a second write on delete. |
| Low-level inline property bytes log read without bucket context? | **Cannot infer width or ordinal ownership.** Labeled APIs resolve the bucket-local live ordinal and bucket schema before reading the independent inline property bytes sequence. |
| Live owner in `src` on write? | **Never read**, same as the removed edge log `src` word. |

**Decision (2026-06-16):** remove the inline property bytes log `src` word and stop writing `LOG_SRC_DEAD`.

- `LVL` stride is `4 + 8` (`prev` + `inline_property_bytes_cell`). Layout version stays **1**; development stores
  are recreated rather than migrated.
- Foreground delete removes the resolved live ordinal from the independent inline property bytes sequence, then
  tombstones the edge entry; retired inline property bytes log cells and blobs may remain until inline property bytes
  `sweep_inline_property_bytes_log_chain` / fold.
- Labeled inline property bytes reads resolve edge residency to a bucket-local live ordinal before reading inline property bytes
  slab/log bytes; edge and inline property bytes log entry indices are never compared.

## Alternatives Considered

### A. Keep separate delete log entries

Rejected as the long-term model. It preserves the current implementation shape, but leaves delete
state split across edge inline propertys and log metadata. Replay and inline-property-bytes-first traversal must keep
interpreting historical delete entries correctly.

### B. Remove deleted log entries by rewiring `prev`

Rejected for foreground delete. It can make the log chain look cleaner, but it risks changing the
slot index of surviving log-backed edges. That would push updates into aliases, postings, cursors,
and inline property bytes slot resolution.

### C. Redefine log-backed slot identity as physical log entry id

Deferred. This could make chain rewiring possible, but it is a larger identity redesign. It would
need a separate ADR covering edge handles, reverse aliases, index postings, traversal order,
inline-property-bytes-first phase two, and maintenance rewrite semantics.

### D. Move only inline property bytes log tags to `prev`

Rejected unless later evidence proves `src_and_tag` is impossible. `prev` owns chain topology.
Packing unrelated state into `prev` would make chain walking and corruption checks harder to reason
about.

### E. Compress inline property bytes log immediately because the design doc already says 16 B

Rejected. The design doc was ahead of implementation. This ADR requires an explicit layout review
and benchmark gate before changing stable bytes.

### F. Keep per-cell inline/blob tags in `PayloadLogCell`

Rejected for the target layout. Tags duplicate bucket schema, force read paths to branch on cell
bytes instead of bucket context, and consume a byte that prevents the 12 B entry target. The write
path already derives inline vs blob from `inline_property_byte_width`; phase 2 aligns the read path and wire
layout with that model. Legacy tagged cells are not supported after this fresh-store layout break.

## Consequences

Positive effects:

- One liveness source: the edge row tombstone contract.
- Foreground delete no longer needs delete-target log history.
- Log delete reports a bounded newer-suffix move batch synchronously.
- Inline property bytes remain subordinate to edge liveness, reducing duplicate delete rules.
- Inline property bytes log 12 B compression avoids mixing tag state into `prev`.
- One schema source for inline vs blob on the inline property bytes log: bucket `inline_property_byte_width` (+ profile).

Trade-offs:

- Labeled foreground delete preserves overflow-chain newest-to-oldest scan order.
- Scans must skip tombstone entries in both slab and log locations.
- Foreground deletes may leave retired inline property bytes log/blob storage until independent inline property bytes maintenance.
- Inline property bytes log 12 B compression keeps interpretation in bucket schema; inline property bytes liveness/order is the
  independently maintained bucket-local live-value sequence.
- Inline property bytes log reads require bucket context (or cached bucket width) to interpret log cells; low-level
  log walks without label context cannot infer inline vs blob from cell bytes alone.

## Implementation status (2026-06-16)

Phase 1 (implemented 2026-06-15, superseded for labeled delete on 2026-07-14):

1. Log-backed delete rewrites the target log entry as a tombstone edge inline property (`rewrite_overflow_log_entry_tombstone`).
2. Slab-backed delete on log rows writes the slab tombstone directly (no delete-target append).
3. Scan/replay paths skip tombstone log entries; legacy delete-target replay remains for old chains.
4. Superseded by ADR 0001: inline property bytes deletion now removes the resolved bucket-local live ordinal;
   edge and inline property bytes log chains are not physically paired.

Phase 2 (implemented 2026-06-16):

1. Inline property bytes log layout version 1: bucket-derived inline/blob; wide bodies in `inline_property_bytes_blobs`.
2. Inline vs blob derived from `LabelBucket::inline_property_byte_width` on read and write; no per-cell tags.

Benchmark gate (implemented 2026-06-16):

- `bench_labeled_inline_property_bytes_log_scan_8b_inline_overflow` — 4.67 M ix (hybrid inline property bytes attach)
- `bench_labeled_direct_unlink_log_delete_then_scan` — current scan-after-delete gate
- `bench_labeled_direct_unlink_log_fold_maintenance` — current overflow delete + fold gate
- `bench_graph_inline_property_bytes_first_log_backed_selective_match` — 698 K ix (48+24 overflow hub expand)

Edge log `src` removal (implemented 2026-06-16):

1. `LLG` entry stride `4 + E::BYTES` (`prev` + edge); layout version 1 unchanged.
2. Scan/replay paths use edge tombstone only; `LogEntryKind` / `decode_log_entry_kind` removed from edge log.
3. Fresh development stores only; no migration path.

Inline property bytes log `src` removal (implemented 2026-06-16):

1. `LVL` entry stride 12 B (`prev` + 8 B cell); layout version 1 unchanged.
2. Remove `LOG_SRC_DEAD`, `mark_inline_property_bytes_log_entry_dead`, and foreground inline property bytes log dead writes.
3. Superseded by ADR 0001: labeled log-backed inline property bytes reads use the resolved bucket-local live
   ordinal and never compare edge and inline property bytes log entry indices.
4. Maintenance sweep still clears inline property bytes log cells and drops blobs on fold.

Independent fold amendment (implemented 2026-07-14):

1. Structural edge fold during rebalance/resize/relocation preserves slab slots and copies every
   edge-log entry, including tombstones, without changing bucket-local slot indices.
2. Deferred overflow compaction leaves the slab prefix untouched, removes tombstones only from the
   bounded edge-log suffix, and reports moves only for shifted log-backed survivors.
3. Edge overflow compaction does not fold or relocate the independent inline property bytes log.

Tombstone-free labeled delete amendment (implemented 2026-07-14):

1. `unlink_overflow_log_entry` removes the head directly or rewrites the target's one newer link
   owner; no new overflow tombstone is written and logical scan order is unchanged.
2. `EdgeRemoval` reports the resulting newer-suffix `EdgeSlotMove` batch; Graph applies it to
   properties, aliases, and property-index postings in the foreground mutation path.
3. Payload deletion shifts the same newer live-ordinal suffix, so edge/value association and scan
   order remain correct while edge and inline property bytes physical maintenance stay independent.

Deferred:

- Variable-length inline property bytes encoding flag (profile) → always blob on log; not in current storage.

Tests should cover:

- slab-backed edge delete,
- log-backed edge delete,
- inline-property-bytes-in-log delete,
- inline-property-bytes-in-slab delete,
- inline property bytes blob cleanup,
- inline-property-bytes-first traversal after log-backed delete,
- alias/posting stability when a middle log-backed edge is deleted.

## Design Documentation Impact

Documents to update when this ADR is implemented:

| Document | Required update |
|----------|-----------------|
| `design/storage/labeled-edge-inline-properties.md` | **Updated 2026-06-16:** `LVL` 12 B entry; edge-tombstone inline property bytes liveness on log |
| `design/storage/lara-dgap-contract.md` | Record log tombstone policy and DGAP divergence |
| `design/storage/inline-property-bytes-first-traversal.md` | **Updated 2026-06-16:** bucket-derived log attach; edge replay filters dead log ordinals |
| `design/storage/stable-memory-inventory.md` | Note `LVL` layout version 1 when revisiting region docs |

## Amendments

- **2026-06-19 (ADR 0022):** A labeled bucket with an active overflow log
  (`log_head >= 0`) is scanned through the synthetic `LabelEdgeSpanAccess`. Its
  on-slab window end is bounded by the next bucket's `successor_start`, **not** by
  leaf-0's physical cap; `EdgeStore::slab_window_exclusive_end` must not clamp the
  window end below the bucket base when the base sits past the indexed leaf's cap.
  See [ADR 0022](0022-degree-driven-hub-edge-storage.md).

## Related

- [ADR 0022: Labeled overflow-log read-window fix](0022-degree-driven-hub-edge-storage.md)
- [ADR 0001: Labeled edge physical layer uses PMA leaf segment slide](0001-labeled-segment-slide.md)
- [ADR 0007: Stable-memory layout policy and measured consolidation](0007-stable-memory-layout.md)
- [ADR 0008: Edge inline property profile schema: router SSOT](0008-edge-inline-property-profile-router-ssot.md)
- [Labeled edge inline property storage](../storage/labeled-edge-inline-properties.md)
- [LARA storage contract (DGAP alignment)](../storage/lara-dgap-contract.md)
- [Inline-property-first traversal](../storage/inline-property-bytes-first-traversal.md)
