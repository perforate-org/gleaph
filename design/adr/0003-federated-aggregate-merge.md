# 0003. Federated aggregate merge on router

Date: 2026-06-08
Status: accepted

## Context

ADR 0002 added federated row-batch union merge (`rows_blob` concatenation) for independent
shard-local query fragments. Multi-shard queries with `PlanOp::Aggregate` return partial
aggregate rows per shard; concatenating them is incorrect — the router must merge by GROUP BY
key and re-apply commutative aggregate functions.

`PLAN_WIRE_VERSION` remains **1**.

## Decision

1. Add `router/federation/aggregate_merge.rs` with:
   - `FederatedMergeMode` (`UnionRows` | `Aggregate(spec)`)
   - Plan-driven descriptor extraction from `PlanOp::Aggregate` + following `Project`
   - `merge_aggregate_blobs` — group by key columns; merge COUNT/COUNT(*)/SUM/MIN/MAX
2. Extend `merge_execute_plan_result` to branch on merge mode:
   - **UnionRows** — existing concat + sum `row_count`
   - **Aggregate** — merge `rows_blob` by key; set `row_count` to merged row count
3. `gql.rs` derives merge mode from physical plans before the shard dispatch loop.

### v1 scope (generic router merge path)

- Mergeable: `CountStar`, `Count`, `Sum`, `Min`, `Max` without `DISTINCT`, filter, or order-by
- Non-mergeable aggregates (e.g. `AVG`, `COLLECT`, `DISTINCT COUNT`) fall back to union merge
- On the **generic path**, `HAVING` applied per shard before merge may filter groups that would
  pass globally — see planned mitigations below
- Sort/Limit after merge on router, client row API, and cross-shard JOIN merge remain future work

### Planned: index pushdown fast path

Some federated aggregate queries do not need per-shard `PlanOp::Aggregate` + router merge.
When `GROUP BY` is an **indexed vertex property**, the aggregate is **`COUNT(*)`**, and
`HAVING` predicates the same count, graph-index postings already encode the answer:

- Posting key order: `(property_id, encoded_value, shard_id, vertex_id)`
- `COUNT(*)` per group key = number of postings sharing that `encoded_value` (global across shards)
- `HAVING COUNT(*) > N` = keep value buckets whose posting count exceeds `N`

Example:

```gql
RETURN n.country, COUNT(*)
GROUP BY n.country
HAVING COUNT(*) > 5
```

On this shape, `lookup_equal(country, "US")` (or a single-value range) is already a global
count for `"US"`. A full `GROUP BY` is a **one-pass scan** of the property bucket:

```text
[property_min(property_id) .. property_end_exclusive(property_id))
```

Keys are sorted by `encoded_value`, so the index can walk contiguous runs and emit
`(value, count)` without materializing `PostingHit` rows. Implementation note: postings live in
`ic_stable_structures::BTreeSet<PostingKey>`; use `range(lo..hi)` (not `BTreeMap::keys_range`).

**Planned API (graph-index):** e.g. `count_postings_by_value(property_id, min_count)` returning
only groups with `count >= min_count` (instruction-bounded; do not return full hit lists).

**Planned routing (Router / planner):** detect the fast-path plan shape and call the index API
instead of dispatching aggregate execution to graph shards. Label / traversal constraints must
still be satisfied (e.g. via seeds, `lookup_intersection`, or an explicit non-fast-path fallback).

**Fast-path eligibility (initial):**

| Requirement | Reason |
|-------------|--------|
| `GROUP BY` on one indexed property | Posting bucket per encoded value |
| `COUNT(*)` only | Posting cardinality |
| `HAVING` on that same `COUNT(*)` (optional) | Filter on bucket size |
| No `DISTINCT`, aggregate `FILTER`, or ordered `COLLECT` | Not posting-count semantics |
| No `SUM` / `AVG` / `MIN` / `MAX` on other expressions | Needs vertex values, not postings |

**HAVING on generic path:** Router merge post-filter remains the fallback for mergeable aggregates
on plans that do not match the index fast path.

## Consequences

- Federated `RETURN COUNT(*)` / `GROUP BY` queries produce correct merged `rows_blob` internally
  on the generic router-merge path
- Public `gql_query` still returns row count only; merged aggregate values live in `rows_blob`
- Router must inspect physical plan ops to choose merge policy (generic merge vs future index
  fast path)
- Index fast path avoids shard-local `HAVING` bugs and reduces graph execution for eligible plans

## Alternatives considered

- **Index pushdown for eligible `COUNT(*)` / `GROUP BY` / `HAVING`** — planned fast path (above);
  not implemented in v1 generic merge
- **Router merge post-filter for `HAVING`** — generic fallback for non-fast-path plans
- **Always union** — incorrect semantics for aggregates
- **Ship merged rows in `gql_query` now** — larger API change; deferred with ADR 0002
