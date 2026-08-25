# Stable-memory inventory

Last updated: 2026-08-22
Status: Partially Implemented (graph: sequential LARA MemoryIds 0–31 + facade 32–46, ADR 0059 canonical-export scopes at 51, and the exact derived-index pending floor at 52, with reserved holes 35, 44–45, and 47–50 = 53 numbered regions, 0–52; router repack ADR 0011/0018/0019 + ADR 0030 constraint catalog + reservation table + slice-6 reverse index + pending-effect discovery index + ADR 0031 Slice 3 embedding-name catalog + vector-index definition catalog + Slice 4 vector dispatch activation flag + Slice 10 vector maintenance policy catalog + ADR 0034 Slice 20 + Slice 24 edge inline property schema record + ADR 0035 provisioning regions + ADR 0057 durable bulk-load receipt map at MemoryId 49 + ADR 0058/0059 schema-migration ledger at MemoryId 50 + ADR 0059 index-catalog epoch at MemoryId 51 + ADR 0059 physical-index allocator cell at MemoryId 20 + ADR 0065 vector-index allocator cell at MemoryId 52 + Router direct-ingestion outbox at MemoryId 53 = 54 regions, 0–53; graph-index: ADR 0059 physical-index build states (MemoryId 7) + touched subjects (MemoryId 8) = 9 regions, 0–8; vector-canister: ADR 0031 Slice 2 + Slice 6 reverse subject map + Slice 7 rebuild state + ADR 0032 slab page store + Slice 10 maintenance scan state + ADR 0064 per-shard watermarks, GC cursor, and deleted-subjects list + plan 0278 slab-compaction driver state reusing the retired reverse-map slot at MemoryId 11 + the ADR-0033-implementation rebuild pool region at MemoryId 18 = 18 allocated regions across 19 numbered slots, 0–18 (hole 8); provision: ADR 0035 Slice 2 + Slice 4 callable canister endpoints + Slice 7 durable bootstrap authority singleton (MemoryId 4) and per-governance audit log (MemoryId 5) + ADR 0036 Slice 8a artifact catalog (MemoryId 6), upload state (MemoryId 7), verified chunk bytes (MemoryId 8), internal storage-id counter (MemoryId 12) + Slice 8b release manifest (MemoryId 9) and active release pointer (MemoryId 10) + Slice 8c artifact audit log (MemoryId 11) = 13 regions, 0–12)
Anchor timestamp: 2026-08-22 17:56:38 UTC +0000

Plan 0171 update (2026-07-24 23:03:51 UTC +0000): the implemented mate owner is explicitly
the leaf-scoped five-byte locator plus one multi-bucket blob per orientation/leaf. A contract test
publishes two buckets through one locator row, reopens the four-region composite, and verifies that
the neighboring locator remains ScanOnly. The bucket-locator/raw-payload fixture remains evidence
only; no production MemoryId or canonical region changed.
Plan 0175 update (2026-07-24 23:03:51 UTC +0000): `MATE_BLOBS` now persists the compact
8-byte-header/15-byte-directory codec at the existing owner boundary. The five-byte leaf locator
and four-region composite are unchanged, and no new MemoryId was allocated.
Plan 0170 update (2026-07-24 13:44:22 UTC +0000): the versioned mate envelope codec, 22-byte locator
value, raw payload region, and reopen fixture are implemented behind isolated `VectorMemory`-backed
stores; these are evidence fixtures rather than the selected production layout. Production remains
the existing leaf locator plus multi-bucket blob owner; a later slice may compact that blob to an
8-byte header and approximately 15-byte bucket-directory entries. No production `MemoryId` or
canonical region changed.
Plan 0188 update (2026-07-28 02:45:06 UTC +0000): Live Graph inline-edge-property mutation now resolves the physical pair through orientation-aware LARA CounterpartScan; directed/undirected self-loops and parallel pair rank are covered by GraphStore tests. `EDGE_ALIASES` itself remains allocated and is still used by local indexes, reverse-adjacency repair, deletion cleanup, and `commit_clear_edge_sidecars`; the deletion observer receives its callback after the removed source row is no longer live, so its migration requires a separate LARA deletion-location contract.
Plan 0189 update (2026-07-28 03:02:03 UTC +0000): Vertex-deletion observers now receive the exact removed logical locations, including the counterpart when present, before LARA emits slot-move notifications. Graph uses the canonical location directly to clear edge sidecars, including directed/undirected self-loops, so deletion-observer cleanup no longer requires `EDGE_ALIASES`. The region remains allocated and is still used by local edge indexes, reverse-adjacency repair, and ordinary scalar-delete canonicalization.
Plan 0190 update (2026-07-28 03:08:14 UTC +0000): Ordinary scalar edge deletion now canonicalizes its pre-removal handle through LARA CounterpartScan and passes the result directly to sidecar cleanup. `EDGE_ALIASES` remains allocated for counterpart-row removal and slot-movement maintenance, but is no longer the ordinary delete canonicalization source of truth.
Plan 0191 update (2026-07-28 03:21:18 UTC +0000): Differential reverse repair now retains matching reverse rows, removes only surplus rows, inserts only missing rows, aligns inline bytes by ordinal, and applies LARA slot moves. Transitional alias-row removal/recreation remains required, but repair no longer rebuilds every reverse row for a diverged key.
Plan 0192 update (2026-07-28 03:39:08 UTC +0000): Graph `EDGE_ALIASES` storage and all live alias mutation, lookup, rebuild, test, and benchmark paths were removed. MemoryId 35 is a reserved hole and is not allocated or reused. CounterpartScan and exact LARA deletion locations are now the sole counterpart source of truth; reverse slot movement is internal to LARA and does not move canonical Graph sidecars.

Plan 0139 update (2026-07-23 12:25:05 UTC +0000): Graph now assigns four shared ADR 0048 mate
regions at MemoryIds 47–50, for 51 graph regions total. The runtime remains dormant beyond storage
ownership.

Layout change policy: [ADR 0007](../adr/0007-stable-memory-layout.md).
Planned production compatibility and migration policy: [ADR 0039](../adr/0039-production-stable-memory-evolution-and-upgrade-safety.md).

Physical backend: the Router, Graph, Graph Index, Vector, and Provision production memory roots use
`ic_stable_memory_backend::DefaultMemoryImpl`. `wasm64-unknown-unknown` builds use private direct
`ic0` stable64 imports, `wasm32-unknown-unknown` retains
`ic_stable_structures::DefaultMemoryImpl`, and native/WASI builds use
`ic_stable_structures::VectorMemory`. This backend substitution does not change per-canister
`MemoryId` allocation, memory-manager formats, or persisted record bytes.

## Purpose

Single inventory of stable-memory regions and heap-only facade state for the graph, router, graph-index, and vector-canister canisters. Each row names the owning domain, classification, and rebuild path where one exists.

Code source of truth for runtime `MemoryId` constants:

- `crates/graph/src/facade/stable/memory.rs`
- `crates/router/src/facade/stable/memory.rs`
- `crates/graph-index/src/facade/stable/memory.rs`
- `crates/vector-canister/src/facade/stable/memory.rs`

Typed layout registry (descriptive mirror + validation tests): `gleaph_graph_kernel::stable_layout`
and per-canister `facade/stable/layout.rs` — [ADR 0007](../adr/0007-stable-memory-layout.md) §7.

Thread-local pairing: `facade/stable.rs` in each crate.

### Region-count doc-sync checklist

Region counts and per-region classes in this document mirror the typed registry, which is the
mechanical source of truth. The registry tests enforce the counts below; when they change, update
this document and [ADR 0007](../adr/0007-stable-memory-layout.md) in the same patch:

| Canister           | Regions | Id range | Registry constant + test                                                       |
| ------------------ | ------- | -------- | ------------------------------------------------------------------------------ |
| Graph              | 53      | 0–52     | `GRAPH_STABLE_LAYOUT` — `graph_layout_registry_matches_baseline`               |
| Router             | 54      | 0–53     | `ROUTER_STABLE_LAYOUT` — `router_layout_registry_matches_baseline`             |
| Graph-index        | 9       | 0–8      | `INDEX_STABLE_LAYOUT` — `index_layout_registry_matches_baseline`               |
| Vector canister    | 17 allocated / 18 slots | 0–17 | `VECTOR_INDEX_STABLE_LAYOUT` — `vector_index_layout_registry_matches_baseline` |
| Provision          | 13      | 0–12     | `PROVISION_STABLE_LAYOUT` — `provision_layout_registry_matches_baseline`       |

The canonical/derived split for the router registry projections is pinned by
`router_registry_canonical_derived_split_matches_inventory`.

### Production compatibility (ADR 0039 gate)

Every registry entry additionally records whether its persisted bytes must survive production
upgrades ([ADR 0039](../adr/0039-production-stable-memory-evolution-and-upgrade-safety.md) N-1
gate) or may be rebuilt, regenerated, or ignored, via `ProductionCompat`
(`gleaph_graph_kernel::stable_layout`; rustdoc on each variant states its obligation at the
gate): `VersionedSurvivor` bytes carry a versioned envelope/header and must decode across
upgrade windows; `Rebuildable` bytes are regenerated through the region's declared rebuild
path; `RegenerateByContract` bytes follow an explicit documented regeneration contract instead
of a versioned envelope; `Reserved` slots back no collection; `Unaudited` marks canisters
whose per-canister classification slice has not run yet. This dimension is orthogonal to the
storage classes above (canonical does not imply survivor: journal 39 is retention-bounded,
export scopes 51 are regenerate-by-contract). The typed registry is the single source of truth
for this classification — this document intentionally keeps no per-region copy. All 53 graph
regions are concretely classified per the [2026-08-24 graph persistence
audit](../investigations/2026-08-24-graph-persistence-audit.md) §3 (enforced by
`graph_regions_fully_classified_for_production_compat`); router, graph-index,
vector-canister, and provision entries currently carry `Unaudited`.

## Classifications

Authoritative definitions and Gleaph examples: `gleaph_graph_kernel::stable_layout::StableMemoryClass`
(rustdoc on each variant). Per-region class and functional role: `GRAPH_STABLE_LAYOUT`,
`ROUTER_STABLE_LAYOUT`, `INDEX_STABLE_LAYOUT`.

| Class           | Meaning                                                               | Examples in this repo                                                                                 |
| --------------- | --------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `canonical`     | Authoritative facts; system meaning does not depend on derived stores | Forward LARA CSR/payloads; vertex/edge properties; router registry and catalogs; mutation idempotency |
| `derived`       | Projection or mirror rebuildable from canonical state                 | Reverse LARA; equality postings; graph-index postings                                                 |
| `maintenance`   | Physical or admin bookkeeping; not query truth                        | LARA free spans; maintenance queue; router backfill cursors                                           |
| `catalog`       | Bidirectional name ↔ id maps (`BidirectionalCatalog`)                 | Router label/property/graph/index-name resolution pairs                                               |
| `telemetry`     | Event-sourced label stats and projection adjuncts                     | Graph label stats delta log; router label stats and `ROUTER_LABEL_STATS_PROJECTION`                   |
| `compatibility` | Legacy read view; another store owns new writes                       | _(none — P1 `EDGE_WEIGHT_PROFILES` retired 2026-06-12)_                                               |
| `ephemeral`     | Heap-only; no `MemoryId` — **not in layout registry**                 | Graph `PENDING` queues; router planner catalog                                                        |

**Sync co-update:** Some derived stores are updated in the same mutation as their canonical source (no async lag). They still have a separate physical region and are classified `derived`.

**Query semantics when derived state lags:** [derived-state-query-semantics.md](../index/derived-state-query-semantics.md).

## Derived-state rebuild summary

| Derived store                             | Canonical source                                                  | Update path                                                                                              | Rebuild / backfill                                                                                                                                                                                                                                                                                                                                               |
| ----------------------------------------- | ----------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| LARA reverse orientation                  | Forward edges + payloads                                          | Co-updated on edge insert/delete                                                                         | **Implemented:** `check_reverse_adjacency` + `rebuild_reverse_adjacency` (`facade/derived_state/reverse_adjacency.rs`). Repair is differential per diverged key; matching rows are retained and reverse slot movement remains LARA-internal.                                                                                                                     |
| Former edge aliases (MemoryId 35)         | None; historical `EDGE_ALIASES` region                            | Reserved, never allocated                                                                                | **Removed by Plan 0192:** counterpart resolution uses LARA CounterpartScan and exact deletion locations.                                                                                                                                                                                                                                                         |
| Edge property postings (graph-index)      | `EDGE_PROPERTIES` (registered props)                              | DML + `edge_pending` flush                                                                               | **Implemented:** `backfill_edge_property_postings` + router `advance_backfill` ([ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md); retired shard-local `EDGE_EQUALITY_POSTINGS` 2026-06-12)                                                                                                                                                           |
| Vertex property postings (graph-index)    | Vertex properties (indexable)                                     | DML + `pending.rs` flush                                                                                 | **Implemented:** `backfill_vertex_property_postings` + router `advance_backfill`                                                                                                                                                                                                                                                                                 |
| Label postings (graph-index)              | `VertexLabelStore`                                                | DML + `label_pending` flush                                                                              | **Implemented:** `backfill_label_postings` + router `advance_backfill` ([label-index.md](../index/label-index.md))                                                                                                                                                                                                                                               |
| Vector-index entries (vector-canister) | vector canister (indexed embedding bytes; Router MemoryId 53 owns pending/retry payload bytes until exact marker retirement; graph holds zero) | Router `ROUTER_VECTOR_INGEST_OUTBOX` (`AwaitingGraph` exact intent → `AwaitingVector` → `AwaitingFrontier` marker) → Graph `stamp_embedding` (validation-only) → typed-only Vector `vector_sync_batch_outcome` and Router-only frontier publication; DML-driven removes remain Graph-owned durable work | **ADR 0064 §1/§6:** Router co-writes every allocated stamp with its exact Graph/Vector targets, canonical metadata, subject, bytes, and phase before the first Graph await. Graph acceptance advances only that row; Graph rejection changes only that row to `AwaitingFrontier`; response loss preserves the current phase. `Progress` and `Terminal` change only the acknowledged Vector prefix to frontier markers. Recovery enumerates fully attached catalog lanes, derives each selected lane from its oldest unresolved exact-lane intent or the allocation ceiling, and may publish an empty marker snapshot for a Graph-only lane. It attempts one lane per tick with cursor-before-await fairness; Vector rechecks Router ownership and exact shard attachment, applies a monotonic MemoryId 15 frontier, and runs one bounded GC step with cutoff `min(graph_watermark, router_watermark)`. This is durable at-least-once retry/convergence without a finite-time or client-level exactly-once guarantee. MemoryId 2 co-stores the public shard projection and Router-private monotonic Vector attach epoch. Unregister advances the epoch while clearing both readiness bits before the first detach await and retains the exact Vector target for detach retry. Successful existing-row index re-registration clears that target while preserving the fence and restoring only `index_attached`. A new explicit same-target attach advances the epoch again, so every pre-unregister finalizer exact-conflicts and only the new claim may publish readiness. The named Graph-only PocketIC gate passes one test with seven filtered; the persisted Router artifact has all 41 terminal benchmark entries, with `markerless_frontier_catalog` at exactly 52,080,138 total and 52,079,143 scoped instructions. A focused non-persisted Vector frontier-plus-GC run recorded 2,106,603,774 instructions. The dated Plan 0277 focused/persisted baseline records 2,110,300,092 total and 2,110,299,087 scoped instructions; the later live artifact currently records 2,084,327,695 total and 2,084,326,690 scoped instructions from unrelated work and is not Plan 0277 evidence. The targeted affected-crate format check passes; the workspace-wide format check is blocked by unrelated dirty paths and never passed. Fully attached Graph-only lane liveness remains best-effort, with no finite-time or global growth bound claimed ([vector-index.md](../index/vector-index.md), [ADR 0064](../adr/0064-vector-canister-redesign-ownership-layout-fencing-ingest.md)) |
| Router label stats projection             | Graph `LabelStatsDelta`                                           | `advance_label_stats_projection` + per-shard cursor                                                      | **Implemented:** graph delta log replay via `advance_backfill (kind = LabelStats)`; no full historical scan                                                                                                                                                                                                                                                      |
| Router indexed-property catalog           | Property catalog + planner stats                                  | Planner registration                                                                                     | **Stable** — row layout MemoryId 18–19                                                                                                                                                                                                                                                                                                                           |

---

## Graph canister — LARA bundle

`init_graph()` wires **32** consecutive `MemoryId` regions (0–31) into one `DeferredBidirectionalLabeledLaraGraph`. Thread-local: `GRAPH`.

The Router admin composite query `get_stable_memory_stats(graph_name)` forwards to each
registered graph shard's guarded `admin_stable_memory_stats` query. The response reports the
logical `VirtualMemory` WebAssembly-page and byte size of all graph-owned regions, including empty
regions, plus the bucket-rounded allocation and slack estimate for each region. The estimate adds
the two-page custom manager metadata area, but the canister's total physical stable-memory usage
is still reported separately by the IC management API / `icp canister status` because that total
also includes other canister memory. The query is observability only and does not expose mutable
storage handles or alter allocation state.

Graph now uses the custom manager from ADR 0043 with a 4-page default; 8 pages for vertex,
label-bucket, and label-sidecar rows; 16 pages for edge slab/log, aliases,
and durable backlog; 32 pages for inline property bytes; and 64 pages for property and local-unique
values. This is an experimental fresh-layout choice: it keeps small regions below the upstream
128-page default while allowing property-like storage to grow with less extent-count pressure. The custom
manager assigns a bucket size per `MemoryId` while implementing the upstream `ic_stable_structures::Memory` trait.
The first implementation slice is present in `ic-stable-variable-memory-manager`: it persists
per-`MemoryId` policies, allocates append-only variable-sized extents, reconstructs mappings on
reopen, and implements the upstream `ic_stable_structures::Memory` trait. Graph now wires this
manager with an experimental 4/8/16/32/64-page policy; migration and production-capacity validation
remain outstanding. Stable-memory compatibility is not maintained for the current development
layout change; existing development data must be recreated, and production rollout requires the
explicit migration and preflight decision required by ADR 0039.

The manager's global `MAX_EXTENTS = 65,536` currently bounds the all-64-page theoretical
extent-data capacity at 256 GiB; the 500 GiB ICP stable-memory limit is therefore not reachable
by this policy in one MemoryId allocation alone. Capacity planning is shard-local: LARA's two
adjacency orientations, PMA slack, relocation metadata, payloads, and properties share the global
extent budget. The current decision is to retain this extent count and scale out with graph shards;
see [ADR 0043](../adr/0043-per-memory-bucket-size-memory-manager.md#capacity-and-shard-size-decision-2026-07-16).
Capacity-slope benchmarks are in `crates/graph/src/bench/capacity.rs` and are gated behind
`canbench_large`; they cover fixed-width and text properties, distributed and hub-skewed edges,
and delete/reinsert churn. Churn capacity must be evaluated separately from live logical rows
because LARA retains physical free spans for reuse. LARA's labeled PMA benchmark also exposes
The historical quota16/8/4/1 and explicit segment-size comparisons remain recorded in ADR 0001;
the quota Cargo features and production policy variants have been removed. Fresh labeled graphs use
`segment_size = 16` and vertex quota `1`. The tail-headroom
contract derives its boundary from the persisted segment size. The segment16 suite covers
tail-headroom scaling, deferred-maintenance deduplication, and hub/churn across a PMA leaf boundary.
Existing development stable data must be recreated when this policy changes; no compatibility
migration is provided.

Fresh Graph stores use independent initial capacities for the labeled orientations:
`bucket_slots = 1,024`, `edge_slots = 4,096`, and `inline_property_bytes = 65,536` per orientation.
These are allocation hints only; LARA grows and relocates the slabs as data arrives. Reopen uses
the capacities persisted in each region's own header, so changing these defaults does not resize
existing stable memory.

### Forward orientation (canonical adjacency + payloads)

| MemoryId | Symbol                                         | Role                                                                                                                  | Class       | Rebuild |
| -------- | ---------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- | ----------- | ------- |
| 0        | `FWD_VERTICES`                                 | Vertex rows                                                                                                           | canonical   | —       |
| 1        | `FWD_BUCKETS`                                  | Per-vertex labeled edge buckets: edge slab/log locator plus independent inline property bytes slab/log split metadata | canonical   | —       |
| 2        | `FWD_BUCKET_FREE_SPANS`                        | Retired bucket physical spans                                                                                         | maintenance | —       |
| 3        | `FWD_BUCKET_FREE_SPAN_BY_START`                | Bucket free-span index                                                                                                | maintenance | —       |
| 4        | `FWD_EDGE_COUNTS`                              | Per-vertex edge counts                                                                                                | canonical   | —       |
| 5        | `FWD_EDGES`                                    | Edge slab                                                                                                             | canonical   | —       |
| 6        | `FWD_EDGE_LOG`                                 | Edge value log                                                                                                        | canonical   | —       |
| 7        | `FWD_EDGE_SPAN_META`                           | Edge span metadata                                                                                                    | maintenance | —       |
| 8        | `FWD_EDGE_FREE_SPANS`                          | Retired edge physical spans                                                                                           | maintenance | —       |
| 9        | `FWD_EDGE_FREE_SPAN_BY_START`                  | Edge free-span index                                                                                                  | maintenance | —       |
| 10       | `FWD_INLINE_PROPERTY_BYTES_SLAB`               | Dense labeled edge inline property bytes prefix, independently allocated/relocated from edge slab                     | canonical   | —       |
| 11       | `FWD_INLINE_PROPERTY_BYTES_FREE_SPANS`         | Inline property bytes free spans                                                                                      | maintenance | —       |
| 12       | `FWD_INLINE_PROPERTY_BYTES_FREE_SPAN_BY_START` | Inline property bytes free-span index                                                                                 | maintenance | —       |
| 13       | `FWD_INLINE_PROPERTY_BYTES_LOG`                | Ordered inline-property suffix log, independently folded from edge log                                                | canonical   | —       |
| 14       | `FWD_INLINE_PROPERTY_BYTES_BLOBS`              | Large inline property bytes blobs                                                                                     | canonical   | —       |

### Reverse orientation (derived adjacency + payloads)

| MemoryId | Symbol                                                                                 | Role                                                                                           | Class       | Rebuild                                                                   |
| -------- | -------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- | ----------- | ------------------------------------------------------------------------- |
| 15       | `REV_VERTICES`                                                                         | Reverse vertex rows                                                                            | derived     | Co-update + `rebuild_reverse_adjacency`; `check_reverse_adjacency` oracle |
| 16       | `REV_BUCKETS`                                                                          | Reverse buckets with independent edge/inline property bytes slab-log split metadata            | derived     | Co-update + `rebuild_reverse_adjacency`                                   |
| 17–18    | `REV_BUCKET_FREE_SPANS`, `REV_BUCKET_FREE_SPAN_BY_START`                               | Reverse bucket maintenance                                                                     | maintenance | —                                                                         |
| 19       | `REV_EDGE_COUNTS`                                                                      | Reverse edge counts                                                                            | derived     | Co-update + `rebuild_reverse_adjacency`                                   |
| 20       | `REV_EDGES`                                                                            | Reverse edge slab                                                                              | derived     | Co-update + `rebuild_reverse_adjacency`                                   |
| 21       | `REV_EDGE_LOG`                                                                         | Reverse edge log                                                                               | derived     | Co-update + `rebuild_reverse_adjacency`                                   |
| 22–24    | `REV_EDGE_SPAN_META`, `REV_EDGE_FREE_SPANS`, `REV_EDGE_FREE_SPAN_BY_START`             | Reverse edge maintenance                                                                       | maintenance | —                                                                         |
| 25       | `REV_INLINE_PROPERTY_BYTES_SLAB`                                                       | Reverse inline property bytes prefix, independently allocated/relocated from reverse edge slab | derived     | Co-update + `rebuild_reverse_adjacency`                                   |
| 26–27    | `REV_INLINE_PROPERTY_BYTES_FREE_SPANS`, `REV_INLINE_PROPERTY_BYTES_FREE_SPAN_BY_START` | Reverse inline property bytes free-span maintenance                                            | maintenance | —                                                                         |
| 28       | `REV_INLINE_PROPERTY_BYTES_LOG`                                                        | Reverse ordered inline property bytes suffix log, independently folded from reverse edge log   | derived     | Co-update + `rebuild_reverse_adjacency`                                   |
| 29       | `REV_INLINE_PROPERTY_BYTES_BLOBS`                                                      | Reverse inline property bytes blobs                                                            | derived     | Co-update + `rebuild_reverse_adjacency`                                   |

### LARA maintenance

| MemoryId | Symbol              | Role                    | Class       | Rebuild             |
| -------- | ------------------- | ----------------------- | ----------- | ------------------- |
| 30       | `MAINTENANCE_QUEUE` | Deferred PMA work queue | maintenance | Internal LARA drain |

MemoryId 31 (`DIRTY_WORK_ITEMS`) was retired in ADR 0052 Slice 6: the priority-ordered
`StableBTreeMap` maintenance queue replaced the `StableRoaringBitmap` dirty-dedup set, so the
region is reserved and unallocated.

### Former adaptive counterpart regions (removed)

ADR 0048 now has one production counterpart algorithm: metadata-free `CounterpartScan` over
canonical live adjacency. The pre-deployment MATE locator/blob/free-span substrate, its feature-
gated fixtures, and its adoption/compression benchmarks were removed. MemoryIds 47–50 remain
reserved registry holes and are not allocated or silently reused.

| MemoryId | Symbol        | Role                              | Class       | Rebuild |
| -------- | ------------- | --------------------------------- | ----------- | ------- |
| 47       | `RESERVED_47` | Former counterpart locator region | maintenance | —       |
| 48       | `RESERVED_48` | Former counterpart blob region    | maintenance | —       |
| 49       | `RESERVED_49` | Former counterpart free spans     | maintenance | —       |
| 50       | `RESERVED_50` | Former counterpart span index     | maintenance | —       |

Historical Plans 0139–0175 and their benchmark numbers remain in the repository as historical
evidence, but they no longer describe active storage or implementation requirements.

## Graph canister — facade regions

Repacked 2026-06-11. **Removed:** property name catalog, `VERTEX_LOGICAL_IDS`, federation remote-ref stable (`REMOTE_VERTEX_REFS`, `REMOTE_FORWARD_IN`), `PEER_GRAPH_CANISTERS`. LARA ids are consecutive **0–31**; facade starts at **32**.

The manual stable records in regions 38 and 39 retain their fixed primary
headers and length-delimited appendices, but no longer use fixed appendix item
counts. Their encoded values are checked against the shared logical payload
limit during encode and decode; the `Unbounded` stable-structure bound remains
an allocation strategy, not an admission bypass.

| MemoryId | Symbol                      | Thread-local                | Init fn                         | Class       | Owner domain                  | Rebuild                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| -------- | --------------------------- | --------------------------- | ------------------------------- | ----------- | ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 32       | `VERTEX_LABEL_SETS`         | `VERTEX_LABELS`             | `init_vertex_label_store`       | canonical   | labels                        | Value is versioned (`VertexLabelSetBlob`): `[layout_version u8 = 1][sorted, deduped u16 LE label ids...]`; unknown versions and missing/truncated payloads panic at decode. |
| 33       | `VERTEX_PROPERTIES`         | `VERTEX_PROPERTIES`         | `init_vertex_property_store`    | canonical   | properties                    | Value is versioned (`StoredPropertyValue::V1`): `[layout_version u8 = 1][gql-value binary bytes]`; unknown versions and truncated payloads panic at decode. |
| 34       | `EDGE_PROPERTIES`           | `EDGE_PROPERTIES`           | `init_edge_property_store`      | canonical   | properties                    | Value is versioned (`StoredPropertyValue::V1`): `[layout_version u8 = 1][gql-value binary bytes]`; unknown versions and truncated payloads panic at decode. |
| 35       | _(reserved)_                | _(none)_                    | _(none)_                        | reserved    | —                             | Former `EDGE_ALIASES`; intentionally not allocated or reused after Plan 0192                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| 36       | `GRAPH_METADATA`            | `METADATA`                  | `init_metadata`                 | canonical   | federation metadata           | —                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| 37       | `LABEL_STATS_DELTA_SEQ`     | `LABEL_STATS_DELTA_SEQ`     | `init_label_stats_delta_seq`    | telemetry   | label stats projection        | Monotonic seq allocator                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| 38       | `LABEL_STATS_DELTA_LOG`     | `LABEL_STATS_DELTA_LOG`     | `init_label_stats_delta_log`    | telemetry   | label stats projection        | Delta replay to router. Value is a versioned fixed-length primary + variable appendix (`StoredLabelStatsDeltaEvent`), not Candid; bounds are enforced at encode time. See ADR 0015 Plan 0120 addendum.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| 39       | `GRAPH_MUTATION_JOURNAL`    | `GRAPH_MUTATION_JOURNAL`    | `init_graph_mutation_journal`   | canonical   | idempotency                   | Mutation outcome + emitted delta seq range. Value is a versioned fixed-length primary + optional appendix (`GraphMutationJournalEntry`), not Candid; bounds are enforced at encode and decode time. See ADR 0027 Plan 0120 addendum. Pre-0120 Candid bytes are discarded at the fresh-install boundary; no legacy decoder or in-place migration exists. A single-DML mutation is recorded `Completed` even when its index flush is deferred to the repair journal (region 41), since the store mutation + deltas are durable and the index converges async (ADR 0024). Bounded by [ADR 0027](../adr/0027-graph-mutation-journal-retention.md): every entry carries `recorded_at_ns`; implemented `GraphMutationRequestIdentityV1::PlanExecution` entries use the 9d age predicate. ADR 0049 and ADR 0057 replace V1 in place with request-kind identity plus stable `NotApplicable | Active | Retired { at_ns }`retirement state; journal wire reads expose the derived timestamp-free enum.`GraphMutationRequestIdentityV1::PlanExecution`requires`NotApplicable`, while `GraphMutationRequestIdentityV1::OrderedEdgeBatch`uses`Active`/`Retired`and is never GC-eligible until the authenticated fingerprint-bound Router retirement transition sets`Retired`. Retired ordered entries use the same 9d retention measured from `Retired.at_ns`. Amortized write-path GC (**B**, heap round-robin cursor) and any future operator sweep share this predicate and cannot force-remove an active ordered entry. The sweep is skipped while the journal length stays below a heap-only minimum threshold, avoiding the fixed stable B-tree range-cursor cost for short journals; it resumes once the length reaches the threshold. Ack-through-seq is **not** used as the eviction trigger (unsound: no-delta mutations, shard-global cursor, ack precedes router completion) |
| 40       | `PENDING_VERTEX_PURGES`     | `PENDING_VERTEX_PURGES`     | `init_pending_vertex_purges`    | maintenance | vertex delete                 | Tombstoned vertices mid-purge (ADR 0021); insert is fail-closed and runs before the tombstone (a failed insert aborts the delete, never leaving ungated ghost edges); rebuildable by scanning tombstoned vertices with surviving incident edges (no API)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| 41       | `INDEX_REPAIR_JOURNAL`      | `INDEX_REPAIR_JOURNAL`      | `init_index_repair_journal`     | maintenance | federated index repair        | **`sequence → RepairJournalEntry { mutation_id, op }`**. Failed-flush index postings persisted on compensation-success (ADR 0023 D5); re-applied by the maintenance driver each tick and on `post_upgrade`, removed on success. Each nonzero mutation id owns one exact region-52 floor key for its source sequence; zero is the untracked sentinel. GraphStore co-updates the journal row and key.                                                                                                                                                                                                      |
| 42       | `UNIQUE_EFFECT_OUTBOX`      | `UNIQUE_EFFECT_OUTBOX`      | `init_unique_effect_outbox`     | canonical   | cross-shard uniqueness        | **`EffectId { mutation_id, effect_ordinal } → UniqueEffectReceipt { claim_id?, owner_element_id, constraint_id, encoded_value, op: Acquire \| Release }`** (ADR 0030). Pinned commit evidence: each unique-affecting canonical segment appends one receipt per effect; an effect stays pinned until the Router acks its `EffectId` (per-effect, never unpinning a sibling). Canonical: a `Reserved`-not-yet-committed claim has no receipt, so un-acked-effect _absence_ is authoritative proof of non-commit — decoupled from the 9-day journal eviction (region 39 / ADR 0027). Replicated `Acquire`-by-`ClaimId` proof read + per-effect ack are Router-only update endpoints. Append is idempotent across deterministic replays. Emit wiring into the DML segment lands in slice 5                                                                                             |
| 43       | `GRAPH_LOCAL_UNIQUE_VALUES` | `GRAPH_LOCAL_UNIQUE_VALUES` | `init_graph_local_unique_table` | canonical   | shard-local global uniqueness | **`(constraint_id, encoded_value) → LocalUniqueRecord { owner_element_id }`** (ADR 0030 slice 10). Canonical source of truth for `ShardLocalGlobal` unique constraints: a graph proven single-shard at CREATE enforces graph-wide uniqueness entirely in its one owning shard's local table, bypassing the Router reservation/`UNIQUE_EFFECT_OUTBOX` path. The acquire path preflights all claims and inserts them inside the canonical write segment (all-or-nothing); a delete/remove frees the value by owner match; the DROP drain purges the constraint's entries and gates `Removed` on the range being empty. The key omits `graph_id` because one graph canister hosts exactly one graph/shard. No `Acquire`/`Release` receipts and no Router reservations are ever written for these constraints                                                                          |
| 44       | `RESERVED_44`               | —                           | —                               | reserved    | reserved                      | Former `VERTEX_EMBEDDINGS` canonical embedding region; intentionally unallocated after Vector became the owner of indexed embedding bytes (ADR 0064 §1). Router MemoryId 53 temporarily owns pending/retry payload bytes until exact marker retirement; the graph holds zero embedding state                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| 45       | `RESERVED_45`               | —                           | —                               | reserved    | reserved                      | Former `VERTEX_EMBEDDING_INCARNATIONS` delete-spanning incarnation region; intentionally unallocated after the `mutation_id` ordering fence replaced the incarnation clock (ADR 0064 §5)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| 46       | `DERIVED_INDEX_OUTBOX`      | `DERIVED_INDEX_OUTBOX`      | `init_derived_index_outbox`     | maintenance | federated index delivery      | **`sequence → Pending(DerivedIndexOutboxEntry { mutation_id, op }) \| Quarantined { entry, reason: IndexDefinitionTablePressure \| SubjectTablePressure }`** (Plan 0088; ADR 0067 Graph quarantine slice). Maintenance passes pending suffixes to target batch endpoints. A validated terminal outcome removes its exact acknowledged prefix and quarantines its exact failed vector row; pending scans and scheduling hide quarantine so it is neither retried nor used to re-arm a quarantine-only timer, while raw durable length/emptiness still expose quarantine to status. Each nonzero `Ordinary` row owns one exact region-52 floor key for its source sequence; quarantine retains the key, acknowledgement removes it, and `IndexBuildDml` owns none. GraphStore co-updates owner rows and floor keys. DML-only ad-hoc/native blocks use the outbox, while DML followed by an in-block read retains the synchronous flush boundary. |
| 51       | `CANONICAL_EXPORT_SCOPES`   | `CANONICAL_EXPORT_SCOPES`   | `init_canonical_export_scopes`  | canonical   | canonical property export     | **`PhysicalIndexId → CanonicalExportRecord`** (ADR 0059). Immutable Graph-owned export scope (graph/name/epoch/target/inline projection) plus durable phase (Building/Sealing/Active/Aborting) and admission/drain sequence watermarks per physical namespace.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| 52       | `INDEX_PENDING_FLOOR`       | `INDEX_PENDING_FLOOR`       | `init_index_pending_floor`      | derived     | AtLeast graph-index barrier   | **`IndexPendingFloorKey { mutation_id: u64 BE, owner: u8, source_sequence: u64 BE } → [u8; 0]`**. Fixed 17-byte keys use owner tags `1 = RepairJournal`, `2 = DerivedIndexOutbox`; zero mutation ids and unknown tags are invalid. The first key is the exact public `index_pending_min_mutation_id()` while one key remains per qualifying durable source row. Registered as `StableMemoryClass::Derived` with `RebuildPath::SyncCoUpdate`; no independent rebuild or readiness state exists. |

Graph facade **17 allocated regions** total (32 LARA + 17 facade; MemoryIds 32–46, `CANONICAL_EXPORT_SCOPES` at 51, and `INDEX_PENDING_FLOOR` at 52, with 47–50 reserved), exposing 53 numbered regions through max id 52. `DERIVED_INDEX_OUTBOX` (46) is the durable FIFO handoff for successful Graph DML; target batch endpoints determine the safe progress prefix and the shared dispatcher removes only acknowledged entries. GraphStore synchronously co-updates qualifying owner rows in regions 41/46 with exact floor keys in region 52. DML-only ad-hoc/native blocks use the outbox; blocks that read after DML retain a synchronous flush boundary for read-after-write visibility. Retired 2026-06-12: `EDGE_PAYLOAD_PROFILES` → router SSOT ([ADR 0008](../adr/0008-edge-inline-property-profile-router-ssot.md)); `EDGE_EQUALITY_POSTINGS` → graph-index ([ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md)).

Property **names** are router-owned (`ROUTER_PROPERTY_CATALOG`); graph stores values by `PropertyId` only.

### Graph ephemeral (not in `memory.rs`)

| Symbol                        | Location                           | Role                           | Reopen behavior                                                                                                                                                          |
| ----------------------------- | ---------------------------------- | ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `PENDING` (property postings) | `graph/src/index/pending.rs`       | Queued property index ops      | In-flight batch lost on upgrade; failed batches persist to `INDEX_REPAIR_JOURNAL` (ADR 0023 D5); `backfill_vertex_property_postings` covers historical vertex properties |
| `PENDING` (edge postings)     | `graph/src/index/edge_pending.rs`  | Queued edge property index ops | In-flight batch lost on upgrade; failed batches persist to `INDEX_REPAIR_JOURNAL` (ADR 0023 D5); `backfill_edge_property_postings` covers historical edge properties     |
| `PENDING` (label postings)    | `graph/src/index/label_pending.rs` | Queued label index ops         | In-flight batch lost on upgrade; failed batches persist to `INDEX_REPAIR_JOURNAL` (ADR 0023 D5); `backfill_label_postings` covers historical labels                      |

---

## Router canister — stable regions

Repacked 2026-06-17: placement removed, controllers merged into auth, MemoryIds compacted to **0–33** (34 regions). ADR 0030 appended the uniqueness constraint catalog (**34–36**), the uniqueness reservation table (**37**), and the slice-6 reservation reverse index (**38**) and pending unique-effect discovery index (**39**), bringing the total to 40 regions (0–39). ADR 0031 Slice 3 appended the graph-scoped embedding-name catalog (**40–41**) and the derived vector-index definition catalog (**42**), and Slice 4 appended the vector dispatch activation flag (**43**), and Slice 10 appended the vector maintenance policy catalog (**44**), bringing the total to **45 regions (0–44)**. Regions grouped **auth → registry → runtime config → idempotency → catalog → telemetry → maintenance → constraint catalog → reservation table → reservation reverse index → pending-effect discovery → embedding-name catalog → vector-index catalog → vector dispatch activation → vector maintenance policy → provisioning → direct vector-ingestion outbox**. `ROUTER_GRAPHS` keyed by **`GraphId`**; the public `ShardRegistryEntry` projection stores **`graph_id: GraphId`** inside the Router-private `RouterShardState`. `ROUTER_SHARDS` keyed by **`GraphShardKey { graph_id, shard_id }`**; `ROUTER_SHARD_BY_GRAPH` is **`Principal → GraphShardKey`**; shard listing per logical graph uses **`ROUTER_SHARDS_BY_GRAPH_ID`**.

| MemoryId | Symbol                          | Thread-local                  | Init fn                     | Class         | Owner domain   | Rebuild                                                                                                                             |
| -------- | ------------------------------- | ----------------------------- | --------------------------- | ------------- | -------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| 0        | `ROUTER_AUTH_PRINCIPAL_RECORDS` | `ROUTER_AUTH_STATE`           | `init_auth_state`           | canonical     | auth           | SSOT for router principal roles (`Role::Admin` for ops)                                                                             |
| 1        | `ROUTER_GRAPHS`                 | `ROUTER_GRAPHS`               | `init_graphs`               | canonical     | registry       | **`BTreeMap<GraphId, GraphRegistryEntry>`** — graph registry SSOT                                                                   |
| 2        | `ROUTER_SHARDS`                 | `ROUTER_SHARDS`               | `init_shards`               | canonical     | registry       | **`GraphShardKey → RouterShardState { entry: ShardRegistryEntry, vector_attach_epoch: u64 }`** — shard dispatch SSOT plus the Router-private same-row Vector attach fence. New claims advance the epoch; same-target concurrent duplicates share it; unregister advances it before await. This current Candid value is the sole layout: fresh reinstall required, with no migration, legacy/default decoder, or fallback ([ADR 0019](../adr/0019-graph-local-shard-id-and-index-clusters.md); [ADR 0064](../adr/0064-vector-canister-redesign-ownership-layout-fencing-ingest.md)) |
| 3        | `ROUTER_SHARD_BY_GRAPH`         | `ROUTER_SHARD_BY_GRAPH`       | `init_shard_by_graph`       | derived index | registry       | **`Principal → GraphShardKey`** — denormalized from `ROUTER_SHARDS`; commit-synced                                                  |
| 4        | `ROUTER_SHARDS_BY_GRAPH_ID`     | `ROUTER_SHARDS_BY_GRAPH_ID`   | `init_shards_by_graph_id`   | derived index | registry       | **`GraphId → Vec<ShardId>`** — active graph-local shard fan-out; monotonic insertion order may be sparse after retirement           |
| 5        | `ROUTER_GRAPH_RUNTIME_CONFIG`   | `ROUTER_GRAPH_RUNTIME_CONFIG` | `init_graph_runtime_config` | canonical     | runtime config | `GraphId → GraphRuntimeConfig` (`element_id_encoding_key`, `index_group_size`, `index_cluster`, `next_shard_id`; ADR 0019 S2b/S3/S6) |

### Registry denormalization invariants (implemented 2026-06-17)

Regions **1–2** (canonical), **3–4** (derived indexes), **`ROUTER_GRAPH_RUNTIME_CONFIG` (5)**, plus **`ROUTER_GRAPH_CATALOG` (15–16)** form an intentional denormalized lookup set — not a merge candidate. Federation dispatch depends on all six staying synchronized at each registry **commit** boundary.

| Region                        | Class                     | Role in invariant                                                      |
| ----------------------------- | ------------------------- | ---------------------------------------------------------------------- |
| `ROUTER_GRAPH_CATALOG`        | catalog (registry commit) | name ↔ `GraphId`                                                       |
| `ROUTER_GRAPHS`               | canonical                 | `GraphId` → `GraphRegistryEntry` (RBAC, status, `is_home`)             |
| `ROUTER_GRAPH_RUNTIME_CONFIG` | canonical                 | `GraphId` → `GraphRuntimeConfig` (encoding key, index cluster, never-reuse shard-id high-water) |
| `ROUTER_SHARDS`               | canonical                 | `GraphShardKey` → `RouterShardState` — public dispatch projection plus private Vector attach epoch SSOT |
| `ROUTER_SHARDS_BY_GRAPH_ID`   | derived index             | `GraphId` → `[ShardId]` — active graph-local fan-out; may be sparse    |
| `ROUTER_SHARD_BY_GRAPH`       | derived index             | `Principal` → `GraphShardKey` — graph canister uniqueness              |

**Commit APIs** (`RouterStore::commit_register_graph`, `commit_register_shard`, `commit_unregister_shard` in `crates/router/src/facade/store/registry.rs`) update the affected regions atomically from the domain owner's perspective. **`commit_register_shard`** requires a matching `ROUTER_GRAPHS` entry, accepts only the exact `next_shard_id`, and advances that high-water in the same commit. Unregister and failed-attach rollback remove live rows without rewinding it. The same MemoryId-2 `RouterShardState` co-owns the public `ShardRegistryEntry` projection and the private monotonic Vector attach epoch: a new claim advances it, an in-flight same-target duplicate shares it, unregister advances it before the first detach await, and the post-await finalizer exact-checks it before publishing readiness. **`check_registry_invariants`** (`registry_invariants.rs`) verifies full bidirectional consistency, requires every active `ShardId` to be below the graph's high-water, and rejects a Vector target without a nonzero attach epoch; unit tests call it after every registry mutation. Per-commit verification is disabled in production for cost (`verify_registry_invariants_after_commit` is a no-op outside tests), so the same read-only oracle is also exposed as the admin query **`check_registry_invariants`** (`Role::Admin`) — used to confirm registry consistency on demand. E2E coverage: `pocket-ic-tests/tests/router_registry_invariants.rs`. **`list_shards_for_graph_id`** / **`list_shards_for_graph`** walk `ROUTER_SHARDS_BY_GRAPH_ID` only (O(shards for graph)), project **`Vec<ShardRegistryEntry>`** from `ROUTER_SHARDS`, and reject duplicate index ids and stale index→primary references; they do not full-scan `ROUTER_SHARDS` — missing index rows are caught on commit / by `check_registry_invariants`.

| 6 | `ROUTER_MUTATION_COUNTER` | `ROUTER_MUTATION_COUNTER` | `init_mutation_counter` | canonical | idempotency | — |
| 7 | `ROUTER_MUTATION_BY_CLIENT_KEY` | `ROUTER_MUTATION_BY_CLIENT_KEY` | `init_mutation_by_client_key` | canonical | idempotency | keys use **`graph_id: GraphId`**; value is `RouterMutationRecord::V1` encoded directly (no double V1 wrapper). The exhaustive current identity/payload layout includes scalar plan execution, ordered atomic edge/vertex/mixed replay, their terminal result variants, and ADR 0057 `BulkLoadJob` / `BulkLoadCoordinator`; retired typed/shared seed-bulk variants are not decoded. Bulk parents retain pinned placement, lifecycle, aggregate counters, terminal anchor, and a receipt-GC cursor; receipt rows live only in MemoryId 49. Bounded by [ADR 0025](../adr/0025-client-mutation-journal-retention-sweep.md): terminal records use `terminal_at_ns` as the seven-day retention anchor, while non-terminal records remain ineligible. Ordinary expired records are evicted by amortized write-path GC plus the operator sweep; expired bulk parents instead drive bounded child receipt GC and are removed only after an empty child-range proof. The outer `RouterMutationRecord::V1` envelope remains the sole durable version envelope. |
| 8 | `ROUTER_PREPARED_PLANS` | `ROUTER_PREPARED_PLANS` | `init_prepared_plans` | canonical | prepared queries | **`PreparedPlanKey → PreparedPlanRecord::V1`** |
| 9–10 | `ROUTER_VERTEX_LABEL_BY_NAME` / `ROUTER_VERTEX_LABEL_BY_ID` | `ROUTER_VERTEX_LABEL_CATALOG` | `init_vertex_label_catalog` | catalog | resolution | **`GraphScopedNameCatalog<VertexLabelId>`** — `(GraphId, name) ↔ id` ([ADR 0018](../adr/0018-graph-scoped-label-property-catalogs.md)) |
| 11–12 | `ROUTER_EDGE_LABEL_BY_NAME` / `ROUTER_EDGE_LABEL_BY_ID` | `ROUTER_EDGE_LABEL_CATALOG` | `init_edge_label_catalog` | catalog | resolution | **`GraphScopedNameCatalog<EdgeLabelId>`** (dense, capped) |
| 13–14 | `ROUTER_PROPERTY_BY_NAME` / `ROUTER_PROPERTY_BY_ID` | `ROUTER_PROPERTY_CATALOG` | `init_property_catalog` | catalog | resolution | **`GraphScopedNameCatalog<PropertyId>`** (dense) |
| 15–16 | `ROUTER_GRAPH_BY_NAME` / `ROUTER_GRAPH_BY_ID` | `ROUTER_GRAPH_CATALOG` | `init_graph_catalog` | catalog | resolution | Logical graph name ↔ **`GraphId`** ([ADR 0011](../adr/0011-gql-graph-resolution-and-catalog-scoping.md)) |
| 17–18 | `ROUTER_INDEX_NAME_BY_NAME` / `ROUTER_INDEX_NAME_BY_ID` | `ROUTER_INDEX_NAME_CATALOG` | `init_index_name_catalog` | catalog | resolution | Graph-scoped index name ↔ **`IndexNameId`** per `GraphId` |
| 19 | `ROUTER_NAMED_INDEXES` | `ROUTER_NAMED_INDEXES` | `init_named_indexes` | catalog | index DDL metadata | **`(GraphId, IndexNameId) → IndexDefRecord`** (ADR 0059): immutable resolved graph/property/direction/`PhysicalIndexId` identity plus Preparing, Building, Sealing, Active, or Aborting target progress. This row is the detailed lifecycle SSOT; planner membership is derived only from Active rows. |
| 20 | `ROUTER_NEXT_PHYSICAL_INDEX_ID` | `ROUTER_NEXT_PHYSICAL_INDEX_ID` | `init_next_physical_index_id` | canonical | index lifecycle allocator | **`() → u64`** (ADR 0059). Monotonic physical-index namespace allocator cell (starts at 1; never rewound, never reused). Replaces the retired `ROUTER_INDEXED_PROPERTY_SET` membership set in place. |
| 21 | `ROUTER_EDGE_INLINE_PROPERTY_PROFILES` | `ROUTER_EDGE_INLINE_PROPERTY_PROFILES` | `init_edge_inline_property_profiles` | catalog | edge inline property schema | **`(GraphId, EdgeLabelId) → EdgeInlinePropertySchemaRecord`** ([ADR 0008](../adr/0008-edge-inline-property-profile-router-ssot.md), [ADR 0018](../adr/0018-graph-scoped-label-property-catalogs.md), [ADR 0034 Slice 20 + Slice 24](../adr/0034-gleaph-gql-extension-syntax.md)) |
| 22–23 | `ROUTER_GRAPH_TYPE_DEFINITIONS` / `ROUTER_GRAPH_SCHEMA_BINDINGS` | `ROUTER_GQL_GRAPH_CATALOG` | `init_gql_graph_catalog` | catalog | graph type catalog | versioned source strings + **`GraphId` bindings**; parsed `GraphTypeDefinition` values are heap caches ([ADR 0013](../adr/0013-gql-graph-type-catalog-on-router.md)) |
| 24–25 | `ROUTER_GRAPH_TYPE_BY_NAME` / `ROUTER_GRAPH_TYPE_BY_ID` | `ROUTER_GRAPH_TYPE_CATALOG` | `init_graph_type_name_catalog` | catalog | resolution | Graph type name ↔ **`GraphTypeId`** ([ADR 0014](../adr/0014-graph-type-id-catalog-on-router.md)) |
| 26 | `ROUTER_VERTEX_LABEL_STATS` | `ROUTER_VERTEX_LABEL_STATS` | `init_vertex_label_stats` | telemetry | label telemetry | **`(GraphId, VertexLabelId) → LabelStats`** (event replay) |
| 27 | `ROUTER_EDGE_LABEL_STATS` | `ROUTER_EDGE_LABEL_STATS` | `init_edge_label_stats` | telemetry | label telemetry | **`(GraphId, EdgeLabelId) → LabelStats`** (event replay) |
| 28 | `ROUTER_VERTEX_LABEL_LIVE_BY_SHARD` | `ROUTER_VERTEX_LABEL_LIVE_BY_SHARD` | `init_vertex_label_live_by_shard` | telemetry | label telemetry | **`(GraphId, ShardId, VertexLabelId) → live_count`** |
| 29 | `ROUTER_EDGE_LABEL_LIVE_BY_SHARD` | `ROUTER_EDGE_LABEL_LIVE_BY_SHARD` | `init_edge_label_live_by_shard` | telemetry | label telemetry | **`(GraphId, ShardId, EdgeLabelId) → live_count`** |
| 30 | `ROUTER_LABEL_STATS_PROJECTION` | `ROUTER_LABEL_STATS_PROJECTION` | `init_label_stats_projection` | telemetry | label stats projection | **`GraphShardKey → applied_through_seq`** |
| 31 | `ROUTER_LABEL_BACKFILL_STATE` | `ROUTER_LABEL_BACKFILL_STATE` | `init_label_backfill_state` | maintenance | label backfill | **`GraphShardKey → BackfillShardState`**, one entry per shard (bounded by live shard count, not operations); the step's TOCTOU concurrency guard is a heap-local `INFLIGHT_BACKFILL` claim set (not stable — auto-released on upgrade), so the stable record is unchanged; entry purged when its shard retires |
| 32 | `ROUTER_VERTEX_PROPERTY_BACKFILL_STATE` | `ROUTER_VERTEX_PROPERTY_BACKFILL_STATE` | `init_vertex_property_backfill_state` | maintenance | vertex property backfill | **`GraphShardKey → BackfillShardState`**; same heap-local claim guard and `unregister_shard` purge as region 31 |
| 33 | `ROUTER_EDGE_BACKFILL_STATE` | `ROUTER_EDGE_BACKFILL_STATE` | `init_edge_backfill_state` | maintenance | edge backfill | **`GraphShardKey → EdgeBackfillShardState`**; same heap-local claim guard and `unregister_shard` purge as region 31 |
| 34–35 | `ROUTER_CONSTRAINT_NAME_BY_NAME` / `ROUTER_CONSTRAINT_NAME_BY_ID` | `ROUTER_CONSTRAINT_NAME_CATALOG` | `init_constraint_name_catalog` | catalog | resolution | Graph-scoped constraint name ↔ **`ConstraintNameId`** per `GraphId` ([ADR 0030](../adr/0030-cross-shard-uniqueness-tcc-reservation.md)) |
| 36 | `ROUTER_UNIQUE_CONSTRAINTS` | `ROUTER_UNIQUE_CONSTRAINTS` | `init_unique_constraints` | catalog | constraint DDL metadata | **`(GraphId, ConstraintNameId) → ConstraintDefStableRecord::V1(ConstraintDefRecord { vertex_label_id, property_id, state: ConstraintLifecycle { Active \| Dropping }, dropping_at_ns, drop_scan_generation })`** — logical uniqueness constraint definitions, declare-on-empty (ADR 0030, first cut: vertex single-property). Versioned envelope (ADR 0030 Revision #18, Slice 9): the DROP lifecycle adds `state`/`dropping_at_ns`/`drop_scan_generation`; `Removed` is the terminal **absence** of the record (deleted by recovery once the completion gate holds), not a persisted enum value |
| 37 | `ROUTER_UNIQUE_RESERVATIONS` | `ROUTER_UNIQUE_RESERVATIONS` | `init_unique_reservations` | canonical | uniqueness reservation table | **`(GraphId, ConstraintNameId, encoded_value) → ReservationRecord { claim, state, reclaim_generation, owner_element_id, reserved_at_ns, proof_scope }`** — cross-shard uniqueness TCC claims (ADR 0030). Canonical: a `Reserved`-not-yet-`Committed` claim has no outbox receipt to rebuild from. Slice 3 implements the no-`await` Try; Confirm/Cancel + reclaim land in slices 4–6 |
| 38 | `ROUTER_MUTATION_RESERVATION_INDEX` | `ROUTER_MUTATION_RESERVATION_INDEX` | `init_mutation_reservation_index` | canonical | uniqueness reservation reverse index | **`MutationId → MutationReservationIndexEntry { client_key, nonterminal }`** — reverse index that resolves a reservation's claim (`MutationId`) to its owning `RouterMutationRecord` and GC-pins that record while non-terminal reservations remain (ADR 0030 slice 6). The row exists **iff** `nonterminal > 0`: `++` per fresh insert at Try, `--` on `FreshlyCommitted` Confirm and on reclaim Cancel; removed at zero |
| 39 | `ROUTER_UNIQUE_EFFECT_PENDING` | `ROUTER_UNIQUE_EFFECT_PENDING` | `init_unique_effect_pending` | canonical | pending unique-effect discovery index | **`(GraphId, MutationId, ShardId) → PendingEffectRecord { schema_version, canister, client_key, state: Active \| Quarantined, next_retry_ns, attempts, diagnostic? }`** — durable discovery source for Driver 2's unified effect recovery (ADR 0030 slice 6). Registered before the first dispatch `await` for any dispatch that may emit a unique `Acquire`/`Release`, so it co-commits with the reservation/envelope. The pinned `canister` is the row's immutable identity, stored verbatim (not re-derived from the shard registry) so recovery reaches the exact canister even after the shard is unregistered/reused; `register` is **fail-closed** (re-registering the same key to a different canister traps). `client_key` is the owning `ClientMutationKey`, stored so Driver 2 resolves the `RouterMutationRecord` (its terminal-completion proof) for **any** effect kind — a `Release` or orphan `Acquire` owns no reservation, so the reverse index (region 38) cannot resolve them; the row GC-pins that record while it exists. Driver 2 only drains a row once its mutation is terminal, classifies a reservation-less `Acquire` as an orphan only then, and removes the row only after a fresh `cursor=None` re-scan is empty. This is the only handle to an orphan `Acquire` whose reservation is gone (parked `Quarantined` on a long backoff, never acked). The value is a **versioned record** so the diagnostic/quarantine/backoff fields evolve without a breaking value-layout change. While any row remains the owning `RouterMutationRecord` is **GC-pinned** (Driver 2 reads its completion state); a row is removed only after the shard re-enumerates the mutation's effects empty (all acked) |
| 40–41 | `ROUTER_EMBEDDING_NAME_BY_NAME` / `ROUTER_EMBEDDING_NAME_BY_ID` | `ROUTER_EMBEDDING_NAME_CATALOG` | `init_embedding_name_catalog` | catalog | resolution | Graph-scoped embedding name ↔ **`EmbeddingNameId`** per `GraphId` ([ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md) Slice 3). The Router is the sole allocator; vector-index registration interns **by name** so the stored id is the one supplied to Graph validation stamps. Graph does not persist canonical embedding bytes. Purged with the graph on `unregister_graph` |
| 42 | `ROUTER_VECTOR_INDEXES` | `ROUTER_VECTOR_INDEXES` | `init_vector_indexes` | catalog | derived vector index catalog | **`(GraphId, index_id) → VectorIndexDefStableRecord::V2(VectorIndexDefRecord { index_id, index_name_id, embedding_name_id, labels, kind, metric, encoding, dims, target: Option<VectorIndexTarget { canister }>, activation_state })`** — Router-owned vector-index definitions (ADR 0065 / ADR 0064). `index_name_id` is the logical index name while `embedding_name_id` identifies the typed embedding field. `VectorIndexDefStableRecord::V2` is the only current shape. The stored `activation_state` is `Registered`/`DispatchBlocked`; **`DispatchEnabled` is derived at read time** by `graph_vector_dispatch_ready(graph_id)` (global flag region 43 **AND** every live shard vector-attached to the graph's single target). Registration enforces **one index per embedding name per graph** and **one target per graph**. Purged with the graph on `unregister_graph` |
| 43 | `ROUTER_VECTOR_DISPATCH_ACTIVATION` | `ROUTER_VECTOR_DISPATCH_ACTIVATION` | `init_vector_dispatch_activation` | canonical | vector dispatch activation flag | **`() → bool`** (single-cell, default `false`) — Router-owned global vector-dispatch activation flag ([ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md) Slice 4). Set by `set_vector_dispatch_enabled` / read by `get_vector_dispatch_enabled` (RBAC admin). Replaces the retired `const fn incarnation_fencing_enabled()`. Necessary but not sufficient: per-graph emission is additionally fenced on every live shard's `vector_index_attached` bit. Reversible without a redeploy |
| 44 | `ROUTER_VECTOR_MAINTENANCE_POLICIES` | `ROUTER_VECTOR_MAINTENANCE_POLICIES` | `init_vector_maintenance_policies` | catalog | vector maintenance policy catalog | **`(GraphId, index_id) → VectorMaintenancePolicyStableRecord::V1(VectorMaintenancePolicyRecord { enabled, policy, target_nlist, sample_limit, scan_max_pages, rebuild_max_subjects, cleanup_max_work })`** — Router-owned SSOT for vector maintenance thresholds + per-step budgets ([ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md) Slice 10). Versioned envelope (ADR 0007), **default absent/disabled** (the push step is a no-op until set + enabled). Authorship is `authorize_index_ddl` (validated: `recommended_*_bps <= required_*_bps`, nonzero budgets, def exists); `advance_vector_maintenance` snapshots it and forwards one bounded unit to the vector canister (which owns the execution state). Purged with the graph on `unregister_graph` |
| 45 | `ROUTER_PROVISIONING_REQUESTS` | `ROUTER_PROVISIONING_REQUESTS` | `init_provisioning_requests` | canonical | provisioning request catalog | **`(request_id, deployment_id) → RouterProvisioningRequest`** (ADR 0035). Canonical immutable caller/owner/admin identity, exact Provision target, byte-exact resolved envelope, state, and creation time. The Graph wrapper admits exactly `[GraphShard(0)]`. This is the only fresh-install layout; no decoder or migration exists. |
| 46 | `ROUTER_PROVISIONING_BY_GRAPH` | `ROUTER_PROVISIONING_BY_GRAPH` | `init_provisioning_by_graph` | derived | provisioning graph index | **`(deployment_id, graph_name, request_id) → ProvisioningRequestKey`** (ADR 0035 Slice 1). Derived graph-scoped secondary index; commit-synced with the canonical request catalog |
| 47 | `ROUTER_PROVISIONING_INTENT_LOCK` | `ROUTER_PROVISIONING_INTENT_LOCK` | `init_provisioning_intent_locks` | canonical | provisioning intent lock | **`(deployment_id, resource_kind, logical_resource_key) → IntentLockOwner`** (ADR 0035 Slice 1). Canonical intent lock owner held while a request targeting this intent is non-terminal |
| 48 | `ROUTER_PROVISION_CONFIG` | `ROUTER_PROVISION_CONFIG` | `init_provision_config` | canonical | provisioning runtime config | **`() → ProvisionRuntimeConfig`** (ADR 0035 Slice 5). Durable Router provision-canister bootstrap binding; re-seeds the heap `PROVISION_CANISTER` threadlocal in `post_upgrade` |
| 49 | `ROUTER_BULK_LOAD_CHUNK_RECEIPTS` | `ROUTER_BULK_LOAD_CHUNK_RECEIPTS` | `init_bulk_load_chunk_receipts` | canonical | durable bulk-load receipts | **`(bulk_job_mutation_id, chunk_index) → BulkLoadChunkReceiptRecordV1`** ([ADR 0057](../adr/0057-router-operation-api-and-durable-bulk-load.md)). Owns the chunk **fingerprint** (`[u8; 32]`) as the resume idempotency key, plus the pinned Graph request and child mutation ID, exact canonical/public receipt, progress, and completion anchor. The chunk envelope itself is **not** persisted — its only role was read-time re-derivation of the digest, which the Graph-request fingerprint handshake already covers for replay correctness. Terminal parent retention is anchored in region 7; bounded receipt GC deletes at most 32 consecutive rows per step, persists its cursor in the parent, and removes the parent binding only after an empty-range proof. |
| 50 | `ROUTER_SCHEMA_MIGRATIONS` | `ROUTER_SCHEMA_MIGRATIONS` | `init_schema_migrations` | canonical | schema migrations | **`migration_id → SchemaMigrationRecord`** (ADR 0058/0059). Values use a Router-local Candid `Storable` wrapper around the shared versioned record. Parent links are the sole ordering source; root-to-head order and the current head are reconstructed on each bounded operation. The ledger is globally linear, capped at 4,096 records, and list pages at 16 records. A record owns one immutable envelope and a compact `PendingIndex { index_name_id, physical_index_id }`, `Applied`, or cleaned terminal `Failed` outcome; detailed index progress lives only in region 19. |
| 51 | `ROUTER_INDEX_CATALOG_EPOCH` | `ROUTER_INDEX_CATALOG_EPOCH` | `init_index_catalog_epoch` | canonical | index lifecycle fence | **`() → u64`** (ADR 0059). Monotonic Router-owned catalog epoch used by physical-index preparation, sealing, Graph export scope identity, graph-index build control, and Active publication. |
| 52 | `ROUTER_NEXT_VECTOR_INDEX_ID` | `ROUTER_NEXT_VECTOR_INDEX_ID` | `init_next_vector_index_id` | canonical | vector-index id allocator | **`() → u32`** (ADR 0065). Monotonic Router-owned next never-issued opaque vector-index id. Zero is invalid; allocation skips every id already present in the vector definition catalog, including caller-assigned ids, and never rewinds or reuses ids. |
| 53 | `ROUTER_VECTOR_INGEST_OUTBOX` | `ROUTER_VECTOR_INGEST_OUTBOX` | `init_vector_ingest_outbox` | canonical | direct vector ingestion | **`VectorIngestOutboxKey → VectorIngestOutboxValue`** — the sole fresh-install `StableBTreeMap` at MemoryId 53 uses a fixed 43-byte key ordered primarily by immutable `mutation_id`, then exact `vector_target`, `shard_id`, and `phase`; the value payload contains only Graph target/metadata, subject, and embedding bytes, with no key-owned identity duplication. One synchronous owner operation revalidates the live shard, exact Graph/Vector targets, immutable definition, capacity, and encoded size, then co-writes the final mutation counter and every canonical `AwaitingGraph` key/value before the first Graph await. Phase transitions preflight and move the exact old key to the exact new phase key in one no-await message. Key-only frontier derivation and target gates do not decode payloads; recovery first selects ordered key identities after its mutation cursor and decodes only the bounded selected batch. `AwaitingFrontier` has no legal payload mutation: a phase or lane change is a different key, and retirement rechecks exact immutable marker keys before removing them. Capacity is 1,024 rows; the persisted value uses `StorableBound::Unbounded`, while `VectorIngestOutboxState::encode_checked` enforces the 2 MiB transport/admission ceiling. Catalog-driven recovery now selects fully attached lanes even when MemoryId 53 has no marker, bounds the frontier by exact-lane unresolved direct intents or the allocation ceiling, and attempts one lane per tick with fair retry. The named PocketIC gate passes one test with seven filtered; the persisted Router artifact has all 41 terminal benchmark entries, with `markerless_frontier_catalog` at exactly 52,080,138 total and 52,079,143 scoped instructions. A focused non-persisted Vector benchmark run recorded 2,106,603,774 instructions. The dated Plan 0277 focused/persisted baseline records 2,110,300,092 total and 2,110,299,087 scoped instructions; the later live artifact currently records 2,084,327,695 total and 2,084,326,690 scoped instructions from unrelated work and is not Plan 0277 evidence. The targeted affected-crate format check passes; the workspace-wide format check is blocked by unrelated dirty paths and never passed. Fresh reinstall is required for this layout; no migration or legacy decoder exists. |

Router **54 regions** total (0–53).

Router uses `ic-stable-variable-memory-manager` with the same shared-capacity, append-only
extent model as Graph. The initial policy is intentionally asymmetric: the default is 2 pages;
single-cell control state uses 1 page; name catalogs and sparse maintenance state use 2 pages;
ordinary registry/telemetry/catalog rows use 4–8 pages; and bursty mutation, prepared-query,
reservation, pending-effect, and provisioning request rows use 16 pages. These values are a
capacity/slack hypothesis to validate with Router canbench and stable-memory measurements, not a
claim that every region should retain its current quantum. The pre-production repository maintains
one canonical current layout.
The Router stable-layout canbench implementation must touch all 54 regions, including the ADR 0057
receipt map, ADR 0058/0059 schema-migration ledger, ADR 0059 index-catalog epoch, and the direct
vector-ingestion outbox at MemoryId 53. Its persisted baseline was refreshed by the unfiltered
`canbench --persist` run; canbench 0.7 expanded the stable-reopen surface, so the resulting baseline
refresh is not a compact-key regression.
Capacity probes currently measure a 2-page property catalog at 1,024 rows and a 16-page prepared
plan catalog at 32 × 256 KiB. They are growth-slope guards; they do not establish the maximum
cardinality of an unbounded catalog or replace canister-level stable-memory monitoring.

### Router ephemeral

| Symbol                                                        | Location | Role | Reopen behavior |
| ------------------------------------------------------------- | -------- | ---- | --------------- |
| _(none beyond graph-index pending queues on other canisters)_ | —        | —    | —               |

---

## Graph-index canister — stable regions

| MemoryId | Symbol                          | Thread-local                   | Init fn                             | Class       | Owner domain                                                | Rebuild                                                                                                                                                                                |
| -------- | ------------------------------- | ------------------------------ | ----------------------------------- | ----------- | ----------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0        | `INDEX_ROUTER`                  | `INDEX_ROUTER`                 | `init_index_router`                 | canonical   | router authorization                                        | —                                                                                                                                                                                      |
| 1        | `INDEX_SHARD_CANISTER_BY_SHARD` | `INDEX_SHARD_CANISTER_CATALOG` | `init_index_shard_canister_catalog` | canonical   | shard canister catalog                                      | —                                                                                                                                                                                      |
| 2        | `INDEX_SHARD_BY_CANISTER`       | `INDEX_SHARD_CANISTER_CATALOG` | `init_index_shard_canister_catalog` | canonical   | shard canister catalog                                      | —                                                                                                                                                                                      |
| 3        | `INDEX_OWNERSHIP_CONFIG`        | `INDEX_OWNERSHIP_CONFIG`       | `init_index_ownership_config`       | canonical   | graph ownership + shard-detach lifecycle                    | Graph owner config (`graph_id`, `index_group_size`, `group_index`) plus optional checked-monotonic `next_detach_generation` and bounded (max 64) active `(shard_id, generation)` sessions. Missing legacy fields decode empty; reattach is excluded until posting EOF. No MemoryId change. |
| 4        | `INDEX_VERTEX_POSTINGS`         | `INDEX_VERTEX_POSTINGS`        | `init_index_vertex_postings`        | derived     | vertex property postings; variable-manager bucket 128 pages | **Implemented:** `backfill_vertex_property_postings` + router `advance_backfill`                                                                                                       |
| 5        | `INDEX_VERTEX_LABEL_POSTINGS`   | `INDEX_VERTEX_LABEL_POSTINGS`  | `init_index_vertex_label_postings`  | derived     | vertex label postings; variable-manager bucket 32 pages     | `backfill_label_postings`                                                                                                                                                              |
| 6        | `INDEX_EDGE_POSTINGS`           | `INDEX_EDGE_POSTINGS`          | `init_index_edge_postings`          | derived     | edge property postings; variable-manager bucket 128 pages   | **Implemented:** `backfill_edge_property_postings` (ADR 0009)                                                                                                                          |
| 7        | `INDEX_BUILD_STATES`            | `INDEX_BUILD_STATES`           | `init_index_build_states`           | maintenance | physical index build (ADR 0059)                             | **`PhysicalIndexId → IndexBuildState`** — immutable build scope plus bounded pull progress, last-page replay receipt, per-shard ack/watermark, and Building/Sealing/Aborting lifecycle |
| 8        | `INDEX_BUILD_TOUCHED_SUBJECTS`  | `INDEX_BUILD_TOUCHED_SUBJECTS` | `init_index_build_touched_subjects` | maintenance | physical index build (ADR 0059)                             | **`(PhysicalIndexId, canonical subject)`** fixed-width touched set for online base-seed exclusion (touched-first DML wins)                                                             |

---

## Vector canister — stable regions

New in ADR 0031 Slice 2; extended in Slice 6 and Slice 7; physical page store replaced by ADR 0032;
maintenance scan state added in Slice 10; per-shard watermark and tombstone-retention bookkeeping
added in ADR 0064 §5. **18 numbered slots (MemoryIds 0–17), 17 allocated; MemoryId 8 is an
unallocated hole, and MemoryId 11 holds the plan 0278 slab-compaction driver state (reused from
the retired `VECTOR_ID_TO_SUBJECT` reverse map so MemoryIds 9/10/12–14 stayed stable).** The vector canister is the sole durable owner of active Vector rows. Only physical
IVF projections (MemoryIds 5, 6, 9, 10, 12, and 18) rebuild from active Vector rows through the live
`admin_start_vector_rebuild` entry point. `VECTOR_SUBJECT_TO_ID` (7) and `VECTOR_ROW_SLAB` (13) name
the symbolic `client_reingestion` recovery contract, not a live API: loss of their durable rows requires
clients to re-ingest active vectors. Code source of truth for runtime `MemoryId` constants:
`crates/vector-canister/src/facade/stable/memory.rs`. The degenerate `ivf_flat` foundation runs with
`nlist = 1` / `partition_id = 0` and no centroids; MemoryId 6 (`IVF_CENTROIDS`) is allocated but empty
so Slice 4 can populate centroid bytes without a `MemoryId` repack.

Metadata ownership split: `VECTOR_INDEX_DEFS` (MemoryId 4) is authoritative for per-index config
(`kind`, `encoding`, `dims`, `metric`, `active_index_version`, `stride_bytes`, page-capacity
contract, and the durable `next_vector_id` allocator); `IVF_CENTROID_META` (MemoryId 5) holds only
centroid-specific derived state. The durable `next_page_id` allocator lives in each
`PartitionHead` (MemoryId 9).

ADR 0064 §7 retires the Slice 6 `VECTOR_ID_TO_SLOT` / `VECTOR_ID_TO_SUBJECT` reverse maps
(MemoryIds 8/11). MemoryId 8 stays an unallocated hole; plan 0278 reuses MemoryId 11 for
`VECTOR_SLAB_COMPACTION_STATE` without repacking MemoryIds 9/10/12–14. The page-store search path validates freshness
positionally against `VECTOR_SUBJECT_TO_ID` (MemoryId 7); rows carry neither `vector_id` nor
`generation`.

Slice 7 adds `VECTOR_REBUILD_STATE` (MemoryId 12): a `index_id → VectorRebuildStateRecord` holding
the per-index bounded shadow-version rebuild lifecycle (`Idle`/`Sampling`/`Training`/`Building`/
`ReadyToPublish`/`Cleaning`/`Aborting`/`Failed`), each long-running phase carrying a resume cursor so
admin steps stay bounded. The value is persisted as `RawRebuildState` — the verbatim
`VectorRebuildStateRecord` Candid bytes — so the persist and the step share a single Candid encode.
It is derived (reconstructible by re-running a rebuild from the active version) through the live
`admin_start_vector_rebuild` entry point. Slice 7 also extends the
`VECTOR_SUBJECT_TO_ID` value (`SubjectMapEntry`) with a second `shadow_slot: Option<SlotRef>`
(serde-default, no repack) so an atomic publish stays metadata-only; search resolves the live slot
via `current_slot_for(active_index_version)`.

The ADR 0033 implementation moves the bounded distinct candidate pool (Slice 8 k-means-lite
training) and the Training centroid work area out of the MemoryId 12 record into the dedicated raw
region `VECTOR_REBUILD_POOL` (MemoryId 18) described below, leaving that record scalars-only
(`pool_len`, cursors, counters). This reverses this inventory's earlier note that the pool "stays
inside the MemoryId 12 record": post-Slice-3 measurement showed the per-step whole-record Candid
decode plus whole-state re-encode at ~71% of full-rebuild instructions, which no in-record layout
or heap memoization addresses as directly. The change is part of the Phase-0 working-notes slice;
fresh install required, no migration or compatibility reader.

`VECTOR_REBUILD_POOL` (MemoryId 18, ADR 0033 implementation; layout **version 3** since Slice 6) is
a **single-tenant** raw region owned by
`crates/vector-canister/src/facade/stable/rebuild_pool.rs`:
`[header 96 B][candidate slots: capacity × (pad_stride + aux)][centroid area: work_centroids × dims×4][coarse-id array: capacity × 4 B, two-level only]`
under a minimal header carrying magic `VRP`, format version 3, the binding
`(index_id, pad_stride, work_centroids, centroid_stride)` plus the two-level and code-tier flags
(Slice 5 / Slice 6 respectively), and the lengths
(`pool_len`, `pool_capacity`, `centroid_count`, `assigned_len`). Every phase step re-validates the
header fail-closed against the definition geometry and the durable lifecycle scalars
(`RebuildPoolInvalid`) before any mutation; Sampling appends rows in bulk, dedup streams the durable
rows through a transient hash→indices map with exact byte verification (no whole-pool clone), and
Training reads rows individually. A two-level rebuild additionally persists each pool row's assigned
coarse subtree id in the coarse-id array (`assigned_len` must cover the whole pool before fine
training streams it), so a resumed `TrainFine` job reproduces its exact membership; flat rebuilds
keep the array unreserved and their capacities byte-identical to the pre-Slice-5 layout. Admission
is bounded by the physical region budget (`8 MiB`) plus the unchanged per-iteration op budget — the
retired Candid-envelope constraint (`MAX_REBUILD_STATE_BYTES`) is deleted. Because the region holds
one rebuild's pool, a second index cannot start a rebuild while another binds it
(`RebuildAlreadyActive`). The binding is released (header zeroed) when the lifecycle leaves
`Sampling`/`Training` for good: abort entry, teardown completion to `Idle`, the `Failed`
transition, and the coordinated definition-domain reset. Like the lifecycle record it is derived,
reconstructed by re-running a rebuild from the active version.

ADR 0064 §7 replaces the ADR 0032 structure-of-arrays page store with the **two-table** page format from
`ic-stable-vector-page-store`: `[PageHeader][run_table × run_capacity][row_meta × capacity][vector_bytes ×
capacity]`. MemoryId 10 remains `VECTOR_PAGE_META` (a `(index_id, version, partition_id, page_id) →
VectorPageMeta` directory of `{ slab_offset, row_count, live_count }`; page geometry is owned by
`VECTOR_INDEX_DEFS` and tombstoned rows are derived as `row_count − live_count`, cross-checked against
the on-slab header at reopen), and MemoryId 13 `VECTOR_ROW_SLAB` holds the raw pages
behind a `VSL`/version-1 slab header. Rows store a packed 30-bit `VertexPayload` (vertex id + bit-31
tombstone); the shard is shared across contiguous rows via the run table, and the page rolls a new page
when its run table is full. `vector_bytes` rows are `pad_stride_bytes` wide (16-byte aligned), with the
trailing pad zeroed so scoring kernels never observe non-finite garbage. Rows are write-once at tail
positions: superseded rows are tombstoned, and freshness is validated positionally (subject-map slot
matches the scanned position) rather than by a row-carried `vector_id`/`generation` (removed). The two
regions form one composite store (`PAGE_STORE`) that opens together and fails closed on a partial layout
(see [ADR 0032](../adr/0032-vector-index-slab-page-store.md); the row format is superseded by
[ADR 0064](../adr/0064-vector-canister-redesign-ownership-layout-fencing-ingest.md)). This is a **fresh
layout cutover** (format version 1, breaking) with no deployed state, no migration, and no compatibility
reader; `VECTOR_SUBJECT_TO_ID` stays the freshness source of truth, and the `VECTOR_ID_TO_SLOT` /
`VECTOR_ID_TO_SUBJECT` reverse maps are **retired** (MemoryId 8 unallocated; MemoryId 11 reused by
plan 0278 for the slab-compaction driver state; no production reader
after ADR 0064 §7 positional validation). `VECTOR_PARTITION_HEADS`
(MemoryId 9) remains the per-partition allocator/counter owner and is deliberately outside the composite
store. Since Slice 5, heads, page metas, and pages exist **for leaf partitions only**: one head per
leaf id `0..leaf_count` (`nlist × nlist_fine`; flat keeps `nlist_fine = 1`). A two-level generation
stores its extra level inside `IVF_CENTROIDS` (MemoryId 6) under level-tagged keys — the 17-byte
`PartitionKey` gained a coarse/leaf tag byte (schema `GLEAPH-PARTKEY-1`) — so no new region and no
MemoryId repack are needed; teardown deletes both levels' keys with one `(index_id, version)`
prefix range deletion. The definition record itself grew to 46 bytes (schema `GLEAPH-VECDEF-02`,
adding `levels`/`nlist_fine`); fresh install required for both schema changes, with no migration or
compatibility reader.

A derived, router-guarded admin query (`get_vector_slab_stats`) reports slab-space observability
over these two regions: whole-slab physical facts (size, `occupied_tail`, global referenced bytes,
conservative dead-space estimate) plus optional per-`index_id`/per-version logical counters. Because
`VECTOR_ROW_SLAB` is a single global allocation domain, the physical facts are always global while
`index_id` scopes only the logical counters. It reads only `VECTOR_PAGE_META` + the slab header
(never row bytes or `VECTOR_SUBJECT_TO_ID`). `get_vector_slab_stats` is the **unbounded** full
page-meta scan kept as a convenience query; `scan_slab_stats` is the IC-safe bounded
companion that scans at most `max_pages` page-meta entries per call and returns an opaque `PageKey`
cursor. Its steps are additive partials merged client-side: each step sums global referenced
bytes across the whole map (even under an `index_id` filter) but carries only snapshot
size/tail facts — no step can know the global dead-space estimate, so it is absent from the step
shape and recomputed once after merging. A malformed step cursor returns an error rather than
trapping. The stepped path is a bounded best-effort scan with **no snapshot isolation** (the cursor
is only a `PageKey`, so concurrent `VECTOR_PAGE_META` writes between calls can be missed or
double-counted); run it during a quiescent window or use the single-call query for an exact figure.
Both queries are diagnostic only and never affect search/freshness. Dead space itself is reclaimed
by the opt-in Router-driven slab compaction (plan 0278) described below; automatic policy triggers
remain deferred.

Plan 0278 adds `VECTOR_SLAB_COMPACTION_STATE` (MemoryId 11, reusing the retired
`VECTOR_ID_TO_SUBJECT` slot): one global `VectorSlabCompactionState` record (`Idle` |
`Compacting { write_cursor, range_end, scan_cursor, pages_moved }`) owned by the
Router-guarded driver endpoints `admin_start_vector_slab_compact` /
`admin_vector_slab_compact_step` / `admin_vector_slab_compact_status`. The start freezes the
snapshot source range `[SLAB_HEADER_SIZE, occupied_tail)`; each bounded step copies live pages
whose spans sit inside the range down into the dense prefix — bytes persisted strictly before
each page's single `VectorPageMeta.slab_offset` swap — and the final step fails closed if any
live span sits inside the reclaimed gap, then rewinds `occupied_tail` once to
`max(write_cursor, highest live span end)` (post-start appends keep the tail above the gap; a
quiescent store reclaims down to the write cursor). Pages appended after start land above
`range_end` and are never touched; metas dropped mid-compaction by GC/cleanup are skipped by the
lap cursor. All cursors are reopen-visible in this one record, so an interrupted or upgraded
driver resumes fail-closed. Operational bookkeeping, not query truth: it carries no rebuild path
and is cleared with the fixture-only definition-domain reset.

| MemoryId | Symbol                                 | Thread-local               | Init fn                       | Class       | Owner domain                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | Rebuild                                                                                    |
| -------- | -------------------------------------- | -------------------------- | ----------------------------- | ----------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| 0        | `VECTOR_INDEX_ROUTER`                  | `VECTOR_INDEX_ROUTER`      | `init_router`                 | canonical   | router authorization                                                                                                                                                                                                                                                                                                                                                                                                                                                                                | —                                                                                          |
| 1        | `VECTOR_INDEX_SHARD_CANISTER_BY_SHARD` | `SHARD_CANISTER_CATALOG`   | `init_shard_canister_catalog` | canonical   | shard canister catalog                                                                                                                                                                                                                                                                                                                                                                                                                                                                              | —                                                                                          |
| 2        | `VECTOR_INDEX_SHARD_BY_CANISTER`       | `SHARD_CANISTER_CATALOG`   | `init_shard_canister_catalog` | canonical   | shard canister catalog                                                                                                                                                                                                                                                                                                                                                                                                                                                                              | —                                                                                          |
| 3        | `VECTOR_INDEX_OWNERSHIP_CONFIG`        | `OWNERSHIP_CONFIG`         | `init_ownership_config`       | canonical   | graph ownership + shard-detach lifecycle                                                                                                                                                                                                                                                                                                                                                                                                                                                            | Graph owner config (`graph_id`) plus optional checked-monotonic `next_detach_generation` and bounded (max 64) active `(shard_id, generation)` sessions. Missing legacy fields decode empty; reattach is excluded until subject-scan EOF. No MemoryId change. |
| 4        | `VECTOR_INDEX_DEFS`                    | `DEFINITION_STORE`         | `create_for_install` / `open_after_upgrade` | canonical   | index definitions + allocators (SSOT for config). The Vector-facade owner starts uninitialized, strict-creates LHM bytes with the trusted install seed, and exact-opens seed-free after upgrade; incompatible bytes stay unavailable and are never treated as empty. **Pre-release destructive cutover:** previous CHM bytes require wipe/reinstall; no reader or migration exists. | — |
| 5        | `IVF_CENTROID_META`                    | `IVF_CENTROID_META`        | `init_centroid_meta`          | derived     | centroid-specific state                                                                                                                                                                                                                                                                                                                                                                                                                                                                             | `admin_start_vector_rebuild`                                                               |
| 6        | `IVF_CENTROIDS`                        | `IVF_CENTROIDS`            | `init_centroids`              | derived     | centroid vectors (**reserved-empty in Slice 2**)                                                                                                                                                                                                                                                                                                                                                                                                                                                    | `admin_start_vector_rebuild`                                                               |
| 7        | `VECTOR_SUBJECT_TO_ID`                 | `SUBJECT_STORE`            | `create_for_install` / `open_after_upgrade` | canonical   | `SubjectStore` is the sole private owner of `StableLinearHashMap<SubjectKey, FixedSubjectMapEntry>`: the canonical subject freshness, tombstone, and slot authority. It starts uninitialized, strict-creates with the trusted `subject_map_seed`, and seed-free exact-opens after upgrade. Existing CHM or unknown bytes require a pre-release wipe/reinstall; no reader, migration, or create-on-open-error fallback exists. All point access and physical scans route through this owner, so unavailable is never interpreted as empty. | `client_reingestion` (symbolic recovery contract, not an API)                             |
| 8        | `VECTOR_ID_TO_SLOT`                    | —                          | —                             | unallocated | **retired** reverse map (no production reader after ADR 0064 §7); explicit hole so MemoryIds 9/10/12/13/14 stay stable                                                                                                                                                                                                                                                                                                                                                                              | —                                                                                          |
| 9        | `VECTOR_PARTITION_HEADS`               | `VECTOR_PARTITION_HEADS`   | `init_partition_heads`        | derived     | partition page chains + `next_page_id` allocator. `StableLinearHashMap<PartitionKey, PartitionHead>` (LHM, same cutover as defs/subjects); previous CHM bytes require a pre-release wipe/reinstall. Cleared (not incarnation-reset) on coordinated reset because it is derived state. | `admin_start_vector_rebuild`                                                               |
| 10       | `VECTOR_PAGE_META`                     | `PAGE_STORE`               | `init_page_store`             | derived     | page-directory metadata (slab offset + capacity/row/live/tombstone counts + meta/run geometry) for the two-table slab page store (**ADR 0064 §7**)                                                                                                                                                                                                                                                                                                                                                  | `admin_start_vector_rebuild`                                                               |
| 11       | `VECTOR_SLAB_COMPACTION_STATE`         | `VECTOR_SLAB_COMPACTION_STATE` | `init_slab_compaction_state` | maintenance | one global `VectorSlabCompactionState` (`Idle` \| `Compacting { write_cursor, range_end, scan_cursor, pages_moved }`) for the opt-in bounded slab dead-space compaction driver (**plan 0278**); reuses the retired `VECTOR_ID_TO_SUBJECT` reverse-map slot (no production reader after ADR 0064 §7) so MemoryIds 9/10/12–14 stay stable. Operational bookkeeping, not query truth; persists across upgrade so an interrupted driver resumes fail-closed | —                                                                                          |
| 12       | `VECTOR_REBUILD_STATE`                 | `VECTOR_REBUILD_STATE`     | `init_rebuild_state`          | derived     | bounded shadow-version rebuild lifecycle (**Slice 7**)                                                                                                                                                                                                                                                                                                                                                                                                                                              | `admin_start_vector_rebuild`                                                               |
| 13       | `VECTOR_ROW_SLAB`                      | `PAGE_STORE`               | `init_page_store`             | derived     | raw two-table vector row slab (run table + packed `VertexPayload` + pad-stride rows, plus a per-generation trailing code-segment table when the generation's `code_tier` is on: `PageHeader.code_stride` at offset `24..28`, 28→32 B header; **ADR 0079** / Slice 6) with `VSL`/version-1 header (**ADR 0064 §7**); companion to `VECTOR_PAGE_META`, opened as one composite store; total durable-row loss requires client re-ingestion                                                                                                                                                                                                                                             | `client_reingestion` (symbolic recovery contract, not an API)                             |
| 14       | `VECTOR_MAINTENANCE_STATE`             | `VECTOR_MAINTENANCE_STATE` | `init_maintenance_state`      | maintenance | `index_id → VectorMaintenanceState` page-health scan progress cursor + merged counters (`Failed` carries a bounded message) for Router-forwarded maintenance orchestration (**Slice 10**); operational bookkeeping discarded/restarted, not reconstructed; persists across upgrade, cleared only on init/reset                                                                                                                                                                                      | —                                                                                          |
| 15       | `VECTOR_SHARD_WATERMARKS`              | `VECTOR_SHARD_WATERMARKS`  | `init_shard_watermarks`       | maintenance | `shard_id → ShardWatermarks { graph_watermark, router_watermark }` (**ADR 0064 §5**): the attached Graph shard advances `graph_watermark` for a contiguous applied prefix; the Router-only frontier endpoint advances `router_watermark` monotonically for an exact fully attached shard after lane validation, including a markerless Graph-only lane. The same no-await update runs one bounded GC step, whose cutoff is always `min(graph_watermark, router_watermark)`. This is operational bookkeeping, not a query-facing index or canonical fact, so it carries no rebuild path | —                                                                                          |
| 16       | `VECTOR_GC_CURSOR`                     | `VECTOR_GC_CURSOR`         | `init_gc_cursor`              | maintenance | `Option<DeletedSubjectKey>` (**ADR 0064 §5**): persisted cursor for bounded tombstone-retention steps. The catalog-driven frontier may advance the cutoff for any fully attached lane; an empty marker snapshot retires no Router rows, but the monotonic Vector watermark still permits the bounded GC step. This is operational bookkeeping, not a query-facing index or canonical fact, so it carries no rebuild path                                                                                                                                                                                                            | —                                                                                          |
| 17       | `VECTOR_DELETED_SUBJECTS`              | `VECTOR_DELETED_SUBJECTS`  | `init_deleted_subjects`       | maintenance | `(shard, tombstone stamp, subject) -> ()` maintenance list for bounded tombstone-retention steps; subject-map slot order is unstable under removal (**ADR 0064 §5**). Eligibility is `stamp <= min(graph_watermark, router_watermark)` for every fully attached lane, including markerless Graph-only lanes. Operational bookkeeping, not a query-facing index or canonical fact, so it carries no rebuild path                                                                                                                                                                         | —                                                                                          |
| 18       | `VECTOR_REBUILD_POOL`                  | — (raw region)             | — (`rebuild_pool::begin`)     | derived     | single-tenant raw rebuild-pool region (**ADR 0033 implementation**): frozen Sampling/Training candidate rows (pad-stride stored bytes + aux), the Training centroid work area, and — two-level rebuilds only (Slice 5) — the per-row coarse-id array behind a minimal `VRP`/**version-3** header (binding + `pool_len`/`pool_capacity`/`centroid_count`/`assigned_len` + Slice 6 `code_tier` flag byte carrying the shadow generation's tier from start to `Building`, **ADR 0079**); fail-closed resume validation against the lifecycle scalars; released at abort entry, teardown completion, `Failed`, and coordinated reset. Fresh layout cutover (format version 3, breaking over earlier headers): no migration or compatibility reader | `admin_start_vector_rebuild`                                                               |

The fixture-only Vector definition-domain reset, compiled under `cfg(test)` /
`cfg(feature = "canbench")`, resets MemoryIds 4, 5, 6, 7, 9, 10, 11, 12, 13, 14, 17, and 18 together after
preflighting the expected MemoryId 4 incarnation and acquiring every coupled region handle. It also
drops the heap centroid cache. MemoryIds 0–3 and 15–16 are not definition-derived: router authority,
graph ownership, shard attachments, watermarks, and the tombstone-retention cursor therefore survive. Production
reset ownership and rollback proof remain pending; no public reset endpoint is exposed.

PocketIC lifecycle evidence (2026-08-21 UTC):
`unavailable_vector_owner_keeps_graph_delete_outbox_until_upgrade_rebind` passed (1 passed, 0
failed), proving that an outer `IndexDefinitionStoreUnavailable` leaves the Graph delete outbox
row pending until Vector exact-open/rebind and maintenance drain. The targeted direct-ingestion
lifecycle gate `unavailable_vector_owner_rebinds_graph_and_router_direct_ingestion_outboxes` also
passed (1 passed, 0 failed, 4 filtered) with:
```sh
cargo test -p gleaph-pocket-ic-tests --test adr0031_vertex_embedding_ingestion \
  unavailable_vector_owner_rebinds_graph_and_router_direct_ingestion_outboxes -- --nocapture
```
It manually drives the Graph and Router recovery seams and covers Router upgrade, Vector
reopen/rebind, exact persisted target/operation replay, exact GQL search, and idempotent replay.
It does not prove autonomous wall-clock recovery-timer firing or deferred watermark/tombstone GC
completion; those remain unproven.
The Router-vector markerless frontier implementation is present in the existing MemoryId 53 and
MemoryId 15 owners. The named Graph-only PocketIC lifecycle gate
`graph_only_markerless_lane_advances_frontier_on_autonomous_timer` passes exactly one test with
seven filtered, one `PocketIc`, one federation bootstrap, and four canister installs; it leaves
MemoryId 53 empty, advances through the ordinary timer, and exercises bounded GC. The persisted
Router artifact has all 41 terminal benchmark entries; `markerless_frontier_catalog` records exactly
52,080,138 total and 52,079,143 scoped instructions. The focused non-persisted Vector
`bench_router_frontier_gc_budget` run recorded 2,106,603,774 instructions. The dated Plan 0277
focused/persisted baseline records 2,110,300,092 total and 2,110,299,087 scoped instructions; the
later live artifact currently records 2,084,327,695 total and 2,084,326,690 scoped instructions
from unrelated work and is not Plan 0277 evidence. The bounded Quint model has fifteen deterministic
tests and 5,000 sampled traces with all requested witnesses; `quint verify` remains unrun and
non-required, so this remains bounded policy evidence rather than production proof. The targeted
affected-crate format check passes; the workspace-wide format check is blocked by unrelated dirty
paths and never passed.
Fully attached Graph-only lanes are eligible for best-effort progress, but finite-time liveness and
a global subject-map growth bound remain unproved.
The final V1 inline-overflow/split-debt map does not expose a bounded PocketIC pressure injector;
fixed-count sequential-key fixtures are intentionally not treated as pressure evidence. Map-level
overflow, split-debt, and pressure behavior is covered by the owning LHM unit suite. The
coordinated reset remains fixture-only and production reset ownership/rollback proof remain
pending.

---

## Provision canister — stable regions

New in ADR 0035 Slice 2. **11 regions (MemoryId 0–10).** Code source of truth for runtime `MemoryId`
constants: `crates/provision/src/stable/memory.rs`. Typed registry and rebuild path:
`PROVISION_STABLE_LAYOUT` in `crates/graph-kernel/src/stable_layout.rs`. Region 2 is a derived
secondary index and is commit-synced with Region 1 (`PROVISION_JOB_BY_REQUEST`).

| MemoryId | Symbol                          | Thread-local                 | Init fn                                       | Class       | Owner domain                                                                                                                                | Rebuild                                       |
| -------- | ------------------------------- | ---------------------------- | --------------------------------------------- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------- |
| 0        | `PROVISION_DEPLOYMENT_TRUST`    | `DEPLOYMENT_TRUST`           | `init_deployment_trust`                       | canonical   | deployment trust binding (`deployment_id → DeploymentBinding`)                                                                              | —                                             |
| 1        | `PROVISION_JOB_BY_REQUEST`      | `JOB_BY_REQUEST`             | `init_job_by_request`                         | canonical   | durable job/receipt state (`(request_id, deployment_id) → ProvisionJobRecord`) with immutable exact-request digest and versionless completion state; fresh-install layout only, with no decoder or migration | —                                             |
| 2        | `PROVISION_JOB_BY_DEPLOYMENT`   | `JOB_BY_DEPLOYMENT`          | `init_job_by_deployment`                      | derived     | intent → owning job secondary index (`ProvisioningIntentKey → ProvisionJobRequestKey`); the Map 2 value is the exact request owner preflighted before fresh completion and co-removed with Map 3 | commit-synced with `PROVISION_JOB_BY_REQUEST` |
| 3        | `PROVISION_JOB_INTENT_LOCK`     | `JOB_INTENT_LOCK`            | `init_job_intent_lock`                        | canonical   | presence-only intent lock marker (`ProvisioningIntentKey → ProvisionIntentLockMarker`); ownership comes from Map 2, so fresh completion requires both the exact Map 2 owner and this marker before co-removal, while Completed replay ignores later rows | —                                             |
| 4        | `PROVISION_BOOTSTRAP_AUTH`      | `BOOTSTRAP_AUTH`             | `ProvisionBootstrapAuthStore::init_authority` | canonical   | durable bootstrap authority singleton (`StableCell<Option<BootstrapAuthorityRecord>>`) (ADR 0035 Slice 7)                                   | —                                             |
| 5        | `PROVISION_BOOTSTRAP_AUDIT_LOG` | `BOOTSTRAP_AUDIT_LOG`        | `ProvisionBootstrapAuthStore::put_record`     | telemetry   | append-oriented per-governance audit log of admin-install decisions (`(Principal, u64) → BootstrapAuthEntry`) with a per-principal cap (ADR 0035 Slice 7) | —                                             |
| 6        | `PROVISION_ARTIFACT_CATALOG`    | `PROVISION_ARTIFACT_CATALOG` | `init_artifact_catalog`                       | canonical   | immutable artifact catalog (`ArtifactId → ArtifactMetadata`) with a durable `verified` flag set once on the final upload chunk (ADR 0036 Slice 8a) | —                                             |
| 7        | `PROVISION_ARTIFACT_UPLOAD`     | `PROVISION_ARTIFACT_UPLOAD`  | `init_artifact_upload`                        | maintenance | mutable upload/verification scratch state (`ArtifactId → ArtifactUpload`), reclaimed on verify success (ADR 0036 Slice 8a)                  | —                                             |
| 8        | `PROVISION_ARTIFACT_CHUNKS`     | `PROVISION_ARTIFACT_CHUNKS`  | `init_artifact_chunks`                        | canonical   | verified canonical artifact chunk bytes (`(storage_id: u64, chunk_index: u32) → ArtifactChunk`), keyed on the fixed-length internal storage id (ADR 0036 Slice 8a)     | —                                             |
| 9        | `PROVISION_RELEASE_MANIFEST`    | `RELEASE_MANIFEST_MAP`       | `init_release_manifest`                       | canonical   | immutable release manifest (`ReleaseId → ReleaseManifest`) (ADR 0036 Slice 8b)                                                              | —                                             |
| 10       | `PROVISION_ACTIVE_RELEASE`      | `ACTIVE_RELEASE_CELL`        | `init_active_release`                         | canonical   | atomic active-release pointer (`StableCell<Option<ReleaseId>>`) (ADR 0036 Slice 8b)                                                         | —                                             |
| 11       | `PROVISION_ARTIFACT_AUDIT_LOG`  | `ARTIFACT_AUDIT_LOG`         | `init_artifact_audit_log`                     | telemetry   | append-oriented artifact/release audit log (`(Principal, u64) -> ArtifactAuditEntry`) (ADR 0036 Slice 8c)                                   | —                                             |
| 12       | `PROVISION_ARTIFACT_STORAGE_ID` | `ARTIFACT_STORAGE_ID`        | `init_artifact_storage_id`                   | maintenance | internal artifact storage-id counter (`StableCell<u64>`), the chunk-store key prefix allocator (ADR 0036 Slice 8a)                            | —                                             |

## Related documents

- [Refactoring roadmap](../architecture/refactoring-roadmap.md) — phased plan; Phase 0 exit criteria
- [LARA and graph facade](./lara-and-facade.md) — layering; defers byte layout to this inventory
- [ADR 0048](../adr/0048-lara-counterpart-resolution.md) — accepted LARA-owned counterpart tables design; canonical leaf enumeration, mutation invalidation/rebuild scheduling, runtime CounterpartScan, and Graph alias removal are implemented
- [Property index](../index/property-index.md) — posting model and router seed routing
- [Label index](../index/label-index.md) — label postings and backfill orchestration
- [ADR 0004: Label index](../adr/0004-label-index.md)
