# Property index

## Status

**Partially Implemented** — `lookup_equal` / `lookup_range` and DML posting sync exist. **`lookup_intersection`** is implemented on graph-index; router `IndexAnchor` + per-shard seeds and graph skip of leading intersection op are **Implemented**. Legacy graph direct index and `RemoteVertex` index bind paths remain deferred; see [../sharding/federation-target.md](../sharding/federation-target.md).

## Purpose

Explain the **graph-index canister** and how the router uses it for query routing (standalone today; per-shard slice in the federation target).

## Non-goals

- Index build algorithms on graph writes (implementation in `graph/src/index/`).
- Full Candid API listing.
- Index canister sharding (multiple index canisters) — future work.

## Components

| Piece | Crate | Role |
|-------|-------|------|
| Posting key/value | `graph-index` | Property equality postings |
| `PostingHit` | `graph-kernel` | `{ shard_id, vertex_id }` |
| Router client | `router/index_client.rs` | `lookup_equal`, `lookup_intersection` |
| Seed resolution | `router/seed.rs` | Map hits → per-shard seed blobs |

## Posting model

Global postings keyed by `(property_id, encoded_value, shard_id, vertex_id)`. A single index canister holds postings for all graph shards; `shard_id` tags the owning graph shard.

**Invariant:** Postings reflect **live** property values. DML on graph shards enqueues `posting_insert` / `posting_remove` (`graph/src/index/pending.rs`). Vertex/property delete removes postings; index read APIs do not consult graph tombstones. See [../sharding/standalone-mode.md](../sharding/standalone-mode.md).

## Read APIs

| API | Status | Role |
|-----|--------|------|
| `lookup_equal` | Implemented | Equality postings for one `(property_id, value)` |
| `lookup_range` | Implemented | Range over encoded values for one property |
| `lookup_intersection` | Implemented | Intersect multiple equality arms ([lookup-intersection.md](lookup-intersection.md)) |

All read APIs run entirely inside graph-index (no graph canister calls).

## Router seed routing (current)

**Partially Implemented** in `dispatch_plan_blob` (`router/src/gql.rs`):

1. `SeedProbe::from_plans` detects an equality `IndexScan` anchor only.
2. Router queries index with property id + encoded value.
3. Hits grouped by `shard_id` → one `execute_plan_on_graph` per shard.
4. `seeds_for_local_shard` builds `seed_bindings_blob` for local vertex ids.

If probe is **None**:

- **Single shard** — execute on that shard without seeds.
- **Multiple shards** — error: `no index anchor: single-shard graph required`.

**Implemented:** `IndexIntersection` anchors via `IndexAnchor::from_plans` and `lookup_intersection` with the same per-shard slice as `IndexScan`.

## Target: Router owns index reads

In the federation target, **graph shards do not call the index on the query hot path**. Router performs `lookup_equal` / `lookup_intersection`, slices `PostingHit` by `shard_id`, and passes seeds to each graph shard.

Legacy: graph executor still calls `PropertyIndexLookup` for `IndexScan` / `IndexIntersection` during the standalone transition.

## Graph shard local indexes

Graph maintains **shard-local** structures distinct from the property index canister:

- Edge equality postings (`facade/stable/edge_equality_postings.rs`)
- Used for `EdgeIndexScan`, expand equality fast paths

## Index maintenance

On DML / property updates, graph enqueues posting changes when federation routing and an index client are configured. Without client, mutations may drop index updates (`index/pending.rs`) — deployments with property indexes must wire the index canister.

## Related documents

- [lookup-intersection.md](lookup-intersection.md)
- [../sharding/standalone-mode.md](../sharding/standalone-mode.md)
- [../sharding/federation-target.md](../sharding/federation-target.md)
- [../architecture/overview.md](../architecture/overview.md)
- [../federation/query-semantics.md](../federation/query-semantics.md)
- [../gql/plan-format.md](../gql/plan-format.md)
