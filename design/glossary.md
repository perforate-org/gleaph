# Glossary

Last updated: 2026-06-17
Anchor timestamp: 2026-06-17 10:34:31 UTC +0000

Terms used across Gleaph design documents. Canonical types live in **`gleaph-graph-kernel`** unless noted.

See [adr/0005-vertex-identity.md](adr/0005-vertex-identity.md), [adr/0006-pre-federation-foundation.md](adr/0006-pre-federation-foundation.md), and [adr/0017-graph-vertex-existence-ssot.md](adr/0017-graph-vertex-existence-ssot.md) for identity and existence policies.

## Identity and liveness

| Term | Type / location | Meaning |
|------|-----------------|--------|
| **Shard id** | `ShardId` (`u32` newtype) | Partition of a logical graph. Semantics are graph-local (`GraphId`-scoped), so `ShardId(0)` exists per graph. Standalone sole shard is **`ShardId(0)`**. |
| **Local vertex id** | `LocalVertexId` / LARA `VertexId` | Dense id within one graph shard’s CSR store; not reused after delete. |
| **Global vertex id** | `GlobalVertexId` | Canonical graph-scoped key: `{ shard_id, local_vertex_id }` (8 bytes LE). Interpreted only under an explicit `GraphId` context. |
| **Global edge id** | `GlobalEdgeId` | Query-time graph-scoped physical edge handle: `{ shard_id, owner_local, edge_slot_index }` (12 bytes). Interpreted only with graph context; not stable across compaction. |
| **Encoded vertex id** | `EncodedVertexId` (`[u8; 8]`) | Opaque client wire id for vertices (`ELEMENT_ID`, path elements). Bijective encoding of `GlobalVertexId` under a per-graph `ElementIdEncodingKey`; encoded bytes are graph-context-bound handles. |
| **Encoded edge id** | `EncodedEdgeId` (`[u8; 12]`) | Opaque client wire id for edges in paths and `ELEMENT_ID`; encoded bytes are graph-context-bound handles. |
| **Vertex liveness** | Graph CSR tombstone bit | Authoritative existence on a shard: row in range and not tombstoned ([ADR 0017](adr/0017-graph-vertex-existence-ssot.md)). |
| **Physical placement key** | `PhysicalPlacementKey` | Type alias for `GlobalVertexId` (deprecated name). |
| **Remote vertex id** | `RemoteVertexId` | Shard-local 30-bit handle inside `VertexRef` for remote CSR endpoints — kernel type only; **no graph stable yet**. |
| **Standalone mode** | [sharding/standalone-mode.md](sharding/standalone-mode.md) | `n = 1` shard: `GlobalVertexId(0, local)`; router catalogs; encoded element ids on the wire. |

**Removed terms:** `LogicalVertexId`, `VertexPlacement`, `ROUTER_PLACEMENTS`, router placement APIs.

## Catalogs (federation)

| Term | Owner | Meaning |
|------|-------|---------|
| **Property id** | Router `ROUTER_PROPERTY_CATALOG` | Name ↔ `PropertyId` SSOT **per `GraphId`** (`GraphScopedNameCatalog`; [ADR 0018](adr/0018-graph-scoped-label-property-catalogs.md)). Same string in two graphs may map to different ids. Graph stores values only. |
| **Vertex label id** | Router `ROUTER_VERTEX_LABEL_CATALOG` | Name ↔ `VertexLabelId` **per `GraphId`**. Graph stores label sets by id. |
| **Edge label id** | Router `ROUTER_EDGE_LABEL_CATALOG` | Name ↔ `EdgeLabelId` **per `GraphId`**. |
| **Resolved property table** | Plan wire (`ResolvedPropertyTable`) | Router-supplied name→id map for the dispatch **`GraphId`**, attached to `ExecutePlanArgs` for graph DML/scan. |
| **Graph-scoped name catalog** | `GraphScopedNameCatalog<Id>` in graph-kernel | Shared router abstraction for `(GraphId, name) ↔ id` partitions ([ADR 0018](adr/0018-graph-scoped-label-property-catalogs.md)). |

## GQL graph type catalog (router)

Distinct from federation **property graph** registration ([ADR 0011](adr/0011-gql-graph-resolution-and-catalog-scoping.md)) and from label/property id catalogs above.

| Term | Owner / region | Meaning |
|------|----------------|---------|
| **`GraphCatalog`** | Router regions **21–22** (`ROUTER_GQL_GRAPH_CATALOG`) | SSOT for GQL **graph type definitions** and **per-graph schema bindings** ([ADR 0013](adr/0013-gql-graph-type-catalog-on-router.md)). |
| **`ROUTER_GRAPH_CATALOG`** | Regions **14–15** | Federation **property graph name ↔ `GraphId`** — prerequisite for `CREATE GRAPH g …` ([ADR 0011](adr/0011-gql-graph-resolution-and-catalog-scoping.md)). |
| **`ROUTER_GRAPH_TYPE_CATALOG`** | Regions **23–24** | GQL **graph type name ↔ `GraphTypeId`**; intern at `CREATE GRAPH TYPE` ([ADR 0014](adr/0014-graph-type-id-catalog-on-router.md)). |
| **`GraphTypeId`** | `ROUTER_GRAPH_TYPE_CATALOG` | Router-issued `GraphTypeId(u32)` for named graph types (`CREATE GRAPH TYPE gt { … }`); **`0` reserved**. Keys `type_map` and `TYPED` binding refs. |
| **Graph schema binding** | `GraphCatalog.binding_map` | Row at federation **`GraphId`**: inline graph type definition or `TYPED` ref to **`GraphTypeId`**. Open graph (`ANY`) = no row. |
| **`object_name_key`** | DDL ingress | Joins qualified GQL object name segments with `.` for lookup at the GQL surface (before intern). |

## Graph storage

| Term | Meaning |
|------|---------|
| **LARA** | Localized Adjacency Relocation Array; CSR-based adjacency in `ic-stable-lara`. |
| **Forward-to-remote index** | — | **Removed**; was `REMOTE_FORWARD_IN` |
| **Authoritative shard** | Shard holding the vertex’s primary CSR record (graph shard for that `shard_id`). |

## Query execution

| Term | Meaning |
|------|---------|
| **Physical plan** | `PhysicalPlan` — ordered `PlanOp` list from `gleaph-gql-planner`. |
| **Plan row** | `PlanRow` — one result row: dense `slots` + optional `spill` map, keyed by `BindingLayout`. |
| **Plan binding** | `PlanBinding` — vertex, edge, path, value, or `RemoteVertex(GlobalVertexId)`. |
| **Materialize** | Convert internal bindings (e.g. `Path`) to GQL `Value` records for the client. |
| **Seed binding** | Router-supplied local vertex ids that skip the first `IndexScan` on a shard. |
| **Index anchor** | Equality predicate on an indexed property used to route a plan to one or more shards. |

## GQL and IC

| Term | Meaning |
|------|---------|
| **Prepared query** | Pre-registered GQL program; executors may run it without ad-hoc parse rights. |
| **Program modification flags** | `gleaph_gql::program_modification` — static read vs write classification. |
| **IC extensions** | `IC.PRINCIPAL`, `IC.MSG_CALLER()` — Gleaph-specific GQL surface (not in ISO core). |
| **USE GRAPH** | GQL focused graph scope; planner emits `PlanOp::UseGraph`; router defocuses top-level `USE`, resolves name → `GraphId`, replans with target stats, and dispatches to that graph’s shards ([ADR 0011](adr/0011-gql-graph-resolution-and-catalog-scoping.md)). |
| **Effective graph** | Session current graph after applying `session_activity`, or HOME / sole-graph default when unset; keys shard dispatch and index catalog for plain queries without `USE`. |
| **HOME graph** | Router default when session current is unset: exactly one visible `GraphRegistryEntry` with `is_home: true`, else sole visible graph (standalone), else error when ambiguous ([ADR 0011](adr/0011-gql-graph-resolution-and-catalog-scoping.md)). |
| **HOME_GRAPH** | GQL special reference resolved to the caller’s HOME graph (same rules as router HOME resolution). |
| **SessionGraphSeed** | Optional ingress input to `gleaph_gql::validate_with_seed`: effective and HOME catalog names so validator `graph_scope` matches router `resolve_graph_context`. |
| **GraphId** | Router-issued `GraphId(u32)` via `BidirectionalCatalog`; stable keys for registry, shards, index rows, idempotency, **graph schema bindings** — **not** embedded graph name strings (ADR 0011). |
| **GraphTypeId** | Router-issued `GraphTypeId(u32)` via `ROUTER_GRAPH_TYPE_CATALOG`; stable keys for GQL graph **type** definitions and `TYPED` refs — separate id space from `GraphId` (ADR 0014). |
| **is_home** | `GraphRegistryEntry.is_home`: marks the HOME graph when multiple graphs are visible; at most one may be registered. |
| **IndexNameId** | Router-issued `IndexNameId(u16)` via graph-scoped `BidirectionalCatalog`; stable key component for `ROUTER_NAMED_INDEXES` — **not** index name strings (ADR 0011). |

## Canisters

| Canister | Role |
|----------|------|
| **Router** | Auth, planning entry, shard registry, catalog SSOT, multi-shard dispatch. |
| **Graph shard** | LARA storage, plan execution (local CSR only today). |
| **Graph index** | Property equality postings tagged with `(shard_id, local_vertex_id)`. |
