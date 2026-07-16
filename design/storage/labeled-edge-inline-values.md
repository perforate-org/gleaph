# Labeled edge inline value storage

Last updated: 2026-07-16
Anchor timestamp: 2026-07-16 22:21:38 UTC +0000

## Overview

Labeled LARA keeps the hot edge row to **4 bytes** (`target` only). Per-label edge inline values (weights, timestamps, numeric payloads) live in a separate **byte-addressed** log-backed CSR (`EdgeInlineValueStore`).

The default edge label never stores payloads (`payload_byte_width = 0`).

## Wire layouts

| Record           | Size | Notes                                                              |
| ---------------- | ---- | ------------------------------------------------------------------ |
| `Edge` (CSR row) | 4 B  | `VertexRef` target only                                            |
| `LabelBucket`    | 29 B | edge locator/degree/slab slots + `inline_value_slab_slots` (u32), offset (u40), byte width (u16), payload log head/length |
| `LabeledVertex`  | 21 B | + `payload_allocated_bytes` (u40)                                    |

`payload_byte_width` is the physical width in bytes per slot (`0..=u16::MAX`). Signed vs unsigned vs float semantics live in the shard **edge inline value profile** catalog (`EdgeInlineValueProfile`).

## Invariant

```text
(vertex_id, label_id, bucket-local live ordinal) → target in EdgeStore
                                                → payload bytes in EdgeInlineValueStore (width > 0)
```

Bucket-local live order is the association source of truth. Edge and payload physical slots are not equal and their log entries are not paired by entry index. Edge compaction preserves live order; payload deletion removes the same live ordinal.

Physical ownership is independent:

- edge slab slots: `stored_slots`; edge log: `overflow_log_head`;
- payload slab slots: `inline_value_slab_slots`; payload log: `inline_value_log_head` + `inline_value_log_len`;
- edge and payload rebalance, resize, and relocation may run in either order and do not mutate the other store's physical metadata;
- a zero-width label owns no payload slab/log entries.

### Payload capacity policy

Payload capacity uses a separate quota from edge capacity. A payload-bearing
bucket is allocated lazily on its first value-bearing edge, with an initial
quota of exactly one entry and `inline_value_byte_width` bytes. A zero-width
label does not allocate a payload span even when it has edges. Subsequent slab
growth is expressed in value-width entries and bytes; it is not rounded to the
edge `segment_size` or edge vertex quota. If the existing payload span cannot
be extended in place without relocating it, new values use the independent
payload log and are folded later. This policy keeps the first value on a sparse
label at `value_width` bytes while preserving a dense payload slab whenever its
span remains extendable.

The growth baseline is intentionally exact rather than reserving geometric
headroom. In the measured 256-edge payload-growth benchmark, the current policy
used 53.10M instructions, 568 heap pages, and no stable-memory page increase.
Tail spans already grow in place, while non-tail spans use the payload log; a
separate reserved-capacity field would therefore add persistent layout and
delete/log-transition complexity without reducing the existing copy paths.
Headroom remains deferred until measurements show a workload where that trade-off
is favorable.

The fragmented first-span benchmarks measured 37.72M instructions, 568 heap
pages, and no stable-memory page increase for the full fixture. On the same
fixture, a free-span-reuse control insertion consumed 53.39K instructions,
whereas the compaction-triggering insertion consumed 114.89K instructions; the
compaction-only scope consumed 74.38K instructions. Thus synchronous
compaction currently adds about 61.50K instructions over the control path. The
additional scan remains isolated to fragmented allocation, but its cost is high
enough that frequent triggers should move to bounded maintenance scheduling.

The deferred labeled wrapper now schedules `CompactPayloadSlab` as a deduplicated
global queue item when a payload insert detects this pressure. That wrapper
suppresses synchronous payload compaction for the insert; the queued item runs
the payload-only pass within the caller's maintenance budget. The queue item is
fixed-width and uses a new stable tag, while unknown tags continue to decode as
the original bucket-segment item for compatibility with older queue contents.

The deferred A/B benchmark measured 36.83K instructions for the pressure
insert's detection and enqueue path, 22.08K for the complete maintenance item,
and 25.78K for its payload-compaction scope. The corresponding synchronous
compaction-triggering insert measured 114.89K instructions. These values are
fixture-specific, but show that the deferred insert avoids paying the full
compaction cost in the mutation path.

The pressure predicate uses only allocator-owned free-byte and largest-span
statistics; it does not scan vertices or recompute live/allocated payload bytes.
The latter remains an observability operation exposed by
`payload_storage_stats()`.

The queue persistence contract is covered by a reopen test: a pending payload
item remains queued across graph reconstruction and is consumed by the next
maintenance step.

An end-to-end deferred-wrapper test also verifies that real payload values and
edge order survive the queued compaction, while fragmented holes become one
reusable span; the backing tail remains intentionally unshrunk.

The maintenance contract also requeues a payload item when compaction returns an
error; a failure-injection test verifies that the item remains pending and is
consumed successfully on the following retry.

Payload-only compaction is available through `compact_payload_slab`. It preflights
earlier free-span prefixes, including spans released by earlier moves in the same
plan, copies only payload slab bytes, updates bucket payload offsets, and retires the old spans. Edge slab positions, edge/payload log chains,
bucket-local live order, and vertex allocation totals are unchanged. The operation
does not shrink the backing capacity or invoke edge maintenance. New payload
span allocation checks `payload_compaction_needed(requested_bytes)` first and
attempts this compaction only when aggregate free space covers the request but
the largest retired span does not. Existing tail growth and non-tail extension
remain on their direct paths; a failed or conservative no-op compaction falls
back to the normal allocator.

## Payload storage class (schema SSOT)

Inline vs blob is **not** a per-slot property stored in the payload slab or log cell. It is derived
from the label bucket schema:

| Location | Rule |
| -------- | ---- |
| **Payload slab** | Always read `payload_byte_width` bytes at the slot byte offset. No blob map. |
| **Payload log** | With bucket context: if `payload_byte_width == 0`, no payload; if `payload_byte_width <= 8`, inline bytes in the 8 B cell; else body in `payload_blobs` at `(leaf_segment, entry_idx)`. |

**Source of truth:** `LabelBucket::payload_byte_width` for fixed-width buckets. Semantic encoding
(signed, float, weight) lives in `EdgeInlineValueProfile` ([ADR 0008](../adr/0008-edge-inline-value-profile-router-ssot.md)).

**Future:** variable-length encodings will add a profile flag; log-backed payloads for those labels
always use the blob map regardless of width. Not implemented in storage as of 2026-06-16.

Foreground insert rejects `edge_inline_value_byte_width != bucket.payload_byte_width`, so storage class
does not vary among live slots in one bucket.

## Catalog

`EdgeInlineValueProfile` pairs `byte_width: u16` with `EdgeInlineValueEncoding` (e.g. `RawI32`, `RawU16`, `F32`, `WeightLinearU16`). Legacy `EdgeWeightProfile` maps to weight encodings with 2-byte width.

**Ownership (implemented):** logical schema `(GraphId, EdgeLabelId) → EdgeInlineValueSchemaRecord` is
**router SSOT** (`ROUTER_EDGE_PAYLOAD_PROFILES`, router MemoryId 21). The record is a versioned
envelope that represents either an admin `UnnamedProfile`, a named scalar or struct inline
schema (`property_id`, scalar type or declaration-ordered logical field specs, derived
`EdgeInlineValueProfile`) per [ADR 0034 Slices 20/24](../adr/0034-gleaph-gql-extension-syntax.md). Development stable data must be wiped
when this format changes because backward compatibility is not maintained. The physical
`EdgeInlineValueProfile` (scalar encoding or `opaque_bytes(total_byte_width)` for structs) and the named
inline schema (`inline_schema`: `None`, `Scalar { property_id }`, or `Struct { property_id, fields }`)
are both derived from the canonical record and travel on `ResolvedEdgeLabel` per
[ADR 0008](../adr/0008-edge-inline-value-profile-router-ssot.md). Graph shards resolve schema from execution
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
- **Mirrored update.** `GraphStore::update_edge_inline_value_at_handle` and the edge-profile commit already
own forward/reverse and undirected-alias synchronization; mutation packing reuses that commit so every
physical mirror of the logical edge reflects the same payload bytes.
- **Absence not represented.** `REMOVE e.inline_property` is rejected. There is no null/presence
bitmap in this slice, so the inline property is required on insertion and cannot be deleted.

Non-inline properties on the same edge keep existing sidecar behavior, including index-maintenance
where applicable. The inline schema itself is never written by Graph; it is derived from Router
stable state and carried on `ResolvedEdgeLabel` per [ADR 0008](../adr/0008-edge-inline-value-profile-router-ssot.md).

## Stable memories (per orientation)

- Existing edge/bucket memories
- `payload_slab` — byte CSR backing store (`EdgeInlineValueStore`)
- `payload_free_spans` / `payload_free_span_by_start` — retired byte-span index
- `payload_log` — per-PMA-leaf overflow log (`LVL`, layout version 1). 12 B entries: `prev`,
  untagged 8 B `payload_cell`; inline/blob derived from bucket `payload_byte_width`, not cell tags.
  Entries form the payload-owned ordered suffix; deletion folds/removes by live ordinal before the edge tombstone commit.
- `payload_blobs` — overflow payload bodies for log entries whose bucket width exceeds 8 B

Payload insertion chooses its own slab or log from payload capacity. It does not follow the edge insertion location. `LabelBucket::inline_value_log_head` and `inline_value_log_len` track the ordered payload suffix independently of `overflow_log_head`.

**Delete (implemented):** edge liveness remains canonical. Before the edge tombstone commit, storage resolves the physical edge slot to its bucket-local live ordinal and folds the payload log when necessary. The payload sequence removes that ordinal and stays dense; it does not consult an edge log entry index for payload liveness.

## Payload overflow log

### Payload log entry layout

| Field | Size | Notes |
| ----- | ---- | ----- |
| `prev` | 4 B | Chain pointer (same as edge log) |
| `payload_cell` | 8 B | Inline payload bytes when `payload_byte_width <= 8`; ignored for blob |

Implemented as `LVL` layout version 1 with 12 B stride.

Layout uses the same bounded per-leaf log primitive as `EdgeStore`, but capacity and entries are independent. `push_vertex` ensures both segment trees can address the leaf; ordinary append, fold, release, resize, and relocation are payload-owned. Edge span rewrites do not fold payload logs.

### Inline cell and blob map

When bucket schema says inline-on-log (`payload_byte_width <= 8`), the 8 B cell holds payload bytes
(width from bucket on decode). When width exceeds 8 B, the cell is zero on wire and the body lives in
`payload_blobs` at `(leaf_segment, entry_idx)` via `EdgeInlineValueBlobId::from_log_site`.

Log-backed payload entries are an ordered suffix of the bucket-local live-value sequence. They are not paired to edge-log entry indices. Blob bodies remain keyed by payload log site and are swept when the payload log folds or its segment is released.

### Blob lifecycle

1. **Fold to slab** — sweep the full payload overflow chain and `drop_log_site` before clearing `payload_log_head`.
2. **Leaf release** — `drain_leaf_segment` on `payload_blobs` when the payload log segment is reclaimed.
3. **Before write** — idempotent `drop_log_site` before each log append (handles slot reuse after release).

## Traversal API

**Implemented:** `visit_out_edge_inline_value_batches_for_label` reads edge rows and payload bytes together (dense: parallel bulk read; sparse: per-edge attach).

**Planned:** inline-value-first two-phase traversal — see [inline-value-first-traversal.md](./inline-value-first-traversal.md).

## Related

- [inline-value-first-traversal.md](./inline-value-first-traversal.md)
- [lara-and-facade.md](./lara-and-facade.md)
- [ADR 0016: Overflow log tombstones and `src` field layout review](../adr/0016-overflow-log-tombstones-and-src-fields.md)
- `crates/ic-stable-lara/src/lara/edge_inline_value/`
