# Stable-memory inventory

Last updated: 2026-06-13  
Status: Implemented (sequential LARA MemoryIds 0–31; facade 32–42; router repack ADR 0011)  
Anchor timestamp: 2026-06-13 07:36:35 UTC +0000

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

## Classifications

Authoritative definitions and Gleaph examples: `gleaph_graph_kernel::stable_layout::StableMemoryClass`
(rustdoc on each variant). Per-region class and functional role: `GRAPH_STABLE_LAYOUT`,
`ROUTER_STABLE_LAYOUT`, `INDEX_STABLE_LAYOUT`.

| Class | Meaning | Examples in this repo |
|-------|---------|------------------------|
| `canonical` | Authoritative facts; system meaning does not depend on derived stores | Forward LARA CSR/payloads; vertex/edge properties; router placement and catalogs; mutation idempotency |
| `derived` | Projection or mirror rebuildable from canonical state | Reverse LARA; edge aliases/equality postings; graph-index postings |
| `maintenance` | Physical or admin bookkeeping; not query truth | LARA free spans; maintenance queue; router backfill cursors |
| `catalog` | Bidirectional name ↔ id maps (`BidirectionalCatalog`) | Router label/property/graph/index-name resolution pairs |
| `telemetry` | Event-sourced label stats and dedup/outbox adjuncts | Graph label outbox; router label stats and `ROUTER_APPLIED_LABEL_TELEMETRY` |
| `compatibility` | Legacy read view; another store owns new writes | *(none — P1 `EDGE_WEIGHT_PROFILES` retired 2026-06-12)* |
| `ephemeral` | Heap-only; no `MemoryId` — **not in layout registry** | Graph `PENDING` queues; router planner catalog |

**Sync co-update:** Some derived stores are updated in the same mutation as their canonical source (no async lag). They still have a separate physical region and are classified `derived`.

**Query semantics when derived state lags:** [derived-state-query-semantics.md](../index/derived-state-query-semantics.md).

## Derived-state rebuild summary

| Derived store | Canonical source | Update path | Rebuild / backfill |
|---------------|------------------|-------------|-------------------|
| LARA reverse orientation | Forward edges + payloads | Co-updated on edge insert/delete | No standalone API; theoretical full-graph scan |
| Edge aliases | Forward/reverse adjacency in `GRAPH` | Sync: `commit_insert_edge_alias` on edge insert | **Implemented:** `check_edge_aliases` + `rebuild_edge_aliases` (`facade/derived_state/edge_alias.rs`) |
| Edge property postings (graph-index) | `EDGE_PROPERTIES` (registered props) | DML + `edge_pending` flush | **Implemented:** `backfill_edge_property_postings` ([ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md); retired shard-local `EDGE_EQUALITY_POSTINGS` 2026-06-12) |
| Property postings (graph-index) | Vertex properties (indexable) | DML + `pending.rs` flush | **Implemented:** `backfill_property_postings` + router `admin_property_backfill_step` |
| Label postings (graph-index) | `VertexLabelStore` | DML + `label_pending` flush | **Implemented:** `backfill_label_postings` + router `admin_label_backfill_step` ([label-index.md](../index/label-index.md)) |
| Router label telemetry | Graph `LabelUsageDelta` | Event apply + `ROUTER_APPLIED_LABEL_TELEMETRY` dedup | **Implemented:** graph outbox replay via `admin_label_telemetry_replay_step`; no full historical scan |
| Router indexed-property catalog | Property catalog + planner stats | Planner registration | **Stable** — row layout MemoryId 22–23 |

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
| 15 | `REV_VERTICES` | Reverse vertex rows | derived | Sync co-update; no scan API |
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
| 37 | `LABEL_TELEMETRY_SEQ` | `LABEL_TELEMETRY_SEQ` | `init_label_telemetry_seq` | telemetry | label telemetry | — |
| 38 | `LABEL_TELEMETRY_OUTBOX` | `LABEL_TELEMETRY_OUTBOX` | `init_label_telemetry_outbox` | telemetry | label telemetry | Event replay to router |
| 39 | `APPLIED_MUTATION_REQUESTS` | `APPLIED_MUTATION_REQUESTS` | `init_applied_mutation_requests` | canonical | idempotency | — |

Graph facade **40 regions** total (32 LARA + 8 facade). Retired 2026-06-12: `EDGE_PAYLOAD_PROFILES` → router SSOT ([ADR 0008](../adr/0008-edge-payload-profile-router-ssot.md)); `EDGE_EQUALITY_POSTINGS` → graph-index ([ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md)).

Property **names** are router-owned (`ROUTER_PROPERTY_CATALOG`); graph stores values by `PropertyId` only.

### Graph ephemeral (not in `memory.rs`)

| Symbol | Location | Role | Reopen behavior |
|--------|----------|------|-----------------|
| `PENDING` (property postings) | `graph/src/index/pending.rs` | Queued property index ops | Lost on upgrade; `backfill_property_postings` covers historical vertex properties |
| `PENDING` (label postings) | `graph/src/index/label_pending.rs` | Queued label index ops | Lost on upgrade; `backfill_label_postings` covers historical labels |

---

## Router canister — stable regions

Repacked 2026-06-13 (ADR 0011 graph/index name catalogs). Prior repack 2026-06-11 (ADR 0006 slice D). **Removed:** `ROUTER_LOGICAL_COUNTER` (5), `ROUTER_PENDING_LOGICAL` (6), `ROUTER_PLACEMENT_BY_PHYSICAL` (13). `ROUTER_PLACEMENTS` keyed by `GlobalVertexId`. `ROUTER_GRAPHS` keyed by **`GraphId`** (not graph name string); values are **`GraphRegistryEntry`** including **`is_home: bool`** for HOME resolution (ADR 0011 §1.3 B). `ShardRegistryEntry` stores **`graph_id: GraphId`**. `ROUTER_SHARD_BY_GRAPH` remains **`Principal → ShardId`** (canister uniqueness); shard listing per logical graph uses **`ROUTER_SHARDS_BY_GRAPH_ID`**.

| MemoryId | Symbol | Thread-local | Init fn | Class | Owner domain | Rebuild |
|--------|--------|--------------|---------|-------|--------------|---------|
| 0 | `ROUTER_CONTROLLERS` | `ROUTER_CONTROLLERS` | `init_controllers` | canonical | auth | — |
| 1 | `ROUTER_GRAPHS` | `ROUTER_GRAPHS` | `init_graphs` | canonical | registry | **`BTreeMap<GraphId, GraphRegistryEntry>`** |
| 2 | `ROUTER_SHARDS` | `ROUTER_SHARDS` | `init_shards` | canonical | registry | values include `graph_id: GraphId` |
| 3 | `ROUTER_SHARD_BY_GRAPH` | `ROUTER_SHARD_BY_GRAPH` | `init_shard_by_graph` | canonical | registry | **`Principal → ShardId`** (not logical graph name) |
| 4 | `ROUTER_PLACEMENTS` | `ROUTER_PLACEMENTS` | `init_placements` | canonical | placement (`GlobalVertexId`) | — |
| 5–6 | `ROUTER_VERTEX_LABEL_BY_NAME` / `ROUTER_VERTEX_LABEL_BY_ID` | `ROUTER_VERTEX_LABEL_CATALOG` | `init_vertex_label_catalog` | catalog | resolution | `BidirectionalCatalog` (dense) |
| 7–8 | `ROUTER_EDGE_LABEL_BY_NAME` / `ROUTER_EDGE_LABEL_BY_ID` | `ROUTER_EDGE_LABEL_CATALOG` | `init_edge_label_catalog` | catalog | resolution | `BidirectionalCatalog` (dense, capped) |
| 9–10 | `ROUTER_PROPERTY_BY_NAME` / `ROUTER_PROPERTY_BY_ID` | `ROUTER_PROPERTY_CATALOG` | `init_property_catalog` | catalog | resolution | `BidirectionalCatalog` (dense) |
| 11 | `ROUTER_AUTH_PRINCIPAL_RECORDS` | `ROUTER_AUTH_STATE` | `init_auth_state` | canonical | auth | — |
| 12 | `ROUTER_VERTEX_LABEL_STATS` | `ROUTER_VERTEX_LABEL_STATS` | `init_vertex_label_stats` | telemetry | label telemetry | Event replay |
| 13 | `ROUTER_EDGE_LABEL_STATS` | `ROUTER_EDGE_LABEL_STATS` | `init_edge_label_stats` | telemetry | label telemetry | Event replay |
| 14 | `ROUTER_VERTEX_LABEL_LIVE_BY_SHARD` | `ROUTER_VERTEX_LABEL_LIVE_BY_SHARD` | `init_vertex_label_live_by_shard` | telemetry | label telemetry | Event replay |
| 15 | `ROUTER_EDGE_LABEL_LIVE_BY_SHARD` | `ROUTER_EDGE_LABEL_LIVE_BY_SHARD` | `init_edge_label_live_by_shard` | telemetry | label telemetry | Event replay |
| 16 | `ROUTER_MUTATION_COUNTER` | `ROUTER_MUTATION_COUNTER` | `init_mutation_counter` | canonical | idempotency | — |
| 17 | `ROUTER_APPLIED_LABEL_TELEMETRY` | `ROUTER_APPLIED_LABEL_TELEMETRY` | `init_applied_label_telemetry` | telemetry | label telemetry | Dedup set for replay |
| 18 | `ROUTER_MUTATION_BY_CLIENT_KEY` | `ROUTER_MUTATION_BY_CLIENT_KEY` | `init_mutation_by_client_key` | canonical | idempotency | keys use **`graph_id: GraphId`** |
| 19 | `ROUTER_LABEL_BACKFILL_STATE` | `ROUTER_LABEL_BACKFILL_STATE` | `init_label_backfill_state` | maintenance | label backfill | Cursor for `admin_label_backfill_step` |
| 20 | `ROUTER_PROPERTY_BACKFILL_STATE` | `ROUTER_PROPERTY_BACKFILL_STATE` | `init_property_backfill_state` | maintenance | property backfill | Cursor for `admin_property_backfill_step` |
| 21 | `ROUTER_EDGE_PAYLOAD_PROFILES` | `ROUTER_EDGE_PAYLOAD_PROFILES` | `init_edge_payload_profiles` | catalog | edge payload schema | — ([ADR 0008](../adr/0008-edge-payload-profile-router-ssot.md)) |
| 22 | `ROUTER_NAMED_INDEXES` | `ROUTER_NAMED_INDEXES` | `init_named_indexes` | catalog | index DDL metadata | **`(GraphId, IndexNameId) → kind, property_id, label_id`** |
| 23 | `ROUTER_INDEXED_PROPERTY_SET` | `ROUTER_INDEXED_PROPERTY_SET` | `init_indexed_property_set` | catalog | index membership | **`(GraphId, kind, property_id)`** for planner + fan-out |
| 24–25 | `ROUTER_GRAPH_BY_NAME` / `ROUTER_GRAPH_BY_ID` | `ROUTER_GRAPH_CATALOG` | `init_graph_catalog` | catalog | resolution | Logical graph name ↔ **`GraphId`** ([ADR 0011](../adr/0011-gql-graph-resolution-and-catalog-scoping.md)) |
| 26–27 | `ROUTER_INDEX_NAME_BY_NAME` / `ROUTER_INDEX_NAME_BY_ID` | `ROUTER_INDEX_NAME_CATALOG` | `init_index_name_catalog` | catalog | resolution | Graph-scoped index name ↔ **`IndexNameId`** per `GraphId` |
| 28 | `ROUTER_SHARDS_BY_GRAPH_ID` | `ROUTER_SHARDS_BY_GRAPH_ID` | `init_shards_by_graph_id` | canonical | registry | **`GraphId → Vec<ShardId>`** shard index |
| 29 | `ROUTER_PREPARED_PLANS` | `ROUTER_PREPARED_PLANS` | `init_prepared_plans` | canonical | prepared queries | **`PreparedPlanKey → PreparedPlanRecord::V1`**; survives upgrade |

Router **30 regions** total (0–29).

### Router ephemeral

| Symbol | Location | Role | Reopen behavior |
|--------|----------|------|-----------------|
| _(none beyond graph-index pending queues on other canisters)_ | — | — | — |

---

## Graph-index canister — stable regions

| MemoryId | Symbol | Thread-local | Init fn | Class | Owner domain | Rebuild |
|--------|--------|--------------|---------|-------|--------------|---------|
| 0 | `INDEX_ADMINS` | `INDEX_ADMINS` | `init_index_admins` | canonical | authorization | — |
| 1 | `INDEX_SHARD_OWNERS` | `INDEX_SHARD_OWNERS` | `init_index_shard_owners` | canonical | shard ownership | — |
| 2 | `INDEX_POSTINGS` | `INDEX_POSTINGS` | `init_index_postings` | derived | property postings | **Implemented:** `backfill_property_postings` + router `admin_property_backfill_step` |
| 3 | `INDEX_ROUTER` | `INDEX_ROUTER` | `init_index_router` | canonical | router authorization | — |
| 4 | `INDEX_LABEL_POSTINGS` | `INDEX_LABEL_POSTINGS` | `init_index_label_postings` | derived | label postings | `backfill_label_postings` |
| 5 | `INDEX_EDGE_POSTINGS` | `INDEX_EDGE_POSTINGS` | `init_index_edge_postings` | derived | edge property postings | **Implemented:** `backfill_edge_property_postings` (ADR 0009) |

---

## Related documents

- [Refactoring roadmap](../architecture/refactoring-roadmap.md) — phased plan; Phase 0 exit criteria
- [LARA and graph facade](./lara-and-facade.md) — layering; defers byte layout to this inventory
- [Property index](../index/property-index.md) — posting model and router seed routing
- [Label index](../index/label-index.md) — label postings and backfill orchestration
- [ADR 0004: Label index](../adr/0004-label-index.md)
