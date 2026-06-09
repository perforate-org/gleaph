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
| `IndexIntersection` | Partial | Intersect on graph-index; router seeds + graph skip leading op **Implemented**; graph direct index when unseeded remains transition path |

## Filter

| PlanOp | Status | Notes |
|--------|--------|-------|
| `PropertyFilter` | Exec | Stage-aware predicates |
| `Filter` | Exec | General `WHERE` residue |

## Traverse

| PlanOp | Status | Federation |
|--------|--------|------------|
| `Expand` | Partial | `{min,max}` hop-count var-length with per-hop index/payload/vector fusion; `label_expr` (`-/A\|B/->`, `-[e:A\|B]->`, `%`, `!`); named unions fuse per label, wildcard/negation fall back to catalog payload-profile labels; quantified subpath `((u)-[e:L]->(v)){m,n}` lowers to one var-length expand with `near_group_var` / `far_group_var`; var-length `e` → **edge group** (or projected edge-property list when RETURN uses only `e.prop`), `u` / `v` → **vertex groups**; `MATCH p = …->{m,n}…` binds **path** on var_len `Expand`; `{edge}__hop_aux` binds inline payload bytes (see [group-variables.md](./group-variables.md)); leading-edge index fusion uses `EdgeIndexScan` + `EdgeBindEndpoints` instead of `Expand` |
| `ExpandFilter` | Partial | Same var-length, `label_expr`, and group-variable semantics as `Expand` |
| `ShortestPath` | Partial | `ShortestK` / `ShortestKGroup` (hop-count and `GLEAPH.COST`); `label_expr` (`-/A\|B/->`, including weighted); `path_var` binds singleton `Path` or grouped `PathGroup` on `SHORTEST k GROUP` |
| `WorstCaseOptimalJoin` | Partial | Simple directed cycles (single-hop edges); `{edge}__hop_aux` per [`WcojEdge`]; `var_len` hops not yet supported |

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
| `For` | Exec | `WITH ORDINALITY` (1-based) and `WITH OFFSET` (0-based) |
| `CallProcedure` / `InlineProcedureCall` | Partial | Inline `CALL { ... }` exec; named `CallProcedure` not implemented |
| `UseGraph` | Partial | Remote graph; distinct from shard federation |

## Output

| PlanOp | Status | Notes |
|--------|--------|-------|
| `Project` | Exec | DISTINCT, column list |
| `Sort` / `Limit` / `TopK` | Exec | TopK fusion in planner |
| `Aggregate` | Exec | Implicit `RETURN SUM(...)` etc.; horizontal `SUM(GLEAPH.WEIGHT(e))` over var-length **edge groups** per input row (see [group-variables.md](./group-variables.md)) |
| `Materialize` | Exec | |
| `SetOperation` | Exec | `UNION` / `EXCEPT` / `INTERSECT` (ALL and DISTINCT), `OTHERWISE` fallback |

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

- [group-variables.md](group-variables.md)
- [pipeline.md](pipeline.md)
- [gql/plan-format.md](../gql/plan-format.md)
