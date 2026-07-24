# 0048. Adaptive LARA mate index replaces Graph edge aliases

Date: 2026-07-23
Status: accepted (ScanOnly implemented; shared four-region mate ownership wired in Plan 0139; bounded promotion admission, pure leaf-blob construction, owner-facing failure-atomic publication, canonical leaf enumeration, mutation invalidation, and maintenance rebuild scheduling wired in Plans 0141–0143; the earlier source/mate Published wire is measurement-only and retired as an adoption candidate; Plan 0176 now makes the topology-specific byte/runtime selector explicit, while ordinary-caller activation and alias removal remain deferred until each supported stratum clears the measured gate)
Last revised: 2026-07-24
Anchor timestamp: 2026-07-23 23:12:48 UTC +0000

## Context

Gleaph uses a physical adjacency location as edge identity:

```text
EdgeHandle = (owner_vertex_id, storage_label_id, slot_index)
```

The persisted edge row remains four bytes (`target`). Its label and slot are supplied by the
containing LARA bucket and iterator. A local logical edge has the following representation:

| Logical edge | Physical entries | Canonical entry |
| --- | --- | --- |
| directed `u -> v` | forward `(u, v)` + reverse `(v, u)` | forward |
| undirected `u -- v`, `u != v` | two forward entries | entry owned by `max(u, v)` |
| undirected self-loop `u -- u` | one forward entry | that entry |

Directed self-loops retain separate forward and reverse entries because outgoing and incoming
traversal are separate orientations. An undirected self-loop has one stored entry. Its
graph-theoretic degree contribution and physical entry count are separate API concepts.

The implemented facade stores one `EDGE_ALIASES` `StableBTreeMap` row for each non-self logical
edge. Its fixed-width key is 10 bytes and value is 8 bytes before B-tree node overhead. Lookup is
efficient only from alias to canonical. Canonical-to-alias lookup, canonical-target movement, and
canonical deletion scan the whole map. Scalar insertion also inserts both entries and then scans
adjacency to rediscover their slots. Slot-renumbering maintenance repairs alias keys and targets.

Adding a second B-tree for reverse lookup would make the raw key/value footprint at least 36 bytes
per non-self logical edge before node overhead and would create another synchronous consistency
surface. It would also leave ownership of a physical-slot invariant outside LARA, which owns slot
allocation, ordering, rebalance, and compaction.

## Problem

Given either physical entry of a local logical edge, Graph must identify the exact paired entry for
deletion and inline-value update, and must identify the canonical entry for property access. This
must remain exact for parallel edges and for directed, undirected, and self-loop contracts.

The solution must:

1. preserve physical `EdgeHandle` identity;
2. avoid a per-edge B-tree key/value pair in both lookup directions;
3. avoid post-insert scans when LARA already knows the written slots;
4. retain a zero-per-edge-metadata path for small or cold buckets;
5. keep ordinary adjacency scans independent of mate metadata;
6. let LARA repair acceleration where slots are changed; and
7. avoid enlarging every edge, vertex, bucket, or PMA node without a measured net benefit.

## Existing architecture assessment

The existing domains can absorb the behavior without a new graph subsystem:

- `ic-stable-lara` owns adjacency entries, bidirectional projection, physical slot allocation,
  bucket order, PMA leaves, rebalance, and compaction.
- `GraphStore` owns canonical edge properties and derived-index events, but should not own a
  duplicate physical-position index.
- The bidirectional labeled LARA wrapper is the smallest boundary that sees both directed
  orientations and both forward halves of an undirected edge. It owns pair ordering and mate
  resolution.

Canonical adjacency order is the source of truth. A packed mate index is derived acceleration and
may fall back to adjacency rank/select. It is not another edge identity.

## Decision

### 1. Retire `EDGE_ALIASES`

Remove the Graph facade `EDGE_ALIASES` stable B-tree, its check/rebuild surface, and its slot-move
repair hooks after the LARA mate APIs are implemented. Do not replace it with two B-trees.

The facade retains canonicalization as an abstraction but delegates physical mate resolution to
LARA. Plan 0140 implements and tests this as an opt-in `scan_only_canonical_edge_handle` bridge;
ordinary callers retain the `EDGE_ALIASES` compatibility path until rank-indexed promotion and
adoption are approved. Rebuilding or unpublished locator rows do not participate in this bridge, so adjacency
remains the fallback source of truth. Orientation must be explicit because `EdgeHandle` alone cannot distinguish a directed
reverse entry from a forward entry:

```rust
struct PhysicalEdgeRef {
    orientation: LabeledOrientation,
    handle: EdgeHandle,
}

fn mate_of(edge: PhysicalEdgeRef) -> Result<PhysicalEdgeRef, MateLookupError>;
fn canonical_handle(
    edge: PhysicalEdgeRef,
    kind: EdgeKind,
) -> Result<EdgeHandle, MateLookupError>;
```

Canonicalization follows these rules:

```text
directed                         -> forward entry
undirected u != v               -> entry owned by max(u, v)
undirected self-loop u == v     -> the sole entry
```

For an undirected self-loop, `mate_of` returns the input entry and no mate-index entry is stored.

### 2. Make pair rank the authoritative zero-metadata relation

For each pair key, corresponding entries have the same live occurrence rank:

```text
directed:   (kind, label, source, target)
undirected: (kind, label, min(endpoint), max(endpoint))
```

The `k`th live forward entry of a directed key corresponds to the `k`th live reverse entry. For a
non-self undirected key, the `k`th live entry at the larger endpoint corresponds to the `k`th live
entry at the smaller endpoint. Directed self-loops use the directed rule across separate
orientations. Undirected self-loops require no rank join.

`mate_of` in scan mode computes the input entry's rank among equal-neighbor entries and selects the
same rank from the counterpart bucket. This is exact for parallel edges without a persistent
logical edge id.

All insert paths enforce the same pair order on both projections. The current partially implemented
ADR 0045 substrate may reorder logical edges only after choosing one internal order per pair key
and applying that identical order to both projections. After this ADR is complete, ADR 0049 replaces
the unshipped unordered public contract with one input-order-preserving batch contract. Independent
sorting of the two projections is forbidden in every phase.

Compaction may renumber slots but preserves relative live order for every pair key. A
transformation that cannot prove this property rebuilds the affected pair relation from a shared
logical-ordinal plan before publishing it.

### 3. Return exact slots from insertion

Plan 0129 implements the internal batch path's physical-location return for every logical ordinal.
Plan 0130 makes that return opt-in: ordinary batch writes use aggregate-only results, while
capture mode remains available for the future mate-index consumer. Scalar return integration and
persistent mate-index consumption remain planned.

Plan 0132 implements the persistence-free part of this boundary. The bidirectional LARA wrapper
now exposes an internal `PhysicalEdgeRef`, exact rank-based `mate_of`, and canonical-handle
resolution for directed, undirected, and self-loop relations. Its live-slot primitive now reads a
bucket's slab/log representation in one pass. Scalar GraphStore insertion consumes the exact
forward and reverse locations returned by the bidirectional LARA write for named buckets; bypass,
default-label, and unsupported paths retain the alias/scan fallback. The Graph facade keeps
`EDGE_ALIASES` as the compatibility and recovery surface; alias removal is not implied by this
slice.

The paired one-edge lookup probe measures approximately 2.3K instructions for alias lookup,
4.3K for post-insert adjacency rediscovery, and 12.5K for the current ScanOnly implementation;
the latest run reports ScanOnly at +5.8% versus its prior baseline while alias and rediscovery
remain within noise. The scalar location slice removes post-insert rediscovery on supported
named-bucket paths. These instruction results are a guardrail, not the primary alias-removal
criterion: the primary objective is reducing persistent bytes per edge, with Sampled/Packed
metadata expected to recover acceptable lookup cost in promoted buckets. GraphStore must not scan
for the most recently matching neighbor or payload after an insertion that returned an exact
location.

The location-producing EdgeStore path derives a log-backed row's next logical slot from the
existing stored degree. It must not rescan the slab or overflow-log chain for every scalar insert;
exact-location capture and update-path traversal remain separate concerns.

A GraphStore footprint probe that creates one source and 128 or 1,024 named directed edges
reports a total stable-memory increase of 16 Wasm pages in both cases. This is only a
MemoryManager allocation baseline and must not be divided by edge count. The alias index's raw
serialized payload is 18 bytes per entry (10-byte key plus 8-byte value), excluding B-tree node
and allocator overhead. Future Sampled/Packed measurements must report this raw payload baseline
plus separately measured node and region overhead.

Plan 0142 adds a read-only owner boundary that discovers existing non-default buckets on one
orientation/PMA leaf, derives canonical source/mate slots by equal-neighbor occurrence rank, and
feeds the existing admission and rebuild boundary. The aggregate is structurally revalidated
before publication. Plan 0143 adds the owner-level invalidation boundary: successful canonical
inserts, deletes, compaction work, and vertex purge work hide affected Published rows before
scheduling one deduplicated `(orientation, leaf)` maintenance item. Compaction invalidates before
processing an eligible row so failure or a later slot rewrite cannot expose stale data; failed
compaction work is requeued for retry. ScanOnly rows do not create unnecessary work; rebuild
failures leave the row non-Published and the item retryable.

The validated Published Sampled/Packed lookup primitive is implemented but dormant: it checks blob
results against canonical rank/select and falls back exactly once on malformed or stale data.
Plan 0176 adds the measurement-only adoption selector. It is fail-closed: low-degree, cold, and
self-loop buckets stay `ScanOnly`; undirected buckets may select only `PairRank`; directed,
parallel, sparse-slot, and mixed-label buckets may select a gated SharedOrientation or
rank-indexed candidate only when exactness, logical-byte, and bounded-runtime evidence is present.
Missing evidence is `Deferred`, not promotion. Ordinary-caller activation and alias replacement
remain deferred.

The bidirectional owner is also the mutation boundary: `forward()` and `reverse()` remain public
read accessors, while single-orientation canonical mutation methods are crate-private. GraphStore,
repair, and batch paths use owner-facing wrappers so a forward/reverse mutation cannot bypass mate
invalidation. Batch commit invalidates all affected owner leaves before its first canonical write;
repair-only one-orientation wrappers do the same and are not general-purpose paired-write APIs. This
visibility split changes no read traversal or diagnostic API; it only removes direct mutation through
an orientation handle.

The internal result distinguishes one-entry and two-entry cases:

```rust
enum InsertedEdgeLocations {
    SelfLoop {
        canonical: PhysicalEdgeRef,
    },
    Pair {
        canonical: PhysicalEdgeRef,
        mate: PhysicalEdgeRef,
    },
}
```

Batch results associate locations by bounded chunk-local logical ordinal. This is internal heap
data and does not require returning one handle per edge in a public replicated response.

### 4. Use an adaptive leaf-owned mate accelerator

Every bucket begins in `ScanOnly` mode and stores no per-edge mate data. LARA may promote a large or
frequently accessed bucket to `Sampled` or `Packed` mode. Promotion uses existing structural facts
such as `LabelBucket::degree`, leaf occupancy, and scan distance plus optional heap-only heat
counters. Access frequency is not persisted. The three modes are:

```text
ScanOnly:
  no mate array; exact rank/select scan

Sampled:
  a checkpoint every K pair entries; checkpoint rows resolve directly and non-checkpoint rows use
  canonical fallback in the current runtime primitive

Packed:
  a counterpart slot for every indexed entry
```

`Sampled` checkpoints store the source and counterpart slots. The checkpoint ordinal is implicit in
its position in the checkpoint array:

```text
checkpoint = (source_slot, mate_slot)
```

Given a physical source handle, the current runtime primitive resolves a source handle directly
only when its rank is represented by a checkpoint; all other Sampled requests use canonical
fallback because the current wire form stores no intermediate pair lanes. A future format change
may add bounded local scanning, but that is outside this slice. `K = 32` or `64` is an initial
benchmark candidate, not a stable wire contract.

`Packed` mode stores only the counterpart `slot_index` for every indexed entry. Counterpart owner,
label, orientation, and directedness derive from the source entry, its target, and its bucket. Slot
values use the smallest width covering the indexed bucket:

| Width code | Bytes per indexed half |
| --- | ---: |
| `U8` | 1 |
| `U16` | 2 |
| `U24` | 3 |
| `U32` | 4 |

Sampled checkpoints and Packed arrays are grouped into a versioned blob per indexed PMA leaf. A
blob contains a header, a directory of indexed buckets only, and bucket-local arrays in live order.
The header records `mode`, `checkpoint_stride`, `entry_count`, and width codes. Exact directory
field packing is implementation- and benchmark-selected; it remains bounds-checked and
self-describing by version and mode.

An indexed leaf may mix modes by bucket. A high-degree bucket therefore first receives a sampled
index if bounded scanning is cheaper than a full array; only a hot or scan-expensive bucket receives
full Packed coverage. A small bucket in the same leaf can remain ScanOnly.

#### Cardinality admission and directory omission

The admission unit is the live logical edge count of one `(orientation, leaf, owner, label)` bucket;
physical slab slots, tombstones, and payload bytes do not satisfy this floor. The initial policy
constants are:

```text
PROMOTE_MIN_LIVE_EDGES = 32
DEMOTE_MAX_LIVE_EDGES  = 16
```

They are policy defaults, not wire-format values, and must be confirmed by the byte/instruction
and read/update amortization gates before ordinary activation. A bucket with fewer than
`PROMOTE_MIN_LIVE_EDGES` live edges is definitively `ScanOnly`, regardless of its leaf or
neighboring buckets. A bucket at or above the promote floor is only eligible for `Sampled` or
`Packed` after the exactness, stale-safety, byte, and request-volume gates also pass. A published
bucket is demoted when its live count reaches `DEMOTE_MAX_LIVE_EDGES` or any gate fails. The gap
between the floors prevents rebuild thrashing around one boundary; a failed rebuild also stays
`ScanOnly` until a later admission attempt crosses the promote floor.

`ScanOnly` buckets are omitted from the blob directory entirely. The directory is therefore a
sparse directory of indexed buckets, not a complete catalog of canonical buckets. If a leaf has
no indexed buckets, no blob is allocated and its five-byte leaf locator remains `ScanOnly`. If
other buckets in the leaf are indexed, only those buckets contribute directory entries and blob
payload; canonical LARA remains the source of truth for omitted buckets. A directory entry is
never used to represent a negative/ScanOnly decision. Plan 0172 implements and tests this policy
in the measurement-only adoption gate and sparse footprint accounting; it does not activate
persistent admission or change the production codec.

Plan 0176 makes the measurement-only selector matrix explicit. The policy is evaluated per bucket,
not per leaf or per graph:

| Stratum | Candidate when all gates pass | Fallback when a gate fails |
| --- | --- | --- |
| directed, non-self, dense | SharedOrientation, otherwise rank-indexed Packed | ScanOnly |
| undirected, non-self, dense | PairRank | ScanOnly |
| directed/undirected self-loop | none | ScanOnly |
| low-degree or cold | none | ScanOnly |
| parallel, sparse-slot, mixed-label | SharedOrientation or rank-indexed Packed only with current matched evidence | ScanOnly; `Deferred` while evidence is absent |

The gates require exact canonical parity, malformed/stale fail-closed behavior, logical bytes no
larger than the active alias baseline, and a bounded request-time instruction result. Stable-memory
page deltas are recorded separately as allocator observations and cannot satisfy the byte gate.
This matrix is a measurement contract only; ordinary callers remain on the canonical/alias path.

Plan 0177 adds an aggregate status over the ten required fixture rows. `Adopt` requires one unique
row for every stratum with exact parity, fail-closed fallback, and passing logical-byte/runtime
gates. Missing or unsafe evidence yields `Hold`; complete but performance-failing evidence yields
`Partial` and keeps the canonical path. The current evidence set does not authorize ordinary-caller
activation.

Packed arrays may reserve bounded geometric capacity. An insertion fitting the current width and
capacity updates one packed word for each physical half. Sampled insertion updates a checkpoint
only when a stride boundary is crossed; otherwise it remains scan-backed. Width/capacity growth,
promotion, demotion, checkpoint-boundary changes, and slot-renumbering compaction rebuild the
affected leaf blob once. A delete may leave an unreachable sampled or packed cell until the next
leaf rebuild because adjacency tombstones remain the liveness authority.

### 5. Store one fixed locator row per orientation and leaf

The bidirectional LARA wrapper owns one shared `MateLeafLocatorStore`. Its dense row key is
`(orientation, leaf_index)`, encoded by deterministic row position such as
`2 * leaf_index + orientation_bit`. Each row is a tagged five-byte `u40`:

```text
0      ScanOnly: no blob
1      Rebuilding: sampled/packed data must not be read; use scan fallback
n >= 2 Sampled or Packed blob: byte offset = n - 2
```

No persistent generation, delta length, indexed-bucket count, or hotness is stored. The mode and
checkpoint stride belong in the blob header, not the locator. Existing adjacency and PMA metadata
remain authoritative for degree, liveness, and leaf geometry.

Implement `MateLeafLocatorStore` as a dedicated fixed-row stable vector modeled on `VertexStore`,
`SegmentSpanMetaStore`, and `SegmentEdgeCountsStore`, not
`ic_stable_structures::StableVec`:

- magic, layout version, logical length, and stride in a fixed header;
- direct `offset = DATA_OFFSET + 5 * row_index` addressing;
- a heap mirror of persisted length;
- exact five-byte reads/writes using the existing `read_u40`/`write_u40` pattern;
- `reserve_to` before a canonical commit; and
- typed reopen errors for magic, version, stride, and backing-size mismatch.

Do not introduce a new generic vector abstraction. `VertexStore<V>` is constrained by
`CsrVertex`; broadening it or generalizing all fixed-row stores would enlarge this change without
improving mate lookup.

Variable-size mate blobs use a separate byte store. Replaced ranges use a mate-blob-specific
instance of the existing LARA `FreeSpanStore` implementation and its by-start index. They do not
share data or address space with edge or payload free-span stores. The operation is:

1. allocate a new byte span by best fit, or append at the mate-blob tail;
2. write and validate the new blob;
3. publish the new five-byte locator; and
4. only then retire the old blob span for coalescing and reuse.

This avoids shifting later blobs and avoids append-only stable-memory leakage. The locator, blob,
free-span records, and by-start index form one composite layout and reopen all-or-nothing. Because
mate data is derived, a valid `ScanOnly` locator is always a correct recoverable state.

### 6. Do not add mate fields to existing rows

Existing metadata is reused for decisions but not enlarged:

| Existing metadata | Reuse | Reason not to add a mate field |
| --- | --- | --- |
| `LabelBucket` (29 bytes/bucket) | degree, label key, live order | charges every small `ScanOnly` bucket |
| `LabeledVertex` (21 bytes/vertex/orientation) | leaf and bucket ownership | about `segment_size` times more rows than leaf locators |
| `SegmentEdgeCounts` (16 bytes/node) | density and promotion input | includes internal PMA nodes, not only leaves |
| `SegmentSpanMeta` (8 bytes/leaf) | physical placement | different lifecycle; combining couples unrelated recovery and scan-isolation contracts |

The separate locator costs five bytes only at leaf/orientation granularity and lets placement work
avoid reading or rewriting mate state.

### 7. Keep canonical sidecars in GraphStore

`EDGE_PROPERTIES` remains keyed by canonical physical `EdgeHandle`:

- directed properties use the forward handle;
- non-self undirected properties use the handle owned by the larger vertex id; and
- undirected self-loop properties use the sole handle.

Inline values remain mirrored physical payloads. Inline update and logical edge deletion call
`mate_of` and update or remove the exact pair, or only the sole self-loop entry. Property lookup
from a non-canonical entry first calls `canonical_handle`.

Edge slots and payload slots are independent physical domains. Their association is the
bucket-local live ordinal, not the numeric edge slot, edge-log entry index, payload-log entry
index, or payload blob location. For every inline-value operation, LARA must therefore:

1. resolve the edge handle to its current bucket-local live ordinal;
2. apply the same ordinal to the corresponding payload sequence;
3. call `mate_of` to resolve the paired edge, then resolve the paired bucket's current live
   ordinal independently; and
4. update, remove, or fold both payload values in the same no-await commit.

Payload log entries and edge log entries must never be paired by entry index. On deletion, the
payload ordinal is removed or folded before the edge tombstone becomes visible. On compaction,
edge and payload sequences may move to different physical locations, but both preserve the same
live ordinal order. A directed mirror or non-self undirected half therefore receives the exact
inline bytes of its logical mate; an undirected self-loop updates or removes one payload value
once.

Ordinary leaf slide/rebalance preserving bucket-local slot identities requires no mate or property
repair. Slot-renumbering maintenance emits its existing slot moves; LARA rebuilds packed mate blobs
for affected leaves before publishing clean locators, while GraphStore repairs canonical property
keys only for canonical slot moves. The facade no longer repairs alias keys or targets.

Reverse-adjacency differential repair rebuilds affected pair ranks and packed mate leaves from
canonical forward rows. It must not use first-match parallel-edge association.

### 8. Separate physical counts from mathematical degree

An undirected self-loop is stored once. APIs name which quantity they expose:

```text
physical adjacency entries     = non-loop entries + self-loops
mathematical undirected degree = non-loop incidences + 2 * self-loops
```

LARA capacity, compaction, and scan-cost planning use physical counts. Graph algorithms and
statistics requesting mathematical degree add the second self-loop incidence or expose an
incidence iterator that duplicates it logically. No second physical row is created solely to make
degree equal physical iterator length.

## Storage and operation cost

Estimates exclude stable-memory-manager extent rounding and blob free-span bookkeeping.

### Fixed metadata

With `segment_size = 16` and one million vertices:

```text
62,500 leaves/orientation * 2 orientations * 5 bytes = 625,000 bytes
```

This is about 0.60 MiB of logical locator bytes. A `u64` row would cost 1,000,000 bytes. Adding
five bytes to every `LabeledVertex` would cost 10,000,000 bytes across both orientations.

### Mate mapping storage

For `Sampled` with stride `K`, each checkpoint contains two slot values. With `u32` slots, its
amortized mapping cost is `8 / K` bytes per indexed entry; with `u16` slots it is `4 / K` bytes.
At `K = 32` this is `0.25` or `0.125` bytes per entry, before the blob directory and header.

For one indexed physical half, the mapping cost is:

| Mode / width | Dense bytes / indexed half | At 1.25x reserved capacity |
| ---: | ---: | ---: |
| `Sampled`, `K=32`, `U32` | 0.25 | n/a |
| `Packed U8` | 1 | 1.25 |
| `Packed U16` | 2 | 2.5 |
| `Packed U24` | 3 | 3.75 |
| `Packed U32` | 4 | 5.0 |

`ScanOnly` entries and undirected self-loops require zero mapping bytes. For a non-self logical edge
with both halves indexed, multiply the per-half mapping cost by two. The current one-way B-tree
stores 18 raw key/value bytes per indexed logical edge before node overhead; two B-trees would
store at least 36 raw bytes.

### Logical footprint accounting (Plan 0133)

The following table is a storage decision aid, not a stable-layout measurement. It charges both
physical halves of a non-self logical edge and includes the two five-byte locator rows amortized
over `n` entries. It excludes each blob's header, indexed-bucket directory, free-span metadata,
rebuild reserve, and StableBTreeMap/MemoryManager overhead; those terms remain explicit unknowns
until a storage prototype exists.

For `Sampled`, the exact variable term is `16 * ceil(n / K) + 10` bytes per two-half bucket
(`8` bytes per checkpoint per half, plus two locator rows). For `Packed`, it is `2 * width * n +
10`, where `width` is the slot width in bytes. Values below are bytes per logical edge:

| Entries `n` | Sampled K=16 | Sampled K=32 | Sampled K=64 | Packed U8 | Packed U16 | Packed U24 | Packed U32 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 26.00 | 26.00 | 26.00 | 12.00 | 14.00 | 16.00 | 18.00 |
| 8 | 3.25 | 3.25 | 3.25 | 3.25 | 5.25 | 7.25 | 9.25 |
| 32 | 1.31 | 0.81 | 0.81 | 2.31 | 4.31 | 6.31 | 8.31 |
| 128 | 1.08 | 0.58 | 0.33 | 2.08 | 4.08 | 6.08 | 8.08 |
| 1,024 | 1.01 | 0.51 | 0.26 | 2.01 | 4.01 | 6.01 | 8.01 |
| 65,536 (hub example) | 1.00 | 0.50 | 0.25 | 2.00 | 4.00 | 6.00 | 8.00 |

The alias comparison remains exactly 18 raw bytes per non-self logical edge. Sampled and Packed
U8/U16/U24 beat that raw payload for sufficiently large buckets even before shared overhead is
added; U32 only ties it in the large-bucket limit and is not a storage win once shared terms are
included. Small buckets should remain `ScanOnly`. This table does not authorize promotion or alias
removal: the storage gate must add the measured shared terms, and instruction results remain a
bounded guardrail.

### Logical footprint prototype (Plan 0134)

The pure accounting model in `crates/graph/src/bench/mate_footprint.rs` is the owner of the
prototype arithmetic. It covers one non-self logical edge represented by two physical halves and
returns these components independently: two five-byte locator rows, blob header bytes, indexed
bucket-directory bytes, checkpoint/packed mapping bytes, free-span bytes, and rebuild-reserve
bytes. The latter four are explicit inputs because the blob wire layout and allocation records are
not implemented yet. StableBTreeMap node bytes, allocator slack, and MemoryManager extent rounding
are deliberately outside the model and are never converted into bytes per edge.

For a candidate with `n` entries, the storage gate is algebraically:

```text
known_mate_bytes = 10 + shared_header + shared_directory + mapping
                   + shared_free_span + shared_rebuild_reserve
unknown_overhead_budget = 18 * n - known_mate_bytes
```

`unknown_overhead_budget` is reported only when positive. A non-positive value rejects the
candidate before any runtime implementation is proposed. The prototype tests all requested
degrees (`1`, `8`, `32`, `128`, `1,024`, and the `65,536` hub case), Sampled strides `16/32/64`,
and Packed widths `U8/U16/U24/U32`, including checked overflow and unsupported-parameter rejection.
The zero-shared-overhead table above is therefore a reproducible lower bound, not an adoption
decision; a follow-up storage prototype is justified only when measured shared terms leave a
positive budget for the target workload.

### Isolated serialized layout prototype (Plan 0135)

The internal `ic-stable-lara::labeled::bidirectional::mate_blob_prototype` makes the shared blob
terms concrete without exposing a runtime promotion API. Its fixed-endian layout is:

| Component | Size |
| --- | ---: |
| compact header (`bucket_count`, `total_length`) | 8 bytes |
| indexed-bucket directory entry (`owner_vertex_id` + `BucketLabelKey` identity) | 15 bytes per bucket |
| Sampled mapping | `8 * ceil(n / K)` bytes per bucket (source and mate `u32` fields); a two-half pair therefore contributes `16 * ceil(n / K)` |
| Packed mapping | `2 * width * n` bytes per bucket |

The header declares the total length. Mode, stride/width, and entry count
are per-directory-entry fields because a leaf may mix modes by bucket; no synthetic bucket-id table
is introduced. Directory entries carry the canonical `(owner_vertex_id, BucketLabelKey)` identity,
are strictly ordered, and point to contiguous mapping ranges. Decode checks every range, count,
mode, width, reserved flag, and the absence of trailing bytes. Free-span records, rebuild reserve,
locator rows, and substrate allocation remain separate terms in the Plan 0134 gate. Round-trip and
corruption tests cover all requested strides and widths, single-bucket and multi-bucket leaves.
Plan 0136 places this codec behind an internal locator/blob/free-span storage boundary with
fresh/reopen/partial-layout validation, publication ordering, span retirement, and
locator-to-blob reopen validation. This remains dormant storage foundation; it is not runtime
promotion or alias removal.

### Reads

- `ScanOnly`: scan the source bucket to compute equal-neighbor rank and the target bucket to select
  that rank. Approximate edge-row traffic is `4 * (source_slots + target_slots)` bytes.
- `Published`: read the locator and addressed blob directory. Packed rows are searched by encoded
  source slot and checked against the live counterpart row and relation counts. Sampled checkpoints
  are used when the requested rank is represented; non-checkpoint requests use the canonical
  fallback because the current Sampled wire form stores no intermediate pair lanes.
  Malformed or stale data is rejected before one canonical fallback, and no blob-derived handle is
  exposed.
- Ordinary adjacency traversal reads no mate metadata.

Two 32-entry buckets imply about 256 edge bytes of full scan traffic; a sampled `K=32` lookup is
bounded in pair-rank work but remains sensitive to interleaved rows. Two 1,024-entry buckets imply
about 8 KiB of full scan traffic. Promotion is therefore adaptive rather than universal.

### Writes

- Inline-value or property update: zero mate writes.
- Packed insert with unchanged width/capacity: one aligned packed-word read/modify/write per half;
  with 64-bit words, approximately 16 bytes read plus 16 bytes written across the pair.
- Sampled insert without crossing a checkpoint stride: no mapping write. Crossing a stride or
  changing pair rank marks the sampled blob for rebuild; reads use scan fallback while rebuilding.
- Delete without slot renumbering: zero immediate mate writes when the cell remains unreachable
  behind an adjacency tombstone. A packed move or other slot-renumbering delete rebuilds the
  affected packed leaf mappings.
- In-window rebalance preserving slot identity: zero mate writes.
- Promotion, growth, slot-renumbering compaction, and reverse repair: contiguous
  `O(indexed half-edges in affected leaves)` rebuild.

Thresholds and capacity factors are not stable format. Canbench selects them and they may change
without migration.

## Failure atomicity and consistency

Adjacency plus pair order is canonical; sampled/packed mate data never makes an edge live. Before
changing adjacency or a clean sampled/packed locator, LARA reserves all required fixed rows, blob bytes, and
free-span records. Commit order is:

1. mark a sampled/packed locator `Rebuilding` when work can span maintenance steps;
2. write or rebuild adjacency and mate blob bytes;
3. validate bounds, pair counts, and reciprocal slot mapping;
4. publish the sampled/packed locator, or `ScanOnly` if acceleration is dropped; and
5. retire the previous blob only after the new locator is visible.

Single-message commits may omit an externally visible rebuilding phase when trap rollback and
preflight make the mutation atomic. No successful return leaves a locator pointing at stale slots.
Reads seeing `Rebuilding` use rank/select.

## Stable layout and migration

Implementation adds four logical regions owned by bidirectional LARA:

1. `MATE_LEAF_LOCATORS` — fixed five-byte rows;
2. `MATE_BLOBS` — versioned sampled/packed leaf blobs;
3. `MATE_FREE_SPANS` — retired blob byte ranges; and
4. `MATE_FREE_SPAN_BY_START` — coalescing index.

`EDGE_ALIASES` is removed, for a net increase of three Graph stable regions. The development
implementation assigns the four regions to Graph `MemoryId`s 47–50. Forward and reverse locator
rows share one store because the bidirectional wrapper owns their joint invariant; the row key is
`2 * leaf_index + orientation_bit`, and no two collections are initialized on one `MemoryId`.

There is no in-place migration from `EDGE_ALIASES`. Implementation lands at a fresh-install
boundary and development stable data is recreated. Production adoption remains gated by ADR 0039.

## Alternatives considered

### Keep the one-way alias B-tree

Minimum change, but canonical-to-mate operations remain full-map scans and insertion still
rediscovers slots. Rejected.

### Add a second reverse B-tree

Provides logarithmic lookup both ways but at least doubles the 18-byte raw row payload and repair
work. Rejected.

### Store a persistent edge id in every physical row

Makes pairing direct but enlarges the four-byte traversal row and still needs id-to-location
lookup. Rejected.

### Always derive mates by rank/select

Uses no metadata and remains the correctness fallback. Rejected as the only path because
high-degree parallel buckets make updates and deletes linear in both adjacencies.

### Store a mate slot in every edge row or `LabelBucket`

An edge field charges all traversal storage. A bucket field cannot encode one counterpart per
parallel edge and charges small buckets. Rejected.

### Add the locator to PMA metadata

`SegmentEdgeCounts` includes internal nodes. `SegmentSpanMeta` has the right cardinality but a
separate placement lifecycle. Combining it with mate state saves a small region/header cost while
coupling format changes and recovery. Rejected in favor of an isolated five-byte column.

### Use `ic_stable_structures::StableVec`

Functionally viable, but a dedicated fixed-row store gives exact five-byte I/O, LARA-aligned
header/stride validation, a length mirror, and preflight reservation without adopting a second
vector convention. Rejected for this column.

### Use append-only blobs without free spans

Suitable for a short-lived prototype, but promotion, width growth, and compaction permanently leak
old blob bytes. Rejected as the final layout. A prototype may measure append-only packed blobs
before wiring the dedicated `FreeSpanStore`, but `EDGE_ALIASES` is not removed until reclamation is
implemented.

### Use fixed-size pages per leaf

Eliminates variable allocation but reserves worst-case width/capacity for `ScanOnly` leaves.
Rejected because it defeats adaptive storage.

## Consequences

Positive:

- Exact mate lookup remains available from either physical half.
- Small/cold buckets pay no per-edge metadata.
- High-degree buckets can use compact sampled lookup; hot or scan-expensive buckets can use full
  Packed lookup.
- Known insertion slots eliminate post-insert neighbor scans.
- Slot allocation and mate repair have one owner and commit boundary.
- Four-byte edge rows and canonical physical-handle properties remain intact.
- Undirected canonical ownership and one-entry self-loops are explicit.

Costs and risks:

- Pair-rank preservation becomes a mandatory LARA write invariant.
- Four LARA regions replace one facade region.
- Sampled/Packed allocation, reopen validation, and rebuild add implementation complexity.
- Scan fallback can be expensive before promotion or while rebuilding; bounded instruction
  regression is accepted when it buys the intended persistent-byte reduction.
- Existing reverse repair is count-exact, not pair-exact, for parallel edges and must be
  strengthened during implementation.

## Test contract

Implementation covers:

- directed fan-out/fan-in and directed self-loops;
- undirected larger-vertex canonical ownership and one-entry self-loops;
- physical count versus mathematical degree;
- parallel edges with distinct inline values and exact update/delete from either half;
- edge/payload slots in different physical domains, including slab/log/blob combinations, with
  ordinal-based synchronization and no edge-log/payload-log index pairing;
- scalar and the current ADR 0045 batch substrate returning exact per-ordinal
  locations, followed by ADR 0049 input-order-preserving batch coverage after
  this ADR's completion gate;
- identical pair-key order across the current ADR 0045 projections and the
  future ADR 0049 merged physical projections;
- `ScanOnly`, promotion, all widths, growth, demotion, and rebuilding fallback;
- slab/log combinations;
- rebalance with zero repair and slot-renumbering compaction with leaf rebuild;
- canonical property-key repair;
- sampled/packed reverse repair restoring pair rank, payloads, and mappings;
- fresh/reopen/partial-layout and corrupt locator/blob bounds;
- failpoints around locator publication and old-blob retirement; and
- complete removal of facade alias dependencies.

## Benchmark contract

Canbench compares rank/select, sampled lookup at `K = 16/32/64`, and packed lookup at bucket
degrees 1, 8, 32, 128, 1,024, and larger hub sizes, with unique and parallel neighbors. Measure
stable reads/writes and instructions for `mate_of`, inline update, delete, scalar insert, and batch
insert; promotion/rebuild amortization; checkpoint stride and width transitions; compaction and
reverse repair; and logical bytes plus stable-memory pages for sparse, mixed, and hub-heavy graphs.

Promotion thresholds are selected from end-to-end update/delete cost, not only lookup
microbenchmarks.

## Design documentation impact

- ADR 0045 delegates physical pairing and returned-slot requirements to this ADR.
- ADR 0026 remains the implemented repair contract until this ADR lands; its successor must restore
  exact pair rank and mate acceleration.
- `design/storage/lara.md` records the Plan 0136 dormant storage foundation and planned mate
  resolution at the bidirectional LARA boundary.
- `design/storage/lara-and-facade.md` moves mate ownership from Graph facade to LARA while retaining
  canonical properties in GraphStore.
- `design/storage/labeled-edge-inline-values.md` records `mate_of` as the planned exact mirrored
  update path.
- `design/storage/stable-memory-inventory.md` records the four-region dormant bundle without
  changing current implemented region counts.
- Plan 0145 adds a side-effect-free adoption-gate policy/accounting harness: the adaptive
  precedence, dense locator charging, explicit denominators, and mode-specific fallback contract
  are centralized before fixture measurement. This is measurement infrastructure only; the gate
  outcome, ordinary-caller activation, and alias replacement remain deferred. The accounting
  accepts fixture-provided physical-half and alias-row counts so self-loops are not charged using
  the non-self two-half assumption. Unknown overheads must carry an explicit proven finite bound,
  and per-edge reporting uses conservative ceiling division.

Plan 0146 adds the first measurement-boundary slice in
`crates/graph/src/bench/mate_adoption_gate.rs`: deterministic shape descriptors, canonical identity
encoding/digests, fixed-seed request identities, and a deferred/measured evidence schema with
fail-closed validation. This slice is a schema and fixture prototype only; it does not claim
independent stable-memory ownership, lifecycle coverage, ordinary-caller activation, or adoption.

Measurement-only LARA fixtures must allocate usable `MemoryId`s from `254` downward (`255` is an
internal `MemoryManager` marker). The production Graph layout owns the low-ID range, so
benchmark/test allocation must not consume IDs from that range even when the fixture uses a fresh
in-memory `MemoryManager`.

Plan 0147 implements this first ownership boundary in `ic-stable-lara` with a fresh
`MeasurementMemoryBundle` per candidate and a non-interference test. AliasOnly fixture
construction now populates real bidirectional LARA and extracts physical identity rows;
Graph evidence now emits Published rows for the promotion-eligible directed-high, parallel, and
undirected-high topologies; other Published shapes remain deferred because promotion eligibility is
shape/policy dependent. The owning layer has independent Published fixtures for those topologies, while ScanOnly has a
canonical-adjacency fixture with no mate metadata. Deferred evidence emits separate ScanOnly rows
for representable shapes and records exact logical identity-envelope bytes separately from
stable-memory page deltas. The AliasOnly builder is
exposed only through the `adoption-fixtures` feature, which Graph enables only for `canbench`.
The Graph bench adapter now consumes those physical rows for both directed sizes plus parallel,
undirected, and undirected-self-loop AliasOnly fixtures. Deferred evidence rows use real identity
digests only for those supported shapes. Plan 0164 adds real mixed-label identities and real sparse
deletion-churn overflow-log locations through a feature-gated measurement reader; these remain
fixture evidence and do not authorize ordinary-caller activation.
Representation setup probes now expose construction-only costs separately: AliasOnly/ScanOnly
directed-high are about 17.7M instructions and 1,355 stable pages, while Published directed-high,
Published parallel, and Published undirected-high are about 59.8M/34.9M/48.0M instructions and
4,171 stable pages. These are fixture-construction measurements, not runtime lookup or adoption
results. Plan 0149 adds fixture-only runtime probes over 1,024 requests: ScanOnly versus Published
directed-high is 18.07M versus 44.84M instructions, undirected-high is 29.19M versus 62.55M, and
parallel is 696.30M versus 1.04B. The probes validate each Published result against canonical
rank/select and preserve zero stable-memory growth; the current Published path is therefore not
assumed to be faster, especially for parallel buckets where canonical validation remains costly.
Malformed/stale fallback-once behavior remains covered by the owner-level runtime tests. These are
runtime fixture measurements, not ordinary-caller adoption evidence or an activation decision.
Plan 0150 investigated a Packed source-slot binary-search replacement. The probe produced no
material instruction change, while removing the strict ordering check would weaken fail-closed
handling for malformed blobs. It is therefore not adopted; the current Published primitive stays
dormant and the canonical rank/select path remains authoritative.
Plan 0151 adds the first shape-specific logical-byte gate. Alias raw payload is 18 bytes per
non-self logical edge: 2,304 bytes for the 128-edge directed and undirected-high fixtures and 576
bytes for the 32-edge parallel fixture. Exact Published blob bytes are 3,776 (directed-high), 1,888
(undirected-high), and 104 (parallel), excluding separately charged locator/allocator overhead.
Thus the current Published blob is not smaller for directed-high but is smaller for undirected-high
and parallel; no shape is activated by this evidence alone.
Plan 0152 confirms the size trend: directed 32/64/128/256 logical edges use 944/1,888/3,776/7,552
blob bytes versus 576/1,152/2,304/4,608 alias bytes, so no crossover is reached. Undirected uses
472/944/1,888/3,776 versus the same alias series, and parallel uses 104/120/152/216 versus
576/1,152/2,304/4,608. These are exact fixture blob payloads with bucket counts recorded
separately; unsupported smaller or self-loop shapes are not extrapolated.
Plan 0154 now treats ScanOnly canonical rank/select instructions as a separate third baseline.
The directed 32/128/256 fixtures keep local degree at two, so their focused probes are all about
18.07M instructions for 1,024 requests and must not be interpreted as a degree-scaling series.
Same-bucket parallel probes add the degree-sensitive points: 32 edges 696.30M, 128 edges 10.14B,
and 256 edges 7.75B instructions in the 2026-07-24 focused run. These new measurements are not
persisted adoption artifacts yet; the non-monotonic parallel result requires a repeat/diagnostic
run before it can be used as a gate. No rank-indexed runtime path or ordinary caller is activated.
The same slice also removes the temporary row-vector pass from canonical `select_rank`; focused
scope results show that this is not the dominant cost, so it is retained as a correctness-preserving
cleanup rather than an adoption result. A dedicated `canbench-scopes` feature enables source and
counterpart scopes without compiling the full low-level benchmark export module. Plan 0155 then
replaced the remaining per-lookup logical-slot materialization with a direct range traversal owned
by LARA. Plan 0156 then reused the existing descending iterator/chunk-prefetch path through an
explicit `(logical_slot, edge)` adapter. The focused parallel-mid probes now report 46.90M /
138.68M / 213.78M instructions for 32/128/256 requests respectively, with zero stable-memory
growth. The existing ordinary descending hub scan remains on its original fast iterator path and
measures 15.27M instructions with zero heap/stable-memory growth (2.70% below its committed
baseline). This is a material ScanOnly traversal improvement, not a rank-indexed adoption signal;
the alias-versus-rank-indexed gate remains separate.

Plan 0154 freezes rank-indexed Packed as the only derived-format candidate. On identical
canonical rows, the 128-entry one-byte codec probe measured 27.25K encode instructions and
1,998 decode/validate/lookup instructions for rank-indexed, versus 49.49K and 2,588 for the
retired source/mate Packed prototype, with zero heap/stable-page growth. Its one-bucket wire
stores one mate slot per canonical rank (`44 + entries * width` bytes: 76/172/556 for 32/128/256
entries at widths 1/1/2). This codec evidence does not establish an alias-vs-rank-indexed
end-to-end runtime winner, so ordinary-caller activation remains deferred; alias remains the
active baseline and the source/mate Published path remains measurement-only.

The activation gate must treat `EDGE_ALIASES` as a partial counterpart map, not as a complete
non-canonical identity index: alias keys identify materialized counterpart rows and values point
to canonical handles. Alias misses therefore require canonical adjacency fallback and are a
separate runtime stratum from alias hits.

Plan 0157's first runtime stratum uses the real measurement `EdgeAliasIndex` and identical
physical identities. With 1,024 repeated requests, directed-high alias hits measured 16.36M
instructions versus 1.56M for decoded-once rank lookup; 32-edge parallel measured 11.51M versus
323.90K. These are hit-only probes and do not yet cover canonical-to-alias reverse lookup or
alias-miss fallback, so no activation decision follows from them.

Plan 0157 then measured the remaining current-alias read strata on directed-high fixtures:
canonical-to-alias reverse lookup costs 148.39M instructions per 1,024 requests, while direct
canonical `mate_of` fallback costs 13.47M; rank lookup remains 1.56M. The reverse result is a
consequence of the current full-map `find_alias_for_canonical` scan. These results still exclude
malformed/stale parity and physical allocation accounting, so ordinary-caller activation remains
deferred.
The rank fixture now verifies exact counterpart parity and fail-closed truncated/out-of-range
lookup behavior, while the alias-miss probe exercises canonical `mate_of` fallback. A combined
adoption decision still requires presenting these strata with byte/page accounting together.

Plan 0157 closes the first alias-vs-rank gate but defers ordinary-caller activation. Rank-indexed
wins the measured runtime strata (directed-high: 1.56M versus 16.36M alias-hit instructions;
13.47M canonical fallback; 148.39M canonical-to-alias reverse lookup; parallel-32: 323.90K
versus 11.51M alias-hit instructions), while the logical byte objective is topology-dependent:
directed-high 2,840 versus 2,304 alias bytes, undirected-high 1,560 versus 2,304, and parallel-32
128 versus 576. The probes use fresh measurement memory and zero page growth, not production
allocator accounting. Alias remains active; rank-indexed remains a future topology-aware
candidate pending production-layout measurement and directed-bucket policy.

Plan 0157 adds a measurement-only rank encoder over the existing AliasOnly physical identities.
For matching fixtures, rank-indexed payloads are 2,840 bytes versus 2,304 raw alias bytes for
directed-high, 1,560 versus 2,304 for undirected-high, and 128 versus 576 for 32-edge parallel.
These are logical payload values; page and allocator effects remain separate. The topology
dependence reinforces the byte-first gate, while alias-vs-rank runtime parity and fallback remain
the next required evidence before activation.

Plan 0158 starts the next policy slice: ScanOnly remains the conservative choice for low-degree or
cold buckets, rank-indexed is considered only for hot/dense buckets that pass both byte and runtime
gates, and compressed alternatives require proven random-access preconditions. Monotone-only
Elias–Fano, restart-point delta coding, and shared-orientation maps are measurement candidates;
none is activated by this plan.

The first Plan 0158 slice freezes the measurement-only selector thresholds: rank candidates require
at least 32 live entries and 64 observed requests, exact/fail-closed evidence, and both byte and
runtime gates. A compressed candidate additionally requires a proven monotone rank sequence and
must not regress either measured bytes or instructions; any failed condition selects ScanOnly.
The candidate slice also adds measurement-only restart-point signed delta sizing for arbitrary
rank sequences and monotone-only Elias–Fano sizing; Elias–Fano rejects non-monotone input rather
than reordering rank semantics. Neither model is a production wire format.
On real identity fixtures, directed-high produced 1,792 bytes for delta/restart and 1,800 bytes
for the conservative shared-orientation model, but only 3/128 sequences were monotone, so
Elias–Fano covered only 27 bytes without fallback. Parallel-32 produced 96 bytes of delta/restart,
84 bytes of shared orientation, and 32 bytes of Elias–Fano across both sequences. Elias–Fano
therefore requires per-bucket fallback and is not a universal candidate; shared orientation is a
smaller but still measurement-only model on the parallel fixture.
The focused candidate probes measured 635.20K instructions for directed-high and 25.98K for
parallel-32, with zero heap and stable-memory page growth. This is model-evaluation cost rather
than a production lookup result, so a separate bounded runtime gate remains mandatory.
The restart candidate now has a measurement-only bounded reconstruction model: lookup starts at
the nearest restart and reconstructs at most `restart_interval - 1` deltas. Exact parity and
u32-boundary behavior are covered; no serialized decoder or caller activation follows.
Focused restart reconstruction measured 8.59M instructions for directed-high and 690.02K for
parallel-32, with zero heap/stable-memory growth. Both exceed the corresponding decoded rank
lookup probes (1.56M and 323.90K), so restart/delta is deferred by the runtime gate despite its
smaller logical byte estimate on some fixtures.
The shared-orientation lookup model measured 672.22K instructions for directed-high and 175.43K
for parallel-32, with zero heap/stable-memory growth. It is below the corresponding rank probes
and its conservative logical size is also smaller (1,800 versus 2,840 bytes; 84 versus 128 bytes),
making it the only remaining compressed candidate that passes the measurement gates. No activation
follows: serialized locator design, malformed/stale fallback, and production-layout accounting
remain required.
The candidate has a measurement-only serialized round trip with strict header, pair ordering,
width, truncation, trailing-byte, and rank-parity checks. This is not a stable wire or persistence
contract.
The decoded candidate was checked against canonical occurrence-rank counterparts for every
physical identity in the directed-high and parallel-32 fixtures, including out-of-range rank
rejection.

Plan 0159 evaluates a stronger measurement-only sampled paired-residual model. Each endpoint pair
is divided into blocks of 8/16/32/64 ranks; each block stores bounded signed
`reverse_slot - forward_slot` residuals (8 or 16 bits) and falls back to raw mate slots when the
residual range does not fit. The lookup receives the canonical forward slot and reconstructs only
the requested rank, so the storage reduction is not allowed to remove canonical validation.
Current logical estimates are 2,696 bytes (21.06 B/edge) for directed-high and 60 bytes
(1.88 B/edge) for parallel-32 at block sizes 32/64. Higher-degree parallel fixtures show the
block-size effect (parallel-128: 276 -> 164 bytes; parallel-256: 532 -> 308 bytes for B=8 -> 64),
while directed-high remains flat because its endpoint pairs are mostly single-rank groups. Focused
lookup probes are 772.60K and 212.29K instructions, with zero heap/stable-memory growth. The
temporary codec now rejects malformed, truncated, and trailing input, but serializes only residual
blocks; raw fallback serializes an explicit reverse stream because absolute mate slots cannot be
derived by negating a residual. A bounded local-scan probe on parallel-32 measures 579.40K
instructions at B=8 and 1.26M at B=32/64, exposing the space/runtime trade-off. No production
adoption follows.

Plan 0160 adds a measurement-only selector branch for `SharedOrientation`: it may win only when
its logical bytes and measured lookup instructions pass the alias/ScanOnly gates; when ranked is
also valid, shared must additionally be no worse than ranked on both dimensions. The selector keeps
ScanOnly as the low-degree, low-request, or fail-closed
fallback; it does not persist or activate a mode. Common-fixture measurements and production
layout accounting remain pending.

The undirected-high fixture reports 1,560 bytes for rank-indexed and explicitly rejects the
directed-only shared-orientation model. Its policy path therefore remains rank-indexed or ScanOnly
until a separate undirected representation gate is completed.

Plan 0161's initial orientation-free pair-rank model reports 1,544 logical bytes (12.06 B/logical
edge) on the same undirected-high fixture, slightly below rank-indexed's 1,560 bytes. Pair-rank
lookup measures 721.26K instructions with zero heap/stable-memory growth, versus 945.74K for
undirected rank-indexed and 16.83M for ScanOnly. A synthetic 128-edge reordered exception pair
uses 1,044 logical bytes and 118.07K lookup instructions, with zero heap/stable-memory growth;
the measurement helper accepts the exception only under an explicit mismatch budget. These
exceptions remain benchmark-only and mutation maintenance is not implemented.

Plan 0162 adds a measurement-only block-local permutation fallback for reordered undirected pairs.
On a synthetic 128-edge reversal, logical metadata is 212/180/164/156 bytes for block sizes
8/16/32/64, versus 1,044 bytes for raw pair-slot exceptions. Focused lookup probes are 188.72K
instructions for each block size, with zero heap/stable-memory growth. This remains a candidate
measurement; production block maintenance and persistence are deferred.

### Topology policy synthesis (measurement-only)

The current evidence supports the following topology-specific precedence. This is a policy summary,
not production activation:

| topology | preferred candidate | fallback | rationale |
|---|---|---|---|
| directed or undirected, low-degree/cold | `ScanOnly` | — | no metadata cost; adaptive gate rejects low request volume |
| directed, dense/high-degree | `SharedOrientation` | `RankedPacked`, then `ScanOnly` | 1,800 B / 672.22K instructions versus 2,840 B / 1.56M for ranked on directed-high; shared remains measurement-only |
| directed, parallel/hot | `SharedOrientation` | `RankedPacked`; `Sampled` only when scan cost is acceptable | 84 B / 175.43K for shared, 128 B / 323.90K for ranked, 60 B for sampled but 1.26M bounded local-scan instructions |
| undirected, aligned non-self | `PairRank` | `RankedPacked`, then `ScanOnly` | 1,544 B / 721.26K on undirected-high versus 1,560 B / 945.74K for ranked; directed shared is not applicable |
| undirected, reordered non-self | `BlockRankPermutation` for bounded exceptions | raw exception, then `ScanOnly` | block fallback is only an exception representation; its 156 B at block size 64 is not comparable to the full 1,544 B fixture total |
| undirected self-loop | `ScanOnly` | — | one stored entry and no mate metadata |
| directed self-loop | directed policy after dedicated evidence | `ScanOnly` | forward/reverse semantics remain distinct; no undirected self-loop shortcut |
| sparse-slot or mixed-label buckets | `SharedOrientation` after per-bucket exactness/request/byte gates | `RankedPacked`, then `ScanOnly` | slot width and label-local degree are measured independently; no cross-label sharing |

The summary deliberately separates logical byte payload from allocator/page overhead and treats
all compressed candidates as dormant until mutation maintenance, stale detection, rebuild, and
stable-layout accounting are measured.

Plan 0163 verifies the self-loop cardinality contract directly: directed self-loops expose two
orientation rows, while undirected self-loops expose one row and require no mate metadata. Plan
0164 adds an isolated real two-label published fixture and a feature-gated measurement reader for
physical slab/log locations. The reader encodes overflow-log entry indices with a high-bit marker;
it is not a production API.

The topology fixture gate also confirms that the directed self-loop can be represented by the
directed rank adapter, while the undirected self-loop has no per-edge mate payload. The real
mixed-label fixture reports each label independently; no candidate is allowed to share metadata
across labels. Sparse-slot evidence now comes from real overflow-log locations rather than the
logical ordinal iterator.

The synthetic topology probes add sparse-slot evidence: sparse directed slots
measure 192 B for ranked versus 148 B for shared, with 302.38K ranked lookup and 171.32K shared
lookup instructions (ranked encoding alone is 81.12K). The real two-label fixture independently
measures 52 B shared versus 96 B ranked per label, with 350.59K shared versus 594.29K ranked
instructions for a matched low-degree 4-edge-per-label probe; its persisted ScanOnly counterpart is 15.94M
instructions for 1,024 requests. The earlier 190.77K/315.96K values remain synthetic
alternating-lookup evidence. The real sparse fixture measures 128 B ranked versus 84 B shared,
with 300.35K versus 175.43K lookup instructions for 32 live edges per orientation; its persisted
ScanOnly counterpart is 45.59M instructions for the same request count. All are measurement-only results;
they do not authorize cross-label metadata sharing or production activation.

Plan 0165 folds these real rows into the evidence adapter. The measurement policy is to evaluate
sparse slots and each mixed-label bucket independently: try `SharedOrientation` when the
request/degree, byte, and exactness gates pass; otherwise try `RankedPacked` only if its own gates
pass, and finally remain `ScanOnly`. This is a candidate precedence rule, not production
activation; no metadata may be shared across labels.

Plan 0166 adds measurement-only mutation traces derived from the real sparse and mixed fixtures.
Each trace applies one insert, delete, or reorder to the extracted physical identities and rebuilds
the candidate without mutating canonical LARA state. Across the three operations, SharedOrientation
rebuild costs 95.16K instructions for sparse slots and 23.08K for one mixed-label bucket, while
RankedPacked costs 235.15K and 43.90K respectively. The persisted ScanOnly baselines are 45.59M
and 15.94M instructions. Stale detection costs 468.95K and 217.17K respectively. The amortization helper charges these costs
against explicit read/update ratios: a candidate is rejected when savings are negative, and
malformed counterpart cardinality fails closed. These are rebuild probes, not persistent maintenance
or activation evidence.

Plan 0167 runs the same operation boundary through real fixture graphs: canonical insert, physical
location extraction, canonical delete, extraction again, and candidate rebuild. The sparse trace
measures 72.74K insert, 162.18K delete, 126.71K extraction (two passes), and 144.90K candidate
rebuild instructions. The mixed-label trace measures 60.96K insert, 35.70K delete, 26.19K
extraction, and 18.25K rebuild instructions. Fixture setup reports 4,171 stable pages, which is
measurement-fixture allocation rather than logical candidate bytes and is not production evidence.
These integrated probes remain feature-gated and do not publish or persist mate metadata.

Plan 0168 closes the measurement-only amortization gate. Using the integrated sparse cost of
506.53K instructions and the 45.59M ScanOnly baseline, SharedOrientation breaks even at one read
per canonical update. The mixed-label integrated cost is 141.10K against a 15.94M ScanOnly
baseline, also breaking even at one read per update. The final selector still requires byte
savings, exact/fail-closed evidence, and stale detection; any failed condition selects ScanOnly.
This is a measurement decision only and does not authorize persistence or ordinary-caller activation.

Plan 0170 implements only an isolated codec and fixture-backed stable map for the persistence
boundary; it does not allocate the production `MATE_*` regions or connect any caller. Canonical
LARA adjacency remains the sole source of truth; derived mate state is owned per
`(orientation, leaf, owner, label)` bucket. The production region has one fixed header containing
magic and format version. Locator metadata owns candidate kind, lifecycle state, topology identity,
canonical generation, cardinality, blob offset, and total length; each blob entry therefore carries
only one bounded candidate payload without repeated magic/version framing. The proposed bounds
are `MAX_MATE_BUCKET_ENTRIES = 65_535` and `MAX_MATE_BUCKET_PAYLOAD_BYTES = 2 MiB`; records
exceeding either bound are rejected and remain `ScanOnly`.

The fixture now models the alternative ownership with a fixed 32-byte region header, matching the
existing LARA byte-store header convention, a 22-byte fixed locator value, and a separate raw
payload region; reopen validates the region header before reading entries. The entry codec is 35
bytes before payload and derives payload length from `total_len`; it has no per-entry checksum.
That bucket-locator/raw-payload split remains an isolated evidence fixture, not the selected
production layout.

Plan 0171 reconciles the implemented owner with the earlier bucket-locator/raw-payload fixture:
the production baseline is the existing leaf-scoped locator plus one multi-bucket blob per leaf.
One locator row therefore covers all indexed buckets in an orientation/leaf; bucket identity is
resolved by the blob directory, not by a second locator collection. Reopen and publication remain
all-or-nothing across the four existing mate regions; format replacement is outside the scope of
format replacement is outside the scope of this pre-production layout. The next compaction target
is to keep the leaf locator unchanged, reduce the blob
header to an 8-byte `{bucket_count, total_length}` header, and use 15-byte bucket-directory entries:
owner vertex (4), label (2),
packed candidate/width flags (1), cardinality (4), and mapping offset (4). Mapping length is
derived from the next offset or blob end. Excluding the fixed 5-byte leaf locator and allocator/
MemoryManager overhead, the logical overhead is `13 + 15B` bytes per leaf with `B` buckets.
Plan 0175 implements this compact codec at
the existing `MateStorage` publication and reopen boundary. Persisted blobs now use the
8-byte/15-byte representation exclusively.

Plan 0173 measured the compact codec at 701 bytes for three indexed buckets and 6,847 instructions
with zero heap or stable-memory growth. Plan 0175 applies that format at the owner boundary; the
full `ic-stable-lara` canbench sweep remains blocked by the pre-existing
`bench_lara_deferred_bidirectional_insert_undirected_1024` `CollectAllocationOverflow` failure.

The lifecycle is `Empty → Rebuilding → Published` or `Stale`. A canonical mutation first makes the
derived record unavailable, commits canonical adjacency, then rebuilds from canonical rows and
publishes only after epoch, topology, cardinality, and candidate-shape validation. Any trap, interruption,
decode error, epoch/topology mismatch, or candidate invariant failure leaves the bucket in `Stale`/`ScanOnly`;
no partial payload is visible. Recovery resumes from canonical state and may discard the derived
record. Region version mismatch, locator range failure, cardinality mismatch, or candidate-shape
failure rejects an entry before exposing a published value. The derived region is separately
versioned from canonical LARA.

Plan 0160's first common-fixture comparison confirms the intended ordering: shared-orientation is
1,800 bytes / 672.22K instructions for directed-high and 84 bytes / 175.43K for parallel-32;
rank-indexed is 2,840 / 1.56M and 128 / 323.90K respectively. Sampled residual reaches 60 bytes
on parallel-32 at B=32/64 but its bounded local scan is 1.26M instructions. These remain
measurement-only values; the threshold gate and production layout accounting are not closed.

## Related

- [ADR 0001](0001-labeled-segment-slide.md): PMA leaf physical ownership and relocation.
- [ADR 0020](0020-deferred-maintenance-timer-drain.md): deferred LARA maintenance.
- [ADR 0026](0026-reverse-adjacency-differential-repair.md): implemented reverse repair.
- [ADR 0039](0039-production-stable-memory-evolution-and-upgrade-safety.md): production migration gate.
- [ADR 0045](0045-unordered-batch-graph-mutations-and-lara-placement.md): batch placement and logical ordinals.
- [ADR 0049](0049-input-order-preserving-batch-graph-mutations.md): planned
  input-order-preserving batch successor; implementation begins after this ADR's
  ordinary-caller adoption and alias-removal completion gate.
- [LARA storage contract](../storage/lara.md).
- [LARA and Graph facade](../storage/lara-and-facade.md).
