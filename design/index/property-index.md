# Property index

Last updated: 2026-06-13  
Anchor timestamp: 2026-06-13 11:04:14 UTC +0000

## Status

**Partially Implemented** — `lookup_equal` / `lookup_range` and DML posting sync exist. **`lookup_intersection`** is implemented on graph-index; router `IndexAnchor` + per-shard seeds and graph skip of leading intersection op are **Implemented**. Graph federated wire path does not call index; library tests may still inject a mock index client.

**Phase A ([ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md)) — Implemented:** graph shard registry (`register_indexed_property`); DML/backfill maintain postings **only for registered** vertex/edge properties; router `admin_set_indexed_vertex_property` / `admin_set_indexed_edge_property` fan out to shards.

**Phase B (ADR 0009) — Implemented:** `INDEX_EDGE_POSTINGS` on graph-index (`EdgePostingKey`); `edge_posting_insert` / `remove`, `lookup_edge_equal`; federated graph `edge_pending` flush; `backfill_edge_property_postings`.

**Phase C (ADR 0009) — Implemented:** `IndexSubject` on `IndexEqualSpec`; `lookup_intersection` returns `IndexIntersectionResult` (`Vertices` or `Edges`); mixed vertex+edge arms project to `(shard_id, owner_vertex_id)`.

**Phase D (ADR 0009) — Implemented:** router `EdgeIndexScan` / all-edge intersection → `lookup_edge_equal` / `lookup_intersection`; per-shard `LocalEdgePosting` seeds; graph applies edge seeds and skips leading `EdgeIndexScan`; shard-local `EDGE_EQUALITY_POSTINGS` retired (MemoryId repack to 40 regions); expand reads graph-index or canonical `EDGE_PROPERTIES` scan when no index client.

**Phase E (ADR 0009) — Implemented:** router extension DDL `CREATE INDEX` / `DROP INDEX` (parsed in `router/index_ddl.rs`, executed on `gql_execute*`); named index catalog per logical graph; controller or Manager+ auth; `admin_set_indexed_*` unchanged (property-level register without index name).

**ADR 0012 — Implemented:** edge `FOR` patterns carry Gleaph GQL `EdgeDirection` (bracket form only; slash rejected); graph-index edge keys use LARA `wire_label_id`; planner applies storage-class subset rule via `is_edge_property_indexed_for`. Leading `EdgeIndexScan` supports `PointingRight`, `PointingLeft`, and `Undirected`. PocketIC e2e covers `AnyDirection`, `PointingRight`, and `Undirected` indexes (including federated undirected anchor and subset negative: undirected-only index does not seed directed wire postings). See [0012-edge-index-direction-in-ddl.md](../adr/0012-edge-index-direction-in-ddl.md).

Property **name → `property_id`** assignment is **router SSOT** ([ADR 0006](../adr/0006-pre-federation-foundation.md) §2). Graph shards no longer maintain `PROPERTY_CATALOG` stable; DML and plan execution use router-resolved `PropertyId` on the wire (`ResolvedPropertyTable`).

## Purpose

Explain the **graph-index canister** and how the router uses it for query routing (standalone: sole shard `ShardId(0)`; per-shard slice in the federation target).

## Non-goals

- Index build algorithms on graph writes (implementation in `graph/src/index/`).
- Full Candid API listing.
- Index canister sharding (multiple index canisters) — **Planned** with graph multi-shard; split strategy deferred ([ADR 0010](../adr/0010-index-sharding-extensibility.md)). Near-term: one index Principal per shard group or shared across shards; capacity axes in [capacity-planning.md](capacity-planning.md).

## Components

| Piece | Crate | Role |
|-------|-------|------|
| Posting key/value | `graph-index` | Property equality postings |
| `PostingHit` | `graph-kernel` | `{ shard_id, vertex_id }` |
| Router client | `router/index_client.rs` | `lookup_equal`, `lookup_intersection` |
| Seed resolution | `router/seed.rs` | Map hits → per-shard seed blobs |

## Catalog ownership

| Layer | Owns |
|-------|------|
| **Router** | Property names ↔ `PropertyId` (`ROUTER_PROPERTY_CATALOG`); planner / DML resolve names before dispatch |
| **Graph shard** | `(property_id, Value)` on vertices and edges only — no property name stable |
| **Graph-index** | Postings keyed by router-issued `property_id` |

Standalone and test graphs without router resolution use hash-based test property ids (`crates/graph/src/test_labels.rs`) or explicit `ResolvedPropertyTable` on the plan wire.

## Posting model

Global postings keyed by `(property_id, encoded_value, shard_id, local_vertex_id)`.

- `property_id` — numeric id from the **router** catalog (same id graph uses when writing values).
- `shard_id` — owning graph shard (`ShardId(0)` in standalone).
- `local_vertex_id` — dense CSR id on that shard (`PostingHit.vertex_id` in `graph-kernel`).

A single index canister holds postings for all graph shards; `shard_id` tags the owning graph shard without embedding Principals in posting keys.

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

**Implemented (ADR 0009 phase D):** edge property equality postings live on **graph-index** with key
`(property_id, value, label_id, shard_id, owner_vertex_id, slot_index)`. Graph shard retains
`EDGE_PROPERTIES` as canonical values; `indexed_edge_equality` / `EdgeIndexScan` expand paths use
router seeds, a graph-index client when present, or a registered-property scan of `EDGE_PROPERTIES`
in standalone/library mode. Postings are maintained **only for administrator-registered**
properties (see ADR 0009 §2).

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
`crates/graph/src/property/`). Router orchestrates per-shard cursors via
`admin_property_backfill_step` / `admin_list_property_backfill_status` (controller-only).

## Derived-state lag

See [derived-state-query-semantics.md](derived-state-query-semantics.md) for query behavior when
pending flush, backfill, or index unavailability leaves postings behind canonical properties.

## Related documents

- [../adr/0006-pre-federation-foundation.md](../adr/0006-pre-federation-foundation.md) — router property catalog SSOT
- [derived-state-query-semantics.md](derived-state-query-semantics.md)
- [label-index.md](label-index.md) — vertex label membership; tiered reads with property index ([ADR 0004](../adr/0004-label-index.md))
- [lookup-intersection.md](lookup-intersection.md)
- [../sharding/standalone-mode.md](../sharding/standalone-mode.md)
- [../sharding/federation-target.md](../sharding/federation-target.md)
- [capacity-planning.md](capacity-planning.md) — 500 GiB headroom, split thresholds, planned inverted posting lists
- [../architecture/overview.md](../architecture/overview.md)
- [../federation/query-semantics.md](../federation/query-semantics.md)
- [../gql/plan-format.md](../gql/plan-format.md)
