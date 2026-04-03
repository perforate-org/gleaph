# Graph PMA Property Store Spec

For current implementation status and next-step guidance, see
[`graph-pma-rewrite-status.md`](/Users/yota/dev/gleaph-project/docs/graph-pma-rewrite-status.md).

## Goal

This document defines the rewrite-side property subsystem for `graph-pma`.

The property subsystem is intentionally separate from the adjacency kernel.

- adjacency storage:
  - fixed-size
  - PMA / DGAP / VCSR oriented
  - hot traversal path
- property storage:
  - variable-length
  - bucket-backed
  - stable-memory resident
  - optimized for persistence and lookup, not adjacency traversal

The immediate goal is to define a stable low-level ownership boundary and a
minimal first implementation path.

## Non-goals

This document does not define:

- PMA density thresholds
- DGAP segment-log behavior for adjacency
- global label-tree design
- final secondary-index design for every predicate type

Those belong to the adjacency spec or later property-index specs.

## Ownership Boundary

### Adjacency kernel owns

- `NodeId`
- `EdgeId`
- `EdgeLocator`
- `VertexEntry`
- `EdgeEntry`
- forward / reverse base adjacency
- overflow chains
- local rebalance

### Property subsystem owns

- node properties
- edge properties
- property persistence
- property key/value encoding
- property lookup by entity
- future secondary property indexes

### Property subsystem does not own

- edge traversal
- label-aware adjacency placement
- neighbor iteration order
- physical edge identity

## Source of Truth

The rewrite implementation must treat stable-memory-backed property regions as
the source of truth.

In particular:

- `NodeRecord.properties` is not the long-term source of truth
- `EdgeRecord.properties` is not the long-term source of truth
- in-memory overlay `PropertyMap`s are a cache / materialized view
- canonical persisted state lives in stable memory

This is a deliberate departure from the current overlay-heavy integration state.

## Stable Memory Regions

The following region kinds already exist in the rewrite low-level model:

- `RegionKind::NodePropertyStore`
- `RegionKind::EdgePropertyStore`
- `RegionKind::PropertyIndex`
- `RegionKind::LabelCatalog`

The intended storage kind is:

- `NodePropertyStore`: `BucketChain`
- `EdgePropertyStore`: `BucketChain`
- `PropertyIndex`: `BucketChain`
- `LabelCatalog`: `BucketChain`

Property data is variable-length and must not be stored in PMA edge regions.

## Storage Model

The first implementation should use a bucket-backed KV store with append-log
semantics and rebuildable in-memory lookup state.

This means:

- stable memory stores variable-length records as raw bytes
- records are keyed by encoded entity/property identity
- runtime may rebuild an in-memory index by scanning the region
- later phases may migrate the same logical API to a more structured
  `(a,b)+ tree` / B+ style backend

This mirrors the `gleaph-old` direction:

- early append-log property persistence
- later migration to stable-memory `(a,b)+ tree`

## Key Space

The property store should use explicit byte keys that support prefix scans.

### Node property key

Conceptual form:

```text
N | node_id | property_name
```

### Edge property key

Conceptual form:

```text
E | edge_id | property_name
```

The exact binary format is implementation-defined, but it must preserve:

- unambiguous entity kind
- stable entity identity
- exact property-name recovery
- prefix-scan support by entity

Prefix-scan examples:

- all properties for one node:
  - prefix `N | node_id |`
- all properties for one edge:
  - prefix `E | edge_id |`

## Value Encoding

Property values are variable-length and should be encoded as explicit byte
payloads.

The rewrite should use a custom byte format, not embed Rust layout directly.

That format should be exposed through the rewrite-local `Storable`-equivalent
boundary, but serialization traits are only the boundary, not the layout
design itself.

### Recommended direction

- `PropertyKey`: custom encoded bytes
- `PropertyValueBlob`: custom encoded bytes
- rewrite-local stable serialization boundary used for:
  - `to_bytes`
  - `from_bytes`
  - `BOUND`

This keeps the implementation compatible with the current rewrite stable-memory
abstractions while preserving full control over the on-memory format.

## First-Phase Record Model

The first phase does not require a page-oriented tree layout.

It only requires:

- append one key/value record
- tombstone one key
- scan by entity prefix
- rebuild runtime lookup state by scanning records

Conceptual append-log record:

```text
record_header | key_bytes | value_bytes
```

Header responsibilities:

- key length
- value length
- tombstone flag
- optional format/version bits

## Minimal API

### Node properties

- `get_node_property(node_id, property_name) -> Option<Value>`
- `set_node_property(node_id, property_name, value) -> Result<()>`
- `remove_node_property(node_id, property_name) -> Result<()>`
- `scan_node_properties(node_id) -> PropertyMap`

### Edge properties

- `get_edge_property(edge_id, property_name) -> Option<Value>`
- `set_edge_property(edge_id, property_name, value) -> Result<()>`
- `remove_edge_property(edge_id, property_name) -> Result<()>`
- `scan_edge_properties(edge_id) -> PropertyMap`

## Secondary Property Indexes

Secondary property indexes are required long-term, but not in the first phase.

### Phase 1

- implement only `NodePropertyStore` and `EdgePropertyStore`
- property scans may rebuild or linearly scan runtime metadata
- `scan_nodes_by_property` / `scan_edges_by_property` may still fall back to
  overlay or full-scan behavior temporarily

### Phase 2

- implement `PropertyIndex` for equality lookups first
- likely key shapes:
  - `VN | property_name | encoded_value | node_id`
  - `VE | property_name | encoded_value | edge_id`

### PropertyIndex tree semantics

When `PropertyIndex` moves beyond append-log / scan-based fallback, its
meaningful direction should be a high-fanout `(a,b)`-tree with linked leaves.

The intended semantics are:

- internal nodes store routing keys only
- leaves store the actual ordered postings / entity bindings
- leaves are linked in key order
- ordered scans continue in the leaf layer without re-traversing internal nodes
- branching factor is configurable and should be chosen relative to bucket/page
  granularity

This is the preferred semantic model for `PropertyIndex`, because it matches
the property-index workload better than the current append-log:

- equality lookups
- prefix scans
- range scans
- low-allocation ordered iteration

The rewrite does not need to copy `gleaph-old` snapshot persistence here.
What should be reused is the logical API and node semantics:

- byte-oriented keys
- byte-oriented values/postings
- prefix-scan support
- internal routing-only nodes
- linked leaves for ordered traversal

Persistence should be redesigned around rewrite bucket-backed regions rather
than whole-tree snapshot blobs.

### PropertyIndex low-level shape

The intended low-level design for `PropertyIndex` is byte-oriented and split
into four concerns:

1. index key encoding
2. posting / entity binding encoding
3. internal / leaf node semantics
4. bucket-backed persistence

#### Index key encoding

The first index keys should target equality lookup.

Conceptual node-property equality key:

```text
VN | property_name_or_id | encoded_value | node_id
```

Conceptual edge-property equality key:

```text
VE | property_name_or_id | encoded_value | edge_id
```

Requirements:

- bytewise order must group entries by property first
- within one property, entries must group by encoded value
- entity id must come last so duplicate values remain uniquely ordered
- prefix scans must support:
  - all entities for one `(property, value)`
  - all values for one property

The property component may start as raw property-name bytes in early rewrite
iterations, but the long-term direction should allow interning to a compact
property id, similar to `gleaph-old`.

#### Posting / entity binding encoding

The first `PropertyIndex` can avoid a separate posting-list object and instead
store one entity binding directly per leaf entry:

```text
index_key_bytes -> empty payload or compact metadata payload
```

This means the entity id is already inside the key, and the value payload can
stay empty or hold only optional metadata.

That keeps phase-1 equality indexing simple:

- one indexed entity/property/value combination
- one ordered leaf entry
- prefix scan over `(property, value)` returns all matching entity ids

If later phases need compressed posting lists, the same leaf-linked tree
semantics can still be preserved while changing only the leaf payload model.

#### Internal / leaf node semantics

Internal nodes:

- store routing keys only
- do not store property payloads or postings
- route search to the correct child by key range

Leaf nodes:

- store ordered leaf entries
- each entry corresponds to one indexed entity binding
- leaves are linked in both directions
- scans continue leaf-to-leaf after the initial seek

Operational consequences:

- exact equality lookup:
  - seek to first key with the requested `(property, encoded_value)` prefix
  - iterate leaf entries while the prefix matches
- prefix scan by property:
  - seek to first key for that property
  - continue until prefix stops matching
- range scan:
  - seek to lower bound once
  - continue leaf-by-leaf until upper bound is exceeded

#### Bucket-backed persistence boundary

Unlike `gleaph-old`, rewrite should not treat the tree as an opaque whole-tree
snapshot blob as its final form.

Instead, persistence should align with region manager concepts:

- `RegionKind::PropertyIndex` is bucket-backed
- internal and leaf nodes are persisted as variable-length or fixed-class
  records inside bucket-backed regions
- the root reference and tree metadata live in region-level metadata
- leaf links are persisted explicitly

The implementation may still begin with coarse-grained snapshot writes during
bring-up, but the target persistence shape should support node-oriented writes
inside bucket-backed stable memory.

#### Comparison to current append-log phase

Current append-log property storage is still the correct phase-1 source of
truth for node/edge properties.

`PropertyIndex` exists to accelerate lookup, not to replace the property store.

So the relationship should remain:

- property store:
  - canonical values
  - append-log first
- property index:
  - derived search structure
  - leaf-linked `(a,b)`-tree semantics

### Phase 3

- range-index support
- planner selectivity integration
- stable statistics / catalog integration

## Label Storage Relationship

Labels and properties should be treated similarly in one respect:

- both are variable-length at the string/schema layer

But they differ in hot-path requirements:

- adjacency hot path stores packed `LabelId`
- property hot path does not exist in adjacency regions

Therefore:

- label names belong in `LabelCatalog`
- edge/node property values belong in property store regions
- packed `LabelId` stays in adjacency metadata

## Overlay Migration Plan

Current rewrite integration stores node/edge properties in overlay record maps.

That is temporary.

Migration steps:

1. implement stable-memory-backed node/edge property store
2. write property mutations into the property store
3. hydrate overlay records from the property store
4. treat overlay `PropertyMap` as cache only
5. add property index regions later

## First Implementation Plan

### Step 1

Create rewrite-side property-store spec and types:

- `PropertyEntityKind`
- `PropertyKey`
- `PropertyStoreError`
- encode/decode helpers

### Step 2

Implement bucket-backed append-log runtime:

- `NodePropertyStoreRuntime`
- `EdgePropertyStoreRuntime`
- append record
- tombstone record
- rebuild in-memory latest-value map

### Step 3

Wire integration overlay reads/writes through the property store:

- `set_node_property`
- `remove_node_property`
- `set_edge_property`
- `remove_edge_property`
- `scan_nodes_by_property`
- `scan_edges_by_property`

### Step 4

Add persistent hydration path for overlay records:

- bootstrap from stable-memory property store
- no longer rely on in-memory-only property truth

### Step 5

Add `PropertyIndex`

## Summary

The rewrite property subsystem should be:

- bucket-backed
- variable-length
- bytes-first
- adjacency-external
- stable-memory authoritative
- initially append-log based
- later upgradeable to a more structured `(a,b)+ tree` backend

This keeps the rewrite aligned with the old design direction while fitting the
new region/bucket model already present in the current `graph-pma` rewrite.

## Incremental flush (extent-backed regions)

For `RegionStorageKind::Extent` node/edge property regions, flush contracts match
[`graph-pma-low-level-spec.md`](graph-pma-low-level-spec.md)
("Node and edge property store regions"):

- Update `logical_len_bytes` to the encoded append-log length **before** writing
  payload bytes.
- Write **only** the serialized payload (`encoded.len()`), not the full extent
  capacity.
- When the logical payload **shrinks**, zero-clear the stable span from the new
  logical end through the previous logical end so truncated tail bytes are not
  left stale (same class of invariant as PIDX shrink handling).

Bucket-chain-backed property regions continue to use whole-bucket writes on
flush; narrowing those writes is a separate follow-up tied to bucket boundaries.
