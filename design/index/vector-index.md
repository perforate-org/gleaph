# Vector index

Last updated: 2026-06-23
Anchor timestamp: 2026-06-23 05:51:03 UTC +0000

## Status

**Planned** — [ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md)
accepts the ownership boundary: graph shards own canonical vertex embeddings, vector index
canisters own derived candidate-generation structures, and Router owns vector query orchestration.

No vector-index stable regions, Candid APIs, or query operators are implemented yet.

## Purpose

Define the planned boundary between:

- the graph-owned vertex embedding store;
- edge payload vectors used during traversal;
- derived vector index canisters; and
- Router vector query coordination.

The goal is to make vector search a graph-native candidate-generation path without turning Gleaph
into a standalone vector database.

## Non-goals

- Committing an IVF, PQ, or HNSW stable-memory layout.
- Defining public GraphRAG syntax.
- Replacing edge payload vectors used by traversal predicates.
- Moving canonical vertex or edge state into an index canister.

## Ownership model

| Layer | Owns | Must not own |
|-------|------|--------------|
| Router | vector index target resolution, auth, planning integration, fan-out, merge, seed construction | canonical vectors, ANN storage internals |
| Graph | canonical vertex embeddings, vertex delete/update semantics, embedding backfill source | ANN posting lists, centroid assignment, cross-canister query merge |
| Vector index canister | derived full-vector copies, Flat/IVF/PQ/HNSW search structures, candidate scoring | final graph results, traversal, property filtering, vertex existence |
| GQL portable crates | generic language and planning structures only | Gleaph/IC-specific vector storage or canister assumptions |

## Vertex embeddings vs edge payload vectors

Vertex embeddings and edge payload vectors are separate concepts.

| Concept | Owner | Use |
|---------|-------|-----|
| Vertex embedding | Graph canister | semantic representation of a vertex; GraphRAG candidate generation; vector-index backfill |
| Edge payload vector | Graph canister / LARA edge payload | traversal-critical edge-local vector predicate during expand |
| Vector index entry | Vector index canister | derived search structure for candidate generation |

`EdgePayloadEncoding::VectorF32` remains valid for edge-local predicates. A vertex embedding store is
planned for vertex semantic embeddings so the graph shard can enforce dimensions, encoding,
versioning, delete behavior, and rebuild/backfill into derived vector indexes.

## Planned canonical vertex embedding store

The exact stable key shape is intentionally deferred. Candidate shapes:

```text
(VertexId, EmbeddingNameId) -> EmbeddingRecord
```

or:

```text
(EmbeddingSlotId, VertexId) -> EmbeddingRecord
```

Minimum record shape:

```text
EmbeddingRecord {
  encoding: F32,
  dims,
  version,
  vector_ref_or_inline_bytes,
}
```

Initial implementation should support only fixed-dimension `F32`. Later encodings such as `F16` or
quantized `I8` require explicit design updates because they affect byte-width validation, scoring,
and backfill.

## Derived vector index model

Vector index canisters return candidates, not final graph rows.

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

Phase 1 should support vertex subjects first. Edge subjects are deferred until there is a concrete
need to externalize edge-payload vector search from graph execution.

## Consistency and rebuild

The vector index follows the graph-index pattern:

1. Graph mutation commits canonical embedding changes on the graph shard.
2. The graph shard emits derived vector-index insert/remove/update deltas.
3. Happy-path flush to vector index is volatile.
4. Failed flush persists to a durable repair path.
5. Bounded backfill can rebuild vector-index entries from the graph-owned embedding store.

Derived vector-index lag follows the same high-level rule as other derived indexes: canonical graph
state wins when derived state disagrees.

## Algorithm roadmap

| Phase | Algorithm | Status | Purpose |
|-------|-----------|--------|---------|
| 1 | Flat exact search | planned | prove storage, repair, backfill, pagination, and query integration |
| 2 | IVF_FLAT | planned | bounded centroid/list candidate generation plus exact rerank |
| 3 | IVF_PQ | planned | compressed approximate scoring plus full-vector rerank |
| 4 | HNSW | experimental planned | only after update/delete/repair and IC instruction bounds are specified |

## Design gates before implementation

- Choose vertex embedding key shape and stable region classification.
- Define vector index wire types in `graph-kernel`.
- Define bounded candidate page/cursor APIs.
- Define mutation delta and repair journal representation.
- Define backfill cursor and delete/tombstone behavior.
- Add canbench targets for write, flush, backfill, and Flat search.

## Related documents

- [ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md)
- [property-index.md](property-index.md)
- [derived-state-query-semantics.md](derived-state-query-semantics.md)
- [capacity-planning.md](capacity-planning.md)
- [../architecture/overview.md](../architecture/overview.md)
- [../storage/labeled-edge-payloads.md](../storage/labeled-edge-payloads.md)
- [../storage/payload-first-traversal.md](../storage/payload-first-traversal.md)
