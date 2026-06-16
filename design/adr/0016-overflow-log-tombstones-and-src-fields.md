# 0016. Overflow log tombstones and `src` field layout review

Date: 2026-06-15  
Status: accepted (phase 1 implemented)  
Last revised: 2026-06-15
Anchor timestamp: 2026-06-15 23:44:43 UTC +0000

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
| Payload overflow log | `PayloadLogStore` (`LVL`) | `prev: i32`, `src: i32`, `PayloadLogCell` |
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
- Payload overflow log entries are currently 17 B in code:
  `prev` (4 B), `src` (4 B), and a 9 B `PayloadLogCell`
  ([`edge_payload/log.rs`](../../crates/ic-stable-lara/src/lara/edge_payload/log.rs),
  [`edge_payload/cell.rs`](../../crates/ic-stable-lara/src/lara/edge_payload/cell.rs)).
- `labeled-edge-payloads.md` had already described the target payload entry as 16 B.
  This ADR records the missing decision gate before code is changed to match that target.

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

This raises two layout questions:

1. Does the edge body log still need a physical `src` word after delete state moves into the edge
   payload tombstone contract?
2. Does the payload log need a full `src` word, or can that word become `src_and_tag` so the payload
   entry shrinks from 17 B to 16 B?

## Existing Architecture Assessment

The existing storage domains can own this change. No new storage subsystem is required.

| Boundary | Owner | Source of truth after this ADR |
|----------|-------|--------------------------------|
| Edge liveness | Edge row payload | Slab and log entries both expose the same tombstone-edge contract |
| Edge slot identity | Labeled bucket scan order | Deleting one edge must not change surviving edge slot indices |
| Payload identity | Edge slot plus label bucket payload width | Payload log site mirrors the edge log site; blob body remains keyed by log site |
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

### 4. Make payload log 16 B only if `src_and_tag` remains fit for purpose

The likely payload log target is:

```text
prev: i32
src_and_tag: i32
payload_or_blob_cell: [u8; 8]
```

This would replace the current 17 B implementation:

```text
prev: i32
src: i32
PayloadLogCell: [u8; 9]
```

The tag can move out of `PayloadLogCell` because:

- inline payload bytes need up to 8 B,
- blob identity is already the log site `(leaf_segment, entry_idx)`,
- blob width is available from the label bucket `payload_byte_width`,
- dead/empty state can be represented by tag bits instead of an all-zero 9 B cell.

Prefer putting tag bits in `src_and_tag`, not `prev`, because `prev` is the chain pointer and should
remain a pure traversal primitive. `src_and_tag` can be decoded once per entry without changing chain
walking.

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

Likely new focused benches:

- `bench_labeled_payload_log_scan_8b_inline_overflow`
- `bench_labeled_payload_first_log_backed_selective_match`
- `bench_labeled_tombstone_log_delete_then_scan`
- `bench_labeled_tombstone_log_rewrite_maintenance`

Benchmark acceptance should compare against the current implementation and must not disable
tombstone handling, payload blob cleanup, alias maintenance, or derived-state updates unless the
benchmark explicitly says it is measuring a lower-level isolated primitive.

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

## Consequences

Positive effects:

- One liveness source: the edge row tombstone contract.
- Foreground delete no longer needs delete-target log history.
- Surviving edge slot indices stay stable after deletion.
- Payload bytes remain subordinate to edge liveness, reducing duplicate delete rules.
- Payload log 16 B compression has a clear path without mixing tag state into `prev`.

Trade-offs:

- Tombstoned log entries remain in chains until maintenance folds or rewrites them.
- Scans must skip tombstone entries in both slab and log locations.
- Foreground deletes may leave unreachable payload slab bytes until maintenance.
- Edge log `src` removal is not decided here; keeping it may leave bytes on the table, while
  removing it too early could split core and labeled LARA layout concepts.

## Implementation status (2026-06-15)

Phase 1 (implemented):

1. Log-backed delete rewrites the target log entry as a tombstone edge payload (`rewrite_overflow_log_entry_tombstone`).
2. Slab-backed delete on log rows writes the slab tombstone directly (no delete-target append).
3. Scan/replay paths skip tombstone log entries; legacy delete-target replay remains for old chains.
4. Payload log chains stay aligned with edge log chains; payload bodies are cleared without rewiring.

Deferred (benchmark gate):

- Edge log `src` removal or repurposing review.
- Payload log 16 B compression (`src_and_tag` + 8 B cell).

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
| `design/storage/labeled-edge-payloads.md` | Mark current 17 B implementation and target 16 B compression accurately |
| `design/storage/lara-dgap-contract.md` | Record log tombstone policy and DGAP divergence |
| `design/storage/payload-first-traversal.md` | Update sparse overflow replay contract if delete-target replay is removed |
| `design/storage/stable-memory-inventory.md` | Update if payload or edge log layout version changes |

## Related

- [ADR 0001: Labeled edge physical layer uses PMA leaf segment slide](0001-labeled-segment-slide.md)
- [ADR 0007: Stable-memory layout policy and measured consolidation](0007-stable-memory-layout.md)
- [ADR 0008: Edge payload profile schema: router SSOT](0008-edge-payload-profile-router-ssot.md)
- [Labeled edge payload storage](../storage/labeled-edge-payloads.md)
- [LARA storage contract (DGAP alignment)](../storage/lara-dgap-contract.md)
- [Payload-first traversal](../storage/payload-first-traversal.md)
