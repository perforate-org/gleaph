# Labeled edge value storage

## Overview

Labeled LARA keeps the hot edge row to **4 bytes** (`target` only). Per-label edge values (weights, timestamps, numeric payloads) live in a separate **byte-addressed** log-backed CSR (`EdgeValueStore`).

The default edge label never stores values (`value_byte_width = 0`).

## Wire layouts

| Record           | Size | Notes                                                                 |
| ---------------- | ---- | --------------------------------------------------------------------- |
| `Edge` (CSR row) | 4 B  | `VertexRef` target only                                                 |
| `LabelBucket`    | 24 B | + `value_offset` (u40), `value_byte_width` (u16), `value_log_byte` |
| `LabeledVertex`  | 21 B | + `value_allocated_bytes` (u40)                                       |

`value_byte_width` is the physical width in bytes per slot (`0..=u16::MAX`). Signed vs unsigned vs float semantics live in the shard **edge value profile** catalog (`EdgeValueProfile`).

## Invariant

```text
(vertex_id, label_id, edge_slot) → target in EdgeStore
                              → value_byte_width bytes in EdgeValueStore (if width > 0)
```

Compaction and span rewrites must apply the **same logical order** to edges and values.

## Catalog

`EdgeValueProfile` pairs `byte_width: u16` with `EdgeValueEncoding` (e.g. `RawI32`, `RawU16`, `F32`, `WeightLinearU16`). Legacy `EdgeWeightProfile` maps to weight encodings with 2-byte width.

## Stable memories (per orientation)

- Existing edge/bucket memories
- `value_slab` — byte CSR backing store (`EdgeValueStore`)
- `value_free_spans` / `value_free_span_by_start` — retired byte-span index
- `value_log` — per-PMA-leaf overflow log (`LVL`, **16 B** entries: `prev`, `src`, **8 B** `ValueLogCell`)
- `value_blobs` — `StableBTreeMap` for payloads wider than 8 bytes on overflow

When an edge insert lands in the edge overflow log, the paired value bytes are written to the value log at the same entry index; `LabelBucket::value_log_head` tracks the chain head (parallel to `overflow_log_head`).

## Value overflow log

Layout mirrors `EdgeStore` segment logs (`LLG`): one index word per leaf segment plus fixed-capacity entry slots. `push_vertex` grows the value log segment tree in lockstep with the edge log. Span rewrites fold log-backed values back onto the byte slab and clear the segment log.

### `ValueLogCell` (8 bytes)

| Tag | When | Storage |
| --- | ---- | ------- |
| INLINE | `value_byte_width <= 7` | Payload in cell bytes `1..`; width from bucket |
| BLOB | `value_byte_width > 7` | Tag + `u16` width in cell; body in `value_blobs` |

Blob identity is derived from `(leaf_segment, entry_idx)` via `EdgeValueBlobId::from_log_site` (not stored in the cell).

### Blob lifecycle

1. **Fold to slab** — sweep the full value overflow chain and `drop_log_site` before clearing `value_log_head`.
2. **Leaf release** — `drain_leaf_segment` on `value_blobs` when the value log segment is reclaimed.
3. **Before write** — idempotent `drop_log_site` before each log append (handles slot reuse after release).

## Related

- [lara-and-facade.md](./lara-and-facade.md)
- `crates/ic-stable-lara/src/lara/edge_value/`
