# Plan operators

## Purpose

Catalog `PlanOp` variants and note **executor support** and **federation relevance**. Detailed planner semantics: `crates/gql-planner/src/plan.rs`.

## Legend

| Status | Meaning |
|--------|---------|
| **Exec** | Implemented in graph executor |
| **Partial** | Some patterns unsupported |
| **DML** | Update path only |

## Scan

| PlanOp | Status | Notes |
|--------|--------|-------|
| `NodeScan` | Exec | Label/property projection |
| `IndexScan` | Exec | May be skipped via router seed |
| `EdgeIndexScan` | Exec | Often paired with `EdgeBindEndpoints` |
| `EdgeBindEndpoints` | Exec | Binds near/far from edge record |
| `ConditionalIndexScan` | Exec | Param-dependent index vs fallback scan |
| `IndexIntersection` | Partial | Planner may emit; check executor |

## Filter

| PlanOp | Status | Notes |
|--------|--------|-------|
| `PropertyFilter` | Exec | Stage-aware predicates |
| `Filter` | Exec | General `WHERE` residue |

## Traverse

| PlanOp | Status | Federation |
|--------|--------|------------|
| `Expand` | Partial | Federated path via `RemoteVertex`; local placement limits |
| `ExpandFilter` | Partial | EV-fused expand + dst filter |
| `ShortestPath` | Partial | Modes like `ShortestK` may be unsupported |
| `WorstCaseOptimalJoin` | Partial | Cyclic patterns |

## Join

| PlanOp | Status | Notes |
|--------|--------|-------|
| `HashJoin` | Exec | Vertex-key fast path; arena recycle |
| `CartesianProduct` | Exec | |
| `OptionalMatch` | Exec | |

## GQL control

| PlanOp | Status | Notes |
|--------|--------|-------|
| `Let` | Exec | |
| `For` | Exec | `WITH OFFSET` not yet supported in planner/executor |
| `CallProcedure` / `InlineProcedureCall` | Partial | |
| `UseGraph` | Partial | Remote graph; distinct from shard federation |

## Output

| PlanOp | Status | Notes |
|--------|--------|-------|
| `Project` | Exec | DISTINCT, column list |
| `Sort` / `Limit` / `TopK` | Exec | TopK fusion in planner |
| `Aggregate` | Exec | |
| `Materialize` | Exec | |
| `SetOperation` | Exec | `UNION` / `EXCEPT` / `INTERSECT` (ALL and DISTINCT); `OTHERWISE` not implemented |

## DML (update path)

| PlanOp | Status |
|--------|--------|
| `InsertVertex` | DML |
| `InsertEdge` | DML |
| `SetProperties` / `RemoveProperties` | DML |
| `DeleteVertex` / `DetachDeleteVertex` | DML |

DML triggers index posting maintenance when index client configured.

## Maintenance

When adding a `PlanOp` variant:

1. Implement in `gql-planner` + wire encode/decode.
2. Implement `execute_*` in graph executor.
3. Update this table and [plan-format.md](../gql/plan-format.md).
4. Add tests + canbench if hot path.

## Related documents

- [pipeline.md](pipeline.md)
- [gql/plan-format.md](../gql/plan-format.md)
