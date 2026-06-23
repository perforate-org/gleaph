# Vector index

Last updated: 2026-06-23
Anchor timestamp: 2026-06-23 06:39:47 UTC +0000

## Status

**Partially implemented** — [ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md)
accepts the ownership boundary: graph shards own canonical vertex embeddings, vector index
canisters own derived candidate-generation structures, and Router owns vector query orchestration.

Slice 1 is implemented: the graph-owned canonical `VertexEmbeddingStore` (fixed-dimension `F32`,
stable region `VERTEX_EMBEDDINGS` / MemoryId 44) plus shared `EmbeddingNameId` / `VectorEncoding`
types in `graph-kernel`.

The derived vector-index canister, repair/backfill path, Candid search APIs, and query operators are
not implemented yet.

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

## Canonical vertex embedding store

**Implemented (slice 1).** The canonical key shape is committed:

```text
(VertexId, EmbeddingNameId) -> EmbeddingRecord
```

The key is vertex-major and big-endian fixed-width (6 bytes) so delete can enumerate all
embeddings owned by one vertex. Backfill-by-embedding-name is deliberately not optimized in the
canonical store; a later derived embedding-name index may be added when vector-index backfill needs
it.

This records the accepted trade-off: `(VertexId, EmbeddingNameId)` favors per-vertex delete
enumeration over whole-embedding-name scans. A future `(EmbeddingNameId, VertexId)` access path may
be added as **derived** state when vector-index backfill needs it, but it must not become a second
canonical store.

Record shape (graph facade `StoredEmbedding`, stable region `VERTEX_EMBEDDINGS`, MemoryId 44):

```text
EmbeddingRecord {
  encoding: F32,
  dims: u16,
  version: u64,   // 1 on insert, +1 per update; 0 reserved = unset / no record
  bytes,          // inline little-endian f32 components, byte width = dims * 4
}
```

The slice supports only fixed-dimension `F32`. `EmbeddingNameId(0)` is reserved and rejected at the
write boundary. Dimension changes on an existing embedding are rejected (`DimensionMismatch`):
re-embedding at a different dimension under the same `EmbeddingNameId` requires remove + insert or a
new embedding name. The stored bytes are a manual, length-prefixed layout led by a `schema_version`
tag; an unknown schema or encoding tag traps on read because an incompatible stable layout requires
a migration. Later encodings such as `F16` or quantized `I8` require explicit design updates because
they affect byte-width validation, scoring, and backfill.

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

- [done, slice 1] Choose vertex embedding key shape and stable region classification.
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
