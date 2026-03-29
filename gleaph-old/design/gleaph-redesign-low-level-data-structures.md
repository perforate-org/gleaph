# Gleaph Redesign: Low-level Data Structures

## Status

- Draft
- Companion to [gleaph-redesign-principles.md](/Users/yota/dev/gleaph/design/gleaph-redesign-principles.md)

## Purpose

This document turns the high-level redesign principles into concrete low-level
storage structures for a new implementation.

The target is a vertex-centric, adjacency-first kernel with compact hot entries
and explicit separation between:

- physical traversal state
- semantic identity
- property data
- update buffering

## Core Types

## Preferred containers

Unless there is a specific adversarial or semantic requirement otherwise:

- `HashSet<u32>` should be represented as `roaring::RoaringBitmap`
- `HashMap<K, V>` with non-adversarial keys should use
  `rapidhash::fast::RapidHashMap<K, V>`

This is especially relevant for:

- visited-vertex sets
- label range membership sets
- temporary planner/executor maps
- per-vertex or per-label auxiliary tables

## Vertex IDs

The storage kernel uses shard-local vertex ids.

```rust
type LocalVertexId = u32;
```

Future global identity belongs above the kernel:

```rust
type GlobalVertexId = u64;
```

or:

```rust
struct GlobalVertexId {
    shard: u32,
    local: u32,
}
```

## Label IDs

```rust
type LabelId = u16;
```

Conventions:

- `0` means unlabeled
- remaining bits are ordinary labels

This keeps label packing simple and gives enough space for typical graph
workloads.

## Vertex entry

The redesign keeps the vertex entry shape aligned with the current Gleaph
layout.

```rust
struct VertexEntry {
    edge_index: u64,
    degree: u32,
    log_offset: i32,
}
```

This is 16 bytes and already matches the right conceptual model:

- `edge_index` is the base physical locator for the vertex neighborhood
- `degree` is the visible degree
- `log_offset` is the overflow/update head; `-1` means no log

This keeps vertex handling simple and preserves compatibility with the existing
mental model and stable-layout intuition.

## Direction-specific adjacency

Keeping `VertexEntry` the same does not mean forward and reverse traversal must
share the same physical backing arrays.

The low-level design can still use:

- one `VertexEntry` array for forward adjacency
- one `VertexEntry` array for reverse adjacency

or an equivalent split at a higher container level, while keeping the per-entry
shape unchanged.

The key requirement is not a different vertex-entry ABI, but that:

- forward traversal remains source-major
- reverse traversal remains destination-major
- exact-label access is available early

So the redesign should preserve the current `VertexEntry` shape and change the
adjacency organization around it, not the entry itself.

## Edge hot entry

The preferred hot edge format is 8 bytes:

```rust
struct EdgeEntry {
    dst: u32,
    meta: u16,
    aux: u16,
}
```

Where:

- `dst` is the neighboring local vertex id
- `meta` packs tombstone and a 15-bit label id
- `aux` is a compact sidecar handle or workload-specific hint

## Meta packing

```text
meta bits:
  bit 15     tombstone
  bits 0-14  label_id
```

Helper operations:

- `is_tombstoned(meta) -> bool`
- `label_id(meta) -> LabelId`
- `with_tombstone(meta, bool) -> u16`
- `with_label(meta, LabelId) -> u16`

## Why `aux` is intentionally generic

`aux` should not be semantically fixed too early.

Possible meanings:

- sidecar slot within a per-vertex edge-aux array
- compact timestamp delta
- quantized weight
- compact locator into an overflow area
- sharding or partition hint

This keeps the hot kernel stable while allowing different graph profiles.

## Edge sidecars

The redesign assumes that many edge fields are cold or semi-hot.

Examples:

```rust
struct EdgeAux {
    edge_id: u32,
    timestamp: u64,
    weight: f32,
}
```

This struct is illustrative only. In practice, separate sidecar arrays are
preferred over an AoS sidecar object:

- `edge_ids[]`
- `timestamps[]`
- `weights[]`

That keeps each query family free to touch only what it needs.

## Forward adjacency

Forward adjacency is source-major.

Logical shape:

```rust
struct ForwardAdjacency {
    edges: Vec<EdgeEntry>,
}
```

The key invariant is:

- all outgoing edges of a vertex occupy a contiguous logical range

The actual implementation may be PMA-like, gap-augmented, or segment-based, but
the visible access model should behave like a contiguous neighborhood.

## Reverse adjacency

Reverse adjacency is destination-major.

Logical shape:

```rust
struct RevEntry {
    src: u32,
    meta: u16,
    aux: u16,
}

struct ReverseAdjacency {
    edges: Vec<RevEntry>,
}
```

`RevEntry` is intentionally symmetric with `EdgeEntry`:

- `src` is the neighboring local vertex id
- `meta` packs tombstone and a 15-bit label id
- `aux` has the same role as on the forward side

This keeps forward and reverse traversal kernels aligned while still allowing
the field names to reflect direction.

Unlike forward adjacency, reverse adjacency should be label-aware from the
start if incoming fan-in is large enough to make mixed scans expensive.

Practical options:

- destination-major region with per-label offsets
- destination-major region plus label bucket table
- destination-major PMA with label-partitioned subranges

The important part is the access contract:

- exact incoming label scans must not require broad mixed-label rejection

## Per-vertex label offsets

A preferred compromise between simplicity and performance is:

```rust
struct VertexLabelRange {
    label: LabelId,
    start: u32,
    len: u32,
}
```

Per vertex:

- one list for outgoing label ranges
- one list for incoming label ranges

This avoids a full global label index while still giving exact-label access.

It also preserves a VCSR-style per-vertex organization.

If these ranges need an auxiliary lookup table, the default choice should be
`RapidHashMap<LabelId, VertexLabelRange>` rather than the standard library
`HashMap`.

## Physical locator

Physical edge identification should use adjacency position first.

```rust
struct EdgeLocator {
    vertex: LocalVertexId,
    ordinal: u32,
    direction: EdgeDirection,
}
```

This is enough to identify a physical edge slot within a vertex neighborhood.

It is not necessarily a stable semantic id across rebalancing, so it should be
treated as a physical locator rather than an externally visible identity.

## Semantic edge identity

If the graph needs stable edge identity, keep it above the hot kernel:

```rust
type EdgeId = u32;
```

and map it to a physical locator:

```rust
edge_locator_by_id: EdgeId -> EdgeLocator
```

If implemented as a hash map, this should default to `RapidHashMap`.

This preserves:

- multi-edge correctness
- stable references
- mutation targeting

without forcing traversal to start from `edge_id`.

## Property stores

Properties should be separate.

Node properties:

```rust
node_props: LocalVertexId -> PropertyMap
```

Edge properties:

```rust
edge_props: EdgeId -> PropertyMap
```

or, if semantic edge ids are optional:

```rust
edge_props: EdgeLocator -> PropertyMap
```

The choice depends on how strongly the system needs stable external edge
identity.

If these property stores use hash maps internally, they should default to
`RapidHashMap`.

## Update logs

Update logs are auxiliary and segment- or vertex-local.

Illustrative shape:

```rust
struct EdgeLogEntry {
    neighbor: LocalVertexId,
    meta: u16,
    aux: u16,
    prev: u32,
}
```

The precise structure can vary, but the read-path contract is fixed:

- base adjacency remains primary
- logs are merged into the visible neighborhood view

## SIMD and scan considerations

The 8-byte `EdgeEntry` layout is chosen partly because it behaves well as a scan
unit.

Benefits:

- 8 entries fit in 64 bytes
- label/tombstone bits are at a fixed offset
- `target` and `meta` can be read without touching sidecars

This layout supports:

- scalar stride scans
- loop unrolling
- possible SIMD-assisted label filtering

without requiring a separate edge-label index for every case.

## Invariants

The low-level structures should preserve these invariants:

- generic traversal does not require semantic edge identity lookups
- exact-label scans are first-class
- tombstone state is inline in hot entries
- sidecar data is only touched when the query needs it
- physical locator and semantic identity are distinct concepts

## Open implementation choices

- Whether forward label filtering should start with stride scan or per-label
  subranges
- Whether reverse adjacency should always keep explicit label ranges
- Whether `aux` should be a sidecar slot or a compact per-workload hint
- Whether semantic edge identity is mandatory in v1
