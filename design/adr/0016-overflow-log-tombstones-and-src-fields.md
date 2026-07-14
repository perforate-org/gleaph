# 0016. Overflow log tombstones and `src` field layout review

Date: 2026-06-15  
Status: accepted (phases 1–3 implemented)  
Last revised: 2026-07-14
Anchor timestamp: 2026-07-14 02:55:40 UTC +0000

Payload-liveness portions of this ADR are amended by [ADR 0001](./0001-labeled-segment-slide.md): edge and payload physical slots/logs are independent, and payload deletion now removes the bucket-local live ordinal rather than relying on a paired edge-log entry.

## Context

LARA edge storage has two physical locations for a labeled edge row:

| Location | Owner | Delete representation (implemented) |
|----------|-------|-------------------------------------|
| Edge slab | `EdgeStore` / labeled bucket span | In-place tombstone edge inline value |
| Edge overflow log | `LogStore` (`LLG`) | Tombstone-free direct unlink; the `prev` chain preserves newest-to-oldest scan order |

Payload bytes preserve bucket-local live order through an independent physical layout:

| Location | Owner | Current layout |
|----------|-------|----------------|
| Payload slab | `EdgeInlineValueStore` (`payload_slab`) | Dense live-value sequence with its own slab-slot count |
| Payload overflow log | `PayloadLogStore` (`LVL`) | `prev: i32`, `payload_cell: [u8; 8]` |
| Payload blobs | `payload_blobs` | Wide overflow payload body keyed by `(leaf_segment, entry_idx)` |

Current implementation facts:

- Edge overflow log entries store `prev` (4 B) and edge bytes only
  ([`edge/log.rs`](../../crates/ic-stable-lara/src/lara/edge/log.rs)). Liveness is encoded in the
  edge inline value tombstone contract; there is no per-entry `src` word on `LLG`.
- **Superseded on 2026-07-14:** labeled log-backed delete no longer writes a tombstone. It advances
  the bucket head or rewrites the one newer entry whose `prev` points to the target. Existing stable
  tombstones remain readable and are reclaimed by maintenance for compatibility.
- **Implemented (2026-06-16):** payload overflow log entries are 12 B (`LVL`, layout version 1):
  `prev` (4 B) and an untagged 8 B inline cell
  ([`edge_inline_value/log.rs`](../../crates/ic-stable-lara/src/lara/edge_inline_value/log.rs),
  [`edge_inline_value/cell.rs`](../../crates/ic-stable-lara/src/lara/edge_inline_value/cell.rs)).
  Inline vs blob on the log is derived from `LabelBucket::payload_byte_width`; wide bodies live in
  `payload_blobs` at `(leaf_segment, entry_idx)`. The payload log is an independently maintained
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

Scan, replay, inline-value-first traversal, and maintenance must then interpret both sources in the same
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
| Edge slot identity | Labeled bucket scan order | Direct unlink shifts only newer suffix ordinals and reports every resulting move |
| Payload identity | Bucket-local live ordinal plus label bucket payload width | Payload slab/log maintain an independent ordered sequence; blob body remains keyed by payload log site |
| Payload storage class | Label bucket schema (`payload_byte_width` + profile encoding) | Inline vs blob on the payload log is derived from bucket schema, not stored per cell |
| Log reclamation | Foreground delete plus maintenance | New log deletes unlink immediately; maintenance only folds live suffixes and removes legacy tombstones |
| Derived state | Graph mutation path | Edge aliases, postings, and payloads update from canonical edge delete once |

The critical invariant is:

```text
Deleting a log-backed edge may shift only the newer overflow suffix, and every move is reported
before the mutation completes.
```

Slot identity is observed outside the physical log by edge handles, reverse aliases, local edge
postings, inline-value-first phase-two lookups, and traversal cursors. Middle-node unlink renumbers
the newer suffix by one. `EdgeRemoval::moves` carries that bounded move batch to Graph sidecars and
index postings; its size is at most the leaf log capacity minus one.

## Decision

Adopt the following target policy for future implementation.

### 1. Do not model deletion as a separate delete log entry

Delete state belongs to the deleted edge row itself.

- If the edge body is on the slab, write the tombstone edge inline value in that slab slot.
- If the edge body is the overflow head, update the bucket head to `head.prev`.
- Otherwise rewrite the one newer entry whose `prev` points to the target so it points to
  `target.prev`.
- Return one `EdgeSlotMove` for each live entry newer than the target; each slot shifts down by one.
- Do not compact payload slab bytes as part of the foreground delete path.

Overflow deletion is therefore O(chain lookup + one fixed-width link-owner write), leaves no new log
tombstone, and preserves newest-to-oldest scan order. Move notification cost is bounded by the
170-entry shared leaf log rather than vertex degree. Rebalance, resize, and relocation may fold the
remaining live chain. Existing slab tombstones and legacy log tombstones are compacted by maintenance.

### 2. Keep edge liveness canonical while maintaining payload order independently

Payload bytes are not the canonical liveness source.

- Resolve the edge physical slot to its bucket-local live ordinal before the tombstone commit.
- If payload bytes exist, fold the payload log when necessary and remove the same live ordinal while
  shifting the newer payload suffix, preserving edge/value scan order.
- Payload slab/log capacity and maintenance remain independent from edge slab/log capacity and
  maintenance. Either maintenance order must preserve observed edge/value pairs.
- Width-zero labels allocate no payload slab or log entries.

This keeps edge body liveness canonical without making payload physical consistency depend on edge
log residency.

### 3. Review the necessity of edge log `src`

After delete state moves into the edge inline value tombstone contract, the edge log `src` word should be
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
first: move log-backed delete state to tombstone edge inline values
then: benchmark and review whether `src` can be removed or repurposed
```

### 4. Derive payload inline/blob from bucket schema, not per-cell tags

Do not store inline vs blob storage class in the payload slab or payload log cell.

**Schema source of truth:** `LabelBucket::payload_byte_width`, plus (when added) the label's
`EdgeInlineValueProfile.encoding` for variable-length payloads.

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
- Foreground insert already rejects `edge_inline_value_byte_width != bucket.payload_byte_width`, so
  storage class does not vary per slot within one bucket.

Per-cell inline/blob tags and duplicated blob widths are not stored on the wire.

### 5. Payload log 12 B with an untagged 8 B cell (implemented)

The payload log entry (`LVL`, layout version 1) is:

```text
prev: i32
payload_cell: [u8; 8]
```

Design constraints:

- `payload_cell` holds up to 8 B of inline payload when bucket schema says inline-on-log; it is
  otherwise ignored and the blob map owns the body.
- Liveness on the payload log is **not** stored in the log entry. The payload sequence follows
  bucket-local live ordinals independently from edge slab/log residency. Foreground delete resolves
  and removes the payload ordinal before tombstoning the canonical edge; unreachable log/blob bytes
  may remain until payload maintenance.
- Do not put inline/blob class bits in `payload_cell`; derive class from bucket schema at read time.
- `prev` remains the chain pointer only.

Variable-length payloads (not implemented in LARA storage as of 2026-06-16) require an additional
profile flag; when present, log-backed payloads always use the blob map regardless of
`payload_byte_width`.

## Benchmark Gate

Changes to log entry layout and scan replay affect storage, traversal, and inline-value-first execution.
Before accepting implementation of `src` removal or payload log 12 B compression, run focused
benchmarks that separate setup, mutation, scan, and payload attach costs.

Required benchmark coverage:

| Path | Benchmark signal |
|------|------------------|
| Same-label overflow insert | Whether smaller entries improve append-heavy log pressure |
| Same-label scan | Whether tombstone skipping and tag decoding affect hot traversal |
| Payload attach scan | Whether 12 B payload entries improve stable-memory IO enough to matter |
| Inline-value-first phase 1/2 | Whether cached replay and slot-to-log lookup stay neutral or faster |
| Tombstone-heavy delete/rewrite | Whether foreground delete stays cheap and maintenance cost remains bounded |

Existing candidate benches:

- `bench_labeled_mixed_label_hub_insert_33x50`
- `bench_labeled_mixed_label_hub_scan_33x50`
- `bench_labeled_mixed_label_hub_asc_iter_33x50`
- `bench_labeled_for_each_edges_for_label_48_x51`
- inline-value-first benches listed in `design/storage/inline-value-first-traversal.md`

Likely new focused benches (added 2026-06-16):

- `bench_labeled_payload_log_scan_8b_inline_overflow` — **implemented**
- `bench_labeled_payload_first_log_backed_selective_match` — **implemented** (`graph`: `bench_graph_payload_first_log_backed_selective_match`)
- `bench_labeled_tombstone_log_delete_then_scan` — **implemented**
- `bench_labeled_tombstone_log_rewrite_maintenance` — **implemented**

Benchmark acceptance should compare against the current implementation and must not disable
tombstone handling, payload blob cleanup, alias maintenance, or derived-state updates unless the
benchmark explicitly says it is measuring a lower-level isolated primitive.

**Status (2026-06-16):** focused benches below are implemented and baselined via canbench.
Edge log `src` wire removal is **implemented** (see review section).

## Edge log `src` review (2026-06-16)

Benchmark gate complete. Code review of prior `LLG` `src` word usage:

| Question | Finding |
| -------- | ------- |
| Required for core scan without labeled context? | **No for neighbor emission.** Scans anchor on the owning vertex row (`log_head`) and walk `prev`. `src` was decoded only for entry kind (`Live` / `Dead` / legacy `Delete`). |
| Required for validation or reopen? | **No after tombstone-only delete.** Tombstone edge inline values subsume `LOG_SRC_DEAD` and legacy `DeleteTarget` replay on the edge log. |
| Is live owner vertex id in `src` read on hot paths? | **No.** Live inserts wrote `log_owner` into `src`, but replay/scan never validated or used that id. |
| Can labeled derive owner without per-entry `src`? | **Yes:** `log_owner = vertices.log_leaf_vertex(vid)` at insert time; leaf segment is derived from the vertex row. |

**Decision (2026-06-16):** remove the edge log `src` word.

- `LLG` stride is `4 + edge_stride` (`prev` + edge bytes). Layout version stays **1**; development
  stores are recreated rather than migrated.
- Replay and scan skip tombstone edge inline values only; no `decode_log_entry_kind` on the edge log.

## Payload log `src` review (2026-06-16)

Benchmark gate and edge-log `src` removal are complete. Payload log review:

| Question | Finding |
| -------- | ------- |
| Separate payload dead marker required? | **No.** Slab payloads already have no tombstone; traversal gates on edge tombstone only. |
| Can log-backed payload mirror slab? | **Yes, by live ordinal.** Edge and payload logs have independent entry indices and maintenance timing; the payload chain stores the same live-value order, not paired edge-log sites. |
| Does `LOG_SRC_DEAD` add information? | **No** after foreground delete writes only the edge tombstone. It duplicated edge liveness and forced a second write on delete. |
| Low-level payload log read without bucket context? | **Cannot infer width or ordinal ownership.** Labeled APIs resolve the bucket-local live ordinal and bucket schema before reading the independent payload sequence. |
| Live owner in `src` on write? | **Never read**, same as the removed edge log `src` word. |

**Decision (2026-06-16):** remove the payload log `src` word and stop writing `LOG_SRC_DEAD`.

- `LVL` stride is `4 + 8` (`prev` + `payload_cell`). Layout version stays **1**; development stores
  are recreated rather than migrated.
- Foreground delete removes the resolved live ordinal from the independent payload sequence, then
  tombstones the edge entry; retired payload log cells and blobs may remain until payload
  `sweep_payload_log_chain` / fold.
- Labeled payload reads resolve edge residency to a bucket-local live ordinal before reading payload
  slab/log bytes; edge and payload log entry indices are never compared.

## Alternatives Considered

### A. Keep separate delete log entries

Rejected as the long-term model. It preserves the current implementation shape, but leaves delete
state split across edge inline values and log metadata. Replay and inline-value-first traversal must keep
interpreting historical delete entries correctly.

### B. Remove deleted log entries by rewiring `prev`

Rejected for foreground delete. It can make the log chain look cleaner, but it risks changing the
slot index of surviving log-backed edges. That would push updates into aliases, postings, cursors,
and payload slot resolution.

### C. Redefine log-backed slot identity as physical log entry id

Deferred. This could make chain rewiring possible, but it is a larger identity redesign. It would
need a separate ADR covering edge handles, reverse aliases, index postings, traversal order,
inline-value-first phase two, and maintenance rewrite semantics.

### D. Move only payload log tags to `prev`

Rejected unless later evidence proves `src_and_tag` is impossible. `prev` owns chain topology.
Packing unrelated state into `prev` would make chain walking and corruption checks harder to reason
about.

### E. Compress payload log immediately because the design doc already says 16 B

Rejected. The design doc was ahead of implementation. This ADR requires an explicit layout review
and benchmark gate before changing stable bytes.

### F. Keep per-cell inline/blob tags in `PayloadLogCell`

Rejected for the target layout. Tags duplicate bucket schema, force read paths to branch on cell
bytes instead of bucket context, and consume a byte that prevents the 12 B entry target. The write
path already derives inline vs blob from `payload_byte_width`; phase 2 aligns the read path and wire
layout with that model. Legacy tagged cells are not supported after this fresh-store layout break.

## Consequences

Positive effects:

- One liveness source: the edge row tombstone contract.
- Foreground delete no longer needs delete-target log history.
- Log delete reports a bounded newer-suffix move batch synchronously.
- Payload bytes remain subordinate to edge liveness, reducing duplicate delete rules.
- Payload log 12 B compression avoids mixing tag state into `prev`.
- One schema source for inline vs blob on the payload log: bucket `payload_byte_width` (+ profile).

Trade-offs:

- Labeled foreground delete preserves overflow-chain newest-to-oldest scan order.
- Scans must skip tombstone entries in both slab and log locations.
- Foreground deletes may leave retired payload log/blob storage until independent payload maintenance.
- Payload log 12 B compression keeps interpretation in bucket schema; payload liveness/order is the
  independently maintained bucket-local live-value sequence.
- Payload log reads require bucket context (or cached bucket width) to interpret log cells; low-level
  log walks without label context cannot infer inline vs blob from cell bytes alone.

## Implementation status (2026-06-16)

Phase 1 (implemented 2026-06-15, superseded for labeled delete on 2026-07-14):

1. Log-backed delete rewrites the target log entry as a tombstone edge inline value (`rewrite_overflow_log_entry_tombstone`).
2. Slab-backed delete on log rows writes the slab tombstone directly (no delete-target append).
3. Scan/replay paths skip tombstone log entries; legacy delete-target replay remains for old chains.
4. Superseded by ADR 0001: payload deletion now removes the resolved bucket-local live ordinal;
   edge and payload log chains are not physically paired.

Phase 2 (implemented 2026-06-16):

1. Payload log layout version 1: bucket-derived inline/blob; wide bodies in `payload_blobs`.
2. Inline vs blob derived from `LabelBucket::payload_byte_width` on read and write; no per-cell tags.

Benchmark gate (implemented 2026-06-16):

- `bench_labeled_payload_log_scan_8b_inline_overflow` — 4.67 M ix (hybrid payload attach)
- `bench_labeled_direct_unlink_log_delete_then_scan` — current scan-after-delete gate
- `bench_labeled_direct_unlink_log_fold_maintenance` — current overflow delete + fold gate
- `bench_graph_payload_first_log_backed_selective_match` — 698 K ix (48+24 overflow hub expand)

Edge log `src` removal (implemented 2026-06-16):

1. `LLG` entry stride `4 + E::BYTES` (`prev` + edge); layout version 1 unchanged.
2. Scan/replay paths use edge tombstone only; `LogEntryKind` / `decode_log_entry_kind` removed from edge log.
3. Fresh development stores only; no migration path.

Payload log `src` removal (implemented 2026-06-16):

1. `LVL` entry stride 12 B (`prev` + 8 B cell); layout version 1 unchanged.
2. Remove `LOG_SRC_DEAD`, `mark_payload_log_entry_dead`, and foreground payload-log dead writes.
3. Superseded by ADR 0001: labeled log-backed payload reads use the resolved bucket-local live
   ordinal and never compare edge and payload log entry indices.
4. Maintenance sweep still clears payload log cells and drops blobs on fold.

Independent fold amendment (implemented 2026-07-14):

1. Structural edge fold during rebalance/resize/relocation preserves slab slots and copies every
   edge-log entry, including tombstones, without changing bucket-local slot indices.
2. Deferred overflow compaction leaves the slab prefix untouched, removes tombstones only from the
   bounded edge-log suffix, and reports moves only for shifted log-backed survivors.
3. Edge overflow compaction does not fold or relocate the independent payload log.

Tombstone-free labeled delete amendment (implemented 2026-07-14):

1. `unlink_overflow_log_entry` removes the head directly or rewrites the target's one newer link
   owner; no new overflow tombstone is written and logical scan order is unchanged.
2. `EdgeRemoval` reports the resulting newer-suffix `EdgeSlotMove` batch; Graph applies it to
   properties, aliases, and property-index postings in the foreground mutation path.
3. Payload deletion shifts the same newer live-ordinal suffix, so edge/value association and scan
   order remain correct while edge and payload physical maintenance stay independent.

Deferred:

- Variable-length payload encoding flag (profile) → always blob on log; not in current storage.

Tests should cover:

- slab-backed edge delete,
- log-backed edge delete,
- payload-in-log delete,
- payload-in-slab delete,
- payload blob cleanup,
- inline-value-first traversal after log-backed delete,
- alias/posting stability when a middle log-backed edge is deleted.

## Design Documentation Impact

Documents to update when this ADR is implemented:

| Document | Required update |
|----------|-----------------|
| `design/storage/labeled-edge-inline-values.md` | **Updated 2026-06-16:** `LVL` 12 B entry; edge-tombstone payload liveness on log |
| `design/storage/lara-dgap-contract.md` | Record log tombstone policy and DGAP divergence |
| `design/storage/inline-value-first-traversal.md` | **Updated 2026-06-16:** bucket-derived log attach; edge replay filters dead log ordinals |
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
- [ADR 0008: Edge payload profile schema: router SSOT](0008-edge-inline-value-profile-router-ssot.md)
- [Labeled edge inline value storage](../storage/labeled-edge-inline-values.md)
- [LARA storage contract (DGAP alignment)](../storage/lara-dgap-contract.md)
- [Inline-value-first traversal](../storage/inline-value-first-traversal.md)
