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
  entries: `prev`, `src`, 9 B tagged `PayloadLogCell`. **Target under review:** 16 B
  entries with tag bits moved to `src_and_tag`; see
  [ADR 0016](../adr/0016-overflow-log-tombstones-and-src-fields.md).
- `payload_blobs` — `StableBTreeMap` for payloads wider than 8 bytes on overflow

When an edge insert lands in the edge overflow log, the paired payload bytes are written to the payload log at the same entry index; `LabelBucket::payload_log_head` tracks the chain head (parallel to `overflow_log_head`).

## Payload overflow log

Layout mirrors `EdgeStore` segment logs (`LLG`): one index word per leaf segment plus fixed-capacity entry slots. `push_vertex` grows the payload log segment tree in lockstep with the edge log. Span rewrites fold log-backed payloads back onto the byte slab and clear the segment log.

### `PayloadLogCell`

**Current implementation:** 9 bytes (`tag` + 8 payload/blob metadata bytes).
**Target under review:** 8 bytes, with the tag moved to the payload log metadata word
([ADR 0016](../adr/0016-overflow-log-tombstones-and-src-fields.md)).

| Tag    | When                    | Storage                                          |
| ------ | ----------------------- | ------------------------------------------------ |
| INLINE | Current: `payload_byte_width <= 8`; target: keep up to 8 B inline | Payload bytes in the cell; width from bucket |
| BLOB   | `payload_byte_width > 8` | Current: tag + `u16` width in cell; target: tag in `src_and_tag`, width from bucket; body in `payload_blobs` |

Blob identity is derived from `(leaf_segment, entry_idx)` via `EdgePayloadBlobId::from_log_site` (not stored in the cell).

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
