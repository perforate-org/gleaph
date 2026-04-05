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

`NodeId` is represented as a packed 40-bit kernel type (5 bytes, big-endian
payload in wire layouts). The adjacency kernel should treat that packed form as
the native vertex id representation.

Overflow beyond 40 bits must fail fast. Silent widening or truncation is not
acceptable.

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
    target: PackedNodeId40, // 5 bytes BE on wire
    meta: u24,              // 3 bytes LE on wire (24-bit packed flags + payload)
}
```

Conceptually:

- `target`: neighbor local vertex id
- `meta`: tombstone, shard vs local label, optional undirected tag, RSV bits,
  and a 16-bit payload (local `LabelId` or shard-directory slot index)

This is the only information the traversal kernel should need for the common
case.

### Undirected semantic tag (bit 21)

When the logical edge is **undirected** (GQL `~`), implementations set the packed **undirected** flag on **both** directional hot entries for the same `EdgeId`. The kernel-facing [`EdgeRecord::undirected`](../crates/graph-kernel/src/records.rs) mirrors that bit for DML and for expand filtering above the PMA overlay. Keeping forward and reverse metadata aligned simplifies maintenance and avoids divergent interpretations during neighborhood walks.

**DDL vs runtime:** `UNDIRECTED EDGE` / `DIRECTED EDGE` in a graph type definition is enforced at plan time via [`PropertySchema::edge_is_undirected`](../crates/gql/src/type_check/schema.rs) (e.g. [`GraphTypePropertySchema`](../crates/gql/src/type_check/graph_type_schema.rs) built from inline `CREATE GRAPH` types). INSERT patterns must use matching syntax (`~[L]~` for undirected schema labels, arrows for directed); the executor still derives stored `undirected` from the physical `InsertEdge` op produced by the planner.

### Why `edge_id` is not in the hot entry

The hot path should not depend on semantic identity.

`EdgeId` belongs in sidecars or locator mappings, not in every adjacency entry,
unless measurement later proves the extra 8 bytes are worth it.

## Shard canister directory (cross-canister edges)

For edges that represent a stub vertex on a **remote** canister, packed `EdgeMeta`
may use the cross-shard bit; the low 16 bits of the payload then name a **dense
slot** in a `ShardCanisterDirectory`, not a `LabelId`.

**On-disk / stable memory:**

- Payload format: magic `SCD1`, little-endian `u32` count, then each principal as
  `u16` byte length + raw bytes (`ShardCanisterDirectory::encode_bytes` in
  `crates/graph-pma`).
- Stored as its own extent region: `RegionKind::ShardCanisterDirectory`.
- Flushed with the rest of the graph via `GraphPma::try_write_all_to_stable_memory`
  (and the dirty refresh path).

**Hydration policy (strict):** after decoding the directory,
`GraphRuntime::validate_shard_canister_slots` requires every live cross-shard edge (forward
and reverse, base and overflow) to reference `slot < directory.len()`. A mismatch
fails hydration with `HydrationError::ShardCanisterSlotOutOfRange`. Corrupt `SCD1`
bytes fail with `HydrationError::InvalidShardCanisterDirectory`.

**Future:** if slots are compacted or reordered (GC), edge metadata and the
directory must be updated in the same persistence unit; slot ids are otherwise
assumed stable for the lifetime of the stored graph image.

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

## Internet Computer: full property search stack (target)

This section ties **persistent graph state** to **Gleaph query execution** the
way a canister is expected to wire them. It is a contract for *big-picture*
work; individual crates keep their own finer-grained docs.

### End-to-end data flow

1. **Stable memory** holds region-managed bytes: adjacency, property append
   logs, property-index snapshots / paged node stores, and related metadata.
2. **`GraphPma`** hydrates from those regions and exposes mutation +
   `try_write_all_to_stable_memory` (or incremental write paths) after updates.
3. **`GraphPmaKernelOverlayGraph`** implements **`GraphRead` / `GraphWrite`** for
   the kernel-facing graph service: traversals, DML, and property lookups merge
   hydrated structures with any dirty overlay state.
4. **Property equality search** for queries must converge on:
   - **clean index:** prefix scans in stable memory (
     `scan_*_property_index_*_from_stable_memory`) keyed by binary-encoded
     values, or an equivalent direct node-store walk; then resolve entities.
   - **dirty stores / indexes:** the append-log and in-memory
     `PropertyIndex` / `PropertyIndexNodeStore` until the next flush, without
   requiring a full graph scan for exact equality when the index is
   authoritative.
5. **`gleaph-gql-planner`** chooses **`IndexScan`**, multi-predicate **`IndexIntersection`**,
   and index-backed edge equality when **catalog stats** (
   `indexed_vertex_properties`, `indexed_edge_properties`, selectivity,
   cardinality) say the property is indexed—this is the **canister-facing
   contract for “indexed means planner may skip NodeScan”** (for edges: may use
   `scan_edges_by_property` during expansion instead of full incident-edge scan).
   - **`IndexIntersection`** is executed by intersecting candidate **`NodeId`**
     sets from repeated **`GraphRead::scan_nodes_by_property`** calls (see
     `gleaph-gql-executor`).
   - **Indexed edge equality** is usually carried on **`Expand` / `ExpandFilter`**
     as `indexed_edge_equality: Option<(property, ScanValue)>` and executed via
     **`scan_edges_by_property`** while preserving the already-bound source node
     (see `gql-executor`).
   - **Leading `EdgeIndexScan`:** when the **first path node** would only produce
     an **unlabeled full vertex scan** and the **first hop** is a **directed**
     edge with **indexed** property equality (per stats), the planner seeds the
     path with **`EdgeIndexScan`** then **`EdgeBindEndpoints`** (binds near/far
     nodes from the edge’s `src`/`dst`), applies residual edge filters, then
     continues with later hops. This avoids scanning all vertices before using
     the edge index. **`EdgeIndexScan`** is still valid as a standalone op for
     tests and manual plans. Stack coverage: **`build_plan`** +
     `gql-executor/tests/canister_property_search_stack.rs`.
   - **`{edge_var}__hop_aux`:** the planner may bind an auxiliary scalar per
     matched edge (backed by **`GraphRead::hop_aux_bytes_for_edge`**, e.g. IC shard
     metadata). The name is always **`{edge_var}__hop_aux`** for the physical edge
     variable (including synthetic **`__anon_eN`** when the pattern omits a name).
     **`RETURN *` does not project this column**; it appears in the plan only when
     the query **explicitly references** that variable (e.g. in **`RETURN`** or
     **`WHERE`**), via **`linear_query_referenced_variables`** in **`gleaph-gql-planner`**.
     **`Expand`**, **`ExpandFilter`**, **`EdgeBindEndpoints`**, and each hop in
     **`WorstCaseOptimalJoin`** (**`WcojEdge`**) carry an optional
     **`hop_aux_binding`** when referenced.
   - **`WorstCaseOptimalJoin`**: for simple cyclic **`Expand`** chains (no
     variable-length / indexed-edge fusion / `ExpandFilter`), the planner may fuse
     hops into one op. The executor evaluates the cycle via bounded backtracking on
     **`GraphRead::expand`** (generic binary-relation join; output capped per input
     row for safety). When a fused hop would have carried **`hop_aux_binding`** on
     **`Expand`**, the same binding is preserved on the corresponding **`WcojEdge`**.
6. **`gleaph-gql-executor`** maps those ops to **`GraphRead::scan_nodes_by_property`** /
   **`scan_edges_by_property`** (and filters). No executor shortcut may bypass
   the kernel trait surface for persisted results.
   - **Preflight:** before running ops, the executor calls
     **`first_executor_unsupported_op`** (`gql-planner`) so plans that include
     unimplemented operators (e.g. **`ShortestPath.path_var`**, **`Let`**,
     variable-length **`Expand`**) fail
     early with **`InvalidPlan`** and a stable operator name. **`ShortestPath`**
     itself runs via unweighted BFS on **`GraphRead::expand`** (hop bounds from
     the path quantifier). **IC / replica errors** (cycles, traps) are still
     handled by the **canister host**, not inside `gql-executor`.
   - **Bound parameters:** the planner often lowers indexed equality predicates
     to **`IndexScan(..., Parameter("$propertyName"), ...)`**. The canister /
     `gleaph` runtime must pass the same values through **`ExecutionContext::params`**
     (e.g. `"uid" → Text("alice")` for parameter `"$uid"`). Missing bindings produce
     null scan keys and empty results even when the index is correct.

### Design commitments (bold moves)

- **Single kernel trait boundary:** all search and mutation semantics for the
  query engine go through **`GraphRead` / `GraphWrite`**. Canister code holds a
  `G: GraphRead + GraphWrite` (backed by `graph-pma`), not ad-hoc index calls
  in the executor.
- **Index authority:** when the planner emits an index scan, the graph
  implementation must return **the same** bindings as a filter after a full
  scan, modulo intentional unsupported types. Tests in `gql-executor` lock this
  stack-wise.
- **Stats are part of the API:** deploying a canister must ship **planner stats**
  (or a derived catalog) consistent with what is actually indexed on stable
  memory; mismatches are correctness bugs, not “optimizer quirks”.
- **Persistence rule:** any leaf or record that must survive upgrades uses
 **`PropertyIndexNodeStore::encode_node_page`** (or the documented successor) at
  the configured page size; multi-page experimental encodings stay off the
  hot persistence path.

### Observability and benchmarks

- **Contract tests:** planner + executor + `GraphPmaKernelOverlayGraph` (or
  harness equivalent) for indexed equality queries.
- **Benchmarks:** hot paths are **stable-memory equality scans** and **index-
  backed `GraphRead::scan_*_by_property`** over increasing entry counts;
  regressions should be caught in CI when benches are run.
- **Wasm SIMD:** property-index `PropertyIndexKey` comparisons use a vectorized
  path when the canister (or `wasm32-unknown-unknown` bench build) is compiled
  with **`-C target-feature=+simd128`** (e.g. `RUSTFLAGS='-C target-feature=+simd128' cargo build ...`).
  Builds without that flag still use a portable scalar fast path (big-endian `u64` chunks).

### Non-goals (this milestone)

- Arbitrary full-text or range indexes without a planned storage layout.
- Pushing property-index logic into the executor or planner beyond **plan
  selection**—storage stays in `graph-pma`.
