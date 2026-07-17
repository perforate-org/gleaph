# 0043. Per-memory bucket sizing for the stable-memory manager

Date: 2026-07-16
Status: Partially Implemented
Last revised: 2026-07-17
Anchor timestamp: 2026-07-16 21:21:47 UTC +0000

## Context

`ic-stable-structures` gives a `MemoryManager` one bucket size for all of its
`VirtualMemory` regions. The upstream default is 128 Wasm pages (8 MiB). Gleaph's
Graph canister currently owns 47 regions, including the 32-region LARA bundle.
Consequently, a small region can reserve a full bucket even when it contains only
a few bytes.

Local social-demo measurements exposed both sides of this tradeoff. The
upstream-sized layout made the physical stable-memory footprint about 383 MiB for
only a few MiB of logical data. A fresh global 8-page layout reduced the footprint
to about 56 MiB, but leaves a 16 GiB ceiling for that manager. The former global
16-page setting raised that ceiling to 32 GiB, while still applying a 1 MiB minimum
allocation to every growing region. The current Graph wiring uses the experimental
per-memory policy described below. Preliminary property-heavy workloads also show
that properties can grow materially faster than adjacency; a single small global
bucket therefore risks making property storage the shard bottleneck.

The existing storage boundary is generic over `ic_stable_structures::Memory`.
`StableBTreeMap`, `StableCell`, and Gleaph's LARA storage must continue to use that
trait. A new manager must not define a look-alike trait or force every stable
structure to depend on a Gleaph-specific abstraction.

The design below is based on the `ic-stable-structures 0.7.2`
`memory_manager.rs` implementation. Its relevant pieces are `MemoryManagerInner`,
the fixed `Header`, the `memory_sizes_in_pages` table, the owner-byte allocation
table, the per-memory `memory_buckets` index, and `for_each_bucket`. The custom
crate should preserve these responsibilities and tests where they remain valid;
the physical bucket address calculation is the part that must change.

## Problem

Gleaph needs independent allocation granularity for regions with different growth
profiles, while preserving one shared stable-memory capacity and the existing
`Memory`-based storage APIs. Fixed partitions would avoid slack for some regions,
but would introduce artificial per-partition limits and prevent one region from
using unused capacity belonging to another.

## Decision

Plan an internal custom stable-memory-manager crate that provides a manager and
virtual-memory type compatible with `ic-stable-structures`:

- `VirtualMemory<M>` will implement `ic_stable_structures::Memory` directly. The
  upstream trait is the only memory trait used by the stable structures and LARA
  integration.
- The manager will persist a validated bucket-size policy per `MemoryId`. The
  policy is owned by the typed stable-layout configuration, not duplicated in each
  storage wrapper.
- Physical allocation will use append-only variable-sized extents. Each extent
  records its owning `MemoryId`; its size comes from that memory's persisted
  bucket-size policy. A virtual address will be translated through that memory's
  extent map rather than through one global bucket-stride formula.
- The existing owner-byte table can remain the durable allocation index. During
  reopen, scanning allocation order and looking up the owning `MemoryId`'s bucket
  size reconstructs each extent's physical offset. No duplicate per-structure
  allocation metadata is introduced.
- Reopening will reconstruct extent offsets from stable metadata. Bucket sizes may
  not change for an existing layout. Invalid, missing, or contradictory metadata
  must fail closed.
- Reopen index construction keeps the one-pass path for small allocations, where
  a second owner-table scan costs more than vector reallocations. Once the owner
  table crosses the measured large-allocation threshold, it performs a counting
  pass and reserves each per-memory extent vector before constructing offsets.
- Header persistence is split by mutation scope. Initialization may write the
  complete header, while policy registration writes only its policy slot and
  `grow` writes only the allocated-extent count and affected logical-size slot.
  The owner table is written before those header slots. These writes rely on the
  Internet Computer's same-canister update-call rollback when execution traps;
  code paths that return successfully must only return after all corresponding
  in-memory and stable-memory state has been updated.
- The first version will not reclaim or reuse extents. `grow` will retain the
  upstream `Memory` contract: it returns the previous logical size on success and
  `-1` when the underlying memory cannot grow.
- The pre-release amendment keeps `LAYOUT_VERSION = 1`, but expands the owner
  table to 65,536 entries over two metadata pages and stores the extent count as
  `u32`. The earlier global-bucket metadata and unamended VMM layout are not
  reinterpreted by this implementation. Development data may be recreated;
  production rollout requires the migration, capability, and preflight
  procedure in ADR 0039.

The initial per-region values remain an experimental policy and are not frozen by
this ADR. The implementation must benchmark property, adjacency, LARA maintenance,
payload, embedding, and log workloads before selecting production defaults. As a
starting hypothesis, tiny metadata regions should use smaller buckets, while
properties, payloads, and embeddings should use larger buckets.

Graph-index now adopts the variable manager with a separate policy from Graph:
catalog/config regions use 4 pages, vertex-label postings use 32 pages, and both
vertex-property and edge-property postings use 128 pages. The larger property
postings quantum preserves the per-region capacity slope for the derived index;
edge postings retain 128 pages because their keys include an unbounded encoded
value plus label, shard, owner-vertex, and slot fields. This is a fresh-layout
development choice; existing graph-index stable data must be recreated.

## Capacity and shard-size decision (2026-07-16)

The current manager has a global `MAX_EXTENTS = 65,536` limit. With the current
largest policy of 64 Wasm pages, the absolute physical extent-data ceiling is
`65,536 × 64 × 64 KiB = 256 GiB`, before accounting for the fact that all
MemoryIds share the extent budget. This is below the ICP stable-memory limit of
500 GiB; it is not a 16 GiB ceiling. A single region's ceiling is proportional
to its policy: 8-page regions have a 32 GiB upper bound, 16-page regions 64 GiB,
32-page regions 128 GiB, and 64-page regions 256 GiB, subject to the shared
extent budget.

The first capacity fixtures are intentionally small and report physical stable
memory, including extent slack and LARA metadata:

| fixture | logical content | physical increase | observation |
| --- | ---: | ---: | --- |
| vertices | 32,768 vertices | 48 pages | approximately 96 B/vertex in this growth range |
| edges | 1,024 vertices + 2,048 directed edges | 24 pages | vertex baseline 8 pages; edge increment 16 pages, approximately 512 B/edge in this range |
| properties | 1,024 vertices + 4,096 Int64 properties | 72 pages | vertex baseline 8 pages; property increment 64 pages, showing the 64-page allocation step |

The follow-up cases make the non-linear effects explicit:

| fixture | physical increase | result |
| --- | ---: | --- |
| 256 vertices + 1,024 short-text properties | 72 pages | same 64-page property step as Int64 |
| 256 vertices + 1,024 256-byte-text properties | 72 pages | still below the next extent step; instructions increase, but physical pages do not yet |
| 1,025 vertices + 512 edges from one hub | 24 pages | same edge allocation quantum, but approximately 23% more instructions per edge than the distributed-edge fixture |
| 257 vertices + 256 insert/delete/reinsert edges | 88 pages | churn retains free-span/replacement state; physical usage is much larger than the live edge set |

These slopes are not universal per-row sizes. LARA adjacency has two
orientations, segment/span metadata, PMA slack, logs, and relocation effects;
property storage grows in extent-sized steps and its eventual slope depends on
key/value width and B-tree occupancy. LARA churn can retain physical extents
even after logical deletion, while high-degree hubs increase relocation work.
The benchmarks therefore provide
calibration points for capacity planning rather than a fixed bytes-per-row
promise. They live in `crates/graph/src/bench/capacity.rs` and are enabled only
with `canbench_large` because the edge fixture is intentionally expensive.

For the current decision, retain `MAX_EXTENTS = 65,536` and make future growth
use multiple graph shards. LARA's local relocation and PMA work remains bounded
to one shard, the failure and upgrade domains stay smaller, and Gleaph already
has graph-local shard routing. A larger single shard would increase the amount
of state touched by rebuild, repair, upgrade, and operational inspection even
when its stable-memory allocation remains below 500 GiB. The ICP resource limit
also caps stable-memory reads/writes per replicated message at 2 GiB and upgrade
stable-memory I/O at 8 GiB ([resource limits](https://docs.internetcomputer.org/references/resource-limits/)),
so a 500 GiB headline does not make one huge shard cheap to maintain. The
canister-wide stable-memory ceiling is 500 GiB ([canister limits](https://docs.internetcomputer.org/concepts/canisters/)).

Do not double `MAX_EXTENTS` yet. Doubling would require a persistent metadata
change (at least a 128 KiB owner table and a revised metadata/data boundary) and
would raise the all-64-page theoretical ceiling to 512 GiB, which is already
above the ICP 500 GiB canister limit. Reconsider it only after a workload-backed
capacity run demonstrates that a useful single shard is hitting the 65,536
extent budget and that cross-shard query fanout, rather than LARA maintenance or
property/value volume, is the dominant cost. The admission test should include
edge-degree skew, delete/reinsert churn, property value-size classes, reopen,
and bounded maintenance calls; a raw stable-memory limit alone is insufficient.

## Per-memory page-size policy

The selected policy is intentionally asymmetric. The page size is an allocation
quantum, not a reservation: an empty `MemoryId` consumes no data extent. A
larger value reduces extent-count and address-map pressure after growth, but
increases the first physical step and the slack retained by that region. The
following table is the current production-wide Graph policy and the reason for
each class:

| MemoryIds | current pages | decision | rationale |
| --- | ---: | --- | --- |
| `FWD_VERTICES`, `REV_VERTICES` | 8 | retain | Fixed 21-byte labeled rows; vertex count is a primary shard axis and 8 pages keeps the extent count reasonable without the upstream 128-page minimum. |
| `FWD_BUCKETS`, `REV_BUCKETS` | 8 | retain | 29-byte label-bucket descriptors grow approximately with labeled vertex/edge groups. |
| `FWD_BUCKET_FREE_SPANS`, `FWD_BUCKET_FREE_SPAN_BY_START`, `REV_BUCKET_FREE_SPANS`, `REV_BUCKET_FREE_SPAN_BY_START` | 4 | retain | Maintenance indexes are sparse and rebuildable; their records should not impose a large first allocation. |
| `FWD_EDGE_COUNTS`, `REV_EDGE_COUNTS`, `FWD_EDGE_SPAN_META`, `REV_EDGE_SPAN_META` | 4 | retain | Per-segment metadata is small (16-byte count rows and 8-byte span rows); the 4-page quantum is sufficient until very large vertex counts. |
| `FWD_EDGES`, `REV_EDGES`, `FWD_EDGE_LOG`, `REV_EDGE_LOG` | 16 | retain | Adjacency is the LARA hot path and pays for both PMA slack and relocation/log records; 16 pages gives useful growth without property-sized slack. |
| `FWD_PAYLOAD_SLAB`, `REV_PAYLOAD_SLAB`, `FWD_PAYLOAD_LOG`, `REV_PAYLOAD_LOG`, `FWD_PAYLOAD_BLOBS`, `REV_PAYLOAD_BLOBS` | 32 | retain | Inline and overflow values are larger and grow with edge volume; payload domains should not compete for the 4-page extent budget. |
| `FWD_PAYLOAD_FREE_SPANS`, `FWD_PAYLOAD_FREE_SPAN_BY_START`, `REV_PAYLOAD_FREE_SPANS`, `REV_PAYLOAD_FREE_SPAN_BY_START` | 4 | retain | Rebuildable free-span indexes are sparse maintenance state. |
| `MAINTENANCE_QUEUE`, `DIRTY_WORK_ITEMS` | 4 | retain | Bounded/deferred maintenance state; large pages would only hide queue pressure as slack. |
| `VERTEX_LABEL_SETS` | 8 | retain | Sidecar rows scale with multi-label vertices but are generally smaller than property values. |
| `VERTEX_PROPERTIES`, `EDGE_PROPERTIES` | 64 | retain provisionally | Property B-trees are the fastest-growing variable-width domains. The 4,096-Int64 fixture consumed one 64-page property extent, so smaller pages would increase extent pressure; the 64-page step is accepted as a known small-graph slack cost. |
| `EDGE_ALIASES` | 16 | retain | Alias rows track adjacency and are fixed-width-ish, but can approach edge cardinality. |
| `GRAPH_METADATA`, `LABEL_STATS_DELTA_SEQ`, `VERTEX_EMBEDDING_INCARNATIONS` | 4 / 4 / 8 | retain | Metadata and sequence state are tiny; incarnation keys can scale with vertex churn and get a larger quantum. |
| `LABEL_STATS_DELTA_LOG`, `GRAPH_MUTATION_JOURNAL` | 8 | retain | Both are append-heavy but bounded by retention/acknowledgement policy; 8 pages balances append growth and slack. |
| `PENDING_VERTEX_PURGES` | 4 | retain | Resumable purge state is sparse and operationally bounded. |
| `INDEX_REPAIR_JOURNAL`, `UNIQUE_EFFECT_OUTBOX`, `DERIVED_INDEX_OUTBOX` | 16 | retain | Failed or deferred cross-canister work can accumulate in bursts; 16 pages avoids repeated tiny extents while remaining below property slack. |
| `GRAPH_LOCAL_UNIQUE_VALUES` | 64 | retain provisionally | It is a variable-width uniqueness B-tree and can be cardinality-bearing like properties; it must be included in property/value-size capacity runs. |
| `VERTEX_EMBEDDINGS` | 32 | retain | Vector bytes dominate row metadata and can be large per vertex; 32 pages is a compromise for value-bearing growth. |

The policy is not justified by row width alone. The capacity benchmark must be
re-run with at least small/medium/large value widths and churn before changing
the 64-page classes. In particular, a 64-page extent is appropriate only when
the region is expected to outgrow the small-graph slack; it is not a reason to
make every MemoryId 64 pages.

This page-size policy is independent of LARA's PMA `segment_size` and
per-vertex edge quota. LARA currently retains the 32/32 default; the
experimental segment16 and quota16/8/4/1 variants are tracked in [ADR 0001](0001-labeled-segment-slide.md)
and must be evaluated separately from the MemoryManager bucket policy. The
tail-headroom contract now derives its boundary from the persisted segment size
rather than a fixed 32-slot threshold. Segment16-specific tail-headroom,
deferred-maintenance deduplication, and hub/churn contract tests pass for the
default, quota8, quota4, and quota1 variants under segment16. Quota4 and quota1
still have legacy default-segment geometry failures in the broader experimental
suite, so this does not promote them to a production policy.

## Invariants and boundary requirements

1. A `VirtualMemory` can read and write only extents owned by its `MemoryId`.
2. Virtual offsets remain contiguous even when physical extents have different
   sizes; reads and writes crossing extent boundaries preserve byte order.
3. Persisted ownership and allocation metadata must not expose a partially claimed
   extent after a failed physical grow. The upstream implementation writes the
   owner marker and mutates its in-memory bucket list before growing the underlying
   memory, then traps if that grow fails. The custom manager must preflight the
   underlying grow, or provide an equivalent rollback/recovery fence, before
   publishing a new extent.
4. The policy and extent map are the single source of truth for address translation;
   wrappers must not maintain a second allocation model.
5. The implementation must preserve the upstream `Memory` semantics expected by
   `StableBTreeMap`, `StableCell`, `Vec`, and LARA.

## Consequences

Positive:

- Small, slow-growing regions no longer impose the same minimum allocation as
  property or payload regions.
- Large regions can receive a bucket size appropriate to their expected shard
  capacity, without reserving a fixed partition in advance.
- Existing storage structures retain their public generic boundary and do not need
  to know how physical extents are allocated.

Costs and risks:

- The custom manager owns a persistent format and an address translator, so mixed
  bucket sizes require more reopen, failure, and boundary tests than the upstream
  implementation.
- Extent lookup adds mapping work to reads and writes, especially at boundaries;
  caching the per-memory extent map may be required after measurement.
- Append-only extents retain the existing non-reclamation behavior. Clearing a
  structure does not return physical stable pages.
- The crate is coupled to the upstream `Memory` trait and its semantics. Upstream
  changes must be reviewed explicitly rather than silently copied.
- The manager must not silently accept a new bucket-size policy when reopening an
  existing header. The upstream `init_with_bucket_size` loads the persisted bucket
  size without comparing it with the caller's argument; per-memory sizing requires
  an explicit policy/configuration consistency check.

## Alternatives considered

### Keep one global bucket size

This is the smallest change and is the current interim approach (16 pages for
Graph). It retains the upstream implementation and simplest persistence model, but
forces one compromise across metadata, LARA, properties, payloads, and embeddings.
It is rejected as the long-term design because it cannot simultaneously minimize
slack and maximize the largest shard.

### Multiple managers over fixed `RestrictedMemory` partitions

This permits different bucket sizes using upstream components, but each partition
has a predetermined capacity. One hot region can hit its partition limit while
another partition is mostly empty. It also moves capacity policy into fixed ranges
and weakens the desired shared-capacity model. Rejected.

### Fork the upstream crate and alter its manager

A fork could change the existing implementation directly, but it broadens the
dependency fork and makes upstream synchronization part of every storage change.
An internal focused crate that depends on the upstream crate and implements its
public `Memory` trait keeps the compatibility boundary explicit. Rejected for the
first implementation; revisit only if upstream integration constraints require it.

## Implementation and validation plan

1. **Partially implemented:** `ic-stable-variable-memory-manager` now provides
   mixed-size persisted policies, append-only extents, reopen, independent grow,
   failed-grow preflight, and cross-boundary tests. The remaining upstream test
   ports include broader random/stability and maximum-allocation coverage.
2. Wire Graph's existing typed region registry to one persisted per-`MemoryId`
   policy; do not change storage wrapper generics.
3. Run focused canbench and PocketIC tests for property-heavy, edge-heavy,
   payload-heavy, embedding, and maintenance workloads. Record logical pages,
   allocated pages, slack, physical stable memory, and reopen cost.
4. Add an upgrade/migration preflight test and update ADR 0007 plus the stable-memory
   inventory with the final policy and region capacities.
5. Only then decide whether the policy is accepted for production durable state.

Graph is now wired to the new crate with an experimental policy: 4 pages by
default; 8 pages for vertex, label-bucket, label-sidecar, and embedding-incarnation
rows; 16 pages for edge slab/log, aliases, and durable backlog; 32 pages for
payload and embeddings; and 64 pages for property and local-unique values. Fresh
local stats showed that the earlier 32/64-page policy
reserved about 49.9 MiB for about 3.7 MiB of logical data before seeding, so the
smaller policy reduces initial slack while remaining above the 16 GiB physical
limit before the 65,536-extent cap becomes relevant for the larger regions.
Migration and production-capacity validation are still
outstanding, and the new layout is not a compatibility replacement for existing
Graph stable data.

The current policy remains the selected capacity-preserving point for the present
workload classes: reducing the 8/16/32/64-page classes to the 4-page uniform
candidate lowers the local benchmark footprint, but does not improve measured
instruction cost and reduces the headroom of the corresponding high-growth
regions. Policy values should therefore change only with workload-specific
capacity evidence, not as a consequence of the reopen-index optimization.

## References

- [ADR 0007 — Stable-memory layout policy](0007-stable-memory-layout.md)
- [ADR 0039 — Production stable-memory evolution and canister upgrade safety](0039-production-stable-memory-evolution-and-upgrade-safety.md)
- [Stable-memory inventory](../storage/stable-memory-inventory.md)
- [`MemoryManager` API](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/memory_manager/struct.MemoryManager.html)
