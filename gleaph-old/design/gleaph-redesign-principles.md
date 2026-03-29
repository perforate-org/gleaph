# Gleaph Redesign Principles

## Status

- Draft
- Scope: greenfield redesign for a new repository
- Goal: recover a simple, structurally fast graph kernel before re-adding higher-level property-graph features

## Why a Redesign

Recent investigation strongly suggests that Gleaph's largest regressions were
not primarily caused by missing query-specific fast paths, but by low-level
drift away from the storage and execution model used in the research/reference
implementations.

The main failure mode was:

- generic traversal stopped being vertex-centric
- auxiliary edge identity became too central to hot reads
- mixed-label adjacency scans became common
- query execution often rediscovered structure that should have been encoded in
  physical layout

The redesign should therefore start from the low-level principles of VCSR and
DGAP, then add only the minimum additional machinery required by Gleaph.

## Design Goals

The new implementation should support all of the following without making the
generic path structurally slow:

- fast forward and reverse neighborhood traversal
- exact-label traversal without broad mixed-label scans
- dynamic updates
- multi-edge correctness
- stable semantic edge identity where needed
- property graph features
- future sharding

## Ground Rules

## 1. Vertex-centric first

The primitive operation is:

- start from a vertex
- iterate its neighborhood

Not:

- start from an auxiliary edge identifier
- recover endpoints and labels
- reconstruct the neighborhood at query time

This is the core lesson from both VCSR and DGAP.

## 2. Physical layout before logical identity

Low-level storage should be organized for scan locality, not for update
identity.

- forward traversal should be source-major
- reverse traversal should be destination-major
- exact-label filtering should be available at the physical access layer

Logical identity is still useful, but it must not define the hot traversal
shape.

## 3. Generic path before fast paths

The redesign should assume that:

- the generic path must already be naturally fast
- fast paths are secondary and optional

If a query family is only viable after adding special-case executors, the
storage/kernel design is probably still wrong.

## 4. Hot/cold separation

The fields needed for neighborhood walking should be small and inline.
Everything else should be sidecar, overlay, or higher-level metadata.

This is required for:

- cache density
- SIMD-friendly scans
- simpler reasoning about the kernel

## 5. Simplicity wins at equal performance

If two designs deliver similar benchmark results, the simpler one should be
chosen.

This means preferring:

- physical adjacency order over secondary indirection
- fewer central identifiers
- fewer semantic layers in hot loops

## 6. Use specialized integer containers by default

The redesign should avoid generic hash collections when the key/value shape is
narrow and known.

Default rules:

- use `roaring::RoaringBitmap` instead of `HashSet<u32>`
- use `rapidhash::fast::RapidHashMap` instead of standard `HashMap` when
  collision resistance is not required

This applies both to storage internals and to the query layer.

## Low-level Storage Model

## Vertex

At the low level, a vertex is an adjacency locator.

Like DGAP, vertex state should primarily answer:

- where does this vertex's outgoing neighborhood begin?
- where does this vertex's incoming neighborhood begin?
- what update/log tail is attached?

It should not try to carry unrelated semantic state in the hot kernel.

Minimum expected vertex responsibilities:

- forward neighborhood locator
- reverse neighborhood locator
- degree / local edge count metadata
- optional log head / segment-local overflow metadata

## Edge

The low-level hot edge entry should be small and traversal-oriented.

A good target is an 8-byte hot edge entry shape such as:

```rust
struct EdgeEntry {
    dst: u32,
    meta: u16, // tombstone bit + label id
    aux: u16,  // sidecar slot, compact locator, delta, or workload-specific hint
}
```

Where:

- `dst` is the destination vertex id
- `meta` packs tombstone and a 15-bit label id
- `label_id = 0` means unlabeled
- `aux` is intentionally not semantically fixed

This keeps the kernel flexible for:

- unweighted graphs
- weighted graphs with sidecar weight storage
- recency-oriented graphs
- future sharding hints
- compact mutation indirection

The reverse side should mirror this with a direction-specific entry:

```rust
struct RevEntry {
    src: u32,
    meta: u16,
    aux: u16,
}
```

## Hot vs Cold edge data

Hot edge data should be the minimum required for generic traversal:

- endpoint id
- label id
- tombstone bit
- compact auxiliary handle

Cold or sidecar edge data should include:

- full weight
- timestamp
- edge properties
- external edge identity
- mutation metadata

The key point is that `weight` and `timestamp` are important properties, but
they are not necessarily hot in every traversal.

## Forward and Reverse Adjacency

## Forward

Forward adjacency must remain source-major and physically contiguous as much as
possible.

The kernel should support:

- scanning all outgoing edges
- scanning outgoing edges with an exact label filter

The preferred first attempt is not necessarily a fully separate label index.
If adjacency for a vertex is contiguous and compact, scanning labels from the
hot entries may be sufficient for many workloads.

That said, the kernel must make exact-label traversal cheap enough that common
queries do not spend most of their time on wrong-label rejects.

## Reverse

Reverse adjacency is more sensitive to broad fan-in. The recent regression
investigation suggests that reverse mixed-label scans are especially harmful.

Therefore reverse access should support label-aware physical access from the
start, even if forward access begins with simpler scans.

A reasonable asymmetry is:

- forward: compact scan first, specialized bucket only if needed
- reverse: label-aware bucket from the beginning

That still fits the vertex-centric model.

## Identity and Mutation

## Edge identity

The redesign should separate:

- physical edge locator
- semantic edge identity

Possible physical locator:

- `src_vertex_id`
- ordinal within that vertex's adjacency

This matches the adjacency-first model used in DGAP-like systems.

Semantic edge identity, if retained, should be optional at the hot layer.

It is useful for:

- external references
- mutation targeting
- stable upgrade semantics
- overlays and logs

But it should live in sidecar or semantic layers rather than in the traversal
kernel itself.

## Update/log model

DGAP is the reference here:

- adjacency remains primary
- update logs are auxiliary
- neighborhood iteration merges base and buffered updates

The redesign should adopt the same rule:

- logs help updates
- logs do not redefine the read-path abstraction

## Properties

Properties should be layered above the adjacency kernel.

- node properties: separate store keyed by local vertex id
- edge properties: separate store keyed by semantic edge handle or compact
  physical locator

The important rule is that property access must not force generic traversal to
reconstruct endpoints or labels.

## Query Execution Principles

## 1. Push exact label filtering down

Exact-label information should be consumed by the physical iterator itself,
not by late executor-side rejection whenever possible.

## 2. Avoid row explosion

If a query is fundamentally:

- grouped traversal
- top-k grouped count
- small aggregation over neighbors

then execution should aggregate as early as possible instead of materializing
full row sets.

## 3. Use sidecar data only when the query actually needs it

Generic traversal should not pay for:

- weight reads
- timestamp reads
- semantic edge identity reads

unless the query explicitly depends on them.

## 4. Treat continuation and var-len carefully

`WITH ... LIMIT` and var-len traversal must be designed around:

- early pruning
- early label filtering
- small state transition kernels

because these shapes are where structural mistakes amplify quickly.

## ID Width and Future Sharding

The redesign should not widen the low-level kernel to `u64` by default.

Instead:

- local physical vertex ids stay `u32`
- global/sharded identity lives above the storage kernel

Recommended split:

- `LocalVertexId = u32`
- `GlobalVertexId = u64` or `(shard_id, local_id)`

This preserves:

- compact adjacency storage
- bitset / roaring density
- SIMD/cache behavior

while still allowing future horizontal scaling.

## Suggested Implementation Order

1. Implement a minimal unweighted vertex-centric kernel.
2. Add reverse traversal with label-aware physical access.
3. Add dynamic update/log support without changing the read abstraction.
4. Add sidecar storage for weight, timestamp, and semantic edge identity.
5. Add property stores.
6. Add planner/executor on top of the stable kernel.
7. Only then add query-family fast paths.

## Non-goals for the first version

The first version should explicitly avoid overfitting to the current codebase.

In particular, it should not assume:

- the current `edge_id` model must survive unchanged
- all edge metadata must be inline
- all property graph semantics belong in the low-level kernel
- special-case executors are the main optimization strategy

## Success Criteria

The redesign is successful if:

- the generic path is already strong on feed / FoF / reverse / var-len queries
- exact-label traversal does not degrade into large wrong-label scan counts
- dynamic updates do not force `edge_id`-centric traversal
- hot edge entries stay compact
- semantic identity and property features remain possible above the kernel

## Open Questions

- Should forward adjacency begin with stride-scan label filtering, or start
  immediately with label-aware buckets?
- Should the hot edge entry be 8 bytes or 16 bytes?
- Should semantic edge identity be optional, or always materialized in a
  sidecar?
- Which workloads justify keeping `weight` semi-hot instead of fully cold?
- How much asymmetry between forward and reverse access is acceptable before
  the model becomes too complex?
