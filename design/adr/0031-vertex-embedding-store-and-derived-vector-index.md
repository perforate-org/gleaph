# 0031. Vertex embedding store and derived vector index canister

Date: 2026-06-23
Status: accepted (partially implemented)
Last revised: 2026-06-23

> **Status note:** The boundary decision is accepted. Slice 1 (the canonical graph-owned
> vertex embedding store) is implemented; the derived vector-index canister, repair/backfill,
> Candid API, and query operators remain planned. This ADR fixes ownership, consistency, the
> standard `ivf_flat` vector-index kind, and the first derived vector-index stable-memory shape
> before the Candid API or query operator is committed.

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

1. Add `design/index/vector-index.md` as the planned design contract.
2. Add shared vector-index wire types to `graph-kernel`.
3. Add a graph-owned `VertexEmbeddingStore` with fixed-dimension `F32` records first.
4. Add vector-index mutation deltas, volatile pending flush, durable repair journal integration, and
   bounded backfill from graph shards.
5. Add `graph-vector-index` canister with index-local `VectorId`s, partition-local fixed-width
   `F32` vector pages, tombstones, `ivf_flat` search, and paginated candidate APIs.
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
