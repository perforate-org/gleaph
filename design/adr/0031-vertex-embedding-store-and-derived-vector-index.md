# 0031. Vertex embedding store and derived vector index canister

Date: 2026-06-23
Status: accepted (partially implemented)
Last revised: 2026-07-04 01:39:31 UTC +0000

> **Status note:** The boundary decision is accepted. Slice 1 (the canonical graph-owned
> vertex embedding store) and Slice 2 (the derived sync path plus a degenerate `ivf_flat`
> `graph-vector-index` canister foundation — mutation-only) are implemented. Slice 2 covers the
> `graph-kernel` sync/mutation wire types, the `VECTOR_INDEX_STABLE_LAYOUT` registry (11 regions,
> MemoryId 0–10, `IVF_CENTROIDS` reserved-empty), the canister storage (`vector_upsert` /
> `vector_remove`, durable allocators, subject tombstone clock, attach/detach), and the graph-side
> delta plumbing (catalog-gated dispatch, `vector_pending`, `RepairPostingOp::VectorEmbedding`,
> repair drain, `vertex_embedding_backfill`). The convergence model is canonical-wins, now made
> production-correct by the Slice 4 incarnation fence (below): an upsert delivered to a strictly
> newer incarnation resurrects the subject with a fresh `VectorId`, and the graph repair-drain
> reconciles journaled vector ops against the canonical store rather than replaying them.
>
> **Slice 3 (implemented): Router catalog, target resolution, and the fail-closed activation gate.**
> Slice 3 adds the Router-owned vector-index definition catalog (`ROUTER_VECTOR_INDEXES`, MemoryId
> 42) and the graph-scoped embedding-name catalog (`ROUTER_EMBEDDING_NAME_CATALOG`, MemoryIds 40–41)
> that the Router solely allocates, plus the admin/query surface (register by embedding **name**, set
> target, list, activation status/explain-blocked, inspect-only single-target resolution) and the
> ephemeral `ExecutePlanArgs.indexed_embeddings` injection path. Slice 3 deliberately did **not**
> activate production dispatch; that gate is now opened by Slice 4.
>
> **Slice 4 (implemented): incarnation-fenced production activation.** Slice 4 makes derived vector
> sync production-correct and activatable:
> - **The incarnation fence.** A graph-owned, delete-spanning, monotonic `embedding_incarnation`
>   (canonical `VERTEX_EMBEDDING_INCARNATIONS`, graph MemoryId 45) strictly increases across each
>   delete/reinsert of a `(VertexId, EmbeddingNameId)` identity and is **never deleted** (it persists
>   across removes as a high-water mark). Sync ops carry `embedding_incarnation`, and the vector
>   canister orders writes by `(embedding_incarnation, embedding_version)`. This closes **both** the
>   forward-orphan (a stale-version remove no-ops against a newer live slot) and the reverse-orphan (a
>   late blind remove racing a reinsert tombstones a live vector): a remove or upsert from an older
>   incarnation can no longer affect a newer one. The repair drain re-derives the current incarnation
>   for canonical-present subjects and stamps the persisted incarnation (with
>   `RECONCILE_TOMBSTONE_VERSION`) for canonical-absent removes.
> - **The activation gate.** A Router-owned **stable activation flag** (default off,
>   `ROUTER_VECTOR_DISPATCH_ACTIVATION`, MemoryId 43; admin set/get) replaces the old `const false`
>   fencing predicate. The flag is necessary but **not sufficient**: per-graph emission is also fenced
>   by `graph_vector_dispatch_ready(graph_id)`, which requires every live shard of the graph to carry
>   a non-anonymous `vector_index_canister` equal to the graph's single target with a durable
>   `vector_index_attached == true` registry bit. `to_indexed_embedding_catalog` emits
>   `DispatchEnabled` specs only when ready; otherwise it stays empty (fail-closed). `activation_state`
>   is **derived** at read time, so the flag/attach activates existing targeted defs with no stored
>   state migration. Blocked status distinguishes `DispatchNotActivated` (flag off) from
>   `ShardsNotVectorAttached`.
> - **Router invariants.** Registration enforces **one vector index per embedding name per graph**
>   (`Conflict` on a second def for the same `embedding_name_id`, checked before interning the name)
>   and **one vector-index target per graph** (every def and every attached shard must share one
>   target principal). Both exist because graph dispatch/backfill/repair use one single-route target;
>   the Graph-to-vector mutation boundary may batch multiple ordered operations within that route.
> - **Target wiring.** A router-guarded graph endpoint `admin_set_vector_index_canister` writes the
>   target into the shard's **local** `FederationRouting` (idempotent, upgrade-durable), and a Router
>   endpoint `admin_attach_vector_index_shard` drives the attach handshake: it writes graph-local
>   routing first, attaches the shard to the vector canister, and flips the registry
>   `vector_index_attached` bit only after both succeed — so the registry bit cannot claim readiness
>   while the shard is locally `None`. A retrofit path attaches already-registered shards without a
>   reinstall. `ShardRegistryEntry` gained `vector_index_canister`/`vector_index_attached` via a `V2`
>   stable envelope (old `V1` bytes still decode). The vector canister fixes ownership on `graph_id`
>   alone and accepts every shard of that graph (different `graph_id` → `GraphOwnershipMismatch`); it
>   carries no property-index group descriptor (`index_group_size` / `group_index`), since one target
>   per graph must own all shards rather than a single contiguous shard group.
> - **Bounded backfill.** `admin_vector_index_backfill_step` is a real bounded driver
>   (router orchestration → `graph_client::backfill_vertex_embeddings` → graph endpoint → existing
>   worker) taking an explicit `(shard_id, start_vertex_id, max_vertices)` resume cursor; it fails
>   closed while dispatch is not ready.
>
> **Plan 0048 (implemented): canonical vertex-embedding ingestion boundary.** Adds the missing
> application-to-Gleaph write path for externally generated vertex embeddings. The Router admin
> endpoint `admin_ingest_vertex_embedding` accepts a logical graph name, an opaque encoded vertex id,
> a registered embedding name, and a finite `Vec<f32>`. Router decodes the vertex id with the graph
> encoding key, validates the live shard and registered vector definition (dims, metric, encoding),
> and dispatches a single canonical write to the owning Graph shard. The Graph endpoint
> `admin_ingest_vertex_embedding` (Router-only caller) verifies vertex existence, installs the
> supplied ephemeral indexed-embedding catalog, commits canonical bytes via the existing
> `set_vertex_embedding` path, and attempts the derived vector-index projection. The result reports
> the canonical `embedding_version` and an explicit `projection_outcome` of `Applied` or
> `DeferredForRepair`, so callers do not retry a canonical write that has already committed when the
> derived index is temporarily unreachable. Direct vector-canister seeding remains a test/index-only
> path; product ingestion must flow through Router. Social-demo semantic retrieval remains a later
> planned slice.
>
> **Slice 5 (implemented): exact `ivf_flat` search MVP (read path).** Slice 5 lands the first
> production vector-search read path over the already-synced derived index, with **no stable-layout
> change**. The vector canister exposes a router-guarded query `vector_search(VectorSearchRequest) ->
> VectorSearchResult` that **scans the live subject map** (`VECTOR_SUBJECT_TO_ID`) over the requested
> `index_id` — the source of truth for which subjects are live and at which slot — reading each live
> slot's bytes through a per-query page cache, scoring with `L2Squared`, and returning a bounded top-k
> ordered by `(distance asc, subject asc)`. Because the subject row carries `(embedding_incarnation,
> embedding_version)` and the live `slot`, tombstones and superseded generations are never scored and
> freshness is exact; this avoids any `PageRow`/reverse-map change or new stable region (deferred to
> Slice 6 partition scans). The Router exposes `vector_search` as a `#[query(composite = true)]` that
> resolves the graph/index to its single activated target and **fails closed** on the same Slice 4
> gate (`activation_block_reason`) before forwarding; the vector-canister query is router-guarded so
> the derived vectors cannot be queried directly around the gate. The kernel adds `VectorSearchRequest`
> / `VectorSearchHit` / `VectorSearchResult` and `MAX_VECTOR_SEARCH_TOP_K` (1024). The search is
> intentionally degenerate IVF (one partition, exact scoring); a `crates/graph-vector-index` canbench
> exact-scan suite (dims 128/384/768 × top_k 10/100) establishes the Slice 6 baseline.
>
> `nprobe` partition pruning landed in Slice 6 (below). Production IVF centroid training + shadow
> rebuild + dual-write landed in Slice 7 (below). Bounded `ivf_flat` training quality landed in Slice
> 8 (below): it replaced the Slice 7 "first distinct samples become centroids" MVP with bounded,
> deterministic k-means-lite over a bounded candidate pool and added a head-only partition-health
> summary without changing the public search API. Slice 9 (below) then added bounded page-meta
> tombstone-ratio health, a policy-driven rebuild recommendation/trigger, and a heap centroid cache,
> again without changing the public search API. Slice 10 (below) then added Router-forwarded
> maintenance and a Router-owned, vector-executed maintenance policy/step boundary, again without
> changing the public search API. Candidate pagination, query ranking/merge, per-index/per-embedding
> fan-out, GQL vector-search syntax, full/balanced k-means, autonomous tombstone cleanup, PQ/HNSW, and
> `VectorSubject::Edge` remain Slice 11+.
> This ADR fixes ownership, consistency, the standard `ivf_flat` vector-index kind, and the first
> derived vector-index stable-memory shape before the GQL query operator is committed.
>
> **Slice 6 (implemented): `ivf_flat` partition-page read path.** Slice 6 lands the first real
> `ivf_flat` read path beyond the exact scan. It adds **one stable region**, `VECTOR_ID_TO_SUBJECT`
> (MemoryId 11), a derived `(index_id, vector_id) → VectorSubject` reverse **locator** maintained in
> lockstep with `VECTOR_ID_TO_SLOT` on insert/resurrect/remove and rebuilt by the same
> `vertex_embedding_backfill` path (`VECTOR_INDEX_STABLE_LAYOUT` is now **12** regions, 0–11). A new
> `partition_page_scan` scores the query against the index's centroids (`IVF_CENTROIDS`), selects the
> `nprobe` nearest partitions, and range-scans only those partitions' `VECTOR_PAGE` chains; each
> scanned row is reverse-mapped to its subject and re-validated against `VECTOR_SUBJECT_TO_ID` (not
> deleted, matching `vector_id`, and `slot` pointing at exactly this page/slot/generation) before
> scoring, so `VECTOR_SUBJECT_TO_ID` remains the single freshness source of truth and the reverse map
> is only a locator. `vector_search` selects the path: exact subject-map scan when `nlist <= 1` or the
> centroids are not ready/current, partition-page scan otherwise; a stale/incomplete centroid set
> falls back to the exact scan with **no error**. `nprobe` is the only recall knob and the selected
> partitions are scanned in full, so the result is the exact top-k over those partitions with **no
> mid-scan budget** that could silently truncate it; the in-canister default is `nprobe = min(4,
> nlist)` (clamped to `1..=nlist`), and the public Router/kernel request stays algorithm-neutral
> (no `nprobe` on the wire — an internal `vector_search_tuned` varies it for tests/benchmarks only).
>
> Production cannot yet create `nlist > 1` indexes, so Slice 6 partitioned/centroid layouts are
> produced by **test/bench-only seed helpers** (`seed_ivf_for_test`); the production mutation path
> still appends to `partition_id = 0` (correct only while `nlist == 1`, the only `nlist` any
> production def has). **Seeded multi-partition indexes are therefore immutable after seeding in Slice
> 6** — mutating one would hide fresh writes for `nprobe < nlist`; centroid-aware mutation assignment
> is owned by Slice 7. A canbench suite over clustered seeded datasets (dims 128/384/768 × `nlist`
> 16/64 × `nprobe` 1/4/8/16) records that `nprobe = nlist` returns the **same result set** as the exact
> scan at higher instruction cost (centroid + reverse-map lookups) while lower `nprobe` measurably
> reduces cost.
>
> **Slice 7 (implemented): production centroid training + shadow-version rebuild + dual-write.** Slice
> 7 turns the Slice 6 seeded partition fixtures into production-created `nlist > 1` indexes and lifts
> the Slice 6 "seeded multi-partition indexes are immutable" limitation: published `nlist > 1` indexes
> are now mutable because the active append routes by centroid whenever the active `nlist > 1`. It adds
> **one stable region**, `VECTOR_REBUILD_STATE` (MemoryId 12), a derived per-index rebuild lifecycle
> (`Idle` → `Sampling` → bounded `Building` steps → `ReadyToPublish` → `Cleaning` → `Idle`, with
> `Aborting` and a sampling-only `Failed`), and extends the `SubjectMapEntry` value with a second
> `shadow_slot` (serde-default, no repack) so publish is metadata-only (`VECTOR_INDEX_STABLE_LAYOUT`
> is now **13** regions, 0–12). Search resolves the live slot via
> `current_slot_for(active_index_version)` — the active `slot` while it matches, else the `shadow_slot`
> after the atomic version flip — across both read paths.
>
> Every long-running phase is **bounded and cursor-resumable**: `Sampling` collects exactly `nlist`
> distinct centroid candidates and writes the target centroids, `Building` shadows every live subject
> into its nearest target partition, `Cleaning` collapses `shadow_slot → slot` and drops the old
> version's pages, and `Aborting` drops the shadow version's pages. `admin_publish_vector_rebuild` is
> **O(1)** (completeness is an invariant of `Building` + dual-write, not a re-scan) and flips
> `active_index_version` + `nlist` + centroid metadata atomically. A rebuild must not lose canonical
> mutations that arrive mid-build, so the concurrency model is **dual-write to both the active and
> shadow `index_version` while the build is `Building`/`ReadyToPublish`**, plus collapse-on-touch
> during `Cleaning` (any state-changing mutation collapses the touched subject; a pure idempotent
> no-op is left for `cleanup_step`); `vector_upsert` / `vector_remove` remain the per-operation
> mutation primitives, while the Graph-to-vector batch endpoint is a bounded transport wrapper
> around them.
> boundary. Quiesce and
> delta-replay are rejected: quiesce blocks normal graph writes during training, and delta-replay
> reintroduces a watermark/clock reconciliation surface after Slice 4 already established
> incarnation-ordered mutation delivery. Slice 7 trains centroids by **sampling distinct live
> vectors** with nearest-centroid assignment. Slice 8 (below) improves centroid quality with bounded
> k-means-lite over that bounded sample and adds partition health/status signals. Slice 9 (below) then
> added page-meta tombstone health, a rebuild recommendation/trigger, and a heap centroid cache.
> Slice 10 (below) then added Router-forwarded maintenance plus a Router-owned policy / vector-executed
> bounded-step boundary. Full/balanced k-means, autonomous partition tombstone-cleanup, GQL vector
> syntax, `VectorSubject::Edge`, and distributed fan-out remain Slice 11+ follow-ups.
>
> **Slice 8 (implemented): bounded training quality + head-only partition health.** Slice 8 inserts a
> deterministic `Training` phase between `Sampling` and `Building` in the existing rebuild lifecycle
> (`Idle` → `Sampling` → `Training` → `Building` → `ReadyToPublish` → `Cleaning` → `Idle`) and adds **no
> new stable region** — the `Training` variant reuses `VECTOR_REBUILD_STATE`. `Sampling` now collects a
> *bounded distinct candidate pool* (typically more than `nlist`) instead of stopping at the first
> `nlist` distinct vectors; `Training` runs k-means-lite over that pool, one iteration per
> `admin_vector_rebuild_step` (assign to nearest centroid, recompute per-cluster means, empty cluster
> keeps its previous centroid), for at most `MAX_REBUILD_TRAINING_ITERATIONS`, then writes exactly
> `nlist` centroids and enters `Building`. Both axes stay bounded: per-message work is capped so
> `candidate_count * nlist * dims <= MAX_REBUILD_TRAINING_DISTANCE_OPS`, and the durable rebuild state
> (`candidates + centroids`) is capped by `MAX_REBUILD_STATE_BYTES`, the **Candid-encoded** `to_bytes()`
> length (with `MAX_REBUILD_STATE_OVERHEAD_BYTES` reserved); every `Training` persist encodes the value
> **once**, re-checks the encoded length, and stores those bytes verbatim (`RawRebuildState`), failing
> closed with `InvalidRebuildParams` (never traps). Storing the pre-encoded bytes (on-disk format
> unchanged) lets the size guard and the persist share a single encode, which cut the full-rebuild
> canbench instruction counts by ~17–21% (see `canbench_results.yml`).
> Mutations during `Training` are active-only and shadowed later by `Building`; publish, dual-write,
> `Cleaning`, `Aborting`, and the search wire are unchanged. A new Router-guarded query
> `admin_vector_partition_health(index_id)` returns an integer-only, head-only
> `VectorPartitionHealthSummary { nlist, partitions_examined, live_rows, page_count,
> max_partition_live_rows }` (O(nlist), no page scan); tombstone-ratio health is deferred to Slice 9+.
>
> **Slice 9 (implemented): maintenance visibility, rebuild recommendation, and a heap centroid
> cache.** Slice 9 adds maintenance/cache surface only — no change to canonical ownership, search
> semantics, or stable layout (no new stable region). Three additions: (1) a bounded, cursor-resumable
> page-meta tombstone-health scan, `admin_vector_partition_health_step(index_id, cursor, max_pages)`,
> returning an additive `VectorPartitionHealthStep { partial: VectorPartitionPageHealth { index_id,
> index_version, page_count, total_rows, physical_live_rows, tombstoned_rows }, cursor, exhausted }`
> scoped to the active version (mirrors the `VectorSlabStatsStep` merge / no-snapshot-isolation
> contract). It complements the head-only skew summary with the tombstone signal the head cannot see.
> (2) A pure `recommend_partition_maintenance(summary, page_health, policy)` and a Router-guarded
> `admin_start_vector_rebuild_if_recommended(index_id, attested_page_health, policy, target_nlist,
> sample_limit)` that, when health crosses a `VectorMaintenancePolicy` (split `recommended`/`required`
> basis-point thresholds for tombstone ratio and partition skew, judged with `u128` cross-multiplication,
> independently min-row gated), begins an existing rebuild and returns the three-state
> `VectorMaintenanceRecommendation { Healthy, RebuildRecommended, RebuildRequired }` (no autonomous timer
> in this slice). The head-only skew summary is **recomputed server-side** from the authoritative
> partition heads (O(`nlist`)), so it has no caller-trust surface; only the page-meta tombstone health is
> trusted admin input (proving its completeness would need an unbounded scan). A generation guard rejects
> page health attested against a different generation (`StaleMaintenanceHealth`), and `target_nlist =
> None` defaults to `def.nlist` only when `>= 2`. (3) A
> transient **heap** centroid cache (`admin_vector_centroid_cache_warmup` / `_clear` / `_status`,
> reporting `VectorCentroidCacheStatus { entries, bytes, max_bytes }`): the partition-page `#[query]`
> search reads decoded centroids from the heap when warmed (skipping the `IVF_CENTROIDS` stable read +
> `f32` decode) and otherwise reads stable for that call only. Because IC `#[query]` execution is
> non-committing, the cache is **read-only** on the query path; population/eviction happen only on
> `#[update]` warmup/clear and a publish-time invalidation (the active generation changing). All new
> admin endpoints stay on the vector canister behind `guard_router_canister`; Router forwarding landed
> in Slice 10 (below).
>
> **Slice 10 (implemented): Router policy with vector-owned maintenance execution.** Slice 10 adds
> Router-forwarded maintenance ergonomics and a Router-owned maintenance policy catalog, and it does
> **not** move maintenance progress state into the Router. The Router remains the source of truth for
> vector-index definitions, targets, readiness, RBAC, and maintenance policy. The vector-index canister
> remains the owner of derived maintenance execution state: page-health scan cursors, merged page-health
> counters, rebuild phase, cleanup/abort cursors, and centroid-cache invalidation. Manual maintenance is
> Router-push: the Router validates policy/readiness and forwards a policy snapshot to a router-guarded
> vector endpoint. Future automatic maintenance is vector-index-pull: a vector timer may ask the Router
> for the current policy/readiness snapshot before advancing one bounded step. The vector canister must
> treat the snapshot as step input, not as a durable copy of Router-owned policy. If the Router is
> unreachable, policy is disabled, target/readiness no longer match, or the snapshot epoch is stale,
> automatic maintenance is a no-op/fail-closed. Autonomous stepping should stop at `ReadyToPublish` by
> default; publishing the active index version remains an explicit Router-forwarded operation unless a
> later ADR adds an explicit `allow_auto_publish` policy.

## Context

Gleaph already has two vector-related surfaces:

- edge inline value vectors (`EdgeInlineValueEncoding::VectorF32`) used by graph execution while traversing
  edges; and
- graph-index canisters that hold derived postings for router-owned query routing.

The proposed vector index canister should support GraphRAG-style candidate generation, but it must
not become a standalone vector database or a second source of truth for graph entities. Gleaph's
existing architecture separates:

- **Router:** external API, authorization, planning, target resolution, orchestration, and result
  merge;
- **Graph:** canonical vertex, edge, property, label, and traversal state; and
- **Index canisters:** derived state rebuilt from graph canonical state.

That split must continue to hold for vectors.

## Problem

Vertex embeddings need a durable home before an ANN index can be trusted.

If vector bytes live only in the vector index canister, the index becomes canonical state and cannot
be rebuilt from graph shards. If vertex embeddings are stored as ordinary properties forever,
Gleaph loses important embedding invariants: fixed dimensions, encoding, normalization policy,
versioning, delete behavior, and bounded backfill into a vector index. If vertex embeddings are
placed in edge inline value storage, vertex semantics and traversal-critical edge inline value semantics are
mixed.

We need a plan that:

- keeps vertex embeddings canonical on the graph shard;
- keeps edge inline value vectors available for edge-local traversal predicates;
- treats vector indexes as derived candidate-generation structures;
- supports bounded IC execution and upgrade-safe repair/backfill; and
- makes `ivf_flat` the standard vector-index kind while leaving room for later `flat`, `ivf_pq`,
  or experimental `hnsw` implementations behind the same query semantics.

## Existing architecture assessment

Property index and label index maintenance already provide the right architectural precedent:

- graph shards hold canonical values;
- index canisters hold derived lookup state;
- router resolves index lookup targets from live registry state;
- graph shards enqueue derived index ops after canonical mutation;
- failed index flushes converge through durable repair/backfill paths; and
- posting keys do not embed graph-wide routing policy that belongs to the router.

The existing edge vector path is not an ANN index. It is a graph-executor scan over edge inline value
bytes, with SIMD and bounded L2 improvements in favorable cases but still worst-case `O(n * d)`.
That path remains valid for traversal-critical edge-local vector predicates.

## Decision

### 1. Graph owns canonical vertex embeddings

Vertex embeddings are canonical graph state. `VertexEmbeddingStore` lives in the graph canister
facade, not in the vector index canister.

The store is a dedicated stable store rather than an edge inline value extension. Slice 1 commits the
canonical key shape:

```text
(VertexId, EmbeddingNameId) -> EmbeddingRecord
```

The key is vertex-major and fixed-width, so vertex delete can enumerate every embedding owned by one
vertex. Backfill-by-embedding-name is deliberately not optimized in the canonical store; a later
derived `(EmbeddingNameId, VertexId)` access path may be added when vector-index backfill needs it,
but it must not become a second canonical store.

Minimum write-boundary invariants:

- embedding encoding is explicit;
- dimensions are fixed per embedding definition;
- byte width mismatches are rejected before stable mutation;
- vertex deletion removes or tombstones the vertex's embeddings through the graph-owned write path;
- embedding updates produce deterministic remove/insert deltas for derived vector indexes; and
- graph canonical state remains sufficient to backfill the vector index.

### 2. Edge payload vectors remain separate

`EdgeInlineValueEncoding::VectorF32` remains the representation for edge-local, traversal-critical
vectors. It is appropriate when query execution evaluates a vector predicate while expanding edges.

Vertex embeddings are not stored in edge inline values. They describe a vertex's semantic representation
and participate in vector candidate generation, backfill, and index synchronization.

### 3. Vector index canisters are derived candidate generators

The vector index canister owns ANN/search structures and returns candidates. It does not own final
query semantics or canonical graph state.

Read APIs return bounded candidate pages, not materialized graph results:

```text
VectorHit {
  shard_id,
  subject,
  score,
}

VectorSubject =
  Vertex { vertex_id }
  | Edge { owner_vertex_id, label_id, slot_index }
```

Initial implementation should focus on vertex embeddings. Edge subjects may be added later when
there is a demonstrated need to externalize edge-inline-value vector search from graph execution.

### 4. Derived vector storage uses index-local ids and partition-local vector pages

The derived vector index canister stores copied vector bytes in search-optimized stable-memory
structures. These bytes are derived from graph-owned canonical embeddings and are rebuildable by
backfill.

`VectorId` is **index-local**, not globally unique:

```text
(index_id, vector_id) -> one vector slot
```

The vector index does not compute a physical offset directly from `index_id`. Instead, every live
vector resolves through a slot reference:

```text
VECTOR_INDEX_DEFS[index_id] -> { kind: ivf_flat, encoding, dims, metric, active_version, ... }
VECTOR_ID_TO_SLOT[(index_id, vector_id)] -> SlotRef { version, partition_id, page_id, slot, generation }
VECTOR_SUBJECT_TO_ID[(index_id, subject)] -> { vector_id, generation }
VECTOR_ID_TO_SUBJECT[(index_id, vector_id)] -> VectorSubject   # Slice 6 reverse locator (partition-page scan)
VECTOR_REBUILD_STATE[index_id] -> rebuild lifecycle             # Slice 7 bounded shadow-version rebuild
```

The Slice 7 `SubjectMapEntry` value (the `VECTOR_SUBJECT_TO_ID` row, simplified above) additionally
carries a second `shadow_slot: Option<SlotRef>` alongside the active `slot`; search resolves the live
slot via `current_slot_for(active_index_version)` so the atomic publish is a metadata-only version
flip rather than an O(N) subject rewrite.

`ivf_flat` stores full-vector bytes in partition-local fixed-width pages:

```text
VECTOR_PAGE[(index_id, version, partition_id, page_id)] ->
  header {
    encoding,
    dims,
    stride_bytes,
    len,
    live_len,
    tombstone_bitmap,
    next_page,
  }
  slot_table [vector_id, generation, flags]
  vector_bytes [slot0][slot1]...[slotN]
```

The first implementation supports `F32` only, but the shape is intentionally encoding-aware.
`VECTOR_INDEX_DEFS[index_id] -> { encoding, dims, stride_bytes }` is the vector-index analogue of
LabeledLARA's `label_id -> payload byte width`: the owner metadata fixes the byte width before any
vector bytes are read. Different dimensions or encodings use different indexes or physical page
families. Slots inside one page always have one fixed stride.

Pages are the read and scoring unit. Search reads selected pages into heap buffers and performs
SIMD exact scoring over page-local contiguous vector bytes. Deletes set page tombstone bits and
remove the subject map entry. Updates append a new vector slot and tombstone the old slot. Initial
`VectorId` values are not reused; reuse requires a generation-bearing handle and is out of scope for
the first derived index.

Search APIs return `VectorHit` by resolving `vector_id` back to `VectorSubject`; `VectorId` remains
an internal storage handle.

### 5. Router owns vector query orchestration

Router resolves vector-index lookup targets from the graph registry and runtime config, just as it
does for property indexes. Router owns authorization, query planning integration, fan-out, merge,
and seed construction.

Graph shards execute final filtering, traversal, materialization, and reranking that requires
canonical graph state. The vector index canister must not call graph shards on the query hot path.

### 6. `ivf_flat` is the standard vector-index kind

Gleaph's default vector index kind is `ivf_flat`: centroid/partition routing with full-vector exact
rerank. The name is intentionally generic ANN terminology rather than `spann`, because Gleaph adopts
SPANN-inspired constraints without implementing SPANN's SSD-specific physical design or initial
closure replication.

`flat` remains useful as a small-index/debug implementation and as a correctness baseline, but it is
not the standard production index kind. The first `ivf_flat` implementation may share most storage
code with `flat`: a flat scan is simply a bounded scan over all vector pages.

The standard `ivf_flat` implementation deliberately prioritizes:

- stable layout correctness;
- dimension and encoding contracts;
- bounded page/cursor APIs;
- graph mutation and repair convergence;
- backfill;
- query integration; and
- benchmark baselines.

All candidate scoring uses full-vector exact distance over derived vector-page bytes. `ivf_flat` may
use SIMD or bounded-distance pruning where profitable, but it must be described honestly as
centroid-pruned exact rerank, not PQ or graph traversal.

### 7. `ivf_flat` uses SPANN-inspired partition pages and excludes CSR

The `ivf_flat` implementation uses SPANN-inspired partitioning, not a canonical CSR-style compact
list. IVF state is derived, so update simplicity and bounded repair/backfill are more important
than maximizing locality through static row compaction.

Planned stable-memory shape:

```text
IVF_CENTROIDS[(index_id, version, partition_id)] -> centroid vector
IVF_CENTROID_META[index_id] -> { active_version, dims, nlist, encoding, metric, centroid_epoch }
IVF_PARTITION_HEADS[(index_id, version, partition_id)] ->
  { first_page, mutable_page, page_count, live_len }
VECTOR_PAGE[(index_id, version, partition_id, page_id)] -> fixed-width vector page
```

Search path:

1. Score query against a bounded heap cache of centroids.
2. Select partitions using `nprobe` plus query-aware pruning and a page/read budget.
3. Read selected partition-local vector pages from stable memory.
4. Skip tombstoned slots.
5. Perform exact SIMD rerank over page-local vector bytes.
6. Return bounded top-k candidates.

The SPANN-inspired constraints are:

- centroid metadata is stored durably in stable memory and may be mirrored into a bounded heap cache;
- partition length and page count must be observable (`live_len`, `page_count`);
- insertion should prefer balanced assignments among near centroid candidates instead of allowing one
  hot partition to dominate tail latency;
- query-aware pruning should reduce unnecessary partition/page reads for easy queries; and
- full-vector rerank stays mandatory for IVF_FLAT.

Closure replication, PQ compression, and graph-style HNSW traversal are deferred. CSR-style IVF
snapshots are intentionally excluded from the vector-index roadmap: they optimize locality for a
mostly static matrix, but they make insert/delete/repair behavior heavier and create a second
derived representation to maintain. If partition-locality benchmarks fail, the preferred follow-up
optimizations are page sizing, balanced assignment, read-budget pruning, tombstone cleanup, and
encoding-specific page reads, not CSR conversion.

Required IC implementation gates:

- centroid cache is derived acceleration only; stable centroid metadata is authoritative;
- cache miss behavior is explicit: either fail closed with `CacheNotReady` or use a bounded stable
  centroid scan fallback chosen by the API contract;
- rebuild creates a shadow `version` and publishes it by atomically switching `active_version`;
- a shadow rebuild is consistency-safe only if vector mutations dual-write to active and shadow
  versions while the build is active;
- production mutation assignment for `nlist > 1` indexes must use centroid-aware partition placement,
  not the Slice 6 test/bench-only seeded fixture shortcut;
- balanced partition assignment is required before `ivf_flat` is considered production-optimized, not
  an optional optimization;
- partition maintenance starts with head-only skew visibility (`live_len`, `page_count`) before adding
  page-scanned or persisted tombstone accounting; tombstone ratio is a cleanup/maintenance follow-up,
  not a Slice 8 training-quality prerequisite;
- any future page/read/instruction budget that can stop a scan early must surface an explicit
  partial/cursor/error contract; Slice 6 intentionally has no silent mid-scan truncation; and
- `flat` baseline canbench results must be kept so `ivf_flat` recall, latency, and cycle costs are
  compared against exact scan at the same dataset size.

### 7.1 Slice 7 rebuild contract (implemented)

Slice 7 does not change the canonical/derived boundary: graph shards remain authoritative for
embeddings and the vector index canister remains a derived candidate generator. The new state and
invariants live inside the vector index canister.

Rebuild state is persisted in the `VECTOR_REBUILD_STATE` stable region (MemoryId 12), registered in
`VECTOR_INDEX_STABLE_LAYOUT` and reflected in ADR 0007 and the stable-memory inventory. Every
long-running phase carries a resume cursor so admin steps stay bounded:

```text
VECTOR_REBUILD_STATE[index_id] ->
  Idle
  | Sampling { target_index_version, nlist, sample_limit, subject_cursor, candidates }
  | Building { target_index_version, nlist, subject_cursor }
  | ReadyToPublish { target_index_version, nlist }
  | Cleaning { old_index_version, old_nlist, subject_cursor, page_cursor }
  | Aborting { target_index_version, target_nlist, subject_cursor, page_cursor }
  | Failed { target_index_version, reason }   # sampling-only; nothing persisted
```

Execution flow:

1. `admin_start_vector_rebuild(index_id, nlist, sample_limit)` is **O(1)**: it validates params
   (`2 <= nlist <= MAX_NLIST`, `nlist <= sample_limit <= MAX_REBUILD_SAMPLE_LIMIT`, the combined-state
   and op-budget envelope described in §7.2 so the durable rebuild state cannot grow oversized for
   large-`dims` indexes, `F32`/`L2Squared`, `Idle`) and records `Sampling` with a
   shadow `target_index_version` greater than `active_index_version`. It does not scan subjects or
   write centroids.
2. Bounded `admin_vector_rebuild_step(index_id, max_subjects)` drives `Sampling` (collect a bounded
   distinct candidate pool), then `Training` (deterministic k-means-lite over that pool; see §7.2),
   which writes the target centroids and enters `Building` (read each live subject's current bytes,
   assign to its nearest target centroid, and append rows into the shadow version only), reaching
   `ReadyToPublish`. Each step is bounded on both axes — the caller's
   `max_subjects` is clamped to `1..=MAX_REBUILD_STEP_WORK` (row count) and the step also breaks once
   the transient vector bytes it buffers on the heap reach `MAX_REBUILD_STEP_VECTOR_BYTES` (since
   `stride_bytes` scales with `dims`), always buffering at least one vector to guarantee progress — so
   neither a malformed huge `max_subjects` nor a large `dims` can make one message perform an
   unbounded scan or buffer unbounded heap bytes. Insufficient distinct live vectors ends in `Failed`
   (sampling-only, O(1) recoverable to `Idle` via abort).
3. While `Building` or `ReadyToPublish` is active, `vector_upsert` and `vector_remove` dual-write:
   they update the active version used by current search and the shadow version being prepared for
   publish. Stale replay, remove, update, resurrection, and incarnation ordering are identical in both
   versions.
4. `admin_publish_vector_rebuild(index_id)` is **O(1)** — completeness is an invariant established by
   `Building` + dual-write, so it performs no live-subject scan — and atomically switches
   `VectorIndexDef.active_index_version`, `nlist`, and centroid metadata to the shadow version, then
   enters `Cleaning`. Empty partitions are allowed.
5. `admin_vector_rebuild_cleanup_step(index_id, max_work)` drives the bounded post-publish `Cleaning`
   teardown (collapse `shadow_slot → slot`, repoint the reverse locator, drop the old version's
   pages/heads/centroids) back to `Idle`, and the bounded `Aborting` teardown after an abort.
   `max_work` is clamped to `1..=MAX_REBUILD_STEP_WORK` like the rebuild step.
6. `admin_abort_vector_rebuild(index_id)` returns straight to `Idle` from `Sampling`/`Training`/`Failed`
   (nothing persisted) or enters bounded `Aborting` from `Building`/`ReadyToPublish`, keeping the active version
   unchanged and dropping the shadow version's pages so it is unreachable from search.

The required tests are:

- upsert during rebuild is visible after publish;
- remove during rebuild does not resurrect after publish;
- delete/reinsert during rebuild preserves the newer incarnation after publish;
- stale repair replay cannot create a shadow-only live row;
- abort never changes search results;
- publish switches search to the target version only after completeness checks;
- `nprobe = nlist` after publish matches the exact subject-map scan; and
- mutation write cost during rebuild is measured separately from normal mutation cost.

### 7.2 Slice 8 training-quality contract (implemented)

Slice 8 keeps the Slice 7 ownership and consistency model: graph shards remain authoritative for
embeddings, the vector index canister owns IVF internals, Router drives admin orchestration, and
publish remains a metadata-only `active_index_version` flip once the shadow version is complete. The
public search request stays algorithm-neutral; Slice 8 does not add public `nprobe`, silent
truncation, or partial-result semantics. No new stable region is added (the `Training` variant reuses
the existing `VECTOR_REBUILD_STATE` region).

The Slice 8 change improves `ivf_flat` centroid quality without introducing an unbounded training
job. Slice 7's training MVP used the first `nlist` distinct live sample vectors as centroids; Slice 8
replaces that with bounded, deterministic k-means-lite over a bounded candidate pool:

```text
VECTOR_REBUILD_STATE[index_id] ->
  Idle
  | Sampling { target_index_version, nlist, sample_limit, subject_cursor, candidates }
  | Training { target_index_version, nlist, sample_limit, iteration, candidates, centroids }
  | Building { target_index_version, nlist, subject_cursor }
  | ReadyToPublish { target_index_version, nlist }
  | Cleaning { old_index_version, old_nlist, subject_cursor, page_cursor }
  | Aborting { target_index_version, target_nlist, subject_cursor, page_cursor }
  | Failed { target_index_version, reason }
```

Invariants:

1. `admin_start_vector_rebuild` remains **O(1)**. It validates the bounded parameter envelope —
   `2 <= nlist <= MAX_NLIST`, `nlist <= sample_limit <= MAX_REBUILD_SAMPLE_LIMIT`, the combined-state
   bound `2 * nlist * stride_bytes + MAX_REBUILD_STATE_OVERHEAD_BYTES <= MAX_REBUILD_STATE_BYTES`, and
   the per-iteration op bound `nlist^2 * dims <= MAX_REBUILD_TRAINING_DISTANCE_OPS` — and enters
   `Sampling`; it does not scan subjects, train centroids, or write pages.
2. `Sampling` accumulates a **bounded distinct candidate pool** (typically more than `nlist`), capped
   by the smaller of the combined-state byte budget (`MAX_REBUILD_STATE_BYTES` reserving the centroids
   and `MAX_REBUILD_STATE_OVERHEAD_BYTES`) and the distance-op count
   (`MAX_REBUILD_TRAINING_DISTANCE_OPS / (nlist * dims)`). If the live range or `sample_limit` exhausts
   with fewer than `nlist` distinct candidates, the rebuild enters `Failed`. `MAX_REBUILD_STATE_BYTES`
   is the **Candid-encoded** cap on the whole rebuild-state value; before persisting any `Training`
   value (the `Sampling → Training` transition and each post-iteration `Training → Training`
   re-persist) the canister encodes the value once, re-checks the encoded length, and stores those
   bytes verbatim (`RawRebuildState`), failing closed with `InvalidRebuildParams` (never `assert!`/trap)
   if it would exceed the cap.
3. `Training` runs deterministic k-means-lite only over the bounded candidate pool: exactly one
   iteration per `admin_vector_rebuild_step` (assign each candidate to its nearest current centroid,
   recompute centroids as the per-cluster mean), bounded so `candidate_count * nlist * dims <=
   MAX_REBUILD_TRAINING_DISTANCE_OPS` with transient `O(nlist * dims)` sums/counts. It runs at most
   `MAX_REBUILD_TRAINING_ITERATIONS` iterations, keeps exactly `nlist` dimension-valid centroids (an
   empty cluster keeps its previous centroid), and never scans the full subject map.
4. Once `Training` finishes, it writes exactly `nlist` target centroids and transitions to the
   existing Slice 7 `Building` phase. `Building`, dual-write, O(1) publish, `Cleaning`, and `Aborting`
   preserve the Slice 7 semantics. A mutation during `Training` is active-only and is later shadowed
   when `Building` walks every live subject.
5. Slice 8 exposes a **head-only** partition-health summary for admin visibility via the
   Router-guarded query `admin_vector_partition_health(index_id)`, scanning the active version's
   `PartitionHead` records bounded by `nlist <= MAX_NLIST`: `VectorPartitionHealthSummary { nlist,
   partitions_examined, live_rows, page_count, max_partition_live_rows }`. The wire is integer-only;
   callers derive average live rows and skew ratio from raw counts. Slice 8 does **not** report
   `tombstoned_rows`, `total_rows`, or tombstone ratio, because those require either a bounded page
   scan or persisted counters. Tombstone accounting is deferred to the cleanup/maintenance slice.
   These signals are derived index health metadata, not canonical embedding state.
6. Search remains exact over the selected partitions. Any future page/read/instruction budget that can
   stop a scan early still requires an explicit partial/cursor/error contract; Slice 8 adds no
   silent mid-scan truncation.

Slice 8 test coverage proves deterministic training, bounded sampling/training (combined-state and
op-budget rejection, encoded-state bound, trap-free boundary), parity with the exact subject-map scan
at `nprobe = nlist`, unchanged dual-write/publish semantics, and observable partition-health output.
Canbench compares Slice 7 sampled-centroid rebuild/search behavior
against Slice 8 k-means-lite rebuild/search behavior on clustered datasets.

### 7.3 Slice 9 maintenance-visibility + centroid-cache contract (implemented)

Slice 9 keeps the Slice 7/8 ownership, consistency, and search model unchanged and adds **no stable
region** (heap-only cache; reuses existing `VECTOR_PAGE_META`, `VECTOR_PARTITION_HEADS`, and
`IVF_CENTROIDS`). It closes the operational gap left by the head-only Slice 8 summary:

1. **Bounded page-meta tombstone health.** `admin_vector_partition_health_step(index_id, cursor,
   max_pages)` (Router-guarded `#[query]`) scans at most `max_pages` `VECTOR_PAGE_META` entries of the
   active `(index_id, active_index_version)` — reading only page meta, never row bytes or
   `VECTOR_SUBJECT_TO_ID` — and returns an additive `VectorPartitionHealthStep { partial:
   VectorPartitionPageHealth { index_id, index_version, page_count, total_rows, physical_live_rows,
   tombstoned_rows }, cursor, exhausted }`. Callers repeat until `exhausted` and sum the partials. It
   mirrors the `VectorSlabStatsStep` merge / no-snapshot-isolation contract; `max_pages` is clamped
   server-side and a malformed or wrong-scope cursor returns `InvalidStatsCursor` rather than trapping.
   `physical_live_rows` is `VectorPageMeta.live_count` (not subject-freshness). This complements the
   head-only skew summary with the tombstone signal the head cannot see.
2. **Recommendation + trigger.** The pure `recommend_partition_maintenance(summary, page_health,
   policy)` returns `VectorMaintenanceRecommendation { Healthy, RebuildRecommended, RebuildRequired }`
   as the max severity across two independently min-row-gated signals — tombstone ratio
   (`tombstoned_rows / total_rows`) and partition skew (`max_partition_live_rows * nlist / live_rows`)
   — compared against the split `recommended_*_bps`/`required_*_bps` thresholds of a
   `VectorMaintenancePolicy` using `u128` cross-multiplication (no floats, no overflow; an inverted
   policy returns `InvalidMaintenancePolicy`). `admin_start_vector_rebuild_if_recommended(index_id,
   attested_page_health, policy, target_nlist, sample_limit)` (Router-guarded `#[update]`) recomputes
   the head-only skew `summary` server-side from the authoritative partition heads (O(`nlist`)),
   re-derives the recommendation, and, when not `Healthy`, begins an existing rebuild, returning the
   recommendation (a `Healthy` result is an explicit no-op, not an error). There is **no autonomous
   timer** in this slice. The skew summary therefore has no caller-trust surface; only the page-meta
   tombstone health is *trusted admin input* (proving its completeness would require an unbounded scan).
   A generation guard rejects page health attested against a different generation
   (`attested_page_health.index_id`/`index_version` must equal the active version, else
   `StaleMaintenanceHealth`). `target_nlist = None` defaults to `def.nlist` only when `>= 2`
   (degenerate `nlist = 1` requires an explicit `target_nlist`, since rebuild requires `nlist >= 2`).
3. **Heap centroid cache.** A transient `thread_local` cache keyed by `(index_id ->
   {version, nlist, dims})` memoizes the decoded centroid set so the partition-page `#[query]` search
   skips the `IVF_CENTROIDS` stable read + `f32` decode. Because IC `#[query]` execution is
   non-committing, the query path is **read-only**: a warmed entry is used, and a miss reads stable for
   that call only without populating the cache. Population/eviction are `#[update]`-only —
   `admin_vector_centroid_cache_warmup(index_id)` (caches a ready `nlist > 1` set; drops any stale
   entry for degenerate/untrained indexes), `admin_vector_centroid_cache_clear()`, and a publish-time
   `invalidate(index_id)` when the active generation changes; the cache is byte-bounded and dropped on
   init/upgrade. `admin_vector_centroid_cache_status()` reports `VectorCentroidCacheStatus { entries,
   bytes, max_bytes }` — per-query hit/miss is intentionally **not** tracked (a query cannot commit
   counters on IC).
4. All new admin endpoints stay on the vector canister behind `guard_router_canister`, driven directly
   by the router principal (the Slice 7/8 precedent); Router forwarding is a deferred follow-up. Search
   semantics, the public request, dual-write, publish, `Cleaning`, and `Aborting` are unchanged.

Slice 9 test coverage proves the bounded page-meta scan's additive merge / version scoping / cursor
scope-check, the recommendation thresholds (split bands, inclusive crossing, independent gating,
`u128` non-overflow), the trigger's generation guard and `nlist` resolution, and the centroid cache's
cold-vs-warm search parity, publish invalidation, and router-guard — at the store layer (unit) and end
to end via PocketIC (`sender = router`). Canbench adds the page-meta health scan (clean and
tombstone-heavy), cold-vs-warm partition-page search, and centroid-cache warmup cost.

### 7.4 Slice 10 maintenance-orchestration boundary (implemented)

Slice 10 is a boundary refinement over the Slice 9 maintenance surface, not a new vector-index
algorithm. It preserves this ownership split:

- **Router-owned source of truth.** The Router owns vector-index definitions, target resolution,
  activation/readiness, RBAC, and graph/index-scoped maintenance policy. A policy record may include
  `enabled`, `VectorMaintenancePolicy`, `target_nlist`, `sample_limit`, per-step budgets, and future
  automation flags, but the Router does not own vector page cursors, rebuild cursors, or centroid-cache
  state.
- **Vector-owned execution state.** The vector-index canister owns the derived maintenance state that
  is coupled to its stable layout: page-health scan cursors and merged counters, rebuild state,
  cleanup/abort cursors, active/shadow index versions, and heap centroid-cache invalidation. Keeping
  these in the vector canister avoids a split-brain scheduler where Router state claims one phase while
  the vector canister's durable rebuild state is elsewhere.
- **Manual mode is Router-push.** For operator-driven maintenance, the Router resolves the graph/index,
  checks readiness and RBAC, reads its policy, and forwards a policy snapshot/budget to a router-guarded
  vector endpoint. The vector canister advances at most one bounded maintenance unit and returns status.
- **Future automatic mode is vector-index-pull.** For timer-driven maintenance, the vector canister may
  ask the Router for the current policy/readiness snapshot before each bounded step. The fetched
  snapshot is step input only; the vector canister must not persist Router policy as an independent
  source of truth. The snapshot should carry a policy/catalog epoch (or equivalent version) so stale
  automatic work can fail closed.
- **Fail-closed automation.** If Router policy lookup fails, policy is disabled, the graph/index target
  no longer matches this vector canister, dispatch/readiness is false, or the snapshot epoch is stale,
  automatic maintenance performs no work. A failed automatic step must not publish, abort, or rewrite
  Router-owned policy.
- **Publish remains explicit.** The bounded maintenance step may scan, recommend, start, and drive
  rebuild work, but it should stop at `ReadyToPublish` by default. Publishing flips
  `active_index_version`, so it remains a separate Router-forwarded operation unless a later ADR adds an
  explicit `allow_auto_publish` policy and its recovery semantics.

This keeps the operational direction symmetric with Gleaph's other canister boundaries: Router owns
global policy and routing authority; the canister that owns derived stable state owns the maintenance
state that mutates that derived representation.

**As implemented (Slice 10):**

- **Vector execution state.** A new stable region `VECTOR_MAINTENANCE_STATE` (MemoryId 14) holds a
  per-index `VectorMaintenanceState` (`Idle`; `Scanning { cursor, exhausted, merged }` carrying the
  scoped `VectorPartitionPageHealth`; `Failed(VectorMaintenanceFailure { code, message })`). It
  persists across upgrade (it holds mid-orchestration scan progress) and is cleared only on canister
  init/reset, the opposite of the heap-only centroid cache. The `Failed` message is truncated to a
  bounded length so persisted state size never depends on a downstream error string. The router-guarded
  `admin_vector_maintenance_step(index_id, VectorMaintenanceStepRequest)` advances exactly one bounded
  unit: it drives an in-flight rebuild/cleanup first, otherwise runs one `partition_page_health_step`,
  and on scan exhaustion validates `merged.index_version == active_index_version` before recomputing the
  head summary and running `recommend_partition_maintenance`. Two generation guards keep it correct
  across an active-version flip: a stale cursor mid-scan (`InvalidStatsCursor`) restarts from the lower
  bound, and the exhausted→recommend boundary re-checks the merged generation. `admin_vector_maintenance_status`
  (query) and `admin_vector_maintenance_reset(index_id)` (update, `Idle` from any state including
  `Failed`, without touching the rebuild state) complete the surface.
- **Router policy SSOT.** A new Router stable region `ROUTER_VECTOR_MAINTENANCE_POLICIES` (MemoryId 44)
  stores a per-`(graph_id, index_id)` `VectorMaintenancePolicyRecord { enabled, policy, target_nlist,
  sample_limit, scan_max_pages, rebuild_max_subjects, cleanup_max_work }`, default absent/disabled.
  Policy authorship (`admin_set_/disable_/delete_vector_maintenance_policy`, validated for
  `recommended_*_bps <= required_*_bps`, nonzero budgets, and an existing definition) is `authorize_index_ddl`;
  stepping/reads/reset are a new Admin-only `authorize_vector_maintenance`.
- **Router forwarding.** The Router exposes the full Slice 7-9 maintenance surface as forwards to the
  resolved single vector target (reads as `#[query(composite = true)]`, mutators/drivers as `#[update]`),
  each gated on resolve + non-anonymous target + dispatch readiness. The push step
  `admin_vector_maintenance_step(graph, index_id)` returns `Disabled` when no enabled policy exists,
  otherwise snapshots the policy into a `VectorMaintenanceStepRequest` and forwards one bounded unit.
  `vector_maintenance_status(graph, index_id)` reports Router policy/readiness plus the forwarded
  execution and rebuild state (cursors present/absent, not decoded).
- **Publish stays explicit.** The bounded step stops at `ReadyToPublish` (returns `AwaitingPublish`);
  flipping `active_index_version` is the separate forwarded `admin_publish_vector_rebuild`. Future
  automatic mode (vector-index-pull) is documented above but not implemented.

### 8. Query syntax stays semantic; algorithm choice belongs to index definition

GQL query syntax should not expose physical index kinds such as `ivf_flat`, `ivf_pq`, or `hnsw`.
Queries express vector-search intent: embedding field, query vector, metric-compatible scoring,
top-k, threshold, and rerank needs. Router chooses the matching vector index from graph metadata and
runtime configuration.

Physical algorithm selection belongs to index definition or Router/index configuration:

```text
algorithm: "ivf_flat"
metric: "cosine" | "l2"
dims: 1536
encoding: "f32"
```

This preserves room for later `ivf_pq`, `hnsw`, or `flat` implementations without changing
user-facing query semantics. If a future feature needs query-time control such as an approximate
search budget, it should be expressed as semantic quality or cost hints, not as direct access to
internal index structures.

### 9. IVF/PQ/HNSW are later phases

The algorithm roadmap is:

1. **IVF_FLAT (`ivf_flat`)** — standard vector-index kind: centroids, balanced partition pages,
   bounded `nprobe`, query-aware pruning, exact rerank.
2. **Flat (`flat`)** — exact full-vector scan over all vector pages; small-index/debug/correctness
   baseline.
3. **IVF_PQ** — codebooks, PQ codes, approximate scoring, full-vector rerank.
4. **HNSW experimental** — only after stable-memory update/delete, bounded instruction, and repair
   behavior are specified.

Stable layout additions for IVF_PQ or HNSW require a later design update and, if the layout or query
contract changes materially, a follow-up ADR. Query syntax should remain algorithm-neutral unless a
future ADR proves that physical index selection must be user-visible.

## Consequences

### Positive

- Graph remains the source of truth for vertex embeddings.
- Vector indexes are rebuildable derived state, matching the property/label index model.
- Edge payload vectors keep their traversal-focused role.
- Router remains the only owner of cross-canister vector query orchestration.
- Initial implementation can validate stable storage, recovery, and query contracts before taking
  on ANN-specific complexity.
- Partition-local vector pages keep SIMD/range-read-friendly storage without requiring global CSR
  compaction.
- `ivf_flat` gives the standard index kind a familiar ANN name while preserving Gleaph-specific
  StableMemory, repair, and Router boundaries.
- Algorithm-neutral query syntax leaves room for later `ivf_pq`, `hnsw`, or `flat` implementations.

### Negative / costs

- A dedicated vertex embedding store adds a new graph stable domain.
- Graph mutations must maintain another derived-index delta stream.
- Router needs vector-specific target resolution and seed/merge integration.
- `ivf_flat` requires centroid training, heap-cache warmup, and partition balancing work before it
  can be production-effective.
- A poorly balanced or stale `ivf_flat` index can be worse than `flat`: it adds centroid/cache
  complexity without enough partition pruning benefit.
- Partition-local pages may duplicate some metadata per page; that cost is accepted so each stable
  read maps directly to a scoring unit.
- Paged partitions may read more pages than a compact static layout; that cost is accepted to keep
  insert/delete/repair and backfill simple.
- Append-and-tombstone updates require explicit cleanup/rebuild thresholds; otherwise dead slots
  become stable-read and instruction overhead.

## Alternatives considered

| Alternative | Why rejected |
|-------------|--------------|
| Store canonical vectors only in vector index canisters | Makes the index canister authoritative and prevents graph-owned rebuild/backfill. |
| Store vertex embeddings as edge inline values | Mixes vertex semantic state with traversal-critical edge-local payload storage. |
| Store embeddings only as ordinary vertex properties | Does not give embedding-specific dimension, encoding, update, and backfill invariants a clear owner. |
| Make `flat` the standard index kind | Simpler, but not enough for production-scale candidate generation; keep it as a baseline/debug implementation. |
| Expose algorithm names in query syntax | Couples user-facing query semantics to derived physical index choices; use index definition/config instead. |
| Start with HNSW | Commits graph-index-specific stable structures before the canonical/derived boundary and repair model are proven. |
| Let graph shards call vector index during query execution | Moves query orchestration away from Router and conflicts with the existing federation target. |
| Store vector bytes in blob-per-vector records | Hurts SIMD/range-read locality; fixed-width vector pages make batched scoring cheaper. |
| Use CSR for IVF list storage or snapshots | Optimizes locality for mostly static rows but creates heavier insert/delete/repair behavior and a second derived representation; partition-local pages fit Gleaph's mutation and repair model better. |
| Use global `VectorId`s across indexes | Makes placement, maintenance, and per-index deletion harder; index-local ids keep slot references small and page placement stable. |

## Implementation plan

1. **[done, slice 1]** Add `design/index/vector-index.md` as the planned design contract.
2. **[done, slice 1/2]** Add shared vector-index wire types to `graph-kernel` (canonical + sync/mutation).
3. **[done, slice 1]** Add a graph-owned `VertexEmbeddingStore` with fixed-dimension `F32` records first.
4. **[done, slice 2]** Add vector-index mutation deltas, volatile pending flush, durable repair journal integration, and
   bounded backfill from graph shards.
5. **[done, slice 2/4]** Add `graph-vector-index` canister with index-local `VectorId`s,
   partition-local fixed-width `F32` vector pages, tombstones, graph-scoped ownership, attach/detach,
   and incarnation-ordered idempotence.
6. **[done, slice 3/4]** Add Router vector-index catalog, target resolution, activation flag,
   graph-local vector routing, shard attach readiness, and bounded vector backfill.
7. **[done, slice 5]** Add router-guarded exact `ivf_flat` search over the live subject map, plus
   canbench exact-scan baselines.
8. **[done, slice 6]** Add `VECTOR_ID_TO_SUBJECT`, partition-page search, internal `nprobe`, seeded
   partition fixtures, and partitioned-search canbench baselines.
9. **[done, slice 7]** Add the bounded rebuild lifecycle state machine (`VECTOR_REBUILD_STATE`,
   MemoryId 12) and admin surface: start/step/status/publish/abort/cleanup for a shadow
   `index_version`.
10. **[done, slice 7]** Add dual-write mutation semantics while a shadow rebuild is active:
    active + shadow writes for upsert, remove, update, resurrection, and stale replay handling, plus
    collapse-on-touch for state-changing mutations during `Cleaning` (a pure idempotent no-op is left
    for `cleanup_step`).
11. **[done, slice 7]** Add centroid training MVP and production `nlist > 1` creation: sampled distinct
    centroids, nearest-centroid partition assignment, O(1) completeness, and atomic
    `active_index_version` publish via the two-slot `SubjectMapEntry` (`shadow_slot` +
    `current_slot_for`).
12. **[done, slice 8]** Add bounded k-means-lite training over a bounded candidate pool: introduce the
    `Training` rebuild phase, deterministic centroid refinement, scalar training status, per-message
    op-budget and Candid-encoded combined-state caps, and canbench comparisons against the Slice 7
    sampled-centroid baseline.
13. **[done, slice 8]** Add the head-only, integer-only partition-health query
    `admin_vector_partition_health` for `ivf_flat` (`nlist`, `partitions_examined`, `live_rows`,
    `page_count`, `max_partition_live_rows`) without making it canonical embedding state; tombstone
    accounting is deferred because it needs a page scan or persisted counters.
14. **[done, slice 9]** Add bounded page-meta tombstone health
    (`admin_vector_partition_health_step`), a pure recommendation + Router-guarded rebuild trigger
    (`recommend_partition_maintenance` / `admin_start_vector_rebuild_if_recommended`, no autonomous
    timer), and a heap centroid cache with explicit query read-only cache-miss behavior
    (`admin_vector_centroid_cache_warmup` / `_clear` / `_status`). No new stable region.
15. **[done, slice 10]** Add Router-forwarded maintenance ergonomics and a Router-owned
    graph/index-scoped maintenance policy catalog (`ROUTER_VECTOR_MAINTENANCE_POLICIES`, MemoryId 44),
    while keeping maintenance execution state in the vector-index canister (`VECTOR_MAINTENANCE_STATE`,
    MemoryId 14). Manual maintenance is Router-push (`admin_vector_maintenance_step` snapshots policy
    and forwards one bounded vector unit); future automatic maintenance (vector-index-pull) is
    documented but not implemented. Default stepping stops at `ReadyToPublish`; publish remains explicit.
16. **[planned, slice 11+]** Add full/balanced k-means or more advanced partition assignment and
    autonomous partition tombstone cleanup/rebuild scheduling beyond the Slice 10 bounded-step policy.
17. **[planned, slice 11+]** Add algorithm-neutral GQL/query planning integration and keep
    `algorithm: "ivf_flat"` in index definition/config, not query syntax.
18. **[ongoing]** Keep canbench baselines for embedding write, flush, backfill, exact scan,
    partitioned `ivf_flat`, rebuild/training, page-meta health scan, and centroid cache warm/cold.

## Required design updates

- `design/index/vector-index.md` records the planned store/index boundary.
- `design/README.md` links the vector index design document.
- `design/adr/README.md` links this ADR.
- When implementation allocates stable regions, update `design/storage/stable-memory-inventory.md`,
  `design/adr/0007-stable-memory-layout.md`, and the typed layout registry in
  `gleaph_graph_kernel::stable_layout`.
