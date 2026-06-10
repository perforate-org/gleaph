# Federation target architecture

Last updated: 2026-06-10

## Status

**Planned** — target design for multi-shard production. Current code contains partial, immature paths (executor-driven index lookup, `RemoteVertex` from index hits, scattered placement calls). Those are **not** this architecture; they are candidates for defer/removal per [standalone-mode.md](standalone-mode.md).

## Purpose

Describe the **intended** distributed query execution model: Router owns index access and per-shard dispatch; graph-index owns posting lookup and intersection; each graph shard executes locally and reaches peers only when traversal requires foreign vertices; Router merges partial results.

## Non-goals

- Index canister sharding (multiple index canisters) — future work; single global index with `shard_id`-tagged postings remains the near-term model.
- Full row shipping vs count-only aggregation policy — to be decided at merge implementation time.
- USE GRAPH remote-graph pushdown (planner feature distinct from shard federation).

## Request flow (read path)

```mermaid
sequenceDiagram
    participant U as Client
    participant R as Router
    participant I as graph-index
    participant G0 as Graph shard A
    participant G1 as Graph shard B

    U->>R: gql_query / prepared execute
    R->>R: parse, plan, encode plan blob
    R->>I: lookup_equal / lookup_intersection
    I-->>R: PostingHit[]
    R->>R: group hits by shard_id, build seed_bindings_blob per shard
    par per participating shard
        R->>G0: ExecutePlanArgs + seeds
        R->>G1: ExecutePlanArgs + seeds
    end
    Note over G0,G1: Local plan execute; skip seeded scan ops
    G0->>G1: federated_expand (only if traverse needs foreign vertex)
    G0-->>R: partial result
    G1-->>R: partial result
    R->>R: merge / aggregate
    R-->>U: result
```

## State and Execution Boundaries

| Domain | Owns / exposes | Must not own |
|--------|---------------|--------------|
| **graph-index** | Posting storage, `lookup_equal`, `lookup_intersection`, range scans | Plan execution, binding, traverse, logical placement |
| **Router** | Index queries, per-shard seed construction, dispatch, result merge | CSR storage, local traverse |
| **Graph shard** | Local `execute_plan_*`, edge/vertex stable stores, **peer** `federated_expand` | Global index lookup on query hot path, placement authority |

## Index anchor and seeds

### Single equality (`IndexScan` anchor)

1. Router resolves property names to ids (router catalog).
2. Router calls `lookup_equal(property_id, encoded_value)`.
3. For each `PostingHit`, Router routes to `hit.shard_id`'s graph canister.
4. `seeds_for_local_shard(variable, hits, shard_id)` encodes local vertex ids for that shard (`router/src/seed.rs` pattern).

### Multiple equalities (`IndexIntersection` anchor)

1. Router calls **`lookup_intersection`** once (see [lookup-intersection.md](../index/lookup-intersection.md)).
2. Index intersects on physical key `(shard_id, vertex_id)` internally — **no graph canister calls**.
3. Router slices the returned hits by `shard_id` and builds per-shard seeds.
4. Graph shards skip the leading `IndexIntersection` op (same class of optimization as skipping leading `IndexScan` as of 2026-06-10).

Graph shards **do not need foreign-shard hits** for intersection: the Router never sends them alien local vertex ids; only their own seed list.

## Local execution on a graph shard

Each shard receives:

- `ExecutePlanArgs { target_shard_id, plan_blob, seed_bindings_blob, ... }`
- Initial row bindings from seeds (local `VertexId` only)

The shard runs the physical plan against **local CSR** only. It does not call the index canister for anchor scans in the target model (Router already resolved anchors).

## Cross-shard traverse (“reach out”)

When a plan expands from a locally seeded vertex to a neighbor whose **authoritative** storage is on another shard:

1. Local executor calls `resolve_traversal_expand_source` (`graph/federation/expand.rs`): placement lookup on the expand source binding (`Vertex` or `RemoteVertex`).
2. **Peer expand** when authoritative shard ≠ local; **local CSR** when authoritative on this shard.
3. Source shard calls target shard's **`federated_expand`** (graph ↔ graph, `graph-kernel/federation/expand.rs`).
4. Returned neighbors are incorporated into **local** execution state for the remainder of the plan on that shard.
5. If multiple shards produce rows for the same logical query, Router merges.

This replaces the immature pattern where a **single** graph shard calls the index, binds `RemoteVertex`, and resolves placement inline during `IndexScan`.

## Index maintenance

Writes remain on graph shards; postings sync to index on DML (`graph/src/index/pending.rs`):

- Property set/unset → `posting_insert` / `posting_remove` with `shard_id = local`.
- Vertex delete → remove all property postings before CSR delete.

Index is authoritative for **which physical vertices match an indexed predicate**; graph tombstones are not consulted on index read paths when sync invariants hold.

**Partial:** vertex **label** membership postings on graph-index ([ADR 0004](../adr/0004-label-index.md), [label-index.md](../index/label-index.md)). `lookup_label` for seeds is implemented. Target: label sieve on property paths, label telemetry for count-only queries; narrow vertex-list export without unseeded-shard fallback on hit size. Interim v1 aggregate/scale-guard behavior to migrate.

## Merge (Router)

**Partial (v1):** `router/federation/merge.rs` unions independent shard-local row batches and sums row counts; `router/federation/aggregate_merge.rs` merges partial `PlanOp::Aggregate` results by GROUP BY key (COUNT/COUNT(*)/SUM/MIN/MAX). Used by `gql.rs` multi-shard dispatch. See [ADR 0002](../adr/0002-federated-row-batch-merge.md) and [ADR 0003](../adr/0003-federated-aggregate-merge.md).

Planned merge capabilities (detail TBD):

- Aggregate pushdown for non-mergeable functions (`AVG`, `DISTINCT`, ordered `COLLECT`).
- Dedup and join of row batches when fragments are not independent unions.
- ~~Returning merged `rows_blob` from `gql_query`.~~ **Done** — `GqlQueryResult`.

## Gap vs current code

| Target | Current (immature) | Action |
|--------|-------------------|--------|
| Router owns index lookup | Graph executor calls `PropertyIndexLookup` | **Done** — router seeds; graph index client disabled on federated wire path |
| Router slices intersection | Graph executor intersects after N× `lookup_equal` | **Done** — `lookup_intersection` + router `IndexAnchor` |
| Seeds per shard | `SeedProbe` only for `IndexScan` | **Done** — `IndexAnchor` for scan + intersection |
| Peer expand only when traversing | `RemoteVertex` from index hits in executor | **Partial** — one-hop `Expand`, var_len `{1,1}`, and `ShortestPath` entry use `resolve_traversal_expand_source`; multi-hop var_len peer sources and mid-path remote hops remain future work |
| Cohesive `federation/` modules | Logic in executor, facade stable, placement | **Partial** — graph `federation/{index_bind,expand,routing}` |

## Related documents

- [standalone-mode.md](standalone-mode.md)
- [../index/lookup-intersection.md](../index/lookup-intersection.md)
- [../federation/model.md](../federation/model.md)
- [../federation/query-semantics.md](../federation/query-semantics.md)
