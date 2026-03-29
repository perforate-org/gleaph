# graph-pma Target Design

## Status

- Target architecture document
- Written to clarify the intended end state of `crates/graph-pma`
- The current implementation does **not** yet satisfy this document in full

## Why This Exists

The current `graph-pma` implementation already establishes several useful
concepts:

- adjacency-first read path
- separation between adjacency, property store, and property index
- `EdgeLocator` vs `EdgeId`
- 8-byte adjacency entries
- per-vertex label-range access

However, some current implementation choices are intentionally transitional:

- adjacency rebuilds are too global
- label ranges are stored in globally rebuilt vectors
- `VertexEntry` still carries search-oriented fields
- mutation cost is not yet aligned with VCSR/DGAP principles

This document defines the intended final shape so that future implementation
work can be judged against a stable target rather than drifting incrementally.

## Core Principles

The target design follows the combined reading of VCSR and DGAP:

- traversal is vertex-centric
- base adjacency is primary
- updates are buffered locally
- reads merge base and buffered state
- semantic identity is separate from physical access order
- properties remain outside the adjacency kernel

The most important consequence is:

- mutation must **not** require global adjacency rebuild
- especially not for high-degree vertices

If an implementation choice makes insertion of one edge rewrite an entire
vertex neighborhood, or worse a broad global structure, that choice is
considered a temporary simplification, not the target.

## Non-Goals

The target is **not**:

- a generic edge-id ordered graph store
- a globally rebuilt adjacency map
- a design where property mutation reshapes adjacency
- a design where label filtering depends on broad mixed-label rejection

## Identity Model

The target design uses three different identities:

### 1. Physical adjacency entry

This is the hot record used during neighborhood traversal.

```rust
#[repr(transparent)]
struct AdjacencyEntry(u64);
```

Layout:

- low 48 bits: neighboring local vertex id
- high 16 bits: packed `meta`

This is 8 bytes.

### 2. Physical locator

This identifies a physical slot within a vertex-local neighborhood view.

```rust
struct EdgeLocator {
    vertex: LocalVertexId,
    ordinal: u32,
    direction: EdgeDirection,
}
```

This is the primary physical identity used by the storage kernel.

### 3. Semantic edge identity

This is the stable identity used above the hot kernel.

```rust
type EdgeId = u64;
```

This remains necessary for:

- DML targets
- edge property store
- public/kernel contract
- prepared mutation semantics

The key rule is:

- mutation may start from `EdgeId`
- traversal must not

## Local Vertex IDs

The target shape assumes adjacency-local vertex ids are compact.

Preferred kernel-local shape:

```rust
type LocalVertexId = u32;
```

`NodeId` is represented as a packed 48-bit kernel type. The adjacency kernel
should treat that packed form as the native vertex id representation.

If 48-bit packing is retained internally, overflow beyond 48 bits must fail
fast. Silent widening or truncation is not acceptable.

## VertexEntry

The target `VertexEntry` is intentionally small and stable:

```rust
struct VertexEntry {
    edge_index: u64,
    degree: u32,
    log_offset: i32,
}
```

Meaning:

- `edge_index`: base adjacency locator
- `degree`: visible degree in the base region
- `log_offset`: overflow/update head, `-1` if empty

### Important constraint

`VertexEntry` should not become a kitchen-sink metadata object.

In particular, it should not permanently absorb:

- label identity itself
- property info
- broad planner state
- arbitrary sidecar pointers

If extra per-vertex indexing is needed, it should generally live in a parallel
structure rather than bloating `VertexEntry`.

## Direction-Specific Adjacency

The target keeps forward and reverse adjacency separate.

Forward:

- source-major
- optimized for outgoing traversal

Reverse:

- destination-major
- optimized for incoming traversal
- should support exact-label access early

This means the system should effectively maintain:

- one forward `VertexEntry[]`
- one reverse `VertexEntry[]`
- one forward adjacency entry region
- one reverse adjacency entry region

## Hot Entry Layout

The target hot entry is:

```rust
struct EdgeEntry {
    target: PackedNodeId48,
    meta: u16,
}
```

Conceptually:

- `target`: neighbor local vertex id
- `meta`: tombstone bit + 15-bit label id

This is the only information the traversal kernel should need for the common
case.

### Why `edge_id` is not in the hot entry

The hot path should not depend on semantic identity.

`EdgeId` belongs in sidecars or locator mappings, not in every adjacency entry,
unless measurement later proves the extra 8 bytes are worth it.

## Label Access

Exact-label traversal is a first-class requirement.

The target design supports this using per-vertex label indexing, but not
necessarily by storing label-range pointers directly inside `VertexEntry`.

### Required capability

Given:

- a vertex
- a direction
- a label id

the kernel must be able to restrict traversal to the relevant subrange without
broad mixed-label scanning.

### Acceptable implementation shapes

#### Option A: Parallel vertex-label index

```rust
struct VertexLabelIndexEntry {
    start: u32,
    len: u32,
}

struct VertexLabelRange {
    label_id: LabelId,
    start: u32,
    len: u32,
}
```

This is preferred if we want to keep `VertexEntry` minimal.

#### Option B: Vertex-local sidecar block

Each vertex can point to a compact sidecar block describing its label subranges.

This is also acceptable if the sidecar stays adjacency-local.

### What is not acceptable

- global label-range rebuild as the steady-state mutation path
- broad full-neighborhood scans for every label-filtered expand

## Mutation Model

The target mutation model is DGAP-like.

### Edge insert

Preferred flow:

1. resolve source and destination vertices
2. append to forward and reverse vertex-local overflow
3. update `EdgeId -> EdgeLocator` or equivalent semantic sidecar
4. update edge property sidecars
5. defer base merge until local threshold or maintenance trigger

Crucially:

- insertion should not require rewriting the entire neighborhood
- insertion should not require rebuilding global adjacency structures

### Edge delete

Preferred flow:

- mark tombstone in local overflow or base slot
- keep traversal semantics consistent
- defer structural cleanup until compaction/merge

### Edge label update

This is a hot-structure update.

Preferred behavior:

- treat as logical delete + insert at the physical layer
- keep semantic `EdgeId` stable
- update locator mapping

### Property update

This is **not** an adjacency mutation.

It should update only:

- property store
- property index
- optional semantic sidecars

It should not reshape adjacency.

## Overflow / Log Design

This is the most important missing piece in the current implementation.

The target requires vertex-local or segment-local buffered updates.

Minimum acceptable shape:

```rust
struct EdgeLogEntry {
    entry: AdjacencyEntry,
    prev: u32,
    edge_id: EdgeId,
}
```

Per vertex and direction:

- `log_offset` points to the latest buffered change
- log entries form a chain or local buffer

### Read contract

Neighborhood iteration must see:

- all live base edges
- all live inserted edges
- all visible label changes
- all tombstones applied

without forcing the caller to care whether an edge came from:

- base adjacency
- local overflow
- merge output

## Compaction / Merge

Compaction should be explicit and local-first.

Preferred triggers:

- per-vertex overflow threshold
- per-segment overflow threshold
- maintenance command
- memory pressure

Preferred order:

1. local merge
2. segment merge
3. broader rebalance only when needed

The target is emphatically **not**:

- rebuild all adjacency vectors after every insert

## Property Subsystem

The property subsystem remains separate from adjacency.

### Property store

Responsibilities:

- node property persistence
- edge property persistence
- property mutation application
- snapshot/load

### Property index

Responsibilities:

- equality lookup
- later range lookup
- independent backend choice

### Backend direction

Both property store and property index may use `(a,b)+tree`-style byte-kv
backends. That is orthogonal to the adjacency kernel.

This split must remain.

## Stable Layout

The target stable layout has explicit regions for:

- node catalog
- edge semantic payload sidecars
- forward vertex entries
- reverse vertex entries
- forward adjacency entries
- reverse adjacency entries
- forward edge-id sidecar
- reverse edge-id sidecar
- forward label index sidecar
- reverse label index sidecar
- property store
- property index
- mutation log

The current compact single-blob snapshot representation is acceptable only as a
transitional persistence format.

The target is page- or region-oriented, not "reconstruct everything from one
opaque blob" forever.

## Current Implementation vs Target

The current implementation is useful, but these parts are still transitional:

### Good enough to keep

- 8-byte `AdjacencyEntry`
- separation of `EdgeLocator` and `EdgeId`
- property store / property index split
- adjacency region as its own payload region

### Transitional and expected to change

- global-ish rebuild in `rebuild_indexes()`
- globally rebuilt label-range vectors
- `VertexEntry` still carrying search-oriented `node_id`
- fully reconstructing adjacency from bytes on open

## Required Invariants

Any future change should preserve these invariants:

- traversal starts from vertex-local adjacency
- semantic `EdgeId` resolves to physical locator in bounded time
- exact-label traversal avoids broad mixed-label scanning
- properties stay outside adjacency
- edge insert/delete/update do not require broad graph scans
- a high-degree vertex insertion must not rewrite the whole graph

## Immediate Next Steps

The next implementation steps should be:

1. stop using full adjacency rebuild as the steady-state mutation path
2. make `log_offset` live with vertex-local overflow
3. move label indexing from globally rebuilt vectors to vertex-local or
   parallel vertex-label sidecars
4. shrink `VertexEntry` toward the target ABI
5. make open/load progressively less "full reconstruct" and more region/page based

## Decision Record

This document intentionally chooses:

- adjacency-first over edge-id-first
- local overflow over global rebuild
- semantic/physical identity split over conflation
- separate property subsystem over integrated god-object design

If future code moves away from these decisions, it should do so explicitly and
with measurement, not by accident.
