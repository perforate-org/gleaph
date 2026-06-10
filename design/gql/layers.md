# GQL stack layers

## Purpose

Fix the **boundary between portable GQL crates and Gleaph-specific execution**, so IC state, storage APIs, and canister calls do not leak into ISO-oriented code.

## Non-goals

- GQL language specification (external).
- Every optimization pass algorithm ([`crates/gql-planner/CLAUDE.md`](../../crates/gql-planner/CLAUDE.md) for implementation detail).

## Layer diagram

```mermaid
flowchart TB
    Q["Client query string"] --> GQL["gleaph-gql<br/>parse, validate, AST"]
    GQL --> PL["gleaph-gql-planner<br/>PhysicalPlan, PlanOp"]
    PL -->|plan blob| RT["gleaph-router<br/>auth, dispatch"]
    RT --> GR["gleaph-graph + graph-kernel<br/>execute_ops, PlanRow"]
    RT --> IC["gleaph-gql-ic<br/>params blob"]
```

## Crate boundaries

| Crate | Owns / exposes | Must not contain |
|-------|----------------|------------------|
| `gleaph-gql` | Parser, validator, `program_modification`, standard types | IC principals, shard ids, canister calls |
| `gleaph-gql-planner` | `build_*_plan`, `PhysicalPlan`, optimizations | GraphStore, federation, stable memory |
| `gleaph-gql-ic` | Parameter encoding for canisters | Planner logic |
| `gleaph-graph-kernel` | Wire types shared by router/graph/index | Full executor |
| `gleaph-graph` | Plan execution, storage, federation expand | GQL parse (except helpers) |
| `gleaph-router` | RBAC, planning entry, dispatch | LARA mutation |

Policy: **`AGENT.md`** — Gleaph/IC-specific behavior stays out of `gql` and `gql-planner`.

## End-to-end read path

1. **Parse** — `gleaph_gql::parser::parse`
2. **Classify** — `classify_program` → read vs write flags
3. **Authorize** — `router::rbac::authorize_adhoc_gql` (or prepared path)
4. **Plan** — `build_block_plan_with_schema(block, stats, schema)`
5. **Encode** — `encode_block_plans` → bytes for `ExecutePlanArgs`
6. **Dispatch** — router seed routing (optional multi-shard)
7. **Execute** — graph `execute_plan_query_bindings`
8. **Materialize** — bindings → GQL values for response

Prepared queries skip parse on hot path where a cached plan blob is stored.

## IC extensions

Documented in root `README.md`:

- Type `IC.PRINCIPAL`
- Function `IC.MSG_CALLER()`

Implemented in the IC bridge and evaluated in the graph executor (caller identity for filters and ACL patterns). These are **Gleaph extensions**, not portable GQL core.

### Planned: bulk ingest finalize (`CALL`)

**Status:** Planned — see [storage/bulk-ingest-finalize.md](../storage/bulk-ingest-finalize.md).

Proposed mutation-only procedures (`GLEAPH.FINALIZE_BULK_INGEST`, `GLEAPH.VERTEX_LIST`, etc.) would be parsed as standard `CALL` and executed in **gleaph-graph** mutation executor only. No new syntax in `gleaph-gql` / `gleaph-gql-planner`.

## USE GRAPH vs federation

| Feature | Meaning |
|---------|---------|
| **USE GRAPH** (planner) | Sub-query against another named graph; `analyze_remote_use_graph_pushdown` may fuse plans |
| **Federation** (router/graph) | Shards of one logical graph; `LogicalVertexId`, placement, `federated_expand` |

They interact only at product boundaries; planner tests for USE GRAPH are not shard-routing tests.

## Program modification (security input)

`gleaph_gql::program_modification::classify_program` drives:

- Whether ad-hoc execution needs Write/Manager/Admin
- Consistency check vs planner `has_dml()` in router

**Source:** `crates/gql/src/program_modification.rs`

## Related documents

- [plan-format.md](plan-format.md)
- [architecture/overview.md](../architecture/overview.md)
- [security/rbac-and-prepared.md](../security/rbac-and-prepared.md)
- [federation/query-semantics.md](../federation/query-semantics.md)
