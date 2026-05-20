# Physical plan format

## Purpose

Define the **contract** between `gleaph-gql-planner` and `gleaph-graph` executor: what a `PhysicalPlan` contains and what the executor may assume.

## Non-goals

- Wire encoding byte layout (see `gql-planner/src/wire/`).
- Cost model formulas (`crates/gql-planner/src/cost.rs`).

## Core types

| Type | Crate | Role |
|------|-------|------|
| `PhysicalPlan` | `gql-planner` | `ops: Vec<PlanOp>` + metadata |
| `PlanOp` | `gql-planner` | One execution step |
| `PlanAnnotations` | `gql-planner` | Hints (CSE, live vars, etc.) |
| `BindingLayout` | `gql-planner` | Dense column order for variables |
| `ExecutePlanArgs` | `graph-kernel` | Router → graph transport |

Planner builds plans; graph decodes plan blobs and runs `execute_ops` (`crates/graph/src/plan/query/executor.rs`).

## Binding layout

Indexed plans attach `BindingLayout` to `PlanRow`:

- Variables map to slot indices for fast fork/merge.
- Spill map holds names outside the layout (rare paths).

Executor optimizations (`PlanRow::try_merge_skip_one`, `QueryArena` slot pool) assume compatible layouts between join inputs.

## PlanOp families

See [execution/operators.md](../execution/operators.md) for the full list. Grouped by role:

| Family | Examples | Executor module |
|--------|----------|-----------------|
| Scan | `NodeScan`, `IndexScan`, `EdgeIndexScan`, `ConditionalIndexScan` | `execute_*scan*` |
| Filter | `PropertyFilter`, `Filter` | expr evaluator |
| Traverse | `Expand`, `ExpandFilter`, `ShortestPath` | `execute_expand`, path search |
| Join | `HashJoin`, `CartesianProduct`, `WorstCaseOptimalJoin` | join helpers |
| GQL control | `Let`, `For`, `OptionalMatch`, `UseGraph` | control flow |
| Output | `Project`, `Sort`, `Limit`, `TopK`, `Aggregate`, `Materialize` | projection / agg |
| DML | `InsertVertex`, `SetProperties`, `DeleteVertex`, … | mutation executor |

## Router seed contract

When router supplies `seed_bindings_blob`:

- Graph **skips** the first anchor `IndexScan` for that variable.
- Binds listed **local** `VertexId`s on the target shard only.

Plan must be written so remaining ops are valid given seeded rows (planner + router `SeedProbe` agree on property/value).

## Federation interaction

- Planner emits normal `IndexScan` / `Expand`; no `ShardId` in `PlanOp`.
- Router may run the **same** plan on multiple shards.
- Executor introduces `RemoteVertex` at runtime ([federation/query-semantics.md](../federation/query-semantics.md)).

## Versioning

**Current practice:** Plan blobs are encoded with `gql-planner` wire format; router and graph must deploy compatible planner versions.

**Future:** Document explicit plan version field if wire breaking changes become frequent.

## Invariants (executor expectations)

1. Variables referenced in an op are bound by a prior scan, expand, join, or `Let`.
2. `PropertyFilter.stage` matches planner’s pipeline staging.
3. DML ops appear only on update path; router verifies `has_dml()` vs program classification.
4. `Expand.emit_edge_binding` controls whether edge variable is populated.

Violations → `PlanQueryError` at execution time.

## Related documents

- [layers.md](layers.md)
- [execution/pipeline.md](../execution/pipeline.md)
- [execution/operators.md](../execution/operators.md)
