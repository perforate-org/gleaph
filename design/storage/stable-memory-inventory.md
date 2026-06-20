# Stable-memory inventory

Last updated: 2026-06-20
Status: Implemented (graph: sequential LARA MemoryIds 0–31 + facade 32–41 = 42 regions; router repack ADR 0011/0018/0019 = 34 regions, 0–33)
Anchor timestamp: 2026-06-20 05:00:00 UTC +0000

Layout change policy: [ADR 0007](../adr/0007-stable-memory-layout.md).

## Purpose

Single inventory of stable-memory regions and heap-only facade state for the graph, router, and graph-index canisters. Each row names the owning domain, classification, and rebuild path where one exists.

Code source of truth for runtime `MemoryId` constants:

- `crates/graph/src/facade/stable/memory.rs`
- `crates/router/src/facade/stable/memory.rs`
- `crates/graph-index/src/facade/stable/memory.rs`

Typed layout registry (descriptive mirror + validation tests): `gleaph_graph_kernel::stable_layout`
and per-canister `facade/stable/layout.rs` — [ADR 0007](../adr/0007-stable-memory-layout.md) §7.

Thread-local pairing: `facade/stable.rs` in each crate.

### Region-count doc-sync checklist

Region counts and per-region classes in this document mirror the typed registry, which is the
mechanical source of truth. The registry tests enforce the counts below; when they change, update
this document and [ADR 0007](../adr/0007-stable-memory-layout.md) in the same patch:

| Canister | Regions | Id range | Registry constant + test |
|----------|---------|----------|--------------------------|
| Graph | 42 | 0–41 | `GRAPH_STABLE_LAYOUT` — `graph_layout_registry_matches_baseline` |
| Router | 34 | 0–33 | `ROUTER_STABLE_LAYOUT` — `router_layout_registry_matches_baseline` |
| Graph-index | 7 | 0–6 | `INDEX_STABLE_LAYOUT` — `index_layout_registry_matches_baseline` |

The canonical/derived split for the router registry projections is pinned by
`router_registry_canonical_derived_split_matches_inventory`.

## Classifications

Authoritative definitions and Gleaph examples: `gleaph_graph_kernel::stable_layout::StableMemoryClass`
(rustdoc on each variant). Per-region class and functional role: `GRAPH_STABLE_LAYOUT`,
`ROUTER_STABLE_LAYOUT`, `INDEX_STABLE_LAYOUT`.

| Class | Meaning | Examples in this repo |
|-------|---------|------------------------|
| `canonical` | Authoritative facts; system meaning does not depend on derived stores | Forward LARA CSR/payloads; vertex/edge properties; router registry and catalogs; mutation idempotency |
| `derived` | Projection or mirror rebuildable from canonical state | Reverse LARA; edge aliases/equality postings; graph-index postings |
| `maintenance` | Physical or admin bookkeeping; not query truth | LARA free spans; maintenance queue; router backfill cursors |
| `catalog` | Bidirectional name ↔ id maps (`BidirectionalCatalog`) | Router label/property/graph/index-name resolution pairs |
| `telemetry` | Event-sourced label stats and projection adjuncts | Graph label stats delta log; router label stats and `ROUTER_LABEL_STATS_PROJECTION` |
| `compatibility` | Legacy read view; another store owns new writes | *(none — P1 `EDGE_WEIGHT_PROFILES` retired 2026-06-12)* |
| `ephemeral` | Heap-only; no `MemoryId` — **not in layout registry** | Graph `PENDING` queues; router planner catalog |

**Sync co-update:** Some derived stores are updated in the same mutation as their canonical source (no async lag). They still have a separate physical region and are classified `derived`.

**Query semantics when derived state lags:** [derived-state-query-semantics.md](../index/derived-state-query-semantics.md).

## Derived-state rebuild summary

| Derived store | Canonical source | Update path | Rebuild / backfill |
|---------------|------------------|-------------|-------------------|
| LARA reverse orientation | Forward edges + payloads | Co-updated on edge insert/delete | **Check:** `check_reverse_adjacency` (`facade/derived_state/reverse_adjacency.rs`) detects forward/reverse divergence over local directed edges. Rebuild deferred: reconstructing reverse reassigns reverse slot indices, cascade-invalidating `EDGE_ALIASES` keys + reverse payload sidecars (future ADR). |
| Edge aliases | Forward/reverse adjacency in `GRAPH` | Sync: `commit_insert_edge_alias` on edge insert | **Implemented:** `check_edge_aliases` + `rebuild_edge_aliases` (`facade/derived_state/edge_alias.rs`) |
| Edge property postings (graph-index) | `EDGE_PROPERTIES` (registered props) | DML + `edge_pending` flush | **Implemented:** `backfill_edge_property_postings` + router `admin_edge_backfill_step` ([ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md); retired shard-local `EDGE_EQUALITY_POSTINGS` 2026-06-12) |
| Vertex property postings (graph-index) | Vertex properties (indexable) | DML + `pending.rs` flush | **Implemented:** `backfill_vertex_property_postings` + router `admin_vertex_property_backfill_step` |
| Label postings (graph-index) | `VertexLabelStore` | DML + `label_pending` flush | **Implemented:** `backfill_label_postings` + router `admin_label_backfill_step` ([label-index.md](../index/label-index.md)) |
| Router label stats projection | Graph `LabelStatsDelta` | `advance_label_stats_projection` + per-shard cursor | **Implemented:** graph delta log replay via `admin_label_stats_projection_step`; no full historical scan |
| Router indexed-property catalog | Property catalog + planner stats | Planner registration | **Stable** — row layout MemoryId 18–19 |

---

## Graph canister — LARA bundle

`init_graph()` wires **32** consecutive `MemoryId` regions (0–31) into one `DeferredBidirectionalLabeledLaraGraph`. Thread-local: `GRAPH`.

### Forward orientation (canonical adjacency + payloads)

| MemoryId | Symbol | Role | Class | Rebuild |
|--------|--------|------|-------|---------|
| 0 | `FWD_VERTICES` | Vertex rows | canonical | — |
| 1 | `FWD_BUCKETS` | Per-vertex edge buckets | canonical | — |
| 2 | `FWD_BUCKET_FREE_SPANS` | Retired bucket physical spans | maintenance | — |
| 3 | `FWD_BUCKET_FREE_SPAN_BY_START` | Bucket free-span index | maintenance | — |
| 4 | `FWD_EDGE_COUNTS` | Per-vertex edge counts | canonical | — |
| 5 | `FWD_EDGES` | Edge slab | canonical | — |
| 6 | `FWD_EDGE_LOG` | Edge value log | canonical | — |
| 7 | `FWD_EDGE_SPAN_META` | Edge span metadata | maintenance | — |
| 8 | `FWD_EDGE_FREE_SPANS` | Retired edge physical spans | maintenance | — |
| 9 | `FWD_EDGE_FREE_SPAN_BY_START` | Edge free-span index | maintenance | — |
| 10 | `FWD_PAYLOAD_SLAB` | Labeled edge payload slab | canonical | — |
| 11 | `FWD_PAYLOAD_FREE_SPANS` | Payload free spans | maintenance | — |
| 12 | `FWD_PAYLOAD_FREE_SPAN_BY_START` | Payload free-span index | maintenance | — |
| 13 | `FWD_PAYLOAD_LOG` | Payload value log | canonical | — |
| 14 | `FWD_PAYLOAD_BLOBS` | Large payload blobs | canonical | — |

### Reverse orientation (derived adjacency + payloads)

| MemoryId | Symbol | Role | Class | Rebuild |
|--------|--------|------|-------|---------|
| 15 | `REV_VERTICES` | Reverse vertex rows | derived | Sync co-update; `check_reverse_adjacency` oracle |
| 16 | `REV_BUCKETS` | Reverse buckets | derived | Sync co-update |
| 17–18 | `REV_BUCKET_FREE_SPANS`, `REV_BUCKET_FREE_SPAN_BY_START` | Reverse bucket maintenance | maintenance | — |
| 19 | `REV_EDGE_COUNTS` | Reverse edge counts | derived | Sync co-update |
| 20 | `REV_EDGES` | Reverse edge slab | derived | Sync co-update |
| 21 | `REV_EDGE_LOG` | Reverse edge log | derived | Sync co-update |
| 22–24 | `REV_EDGE_SPAN_META`, `REV_EDGE_FREE_SPANS`, `REV_EDGE_FREE_SPAN_BY_START` | Reverse edge maintenance | maintenance | — |
| 25 | `REV_PAYLOAD_SLAB` | Reverse payload slab | derived | Sync co-update |
| 26–27 | `REV_PAYLOAD_FREE_SPANS`, `REV_PAYLOAD_FREE_SPAN_BY_START` | Reverse payload maintenance | maintenance | — |
| 28 | `REV_PAYLOAD_LOG` | Reverse payload log | derived | Sync co-update |
| 29 | `REV_PAYLOAD_BLOBS` | Reverse payload blobs | derived | Sync co-update |

### LARA maintenance

| MemoryId | Symbol | Role | Class | Rebuild |
|--------|--------|------|-------|---------|
| 30 | `MAINTENANCE_QUEUE` | Deferred PMA work queue | maintenance | Internal LARA drain |
| 31 | `DIRTY_WORK_ITEMS` | Dirty work tracking | maintenance | Internal LARA drain |

Owner: `ic-stable-lara` / graph `GRAPH` thread-local. Scan paths must not consult PMA maintenance stores ([lara.md](./lara.md)).

---

## Graph canister — facade regions

Repacked 2026-06-11. **Removed:** property name catalog, `VERTEX_LOGICAL_IDS`, federation remote-ref stable (`REMOTE_VERTEX_REFS`, `REMOTE_FORWARD_IN`), `PEER_GRAPH_CANISTERS`. LARA ids are consecutive **0–31**; facade starts at **32**.

| MemoryId | Symbol | Thread-local | Init fn | Class | Owner domain | Rebuild |
|--------|--------|--------------|---------|-------|--------------|---------|
| 32 | `VERTEX_LABEL_SETS` | `VERTEX_LABELS` | `init_vertex_label_store` | canonical | labels | — |
| 33 | `VERTEX_PROPERTIES` | `VERTEX_PROPERTIES` | `init_vertex_property_store` | canonical | properties | — |
| 34 | `EDGE_PROPERTIES` | `EDGE_PROPERTIES` | `init_edge_property_store` | canonical | properties | — |
| 35 | `EDGE_ALIASES` | `EDGE_ALIASES` | `init_edge_alias_index` | derived | adjacency | `check_edge_aliases` / `rebuild_edge_aliases` |
| 36 | `GRAPH_METADATA` | `METADATA` | `init_metadata` | canonical | federation metadata | — |
| 37 | `LABEL_STATS_DELTA_SEQ` | `LABEL_STATS_DELTA_SEQ` | `init_label_stats_delta_seq` | telemetry | label stats projection | Monotonic seq allocator |
| 38 | `LABEL_STATS_DELTA_LOG` | `LABEL_STATS_DELTA_LOG` | `init_label_stats_delta_log` | telemetry | label stats projection | Delta replay to router |
| 39 | `GRAPH_MUTATION_JOURNAL` | `GRAPH_MUTATION_JOURNAL` | `init_graph_mutation_journal` | canonical | idempotency | Mutation outcome + emitted delta seq range. A single-DML mutation is recorded `Completed` even when its index flush is deferred to the repair journal (region 41), since the store mutation + deltas are durable and the index converges async (ADR 0024) |
| 40 | `PENDING_VERTEX_PURGES` | `PENDING_VERTEX_PURGES` | `init_pending_vertex_purges` | maintenance | vertex delete | Tombstoned vertices mid-purge (ADR 0021); rebuildable by scanning tombstoned vertices with surviving incident edges (no API) |
| 41 | `INDEX_REPAIR_JOURNAL` | `INDEX_REPAIR_JOURNAL` | `init_index_repair_journal` | maintenance | federated index repair | Failed-flush index postings persisted on compensation-success (ADR 0023 D5); re-applied by the maintenance driver each tick and on `post_upgrade`, removed on success |

Graph facade **42 regions** total (32 LARA + 10 facade). Retired 2026-06-12: `EDGE_PAYLOAD_PROFILES` → router SSOT ([ADR 0008](../adr/0008-edge-payload-profile-router-ssot.md)); `EDGE_EQUALITY_POSTINGS` → graph-index ([ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md)).

Property **names** are router-owned (`ROUTER_PROPERTY_CATALOG`); graph stores values by `PropertyId` only.

### Graph ephemeral (not in `memory.rs`)

| Symbol | Location | Role | Reopen behavior |
|--------|----------|------|-----------------|
| `PENDING` (property postings) | `graph/src/index/pending.rs` | Queued property index ops | In-flight batch lost on upgrade; failed batches persist to `INDEX_REPAIR_JOURNAL` (ADR 0023 D5); `backfill_vertex_property_postings` covers historical vertex properties |
| `PENDING` (edge postings) | `graph/src/index/edge_pending.rs` | Queued edge property index ops | In-flight batch lost on upgrade; failed batches persist to `INDEX_REPAIR_JOURNAL` (ADR 0023 D5); `backfill_edge_property_postings` covers historical edge properties |
| `PENDING` (label postings) | `graph/src/index/label_pending.rs` | Queued label index ops | In-flight batch lost on upgrade; failed batches persist to `INDEX_REPAIR_JOURNAL` (ADR 0023 D5); `backfill_label_postings` covers historical labels |

---

## Router canister — stable regions

Repacked 2026-06-17: placement removed, controllers merged into auth, MemoryIds compacted to **0–33** (34 regions). Regions grouped **auth → registry → runtime config → idempotency → catalog → telemetry → maintenance**. `ROUTER_GRAPHS` keyed by **`GraphId`**; `ShardRegistryEntry` stores **`graph_id: GraphId`**. `ROUTER_SHARDS` keyed by **`GraphShardKey { graph_id, shard_id }`**; `ROUTER_SHARD_BY_GRAPH` is **`Principal → GraphShardKey`**; shard listing per logical graph uses **`ROUTER_SHARDS_BY_GRAPH_ID`**.

| MemoryId | Symbol | Thread-local | Init fn | Class | Owner domain | Rebuild |
|--------|--------|--------------|---------|-------|--------------|---------|
| 0 | `ROUTER_AUTH_PRINCIPAL_RECORDS` | `ROUTER_AUTH_STATE` | `init_auth_state` | canonical | auth | SSOT for router principal roles (`Role::Admin` for ops) |
| 1 | `ROUTER_GRAPHS` | `ROUTER_GRAPHS` | `init_graphs` | canonical | registry | **`BTreeMap<GraphId, GraphRegistryEntry>`** — graph registry SSOT |
| 2 | `ROUTER_SHARDS` | `ROUTER_SHARDS` | `init_shards` | canonical | registry | **`GraphShardKey → ShardRegistryEntry`** — shard dispatch SSOT ([ADR 0019](../adr/0019-graph-local-shard-id-and-index-clusters.md)) |
| 3 | `ROUTER_SHARD_BY_GRAPH` | `ROUTER_SHARD_BY_GRAPH` | `init_shard_by_graph` | derived index | registry | **`Principal → GraphShardKey`** — denormalized from `ROUTER_SHARDS`; commit-synced |
| 4 | `ROUTER_SHARDS_BY_GRAPH_ID` | `ROUTER_SHARDS_BY_GRAPH_ID` | `init_shards_by_graph_id` | derived index | registry | **`GraphId → Vec<ShardId>`** — denormalized fan-out index; commit-synced |
| 5 | `ROUTER_GRAPH_RUNTIME_CONFIG` | `ROUTER_GRAPH_RUNTIME_CONFIG` | `init_graph_runtime_config` | canonical | runtime config | `GraphId → GraphRuntimeConfig` (`element_id_encoding_key`, `index_group_size`, `index_cluster`; ADR 0019 S2b/S3) |

### Registry denormalization invariants (implemented 2026-06-17)

Regions **1–2** (canonical), **3–4** (derived indexes), **`ROUTER_GRAPH_RUNTIME_CONFIG` (5)**, plus **`ROUTER_GRAPH_CATALOG` (15–16)** form an intentional denormalized lookup set — not a merge candidate. Federation dispatch depends on all six staying synchronized at each registry **commit** boundary.

| Region | Class | Role in invariant |
|--------|-------|-------------------|
| `ROUTER_GRAPH_CATALOG` | catalog (registry commit) | name ↔ `GraphId` |
| `ROUTER_GRAPHS` | canonical | `GraphId` → `GraphRegistryEntry` (RBAC, status, `is_home`) |
| `ROUTER_GRAPH_RUNTIME_CONFIG` | canonical | `GraphId` → `GraphRuntimeConfig` (encoding key, index cluster) |
| `ROUTER_SHARDS` | canonical | `GraphShardKey` → `ShardRegistryEntry` — dispatch SSOT |
| `ROUTER_SHARDS_BY_GRAPH_ID` | derived index | `GraphId` → `[ShardId]` — fan-out listing (shard ordinals graph-local) |
| `ROUTER_SHARD_BY_GRAPH` | derived index | `Principal` → `GraphShardKey` — graph canister uniqueness |

**Commit APIs** (`RouterStore::commit_register_graph`, `commit_register_shard`, `commit_unregister_shard` in `crates/router/src/facade/store/registry.rs`) update the affected regions atomically from the domain owner's perspective. **`commit_register_shard`** requires a matching `ROUTER_GRAPHS` entry (not catalog-only). **`check_registry_invariants`** (`registry_invariants.rs`) verifies full bidirectional consistency; unit tests call it after every registry mutation. Per-commit verification is disabled in production for cost (`verify_registry_invariants_after_commit` is a no-op outside tests), so the same read-only oracle is also exposed as the admin query **`admin_check_registry_invariants`** (`Role::Admin`) — used to confirm registry consistency on demand, including across a canister upgrade (the router has no upgrade hook and relies on stable memory surviving the reinstall). E2E coverage: `pocket-ic-tests/tests/router_registry_invariants.rs`. **`list_shards_for_graph_id`** / **`list_shards_for_graph`** walk `ROUTER_SHARDS_BY_GRAPH_ID` only (O(shards for graph)), hydrate **`Vec<ShardRegistryEntry>`** from `ROUTER_SHARDS`, and reject duplicate index ids and stale index→primary references; they do not full-scan `ROUTER_SHARDS` — missing index rows are caught on commit / by `check_registry_invariants`.

| 6 | `ROUTER_MUTATION_COUNTER` | `ROUTER_MUTATION_COUNTER` | `init_mutation_counter` | canonical | idempotency | — |
| 7 | `ROUTER_MUTATION_BY_CLIENT_KEY` | `ROUTER_MUTATION_BY_CLIENT_KEY` | `init_mutation_by_client_key` | canonical | idempotency | keys use **`graph_id: GraphId`**; bounded by [ADR 0025](../adr/0025-client-mutation-journal-retention-sweep.md): completed records are compacted (heavy fields dropped, **E**), and expired records (`created_at_ns` + `CLIENT_MUTATION_KEY_TTL_NS` 7d) are evicted automatically by amortized write-path GC (**B**, heap round-robin cursor) plus the operator backstop `admin_sweep_expired_client_mutation_keys`. `created_at_ns` is the sole age SSOT; GC cursor is ephemeral heap |
| 8 | `ROUTER_PREPARED_PLANS` | `ROUTER_PREPARED_PLANS` | `init_prepared_plans` | canonical | prepared queries | **`PreparedPlanKey → PreparedPlanRecord::V1`** |
| 9–10 | `ROUTER_VERTEX_LABEL_BY_NAME` / `ROUTER_VERTEX_LABEL_BY_ID` | `ROUTER_VERTEX_LABEL_CATALOG` | `init_vertex_label_catalog` | catalog | resolution | **`GraphScopedNameCatalog<VertexLabelId>`** — `(GraphId, name) ↔ id` ([ADR 0018](../adr/0018-graph-scoped-label-property-catalogs.md)) |
| 11–12 | `ROUTER_EDGE_LABEL_BY_NAME` / `ROUTER_EDGE_LABEL_BY_ID` | `ROUTER_EDGE_LABEL_CATALOG` | `init_edge_label_catalog` | catalog | resolution | **`GraphScopedNameCatalog<EdgeLabelId>`** (dense, capped) |
| 13–14 | `ROUTER_PROPERTY_BY_NAME` / `ROUTER_PROPERTY_BY_ID` | `ROUTER_PROPERTY_CATALOG` | `init_property_catalog` | catalog | resolution | **`GraphScopedNameCatalog<PropertyId>`** (dense) |
| 15–16 | `ROUTER_GRAPH_BY_NAME` / `ROUTER_GRAPH_BY_ID` | `ROUTER_GRAPH_CATALOG` | `init_graph_catalog` | catalog | resolution | Logical graph name ↔ **`GraphId`** ([ADR 0011](../adr/0011-gql-graph-resolution-and-catalog-scoping.md)) |
| 17–18 | `ROUTER_INDEX_NAME_BY_NAME` / `ROUTER_INDEX_NAME_BY_ID` | `ROUTER_INDEX_NAME_CATALOG` | `init_index_name_catalog` | catalog | resolution | Graph-scoped index name ↔ **`IndexNameId`** per `GraphId` |
| 19 | `ROUTER_NAMED_INDEXES` | `ROUTER_NAMED_INDEXES` | `init_named_indexes` | catalog | index DDL metadata | **`(GraphId, IndexNameId) → kind, property_id, label_id`** |
| 20 | `ROUTER_INDEXED_PROPERTY_SET` | `ROUTER_INDEXED_PROPERTY_SET` | `init_indexed_property_set` | catalog | index membership | **`(GraphId, kind, property_id)`** for planner + fan-out |
| 21 | `ROUTER_EDGE_PAYLOAD_PROFILES` | `ROUTER_EDGE_PAYLOAD_PROFILES` | `init_edge_payload_profiles` | catalog | edge payload schema | **`(GraphId, EdgeLabelId) → EdgePayloadProfile`** ([ADR 0008](../adr/0008-edge-payload-profile-router-ssot.md), [ADR 0018](../adr/0018-graph-scoped-label-property-catalogs.md)) |
| 22–23 | `ROUTER_GRAPH_TYPE_DEFINITIONS` / `ROUTER_GRAPH_SCHEMA_BINDINGS` | `ROUTER_GQL_GRAPH_CATALOG` | `init_gql_graph_catalog` | catalog | graph type catalog | type defs + **`GraphId` bindings** ([ADR 0013](../adr/0013-gql-graph-type-catalog-on-router.md)) |
| 24–25 | `ROUTER_GRAPH_TYPE_BY_NAME` / `ROUTER_GRAPH_TYPE_BY_ID` | `ROUTER_GRAPH_TYPE_CATALOG` | `init_graph_type_name_catalog` | catalog | resolution | Graph type name ↔ **`GraphTypeId`** ([ADR 0014](../adr/0014-graph-type-id-catalog-on-router.md)) |
| 26 | `ROUTER_VERTEX_LABEL_STATS` | `ROUTER_VERTEX_LABEL_STATS` | `init_vertex_label_stats` | telemetry | label telemetry | **`(GraphId, VertexLabelId) → LabelStats`** (event replay) |
| 27 | `ROUTER_EDGE_LABEL_STATS` | `ROUTER_EDGE_LABEL_STATS` | `init_edge_label_stats` | telemetry | label telemetry | **`(GraphId, EdgeLabelId) → LabelStats`** (event replay) |
| 28 | `ROUTER_VERTEX_LABEL_LIVE_BY_SHARD` | `ROUTER_VERTEX_LABEL_LIVE_BY_SHARD` | `init_vertex_label_live_by_shard` | telemetry | label telemetry | **`(GraphId, ShardId, VertexLabelId) → live_count`** |
| 29 | `ROUTER_EDGE_LABEL_LIVE_BY_SHARD` | `ROUTER_EDGE_LABEL_LIVE_BY_SHARD` | `init_edge_label_live_by_shard` | telemetry | label telemetry | **`(GraphId, ShardId, EdgeLabelId) → live_count`** |
| 30 | `ROUTER_LABEL_STATS_PROJECTION` | `ROUTER_LABEL_STATS_PROJECTION` | `init_label_stats_projection` | telemetry | label stats projection | **`GraphShardKey → applied_through_seq`** |
| 31 | `ROUTER_LABEL_BACKFILL_STATE` | `ROUTER_LABEL_BACKFILL_STATE` | `init_label_backfill_state` | maintenance | label backfill | **`GraphShardKey → BackfillShardState`** |
| 32 | `ROUTER_VERTEX_PROPERTY_BACKFILL_STATE` | `ROUTER_VERTEX_PROPERTY_BACKFILL_STATE` | `init_vertex_property_backfill_state` | maintenance | vertex property backfill | **`GraphShardKey → BackfillShardState`** |
| 33 | `ROUTER_EDGE_BACKFILL_STATE` | `ROUTER_EDGE_BACKFILL_STATE` | `init_edge_backfill_state` | maintenance | edge backfill | **`GraphShardKey → EdgeBackfillShardState`** |

Router **34 regions** total (0–33).

### Router ephemeral

| Symbol | Location | Role | Reopen behavior |
|--------|----------|------|-----------------|
| _(none beyond graph-index pending queues on other canisters)_ | — | — | — |

---

## Graph-index canister — stable regions

| MemoryId | Symbol | Thread-local | Init fn | Class | Owner domain | Rebuild |
|--------|--------|--------------|---------|-------|--------------|---------|
| 0 | `INDEX_ROUTER` | `INDEX_ROUTER` | `init_index_router` | canonical | router authorization | — |
| 1 | `INDEX_SHARD_CANISTER_BY_SHARD` | `INDEX_SHARD_CANISTER_CATALOG` | `init_index_shard_canister_catalog` | canonical | shard canister catalog | — |
| 2 | `INDEX_SHARD_BY_CANISTER` | `INDEX_SHARD_CANISTER_CATALOG` | `init_index_shard_canister_catalog` | canonical | shard canister catalog | — |
| 3 | `INDEX_OWNERSHIP_CONFIG` | `INDEX_OWNERSHIP_CONFIG` | `init_index_ownership_config` | canonical | graph ownership | Graph owner config (`graph_id`, `index_group_size`, `group_index`) for attach range checks (ADR 0019 S4) |
| 4 | `INDEX_VERTEX_POSTINGS` | `INDEX_VERTEX_POSTINGS` | `init_index_vertex_postings` | derived | vertex property postings | **Implemented:** `backfill_vertex_property_postings` + router `admin_vertex_property_backfill_step` |
| 5 | `INDEX_VERTEX_LABEL_POSTINGS` | `INDEX_VERTEX_LABEL_POSTINGS` | `init_index_vertex_label_postings` | derived | vertex label postings | `backfill_label_postings` |
| 6 | `INDEX_EDGE_POSTINGS` | `INDEX_EDGE_POSTINGS` | `init_index_edge_postings` | derived | edge property postings | **Implemented:** `backfill_edge_property_postings` (ADR 0009) |

---

## Related documents

- [Refactoring roadmap](../architecture/refactoring-roadmap.md) — phased plan; Phase 0 exit criteria
- [LARA and graph facade](./lara-and-facade.md) — layering; defers byte layout to this inventory
- [Property index](../index/property-index.md) — posting model and router seed routing
- [Label index](../index/label-index.md) — label postings and backfill orchestration
- [ADR 0004: Label index](../adr/0004-label-index.md)
