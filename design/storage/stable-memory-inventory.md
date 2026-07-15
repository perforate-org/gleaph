# Stable-memory inventory

Last updated: 2026-07-15
Status: Partially Implemented (graph: sequential LARA MemoryIds 0–31 + facade 32–46 = 47 regions, incl. ADR 0030 unique-effect outbox + slice-10 shard-local unique values + ADR 0031 canonical vertex embeddings + Slice 4 embedding incarnations + Plan 0088 durable derived-index outbox storage; router repack ADR 0011/0018/0019 + ADR 0030 constraint catalog + reservation table + slice-6 reverse index + pending-effect discovery index + ADR 0031 Slice 3 embedding-name catalog + vector-index definition catalog + Slice 4 vector dispatch activation flag + Slice 10 vector maintenance policy catalog + ADR 0034 Slice 20 + Slice 24 edge inline value schema record + ADR 0035 Slice 1 provisioning-request catalog + Slice 5 Router outbound accept_envelope send (ROUTER_PROVISION_CONFIG durable binding) + Slice 6 owner-identity-bound intent lock release on Completed and four-branch invocation-owned rollback on send failure (only if current operation inserted the record and it is still AwaitingAck) (no new regions) (development stable data must be wiped when this format changes because backward compatibility is not maintained) = 49 regions, 0–48; graph-vector-index: ADR 0031 Slice 2 + Slice 6 reverse subject map + Slice 7 rebuild state + ADR 0032 slab page store + Slice 10 maintenance scan state = 15 regions, 0–14; provision: ADR 0035 Slice 2 + Slice 4 callable canister endpoints + Slice 7 durable bootstrap authority singleton (MemoryId 4) and per-governance audit log (MemoryId 5) + ADR 0036 Slice 8a artifact catalog (MemoryId 6), upload state (MemoryId 7), verified chunk bytes (MemoryId 8) + Slice 8b release manifest (MemoryId 9) and active release pointer (MemoryId 10) + Slice 8c artifact audit log (MemoryId 11) = 12 regions, 0–11)
Anchor timestamp: 2026-07-14 01:17:34 UTC +0000

Layout change policy: [ADR 0007](../adr/0007-stable-memory-layout.md).
Planned production compatibility and migration policy: [ADR 0039](../adr/0039-production-stable-memory-evolution-and-upgrade-safety.md).

## Purpose

Single inventory of stable-memory regions and heap-only facade state for the graph, router, graph-index, and graph-vector-index canisters. Each row names the owning domain, classification, and rebuild path where one exists.

Code source of truth for runtime `MemoryId` constants:

- `crates/graph/src/facade/stable/memory.rs`
- `crates/router/src/facade/stable/memory.rs`
- `crates/graph-index/src/facade/stable/memory.rs`
- `crates/graph-vector-index/src/facade/stable/memory.rs`

Typed layout registry (descriptive mirror + validation tests): `gleaph_graph_kernel::stable_layout`
and per-canister `facade/stable/layout.rs` — [ADR 0007](../adr/0007-stable-memory-layout.md) §7.

Thread-local pairing: `facade/stable.rs` in each crate.

### Region-count doc-sync checklist

Region counts and per-region classes in this document mirror the typed registry, which is the
mechanical source of truth. The registry tests enforce the counts below; when they change, update
this document and [ADR 0007](../adr/0007-stable-memory-layout.md) in the same patch:

| Canister | Regions | Id range | Registry constant + test |
|----------|---------|----------|--------------------------|
| Graph | 47 | 0–46 | `GRAPH_STABLE_LAYOUT` — `graph_layout_registry_matches_baseline` |
| Router | 49 | 0–48 | `ROUTER_STABLE_LAYOUT` — `router_layout_registry_matches_baseline` |
| Graph-index | 7 | 0–6 | `INDEX_STABLE_LAYOUT` — `index_layout_registry_matches_baseline` |
| Graph-vector-index | 15 | 0–14 | `VECTOR_INDEX_STABLE_LAYOUT` — `vector_index_layout_registry_matches_baseline` |
| Provision | 12 | 0–11 | `PROVISION_STABLE_LAYOUT` — `provision_layout_registry_matches_baseline` |

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
| LARA reverse orientation | Forward edges + payloads | Co-updated on edge insert/delete | **Implemented:** `check_reverse_adjacency` + `rebuild_reverse_adjacency` (`facade/derived_state/reverse_adjacency.rs`). The rebuild is a **differential** per-diverged-key reconcile (ADR 0026), not a full clear-and-rebuild — a full rebuild would reassign reverse slot indices, cascade-invalidating `EDGE_ALIASES` keys + reverse payload sidecars. |
| Edge aliases | Forward/reverse adjacency in `GRAPH` | Sync: `commit_insert_edge_alias` on edge insert | **Implemented:** `check_edge_aliases` + `rebuild_edge_aliases` (`facade/derived_state/edge_alias.rs`) |
| Edge property postings (graph-index) | `EDGE_PROPERTIES` (registered props) | DML + `edge_pending` flush | **Implemented:** `backfill_edge_property_postings` + router `admin_edge_backfill_step` ([ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md); retired shard-local `EDGE_EQUALITY_POSTINGS` 2026-06-12) |
| Vertex property postings (graph-index) | Vertex properties (indexable) | DML + `pending.rs` flush | **Implemented:** `backfill_vertex_property_postings` + router `admin_vertex_property_backfill_step` |
| Label postings (graph-index) | `VertexLabelStore` | DML + `label_pending` flush | **Implemented:** `backfill_label_postings` + router `admin_label_backfill_step` ([label-index.md](../index/label-index.md)) |
| Vector-index entries (graph-vector-index) | `VERTEX_EMBEDDINGS` + `VERTEX_EMBEDDING_INCARNATIONS` (canonical embeddings + incarnation fence) | DML + `vector_pending` flush | **Slice 4:** dispatch activated behind the incarnation fence + Router stable flag; **Slice 5:** bounded `admin_vector_index_backfill_step` driver (router → `graph_client::backfill_vertex_embeddings` → graph worker) ([vector-index.md](../index/vector-index.md), [ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md)) |
| Router label stats projection | Graph `LabelStatsDelta` | `advance_label_stats_projection` + per-shard cursor | **Implemented:** graph delta log replay via `admin_label_stats_projection_step`; no full historical scan |
| Router indexed-property catalog | Property catalog + planner stats | Planner registration | **Stable** — row layout MemoryId 18–19 |

---

## Graph canister — LARA bundle

`init_graph()` wires **32** consecutive `MemoryId` regions (0–31) into one `DeferredBidirectionalLabeledLaraGraph`. Thread-local: `GRAPH`.

### Forward orientation (canonical adjacency + payloads)

| MemoryId | Symbol | Role | Class | Rebuild |
|--------|--------|------|-------|---------|
| 0 | `FWD_VERTICES` | Vertex rows | canonical | — |
| 1 | `FWD_BUCKETS` | Per-vertex labeled edge buckets: edge slab/log locator plus independent payload slab/log split metadata | canonical | — |
| 2 | `FWD_BUCKET_FREE_SPANS` | Retired bucket physical spans | maintenance | — |
| 3 | `FWD_BUCKET_FREE_SPAN_BY_START` | Bucket free-span index | maintenance | — |
| 4 | `FWD_EDGE_COUNTS` | Per-vertex edge counts | canonical | — |
| 5 | `FWD_EDGES` | Edge slab | canonical | — |
| 6 | `FWD_EDGE_LOG` | Edge value log | canonical | — |
| 7 | `FWD_EDGE_SPAN_META` | Edge span metadata | maintenance | — |
| 8 | `FWD_EDGE_FREE_SPANS` | Retired edge physical spans | maintenance | — |
| 9 | `FWD_EDGE_FREE_SPAN_BY_START` | Edge free-span index | maintenance | — |
| 10 | `FWD_PAYLOAD_SLAB` | Dense labeled edge inline-value prefix, independently allocated/relocated from edge slab | canonical | — |
| 11 | `FWD_PAYLOAD_FREE_SPANS` | Payload free spans | maintenance | — |
| 12 | `FWD_PAYLOAD_FREE_SPAN_BY_START` | Payload free-span index | maintenance | — |
| 13 | `FWD_PAYLOAD_LOG` | Ordered inline-value suffix log, independently folded from edge log | canonical | — |
| 14 | `FWD_PAYLOAD_BLOBS` | Large payload blobs | canonical | — |

### Reverse orientation (derived adjacency + payloads)

| MemoryId | Symbol | Role | Class | Rebuild |
|--------|--------|------|-------|---------|
| 15 | `REV_VERTICES` | Reverse vertex rows | derived | Co-update + `rebuild_reverse_adjacency`; `check_reverse_adjacency` oracle |
| 16 | `REV_BUCKETS` | Reverse buckets with independent edge/payload slab-log split metadata | derived | Co-update + `rebuild_reverse_adjacency` |
| 17–18 | `REV_BUCKET_FREE_SPANS`, `REV_BUCKET_FREE_SPAN_BY_START` | Reverse bucket maintenance | maintenance | — |
| 19 | `REV_EDGE_COUNTS` | Reverse edge counts | derived | Co-update + `rebuild_reverse_adjacency` |
| 20 | `REV_EDGES` | Reverse edge slab | derived | Co-update + `rebuild_reverse_adjacency` |
| 21 | `REV_EDGE_LOG` | Reverse edge log | derived | Co-update + `rebuild_reverse_adjacency` |
| 22–24 | `REV_EDGE_SPAN_META`, `REV_EDGE_FREE_SPANS`, `REV_EDGE_FREE_SPAN_BY_START` | Reverse edge maintenance | maintenance | — |
| 25 | `REV_PAYLOAD_SLAB` | Reverse inline-value prefix, independently allocated/relocated from reverse edge slab | derived | Co-update + `rebuild_reverse_adjacency` |
| 26–27 | `REV_PAYLOAD_FREE_SPANS`, `REV_PAYLOAD_FREE_SPAN_BY_START` | Reverse payload maintenance | maintenance | — |
| 28 | `REV_PAYLOAD_LOG` | Reverse ordered inline-value suffix log, independently folded from reverse edge log | derived | Co-update + `rebuild_reverse_adjacency` |
| 29 | `REV_PAYLOAD_BLOBS` | Reverse payload blobs | derived | Co-update + `rebuild_reverse_adjacency` |

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
| 39 | `GRAPH_MUTATION_JOURNAL` | `GRAPH_MUTATION_JOURNAL` | `init_graph_mutation_journal` | canonical | idempotency | Mutation outcome + emitted delta seq range. A single-DML mutation is recorded `Completed` even when its index flush is deferred to the repair journal (region 41), since the store mutation + deltas are durable and the index converges async (ADR 0024). Bounded by [ADR 0027](../adr/0027-graph-mutation-journal-retention.md): every entry carries `recorded_at_ns` and is evicted after `GRAPH_MUTATION_JOURNAL_RETENTION_NS` (9d, a lower bound `>=` the router's 7d replay TTL — see region 7) via amortized write-path GC (**B**, heap round-robin cursor) on the completed-journal write. Legacy entries (`recorded_at_ns == None`) are lazy-stamped to "now" on first sweep so the pre-upgrade backlog ages out from upgrade time. Ack-through-seq is **not** used as the eviction trigger (unsound: no-delta mutations, shard-global cursor, ack precedes router completion) |
| 40 | `PENDING_VERTEX_PURGES` | `PENDING_VERTEX_PURGES` | `init_pending_vertex_purges` | maintenance | vertex delete | Tombstoned vertices mid-purge (ADR 0021); insert is fail-closed and runs before the tombstone (a failed insert aborts the delete, never leaving ungated ghost edges); rebuildable by scanning tombstoned vertices with surviving incident edges (no API) |
| 41 | `INDEX_REPAIR_JOURNAL` | `INDEX_REPAIR_JOURNAL` | `init_index_repair_journal` | maintenance | federated index repair | Failed-flush index postings persisted on compensation-success (ADR 0023 D5); re-applied by the maintenance driver each tick and on `post_upgrade`, removed on success. Value type is `RepairJournalEntry { mutation_id, op }` (ADR 0029 Phase 2): each entry carries the originating federated `mutation_id` (`0` = untracked sentinel) so `index_pending_min_mutation_id()` derives the mutation-linked index watermark (smallest unapplied tracked mutation). **Backward-incompatible repack**: the value schema changed in place from a bare `RepairPostingOp` to `RepairJournalEntry`; pre-existing entries in the old layout are not migrated (no production deployment) |
| 42 | `UNIQUE_EFFECT_OUTBOX` | `UNIQUE_EFFECT_OUTBOX` | `init_unique_effect_outbox` | canonical | cross-shard uniqueness | **`EffectId { mutation_id, effect_ordinal } → UniqueEffectReceipt { claim_id?, owner_element_id, constraint_id, encoded_value, op: Acquire \| Release }`** (ADR 0030). Pinned commit evidence: each unique-affecting canonical segment appends one receipt per effect; an effect stays pinned until the Router acks its `EffectId` (per-effect, never unpinning a sibling). Canonical: a `Reserved`-not-yet-committed claim has no receipt, so un-acked-effect *absence* is authoritative proof of non-commit — decoupled from the 9-day journal eviction (region 39 / ADR 0027). Replicated `Acquire`-by-`ClaimId` proof read + per-effect ack are Router-only update endpoints. Append is idempotent across deterministic replays. Emit wiring into the DML segment lands in slice 5 |
| 43 | `GRAPH_LOCAL_UNIQUE_VALUES` | `GRAPH_LOCAL_UNIQUE_VALUES` | `init_graph_local_unique_table` | canonical | shard-local global uniqueness | **`(constraint_id, encoded_value) → LocalUniqueRecord { owner_element_id }`** (ADR 0030 slice 10). Canonical source of truth for `ShardLocalGlobal` unique constraints: a graph proven single-shard at CREATE enforces graph-wide uniqueness entirely in its one owning shard's local table, bypassing the Router reservation/`UNIQUE_EFFECT_OUTBOX` path. The acquire path preflights all claims and inserts them inside the canonical write segment (all-or-nothing); a delete/remove frees the value by owner match; the DROP drain purges the constraint's entries and gates `Removed` on the range being empty. The key omits `graph_id` because one graph canister hosts exactly one graph/shard. No `Acquire`/`Release` receipts and no Router reservations are ever written for these constraints |
| 44 | `VERTEX_EMBEDDINGS` | `VERTEX_EMBEDDINGS` | `init_vertex_embedding_store` | canonical | embeddings | **`(VertexId, EmbeddingNameId) → StoredEmbedding { encoding, dims, version, bytes }`** ([ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md)). Canonical fixed-dimension `F32` vertex embeddings owned by the graph shard. Vertex-major, big-endian, fixed-width 6-byte key so vertex delete enumerates a vertex's embeddings via a per-vertex range scan (`commit_clear_vertex_embeddings`). Value is a length-prefixed manual layout led by a `schema_version: u8` tag; an unknown schema/encoding tag traps on read (incompatible layout requires a migration). Source for the derived vector-index backfill (Slice 5); dispatch is activated behind the incarnation fence + Router stable flag (Slice 4) |
| 45 | `VERTEX_EMBEDDING_INCARNATIONS` | (within `init_vertex_embedding_store`) | `init_vertex_embedding_store` | canonical | embeddings | **`(VertexId, EmbeddingNameId) → u64`** ([ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md) Slice 4). Delete-spanning, monotonic `embedding_incarnation` high-water mark per embedding identity: it strictly increases on each reinsert and is **never deleted** (a remove keeps the entry while clearing region 44). Shares the `VertexEmbeddingKey` shape with region 44; the live `StoredEmbedding` payload stays incarnation-free. Sync ops carry `embedding_incarnation` so the vector canister orders writes by `(incarnation, version)`, closing the delete/reinsert forward- and reverse-orphan races. `set`/`remove` checked-fail on `IncarnationOverflow` (`u64::MAX` wraparound) |
| 46 | `DERIVED_INDEX_OUTBOX` | `DERIVED_INDEX_OUTBOX` | `init_derived_index_outbox` | maintenance | federated index delivery | **`sequence → DerivedIndexOutboxEntry { mutation_id, op }`** (Plan 0088). Durable FIFO storage and Router→Graph wire-DML handoff for derived property, label, and vector-index work. Maintenance currently promotes bounded prefixes into the existing repair dispatcher; ad-hoc/native transaction delivery remains on the legacy path. |

Graph facade **15 regions** total (32 LARA + 15 facade). `DERIVED_INDEX_OUTBOX` (MemoryId 46) is the durable FIFO handoff for successful Router→Graph wire-DML-derived index work; bounded maintenance promotion reuses the existing repair dispatcher. Ad-hoc/native transaction delivery remains legacy pending/flush behavior until the rest of Plan 0088 lands. Retired 2026-06-12: `EDGE_PAYLOAD_PROFILES` → router SSOT ([ADR 0008](../adr/0008-edge-inline-value-profile-router-ssot.md)); `EDGE_EQUALITY_POSTINGS` → graph-index ([ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md)).

Property **names** are router-owned (`ROUTER_PROPERTY_CATALOG`); graph stores values by `PropertyId` only.

### Graph ephemeral (not in `memory.rs`)

| Symbol | Location | Role | Reopen behavior |
|--------|----------|------|-----------------|
| `PENDING` (property postings) | `graph/src/index/pending.rs` | Queued property index ops | In-flight batch lost on upgrade; failed batches persist to `INDEX_REPAIR_JOURNAL` (ADR 0023 D5); `backfill_vertex_property_postings` covers historical vertex properties |
| `PENDING` (edge postings) | `graph/src/index/edge_pending.rs` | Queued edge property index ops | In-flight batch lost on upgrade; failed batches persist to `INDEX_REPAIR_JOURNAL` (ADR 0023 D5); `backfill_edge_property_postings` covers historical edge properties |
| `PENDING` (label postings) | `graph/src/index/label_pending.rs` | Queued label index ops | In-flight batch lost on upgrade; failed batches persist to `INDEX_REPAIR_JOURNAL` (ADR 0023 D5); `backfill_label_postings` covers historical labels |

---

## Router canister — stable regions

Repacked 2026-06-17: placement removed, controllers merged into auth, MemoryIds compacted to **0–33** (34 regions). ADR 0030 appended the uniqueness constraint catalog (**34–36**), the uniqueness reservation table (**37**), and the slice-6 reservation reverse index (**38**) and pending unique-effect discovery index (**39**), bringing the total to 40 regions (0–39). ADR 0031 Slice 3 appended the graph-scoped embedding-name catalog (**40–41**) and the derived vector-index definition catalog (**42**), and Slice 4 appended the vector dispatch activation flag (**43**), and Slice 10 appended the vector maintenance policy catalog (**44**), bringing the total to **45 regions (0–44)**. Regions grouped **auth → registry → runtime config → idempotency → catalog → telemetry → maintenance → constraint catalog → reservation table → reservation reverse index → pending-effect discovery → embedding-name catalog → vector-index catalog → vector dispatch activation → vector maintenance policy**. `ROUTER_GRAPHS` keyed by **`GraphId`**; `ShardRegistryEntry` stores **`graph_id: GraphId`**. `ROUTER_SHARDS` keyed by **`GraphShardKey { graph_id, shard_id }`**; `ROUTER_SHARD_BY_GRAPH` is **`Principal → GraphShardKey`**; shard listing per logical graph uses **`ROUTER_SHARDS_BY_GRAPH_ID`**.

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
| 7 | `ROUTER_MUTATION_BY_CLIENT_KEY` | `ROUTER_MUTATION_BY_CLIENT_KEY` | `init_mutation_by_client_key` | canonical | idempotency | keys use **`graph_id: GraphId`**; bounded by [ADR 0025](../adr/0025-client-mutation-journal-retention-sweep.md): completed records are compacted (heavy fields dropped, **E**), and expired records (`created_at_ns` + `CLIENT_MUTATION_KEY_TTL_NS` 7d) are evicted automatically by amortized write-path GC (**B**, heap round-robin cursor) plus the operator backstop `admin_sweep_expired_client_mutation_keys`. `created_at_ns` is the sole age SSOT; GC cursor is ephemeral heap. [ADR 0029 Phase 4](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md): TTL eviction is now **terminal-only** (non-terminal sagas are retained as recovery targets); the record gained `routing_lease_ns: Option<u64>` (routing-lease reclaim) and `last_error: Option<String>` (recovery diagnostic), both Candid `opt` so pre-Phase-4 records decode as `None` with no migration |
| 8 | `ROUTER_PREPARED_PLANS` | `ROUTER_PREPARED_PLANS` | `init_prepared_plans` | canonical | prepared queries | **`PreparedPlanKey → PreparedPlanRecord::V1`** |
| 9–10 | `ROUTER_VERTEX_LABEL_BY_NAME` / `ROUTER_VERTEX_LABEL_BY_ID` | `ROUTER_VERTEX_LABEL_CATALOG` | `init_vertex_label_catalog` | catalog | resolution | **`GraphScopedNameCatalog<VertexLabelId>`** — `(GraphId, name) ↔ id` ([ADR 0018](../adr/0018-graph-scoped-label-property-catalogs.md)) |
| 11–12 | `ROUTER_EDGE_LABEL_BY_NAME` / `ROUTER_EDGE_LABEL_BY_ID` | `ROUTER_EDGE_LABEL_CATALOG` | `init_edge_label_catalog` | catalog | resolution | **`GraphScopedNameCatalog<EdgeLabelId>`** (dense, capped) |
| 13–14 | `ROUTER_PROPERTY_BY_NAME` / `ROUTER_PROPERTY_BY_ID` | `ROUTER_PROPERTY_CATALOG` | `init_property_catalog` | catalog | resolution | **`GraphScopedNameCatalog<PropertyId>`** (dense) |
| 15–16 | `ROUTER_GRAPH_BY_NAME` / `ROUTER_GRAPH_BY_ID` | `ROUTER_GRAPH_CATALOG` | `init_graph_catalog` | catalog | resolution | Logical graph name ↔ **`GraphId`** ([ADR 0011](../adr/0011-gql-graph-resolution-and-catalog-scoping.md)) |
| 17–18 | `ROUTER_INDEX_NAME_BY_NAME` / `ROUTER_INDEX_NAME_BY_ID` | `ROUTER_INDEX_NAME_CATALOG` | `init_index_name_catalog` | catalog | resolution | Graph-scoped index name ↔ **`IndexNameId`** per `GraphId` |
| 19 | `ROUTER_NAMED_INDEXES` | `ROUTER_NAMED_INDEXES` | `init_named_indexes` | catalog | index DDL metadata | **`(GraphId, IndexNameId) → kind, property_id, label_id`** |
| 20 | `ROUTER_INDEXED_PROPERTY_SET` | `ROUTER_INDEXED_PROPERTY_SET` | `init_indexed_property_set` | catalog | index membership | **`(GraphId, kind, property_id)`** for planner + fan-out |
| 21 | `ROUTER_EDGE_PAYLOAD_PROFILES` | `ROUTER_EDGE_PAYLOAD_PROFILES` | `init_edge_inline_value_profiles` | catalog | edge inline value schema | **`(GraphId, EdgeLabelId) → EdgeInlineValueSchemaRecord`** ([ADR 0008](../adr/0008-edge-inline-value-profile-router-ssot.md), [ADR 0018](../adr/0018-graph-scoped-label-property-catalogs.md), [ADR 0034 Slice 20 + Slice 24](../adr/0034-gleaph-gql-extension-syntax.md)) |
| 22–23 | `ROUTER_GRAPH_TYPE_DEFINITIONS` / `ROUTER_GRAPH_SCHEMA_BINDINGS` | `ROUTER_GQL_GRAPH_CATALOG` | `init_gql_graph_catalog` | catalog | graph type catalog | type defs + **`GraphId` bindings** ([ADR 0013](../adr/0013-gql-graph-type-catalog-on-router.md)) |
| 24–25 | `ROUTER_GRAPH_TYPE_BY_NAME` / `ROUTER_GRAPH_TYPE_BY_ID` | `ROUTER_GRAPH_TYPE_CATALOG` | `init_graph_type_name_catalog` | catalog | resolution | Graph type name ↔ **`GraphTypeId`** ([ADR 0014](../adr/0014-graph-type-id-catalog-on-router.md)) |
| 26 | `ROUTER_VERTEX_LABEL_STATS` | `ROUTER_VERTEX_LABEL_STATS` | `init_vertex_label_stats` | telemetry | label telemetry | **`(GraphId, VertexLabelId) → LabelStats`** (event replay) |
| 27 | `ROUTER_EDGE_LABEL_STATS` | `ROUTER_EDGE_LABEL_STATS` | `init_edge_label_stats` | telemetry | label telemetry | **`(GraphId, EdgeLabelId) → LabelStats`** (event replay) |
| 28 | `ROUTER_VERTEX_LABEL_LIVE_BY_SHARD` | `ROUTER_VERTEX_LABEL_LIVE_BY_SHARD` | `init_vertex_label_live_by_shard` | telemetry | label telemetry | **`(GraphId, ShardId, VertexLabelId) → live_count`** |
| 29 | `ROUTER_EDGE_LABEL_LIVE_BY_SHARD` | `ROUTER_EDGE_LABEL_LIVE_BY_SHARD` | `init_edge_label_live_by_shard` | telemetry | label telemetry | **`(GraphId, ShardId, EdgeLabelId) → live_count`** |
| 30 | `ROUTER_LABEL_STATS_PROJECTION` | `ROUTER_LABEL_STATS_PROJECTION` | `init_label_stats_projection` | telemetry | label stats projection | **`GraphShardKey → applied_through_seq`** |
| 31 | `ROUTER_LABEL_BACKFILL_STATE` | `ROUTER_LABEL_BACKFILL_STATE` | `init_label_backfill_state` | maintenance | label backfill | **`GraphShardKey → BackfillShardState`**, one entry per shard (bounded by live shard count, not operations); the step's TOCTOU concurrency guard is a heap-local `INFLIGHT_BACKFILL` claim set (not stable — auto-released on upgrade), so the stable record is unchanged; entry purged on `unregister_shard` so a re-registered shard does not inherit a stale cursor |
| 32 | `ROUTER_VERTEX_PROPERTY_BACKFILL_STATE` | `ROUTER_VERTEX_PROPERTY_BACKFILL_STATE` | `init_vertex_property_backfill_state` | maintenance | vertex property backfill | **`GraphShardKey → BackfillShardState`**; same heap-local claim guard and `unregister_shard` purge as region 31 |
| 33 | `ROUTER_EDGE_BACKFILL_STATE` | `ROUTER_EDGE_BACKFILL_STATE` | `init_edge_backfill_state` | maintenance | edge backfill | **`GraphShardKey → EdgeBackfillShardState`**; same heap-local claim guard and `unregister_shard` purge as region 31 |
| 34–35 | `ROUTER_CONSTRAINT_NAME_BY_NAME` / `ROUTER_CONSTRAINT_NAME_BY_ID` | `ROUTER_CONSTRAINT_NAME_CATALOG` | `init_constraint_name_catalog` | catalog | resolution | Graph-scoped constraint name ↔ **`ConstraintNameId`** per `GraphId` ([ADR 0030](../adr/0030-cross-shard-uniqueness-tcc-reservation.md)) |
| 36 | `ROUTER_UNIQUE_CONSTRAINTS` | `ROUTER_UNIQUE_CONSTRAINTS` | `init_unique_constraints` | catalog | constraint DDL metadata | **`(GraphId, ConstraintNameId) → ConstraintDefStableRecord::V1(ConstraintDefRecord { vertex_label_id, property_id, state: ConstraintLifecycle { Active \| Dropping }, dropping_at_ns, drop_scan_generation })`** — logical uniqueness constraint definitions, declare-on-empty (ADR 0030, first cut: vertex single-property). Versioned envelope (ADR 0030 Revision #18, Slice 9): the DROP lifecycle adds `state`/`dropping_at_ns`/`drop_scan_generation`; `Removed` is the terminal **absence** of the record (deleted by recovery once the completion gate holds), not a persisted enum value |
| 37 | `ROUTER_UNIQUE_RESERVATIONS` | `ROUTER_UNIQUE_RESERVATIONS` | `init_unique_reservations` | canonical | uniqueness reservation table | **`(GraphId, ConstraintNameId, encoded_value) → ReservationRecord { claim, state, reclaim_generation, owner_element_id, reserved_at_ns, proof_scope }`** — cross-shard uniqueness TCC claims (ADR 0030). Canonical: a `Reserved`-not-yet-`Committed` claim has no outbox receipt to rebuild from. Slice 3 implements the no-`await` Try; Confirm/Cancel + reclaim land in slices 4–6 |
| 38 | `ROUTER_MUTATION_RESERVATION_INDEX` | `ROUTER_MUTATION_RESERVATION_INDEX` | `init_mutation_reservation_index` | canonical | uniqueness reservation reverse index | **`MutationId → MutationReservationIndexEntry { client_key, nonterminal }`** — reverse index that resolves a reservation's claim (`MutationId`) to its owning `RouterMutationRecord` and GC-pins that record while non-terminal reservations remain (ADR 0030 slice 6). The row exists **iff** `nonterminal > 0`: `++` per fresh insert at Try, `--` on `FreshlyCommitted` Confirm and on reclaim Cancel; removed at zero |
| 39 | `ROUTER_UNIQUE_EFFECT_PENDING` | `ROUTER_UNIQUE_EFFECT_PENDING` | `init_unique_effect_pending` | canonical | pending unique-effect discovery index | **`(GraphId, MutationId, ShardId) → PendingEffectRecord { schema_version, canister, client_key, state: Active \| Quarantined, next_retry_ns, attempts, diagnostic? }`** — durable discovery source for Driver 2's unified effect recovery (ADR 0030 slice 6). Registered before the first dispatch `await` for any dispatch that may emit a unique `Acquire`/`Release`, so it co-commits with the reservation/envelope. The pinned `canister` is the row's immutable identity, stored verbatim (not re-derived from the shard registry) so recovery reaches the exact canister even after the shard is unregistered/reused; `register` is **fail-closed** (re-registering the same key to a different canister traps). `client_key` is the owning `ClientMutationKey`, stored so Driver 2 resolves the `RouterMutationRecord` (its terminal-completion proof) for **any** effect kind — a `Release` or orphan `Acquire` owns no reservation, so the reverse index (region 38) cannot resolve them; the row GC-pins that record while it exists. Driver 2 only drains a row once its mutation is terminal, classifies a reservation-less `Acquire` as an orphan only then, and removes the row only after a fresh `cursor=None` re-scan is empty. This is the only handle to an orphan `Acquire` whose reservation is gone (parked `Quarantined` on a long backoff, never acked). The value is a **versioned record** so the diagnostic/quarantine/backoff fields evolve without a breaking value-layout change. While any row remains the owning `RouterMutationRecord` is **GC-pinned** (Driver 2 reads its completion state); a row is removed only after the shard re-enumerates the mutation's effects empty (all acked) |
| 40–41 | `ROUTER_EMBEDDING_NAME_BY_NAME` / `ROUTER_EMBEDDING_NAME_BY_ID` | `ROUTER_EMBEDDING_NAME_CATALOG` | `init_embedding_name_catalog` | catalog | resolution | Graph-scoped embedding name ↔ **`EmbeddingNameId`** per `GraphId` ([ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md) Slice 3). The Router is the sole allocator; vector-index registration interns **by name** so the stored id is exactly the one the graph stamps on canonical embedding writes. Purged with the graph on `unregister_graph` |
| 42 | `ROUTER_VECTOR_INDEXES` | `ROUTER_VECTOR_INDEXES` | `init_vector_indexes` | catalog | derived vector index catalog | **`(GraphId, index_id) → VectorIndexDefStableRecord::V1(VectorIndexDefRecord { index_id, embedding_name_id, kind, metric, encoding, dims, target: Option<VectorIndexTarget { canister }>, activation_state })`** — derived vector-index definitions (ADR 0031 Slice 3). Versioned envelope (ADR 0007). The stored `activation_state` is `Registered`/`DispatchBlocked`; **`DispatchEnabled` is derived at read time** by `graph_vector_dispatch_ready(graph_id)` (Slice 4 two-condition gate: global flag region 43 **AND** every live shard vector-attached to the graph's single target). The Router emits a non-empty ephemeral embedding catalog only when ready (fail-closed otherwise). Registration enforces **one index per embedding name per graph** and **one target per graph**. Purged with the graph on `unregister_graph` |
| 43 | `ROUTER_VECTOR_DISPATCH_ACTIVATION` | `ROUTER_VECTOR_DISPATCH_ACTIVATION` | `init_vector_dispatch_activation` | canonical | vector dispatch activation flag | **`() → bool`** (single-cell, default `false`) — Router-owned global vector-dispatch activation flag ([ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md) Slice 4). Set by `admin_set_vector_dispatch_activation` / read by `vector_dispatch_activation_enabled` (RBAC admin). Replaces the retired `const fn incarnation_fencing_enabled()`. Necessary but not sufficient: per-graph emission is additionally fenced on every live shard's `vector_index_attached` bit. Reversible without a redeploy |
| 44 | `ROUTER_VECTOR_MAINTENANCE_POLICIES` | `ROUTER_VECTOR_MAINTENANCE_POLICIES` | `init_vector_maintenance_policies` | catalog | vector maintenance policy catalog | **`(GraphId, index_id) → VectorMaintenancePolicyStableRecord::V1(VectorMaintenancePolicyRecord { enabled, policy, target_nlist, sample_limit, scan_max_pages, rebuild_max_subjects, cleanup_max_work })`** — Router-owned SSOT for vector maintenance thresholds + per-step budgets ([ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md) Slice 10). Versioned envelope (ADR 0007), **default absent/disabled** (the push step is a no-op until set + enabled). Authorship is `authorize_index_ddl` (validated: `recommended_*_bps <= required_*_bps`, nonzero budgets, def exists); `admin_vector_maintenance_step` snapshots it and forwards one bounded unit to the vector canister (which owns the execution state). Purged with the graph on `unregister_graph` |
| 45 | `ROUTER_PROVISIONING_REQUESTS` | `ROUTER_PROVISIONING_REQUESTS` | `init_provisioning_requests` | canonical | provisioning request catalog | **`(request_id, deployment_id) → RouterProvisioningRequest`** (ADR 0035 Slice 1). Canonical Router-owned issuance intent before any canister id exists |
| 46 | `ROUTER_PROVISIONING_BY_GRAPH` | `ROUTER_PROVISIONING_BY_GRAPH` | `init_provisioning_by_graph` | derived | provisioning graph index | **`(deployment_id, graph_name, request_id) → ProvisioningRequestKey`** (ADR 0035 Slice 1). Derived graph-scoped secondary index; commit-synced with the canonical request catalog |
| 47 | `ROUTER_PROVISIONING_INTENT_LOCK` | `ROUTER_PROVISIONING_INTENT_LOCK` | `init_provisioning_intent_locks` | canonical | provisioning intent lock | **`(deployment_id, resource_kind, logical_resource_key) → IntentLockMarker`** (ADR 0035 Slice 1). Canonical intent lock held while a request targeting this intent is non-terminal |
| 48 | `ROUTER_PROVISION_CONFIG` | `ROUTER_PROVISION_CONFIG` | `init_provision_config` | canonical | provisioning runtime config | **`() → ProvisionRuntimeConfig`** (ADR 0035 Slice 5). Durable Router provision-canister bootstrap binding; re-seeds the heap `PROVISION_CANISTER` threadlocal in `post_upgrade` |

Router **49 regions** total (0–48).

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

## Graph-vector-index canister — stable regions

New in ADR 0031 Slice 2; extended in Slice 6 and Slice 7; physical page store replaced by ADR 0032; maintenance scan state added in Slice 10. **15 regions (MemoryId 0–14).** The entire derived
state is rebuildable from canonical graph embeddings via `vertex_embedding_backfill`, so every
derived region shares the `"vertex_embedding_backfill"` rebuild path in `VECTOR_INDEX_STABLE_LAYOUT`
(`crates/graph-kernel/src/stable_layout.rs`). Code source of truth for runtime `MemoryId` constants:
`crates/graph-vector-index/src/facade/stable/memory.rs`. The degenerate `ivf_flat` foundation runs
with `nlist = 1` / `partition_id = 0` and no centroids; MemoryId 6 (`IVF_CENTROIDS`) is allocated but
empty so Slice 4 can populate centroid bytes without a `MemoryId` repack.

Metadata ownership split: `VECTOR_INDEX_DEFS` (MemoryId 4) is authoritative for per-index config
(`kind`, `encoding`, `dims`, `metric`, `active_index_version`, `stride_bytes`, page-capacity
contract, and the durable `next_vector_id` allocator); `IVF_CENTROID_META` (MemoryId 5) holds only
centroid-specific derived state. The durable `next_page_id` allocator lives in each
`PartitionHead` (MemoryId 9).

Slice 6 adds `VECTOR_ID_TO_SUBJECT` (MemoryId 11): a `(index_id, vector_id) → VectorSubject`
reverse locator used by the partition-page search path to resolve a scanned page row back to its
subject. It is maintained in lockstep with `VECTOR_ID_TO_SLOT` on insert/resurrect/remove and is a
pure locator — `VECTOR_SUBJECT_TO_ID` (MemoryId 7) remains the single source of truth for
liveness/freshness (the row is only scored after re-validating the subject entry's `vector_id`,
`slot`, and `generation`).

Slice 7 adds `VECTOR_REBUILD_STATE` (MemoryId 12): a `index_id → VectorRebuildStateRecord` holding
the per-index bounded shadow-version rebuild lifecycle (`Idle`/`Sampling`/`Training`/`Building`/
`ReadyToPublish`/`Cleaning`/`Aborting`/`Failed`), each long-running phase carrying a resume cursor so
admin steps stay bounded. The `Sampling` and `Training` variants additionally carry the bounded
distinct candidate pool (Slice 8 k-means-lite training); it stays inside this MemoryId 12 record and is
not split into a separate region (ADR 0033). The value is persisted as `RawRebuildState` — the verbatim
`VectorRebuildStateRecord` Candid bytes (on-disk format unchanged) — so the per-step fail-closed
encoded-size guard and the persist share a single Candid encode instead of encoding twice. It is derived (reconstructible by re-running a rebuild from the active
version) and shares the `vertex_embedding_backfill` rebuild path. Slice 7 also extends the
`VECTOR_SUBJECT_TO_ID` value (`SubjectMapEntry`) with a second `shadow_slot: Option<SlotRef>`
(serde-default, no repack) so an atomic publish stays metadata-only; search resolves the live slot
via `current_slot_for(active_index_version)`.

ADR 0032 replaces the MemoryId 10 large-value page store with a composite slab page store: MemoryId
10 becomes `VECTOR_PAGE_META` (a `(index_id, version, partition_id, page_id) → VectorPageMeta`
directory of `{ slab_offset, capacity, row_count, live_count, row_stride, tombstone_count }`), and a
new MemoryId 13 `VECTOR_ROW_SLAB` holds the raw structure-of-arrays row bytes behind a magic/version
header. The two regions form one composite store (`PAGE_STORE`) that opens together and fails closed
on a partial layout (see [ADR 0032](../adr/0032-vector-index-slab-page-store.md)). This is a **fresh
layout cutover** with no deployed `VECTOR_PAGE` state, no migration, and no compatibility reader; the
`vertex_embedding_backfill` rebuild path is the canonical-embedding reconstruction route, not a
page-format migration. Each row carries a derived `subject_locator` that retires
`VECTOR_ID_TO_SUBJECT` from the partition-scan hot path while `VECTOR_SUBJECT_TO_ID` stays the
freshness source of truth. `VECTOR_PARTITION_HEADS` (MemoryId 9) remains the per-partition
allocator/counter owner and is deliberately outside the composite store.

A derived, router-guarded admin query (`admin_vector_slab_stats`) reports slab-space observability
over these two regions: whole-slab physical facts (size, `occupied_tail`, global referenced bytes,
conservative dead-space estimate) plus optional per-`index_id`/per-version logical counters. Because
`VECTOR_ROW_SLAB` is a single global allocation domain, the physical facts are always global while
`index_id` scopes only the logical counters. It reads only `VECTOR_PAGE_META` + the slab header
(never row bytes or `VECTOR_SUBJECT_TO_ID`). `admin_vector_slab_stats` is the **unbounded** full
page-meta scan kept as a convenience query; `admin_vector_slab_stats_step` is the IC-safe bounded
companion that scans at most `max_pages` page-meta entries per call and returns an opaque `PageKey`
cursor. Its steps are additive partials merged client-side: each step still sums global referenced
bytes across the whole map (even under an `index_id` filter), so the per-step dead-space estimate is
`0` and is recomputed once after merging. A malformed step cursor returns an error rather than
trapping. The stepped path is a bounded best-effort scan with **no snapshot isolation** (the cursor
is only a `PageKey`, so concurrent `VECTOR_PAGE_META` writes between calls can be missed or
double-counted); run it during a quiescent window or use the single-call query for an exact figure.
Both queries are diagnostic only and never affect search/freshness; allocator/compaction work remains
deferred.

| MemoryId | Symbol | Thread-local | Init fn | Class | Owner domain | Rebuild |
|--------|--------|--------------|---------|-------|--------------|---------|
| 0 | `VECTOR_INDEX_ROUTER` | `VECTOR_INDEX_ROUTER` | `init_router` | canonical | router authorization | — |
| 1 | `VECTOR_INDEX_SHARD_CANISTER_BY_SHARD` | `SHARD_CANISTER_CATALOG` | `init_shard_canister_catalog` | canonical | shard canister catalog | — |
| 2 | `VECTOR_INDEX_SHARD_BY_CANISTER` | `SHARD_CANISTER_CATALOG` | `init_shard_canister_catalog` | canonical | shard canister catalog | — |
| 3 | `VECTOR_INDEX_OWNERSHIP_CONFIG` | `OWNERSHIP_CONFIG` | `init_ownership_config` | canonical | graph ownership | Graph owner config (`graph_id`, `index_group_size`, `group_index`) for attach range checks |
| 4 | `VECTOR_INDEX_DEFS` | `VECTOR_INDEX_DEFS` | `init_defs` | derived | index definitions + allocators (SSOT for config) | `vertex_embedding_backfill` |
| 5 | `IVF_CENTROID_META` | `IVF_CENTROID_META` | `init_centroid_meta` | derived | centroid-specific state | `vertex_embedding_backfill` |
| 6 | `IVF_CENTROIDS` | `IVF_CENTROIDS` | `init_centroids` | derived | centroid vectors (**reserved-empty in Slice 2**) | `vertex_embedding_backfill` |
| 7 | `VECTOR_SUBJECT_TO_ID` | `VECTOR_SUBJECT_TO_ID` | `init_subject_map` | derived | subject tombstone clock | `vertex_embedding_backfill` |
| 8 | `VECTOR_ID_TO_SLOT` | `VECTOR_ID_TO_SLOT` | `init_id_to_slot` | derived | vector-id → slot map | `vertex_embedding_backfill` |
| 9 | `VECTOR_PARTITION_HEADS` | `VECTOR_PARTITION_HEADS` | `init_partition_heads` | derived | partition page chains + `next_page_id` allocator | `vertex_embedding_backfill` |
| 10 | `VECTOR_PAGE_META` | `PAGE_STORE` | `init_page_store` | derived | page-directory metadata (slab offset + capacity/row/live/tombstone counts) for the slab page store (**ADR 0032**) | `vertex_embedding_backfill` |
| 11 | `VECTOR_ID_TO_SUBJECT` | `VECTOR_ID_TO_SUBJECT` | `init_id_to_subject` | derived | vector-id → subject reverse locator (**Slice 6**; retired from the search hot path by ADR 0032's row-local `subject_locator`, region retained) | `vertex_embedding_backfill` |
| 12 | `VECTOR_REBUILD_STATE` | `VECTOR_REBUILD_STATE` | `init_rebuild_state` | derived | bounded shadow-version rebuild lifecycle (**Slice 7**) | `vertex_embedding_backfill` |
| 13 | `VECTOR_ROW_SLAB` | `PAGE_STORE` | `init_page_store` | derived | raw structure-of-arrays vector row slab with magic/version header (**ADR 0032**); companion to `VECTOR_PAGE_META`, opened as one composite store | `vertex_embedding_backfill` |
| 14 | `VECTOR_MAINTENANCE_STATE` | `VECTOR_MAINTENANCE_STATE` | `init_maintenance_state` | maintenance | `index_id → VectorMaintenanceState` page-health scan progress cursor + merged counters (`Failed` carries a bounded message) for Router-forwarded maintenance orchestration (**Slice 10**); operational bookkeeping discarded/restarted, not reconstructed; persists across upgrade, cleared only on init/reset | — |

---

## Provision canister — stable regions

New in ADR 0035 Slice 2. **11 regions (MemoryId 0–10).** Code source of truth for runtime `MemoryId`
constants: `crates/provision/src/stable/memory.rs`. Typed registry and rebuild path:
`PROVISION_STABLE_LAYOUT` in `crates/graph-kernel/src/stable_layout.rs`. Region 2 is a derived
secondary index and is commit-synced with Region 1 (`PROVISION_JOB_BY_REQUEST`).

| MemoryId | Symbol | Thread-local | Init fn | Class | Owner domain | Rebuild |
|--------|--------|--------------|---------|-------|--------------|---------|
| 0 | `PROVISION_DEPLOYMENT_TRUST` | `DEPLOYMENT_TRUST` | `init_deployment_trust` | canonical | deployment trust binding (`deployment_id → DeploymentBinding`) | — |
| 1 | `PROVISION_JOB_BY_REQUEST` | `JOB_BY_REQUEST` | `init_job_by_request` | canonical | durable job/receipt state (`(request_id, deployment_id) → ProvisionJobRecord`); value now includes `accepted_registry_version: Option<u64>` | — |
| 2 | `PROVISION_JOB_BY_DEPLOYMENT` | `JOB_BY_DEPLOYMENT` | `init_job_by_deployment` | derived | intent → job secondary index (`ProvisioningIntentKey → ProvisionJobRequestKey`) | commit-synced with `PROVISION_JOB_BY_REQUEST` |
| 3 | `PROVISION_JOB_INTENT_LOCK` | `JOB_INTENT_LOCK` | `init_job_intent_lock` | canonical | intent lock held while a request targeting this intent is non-terminal (`ProvisioningIntentKey → ProvisionIntentLockMarker`) | — |
| 4 | `PROVISION_BOOTSTRAP_AUTH` | `BOOTSTRAP_AUTH` | `ProvisionBootstrapAuthStore::init_authority` | canonical | durable bootstrap authority singleton (`StableCell<Option<BootstrapAuthorityRecord>>`) (ADR 0035 Slice 7) | — |
| 5 | `PROVISION_BOOTSTRAP_AUDIT_LOG` | `BOOTSTRAP_AUDIT_LOG` | `ProvisionBootstrapAuthStore::put_record` | telemetry | per-governance audit log of admin-install decisions (`Principal → BootstrapAuthHistory`) (ADR 0035 Slice 7) | — |
| 6 | `PROVISION_ARTIFACT_CATALOG` | `PROVISION_ARTIFACT_CATALOG` | `init_artifact_catalog` | canonical | immutable artifact catalog (`ArtifactId → ArtifactMetadata`) (ADR 0036 Slice 8a) | — |
| 7 | `PROVISION_ARTIFACT_UPLOAD` | `PROVISION_ARTIFACT_UPLOAD` | `init_artifact_upload` | maintenance | mutable upload/verification scratch state (`ArtifactId → ArtifactUpload`), reclaimed on verify success (ADR 0036 Slice 8a) | — |
| 8 | `PROVISION_ARTIFACT_CHUNKS` | `PROVISION_ARTIFACT_CHUNKS` | `init_artifact_chunks` | canonical | verified canonical artifact chunk bytes (`ArtifactChunkKey → ArtifactChunk`) retained until explicit GC (ADR 0036 Slice 8a) | — |
| 9 | `PROVISION_RELEASE_MANIFEST` | `RELEASE_MANIFEST_MAP` | `init_release_manifest` | canonical | immutable release manifest (`ReleaseId → ReleaseManifest`) (ADR 0036 Slice 8b) | — |
| 10 | `PROVISION_ACTIVE_RELEASE` | `ACTIVE_RELEASE_CELL` | `init_active_release` | canonical | atomic active-release pointer (`StableCell<Option<ReleaseId>>`) (ADR 0036 Slice 8b) | — |
| 11 | `PROVISION_ARTIFACT_AUDIT_LOG` | `ARTIFACT_AUDIT_LOG` | `init_artifact_audit_log` | telemetry | append-oriented artifact/release audit log (`(Principal, u64) -> ArtifactAuditEntry`) (ADR 0036 Slice 8c) | — |

## Related documents

- [Refactoring roadmap](../architecture/refactoring-roadmap.md) — phased plan; Phase 0 exit criteria
- [LARA and graph facade](./lara-and-facade.md) — layering; defers byte layout to this inventory
- [Property index](../index/property-index.md) — posting model and router seed routing
- [Label index](../index/label-index.md) — label postings and backfill orchestration
- [ADR 0004: Label index](../adr/0004-label-index.md)
