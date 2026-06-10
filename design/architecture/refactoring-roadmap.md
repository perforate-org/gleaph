# Gleaph Refactoring Roadmap

Last updated: 2026-06-10 UTC  
Status: In progress (Phase 0 complete; Phase 2 started)  
Anchor timestamp: 2026-06-10 13:39:55 UTC +0000

## Purpose

Define a phased refactoring plan for reducing long-lived technical debt in Gleaph without weakening stable-storage safety, query semantics, or canister boundary ownership.

This document is a planning contract. It does not describe shipped behavior unless a section explicitly says so. Each phase should be implemented through small patches that update the relevant design document and tests in the same change.

## Non-goals

- A big-bang rewrite of graph storage, router dispatch, or GQL planning.
- A blanket reduction in crate count.
- A blanket merge of stable-memory regions.
- Preserving backward compatibility with existing development data.
- Updating crate, schema, or stable-layout version numbers as part of every refactoring patch.
- Moving Gleaph-specific or Internet Computer-specific behavior into `gleaph-gql` or `gleaph-gql-planner`.
- Changing LARA scan, tombstone, payload, or free-span semantics without a dedicated ADR.

## Development Compatibility Policy

Gleaph is still under active development, so this roadmap does not require backward compatibility with earlier development snapshots. Refactoring patches may reshape public-internal APIs, stable-memory layouts, persisted key formats, and storage-domain boundaries when doing so produces a cleaner source of truth or stronger invariant ownership.

Because backward compatibility is not a requirement, a refactor does not need to preserve old data or update version numbers merely to describe an incompatible development-only layout change. The required work is instead:

- Make the new owner of each invariant explicit.
- Update the relevant design document or ADR.
- Add or update tests for the new contract.
- Run focused benchmarks when the changed path is performance-sensitive.
- Clearly mark any planned behavior that is not implemented yet.

## Refactoring Principles

### Module layout

When splitting a module into submodules, use `parent.rs` plus `parent/child.rs`. Do not introduce `parent/mod.rs` as the parent module file.

Example (preferred):

```text
facade/store.rs
facade/store/registry.rs
facade/store/placement.rs
```

Avoid:

```text
facade/store/mod.rs
facade/store/registry.rs
```

The graph `GraphStore` layout (`crates/graph/src/facade/store.rs` with `facade/store/*.rs` domain files) is the reference pattern for Phase 2 facade splits.

### Source of truth before shape

The refactor should first identify who owns each fact and invariant, then reshape code around that ownership. Reducing duplicate code is useful only when it also reduces duplicate knowledge.

| Domain fact | Source of truth |
|-------------|-----------------|
| Vertex and canonical edge existence | `gleaph-graph` / LARA-backed graph storage |
| Canonical edge identity | `owner_vertex_id`, `label_id`, `edge_slot_index` |
| Edge payload bytes | Labeled LARA payload slab/log/blob stores |
| Vertex and edge property values | Graph property stores |
| Property and label names in federated planning | Router catalogs |
| Router placement | Router placement maps |
| Global property postings | `graph-index` property postings |
| Global vertex label postings | `graph-index` label postings |
| Local edge equality postings | Graph shard local edge equality store |
| Label telemetry | Router aggregate state derived from graph shard events |

### Canonical and derived state must be separated

Canonical state should be recoverable without consulting derived state. Derived state may be optimized for read paths, but it needs one update path and, where possible, one rebuild or backfill path.

Canonical state includes:

- Vertex rows.
- Canonical forward edges.
- Edge payload bytes.
- Property values.
- Router placement records.
- Label and property catalogs.
- Mutation idempotency records.

Derived or rebuildable state includes:

- Reverse adjacency.
- Edge aliases.
- Edge equality postings.
- Global property postings.
- Vertex label postings.
- Label telemetry.
- Backfill cursors and maintenance queues.

### Storage layout changes require explicit gates

Any change that affects stable memory layout, memory ids, persisted key encodings, LARA slab layout, payload layout, postings layout, or router idempotency records requires:

1. A design update or ADR.
2. Reopen or upgrade tests.
3. Targeted behavior tests for the invariant being moved.
4. Relevant canbench or criterion benchmarks for hot paths.

These gates protect the new contract; they are not a requirement to preserve old development data or bump layout versions.

## Data-layer Debt Themes

### Hidden transaction boundaries

`GraphStore`, `RouterStore`, and `IndexStore` are stateless facades over thread-local stable structures. That keeps call sites simple, but it also hides which operations must update several stores together.

The refactor should introduce explicit storage-domain methods before changing stable layout. For example:

- Property write = primary property store mutation plus posting-maintenance event.
- Edge insert = canonical edge write plus reverse/alias handling, payload-width validation, telemetry, and local edge postings.
- Router placement update = logical placement plus reverse physical lookup plus mutation/idempotency state where applicable.

### Duplicate catalog patterns

The codebase has several bidirectional catalog patterns:

- Router vertex-label name/id maps.
- Router edge-label name/id maps.
- Router property name/id maps.
- Graph property catalog.
- Edge weight and payload profile catalogs.

The desired direction is one reusable stable bidirectional catalog implementation, with domain-specific allocation policy supplied by the owning module. Reserved ids, sparse allocation behavior, manual insertion rules, and max-id limits should not be reimplemented in multiple places.

### Rigid property storage

Vertex and edge properties are separate stores with similar value encoding and validation behavior. Edge property identity is tied to owner vertex, label id, and slot index. That identity is correct, but the data layer is not flexible enough for future entity classes or richer indexing rules.

The desired direction is an internal property entity model:

| Entity | Identity |
|--------|----------|
| Vertex | `VertexId` |
| Edge | `owner_vertex_id`, `label_id`, `edge_slot_index` |

Physical stable keys may remain separate until migration is justified. The important refactor is centralizing value validation, persisted encoding, sortable index-key encoding, and posting-event generation.

### Index ownership drift

Distributed property and label reads should be router-owned. Graph shards should execute local plans and maintain local state. Graph-index should own postings and posting-local operations, not graph traversal.

Legacy direct graph index paths should be isolated and removed when router seed routing covers the corresponding query shapes.

### LARA and payload coupling

Edge rows and payload bytes must stay aligned in logical slot order. Payload compaction and edge compaction should remain separate physical stores but one logical operation. Labeled LARA changes must preserve:

- Tombstone skipping.
- Fail-fast payload reads.
- `label_id` plus `edge_slot_index` edge identity.
- Segment-footprint retirement semantics for leaf relocation.
- Scan paths that do not consult PMA maintenance stores.

## VirtualMemory Policy

### Recommendation

Use a hybrid policy. Do not consolidate all persistent data into one custom memory region as an early refactor.

As of 2026-06-10, the use of many `VirtualMemory<DefaultMemoryImpl>` regions has real overhead, but the separation also encodes failure isolation, growth isolation, and ownership boundaries. A monolithic memory would make migration, corruption isolation, and stable-structure reuse harder before the codebase has explicit storage-domain APIs.

### Keep separated

Keep separate stable-memory regions when any of these are true:

- The data is canonical while the neighbor data is derived.
- The stores have different rebuild or backfill strategies.
- The stores grow at substantially different rates.
- The stores use different access patterns, such as slab scans, log append, point lookup, or prefix range scans.
- The stores have different failure recovery behavior.
- The stores are owned by different architectural domains.

Examples:

- Forward adjacency and reverse adjacency.
- Edge slab, payload slab, payload log, and payload blobs.
- Property values and property postings.
- Router placement and label telemetry.
- Canonical graph data and maintenance queues.
- Graph-index property postings and label postings.

### Consider consolidation

Consider consolidation only after the owner API is explicit and benchmarks show a measurable win.

Good candidates:

- Small bidirectional catalog maps updated together.
- Edge weight profile and edge payload profile state if weight becomes a compatibility view over payload profiles.
- Tiny metadata cells that share identical lifecycle and upgrade behavior.
- Maintenance metadata that is always read and written as one unit.

### Required decision artifact

Before changing memory layout, add a stable-memory layout ADR that records:

- Memory ids before and after the change.
- Canonical vs derived classification.
- Reopen strategy for the new layout.
- Whether old development data is intentionally discarded.
- Corruption isolation impact.
- Benchmark evidence.
- Any compatibility choice that is intentionally retained.

## Phased Plan

### Phase 0: Inventory and contract freeze

Goal: make the existing system legible before changing it.

Deliverables:

- Add a stable-memory inventory for router, graph, and graph-index.
- Classify each memory as canonical, derived, maintenance, catalog, telemetry, or compatibility.
- Document the rebuild path for each derived store.
- Mark design docs that are planned, partially implemented, or implemented.
- Identify stale sections that describe target behavior as shipped behavior.

Output artifact: [`design/storage/stable-memory-inventory.md`](../storage/stable-memory-inventory.md)

Primary files:

- `crates/graph/src/facade/stable/memory.rs`
- `crates/router/src/facade/stable/memory.rs`
- `crates/graph-index/src/facade/stable/memory.rs`
- `design/storage/lara-and-facade.md`
- `design/index/property-index.md`
- `design/index/label-index.md`

Exit criteria:

- A contributor can identify the owner and rebuildability of every stable memory region.
- No new storage refactor begins without naming the affected invariant and design doc.

### Phase 1: Quarantine inactive implementation trees

Goal: reduce search noise and accidental reuse of obsolete concepts.

Deliverables:

- Decide whether `old_crates/`, `frontend-old/`, and `escape_crates/` are archived reference material or removable code.
- If retained, add archive markers that state they are not active implementation boundaries.
- Keep active workspace members limited to crates that participate in supported behavior.

Exit criteria:

- Repository search results clearly distinguish active implementation from archived reference.
- New refactor work does not copy concepts from inactive crates without an explicit design reason.

### Phase 2: Introduce storage-domain APIs

Goal: make multi-store invariants explicit while preserving the existing stable layout.

**Status: Complete (2026-06-10).**

**Progress:** All three facades (`graph`, `router`, `graph-index`) use `facade/store.rs` + `facade/store/*.rs` domain modules (not `store/mod.rs`). Graph `GraphStore` domain commits cover adjacency, properties, labels, vertex delete, remote refs, telemetry, edge profiles, local indexes, and sidecar coordination; mutation and federation call sites route through `commit_*` APIs. Router domains cover registry (including controllers), placement, catalogs (`commit_intern_*`), idempotency, telemetry (`commit_apply_label_delta`), and backfill. Graph-index domains cover authorization (`commit_register_shard_owner`), property postings (`commit_posting_*`), and label postings (`commit_label_posting_*`).

Deliverables:

- Split graph facade behavior into storage domains: adjacency, properties, labels, edge profiles, remote refs, local indexes, telemetry, and maintenance. **Done.**
- Split router facade behavior into registry, placement, resolution catalogs, idempotency, telemetry, and backfill domains. **Done.**
- Split graph-index behavior into property postings, label postings, shard ownership, and router authorization domains. **Done.**
- Move repeated write sequences behind methods owned by the invariant owner. **Done** for graph/router/graph-index mutation paths; query planners still read derived indexes directly where read-only.

Exit criteria:

- Call sites no longer coordinate canonical data and derived state manually. **Met** for graph edge/vertex mutations, label telemetry, and federation forward-in maintenance; router label telemetry and catalog intern paths.
- Tests can target domain APIs rather than scattered thread-local stores. **Met** for graph `facade::store` and `sidecar` domain tests; graph-index and router tests exercise `IndexStore` / `RouterStore` domain methods.

### Phase 3: Unify catalog abstractions

Goal: remove duplicated name/id catalog rules.

**Status: Complete (2026-06-10).**

**Progress:** `gleaph-graph-kernel::bidirectional_catalog` provides shared `BidirectionalCatalog` with sparse and dense allocation policies. Graph property catalog and router vertex/edge/property resolution catalogs use the shared type (same stable memory regions). Router retains ownership of federated label and property resolution APIs. Edge weight profiles are a compatibility view over canonical payload profiles (`to_weight_profile` on read; new installs write payload only).

Deliverables:

- Implement a reusable stable bidirectional catalog abstraction. **Done** (`bidirectional_catalog` in `graph-kernel`).
- Move router label catalogs and property catalogs onto the shared implementation where the semantics match. **Done** (vertex/edge dense, property dense; graph property sparse).
- Preserve router ownership of federated label and property resolution. **Done** (router `catalogs` domain unchanged at API boundary).
- Evaluate graph property catalog migration separately from router catalogs. **Done** (graph re-exports shared catalog with `SparseFromOnePolicy`).
- Convert edge weight profiles into a compatibility layer over edge payload profiles, if the compatibility surface is still required. **Done** (`EdgePayloadProfile::to_weight_profile`; weight install writes payload only; `EDGE_WEIGHT_PROFILES` read fallback for legacy stable data).

Exit criteria:

- Reserved-id and sparse-allocation behavior is implemented once. **Met** for sparse (`SparseFromOnePolicy`) and dense (`DenseMaxPlusOnePolicy` / `DenseEdgeLabelPolicy`).
- Bidirectional map consistency is enforced by one abstraction. **Met** for property and router resolution catalogs.
- General-purpose GQL crates remain free of Gleaph-specific catalog behavior. **Met** (catalog lives in `graph-kernel`, not `gql`).

### Phase 4: Refactor property storage and indexing events

Goal: make properties flexible without duplicating value encoding rules.

**Status: Complete (2026-06-10).**

**Progress:** `PropertyEntity` in `graph-kernel` names vertex and edge property hosts. Graph `property` module owns persisted validation (`ensure_persistable`), explicit `PropertyIndexability`, shared `index_ops_for_value_change`, and `dispatch_property_index_ops` for federated vertex and local edge equality backends.

Deliverables:

- Introduce an internal property entity identity model for vertex and edge properties. **Done** (`PropertyEntity` in `graph-kernel`).
- Centralize persisted value encoding and sortable index-key encoding. **Done** (`ensure_persistable` / `ensure_property_id` in vertex and edge stores; `sortable_index_key`).
- Make indexability explicit instead of implicit in scattered call sites. **Done** (`PropertyIndexability`).
- Replace ad hoc pending posting calls with typed property change events. **Done** (`PropertyValueChange` → `dispatch_property_index_ops`).
- Keep physical vertex and edge property stores separate until a stable-layout ADR justifies migration. **Done** (no store merge).

Exit criteria:

- Property writes produce primary storage changes and index-maintenance events through one path. **Met** (`properties.rs` commits + `dispatch_property_index_ops`).
- Index-only misses for unindexable values are documented and tested. **Met** (`property-index.md`, `property::change` / `index_key` tests).
- Vertex and edge property APIs share validation semantics without sharing the wrong physical key layout. **Met** (shared `ensure_*` helpers; separate stable keys unchanged).

### Phase 5: Rebuildable derived-state boundaries

Goal: make derived state safe to optimize, rebuild, and validate.

**Status: In progress (2026-06-10).**

**Progress:** Edge equality postings and edge aliases have consistency checks + full rebuild from canonical state (`facade/derived_state/`). Label postings backfill was already implemented (`label_backfill.rs`). Sync vs backfill lag documented in [stable-memory-inventory.md](../storage/stable-memory-inventory.md).

Deliverables:

- Document and test rebuild or backfill paths for edge aliases, edge equality postings, property postings, label postings, and label telemetry. **In progress** (edge equality, edge aliases, label postings, property postings **done**; label telemetry pending).
- Add consistency checks between canonical graph state and derived indexes. **In progress** (edge equality, edge aliases **done**).
- Decide which derived stores must be synchronously updated and which can tolerate backfill lag. **In progress** (inventory table updated for edge equality, label postings, property postings).
- Keep query semantics honest when derived state may be stale or unavailable. **Not started.**

Exit criteria:

- Derived state has a named canonical source and one update path. **Met** for edge equality and label postings.
- Tests cover canonical mutation plus derived-state observation. **Met** for edge equality (`derived_state::edge_equality` tests).
- Backfill state is not mistaken for canonical state. **Partial** (label and property backfill cursors are maintenance class; edge equality rebuild is explicit derived repair).

### Phase 6: LARA and payload physical cleanup

Goal: reduce low-level waste without weakening LARA contracts.

Deliverables:

- Continue moving labeled edge byte management toward segment-footprint retirement rather than per-vertex peel behavior.
- Keep edge rows and payload bytes aligned by logical slot order during compaction.
- Centralize dense/tiled payload offset math and batch traversal helpers.
- Preserve `LabeledOperationError`, tombstone skipping, and fail-fast value-log reads.
- Add high-degree, many-label regression tests and canbench coverage.

Exit criteria:

- Scan paths do not consult PMA maintenance state.
- Segment relocation releases retired physical ranges only after live pointers are rewritten.
- Payload and edge compaction preserve the same logical order.
- Hot-hub insertion and traversal costs are measured before and after.

### Phase 7: Query, router, and index boundary cleanup

Goal: keep distributed query planning and index routing in the owning layer.

Deliverables:

- Complete router-owned index seed routing for supported federated query shapes.
- Remove or isolate graph direct-index query paths once router coverage is sufficient.
- Keep graph-index APIs posting-local: lookup, range, intersection, count, label membership, and paginated export.
- Keep graph shard execution local except for explicit federation expand paths.
- Update design docs when an unsupported multi-shard fallback becomes implemented or intentionally rejected.

Exit criteria:

- Router owns distributed index reads and shard slicing.
- Graph shards do not need global index access for query hot paths.
- Graph-index does not know graph traversal semantics.

### Phase 8: Stable-memory layout policy and measured consolidation

Goal: consolidate only where it improves efficiency without damaging ownership or recovery.

Deliverables:

- Add a stable-memory layout ADR before changing memory ids or physical layout.
- Introduce a named memory-layout registry for graph, router, and graph-index.
- Benchmark many small `VirtualMemory` regions versus grouped metadata regions.
- Prototype catalog/profile consolidation only after domain APIs are explicit.
- Keep canonical and derived stores separate unless the ADR justifies the coupling.
- State explicitly when old development data is not migrated.

Exit criteria:

- Any consolidation has benchmark evidence.
- Upgrade and reopen tests cover the migration.
- Failure isolation impact is documented.

### Phase 9: Validation and release gates

Goal: make refactoring progress safe to merge incrementally.

Required tests:

- Property write, replace, and delete update primary storage and posting events exactly once.
- Vertex delete removes properties and derived postings.
- Edge delete removes edge properties, aliases, local edge postings, and payloads.
- Reverse adjacency and alias rebuild preserve canonical edge identity.
- Payload compaction preserves edge/payload slot order.
- Router idempotency preserves client mutation key, request fingerprint, and zero-shard completion semantics.
- Label and property catalogs reject conflicting mappings and round-trip after reopen.

Required benchmarks:

- High-degree labeled hub insert and rebalance.
- Payload predicate expand.
- Property equality and intersection seed routing.
- Label seed pagination.
- Bulk ingest and finalize.
- Router dispatch with index anchors.

Validation sequence:

1. `cargo fmt --check`
2. Targeted crate tests for the changed boundary.
3. Broader workspace tests when shared contracts move.
4. Relevant `canbench` or criterion runs.
5. Persist benchmark results only when the benchmark baseline intentionally changes.

## Suggested Implementation Order

1. Add memory inventory and design status cleanup.
2. Archive or mark inactive implementation trees.
3. Introduce storage-domain APIs without changing stable layout.
4. Unify catalog rules.
5. Centralize property value and index-event behavior.
6. Add rebuildable derived-state checks.
7. Continue LARA/payload physical cleanup.
8. Remove legacy direct-index query paths.
9. Decide stable-memory consolidation from benchmark evidence.

This order intentionally defers memory-layout consolidation until the owning APIs and invariants are explicit. That keeps the refactor useful even if later benchmark evidence says the existing VirtualMemory separation should mostly remain.

## Related Documents

- [Stable-memory inventory](../storage/stable-memory-inventory.md)
- [System overview](overview.md)
- [LARA and graph facade](../storage/lara-and-facade.md)
- [LARA](../storage/lara.md)
- [Labeled edge payload storage](../storage/labeled-edge-payloads.md)
- [Property index](../index/property-index.md)
- [Label index](../index/label-index.md)
- [Federation target](../sharding/federation-target.md)
- [GQL layers](../gql/layers.md)
