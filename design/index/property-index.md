# Property index

## Purpose

Explain the **graph-index canister** and how the router uses it for multi-shard query routing.

## Non-goals

- Index build algorithms on graph writes (implementation in `graph/src/index/`).
- Full Candid API listing.

## Components

| Piece | Crate | Role |
|-------|-------|------|
| Posting key/value | `graph-index` | Property equality postings |
| `PostingHit` | `graph-kernel` | `{ shard_id, vertex_id }` |
| Router client | `router/index_client.rs` | `lookup_equal` |
| Seed resolution | `router/seed.rs` | Map hits → per-shard seed blobs |

## Router seed routing

**Implemented** in `dispatch_plan_blob` (`router/src/gql.rs`):

1. `SeedProbe::from_plans` detects an equality `IndexScan` anchor.
2. Router queries index with property id + encoded value.
3. Hits grouped by `shard_id` → one `execute_plan_on_graph` per shard.
4. `seeds_for_local_shard` builds `seed_bindings_blob` for local vertex ids.

If probe is **None**:

- **Single shard** — execute on that shard without seeds.
- **Multiple shards** — error: `no index anchor: single-shard graph required`.

**Design implication:** Multi-shard graphs need planner+schema support for an index-friendly anchor on common query shapes.

## Graph shard local indexes

Graph also maintains **shard-local** structures:

- Edge equality postings (`facade/stable/edge_equality_postings.rs`)
- Used for `EdgeIndexScan`, expand equality fast paths

These are distinct from the global property index canister.

## Federation materialization

When index hits reference foreign shards, executor materializes `PlanBinding::RemoteVertex` for variables bound from federated index rows ([federation/query-semantics.md](../federation/query-semantics.md)).

## Index maintenance

On DML / property updates, graph enqueues posting changes when an index client exists. Without client, mutations may drop index updates (`index/pending.rs`) — federated deployments should always wire index canister ids in shard registry.

## Related documents

- [architecture/overview.md](../architecture/overview.md)
- [federation/query-semantics.md](../federation/query-semantics.md)
- [gql/plan-format.md](../gql/plan-format.md)
