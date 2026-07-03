# Labeled edge payload storage

Last updated: 2026-07-03
Anchor timestamp: 2026-07-03 09:35:13 UTC +0000

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

## Catalog

`EdgePayloadProfile` pairs `byte_width: u16` with `EdgePayloadEncoding` (e.g. `RawI32`, `RawU16`, `F32`, `WeightLinearU16`). Legacy `EdgeWeightProfile` maps to weight encodings with 2-byte width.

**Ownership (implemented):** logical schema `(GraphId, EdgeLabelId) → EdgePayloadSchemaRecord` is
**router SSOT** (`ROUTER_EDGE_PAYLOAD_PROFILES`, router MemoryId 21). The record is a versioned
envelope that represents either an admin `UnnamedProfile`, a named scalar or struct inline
schema (`property_id`, scalar type or declaration-ordered logical field specs, derived
`EdgePayloadProfile`) per [ADR 0034 Slices 20/24](../adr/0034-gleaph-gql-extension-syntax.md). Development stable data must be wiped
when this format changes because backward compatibility is not maintained. The physical
`EdgePayloadProfile` (scalar encoding or `opaque_bytes(total_byte_width)` for structs) and the named
inline schema (`inline_schema`: `None`, `Scalar { property_id }`, or `Struct { property_id, fields }`)
are both derived from the canonical record and travel on `ResolvedEdgeLabel` per
[ADR 0008](../adr/0008-edge-payload-profile-router-ssot.md). Graph shards resolve schema from execution
context and must treat payload bytes as the only read source for the matching inline property; sidecar
property values are not consulted. Scalar reads, struct field reads, filters, projections, `ORDER BY`,
and aggregate inputs all share one inline-aware read helper. Slice 25 validates the physical struct
projection and decodes the payload into a declaration-ordered GQL `Value::Record`. Graph stable `EDGE_PAYLOAD_PROFILES` is retired
(facade MemoryIds 38–41 repacked to 37–40). Tests may inject profiles via `test_labels` or an
explicit `ResolvedLabelTable`.

**Struct reads:** in Slice 25, Graph receives a Router-derived physical field projection (name, byte
offset, exact scalar profile) for each fixed-size inline struct field. It validates the projection
against the payload width and decodes the payload into a declaration-ordered GQL `Value::Record`;
`e.stats.field` works uniformly in projection, filter, comparison, aggregate input, and `ORDER BY`.
**Struct mutation packing:** remains planned (Slice 26). `COST BY` over a struct field and property
indexes on inline struct fields also remain planned.

## Mutation write semantics (implemented)

For an `InlineScalar` edge label, ordinary GQL mutations treat payload bytes as the only canonical
value for the named inline property:

- **No sidecar write.** A successful `INSERT`, `SET`, or all-properties replacement never puts the
inline property id into `EDGE_PROPERTIES`, never enqueues index maintenance for it, and never falls
back to a sidecar value if encoding fails.
- **Validation before write.** All mutation expressions, property-id resolutions, duplicate checks,
inline scalar encoding, and sidecar property validation (reserved property ids and
`Value::to_binary_bytes()` encodability) happen before the first adjacency record is created or before
existing sidecar properties are removed. Invalid input therefore cannot leave a partially initialized
edge, a stale payload, or a torn sidecar record.
- **Mirrored update.** `GraphStore::update_edge_payload_at_handle` and the edge-profile commit already
own forward/reverse and undirected-alias synchronization; mutation packing reuses that commit so every
physical mirror of the logical edge reflects the same payload bytes.
- **Absence not represented.** `REMOVE e.inline_property` is rejected. There is no null/presence
bitmap in this slice, so the inline property is required on insertion and cannot be deleted.

Non-inline properties on the same edge keep existing sidecar behavior, including index-maintenance
where applicable. The inline schema itself is never written by Graph; it is derived from Router
stable state and carried on `ResolvedEdgeLabel` per [ADR 0008](../adr/0008-edge-payload-profile-router-ssot.md).

## Stable memories (per orientation)

- Existing edge/bucket memories
- `payload_slab` — byte CSR backing store (`EdgePayloadStore`)
- `payload_free_spans` / `payload_free_span_by_start` — retired byte-span index
- `payload_log` — per-PMA-leaf overflow log (`LVL`, layout version 1). 12 B entries: `prev`,
  untagged 8 B `payload_cell`; inline/blob derived from bucket `payload_byte_width`, not cell tags.
  Log-backed liveness follows the paired edge tombstone at the same ordinal ([ADR 0016](../adr/0016-overflow-log-tombstones-and-src-fields.md)).
- `payload_blobs` — overflow payload bodies for log entries whose bucket width exceeds 8 B

When an edge insert lands in the edge overflow log, the paired payload bytes are written to the payload log at the same entry index; `LabelBucket::payload_log_head` tracks the chain head (parallel to `overflow_log_head`).

**Delete (implemented):** edge liveness is the single source of truth. Log-backed edge delete rewrites the target log entry to the tombstone edge payload without rewiring `prev`. Payload log entries at the same logical site are not separately marked dead; paired edge tombstone gates reads. Payload slab bytes and log cells/blobs may remain until maintenance.

## Payload overflow log

### Payload log entry layout

| Field | Size | Notes |
| ----- | ---- | ----- |
| `prev` | 4 B | Chain pointer (same as edge log) |
| `payload_cell` | 8 B | Inline payload bytes when `payload_byte_width <= 8`; ignored for blob |

Implemented as `LVL` layout version 1 with 12 B stride.

Layout mirrors `EdgeStore` segment logs (`LLG`, `prev` + edge bytes per entry): one index word per leaf segment plus fixed-capacity
entry slots. `push_vertex` grows the payload log segment tree in lockstep with the edge log. Span
rewrites fold log-backed payloads back onto the byte slab and clear the segment log.

### Inline cell and blob map

When bucket schema says inline-on-log (`payload_byte_width <= 8`), the 8 B cell holds payload bytes
(width from bucket on decode). When width exceeds 8 B, the cell is zero on wire and the body lives in
`payload_blobs` at `(leaf_segment, entry_idx)` via `EdgePayloadBlobId::from_log_site`.

Log-backed payload liveness matches the slab: the paired edge row tombstone is the delete signal.
Unreachable inline cells and blob bodies may remain until maintenance fold/sweep
([ADR 0016](../adr/0016-overflow-log-tombstones-and-src-fields.md)).

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
