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
| `IndexScan` | Exec | May be skipped via router seed; carries `ordered_by_sort` delivery intent (ADR 0081 Slice A) — when set, rows arrive in ascending encoded-key order of that property and the executor may elide `Sort` or early-exit `TopK` (see Output) |
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
| `Expand` | Partial | `{min,max}` hop-count var-length with per-hop index/inline-property-bytes/vector fusion; `label_expr` (`-/A\|B/->`, `-[e:A\|B]->`, `%`, `!`); named unions fuse per label, wildcard/negation fall back to catalog inline-property-profile labels; quantified subpath `((u)-[e:L]->(v)){m,n}` lowers to one var-length expand with `near_group_var` / `far_group_var`; var-length `e` → **edge group** (or projected edge-property list when RETURN uses only `e.prop`), `u` / `v` → **vertex groups**; `MATCH p = …->{m,n}…` binds **path** on var_len `Expand`; `{edge}__hop_aux` binds inline property bytes (see [group-variables.md](./group-variables.md)); leading-edge index fusion uses `EdgeIndexScan` + `EdgeBindEndpoints` instead of `Expand` |
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
| `UseGraph` | Partial | Router: top-level remote `USE` defocus + dispatch **Implemented** (read, pushdown-eligible). Graph executor: single-store pass-through of sub-plan when `UseGraph` remains on wire. Nested / multi-target `USE` rejected at ingress. |

## Output

| PlanOp | Status | Notes |
|--------|--------|-------|
| `Project` | Exec | DISTINCT, column list |
| `Sort` / `Limit` / `TopK` | Exec | TopK fusion in planner; over intent-verified ascending input (ADR 0081 Slice A) TopK stops consuming at the first strictly greater key past its boundary survivor — ties continue until the group closes, `OFFSET` skips inside the delivered stream, residual filters stay valid, and any ordering violation fails safe into a full sort. Router-seeded executions skip the annotated scan op, so the gate fails closed there |
| `Aggregate` | Exec | Implicit `RETURN SUM(...)` etc.; horizontal aggregate over var-length **edge groups** per input row (see [group-variables.md](./group-variables.md)) |
| `Materialize` | Exec | |
| `SetOperation` | Exec | `UNION` / `EXCEPT` / `INTERSECT` (ALL and DISTINCT), `OTHERWISE` fallback |

### Aggregate function contracts

The `Aggregate` operator's semantics live here rather than in the syntax reference, because these are execution contracts
a caller must satisfy, not grammar.

**Percentile parameters (`PERCENTILE_CONT`, `PERCENTILE_DISC`).** The second argument is the *fraction* and must be:

| Requirement | Rejected with | Why |
| --- | --- | --- |
| Constant within a group | `aggregate: percentile fraction must be constant per group` | A percentile has one fraction per group; a row-varying one has no single meaning, so the executor fails closed instead of silently picking a row's value |
| Finite | `aggregate: percentile fraction must be finite` | `NaN`/`±Inf` are not positions in an ordered sample |
| Within `[0, 1]` (an epsilon of slack is clamped away) | `aggregate: percentile fraction must be in [0, 1]` | Outside the unit interval there is no sample position |
| Present when the group has rows | `aggregate: percentile fraction missing` | A `NULL` fraction means "no percentile was asked for", which is an error rather than `NULL` |

`PERCENTILE_CONT` interpolates between the two neighbouring values; `PERCENTILE_DISC` returns the first sample value at or
above the fraction's position, both over the group's non-`NULL` samples in `ORDER BY` order when one is given. A group whose
samples are all `NULL` yields `NULL`, not an error.

These three fail-closed arms are locked by
`percentile_fraction_must_be_finite_and_within_the_unit_interval` and
`percentile_fraction_must_be_constant_per_group` in `crates/graph/src/plan/query/aggregate.rs`, so relaxing the contract
requires changing a test, not just adding a document line.

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
