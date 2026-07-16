# 0043. Per-memory bucket sizing for the stable-memory manager

Date: 2026-07-16
Status: Partially Implemented
Last revised: 2026-07-16
Anchor timestamp: 2026-07-16 13:36:25 UTC +0000

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
