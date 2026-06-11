# Glossary

Last updated: 2026-06-11  
Anchor timestamp: 2026-06-11 16:02:17 UTC +0000

Terms used across Gleaph design documents. Canonical types live in **`gleaph-graph-kernel`** unless noted.

See [adr/0005-vertex-identity.md](adr/0005-vertex-identity.md) and [adr/0006-pre-federation-foundation.md](adr/0006-pre-federation-foundation.md) for the current identity and catalog policies.

## Identity and placement

| Term | Type / location | Meaning |
|------|-----------------|--------|
| **Shard id** | `ShardId` (`u32` newtype) | Partition of a logical graph. Standalone sole shard is **`ShardId(0)`**. |
| **Local vertex id** | `LocalVertexId` / LARA `VertexId` | Dense id within one graph shard’s CSR store. |
| **Global vertex id** | `GlobalVertexId` | Canonical global key: `{ shard_id, local_vertex_id }` (8 bytes LE). Used by router placement, index routing, federation expand. |
| **Global edge id** | `GlobalEdgeId` | Query-time physical edge handle: `{ shard_id, owner_local, edge_slot_index }` (12 bytes). Not stable across compaction. |
| **Encoded vertex id** | `EncodedVertexId` (`[u8; 8]`) | Opaque client wire id for vertices (`ELEMENT_ID`, path elements). Bijective encoding of `GlobalVertexId` under a per-graph `ElementIdEncodingKey`. |
| **Encoded edge id** | `EncodedEdgeId` (`[u8; 12]`) | Opaque client wire id for edges in paths and `ELEMENT_ID`. |
| **Physical vertex location** | `PhysicalVertexLocation` | `(shard_id, local_vertex_id)` — active home of vertex data; same tuple as `GlobalVertexId`. |
| **Vertex placement** | `VertexPlacement` | Router-owned state: `Active(PhysicalVertexLocation)` keyed by `GlobalVertexId`. |
| **Physical placement key** | `PhysicalPlacementKey` | Type alias for `GlobalVertexId` (deprecated name). |
| **Remote vertex id** | `RemoteVertexId` | Shard-local 30-bit handle inside `VertexRef` for remote CSR endpoints — kernel type only; **no graph stable yet**. |
| **Standalone mode** | [sharding/standalone-mode.md](sharding/standalone-mode.md) | `n = 1` shard: `GlobalVertexId(0, local)`; router catalogs; encoded element ids on the wire. |

**Removed terms:** `LogicalVertexId`, `standalone_logical_vertex_id`, router logical-id allocation, graph `VERTEX_LOGICAL_IDS`, `RemoteRefId`.

## Catalogs (federation)

| Term | Owner | Meaning |
|------|-------|---------|
| **Property id** | Router `ROUTER_PROPERTY_CATALOG` | Name ↔ `PropertyId` SSOT for federated graphs. Graph stores values only. |
| **Vertex / edge label id** | Router label catalogs | Name ↔ label id SSOT; graph stores label sets by id. |
| **Resolved property table** | Plan wire (`ResolvedPropertyTable`) | Router-supplied name→id map attached to `ExecutePlanArgs` for graph DML/scan. |

## Graph storage

| Term | Meaning |
|------|---------|
| **LARA** | Localized Adjacency Relocation Array; CSR-based adjacency in `ic-stable-lara`. |
| **Forward-to-remote index** | — | **Removed**; was `REMOTE_FORWARD_IN` |
| **Authoritative shard** | Shard holding the vertex’s primary record (`VertexPlacement::Active` location). |

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
| **USE GRAPH** | GQL composite / remote graph reference; planner may push sub-plans (distinct from shard federation). |

## Canisters

| Canister | Role |
|----------|------|
| **Router** | Auth, planning entry, shard registry, placement authority, catalog SSOT, multi-shard dispatch. |
| **Graph shard** | LARA storage, plan execution (local CSR only today). |
| **Graph index** | Property equality postings tagged with `(shard_id, local_vertex_id)`. |
