# 0048. Adaptive LARA mate index replaces Graph edge aliases

Date: 2026-07-23
Status: accepted (ScanOnly implemented; shared four-region mate ownership wired in Plan 0139; bounded promotion admission and pure leaf-blob construction started in Plan 0141; publication/runtime lookup, mutation invalidation, and alias replacement remain deferred)
Last revised: 2026-07-23
Anchor timestamp: 2026-07-23 14:13:50 UTC +0000

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

The repository has no production stable data requiring compatibility with the current layout.
Development data may be recreated when this decision is implemented.

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
ordinary callers retain the `EDGE_ALIASES` compatibility path until promotion and adoption are
approved. Rebuilding or unpublished locator rows do not participate in this bridge, so adjacency
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

All insert paths enforce the same pair order on both projections. An ordered batch additionally
preserves its public input-order contract. An unordered batch may reorder logical edges, but it
chooses one internal order per pair key and applies that identical order to both projections.
Independent sorting of the two projections is forbidden.

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

A GraphStore footprint probe that creates one source and 128 or 1,024 named directed edges
reports a total stable-memory increase of 16 Wasm pages in both cases. This is only a
MemoryManager allocation baseline and must not be divided by edge count. The alias index's raw
serialized payload is 18 bytes per entry (10-byte key plus 8-byte value), excluding B-tree node
and allocator overhead. Future Sampled/Packed measurements must report this raw payload baseline
plus separately measured node and region overhead.

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
  a checkpoint every K pair entries; scan at most K - 1 matching entries around it

Packed:
  a counterpart slot for every indexed entry
```

`Sampled` checkpoints store the source and counterpart slots. The checkpoint ordinal is implicit in
its position in the checkpoint array:

```text
checkpoint = (source_slot, mate_slot)
```

Given a physical source handle, lookup binary-searches the source checkpoints, scans forward to
establish the exact pair rank, then scans the mate bucket to resolve the counterpart. There are at
most `K - 1` matching pair entries between checkpoints; unrelated entries interleaved in the
bucket may add physical-row reads. This is exact for parallel edges while bounding the pair-rank
work introduced by sampled mode. `K = 32` or `64` is an initial benchmark candidate, not a stable
wire contract.

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
| versioned header | 24 bytes |
| indexed-bucket directory entry (`owner_vertex_id` + `BucketLabelKey` identity) | 20 bytes per bucket |
| Sampled mapping | `8 * ceil(n / K)` bytes per bucket (source and mate `u32` fields); a two-half pair therefore contributes `16 * ceil(n / K)` |
| Packed mapping | `2 * width * n` bytes per bucket |

The header declares the directory, mapping, and total lengths. Mode, stride/width, and entry count
are per-directory-entry fields because a leaf may mix modes by bucket; no synthetic bucket-id table
is introduced. Directory entries carry the canonical `(owner_vertex_id, BucketLabelKey)` identity,
are strictly ordered, and point to contiguous mapping ranges. Decode checks every range, count,
mode, width, reserved flag, and the absence of trailing bytes. Free-span records, rebuild reserve,
locator rows, and substrate allocation remain separate terms in the Plan 0134 gate. Round-trip and
corruption tests cover all requested strides and widths, single-bucket and multi-bucket leaves.
Plan 0136 places this codec behind an internal locator/blob/free-span storage boundary with
fresh/reopen/partial-layout validation, publication ordering, old-span retirement, and
locator-to-blob reopen validation. This remains dormant storage foundation; it is not runtime
promotion or alias removal.

### Reads

- `ScanOnly`: scan the source bucket to compute equal-neighbor rank and the target bucket to select
  that rank. Approximate edge-row traffic is `4 * (source_slots + target_slots)` bytes.
- `Sampled`: read the locator, blob directory, and nearest checkpoint, then scan at most `K - 1`
  matching pair entries on each side, plus any unrelated physical rows interleaved in the two
  buckets.
- `Packed`: read the five-byte locator, blob header/directory, one packed word, and candidate
  adjacency row. A heap directory cache may reduce this after validation.
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
- scalar and ordered/unordered batch insertion returning exact per-ordinal locations;
- identical pair-key order across unordered projections;
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

## Related

- [ADR 0001](0001-labeled-segment-slide.md): PMA leaf physical ownership and relocation.
- [ADR 0020](0020-deferred-maintenance-timer-drain.md): deferred LARA maintenance.
- [ADR 0026](0026-reverse-adjacency-differential-repair.md): implemented reverse repair.
- [ADR 0039](0039-production-stable-memory-evolution-and-upgrade-safety.md): production migration gate.
- [ADR 0045](0045-unordered-batch-graph-mutations-and-lara-placement.md): batch placement and logical ordinals.
- [LARA storage contract](../storage/lara.md).
- [LARA and Graph facade](../storage/lara-and-facade.md).
