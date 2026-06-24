# Vector index

Last updated: 2026-06-23
Anchor timestamp: 2026-06-23 22:55:58 UTC +0000

## Status

**Partially implemented** — [ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md)
accepts the ownership boundary: graph shards own canonical vertex embeddings, vector index
canisters own derived candidate-generation structures, and Router owns vector query orchestration.

Slice 1 is implemented: the graph-owned canonical `VertexEmbeddingStore` (fixed-dimension `F32`,
stable region `VERTEX_EMBEDDINGS` / MemoryId 44) plus shared `EmbeddingNameId` / `VectorEncoding`
types in `graph-kernel`.

Slice 2 is implemented: the derived sync path plus a degenerate `ivf_flat` `graph-vector-index`
canister foundation (mutation-only). This covers `graph-kernel` sync/mutation wire types
(`VectorEmbeddingSyncOp`, `VectorSubject::Vertex`, `IndexedEmbeddingCatalog`), the
`VECTOR_INDEX_STABLE_LAYOUT` registry (11 regions, MemoryId 0–10), the `graph-vector-index` canister
storage (`vector_upsert` / `vector_remove`, durable allocators, subject tombstone clock, attach /
detach), and the graph-side delta plumbing (catalog-gated dispatch, `vector_pending`,
`RepairPostingOp::VectorEmbedding`, repair drain, and bounded `vertex_embedding_backfill`).

The degenerate `ivf_flat` foundation runs with `nlist = 1`, `partition_id = 0`, and no centroids;
the `IVF_CENTROIDS` region (MemoryId 6) is reserved-but-empty so Slice 4 needs no stable repack.

**Slice 2 production deployment creates the vector-index canister and the graph
sync/repair/backfill plumbing, but does not produce vector-index entries until the Router supplies an
ephemeral `IndexedEmbeddingCatalog` per operation in Slice 3.** Without an installed catalog the
graph shard skips vector dispatch entirely (mirroring property-index behavior when no catalog is
present); the shard never persists an indexed-embedding registry.

Candid search APIs, the Router registry, and query operators are not implemented yet. The standard
vector-index kind is `ivf_flat`; `flat` is collapsed into degenerate `ivf_flat` rather than a
separate kind, and later `ivf_pq` or experimental `hnsw` implementations must preserve the same
canonical/derived boundary.

## Version naming glossary

Three distinct concepts; `version` is never overloaded in APIs or idempotence rules:

| Name                | Owner                 | Meaning                                                                                                                  |
| ------------------- | --------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `index_version`     | vector-index canister | Physical index generation: `active_index_version` in defs, shadow rebuild target, page / partition-head keys           |
| `embedding_version` | graph canonical store | `StoredEmbedding.version` from the `VertexEmbeddingStore`; carried on sync ops and the repair journal                  |
| `generation`        | vector-index canister | Slot / entity handle generation for append-and-tombstone; bumps on each new slot for a subject                          |

The subject map row (`VECTOR_SUBJECT_TO_ID[(index_id, subject)]`) is a **clock that survives
deletion**: a removed subject retains `stored_embedding_version` and `deleted = true`. Idempotence
is split by subject liveness:

- **Live subject:** ordered by `embedding_version` against the clock (stale `<` no-op; `==`
  identical no-op / different conflict; `>` appends a new slot, reusing the live `VectorId`).
- **Deleted subject:** any delivered `vector_upsert` **resurrects** with a *fresh* `VectorId`,
  regardless of version. The canister cannot order a re-insert against the tombstone clock because
  the canonical `embedding_version` resets to `1` on re-insert, so it trusts the graph to deliver
  only current upserts.

Stale-replay protection lives in the **graph repair-drain**, not the canister clock: vector journal
entries are not replayed verbatim but **reconciled against the canonical store** (canonical wins).
If the subject still owns the embedding the drain delivers a current upsert; if it was deleted the
drain delivers a remove stamped with an **authoritative (maximum) `embedding_version`** so it
supersedes any live slot regardless of the stale op's version, and a stale upsert can never
resurrect a tombstoned vector. (The high clock does not block re-inserts, since upserts to a deleted
subject resurrect regardless of version.) A repair entry with no configured vector client is skipped
(left durable) and never wedges the property repairs queued after it.

`VectorId` is never reused: a re-insert after delete allocates a fresh id from the durable
`next_vector_id` allocator. Remove ops carry an empty `bytes` field and rely only on
`embedding_version` for idempotence.

### Slice 3 activation gate (canonical-wins drain is not yet production-correct)

The canonical-wins repair drain described above is a **Slice 2 scaffold that runs only while vector
dispatch is inert**. Slice 2 production never injects an `IndexedEmbeddingCatalog`, so no vector ops
are ever queued or journaled, and the drain is exercised only by tests. It is **not** a correct
production convergence path, and dispatch must not be activated in production until this gate is
closed:

- `embedding_version` resets to `1` on re-insert, so it is **insufficient as an ordering token**
  across a delete boundary.
- The canonical-deleted reconcile sends a *blind* remove (it reads canonical as absent, then awaits
  the cross-canister remove). A blind remove **cannot avoid both** failure modes: a stale-version
  remove no-ops against a newer live slot (**forward-orphan**: a deleted embedding's vector
  survives), while a `u64::MAX` remove that races a concurrent re-insert's direct-flush upsert
  tombstones a live vector (**reverse-orphan**: a live embedding's vector is deleted). On IC the
  await commit point makes this race reachable, not merely theoretical.
- Once the reconcile entry is dropped from the journal there is no source to re-detect the
  divergence, so "re-reconcile on a later maintenance tick" does not close it.

**Activation requirement:** before Slice 3 injects a production catalog, a delete-spanning
**monotonic incarnation/epoch** carried in the sync op and enforced by the vector canister MUST be
added so removes and re-inserts order independent of arrival order. Until then the canonical-wins
drain is treated as an inert scaffold, and catalog-backed dispatch stays fail-closed. See ADR 0031.

## Purpose

Define the planned boundary between:

- the graph-owned vertex embedding store;
- edge payload vectors used during traversal;
- derived vector index canisters; and
- Router vector query coordination.

The goal is to make vector search a graph-native candidate-generation path without turning Gleaph
into a standalone vector database.

## Non-goals

- Committing PQ or HNSW stable-memory layouts.
- Using CSR as a vector-index stable-memory layout or snapshot format.
- Exposing physical index kinds in GQL query syntax.
- Defining public GraphRAG syntax.
- Replacing edge payload vectors used by traversal predicates.
- Moving canonical vertex or edge state into an index canister.

## Ownership model

| Layer | Owns | Must not own |
|-------|------|--------------|
| Router | vector index target resolution, auth, planning integration, fan-out, merge, seed construction | canonical vectors, ANN storage internals |
| Graph | canonical vertex embeddings, vertex delete/update semantics, embedding backfill source | ANN partitions, centroid assignment, cross-canister query merge |
| Vector index canister | derived full-vector copies, `ivf_flat`/`flat`/future `ivf_pq`/future `hnsw` search structures, candidate scoring | final graph results, traversal, property filtering, vertex existence |
| GQL portable crates | generic language and planning structures only | Gleaph/IC-specific vector storage or canister assumptions |

## Vertex embeddings vs edge payload vectors

Vertex embeddings and edge payload vectors are separate concepts.

| Concept | Owner | Use |
|---------|-------|-----|
| Vertex embedding | Graph canister | semantic representation of a vertex; GraphRAG candidate generation; vector-index backfill |
| Edge payload vector | Graph canister / LARA edge payload | traversal-critical edge-local vector predicate during expand |
| Vector index entry | Vector index canister | derived search structure for candidate generation |

`EdgePayloadEncoding::VectorF32` remains valid for edge-local predicates. The canonical vertex
embedding store exists for vertex semantic embeddings so the graph shard can enforce dimensions,
encoding, versioning, delete behavior, and rebuild/backfill into derived vector indexes.

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

### Derived vector storage

Vector index canisters store derived full-vector copies in partition-local fixed-width vector pages.
`VectorId` is index-local:

```text
(index_id, vector_id) -> one vector slot
```

Placement is resolved through the index definition and slot map:

```text
VECTOR_INDEX_DEFS[index_id] -> { kind: ivf_flat, encoding, dims, metric, active_version, ... }
VECTOR_ID_TO_SLOT[(index_id, vector_id)] -> SlotRef { version, partition_id, page_id, slot, generation }
VECTOR_SUBJECT_TO_ID[(index_id, subject)] -> { vector_id, generation }
```

Each page is both a storage unit and a scoring unit:

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

The first derived store supports `F32` only. The structure is still encoding-aware: different
dimensions or encodings use different indexes or physical page families, mirroring the LabeledLARA
pattern where owner metadata fixes the byte width before reading. Vector ids are not reused in the
first implementation; deletes set tombstone bits so stale slot references remain safe until cleanup.

Updates append a new slot and tombstone the old slot. Search reads selected pages into heap buffers
and performs SIMD exact scoring over page-local contiguous vector bytes.

### IVF stable layout

`ivf_flat` is the standard vector-index kind. It uses SPANN-inspired partition pages: stable-backed
centroids, optional bounded heap centroid cache, balanced partition assignment, query-aware pruning,
and exact full-vector rerank. CSR is intentionally not part of the vector-index stable layout or
future snapshot format:

```text
IVF_CENTROIDS[(index_id, version, partition_id)] -> centroid vector
IVF_CENTROID_META[index_id] -> { active_version, dims, nlist, encoding, metric, centroid_epoch }
IVF_PARTITION_HEADS[(index_id, version, partition_id)] ->
  { first_page, mutable_page, page_count, live_len }
VECTOR_PAGE[(index_id, version, partition_id, page_id)] -> fixed-width vector page
```

The search path scores the query against the heap centroid cache, reads a bounded number of
centroid-selected partition pages, skips tombstoned slots, and performs exact SIMD rerank over the
page-local vector bytes. Balanced partition assignment and query-aware pruning are part of the first
`ivf_flat` contract; closure replication, PQ, and HNSW are later optimizations. If partition-locality
benchmarks fail, the preferred fixes are page sizing, balanced assignment, read-budget pruning,
tombstone cleanup, and encoding-specific page reads, not CSR conversion.

### IC implementation gates

`ivf_flat` is the standard vector-index kind only if the implementation preserves IC execution and
upgrade constraints:

- centroid metadata is authoritative in stable memory; heap centroid cache is derived acceleration;
- cache miss behavior is explicit: either `CacheNotReady` or a bounded stable centroid scan fallback;
- rebuild writes a shadow `version` and publishes by atomically switching `active_version`;
- balanced partition assignment is required before publishing a rebuilt index;
- partition maintenance records `live_len`, `page_count`, and tombstone ratio;
- partitions whose tombstone ratio or page count crosses a threshold enter cleanup/rebuild;
- search enforces page/read/instruction budgets before reading vector pages; and
- canbench compares `ivf_flat` against a `flat` exact-scan baseline at the same dataset size.

### Query syntax and algorithm selection

GQL query syntax should express vector-search intent, not physical index selection. Queries name the
embedding field, query vector, metric-compatible scoring, top-k, thresholds, and rerank needs. They
should not name `ivf_flat`, `ivf_pq`, or `hnsw` directly.

Algorithm choice belongs to index definition or Router/index configuration:

```text
algorithm: "ivf_flat"
metric: "cosine" | "l2"
dims: 1536
encoding: "f32"
```

This keeps future `ivf_pq`, `hnsw`, or `flat` implementations behind the same query semantics.
Query-time knobs, if needed, should be semantic quality or cost hints rather than direct access to
internal index structures.

## Consistency and rebuild

The vector index follows the graph-index pattern:

1. Graph mutation commits canonical embedding changes on the graph shard.
2. The graph shard emits derived vector-index insert/remove/update deltas.
3. Happy-path flush to vector index is volatile.
4. Failed flush persists to a durable repair path.
5. Bounded backfill can rebuild vector-index entries from the graph-owned embedding store.

Derived vector-index lag follows the same high-level rule as other derived indexes: canonical graph
state wins when derived state disagrees.

Rebuild is a bounded maintenance state machine:

```text
CollectSample(cursor)
TrainCentroids(iteration, batch_cursor)
AssignVectors(cursor)
Publish(active_version)
Cleanup(old_version_cursor)
```

The publish step must be metadata-only from the query path's perspective: once `active_version`
changes, searches use the new centroid metadata and partition pages. Old pages are deleted by
bounded cleanup after publication.

## Algorithm roadmap

| Phase | Algorithm | Status | Purpose |
|-------|-----------|--------|---------|
| 1 | IVF_FLAT (`ivf_flat`) | planned | standard vector index: centroid routing, partition pages, query-aware pruning, exact rerank |
| 2 | Flat (`flat`) | planned | exact scan over all vector pages for small indexes, debugging, and correctness baselines |
| 3 | IVF_PQ | planned | compressed approximate scoring plus full-vector rerank |
| 4 | HNSW | experimental planned | only after update/delete/repair and IC instruction bounds are specified |

## Design gates before implementation

- [done, slice 1] Choose vertex embedding key shape and stable region classification.
- [done, slice 1] Define canonical embedding ids and encoding types in `graph-kernel`.
- Define derived vector-index wire types and bounded candidate page/cursor APIs.
- Define `ivf_flat` index-definition metadata and keep algorithm choice out of query syntax.
- Define mutation delta and repair journal representation.
- Define backfill cursor and delete/tombstone behavior.
- Define centroid cache miss behavior.
- Define shadow-version rebuild, balanced assignment, publish, and cleanup.
- Define partition tombstone cleanup thresholds.
- Add canbench targets for write, flush, backfill, centroid warmup, `flat`, and `ivf_flat` search.

## Related documents

- [ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md)
- [property-index.md](property-index.md)
- [derived-state-query-semantics.md](derived-state-query-semantics.md)
- [capacity-planning.md](capacity-planning.md)
- [../architecture/overview.md](../architecture/overview.md)
- [../storage/labeled-edge-payloads.md](../storage/labeled-edge-payloads.md)
- [../storage/payload-first-traversal.md](../storage/payload-first-traversal.md)
