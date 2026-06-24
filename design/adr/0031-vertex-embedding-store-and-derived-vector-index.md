# 0031. Vertex embedding store and derived vector index canister

Date: 2026-06-23
Status: accepted (partially implemented)
Last revised: 2026-06-24

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
>   target principal). Both exist because graph dispatch/backfill/repair are single-op, single-route.
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
> `nprobe` partition pruning landed in Slice 6 (below). Production IVF centroid training, candidate
> pagination, query ranking/merge, per-index/per-embedding fan-out, GQL vector-search syntax, and
> `VectorSubject::Edge` remain Slice 7+. This ADR fixes ownership, consistency, the standard
> `ivf_flat` vector-index kind, and the first derived vector-index stable-memory shape before the GQL
> query operator is committed.
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
> **Slice 7 rebuild design gate (required, not implemented in Slice 6).** Production centroid training
> and a bounded **shadow-version rebuild** (build partition pages for a new `index_version`, then
> atomically publish it) must not lose canonical mutations that arrive mid-build. The required
> concurrency model is **dual-write to both the active and shadow `index_version` while a build is
> active**, keeping mutation ownership inside the vector canister's mutation boundary so the published
> shadow is consistent at swap time. Quiesce and delta-replay are rejected (operational cost and a
> reintroduced watermark/clock design, respectively). Production k-means / balanced assignment,
> shadow-version rebuild + atomic publish + dual-write, partition tombstone cleanup, a heap centroid
> cache, GQL vector syntax, `VectorSubject::Edge`, and distributed fan-out remain Slice 7+.

## Context

Gleaph already has two vector-related surfaces:

- edge payload vectors (`EdgePayloadEncoding::VectorF32`) used by graph execution while traversing
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
placed in edge payload storage, vertex semantics and traversal-critical edge payload semantics are
mixed.

We need a plan that:

- keeps vertex embeddings canonical on the graph shard;
- keeps edge payload vectors available for edge-local traversal predicates;
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

The existing edge vector path is not an ANN index. It is a graph-executor scan over edge payload
bytes, with SIMD and bounded L2 improvements in favorable cases but still worst-case `O(n * d)`.
That path remains valid for traversal-critical edge-local vector predicates.

## Decision

### 1. Graph owns canonical vertex embeddings

Vertex embeddings are canonical graph state. `VertexEmbeddingStore` lives in the graph canister
facade, not in the vector index canister.

The store is a dedicated stable store rather than an edge payload extension. Slice 1 commits the
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

`EdgePayloadEncoding::VectorF32` remains the representation for edge-local, traversal-critical
vectors. It is appropriate when query execution evaluates a vector predicate while expanding edges.

Vertex embeddings are not stored in edge payloads. They describe a vertex's semantic representation
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
there is a demonstrated need to externalize edge-payload vector search from graph execution.

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
```

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
- balanced partition assignment is required, not an optional optimization;
- partition maintenance tracks `live_len`, `page_count`, and tombstone ratio so cleanup/rebuild can
  be queued before dead slots dominate read cost;
- every search path enforces page/read/instruction budgets before reading vector pages; and
- `flat` baseline canbench results must be kept so `ivf_flat` recall, latency, and cycle costs are
  compared against exact scan at the same dataset size.

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
| Store vertex embeddings as edge payloads | Mixes vertex semantic state with traversal-critical edge-local payload storage. |
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
5. **[partial, slice 2]** Add `graph-vector-index` canister with index-local `VectorId`s, partition-local fixed-width
   `F32` vector pages, tombstones, and attach/detach. (`ivf_flat` search and paginated candidate APIs remain Slice 4+.)
6. Add router target resolution and query seed integration.
7. Add algorithm-neutral GQL/query planning integration and keep `algorithm: "ivf_flat"` in index
   definition/config, not query syntax.
8. Add `ivf_flat` rebuild state machine: sample collection, centroid training, balanced assignment,
   shadow-version publish, and old-version cleanup.
9. Add centroid cache warmup and explicit cache-miss behavior.
10. Add tombstone cleanup/rebuild thresholds for partitions.
11. Add canbench baselines for embedding write, flush, backfill, centroid warmup, `flat`, and
   `ivf_flat` search.

## Required design updates

- `design/index/vector-index.md` records the planned store/index boundary.
- `design/README.md` links the vector index design document.
- `design/adr/README.md` links this ADR.
- When implementation allocates stable regions, update `design/storage/stable-memory-inventory.md`,
  `design/adr/0007-stable-memory-layout.md`, and the typed layout registry in
  `gleaph_graph_kernel::stable_layout`.
