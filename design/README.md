# Gleaph design documents

Architecture and semantics for **Gleaph** (IC graph database) and the **GQL** stack. These docs are the human-facing counterpart to module comments and agent notes (`AGENT.md`, `crates/gql-planner/CLAUDE.md`).

> **Note:** `frontend/design/` holds UI/CSS tokens only. This directory is for backend, query, and federation design.

## Audience

| Reader | Start here |
|--------|------------|
| New contributor | [architecture/overview.md](architecture/overview.md) → [glossary.md](glossary.md) |
| Query / planner work | [gql/layers.md](gql/layers.md) → [gql/plan-format.md](gql/plan-format.md) → [execution/pipeline.md](execution/pipeline.md) |
| Federation / sharding | [federation/model.md](federation/model.md) → [federation/query-semantics.md](federation/query-semantics.md) |
| Security / product | [security/rbac-and-prepared.md](security/rbac-and-prepared.md) |

## Document map

| Path | Status | Summary |
|------|--------|---------|
| [glossary.md](glossary.md) | draft | Shared terminology |
| [architecture/overview.md](architecture/overview.md) | draft | Canisters, request flow, crate boundaries |
| [federation/model.md](federation/model.md) | draft | Identifiers, placement, remote edges |
| [federation/operations.md](federation/operations.md) | draft | Lifecycle: register, place, expand |
| [federation/query-semantics.md](federation/query-semantics.md) | draft | Executor behavior and limits |
| [gql/layers.md](gql/layers.md) | draft | Parser → planner → executor split |
| [gql/plan-format.md](gql/plan-format.md) | draft | `PhysicalPlan` contract |
| [execution/pipeline.md](execution/pipeline.md) | draft | `PlanRow`, arena, materialize |
| [execution/operators.md](execution/operators.md) | draft | `PlanOp` catalog (planner vs executor) |
| [storage/lara-and-facade.md](storage/lara-and-facade.md) | draft | LARA vs graph stable stores |
| [security/rbac-and-prepared.md](security/rbac-and-prepared.md) | draft | Roles and prepared queries |
| [index/property-index.md](index/property-index.md) | draft | graph-index and router seed routing |
| [adr/README.md](adr/README.md) | draft | How we record decisions |

## Conventions

Each document should include where possible:

1. **Purpose / non-goals**
2. **Invariants** (things that must stay true)
3. **Source of truth** (crate and module paths)
4. **Limits** (explicit `UnsupportedOp`, missing orchestration)
5. **Related ADRs** (when they exist)

## Keeping docs honest

When behavior changes, update the design doc in the same PR when the change affects invariants or public semantics. Stale docs are worse than none—prefer marking sections **Implemented** / **Planned** / **Not implemented**.
