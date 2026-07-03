# Gleaph design documents

Architecture and semantics for **Gleaph** (IC graph database) and the **GQL** stack. These docs are the human-facing counterpart to module comments and agent notes (`AGENT.md`, `crates/gql-planner/CLAUDE.md`).

> **Note:** `frontend/design/` holds UI/CSS tokens only. This directory is for backend, query, and federation design.

## Audience

| Reader | Start here |
|--------|------------|
| New contributor | [architecture/overview.md](architecture/overview.md) → [glossary.md](glossary.md) |
| Query / planner work | [gql/layers.md](gql/layers.md) → [gql/extension-syntax.md](gql/extension-syntax.md) → [gql/plan-format.md](gql/plan-format.md) → [execution/pipeline.md](execution/pipeline.md) |
| Federation / sharding | [sharding/README.md](sharding/README.md) → [sharding/standalone-mode.md](sharding/standalone-mode.md) → [sharding/federation-target.md](sharding/federation-target.md) |
| Security / product | [security/rbac-and-prepared.md](security/rbac-and-prepared.md) |

## Document map

| Path | Status | Summary |
|------|--------|---------|
| [glossary.md](glossary.md) | draft | Shared terminology |
| [architecture/overview.md](architecture/overview.md) | draft | Canisters, request flow, crate boundaries |
| [architecture/refactoring-roadmap.md](architecture/refactoring-roadmap.md) | planned | Phased technical-debt refactor plan, data-layer ownership, and stable-memory policy |
| [architecture/acid-roadmap.md](architecture/acid-roadmap.md) | planned | Shard-local atomicity, cross-canister convergence, mutation visibility, and selective strong-consistency roadmap |
| [implementation-gaps.md](implementation-gaps.md) | active | Cross-cutting defects and missing capabilities discovered during implementation |
| [sharding/README.md](sharding/README.md) | planned | Standalone vs federation target entry |
| [sharding/standalone-mode.md](sharding/standalone-mode.md) | planned | Default single-shard mode, defer list, module layout |
| [sharding/federation-target.md](sharding/federation-target.md) | planned | Router-centric index slice, dispatch, merge |
| [index/lookup-intersection.md](index/lookup-intersection.md) | planned | `lookup_intersection` on graph-index |
| [federation/model.md](federation/model.md) | draft | Identifiers, placement, remote edges |
| [federation/operations.md](federation/operations.md) | draft | Lifecycle: register, place, expand |
| [federation/query-semantics.md](federation/query-semantics.md) | draft | Executor behavior and limits |
| [gql/layers.md](gql/layers.md) | draft | Parser → planner → executor split |
| [gql/extension-syntax.md](gql/extension-syntax.md) | planned / partially implemented | Gleaph GQL dialect surface: IC values, INLINE, SEARCH, vector indexes, and operational procedures |
| [gql/plan-format.md](gql/plan-format.md) | draft | `PhysicalPlan` contract |
| [execution/pipeline.md](execution/pipeline.md) | draft | `PlanRow`, arena, materialize |
| [execution/operators.md](execution/operators.md) | draft | `PlanOp` catalog (planner vs executor) |
| [demo/knowledge-map.md](demo/knowledge-map.md) | partially implemented | PocketIC-backed visual knowledge-map demo |
| [demo/social-graph-rag.md](demo/social-graph-rag.md) | planned | Relational, graph, vector, and GraphRAG social-demo contract |
| [storage/lara.md](storage/lara.md) | accepted | **LARA consensus:** four contracts, DGAP vs LARA, FreeSpanStore |
| [storage/lara-and-facade.md](storage/lara-and-facade.md) | draft | LARA vs graph stable stores |
| [storage/lara-dgap-contract.md](storage/lara-dgap-contract.md) | partially implemented | DGAP mapping detail and labeled gaps |
| [storage/lara-labeled-migration-tests.md](storage/lara-labeled-migration-tests.md) | accepted | Labeled migration Phases A–E test gates |
| [storage/labeled-edge-payloads.md](storage/labeled-edge-payloads.md) | implemented | Edge row vs payload slab layout |
| [storage/payload-first-traversal.md](storage/payload-first-traversal.md) | partially implemented | Two-phase payload / edge read API (M1–M6 LARA sparse/hybrid phase 1) |
| [storage/bulk-ingest-finalize.md](storage/bulk-ingest-finalize.md) | implemented | GraphStore + canister + GQL `CALL` + router auto-finalize + WSP canbench baseline |
| [security/rbac-and-prepared.md](security/rbac-and-prepared.md) | draft | Roles and prepared queries |
| [index/property-index.md](index/property-index.md) | draft | graph-index and router seed routing |
| [index/vector-index.md](index/vector-index.md) | planned | Graph-owned vertex embeddings and derived vector index candidate generation |
| [index/capacity-planning.md](index/capacity-planning.md) | planned | 500 GiB limits, posting size formulas, split thresholds |
| [adr/README.md](adr/README.md) | draft | How we record decisions |
| [adr/0001-labeled-segment-slide.md](adr/0001-labeled-segment-slide.md) | accepted | Labeled physical layer → PMA leaf segment slide |
| [adr/0029-shard-local-atomicity-and-cross-canister-consistency.md](adr/0029-shard-local-atomicity-and-cross-canister-consistency.md) | accepted | Shard-local atomic writes plus durable asynchronous projection and federated saga consistency |
| [adr/0031-vertex-embedding-store-and-derived-vector-index.md](adr/0031-vertex-embedding-store-and-derived-vector-index.md) | accepted (planned) | Vertex embedding store and derived vector index canister |
| [adr/0032-vector-index-slab-page-store.md](adr/0032-vector-index-slab-page-store.md) | accepted (planned) | Vector-index physical page store split into metadata directory and raw stable slab |
| [adr/0034-gleaph-gql-extension-syntax.md](adr/0034-gleaph-gql-extension-syntax.md) | accepted (syntax design) | Dedicated Gleaph GQL extension syntax contract |

## Conventions

Each document should include where possible:

1. **Purpose / non-goals**
2. **Invariants** (things that must stay true)
3. **Source of truth** (crate and module paths)
4. **Limits** (explicit `UnsupportedOp`, missing orchestration)
5. **Related ADRs** (when they exist)

## Keeping docs honest

When behavior changes, update the design doc in the same PR when the change affects invariants or public semantics. Stale docs are worse than none—prefer marking sections **Implemented** / **Planned** / **Not implemented**.
