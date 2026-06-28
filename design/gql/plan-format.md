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
| Vector search | `Search` | `execute_ops` resolved-search join |
| Output | `Project`, `Sort`, `Limit`, `TopK`, `Aggregate`, `Materialize` | projection / agg |
| DML | `InsertVertex`, `SetProperties`, `DeleteVertex`, … | mutation executor |

## Router seed contract

When router supplies `seed_bindings_blob`:

- Graph **skips** the first anchor `IndexScan` for that variable.
- Binds listed **local** `VertexId`s on the target shard only.

Plan must be written so remaining ops are valid given seeded rows (planner + router `SeedProbe` agree on property/value).

## Router resolved-search contract

For a non-leading `PlanOp::Search` the Router supplies `ExecutePlanArgs.resolved_search_blob`:

- The blob is a per-shard `ResolvedSearchWire` produced by the Router from a single global vector top-k call.
- The wire carries the bound vertex variable name, the scalar output alias, and the finite, de-duplicated shard-local vertex hits with their user-visible scalar values.
- Shards with no local hit receive an explicit empty wire, not an absent field.
- The Graph executor decodes the wire, validates it against the `PlanOp::Search` binding/alias, and executes the operator as an inner join: input rows whose bound vertex matches a hit survive and get the scalar alias bound.
- A `PlanOp::Search` that reaches the Graph executor without a matching resolved relation fails closed with `PlanQueryError::UnsupportedOp`.

For a leading `NodeScan + Search` prefix the Router strips the prefix and dispatches the tail plan with row-shaped `seed_bindings_blob`; the Graph executor never sees a raw `PlanOp::Search`.


### Filtered search contract

For a leading `NodeScan + Search` with an accepted `WHERE` equality predicate:

- The planner carries the filter expression in `PlanOp::Search` after structural validation: exactly
  one equality comparison between a property of the searched binding and a literal or parameter, with
  either operand order accepted. The planner does not verify label or index coverage.
- The Router resolves the searched label and filter property to router-issued ids, proves an active
  vertex equality index for the exact `(graph_id, label_id, property_id)` tuple in the named-index
  catalog, encodes the comparison value with `gleaph_gql::value_to_index_key_bytes`, and validates the
  encoded size against `MAX_INDEX_VALUE_KEY_BYTES` before calling the index.
- The Router collects at most `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` (4096) distinct vertex subjects
  from the Property Index via paginated `lookup_equal_page`. Deduplicating by `(shard_id, vertex_id)`,
  it stops as soon as a 4097th distinct subject is observed and returns an explicit error instead of
  truncating. Malformed postings are rejected.
- If the candidate set is empty, the Router skips the vector canister and dispatches the stripped plan
  with an empty `SeedBindingsWire` to every live shard, preserving the leading-search global aggregate
  contract.
- Otherwise the Router forwards one `VectorSearchRequest` with `candidate_subjects = Some(allowlist)`
  to the vector canister. `candidate_subjects = None` retains the existing unrestricted search semantics;
  `Some([])` is an empty allowlist that returns no hits.
- The vector canister validates the allowlist count, vertex-only subjects, and duplicates at its
  boundary, then ranks exactly over current live vector slots for those subjects.

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
