# Labeled edge value storage

## Overview

Labeled LARA keeps the hot edge row to **4 bytes** (`target` only). Per-label edge values (weights, timestamps, numeric payloads) live in a separate **byte-addressed** log-backed CSR (`EdgeValueStore`).

The default edge label never stores values (`value_width = 0`).

## Wire layouts

| Record           | Size | Notes                                                                       |
| ---------------- | ---- | --------------------------------------------------------------------------- |
| `Edge` (CSR row) | 4 B  | `VertexRef` target only                                                     |
| `LabelBucket`    | 22 B | + `value_offset` (u40), `value_log_head`, `value_width_code` in packed word |
| `LabeledVertex`  | 21 B | + `value_allocated_bytes` (u40)                                             |

`value_width_code` encodes physical width only: `0, 1, 2, 4, 8, 16, 32, 64` bytes. Signed vs unsigned vs float semantics live in the shard **edge value profile** catalog (`EdgeValueProfile`).

## Invariant

```text
(vertex_id, label_id, edge_slot) → target in EdgeStore
                              → value_width bytes in EdgeValueStore (if width > 0)
```

Compaction and span rewrites must apply the **same logical order** to edges and values.

## Catalog

`EdgeValueProfile` pairs `EdgeValueWidth` with `EdgeValueEncoding` (e.g. `RawI32`, `RawU16`, `F32`, `WeightLinearU16`). Legacy `EdgeWeightProfile` maps to weight encodings with width `W2`.

## Stable memories (per orientation)

- Existing 10 edge/bucket memories
- `value_slab` — byte CSR backing store (`EdgeValueStore`)
- `value_free_spans` / `value_free_span_by_start` — retired byte-span index
- `value_log` — per-PMA-leaf overflow log (`LVL`, 72 B entries: `prev`, `src`, 64 B payload)

When an edge insert lands in the edge overflow log, the paired value bytes are written to the value log at the same entry index; `LabelBucket::value_log_head` tracks the chain head (parallel to `overflow_log_head`).

## Value overflow log

Layout mirrors `EdgeStore` segment logs (`LLG`): one index word per leaf segment plus fixed-capacity entry slots. `push_vertex` grows the value log segment tree in lockstep with the edge log. Span rewrites fold log-backed values back onto the byte slab and clear the segment log.

## Related

- [lara-and-facade.md](./lara-and-facade.md)
- `crates/ic-stable-lara/src/lara/edge_value/`
