# Property index

Last updated: 2026-06-30
Anchor timestamp: 2026-06-30 08:27:33 UTC +0000

## Status

**Partially Implemented** — `lookup_equal` / `lookup_range` and DML posting sync exist. **`lookup_intersection`** is implemented on graph-index; router `IndexAnchor` + per-shard seeds and graph skip of leading intersection op are **Implemented**. Graph federated wire path does not call index; library tests may still inject a mock index client.

**Phase A ([ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md)) — superseded by [ADR 0023](../adr/0023-federated-index-consistency-upgrade-compaction.md):** Phase A gated DML/backfill posting maintenance with a **persistent shard-local registry** (`register_indexed_property`) fanned out from the router. ADR 0023 (phases 1–2 implemented) **removes that registry** because it could not survive the upgrade boundary. The graph shard now holds **no persisted indexed catalog**: the router (definitions SSOT) supplies an `IndexedPropertyCatalog` in `ExecutePlanArgs.indexed_properties` (and in the backfill request), which the shard installs in an **ephemeral per-operation context** (`index/catalog_context.rs`) consulted by `dispatch_property_index_ops` and backfill. `CREATE INDEX` / `DROP INDEX` no longer fan registrations out to shards.

**Phase B (ADR 0009) — Implemented:** `INDEX_EDGE_POSTINGS` on graph-index (`EdgePostingKey`); `edge_posting_insert` / `remove`, `lookup_edge_equal`; federated graph `edge_pending` flush; `backfill_edge_property_postings`.

**Phase C (ADR 0009) — Implemented:** `IndexSubject` on `IndexEqualSpec`; `lookup_intersection` returns `IndexIntersectionResult` (`Vertices` or `Edges`); mixed vertex+edge arms project to `(shard_id, owner_vertex_id)`.

**Phase D (ADR 0009) — Implemented:** router `EdgeIndexScan` / all-edge intersection → `lookup_edge_equal` / `lookup_intersection`; per-shard `LocalEdgePosting` seeds; graph applies edge seeds and skips leading `EdgeIndexScan`; shard-local `EDGE_EQUALITY_POSTINGS` retired (MemoryId repack to 40 regions); expand reads graph-index or canonical `EDGE_PROPERTIES` scan when no index client.

**Phase E (ADR 0009) — Implemented:** router extension DDL `CREATE INDEX` / `DROP INDEX` (parsed in `router/index_ddl.rs`, executed on `gql_execute*`); named index catalog per logical graph; Admin or Manager+ auth. Legacy `admin_set_indexed_*` APIs delegate to `CREATE INDEX IF NOT EXISTS` with synthetic index names (require entity label + property).

**ADR 0012 — Implemented:** edge `FOR` patterns carry Gleaph GQL `EdgeDirection` (bracket form only; slash rejected); graph-index edge keys use LARA `wire_label_id`; planner applies storage-class subset rule via `is_edge_property_indexed_for`. Leading `EdgeIndexScan` supports `PointingRight`, `PointingLeft`, and `Undirected`. PocketIC e2e covers `AnyDirection`, `PointingRight`, and `Undirected` indexes (including federated undirected anchor and subset negative: undirected-only index does not seed directed wire postings). See [0012-edge-index-direction-in-ddl.md](../adr/0012-edge-index-direction-in-ddl.md).

**ADR 0018 — Implemented:** property and label **name → id** catalogs are **graph-scoped** per `GraphId` on the router. Plan build, DDL, and `admin_intern_*` resolve names in the effective graph context. Posting keys are unchanged — numeric `property_id` on the wire is already scoped by dispatch `GraphId`.

**ADR 0019 — Partially Implemented:** per-graph index clusters and graph-local `ShardId`; router `graph_index_lookup_targets` derives from **live shard registry**; shard unregister detaches auth and purges postings for the detached shard in **bounded, resumable steps** (`admin_detach_shard_canister` returns a `ShardDetachCursor`; the router resumes until `done`) so a large index cannot trap on the per-message instruction / stable read-write limit. See [0019-graph-local-shard-id-and-index-clusters.md](../adr/0019-graph-local-shard-id-and-index-clusters.md).

Property **name → `property_id`** assignment is **router SSOT per `GraphId`** ([ADR 0006](../adr/0006-pre-federation-foundation.md) §2, scope amended by [ADR 0018](../adr/0018-graph-scoped-label-property-catalogs.md)). Graph shards no longer maintain `PROPERTY_CATALOG` stable; DML and plan execution use router-resolved `PropertyId` on the wire (`ResolvedPropertyTable`).

## Purpose

Explain the **graph-index canister** and how the router uses it for query routing (standalone: sole shard `ShardId(0)`; per-shard slice in the federation target).

## Non-goals

- Index build algorithms on graph writes (implementation in `graph/src/index/`).
- Full Candid API listing.
- Index canister sharding (multiple index canisters) — **Partially Implemented** per-graph `index_cluster` and shard-group formula ([ADR 0019](../adr/0019-graph-local-shard-id-and-index-clusters.md)); subject/range split axes remain planned ([ADR 0010](../adr/0010-index-sharding-extensibility.md), [capacity-planning.md](capacity-planning.md)).

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
| **Router** | Property names ↔ `PropertyId` **per `GraphId`** (`ROUTER_PROPERTY_CATALOG`); planner / DML resolve names in graph context before dispatch |
| **Graph shard** | `(property_id, Value)` on vertices and edges only — no property name stable |
| **Graph-index** | Postings keyed by router-issued `property_id` (interpreted under the owning graph's dispatch / index-cluster boundary) |

Standalone and test graphs without router resolution use hash-based test property ids (`crates/graph/src/test_labels.rs`) or explicit `ResolvedPropertyTable` on the plan wire.

## Posting model

Postings keyed by `(property_id, encoded_value, shard_id, local_vertex_id)` — **no `GraphId` in the key** ([ADR 0010](../adr/0010-index-sharding-extensibility.md)).

- `property_id` — numeric id from the router catalog **for the dispatch `GraphId`** (same id the graph shard uses when writing values for that graph).
- `shard_id` — graph-local shard ordinal within the dispatch graph ([ADR 0019](../adr/0019-graph-local-shard-id-and-index-clusters.md)); `ShardId(0)` in standalone.
- `local_vertex_id` — dense CSR id on that shard (`PostingHit.vertex_id` in `graph-kernel`).

An index canister holds postings for shards attached to **one graph's index cluster**; `shard_id` tags the owning graph shard without embedding Principals or `GraphId` in posting keys. Router read paths derive lookup targets from **live shard registry** and filter hits to registered shards.

**Invariant:** Postings reflect **live** property values. DML on graph shards enqueues `posting_insert` / `posting_remove` (`graph/src/index/pending.rs`). Vertex/property delete removes postings; index read APIs do not consult graph tombstones. See [../sharding/standalone-mode.md](../sharding/standalone-mode.md).

## Read APIs

| API | Status | Role |
|-----|--------|------|
| `lookup_equal` | Implemented | Equality postings for one `(property_id, value)` — full bucket (small buckets / tests) |
| `lookup_range` | Implemented | Range over encoded values for one property — full range (small ranges / tests) |
| `lookup_equal_page` | Implemented | Paginated equality export (`after` + `limit`); the seed-routing read path |
| `lookup_range_page` | Implemented | Paginated range export over encoded values (`after` + `limit`) |
| `lookup_edge_equal_page` | Implemented | Paginated edge equality export (`after` + `limit`) |
| `lookup_intersection` | Implemented | Intersect multiple equality arms (edge/mixed; vertex-only is streamed via `lookup_intersection_page` — [lookup-intersection.md](lookup-intersection.md)) |
| `lookup_intersection_page` | Implemented | Paginated all-vertex equality intersection (`after` + `limit`): server-side walk-arm page + in-heap merge-join sieve; the streamed vertex-intersection read path |
| `lookup_range_intersection_page` | Implemented | Paginated range-walk plus one equality sieve (`after` + `limit`): server-side finite range page filtered by one vertex equality arm; the mixed equality-plus-range read path |
| `count_postings_by_value` | Implemented | Walk one property bucket; return `(encoded_value, count)` groups ([ADR 0003](../adr/0003-federated-aggregate-merge.md)) |

All read APIs run entirely inside graph-index (no graph canister calls).

**No full-bucket heap materialization invariant** ([capacity-planning.md](capacity-planning.md):
"Query paths must not materialize full buckets in heap"). Query consumers read postings through the
paginated `*_page` APIs, which return at most `limit` hits plus a resume cursor — bounding the
per-message heap on the index canister. The router (`RouterIndexLookup` /
`RouterIndexClient::lookup_equal_page` / `lookup_edge_equal_page`) and the standalone graph client
(`IcPropertyIndexClient`) loop pages for `lookup_equal` / `lookup_range` / `lookup_edge_equal`. The
non-paginated `lookup_equal` / `lookup_range` / `lookup_edge_equal` endpoints are retained for small
buckets and tests, mirroring `lookup_label` vs `lookup_label_page` ([label-index.md](label-index.md)
path A). **All-vertex equality intersection** (the planner's `IndexIntersection`) is now **streamed**:
consumers loop the server-side `lookup_intersection_page`, which walks the first arm one page at a
time and sieves the rest in-heap via a bounded merge-join, so no arm's full bucket is materialized and
the walk + sieve fold into one message per page. **Edge / mixed** intersection still builds per-arm
posting sets in heap server-side — see
[lookup-intersection.md](lookup-intersection.md#streaming-intersection-status).

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
key, or encoded index key length above `MAX_INDEX_VALUE_KEY_BYTES` — see
[capacity-planning.md](capacity-planning.md)) or `Absent` (null). Equality and range scans will not
find those vertices or edges until a full scan path is used. Router and graph reject oversized query
keys before graph-index calls; graph-index read APIs reject them as `IndexValueKeyTooLarge` (no
silent empty range).


## Vector search filter membership (ADR 0034 Slices 6, 7, 8, 9, 10, 11, 12 and 13)

Property-index postings own filter membership for leading and non-leading `SEARCH ... WHERE`
predicates. The Router consumes postings through bounded pagination:

- The planner accepts one same-binding equality predicate, one to eight `AND`-connected
  same-binding equality predicates on **distinct** properties, exactly one same-binding numeric range
  predicate (`<`, `<=`, `>`, `>=`), exactly two same-property range predicates forming one lower
  and one upper bound, exactly one equality predicate and one one-sided numeric range predicate
  on distinct properties, or exactly one equality predicate and two same-property range predicates
  (one lower and one upper) on a distinct property, and preserves the original filter expression in
  `PlanOp::Search`.
- The Router resolves the searched label and every filter property to router-issued ids and proves
  an active vertex property index for the exact `(graph_id, label_id, property_id)` tuple in the
  named-index catalog for every arm. For a leading search the label is taken from the leading
  labeled `NodeScan`. For a non-leading search the label is proved from the top-level prefix: a
  labeled `NodeScan` for the searched binding, or a `PropertyFilter`/`ExpandFilter` carrying
  `IS LABELED(binding, label, negated = false)` before the `PlanOp::Search`.
- For equality arms it encodes every comparison value with `gleaph_gql::value_to_index_key_bytes`
  and validates each encoded size against `MAX_INDEX_VALUE_KEY_BYTES` before calling the index.
  For one equality arm it pages through `lookup_equal_page` for `(property_id, encoded_value)`. For
  two to eight equality arms it constructs vertex-only `IndexEqualSpec`s and pages through the
  server-side `lookup_intersection_page`, which canonicalises the walk arm by `(property_id,
  encoded_value)` order and sieves the remaining arms in heap without materializing full buckets.
  Nine or more equality arms are rejected with `InvalidArgument`.
- For one numeric range arm it derives a finite half-open encoded comparison-domain range with the
  canonical `gleaph_gql::numeric_range_bounds` helper, validates each bound size against
  `MAX_INDEX_VALUE_KEY_BYTES`, and pages through `lookup_range_page` with
  `PostingRangeRequest::Between { low, high }`. For two same-property range arms the Router derives
  both half-open intervals through the same canonical helper, intersects them once (`low =
  max(first.low, second.low)`, `high = min(first.high, second.high)`), validates the final bounds, and
  issues one paginated `lookup_range_page` stream with the same `PostingRangeRequest::Between`.
- For one equality arm plus one one-sided range arm on distinct properties the Router proves active
  vertex property indexes for both properties, encodes the equality value with
  `gleaph_gql::value_to_index_key_bytes`, derives a finite half-open encoded numeric interval with
  `gleaph_gql::numeric_range_bounds`, and pages through `lookup_range_intersection_page`. Property
  Index walks the finite encoded range one page at a time and sieves each page against the equality
  arm server-side. The sieve is span-aware: it uses a fast dense merge scan when the page's
  `(shard_id, vertex_id)` span is small relative to its size, and falls back to page-size-bounded
  point lookups when the range-walk page scatters hits across arbitrary subject ids. The returned
  `next`/`done` always describe the range walk, so a page with zero survivors is not terminal while
  the range walk has more pages.
- For one equality arm plus two same-property range arms on a distinct property (ADR 0034 Slice 12)
  the Router performs no new Property Index operation. It reuses the same `lookup_range_intersection_page`
  path: the two range arms are collapsed into one intersected finite half-open encoded interval by the
  same `resolve_filtered_range_interval` logic used for Slice 10, the equality value is encoded once,
  and one paginated `lookup_range_intersection_page` stream walks the interval and sieves each page by
  the equality arm.
- If the numeric interval is empty (`low >= high`) the Router returns an empty candidate set before
  calling the Property Index or Vector Index, preserving the empty-candidate dispatch contract.
- In all cases the Router deduplicates by `(shard_id, vertex_id)` and stops as soon as a 4097th
  distinct subject is observed. Exceeding the bound returns an explicit
  `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` error. The Property Index validates bounds structurally
  (`low < high`, key sizes) and scans only the opaque encoded interval inside the requested property
  bucket; it does not interpret GQL value types or numeric ordering.
- The candidate set is intersected with the searched vertex label on each page using
  `filter_hits_by_label`, so vertices that do not belong to the searched label do not consume the
  candidate bound.
- Postings are typed as vertex subjects by the graph-index API; the Router’s allowlist carries only
  `(shard_id, vertex_id)` tuples. Vector hits for shards that are no longer live are ignored downstream
  by the Router while it builds per-shard search seeds (leading) or lowers resolved search relations
  (non-leading). The Vector Index does not know live topology and therefore does not perform this filtering.
- The resulting allowlist is passed to Vector Index as a transient `candidate_subjects` field on the
  internal `VectorSearchRequest`. No new stable region is added and no property values are duplicated in
  the vector canister.

This follows the existing derived-state contract: postings may lag canonical vertex properties, and the search read path treats those postings as the source of filter membership. The query does not re-verify the equality property against canonical vertex properties at runtime; results are therefore eventually consistent with the primary store and may temporarily include vertices whose property has changed or omit vertices whose posting is still pending.

## Index maintenance

On DML / property updates, graph enqueues posting changes when federation routing and an index client are configured. Without client, mutations may drop index updates (`index/pending.rs`) — deployments with property indexes must wire the index canister.

**Backfill:** `backfill_vertex_property_postings` on graph shards replays indexable vertex properties from
`VERTEX_PROPERTIES` into graph-index via `posting_insert` (router-guarded update, same cursor batching
model as `backfill_label_postings`). Unindexable values are skipped (see `property_indexability` in
`crates/graph/src/property/`). Router orchestrates per-shard cursors via
`admin_vertex_property_backfill_step` / `admin_list_vertex_property_backfill_status` (Admin-only).

**Durable repair journal (ADR 0023 D5):** the happy-path flush stays volatile and persists nothing.
When a flush fails, the batch is compensated and persisted to the stable `INDEX_REPAIR_JOURNAL`
(`graph/src/index/repair_journal.rs`, `MemoryId 41`) instead of the volatile queue, so the
store-ahead/index-behind delta survives the upgrade boundary, the router-less timer context, and
traps. The maintenance driver re-applies journal entries each tick and on `post_upgrade`, removing
each once graph-index accepts it; re-application is idempotent so no compensation is needed on the
drain path. If compensation itself fails, the canister no longer traps (P4) — the full batch is
journaled all the same, since idempotent re-application converges the index to the store regardless
of the partial compensation state.

**`DROP INDEX` posting purge (ADR 0023 D6):** dropping an index removes the dropped property's
postings from graph-index, not just the router catalog entry (closing P7, where dropped indexes
orphaned their postings). The index exposes a router-guarded, bounded, resumable
`admin_purge_property_postings(kind, property_id, label_id, resume)`
(`graph-index/src/facade/store/posting_purge.rs`) mirroring `admin_detach_shard_canister`: posting
keys order `property_id` first, so each scope is a contiguous range — **vertex** keys carry no label
(purge the whole `property_id` range); **edge** keys carry the catalog `label_id` (direction
stripped), so the purge filters to `(property_id, label_id)`. `router::drop_index` purges only when
the postings are no longer referenced (`is_property_registered` for a shared vertex property;
`edge_index_uses_property_label` for a per-`(property, label)` edge scope), fanning the resume loop
out to every index canister backing the graph's live shards (`graph_index_lookup_targets`). The
purge is stateless (no new stable region).

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
