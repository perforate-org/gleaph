# Labeled edge inline property storage

Last updated: 2026-11-09
Anchor timestamp: 2026-11-09 18:48:00 UTC +0900

## Overview

Labeled LARA keeps the hot edge row to **4 bytes** (`target` only). Per-label edge inline propertys (weights, timestamps, numeric payloads) live in a separate **byte-addressed** CSR backed by a slab span plus the bucket's values spill run (`EdgeInlinePropertyBytesStore`).

The default edge label never stores payloads (`inline_property_byte_width = 0`).

## Wire layouts

| Record           | Size | Notes                                                              |
| ---------------- | ---- | ------------------------------------------------------------------ |
| `Edge` (CSR row) | 4 B  | `VertexRef` target only                                            |
| `LabelBucket`    | 34 B | locator word (mode/tombstone/edge-run low bits), degree, stored slots, value slab slots (u32), value offset (u40), byte width (u16), and bytes 27..34 as mode payload: slab edge-run high bits + values run id + reserve; tiny T2 top byte + used width; tree no-log byte (`0xFF`) + packed depth marker |
| `LabeledVertex`  | 21 B | + `payload_allocated_bytes` (u40)                                    |

`inline_property_byte_width` is the physical width in bytes per slot (`0..=u16::MAX`). Signed vs unsigned vs float semantics live in the shard **edge inline property profile** catalog (`EdgeInlinePropertyProfile`).

## Invariant

```text
(vertex_id, label_id, bucket-local live ordinal) → target in EdgeStore
                                                → inline property bytes in EdgeInlinePropertyBytesStore (width > 0)
```

Bucket-local live order is the association source of truth. Edge and inline property bytes physical slots are not equal and their spill rows are not paired by ordinal. For an `Insertion` edge-ordering policy, edge compaction preserves live order; for an `Unordered` policy, compaction may move a later live edge into an interior tombstone and must move the corresponding inline property bytes by the same live ordinal. Inline property bytes deletion removes the same live ordinal.

The ordering policy is a Graph Type schema capability resolved by Router; it is not duplicated in
`LabelBucket`. Unordered tombstone reuse and swap-compaction must use the exact edge/inline-property
ordinal and location results owned by LARA and consumed by Graph.

Physical ownership is independent:

- edge slab slots: `stored_slots`; edge spill: the per-bucket edge run (`edge_run()`), absent when every edge row is slab-resident;
- inline property bytes slab slots: `inline_property_bytes_slab_slots`; inline property bytes spill: the per-bucket values run (`values_run()`); the descriptor's log byte is the canonical no-log byte (`inline_property_bytes_log_byte == 0xFF`) and slab rows decode `inline_property_bytes_log_len` as 0;
- edge and inline property bytes rebalance, resize, and relocation may run in either order and do not mutate the other store's physical metadata;
- a zero-width label owns no inline property bytes slab or values-run rows.

### Payload capacity policy

Payload capacity uses a separate quota from edge capacity. A inline property bytes-bearing
bucket is allocated lazily on its first value-bearing edge, with an initial
quota of exactly one entry and `inline_property_byte_width` bytes. A zero-width
label does not allocate a inline property bytes span even when it has edges. Subsequent slab
growth is expressed in value-width entries and bytes; it is not rounded to the
edge `segment_size` or edge vertex quota. If the existing inline property bytes span cannot
be extended in place without relocating it, new values use the independent
values spill run and are folded into the slab later. This policy keeps the first value on a sparse
label at `value_width` bytes while preserving a dense inline property bytes slab whenever its
span remains extendable.

The growth baseline is intentionally exact rather than reserving geometric
headroom. In the measured 256-edge inline property bytes-growth benchmark, the current policy
used 53.10M instructions, 568 heap pages, and no stable-memory page increase.
Tail spans already grow in place, while non-tail spans append to the values spill run; a
separate reserved-capacity field would therefore add persistent layout and
delete/run-transition complexity without reducing the existing copy paths.
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
global queue item when a inline property bytes insert detects this pressure. That wrapper
suppresses synchronous inline property bytes compaction for the insert; the queued item runs
the inline-property-bytes-only pass within the caller's maintenance budget. The queue item is
fixed-width and uses a new stable tag, while unknown tags continue to decode as
the original bucket-segment item for compatibility with older queue contents.

The deferred A/B benchmark measured 36.83K instructions for the pressure
insert's detection and enqueue path, 22.08K for the complete maintenance item,
and 25.78K for its inline property bytes-compaction scope. The corresponding synchronous
compaction-triggering insert measured 114.89K instructions. These values are
fixture-specific, but show that the deferred insert avoids paying the full
compaction cost in the mutation path.

The pressure predicate uses only allocator-owned free-byte and largest-span
statistics; it does not scan vertices or recompute live/allocated inline property bytes.
The latter remains an observability operation exposed by
`payload_storage_stats()`.

The free-span allocator now persists `free_bytes` and `largest_free_span` in
unused metadata space after the bin-head table; `active_count` remains the
persisted span count. Allocation, split, release, coalescing, and replacement
update the summary with the corresponding free-span mutation. If a mutation
removes the current largest span, the largest value is rebuilt from the highest
non-empty size-class bin rather than scanning the entire by-start index. Reopen validates the existing free-span structures and rebuilds
the persisted summary is used directly on reopen; the transient max-heap is
rebuilt lazily only when a mutation needs to recover a stale largest candidate.
Consequently, the pressure predicate no longer materializes
`free_spans.spans()` on its hot path.

The updated 64-check pressure benchmark measured 36.23K instructions for each
of approximately 16, 64, and 256 retired spans, compared with the previous
6.24M, 27.90M, and 119.04M measurements. The one-time rebuild and largest-span
rescans remain mutation/reopen work rather than query-time work.

The largest-span recovery benchmark intentionally places one 128-byte span
and many 96-byte spans in the same `[65, 128]` size-class bin. The allocator
now keeps an in-memory max-heap of `(span length, record id)` candidates;
removals use lazy stale-candidate eviction, and the first post-reopen mutation
rebuilds the heap from the validated by-start records if largest-span recovery
requires it. The heap is not part of the stable layout;
the persisted `free_bytes` and `largest_free_span` fields remain the durable
summary and are updated as before. Sixteen take/restore cycles measured 1.61M
instructions with 256 spans and 5.12M with 4,096 spans, compared with the
previous 5.11M and 59.73M bin-scan measurements. The best-fit split benchmark
stayed within its noise threshold, with changes of approximately 1%.

Because heap entries are lazily invalidated, the heap is rebuilt from active
by-start records when its size exceeds `2 * active_count + 64`. This bounds
stale-candidate memory while keeping ordinary insert/release operations on the
heap fast. A churn test covering 256 remove/reinsert cycles verifies that the
bound and allocator summary remain correct.

The queue persistence contract is covered by a reopen test: a pending inline property bytes
item remains queued across graph reconstruction and is consumed by the next
maintenance step.

An end-to-end deferred-wrapper test also verifies that real inline property bytes values and
the applicable edge-order contract survive the queued compaction: `ORDER BY INSERTION` buckets
retain live order, while `Unordered` buckets may use swap-compaction as long as each edge keeps
its corresponding inline property bytes. Fragmented holes become one reusable span; the backing
tail remains intentionally unshrunk.

The maintenance contract also requeues a inline property bytes item when compaction returns an
error; a failure-injection test verifies that the item remains pending and is
consumed successfully on the following retry.

Payload-only compaction is available through `compact_inline_property_bytes_slab`. It preflights
earlier free-span prefixes, including spans released by earlier moves in the same
plan, copies only inline property bytes slab bytes, updates bucket inline property bytes offsets, and retires the old spans. Edge slab positions, edge-run and values-run ordinals,
bucket-local live order, and vertex allocation totals are unchanged. The operation
does not shrink the backing capacity or invoke edge maintenance. New inline property bytes
span allocation checks `payload_compaction_needed(requested_bytes)` first and
attempts this compaction only when aggregate free space covers the request but
the largest retired span does not. Existing tail growth and non-tail extension
remain on their direct paths; a failed or conservative no-op compaction falls
back to the normal allocator.

## Inline property bytes storage class (schema SSOT)

Inline vs blob is **not** a per-slot property stored in the inline property bytes slab span or values-run cell. It is derived
from the label bucket schema:

| Location | Rule |
| -------- | ---- |
| **Inline property bytes slab** | Always read `inline_property_byte_width` bytes at the slot byte offset. No blob map. |
| **Values spill run** | With bucket context: if `inline_property_byte_width == 0`, no inline property bytes; if `inline_property_byte_width <= 8`, the value is inline in the row's 8 B cell; else the body is in `inline_property_bytes_blobs` at `(run, row)`. |

**Source of truth:** `LabelBucket::inline_property_byte_width` for fixed-width buckets. Semantic encoding
(signed, float, weight) lives in `EdgeInlinePropertyProfile` ([ADR 0008](../adr/0008-edge-inline-property-profile-router-ssot.md)).

**Future:** variable-length encodings will add a profile flag; values-run-backed payloads for those labels
always use the blob map regardless of width. Not implemented in storage as of 2026-06-16.

Foreground insert rejects `edge_inline_property_byte_width != bucket.inline_property_byte_width`, so storage class
does not vary among live slots in one bucket.

## Catalog

`EdgeInlinePropertyProfile` pairs `byte_width: u16` with `EdgeInlinePropertyEncoding` (e.g. `RawI32`, `RawU16`, `F32`). Legacy weight encodings such as `WeightLinearU16` were removed in ADR 0051 Phase B.

**Ownership (implemented):** logical schema `(GraphId, EdgeLabelId) → EdgeInlinePropertySchemaRecord` is
**router SSOT** (`ROUTER_EDGE_INLINE_PROPERTY_PROFILES`, router MemoryId 21). The record is a versioned
envelope that represents either an admin `UnnamedProfile`, a named scalar or struct inline
schema (`property_id`, scalar type or declaration-ordered logical field specs, derived
`EdgeInlinePropertyProfile`) per [ADR 0034 Slices 20/24](../adr/0034-gleaph-gql-extension-syntax.md). Development stable data must be wiped
when this format changes because backward compatibility is not maintained. The physical
`EdgeInlinePropertyProfile` (scalar encoding or `opaque_bytes(total_byte_width)` for structs) and the named
inline schema (`inline_schema`: `None`, `Scalar { property_id }`, or `Struct { property_id, fields }`)
are both derived from the canonical record and travel on `ResolvedEdgeLabel` per
[ADR 0008](../adr/0008-edge-inline-property-profile-router-ssot.md). Graph shards resolve schema from execution
context and must treat inline property bytes as the only read source for the matching inline property; sidecar
property values are not consulted. Scalar reads, struct field reads, filters, projections, `ORDER BY`,
and aggregate inputs all share one inline-aware read helper. Slice 25 validates the physical struct
projection and decodes the inline property bytes into a declaration-ordered GQL `Value::Record`. Graph stable `EDGE_PAYLOAD_PROFILES` is retired
(facade MemoryIds 38–41 repacked to 37–40). Tests may inject profiles via `test_labels` or an
explicit `ResolvedLabelTable`.

**Struct reads:** in Slice 25, Graph receives a Router-derived physical field projection (name, byte
offset, exact scalar profile) for each fixed-size inline struct field. It validates the projection
against the inline property bytes width and decodes the inline property bytes into a declaration-ordered GQL `Value::Record`;
`e.stats.field` works uniformly in projection, filter, comparison, aggregate input, and `ORDER BY`.
**Struct mutation packing:** implemented (Slice 25). `COST BY` over a struct field is implemented,
and inline struct leaf indexes are implemented: the Router interns each dotted leaf path as a
property identity and Graph decodes that leaf from canonical inline property bytes for posting
maintenance.

## Mutation write semantics (implemented)

For an `InlineScalar` edge label, ordinary GQL mutations treat inline property bytes as the only canonical
value for the named inline property:

- **No sidecar write.** A successful `INSERT`, `SET`, or all-properties replacement never puts the
inline property id into `EDGE_PROPERTIES`, never enqueues index maintenance for it, and never falls
back to a sidecar value if encoding fails.
- **Validation before write.** All mutation expressions, property-id resolutions, duplicate checks,
inline scalar encoding, and sidecar property validation (reserved property ids and
`Value::to_binary_bytes()` encodability) happen before the first adjacency record is created or before
existing sidecar properties are removed. Invalid input therefore cannot leave a partially initialized
edge, a stale inline property bytes, or a torn sidecar record.
- **Mirrored update.** `GraphStore::update_edge_inline_property_at_handle` and the edge-profile commit
  currently synchronize forward/reverse and undirected aliases. ADR 0048 replaces that lookup with
  bidirectional LARA `counterpart_of`, so updates address the exact parallel-edge counterpart; an undirected
  self-loop has one physical entry and is updated once. Until ADR 0048 is implemented, the existing
  facade alias path remains active.
- **Absence not represented.** `REMOVE e.inline_property` is rejected. There is no null/presence
bitmap in this slice, so the inline property is required on insertion and cannot be deleted.
- **Index old-key/new-key transitions (implemented).** Every accepted inline mutation resolves the
previous and next indexed values per catalog membership before the canonical write and dispatches
exactly `Remove(prev key)` + `Insert(next key)` when both exist, `Remove(prev key)` alone when a
membership's decoded value disappears (schema no longer exposes that leaf path), and nothing when
the value is unchanged (`crates/graph/src/property/change.rs::index_ops_for_value_change`). A
rejected SET (top-level or nested `NULL`, wrong-kind leaf, missing schema field, byte-width
mismatch) fails before the first stable write, so no stale posting can outlive its value. Active
memberships post through the ordinary pending queue; Building/Sealing memberships are admitted
through the pre-canonical build fence. Owning tests: the executor NULL/missing-field/NULL-leaf
rejection contracts and the store-level posting-swap/no-op/leaf-scoped/width-reject contracts
(GAP-2026-07-29-004).

Non-inline properties on the same edge keep existing sidecar behavior, including index-maintenance
where applicable. The inline schema itself is never written by Graph; it is derived from Router
stable state and carried on `ResolvedEdgeLabel` per [ADR 0008](../adr/0008-edge-inline-property-profile-router-ssot.md).

## Stable memories (per orientation)

- Existing edge/bucket memories
- `inline_property_bytes_slab` — byte CSR backing store (`EdgeInlinePropertyBytesStore`)
- `payload_free_spans` / `payload_free_span_by_start` — retired byte-span index
- `inline_property_bytes_log` — legacy region name retained for the **values spill arena** (ADR 0097 §5): a run store over `ArenaRows::VALUES` with two header rows (used length or free marker, then the free-list link) plus power-of-two class data rows of 8 B cells. The retired per-PMA-leaf `LVL` chain no longer lives here.
- `inline_property_bytes_blobs` — bodies for values-run cells whose bucket width exceeds 8 B, keyed by `(run, row)` and moved with the row when a run relocates.

Inline property bytes insertion chooses its own slab or values run from inline property bytes capacity. It does not follow the edge insertion location. `LabelBucket::values_run()` tracks the ordered inline property bytes suffix independently of the edge run (`edge_run()`); descriptor byte 27 (`inline_property_bytes_log_byte`) is the canonical no-log value (`0xFF`) on slab and tree rows, and byte 28 (`inline_property_bytes_log_len`) is 0 on slab rows.

**Delete (implemented):** edge liveness remains canonical. Before the edge tombstone commit, storage resolves the physical edge slot to its bucket-local live ordinal and folds the values run when necessary. The inline property bytes sequence removes that ordinal and stays dense; it does not consult an edge-run ordinal for inline property bytes liveness.

## Values spill run

Inline property bytes that do not fit the slab prefix live in the bucket's **values spill run**
(ADR 0097 §5) — the same bounded per-bucket spill primitive the edge plane uses, with an
inline-property row payload instead of an edge row.

- **Allocation and growth.** A run is minted on the first row that does not fit the slab prefix,
  allocated in power-of-two capacity classes with a free list per class, and grows by class (in
  place at the arena tail, otherwise by copying to the next class). A bucket that needs more than
  `MAX_ROWS` promotes to tree mode (ADR 0097 §6).
- **Rows.** One row is one 8 B cell (`INLINE_PROPERTY_BYTES_LOG_CELL_BYTES`). The cell holds the
  value inline when the bucket width is at most 8 B; a wider value stores a blob tag in the cell and
  the body in `inline_property_bytes_blobs` at `(run, row)`.
- **Ordering.** Run rows are the bucket-local live-value suffix. They are not paired to edge-run
  ordinals; deletion removes the live ordinal and keeps the suffix dense.
- **Fold and release.** When the run reaches its largest class the caller folds its head into the
  values slab, drops those rows and releases the run once it is empty.
- **Relocation re-keys blobs.** `grow_to` copies a run that is not at the arena tail; it re-keys the
  moved rows' blob sites inside the growth step, before publishing the new run id, so a published
  run id always reads every row it is said to hold.

### Inline cell and blob map

A row's cell is 8 B. When the bucket schema says the value is inline-on-row
(`inline_property_byte_width <= 8`), the cell holds the value (width comes from the bucket on
decode). When the width exceeds 8 B the cell is a blob tag and the body lives in
`inline_property_bytes_blobs` at `(run, row)`.

### Blob lifecycle

1. **Fold to slab** — copy the foldable head of the values run into the inline property bytes slab
   span, remove those rows from the run, and release the run once it is empty; a folded row's blob
   site is dropped as the row leaves the run.
2. **Run release** — releasing a run drops every row's blob site before the run header is retired.
3. **Row delete / relocation** — deleting a row drops its `(run, row)` blob; relocation re-keys the
   moved rows' blob sites inside the growth step.

## Traversal API

**Implemented:** `visit_out_edge_inline_property_batches_for_label` reads edge rows and inline property bytes together (dense: parallel bulk read; sparse: per-edge attach).

**Planned:** inline-property-bytes-first two-phase traversal — see [inline-property-bytes-first-traversal.md](./inline-property-bytes-first-traversal.md).

## Related

- [inline-property-bytes-first-traversal.md](./inline-property-bytes-first-traversal.md)
- [lara-and-facade.md](./lara-and-facade.md)
- [ADR 0050: LARA labeled traverse read API consolidation](../adr/0050-lara-traverse-read-api.md)
- [ADR 0016: Overflow log tombstones and `src` field layout review](../adr/0016-overflow-log-tombstones-and-src-fields.md) — historical layout decisions, superseded by ADR 0097
- [ADR 0097: Per-bucket spill runs replace the shared per-leaf overflow log](../adr/0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md) — current values/edge run mechanism
- `crates/ic-stable-lara/src/lara/edge_inline_property/`
