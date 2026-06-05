# Glossary

Terms used across Gleaph design documents. Canonical types live in **`gleaph-graph-kernel`** unless noted.

## Identity and placement

| Term | Type / location | Meaning |
|------|-----------------|--------|
| **Logical vertex id** | `LogicalVertexId` (`u64`) | Global, stable vertex identity assigned by the router. |
| **Local vertex id** | `LocalVertexId` / LARA `VertexId` | Dense id within one graph shard’s CSR store. |
| **Shard id** | `ShardId` (`u32`) | Partition of a logical graph. |
| **Physical placement** | `PhysicalVertexLocation` | `(shard_id, local_vertex_id)` where vertex data currently lives. |
| **Vertex placement** | `VertexPlacement` | Router-owned state: `Active(loc)`. |
| **Physical placement key** | `PhysicalPlacementKey` | 8-byte stable key for reverse lookup `(shard → local → logical)`. |
| **Standalone mode** | `standalone_logical_vertex_id` | Single-shard dev: local id equals logical id. |

## Graph storage

| Term | Meaning |
|------|---------|
| **LARA** | Localized Adjacency Relocation Array; CSR-based adjacency in `ic-stable-lara`. |
| **Remote ref** | `RemoteRefId` — compact handle for an edge whose far endpoint is on another shard. |
| **Forward-to-remote index** | `REMOTE_FORWARD_IN` postings: incoming edges on this shard that target a remote logical vertex. |
| **Authoritative shard** | Shard holding the vertex’s primary record (`VertexPlacement::Active` location). |

## Query execution

| Term | Meaning |
|------|---------|
| **Physical plan** | `PhysicalPlan` — ordered `PlanOp` list from `gleaph-gql-planner`. |
| **Plan row** | `PlanRow` — one result row: dense `slots` + optional `spill` map, keyed by `BindingLayout`. |
| **Plan binding** | `PlanBinding` — vertex, edge, path, value, or `RemoteVertex(logical_id)`. |
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
| **Router** | Auth, planning entry, shard registry, placement authority, multi-shard dispatch. |
| **Graph shard** | LARA storage, plan execution, federated expand. |
| **Graph index** | Property equality postings tagged with `shard_id`. |
