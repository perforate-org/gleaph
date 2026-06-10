# Property index

Last updated: 2026-06-10

## Status

**Partially Implemented** — `lookup_equal` / `lookup_range` and DML posting sync exist. **`lookup_intersection`** is implemented on graph-index; router `IndexAnchor` + per-shard seeds and graph skip of leading intersection op are **Implemented**. Graph federated wire path does not call index; library tests may still inject a mock index client.

## Purpose

Explain the **graph-index canister** and how the router uses it for query routing (standalone as of 2026-06-10; per-shard slice in the federation target).

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
| `count_postings_by_value` | Implemented | Walk one property bucket; return `(encoded_value, count)` groups ([ADR 0003](../adr/0003-federated-aggregate-merge.md)) |

All read APIs run entirely inside graph-index (no graph canister calls).

**Planned:** `count_postings_by_value_for_label` — same bucket walk with label membership sieve
per posting ([label-index.md](label-index.md) Tier 3).

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

## Indexability vs primary storage

Primary vertex and edge property maps persist [`gleaph_gql::Value`] with [`Value::to_binary_bytes`].
Index postings use a separate **sortable index key** from [`gleaph_gql::value_to_index_key_bytes`].

Graph centralizes both paths in `crates/graph/src/property/`:

| Function | Role |
|----------|------|
| `ensure_persistable` | Primary-store write validation |
| `property_indexability` / `sortable_index_key` | Whether a value gets equality/range postings |
| `dispatch_property_index_ops` | Routes derived ops to federated vertex index or local edge equality |

**Index-only miss:** A value can be stored but omitted from indexes when `property_indexability`
is `NotIndexable` (non-finite floats, unsupported composite shapes, extensions without a sortable
key) or `Absent` (null). Equality and range scans will not find those vertices or edges until a
full scan path is used.

## Index maintenance

On DML / property updates, graph enqueues posting changes when federation routing and an index client are configured. Without client, mutations may drop index updates (`index/pending.rs`) — deployments with property indexes must wire the index canister.

**Backfill:** `backfill_property_postings` on graph shards replays indexable vertex properties from
`VERTEX_PROPERTIES` into graph-index via `posting_insert` (router-guarded update, same cursor batching
model as `backfill_label_postings`). Unindexable values are skipped (see `property_indexability` in
`crates/graph/src/property/`). Router orchestration is not yet wired; operators may call the graph
canister method directly during recovery.

## Related documents

- [label-index.md](label-index.md) — vertex label membership; tiered reads with property index ([ADR 0004](../adr/0004-label-index.md))
- [lookup-intersection.md](lookup-intersection.md)
- [../sharding/standalone-mode.md](../sharding/standalone-mode.md)
- [../sharding/federation-target.md](../sharding/federation-target.md)
- [../architecture/overview.md](../architecture/overview.md)
- [../federation/query-semantics.md](../federation/query-semantics.md)
- [../gql/plan-format.md](../gql/plan-format.md)
