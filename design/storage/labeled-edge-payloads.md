# Labeled edge payload storage

## Overview

Labeled LARA keeps the hot edge row to **4 bytes** (`target` only). Per-label edge payloads (weights, timestamps, numeric payloads) live in a separate **byte-addressed** log-backed CSR (`EdgePayloadStore`).

The default edge label never stores payloads (`payload_byte_width = 0`).

## Wire layouts

| Record           | Size | Notes                                                              |
| ---------------- | ---- | ------------------------------------------------------------------ |
| `Edge` (CSR row) | 4 B  | `VertexRef` target only                                            |
| `LabelBucket`    | 24 B | + `payload_offset` (u40), `payload_byte_width` (u16), `payload_log_byte` |
| `LabeledVertex`  | 21 B | + `payload_allocated_bytes` (u40)                                    |

`payload_byte_width` is the physical width in bytes per slot (`0..=u16::MAX`). Signed vs unsigned vs float semantics live in the shard **edge payload profile** catalog (`EdgePayloadProfile`).

## Invariant

```text
(vertex_id, label_id, edge_slot) → target in EdgeStore
                              → payload_byte_width bytes in EdgePayloadStore (if width > 0)
```

Compaction and span rewrites must apply the **same logical order** to edges and payloads.

## Payload storage class (schema SSOT)

Inline vs blob is **not** a per-slot property stored in the payload slab or log cell. It is derived
from the label bucket schema:

| Location | Rule |
| -------- | ---- |
| **Payload slab** | Always read `payload_byte_width` bytes at the slot byte offset. No blob map. |
| **Payload log** | With bucket context: if `payload_byte_width == 0`, no payload; if `payload_byte_width <= 8`, inline bytes in the 8 B cell; else body in `payload_blobs` at `(leaf_segment, entry_idx)`. |

**Source of truth:** `LabelBucket::payload_byte_width` for fixed-width buckets. Semantic encoding
(signed, float, weight) lives in `EdgePayloadProfile` ([ADR 0008](../adr/0008-edge-payload-profile-router-ssot.md)).

**Future:** variable-length encodings will add a profile flag; log-backed payloads for those labels
always use the blob map regardless of width. Not implemented in storage as of 2026-06-16.

Foreground insert rejects `edge_payload_byte_width != bucket.payload_byte_width`, so storage class
does not vary among live slots in one bucket.

**Current wire shape (legacy):** the 9 B `PayloadLogCell` still stores inline/blob tags and repeats
blob width on write. Phase 2 ([ADR 0016](../adr/0016-overflow-log-tombstones-and-src-fields.md)) removes
those tags and aligns reads with bucket-derived class.

## Catalog

`EdgePayloadProfile` pairs `byte_width: u16` with `EdgePayloadEncoding` (e.g. `RawI32`, `RawU16`, `F32`, `WeightLinearU16`). Legacy `EdgeWeightProfile` maps to weight encodings with 2-byte width.

**Ownership (implemented):** logical schema (`EdgeLabelId → EdgePayloadProfile`) is **router SSOT**
(`ROUTER_EDGE_PAYLOAD_PROFILES`, router MemoryId 21). Plan and mutation wire carry
`payload_profile` on `ResolvedEdgeLabel` per [ADR 0008](../adr/0008-edge-payload-profile-router-ssot.md).
Graph shards resolve schema from execution context; graph stable `EDGE_PAYLOAD_PROFILES` is retired
(facade MemoryIds 38–41 repacked to 37–40). Tests may inject profiles via `test_labels` or an
explicit `ResolvedLabelTable`.

## Stable memories (per orientation)

- Existing edge/bucket memories
- `payload_slab` — byte CSR backing store (`EdgePayloadStore`)
- `payload_free_spans` / `payload_free_span_by_start` — retired byte-span index
- `payload_log` — per-PMA-leaf overflow log (`LVL`). **Current implementation:** 17 B
  entries: `prev`, `src`, 9 B tagged `PayloadLogCell`. **Target (ADR 0016 phase 2):** 16 B
  entries: `prev`, `src_and_tag`, untagged 8 B `payload_cell`; inline/blob derived from bucket
  schema, not cell tags.
- `payload_blobs` — overflow payload bodies for log entries whose bucket width exceeds 8 B

When an edge insert lands in the edge overflow log, the paired payload bytes are written to the payload log at the same entry index; `LabelBucket::payload_log_head` tracks the chain head (parallel to `overflow_log_head`).

**Delete (implemented):** edge liveness is the single source of truth. Log-backed edge delete rewrites the target log entry to the tombstone edge payload without rewiring `prev`. Payload log entries at the same logical site are marked dead or blob bodies dropped; payload slab bytes may remain until maintenance.

## Payload overflow log

### Payload log entry layout

| Layout | Bytes | Status |
| ------ | ----- | ------ |
| Current | `prev: i32`, `src: i32`, `PayloadLogCell: [u8; 9]` | Implemented (`LVL` v3, 17 B stride) |
| Target | `prev: i32`, `src_and_tag: i32`, `payload_cell: [u8; 8]` | Planned; benchmark gate ([ADR 0016](../adr/0016-overflow-log-tombstones-and-src-fields.md)) |

Layout mirrors `EdgeStore` segment logs (`LLG`): one index word per leaf segment plus fixed-capacity
entry slots. `push_vertex` grows the payload log segment tree in lockstep with the edge log. Span
rewrites fold log-backed payloads back onto the byte slab and clear the segment log.

### `PayloadLogCell` (current implementation only)

9 bytes on wire: byte 0 is an inline/blob tag; bytes 1–8 hold inline payload or blob width metadata.

| Tag (byte 0) | When (write path) | Cell contents | Read path (target) |
| ------------ | ----------------- | ------------- | ------------------ |
| INLINE | `payload_byte_width <= 8` | payload bytes; width from bucket on decode | Derive inline from bucket; ignore tag |
| BLOB | `payload_byte_width > 8` | tag + `u16` width (redundant with bucket) | Derive blob from bucket; body in `payload_blobs` |

Blob identity is derived from `(leaf_segment, entry_idx)` via `EdgePayloadBlobId::from_log_site`.

**Target (phase 2):** retire the tag byte and duplicated blob width. The 8 B cell holds inline payload
bytes only when bucket schema says inline-on-log; otherwise the cell is ignored and the blob map owns
the body. Dead/empty log entry state moves to `src_and_tag` bits; edge tombstone remains the primary
liveness source ([ADR 0016](../adr/0016-overflow-log-tombstones-and-src-fields.md)).

### Blob lifecycle

1. **Fold to slab** — sweep the full payload overflow chain and `drop_log_site` before clearing `payload_log_head`.
2. **Leaf release** — `drain_leaf_segment` on `payload_blobs` when the payload log segment is reclaimed.
3. **Before write** — idempotent `drop_log_site` before each log append (handles slot reuse after release).

## Traversal API

**Implemented:** `visit_out_edge_payload_batches_for_label` reads edge rows and payload bytes together (dense: parallel bulk read; sparse: per-edge attach).

**Planned:** payload-first two-phase traversal — see [payload-first-traversal.md](./payload-first-traversal.md).

## Related

- [payload-first-traversal.md](./payload-first-traversal.md)
- [lara-and-facade.md](./lara-and-facade.md)
- [ADR 0016: Overflow log tombstones and `src` field layout review](../adr/0016-overflow-log-tombstones-and-src-fields.md)
- `crates/ic-stable-lara/src/lara/edge_payload/`
