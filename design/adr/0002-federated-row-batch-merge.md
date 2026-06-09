# 0002. Federated row-batch merge on router

Date: 2026-06-08
Status: accepted

## Context

Multi-shard queries dispatch the same physical plan to each participating graph shard with
per-shard index seeds. Each shard returns a partial result. Federation v1 already summed
`row_count` across shards, but clients and future router APIs need merged row materialization
when fragments are independent (union semantics).

`PLAN_WIRE_VERSION` remains **1**; this ADR extends the router ↔ graph execution wire only.

## Decision

1. Extend `ExecutePlanResult` with optional `rows_blob`: Candid-encoded
   `gleaph_gql_ic::IcWirePlanQueryResult`.
2. Graph shards materialize the last read statement on the query wire path and populate
   `rows_blob` (update path leaves it `None`).
3. Router `federation/merge.rs` unions row batches by concatenating decoded rows and sums
   `row_count` via `merge_execute_plan_result`.
4. Cross-shard aggregate merge is covered by [ADR 0003](0003-federated-aggregate-merge.md).
   Cross-shard join merge and dedup policy remain future work.
5. `gql_query` and `prepared_execute_query` return `GqlQueryResult` (`row_count` + merged
   `rows_blob`; defined in `crates/graph-kernel/src/plan_exec.rs`).

## Consequences

- Query wire execution materializes rows on graph (more CPU/memory than count-only).
- Router returns merged `rows_blob` on read-path entrypoints (`gql_query`, `prepared_execute_query`).
- `IcWirePlanQueryResult` wire types live in `gleaph-gql-ic` for shared router/graph use.

## Alternatives considered

- **Count-only forever** — insufficient for RETURN projections across shards.
- **Ship full rows in `gql_query` immediately** — accepted; pre-production API.
- **Per-shard distinct wire version** — rejected; single `ExecutePlanResult` extension is enough.
