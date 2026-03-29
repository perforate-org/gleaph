# Gleaph Redesign: Mutation and Log API

## Status

- Draft
- Companion to [gleaph-redesign-principles.md](/Users/yota/dev/gleaph/design/gleaph-redesign-principles.md)

## Purpose

This document describes how the redesigned storage kernel should support
mutation without giving up a vertex-centric read path.

The guiding model is DGAP:

- base adjacency is primary
- updates are buffered in auxiliary structures
- reads merge base and buffered state

## Goals

- preserve fast generic traversal
- support inserts, deletes, revives, and property updates
- support stable semantic edge identity if required
- avoid `edge_id`-centric traversal

## Mutation model

## Edge insertion

Insertions should not require immediate full rebalance of adjacency regions.

Preferred flow:

1. determine source and destination vertices
2. append compact edge-log entries to forward and reverse mutation logs
3. update semantic sidecars if the edge has identity or properties
4. periodically merge logs into base adjacency

This keeps mutation cheap while preserving a simple read abstraction.

## Edge deletion

Deletion should be represented as a logical tombstone first.

Preferred options:

- set tombstone in log entry shadowing base edge
- set tombstone in base slot if in-place mutation is cheap and safe

The read path must see a single consistent visible edge state without requiring
query-time semantic reconstruction.

## Edge update

Edge updates split naturally into two kinds:

- hot-structure updates
  - label change
  - tombstone toggle
- cold metadata updates
  - properties
  - weight
  - timestamp

Hot-structure updates may require moving the edge between label ranges or log
buckets. Cold metadata updates should remain sidecar-only whenever possible.

## Semantic identity

If the system keeps stable semantic edge ids, mutation API should be expressed
in terms of `EdgeId`, but resolved quickly to physical locators.

```rust
fn locate_edge(edge_id: EdgeId) -> Option<EdgeLocator>;
```

The key rule is:

- mutation may begin with `edge_id`
- traversal must not

## Suggested API

```rust
fn insert_edge(src: LocalVertexId, dst: LocalVertexId, label: LabelId) -> InsertResult;
fn delete_edge(locator: EdgeLocator) -> bool;
fn revive_edge(locator: EdgeLocator) -> bool;
fn update_edge_label(locator: EdgeLocator, new_label: LabelId) -> bool;
fn update_edge_weight(locator: EdgeLocator, weight: f32) -> bool;
fn update_edge_timestamp(locator: EdgeLocator, ts: u64) -> bool;
```

If semantic ids are enabled:

```rust
fn delete_edge_by_id(edge_id: EdgeId) -> bool;
fn update_edge_props_by_id(edge_id: EdgeId, props: PropertyPatch) -> bool;
```

The `by_id` entry points should resolve once to locators and then operate on
the physical structures.

## Log structure

The log should be local and merge-friendly.

Illustrative shape:

```rust
struct EdgeLogEntry {
    neighbor: LocalVertexId,
    meta: u16,
    aux: u16,
    prev: u32,
}
```

Per vertex and direction:

- `out_log_head`
- `in_log_head`

Alternative organization:

- per-segment append-only log with per-vertex heads

The important properties are:

- cheap append
- easy neighborhood merge
- bounded rebalance cost

## Visible neighborhood contract

Every neighborhood iterator should expose the same logical contract:

- see all live base edges
- see all live inserted edges
- hide tombstoned edges
- respect label filtering early

The iterator must not force callers to understand whether an edge came from:

- base adjacency
- per-vertex log
- per-segment log

## Rebuild / compaction

Logs should be merged back into base adjacency under explicit maintenance
operations:

- per-vertex compaction
- per-segment compaction
- global rebalance

Triggers may include:

- log size threshold
- degree skew threshold
- memory pressure
- explicit admin command

## Multi-edge semantics

The redesign must preserve distinct multi-edges.

A live edge is not identified only by:

- `(src, dst, label)`

because identical triples may represent multiple distinct relationships.

Options:

- semantic `EdgeId`
- physical locator plus duplicate ordinal
- both

The storage kernel should treat “same endpoint and label” as possibly multiple
live records.

## Property mutation

Property mutation belongs to the property layer, not the adjacency kernel.

Recommended split:

- adjacency mutation handles topology
- property mutation handles sidecar stores

This keeps the hot kernel stable even when property semantics evolve.

## Consistency model

The first version should prefer a simple consistency rule:

- neighborhood iterators always see the latest committed visible state
- compaction preserves visible semantics

It is better to keep this model simple than to over-engineer fine-grained
transaction semantics into the kernel.

## Required invariants

- semantic edge identity resolves to physical locator in bounded time
- logs do not become the primary traversal abstraction
- label-aware traversal still works when edges are buffered
- compaction preserves multi-edge identity and visibility
- delete/revive/update do not require broad graph scans

## Open questions

- Whether logs should be per-vertex or per-segment in v1
- Whether label changes should be encoded as delete+insert at the physical
  layer
- Whether semantic `EdgeId` is required from day one or can be optional
