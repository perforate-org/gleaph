# 0031. Vertex embedding store and derived vector index canister

Date: 2026-06-23
Status: accepted (planned)
Last revised: 2026-06-23

> **Status note:** The boundary decision is accepted. Implementation is planned.
> This ADR fixes ownership and consistency contracts before any stable layout,
> Candid API, or query operator is committed.

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
- leaves room for Flat, IVF, PQ, and later HNSW without committing the first stable layout to one
  ANN algorithm.

## Existing architecture assessment

Property index and label index maintenance already provide the right architectural precedent:

- graph shards hold canonical values;
- index canisters hold derived lookup state;
- router resolves index lookup targets from live registry state;
- graph shards enqueue derived index ops after canonical mutation;
- failed index flushes converge through durable repair/backfill paths; and
- posting keys do not embed graph-wide routing policy that belongs to the router.

The current edge vector path is not an ANN index. It is a graph-executor scan over edge payload
bytes, with SIMD and bounded L2 improvements in favorable cases but still worst-case `O(n * d)`.
That path remains valid for traversal-critical edge-local vector predicates.

## Decision

### 1. Graph owns canonical vertex embeddings

Vertex embeddings are canonical graph state. A future `VertexEmbeddingStore` will live in the graph
canister facade, not in the vector index canister.

The store is planned as a dedicated stable store rather than an edge payload extension. The exact
key shape is not fixed by this ADR; candidates include:

```text
(VertexId, EmbeddingNameId) -> EmbeddingRecord
```

or:

```text
(EmbeddingSlotId, VertexId) -> EmbeddingRecord
```

The design document will choose the concrete layout when implementation starts.

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

### 4. Router owns vector query orchestration

Router resolves vector-index lookup targets from the graph registry and runtime config, just as it
does for property indexes. Router owns authorization, query planning integration, fan-out, merge,
and seed construction.

Graph shards execute final filtering, traversal, materialization, and reranking that requires
canonical graph state. The vector index canister must not call graph shards on the query hot path.

### 5. Phase 1 is exact Flat search

The first vector index implementation uses exact Flat search over derived full-vector storage.
This deliberately prioritizes:

- stable layout correctness;
- dimension and encoding contracts;
- bounded page/cursor APIs;
- graph mutation and repair convergence;
- backfill;
- query integration; and
- benchmark baselines.

Flat search may use SIMD or bounded-distance pruning where profitable, but it must be described
honestly as exact scan, not ANN.

### 6. IVF/PQ/HNSW are later phases

The algorithm roadmap is:

1. **Flat** — exact full-vector scan, candidate pages, stable repair/backfill.
2. **IVF_FLAT** — centroids, list assignment, bounded `nprobe`, exact rerank.
3. **IVF_PQ** — codebooks, PQ codes, approximate scoring, full-vector rerank.
4. **HNSW experimental** — only after stable-memory update/delete, bounded instruction, and repair
   behavior are specified.

Stable layout additions for IVF/PQ/HNSW require a later design update and, if the layout or query
contract changes materially, a follow-up ADR.

## Consequences

### Positive

- Graph remains the source of truth for vertex embeddings.
- Vector indexes are rebuildable derived state, matching the property/label index model.
- Edge payload vectors keep their traversal-focused role.
- Router remains the only owner of cross-canister vector query orchestration.
- Initial implementation can validate stable storage, recovery, and query contracts before taking
  on ANN-specific complexity.

### Negative / costs

- A dedicated vertex embedding store adds a new graph stable domain.
- Graph mutations must maintain another derived-index delta stream.
- Router needs vector-specific target resolution and seed/merge integration.
- Flat search is not enough for large production-scale ANN performance; it is an architectural
  foundation, not the final performance target.

## Alternatives considered

| Alternative | Why rejected |
|-------------|--------------|
| Store canonical vectors only in vector index canisters | Makes the index canister authoritative and prevents graph-owned rebuild/backfill. |
| Store vertex embeddings as edge payloads | Mixes vertex semantic state with traversal-critical edge-local payload storage. |
| Store embeddings only as ordinary vertex properties | Does not give embedding-specific dimension, encoding, update, and backfill invariants a clear owner. |
| Start with IVF or HNSW | Commits ANN-specific stable structures before the canonical/derived boundary and repair model are proven. |
| Let graph shards call vector index during query execution | Moves query orchestration away from Router and conflicts with the existing federation target. |

## Implementation plan

1. Add `design/index/vector-index.md` as the planned design contract.
2. Add shared vector-index wire types to `graph-kernel`.
3. Add a graph-owned `VertexEmbeddingStore` with fixed-dimension `F32` records first.
4. Add vector-index mutation deltas, volatile pending flush, durable repair journal integration, and
   bounded backfill from graph shards.
5. Add `graph-vector-index` canister with Flat exact search and paginated candidate APIs.
6. Add router target resolution and query seed integration.
7. Add canbench baselines for embedding write, flush, backfill, and Flat search.
8. Consider IVF_FLAT only after Phase 1 correctness and benchmark gates pass.

## Required design updates

- `design/index/vector-index.md` records the planned store/index boundary.
- `design/README.md` links the vector index design document.
- `design/adr/README.md` links this ADR.
- When implementation allocates stable regions, update `design/storage/stable-memory-inventory.md`,
  `design/adr/0007-stable-memory-layout.md`, and the typed layout registry in
  `gleaph_graph_kernel::stable_layout`.
