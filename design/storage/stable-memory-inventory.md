# Stable-memory inventory

Last updated: 2026-06-11  
Status: Implemented (pre-federation repack per ADR 0006 slice D)  
Anchor timestamp: 2026-06-11 14:21:44 UTC +0000

## Purpose

Single inventory of stable-memory regions and heap-only facade state for the graph, router, and graph-index canisters. Each row names the owning domain, classification, and rebuild path where one exists.

Code source of truth for `MemoryId` constants:

- `crates/graph/src/facade/stable/memory.rs`
- `crates/router/src/facade/stable/memory.rs`
- `crates/graph-index/src/facade/stable/memory.rs`

Thread-local pairing: `facade/stable.rs` in each crate.

## Classifications

| Class | Meaning |
|-------|---------|
| `canonical` | Authoritative state; recoverable without consulting derived stores |
| `derived` | Rebuildable or optimizable from canonical state |
| `maintenance` | PMA/LARA deferred work, free spans, or operational cursors |
| `catalog` | Bidirectional name/id maps |
| `telemetry` | Aggregates derived from graph shard events |
| `compatibility` | Legacy or transitional view over another store |
| `ephemeral` | Heap-only; lost on canister upgrade |

**Sync co-update:** Some derived stores are updated in the same mutation as their canonical source (no async lag). They still have a separate physical region and are classified `derived`.

**Query semantics when derived state lags:** [derived-state-query-semantics.md](../index/derived-state-query-semantics.md).

## Derived-state rebuild summary

| Derived store | Canonical source | Update path | Rebuild / backfill |
|---------------|------------------|-------------|-------------------|
| LARA reverse orientation | Forward edges + payloads | Co-updated on edge insert/delete | No standalone API; theoretical full-graph scan |
| Edge aliases | Forward/reverse adjacency in `GRAPH` | Sync: `commit_insert_edge_alias` on edge insert | **Implemented:** `check_edge_aliases` + `rebuild_edge_aliases` (`facade/derived_state/edge_alias.rs`) |
| Edge equality postings | Edge properties | Sync: `dispatch_property_index_ops` on DML | **Implemented:** `check_edge_equality_postings` + `rebuild_edge_equality_postings` (`facade/derived_state/edge_equality.rs`) |
| Property postings (graph-index) | Vertex properties (indexable) | DML + `pending.rs` flush | **Implemented:** `backfill_property_postings` + router `admin_property_backfill_step` |
| Label postings (graph-index) | `VertexLabelStore` | DML + `label_pending` flush | **Implemented:** `backfill_label_postings` + router `admin_label_backfill_step` ([label-index.md](../index/label-index.md)) |
| Remote forward-in | Remote forward edges | Register/insert paths | Scan fallback per [federation/operations.md](../federation/operations.md) |
| Router label telemetry | Graph `LabelUsageDelta` | Event apply + `ROUTER_APPLIED_LABEL_TELEMETRY` dedup | **Implemented:** graph outbox replay via `admin_label_telemetry_replay_step`; no full historical scan |
| Router indexed-property catalog | Property catalog + planner stats | Planner registration | **Ephemeral** — rebuilt after upgrade |

---

## Graph canister — LARA bundle

`init_graph()` wires 30 `MemoryId` regions into one `DeferredBidirectionalLabeledLaraGraph`. Thread-local: `GRAPH`.

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
| 42 | `FWD_PAYLOAD_SLAB` | Labeled edge payload slab | canonical | — |
| 45 | `FWD_PAYLOAD_FREE_SPANS` | Payload free spans | maintenance | — |
| 46 | `FWD_PAYLOAD_FREE_SPAN_BY_START` | Payload free-span index | maintenance | — |
| 49 | `FWD_PAYLOAD_LOG` | Payload value log | canonical | — |
| 57 | `FWD_PAYLOAD_BLOBS` | Large payload blobs | canonical | — |

### Reverse orientation (derived adjacency + payloads)

| MemoryId | Symbol | Role | Class | Rebuild |
|--------|--------|------|-------|---------|
| 10 | `REV_VERTICES` | Reverse vertex rows | derived | Sync co-update; no scan API |
| 11 | `REV_BUCKETS` | Reverse buckets | derived | Sync co-update |
| 12–13 | `REV_BUCKET_FREE_SPANS`, `REV_BUCKET_FREE_SPAN_BY_START` | Reverse bucket maintenance | maintenance | — |
| 14 | `REV_EDGE_COUNTS` | Reverse edge counts | derived | Sync co-update |
| 15 | `REV_EDGES` | Reverse edge slab | derived | Sync co-update |
| 16 | `REV_EDGE_LOG` | Reverse edge log | derived | Sync co-update |
| 17–18 | `REV_EDGE_SPAN_META`, `REV_EDGE_FREE_SPANS`, `REV_EDGE_FREE_SPAN_BY_START` | Reverse edge maintenance | maintenance | — |
| 43 | `REV_PAYLOAD_SLAB` | Reverse payload slab | derived | Sync co-update |
| 47–48 | `REV_PAYLOAD_FREE_SPANS`, `REV_PAYLOAD_FREE_SPAN_BY_START` | Reverse payload maintenance | maintenance | — |
| 50 | `REV_PAYLOAD_LOG` | Reverse payload log | derived | Sync co-update |
| 58 | `REV_PAYLOAD_BLOBS` | Reverse payload blobs | derived | Sync co-update |

### LARA maintenance

| MemoryId | Symbol | Role | Class | Rebuild |
|--------|--------|------|-------|---------|
| 20 | `MAINTENANCE_QUEUE` | Deferred PMA work queue | maintenance | Internal LARA drain |
| 21 | `DIRTY_WORK_ITEMS` | Dirty work tracking | maintenance | Internal LARA drain |

Owner: `ic-stable-lara` / graph `GRAPH` thread-local. Scan paths must not consult PMA maintenance stores ([lara.md](./lara.md)).

---

## Graph canister — facade regions

Repacked 2026-06-11 (ADR 0006 slice D). **Removed:** property name catalog (25–26), `VERTEX_LOGICAL_IDS` (36). LARA bundle ids (0–21, 42–58) unchanged.

| MemoryId | Symbol | Thread-local | Init fn | Class | Owner domain | Rebuild |
|--------|--------|--------------|---------|-------|--------------|---------|
| 22 | `VERTEX_LABEL_SETS` | `VERTEX_LABELS` | `init_vertex_label_store` | canonical | labels | — |
| 23 | `VERTEX_PROPERTIES` | `VERTEX_PROPERTIES` | `init_vertex_property_store` | canonical | properties | — |
| 24 | `EDGE_PROPERTIES` | `EDGE_PROPERTIES` | `init_edge_property_store` | canonical | properties | — |
| 25 | `EDGE_ALIASES` | `EDGE_ALIASES` | `init_edge_alias_index` | derived | adjacency | `check_edge_aliases` / `rebuild_edge_aliases` |
| 26 | `GRAPH_METADATA` | `METADATA` | `init_metadata` | canonical | federation metadata | — |
| 27 | `EDGE_WEIGHT_PROFILES` | `EDGE_WEIGHT_PROFILES` | `init_edge_weight_profiles` | compatibility (legacy read fallback) | edge profiles | — |
| 28 | `EDGE_PAYLOAD_PROFILES` | `EDGE_PAYLOAD_PROFILES` | `init_edge_payload_profiles` | canonical | edge profiles | — |
| 29 | `EDGE_EQUALITY_POSTINGS` | `EDGE_EQUALITY_POSTINGS` | `init_edge_equality_postings` | derived | local indexes | `check_edge_equality_postings` / `rebuild_edge_equality_postings` |
| 30 | `REMOTE_REF_TO_VERTEX` | `REMOTE_VERTEX_REFS` | `init_remote_vertex_refs` | canonical | remote refs (deferred prod) | — |
| 31 | `VERTEX_TO_REMOTE_REF` | `REMOTE_VERTEX_REFS` | `init_remote_vertex_refs` | canonical | remote refs (deferred prod) | — |
| 32 | `REMOTE_FORWARD_IN` | `REMOTE_FORWARD_IN` | `init_remote_forward_in` | derived | remote refs (deferred prod) | Scan fallback |
| 33 | `PEER_GRAPH_CANISTERS` | `PEER_GRAPH_CANISTERS` | `init_peer_graph_canisters` | canonical | federation peers (deferred prod) | — |
| 34 | `LABEL_TELEMETRY_SEQ` | `LABEL_TELEMETRY_SEQ` | `init_label_telemetry_seq` | telemetry | label telemetry | — |
| 35 | `LABEL_TELEMETRY_OUTBOX` | `LABEL_TELEMETRY_OUTBOX` | `init_label_telemetry_outbox` | telemetry | label telemetry | Event replay to router |
| 36 | `APPLIED_MUTATION_REQUESTS` | `APPLIED_MUTATION_REQUESTS` | `init_applied_mutation_requests` | canonical | idempotency | — |

Property **names** are router-owned (`ROUTER_PROPERTY_CATALOG`); graph stores values by `PropertyId` only.

### Graph ephemeral (not in `memory.rs`)

| Symbol | Location | Role | Reopen behavior |
|--------|----------|------|-----------------|
| `PENDING` (property postings) | `graph/src/index/pending.rs` | Queued property index ops | Lost on upgrade; `backfill_property_postings` covers historical vertex properties |
| `PENDING` (label postings) | `graph/src/index/label_pending.rs` | Queued label index ops | Lost on upgrade; `backfill_label_postings` covers historical labels |

---

## Router canister — stable regions

Repacked 2026-06-11 (ADR 0006 slice D). **Removed:** `ROUTER_LOGICAL_COUNTER` (5), `ROUTER_PENDING_LOGICAL` (6), `ROUTER_PLACEMENT_BY_PHYSICAL` (13). `ROUTER_PLACEMENTS` keyed by `GlobalVertexId`.

| MemoryId | Symbol | Thread-local | Init fn | Class | Owner domain | Rebuild |
|--------|--------|--------------|---------|-------|--------------|---------|
| 0 | `ROUTER_CONTROLLERS` | `ROUTER_CONTROLLERS` | `init_controllers` | canonical | auth | — |
| 1 | `ROUTER_GRAPHS` | `ROUTER_GRAPHS` | `init_graphs` | canonical | registry | — |
| 2 | `ROUTER_SHARDS` | `ROUTER_SHARDS` | `init_shards` | canonical | registry | — |
| 3 | `ROUTER_SHARD_BY_GRAPH` | `ROUTER_SHARD_BY_GRAPH` | `init_shard_by_graph` | canonical | registry | — |
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
| 18 | `ROUTER_MUTATION_BY_CLIENT_KEY` | `ROUTER_MUTATION_BY_CLIENT_KEY` | `init_mutation_by_client_key` | canonical | idempotency | — |
| 19 | `ROUTER_LABEL_BACKFILL_STATE` | `ROUTER_LABEL_BACKFILL_STATE` | `init_label_backfill_state` | maintenance | label backfill | Cursor for `admin_label_backfill_step` |
| 20 | `ROUTER_PROPERTY_BACKFILL_STATE` | `ROUTER_PROPERTY_BACKFILL_STATE` | `init_property_backfill_state` | maintenance | property backfill | Cursor for `admin_property_backfill_step` |

### Router ephemeral

| Symbol | Location | Role | Reopen behavior |
|--------|----------|------|-----------------|
| `ROUTER_INDEXED_PROPERTIES` | `router/src/facade/stable.rs` | Per-graph indexed-property planner catalog | Lost on upgrade; re-register via planner |
| `ROUTER_PREPARED_PLANS` | `router/src/facade/stable.rs` | Cached prepared plan blobs | Lost on upgrade; re-prepare on demand |

---

## Graph-index canister — stable regions

| MemoryId | Symbol | Thread-local | Init fn | Class | Owner domain | Rebuild |
|--------|--------|--------------|---------|-------|--------------|---------|
| 0 | `INDEX_ADMINS` | `INDEX_ADMINS` | `init_index_admins` | canonical | authorization | — |
| 1 | `INDEX_SHARD_OWNERS` | `INDEX_SHARD_OWNERS` | `init_index_shard_owners` | canonical | shard ownership | — |
| 2 | `INDEX_POSTINGS` | `INDEX_POSTINGS` | `init_index_postings` | derived | property postings | **Not implemented** |
| 3 | `INDEX_ROUTER` | `INDEX_ROUTER` | `init_index_router` | canonical | router authorization | — |
| 4 | `INDEX_LABEL_POSTINGS` | `INDEX_LABEL_POSTINGS` | `init_index_label_postings` | derived | label postings | `backfill_label_postings` |

---

## Related documents

- [Refactoring roadmap](../architecture/refactoring-roadmap.md) — phased plan; Phase 0 exit criteria
- [LARA and graph facade](./lara-and-facade.md) — layering; defers byte layout to this inventory
- [Property index](../index/property-index.md) — posting model and router seed routing
- [Label index](../index/label-index.md) — label postings and backfill orchestration
- [ADR 0004: Label index](../adr/0004-label-index.md)
