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

Slice 3 is implemented: the Router-owned vector-index definition catalog (`ROUTER_VECTOR_INDEXES`,
MemoryId 42) and graph-scoped embedding-name catalog (`ROUTER_EMBEDDING_NAME_CATALOG`, MemoryIds
40–41), the admin/query surface, single-target (inspect-only) resolution, and the ephemeral
`ExecutePlanArgs.indexed_embeddings` injection path.

Slice 4 is implemented: the delete-spanning **incarnation fence** (canonical
`VERTEX_EMBEDDING_INCARNATIONS`, graph MemoryId 45) makes the canonical-wins drain production-correct
and **activates production dispatch** behind a Router-owned stable activation flag
(`ROUTER_VECTOR_DISPATCH_ACTIVATION`, MemoryId 43, default off) plus a per-graph readiness gate. See
*Router catalog, target resolution, and the activation gate (Slice 4, implemented)* below.

**Without an installed catalog the graph shard skips vector dispatch entirely** (mirroring
property-index behavior when no catalog is present); the shard never persists an indexed-embedding
registry, and the injected catalog is empty whenever dispatch is not ready.

Slice 5 is implemented: the first production **read path** — an exact top-k `ivf_flat` search. The
vector canister exposes a router-guarded query `vector_search(VectorSearchRequest) ->
VectorSearchResult` that scans the **live subject map** (`VECTOR_SUBJECT_TO_ID`) over the requested
`index_id`, reads each live slot's bytes through a per-query page cache, scores with `L2Squared`, and
returns a bounded, deterministically ordered top-k. An activated index with no embeddings yet has no
physical def (it is created lazily on first upsert), so search returns an **empty** result rather than
an error. The Router exposes `vector_search` as a
`#[query(composite = true)]` that resolves the graph/index to its single activated target and **fails
closed** on the same Slice 4 activation gate (`activation_block_reason`) before forwarding; it also
prevalidates `top_k` / `dims` / query byte length against the registered definition so user mistakes
surface as `InvalidArgument` rather than an opaque internal error. The vector-canister query is
router-guarded so the derived vectors cannot be queried directly around the gate. This slice
introduces **no stable-layout change** (no new region, no `PageRow`/reverse-map
change): correctness and freshness come from the subject map, which is already the source of truth for
which subjects are live and at which slot. The kernel adds `VectorSearchRequest` / `VectorSearchHit`
/ `VectorSearchResult` and `MAX_VECTOR_SEARCH_TOP_K` (1024).

The search is intentionally degenerate IVF: one partition, exact scoring, no pruning. IVF centroid
training, `nprobe` partition pruning, candidate pagination, query ranking/merge, page self-describing
rows / reverse map (for partition scans), PQ/HNSW, and `VectorSubject::Edge` are deferred to Slice 6+.
The standard vector-index kind is `ivf_flat`; `flat` is collapsed into degenerate `ivf_flat` rather
than a separate kind, and later `ivf_pq` or experimental `hnsw` implementations must preserve the same
canonical/derived boundary.

## Version naming glossary

Four distinct concepts; `version` is never overloaded in APIs or idempotence rules:

| Name                   | Owner                 | Meaning                                                                                                                  |
| ---------------------- | --------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `embedding_incarnation`| graph canonical store | Delete-spanning, monotonic per `(VertexId, EmbeddingNameId)` ordering token; strictly increases on each reinsert and is **never deleted** (high-water mark). Stamped on every sync op. |
| `embedding_version`    | graph canonical store | `StoredEmbedding.version` per-incarnation update counter; resets to `1` on reinsert; carried on sync ops and the repair journal |
| `index_version`        | vector-index canister | Physical index generation: `active_index_version` in defs, shadow rebuild target, page / partition-head keys           |
| `generation`           | vector-index canister | Slot / entity handle generation for append-and-tombstone; bumps on each new slot for a subject                          |

The subject map row (`VECTOR_SUBJECT_TO_ID[(index_id, subject)]`) is a **clock that survives
deletion**: a removed subject retains its `(embedding_incarnation, embedding_version)` and
`deleted = true`. The canister orders every write by `(embedding_incarnation, embedding_version)`:

- **Older incarnation (`op.inc < clock.inc`):** stale no-op — neither an upsert nor a remove from a
  prior incarnation can affect a newer one. This closes the reverse-orphan race.
- **Same incarnation (`op.inc == clock.inc`):** ordered by `embedding_version` against the clock for
  a live subject (stale `<` no-op; `==` identical no-op / different conflict; `>` appends a new slot,
  reusing the live `VectorId`); a deleted subject no-ops a same-incarnation upsert.
- **Newer incarnation (`op.inc > clock.inc`):** an upsert **resurrects** with a *fresh* `VectorId`
  (resurrection requires a strictly newer incarnation); a remove records the newer deleted clock.

Stale-replay protection is now enforced by **both** the canister clock (incarnation ordering) and
the **graph repair-drain**, which reconciles vector journal entries against the canonical store
rather than replaying them verbatim (canonical wins). If the subject still owns the embedding the
drain delivers an upsert stamped with the re-derived current `(incarnation, version)`; if it was
deleted the drain delivers a remove stamped with the **persisted incarnation** and
`RECONCILE_TOMBSTONE_VERSION` (the within-incarnation max), which supersedes a live slot of the same
incarnation yet — being incarnation-fenced — cannot tombstone a newer reinsert. A repair entry with
no configured vector client is skipped (left durable) and never wedges the property repairs queued
after it.

`VectorId` is never reused: a reinsert after delete allocates a fresh id from the durable
`next_vector_id` allocator. Remove ops carry an empty `bytes` field and rely on
`(embedding_incarnation, embedding_version)` for idempotence.

### Router catalog, target resolution, and the activation gate (Slice 4, implemented)

Slice 3 made vector-index dispatch **addressable** from the Router; Slice 4 **activates** it behind
the incarnation fence and a two-condition gate (global flag AND per-graph shard attach):

- **Embedding-name catalog (Router-owned, graph-scoped).** `ROUTER_EMBEDDING_NAME_CATALOG`
  (MemoryIds 40–41) interns embedding **names** to `EmbeddingNameId`s. The Router is the sole
  allocator, so the id stored on a definition is exactly the id the graph stamps on canonical
  embedding writes. Registration resolves **by name**; a caller-supplied raw `u16` is never accepted.
- **Vector-index definition catalog.** `ROUTER_VECTOR_INDEXES` (MemoryId 42) maps
  `(graph_id, index_id)` to a versioned `VectorIndexDefRecord { embedding_name_id, kind, metric,
  encoding, dims, target: Option<VectorIndexTarget { canister }>, activation_state }`.
- **Router invariants.** **One vector index per embedding name per graph** (registration rejects a
  second def for the same `embedding_name_id` with `Conflict`, checked before interning the name) and
  **one vector-index target per graph** (every def and every attached shard must share one target
  principal). Both exist because graph dispatch/backfill/repair are single-op, single-route.
- **Activation flag.** `ROUTER_VECTOR_DISPATCH_ACTIVATION` (MemoryId 43) is a Router-owned stable
  boolean, default off, toggled by `admin_set_vector_dispatch_activation` and read by
  `vector_dispatch_activation_enabled` (RBAC `authorize_index_ddl` / admin). It replaces the old
  `const false` fencing predicate and is reversible without a redeploy.
- **The two-condition gate.** `graph_vector_dispatch_ready(graph_id)` is true only when the global
  flag is on **and** every live (index-attached) shard of the graph carries a non-anonymous
  `vector_index_canister` equal to the graph's single target with `vector_index_attached == true`.
  `to_indexed_embedding_catalog(graph_id)` exports `DispatchEnabled` specs **only** when ready;
  otherwise it stays empty (fail-closed), so a global enable can never act on a partially wired graph.
- **Derived activation state.** `VectorIndexActivationState { Registered, DispatchBlocked,
  DispatchEnabled }` is recomputed at read time, so the flag/attach activates existing targeted defs
  with no stored-state migration. The activation-status query distinguishes blocked reasons
  `DispatchNotActivated` (flag off) and `ShardsNotVectorAttached`.
- **Target wiring.** A router-guarded graph endpoint `admin_set_vector_index_canister` writes the
  target into the shard's **local** `FederationRouting` (idempotent, upgrade-durable). The Router
  endpoint `admin_attach_vector_index_shard` drives the attach handshake — write graph-local routing
  first, attach the shard to the vector canister, then flip the registry `vector_index_attached` bit
  only after both succeed — so the registry bit cannot claim readiness while the shard is locally
  `None`. A retrofit path attaches already-registered shards. `ShardRegistryEntry` carries
  `vector_index_canister`/`vector_index_attached` via a `V2` stable envelope (old `V1` bytes decode).
- **Vector-canister ownership is graph-scoped.** Because there is one target per graph, the vector
  canister fixes ownership on `graph_id` alone and accepts **every** shard of that graph (a different
  `graph_id` is rejected with `GraphOwnershipMismatch`). Unlike property indexes — where each index
  canister owns one contiguous shard *group* (`index_group_size` / `group_index`) — the vector attach
  carries no group descriptor; reusing the property group formula would split a multi-shard graph
  into per-shard groups that a single target rejects.
- **Admin/query surface (RBAC via `authorize_index_ddl`).** Register, set target, list, activation
  status / explain-blocked, inspect-only single-target resolution (rejecting anonymous targets), the
  activation flag set/get, and the vector-shard attach endpoint.
- **Bounded backfill.** `admin_vector_index_backfill_step` is a real bounded driver (router
  orchestration → `graph_client::backfill_vertex_embeddings` → graph endpoint → existing worker)
  taking an explicit `(shard_id, start_vertex_id, max_vertices)` resume cursor; it fails closed
  (`VectorDispatchActivationBlocked`) while dispatch is not ready.

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
| 1 | IVF_FLAT (`ivf_flat`) | exact scan implemented (Slice 5); centroid routing / partition pruning / rerank planned (Slice 6+) | standard vector index: centroid routing, partition pages, query-aware pruning, exact rerank |
| 2 | Flat (`flat`) | subsumed by degenerate `ivf_flat` exact scan (Slice 5) | exact scan over all vector pages for small indexes, debugging, and correctness baselines |
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
- [done, slice 5] Add canbench targets for exact `ivf_flat` search (`crates/graph-vector-index`,
  dims 128/384/768 × top_k 10/100). Centroid-warmup / pruned-search benches remain for Slice 6+.

## Related documents

- [ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md)
- [property-index.md](property-index.md)
- [derived-state-query-semantics.md](derived-state-query-semantics.md)
- [capacity-planning.md](capacity-planning.md)
- [../architecture/overview.md](../architecture/overview.md)
- [../storage/labeled-edge-payloads.md](../storage/labeled-edge-payloads.md)
- [../storage/payload-first-traversal.md](../storage/payload-first-traversal.md)
