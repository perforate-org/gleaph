# Execution pipeline

## Purpose

Describe how `gleaph-graph` runs a physical plan: row representation, operator dispatch, memory pooling, and materialization.

## Non-goals

- Mutation executor internals (`crates/graph/src/plan/mutation/`).
- GQL client result serialization (router/SDK).

## Entry points

| API | Path |
|-----|------|
| `execute_plan_query_bindings` | `crates/graph/src/plan/query/executor.rs` |
| Canister | `execute_plan_query` / `execute_plan_update` handlers |

Flow:

```mermaid
flowchart LR
    A[ExecutePlanArgs] --> B[Decode plan + params]
    B --> C[Seed rows<br/>optional]
    B --> G[Resolved search relation<br/>optional]
    C --> D[execute_ops]
    G --> D
    D --> E[Plan rows]
    E --> F[Materialize]
```

`ExecutePlanArgs.resolved_search_blob` carries the Router-resolved non-leading `SEARCH` relation for the target shard. `QueryArena::reset()` at query start; thread-local pool reused across operators within one query.

## PlanRow

**Module:** `crates/graph/src/plan/query/row.rs`

| Field | Role |
|-------|------|
| `layout: Option<Rc<BindingLayout>>` | Dense column schema |
| `slots: Vec<Option<PlanBinding>>` | Column values |
| `spill: BTreeMap<String, PlanBinding>` | Overflow bindings |

**Operations:**

- `fork` / `fork_with_arena` — copy row with updates (expand, branch)
- `try_merge` / `try_merge_skip_one` — hash join combine (skip join keys)
- `insert` — in-place binding update

**Arena:** `QueryArena` (`arena.rs`) recycles slot `Vec` capacity after hash join; `fork_with_arena` uses pool only when buffers are available. Merge stays on slot clone for probe hot path.

## Operator dispatch

`execute_ops` matches `PlanOp` variants and calls specialized functions (`execute_expand`, `execute_hash_join`, `execute_shortest_path`, …).

Optimizations layered in executor (not only planner):

- CSR fast paths for expand
- Streaming expand when later ops preserve cardinality
- Indexed hash join merge when layouts match
- Path-only shortest-path rows with shared `PathBinding` arc

### `PlanOp::Search`

The Graph executor supports one top-level non-leading `PlanOp::Search` per plan when the Router provides a `resolved_search_blob`:

- Decode the blob into `ResolvedSearchWire` at plan-entry time and build an invocation-local lookup from local vertex id to the user-visible scalar value.
- Validate that the wire binding and alias match the plan, that all values are finite, that there are no duplicate vertex ids, and that the hit count does not exceed `MAX_VECTOR_SEARCH_TOP_K`.
- Execute as an inner join/filter against the current row set: rows whose bound vertex variable is present in the lookup survive, the scalar alias is bound to the lookup value, and row multiplicity is preserved.
- If the bound vertex is absent from a row the row is dropped (inner-join semantics).
- A `PlanOp::Search` without a decoded `resolved_search_blob` fails closed because the Router has not lowered it.

For a leading `NodeScan + Search` with a `WHERE` equality predicate, the Router does not
forward a vector request when the Property Index candidate set is empty. Instead it dispatches the
stripped tail plan with an empty `SeedBindingsWire` to every live shard, so a global aggregate over
zero seed rows still produces one `count = 0` row. When the candidate set is non-empty, the vector
canister receives a bounded allowlist and returns exact top-k hits; the normal leading-search
hit-shard-only dispatch then applies.

For a non-leading `PlanOp::Search` with a `WHERE` equality predicate, the Router requires exactly one
positive simple label proof for the searched binding from the top-level prefix, reuses the same
bounded Property Index candidate resolution, and skips the vector canister when the candidate set is
empty. It dispatches the full plan with an explicit empty `ResolvedSearchWire` to every live shard,
so the Graph executor still runs the prefix and any global aggregate returns one `count = 0` row.
When the candidate set is non-empty, the vector canister ranks exactly within the allowlist and the
Router partitions hits into per-shard resolved relations as for unfiltered non-leading search.

## Materialization

Internal bindings may stay lazy until output:

| Binding | Materialized as |
|---------|-----------------|
| `Vertex` | Record with properties (projection-aware) |
| `Edge` | Edge record |
| `Path` | Walk `PathBinding` states → vertex/edge sequence |
| `RemoteVertex` | Logical id reference (limited property access) |
| `Value` | Already materialized |

`materialize_plan_rows` / `PlanQueryResult` convert rows for GQL clients.

## Error model

`PlanQueryError` — unsupported ops, federated call failures, invalid expressions.

Federation-specific failures: see [federation/query-semantics.md](../federation/query-semantics.md).

## Benchmarks

Hot scopes instrumented under `feature = "canbench"` (e.g. `hash_join_vertex_probe_merge`, `expand_*`). See `crates/graph/src/bench/mod.rs` and `design/` benchmarking doc when added.

## Related documents

- [operators.md](operators.md)
- [gql/plan-format.md](../gql/plan-format.md)
- [federation/query-semantics.md](../federation/query-semantics.md)
