# 0064. Vector canister redesign: ownership, layout, fencing, and ingest

Date: 2026-08-07
Status: Partially Implemented (design contract; Router direct-ingestion durability slice implemented)
Last revised: 2026-08-21 (EncodingRecord pad-stride widths: `align16(component_bytes × dims)`)

> **Summary.** Replaces the ADR 0031/0032/0033 vector-index design (Slices 1–10, **completely
> discarded**) with a fresh ownership model and layout: the vector canister owns the only durable copy
> of embedding bytes (the graph holds zero embedding state), ordering uses Router-issued nonzero
> `mutation_id` stamps comparable with Graph DML removes, presence is determined by label sets on
> index definitions, ingest is stamp-separated
> so bytes cross the subnet once, and the physical page store is a new two-table format (layout version
> 1, a breaking change). The current Router direct-ingestion path collects the successful Graph
> stamps, synchronously revalidates the live lifecycle and immutable definition, and persists each
> stamped operation to a Router-owned durable suffix before its first Vector await; the physical layer is split
> into a domain-free crate
> `ic-stable-vector-page-store`. The active design contract is `design/index/vector-index.md`.

## Context

Gleaph's vector search needs a durable home for embedding bytes and a search structure that is
mutable, deterministic, instruction-bounded, and capable of scaling across canisters. The previous
design (ADR 0031/0032/0033) placed canonical embedding bytes on graph shards and made the vector
canister a derived candidate generator, fenced by `(incarnation, version)`.

Three facts drove the redesign:

1. **Capacity.** At `d = 1536` (the design target), an `F32` embedding is 6,144 bytes/vertex. At
   10⁸ subjects the vector slab alone exceeds one canister's 500 GiB; storing a second full copy on
   the graph (the old canonical store) made the graph canister — already the fastest-growing canister
   — pay the same bytes again, plus relay amplification on every ingest.
2. **Placement mismatch.** Embedding bytes are consumed only by the vector canister, and their search
   placement is by vector-space cluster, not by graph shard. A per-shard canonical store is the wrong
   home.
3. **Research.** Industry graph+vector systems (Neo4j, Memgraph, Kuzu, TigerGraph, FalkorDB, Dgraph,
   pgvector) store embeddings as node data with label-scoped indexes and hash-stable routing; SPFresh
   (SOSP 2023) shows cluster-partitioned indexes stay mutable via boundary-only reassignment; HARMONY
   (ACM MOD 2025) shows dimension-split partitioning is communication-expensive but its early-stop
   pruning transfers.

## Problem

The old design's costs:

- the graph canister held a full second copy of every embedding byte and relayed every ingest
  payload (subnet byte volume ×2, graph stable writes, and graph-side instruction cost for bytes the
  graph never queries);
- the per-identity `(incarnation, version)` fence required a per-vertex embedding registry on the
  graph (more graph-side vector state);
- backfill-from-graph was the only recovery path, but the bytes lived in the wrong canister;
- the physical layer (page chains in a global tail slab) had grown ad hoc, and the design had not
  settled on encodings, row metadata, or a crate boundary.

## Existing architecture assessment

**Kept:**

- canonical/derived separation (graph data canonical; search structures derived);
- Router ownership of definitions, activation, orchestration, and (future) fan-out/merge;
- the shadow-version rebuild with dual-write and atomic publish as the only compaction;
- the search freshness contract (subject revalidation, no mid-scan truncation, deterministic top-k);
- filtered search via the ADR 0034 candidate allowlist.

**Discarded:**

- the graph canonical embedding store (`VERTEX_EMBEDDINGS`, `VERTEX_EMBEDDING_INCARNATIONS`);
- the `(incarnation, version)` fence and per-vertex graph registry;
- byte-relaying ingest and the graph's durable byte outbox role;
- the ADR 0032 page layout and the Slice 1–10 stable layout (breaking; dev data wiped).

## Decision

### 1. Vector canister owns embedding bytes; graph holds zero embedding state

The vector canister owns the **only durable copy** of embedding bytes plus the subject clock and
search structures. The graph owns canonical graph data (the semantic source) and **no vector-domain
state**: no bytes, no registry, no per-identity clock. Recovery is self-rebuild (partition rebuild
from the canister's own rows) plus client re-ingestion for full loss. This decision relieves the
graph canister (the shared capacity bottleneck) and removes relay amplification; it does **not** by
itself remove the vector canister's own 500 GiB wall at `d = 1536` (~616 GB at 10⁸ subjects) — that
is addressed by encoding compression (I8/Binary) and future multi-canister placement (§11).

### 2. Renaming

The role _vector index canister_ is renamed **vector canister** — it owns vector _data_, not just an
index. Type names (`VectorIndexStore`, `VectorIndexLookup`, `vector_index_canister`) are renamed in a
dedicated mechanical slice; the crate name `graph-vector-index` is likewise renamed (e.g.
`vector-canister`) in the same slice.

### 3. Embeddings are derived encodings; stored precision = search precision

An embedding is a derived encoding of graph data through an **external** model; the graph cannot
re-derive it. Therefore the stored encoding is the search precision: non-`F32` indexes are
approximate by contract, documented per definition. Quantized indexes use quantized centroids with
SPANN's representative-point replacement.

### 4. Label sets on definitions; two-layer presence

`VectorIndexDefRecord` gains `labels: Vec<LabelId>`, creation-fixed (change = drop + recreate). The
injected `IndexedEmbeddingCatalog` carries each spec's label set plus a derived `label → [index_id]`
projection. The graph determines notification locally from its canonical label state (**upper
bound**); the vector canister's subject map is the **exact** authority. The graph may over-notify;
remove-on-missing-row writes a deleted subject clock without creating a live row. Ingest validates
label membership. Label loss removes the embedding (IC deviation from Memgraph's move-back,
documented).

### 5. Mutation-ordering fence and conservative tombstone retention

- Every op carries a nonzero stamp allocated by the Router's durable mutation counter. The vector
  canister keeps a per-`(index_id, subject)` clock: an older stamp is a no-op; a same-stamp upsert
  is idempotent only when its canonical payload matches and rejects a different payload; a remove
  on a missing row writes a deleted subject clock. The stamp remains comparable with Graph DML
  stamps where those operations emit vector removes.
- Router validates each public embedding's dimension and finiteness before allocating its stamp or
  making the Graph call. `stamp_embedding` then validates the Graph vertex/tombstone, required
  labels, and payload-independent embedding metadata/encoding, returning the supplied stamp without
  an accompanying DML mutation, canonical write, mutation-journal entry, derived-index outbox row,
  or watermark advance. The Router owns the direct-ingest durable suffix so this validation-only
  stamp cannot be mistaken for a completed Vector projection.
- The subject map is the only store that would otherwise grow monotonically (deleted clocks).
  The attached Graph shard advances `graph_watermark` only for its contiguous applied prefix.
  Router direct-ingestion delivery does not advance `router_watermark`: the current path has no
  contiguous Router-owned acknowledgement frontier, so the production Router watermark remains
  zero. Tombstone deletion is conservatively paused in production (`min(graph_watermark,
  router_watermark) == 0`) pending a future contiguous frontier contract; a deleted entry is not
  treated as removable merely because a Router call returned. No current subject-map growth bound
  is claimed while that frontier is absent.

### 6. Stamp-separated ingest

```text
Router ── metadata + Router-issued stamp ─▶ Graph  (validate existence/labels/encoding; return stamp)
Graph  ── VertexEmbeddingStamp {mutation_id} ────────────────────────────────────▶ Router
Router ── Pending {mutation_id, exact target, exact op} ──────────────────────────▶ stable outbox
Router ── exact pending prefix ────────────────────────────────────────────────────▶ Vector
```

Bytes cross the subnet **once**, never through the graph; the graph holds no bytes even transiently.
`stamp_embedding` is validation-only in this path: Router has already checked value dimension and
finiteness, while Graph checks vertex/tombstone state, required labels, and payload-independent
embedding metadata/encoding. Graph returns the Router-issued stamp without a Graph journal,
derived-index outbox, or watermark write. After **all successful Graph stamps for the request** are collected, the Router synchronously
revalidates every operation against the current live shard row, exact attached target, and current
immutable definition, then appends the exact `(mutation_id, vector target, VectorEmbeddingSyncOp)`
rows to `ROUTER_VECTOR_INGEST_OUTBOX` (MemoryId 53) in one owner operation **before the first
Vector `vector_sync_batch_outcome` await**. No await can interleave this lifecycle revalidation and
append. This stable outbox is the durable owner of every suffix that may be returned as
`DeferredForRepair`; it pins the target and operation bytes rather than re-resolving them from
mutable catalog state. Two calls is the minimum (the fence requires comparable ingest/DML stamps).
The graph's vector surface is `stamp_embedding` plus DML-driven removes (vertex delete, label loss),
re-derivable from graph state.

The outbox is bounded to `MAX_VECTOR_INGEST_OUTBOX_ROWS = 1024` rows. Each encoded row is
prevalidated against `MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES` (2 MiB); capacity, duplicate
identity, and row-encoding failures are checked before the first stable insertion. The persisted
value uses `StorableBound::Unbounded`, because this transport/admission ceiling must not determine
the `StableBTreeMap` node page size; `VectorIngestOutboxState::encode_checked` remains the single
write-boundary size check. Every persisted row is `Pending`; terminal outcomes leave the failed row
and suffix in that same durable state.

Only the typed `vector_sync_batch_outcome` endpoint is supported for batch synchronization. Its
response is an exact-prefix acknowledgement. `Progress { applied }` removes only the first `applied`
submitted rows; the unacknowledged suffix remains `Pending`. `Terminal { applied,
failed_index: applied, error }` removes that prefix, while the failed row and every later row remain
`Pending` in the durable suffix. The Router reports that retained terminal/suffix work as
`DeferredForRepair`; it returns no separate outer error. A
transport/unavailable or malformed response leaves every submitted row pending. The typed Vector
driver processes internal chunks of at most **32 rows**; the Router independently fits each complete
Candid request below the 2 MiB ceiling. This is an at-least-once retry/convergence contract without
a finite-time convergence guarantee or client-level exactly-once semantics.
Vector's per-subject stamp makes replay idempotent when a successful Vector write is followed by
response loss.

Pending rows are retried by the Router's bounded recovery timer in groups keyed by their persisted
Vector target. The direct-ingestion scan examines at most 16 rows per tick and retains the exact
target and operation bytes; it never re-resolves a target from mutable catalog state. Appending a
row calls `recovery::arm_if_needed()`. If an append occurs while a recovery pass is awaiting a
remote call, the arm request is latched and the tick re-arms the floor delay even when that pass
reported no work, preventing a lost wake. Router `init` and `post_upgrade` re-arm the same recovery
lane because timers do not survive an upgrade. Each Vector batch measures the complete Candid
request and adaptively fits a prefix below the 2 MiB hard ceiling, targeting the ceiling minus the
fixed 500 KiB envelope headroom. Focused Router unit tests cover reopen, exact-prefix transitions,
lost-wake re-arming, immutable target retention, and adaptive sizing. The targeted PocketIC lifecycle
gate passed with one test and four filtered:
`cargo test -p gleaph-pocket-ic-tests --test adr0031_vertex_embedding_ingestion unavailable_vector_owner_rebinds_graph_and_router_direct_ingestion_outboxes -- --nocapture`
(1 passed, 0 failed, 4 filtered, 17.95s). It manually drives the Graph and Router timer/recovery
seams and covers Router upgrade, Vector reopen/rebind, exact GQL search, and idempotent replay. This
targeted evidence does not prove autonomous wall-clock timer firing or deferred watermark/tombstone
GC completion.

### 7. Fresh physical layout (two-table pages; run table; packed payload; version 1, breaking)

Layout lineage restarts at version 1: slab/page headers use a 3-byte magic (`VSL`/`VPG`) plus a
binary `u8` version byte `1`; the discarded ASCII magic (`VSL1`/`VPG1`) is rejected fail-closed.
Dev stable data is wiped; the current cutover requires a fresh reinstall only, with no compatibility
reader, migration, or backward-compatible endpoint.

```text
[PageHeader] [run_table × run_capacity] [row_meta × capacity] [vector_bytes × capacity]
```

- **Run table** shares the shard across contiguous rows (rows store only the 30-bit vertex id).
  Runs are created at the page tail; `run_capacity = min(owned_shards, 64)`.
- **row_meta** unifies identity + aux into one table (one metadata span read per page):
  `VertexPayload` (30-bit vertex id; bit 30 reserved; bit 31 tombstone, mirroring `VertexRef`) plus
  encoding-dependent aux (0 | 4 | 8 bytes; `meta_stride ∈ {4, 8, 12}`).
- **vector_bytes** is 16-byte aligned for SIMD.
- **Tail-only, write-once positions** → positional validation (subject map slot matches the scanned
  position); no per-row stamp/vector_id/generation.
- Memory manager: `ic-stable-variable-memory-manager` with per-region bucket policies.

### 8. EncodingRecord (multi-encoding; kernels; binary cosine)

`EncodingRecord { encoding, dims, stride_bytes, pad_stride_bytes, aux_bytes, binary_convention,
kernel }` is a first-class concept (the LARA `label → width` generalization, owned by the vector
canister). One index = one encoding = one stride. Widths derive from `(encoding, dims)`:
`stride_bytes = component_bytes × dims` and `pad_stride_bytes = align16(stride_bytes)`, the 16-byte
SIMD row alignment the page store stores as `row_stride`. Scoring materializes once per page read into the
f32 scratch (except binary, scored via `i64.popcnt`). Binary cosine: `Bits01` → `n11·rq·rv`; `Signs`
→ `1 − 2H/d` (cosine ≡ Hamming order). Aux defaults: `F32` 0 bytes (sub-square + early exit, or
normalized dot), `I8` 4 bytes (scale, mandatory), binary 0 bytes; row-level pruning is an opt-in
`+4/+8` bytes.

### 9. Search algorithm (ivf_flat kind; SPANN blueprint; ε₂; sub-square + early exit)

`ivf_flat` is the kind (category); SPANN is the blueprint (a high-performance instance of the same
family). Fixed `nprobe` is replaced by query-aware pruning (`dist(q, c_p) <= (1 + eps_query) *
dist(q, c_best)`). `L2` scoring defaults to sub-square SIMD with dimension-blocked early exit
(prewarm the k-th best, then stop each row's SIMD once its partial distance exceeds the threshold);
`dot + norms` remains an opt-in alternative. The search contract (subject revalidation, no mid-scan
truncation, deterministic top-k, ADR 0034 allowlist) is unchanged. The partition structure is
**level-generic from the start** (flat is the degenerate `levels = 1` case; a leaf `partition_id`
packs its path; per-level `nlist`/`eps_query`; per-subtree training jobs), so the 8 MiB envelope
becomes a per-training-job bound — see `design/index/vector-index.md`.

### 10. Compaction = rebuild only

No free-span allocator, no PMA rebalance, no routine extent compaction. Tail-only allocation grows
the slab; the Slice 9/10 tombstone-ratio policy bounds it (`slab <= live/(1 − r)`).

### 11. Multi-canister readiness

Single canister today; two future axes are distinguished. **A. Shard-group split** (near-term future;
mirrors ADR 0019; industry standard): per-canister IVF, Router fan-out + merge, rebuild stays
canister-local; the run table's shard sharing becomes most effective. **B. Partition-based split**
(future ADR; SPANN §7): global centroids, partition ownership, query-aware dispatch; requires global
training, cross-canister rebuild, and a routing story (SPFresh/LIRE boundary-only reassignment makes
the subject→canister routing index mostly static). Prepared now: Router resolution returns a target
list with a global deterministic top-k merge contract; `VectorIndexOwnershipConfig` gains
`index_group_size`/`group_index` (Option). No row-copy migration endpoint is implemented or
supported by this cutover: the current vector layout and target assignment require a fresh reinstall,
and any future split needs a separate ADR.

### 12. Crate boundary: `ic-stable-vector-page-store`

The physical layer is split into a domain-free crate (the LARA precedent: a single-user, low-level
storage crate). The crate owns physical facts only: raw slab, page/run/row mechanics, header
validation, distance kernels. It must not know subject-map clocks, partitions/centroids, labels,
rebuild, or search semantics (strides and aux bytes arrive as parameters). The canister keeps the
domain layers. ADR 0032's objection to a premature _generic_ crate does not apply: the page format
is now a settled spec, and the LARA precedent accepts single-user low-level crates. The structure
does not yet receive a proper name (unlike LARA): its components are known techniques composed for IC
constraints — naming is revisited after the implementation proves distinctive properties.

## Consequences

### Positive

- Graph canister embedding bytes: zero (persistent and transient); ingest subnet bytes halved.
- One durable byte copy; self-rebuild; no graph backfill path.
- Deterministic, mutable ordering via `mutation_id`; subject-map tombstone deletion remains paused
  until a contiguous Router watermark frontier exists, so no current growth bound is claimed.
- Label-scoped definitions match industry practice and give cheap graph-side presence determination.
- Fresh layout is a clean break (version 1) with fail-closed rejection of old data.
- Multi-encoding (f16/int8/binary) gives the storage levers needed at `d = 1536`.
- Physical layer is unit-testable and canbench-able in isolation.

### Negative / costs

- Recovery on total vector-canister loss requires client re-ingestion (deviation from the
  graph-backed recovery the old design had).
- Breaking change: the Slices 1–10 implementation and layout are discarded.
- Per-training-job `nlist` bound at `d = 1536` (≈677 per job under the 8 MiB envelope); the
  level-generic structure makes it a per-level bound, restoring trainable counts (raw candidate
  region remains the documented fallback).
- Label loss removes embeddings (IC deviation from the industry "data outlives labels" model).
- Crate split adds a workspace/CI dependency edge; the boundary API must be designed carefully.

## Alternatives considered

| Alternative                                                           | Why rejected                                                                                                                                       |
| --------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| Keep the graph canonical byte store (old design)                      | Capacity (2nd full copy in the fastest-growing canister), relay amplification, placement mismatch                                                  |
| Dedicated embedding-store canister (bytes in a third canister)        | Extra canister role, provisioning, and a graph round-trip per write; deferred as an option if independent byte-tier destruction is ever required   |
| Partition-based split now (option B)                                  | Global training, cross-canister rebuild, and derived routing are structural costs; deferred to a future ADR grounded in SPFresh/LIRE               |
| LARA-ize the page store (PMA segments + free-span + compaction tiers) | Over-engineered for vectors: no middle insertion, no order preservation, rebuild is the natural compaction                                         |
| Per-row stamp/vector_id/generation in rows                            | Positional validation (write-once positions + atomic co-update) makes them redundant                                                               |
| Name the crate `ic-stable-ivf` / a proper name like LARA              | The structure is a composition of known techniques, not a new structure; naming is deferred until the implementation proves distinctive properties |

## Implementation plan

1. **[planned]** Rename the role to vector canister (mechanical slice).
2. **[planned]** Create `ic-stable-vector-page-store` with the two-table page format, run table,
   `VertexPayload`, header validation, and distance kernels; unit tests + canbench in isolation.
3. **[partial]** Rework the vector canister: the subject map clock and typed outcome path are
   implemented; the attached Graph may advance `graph_watermark`, while Router watermark remains
   zero and tombstone deletion stays paused. The physical layout and search/encoding roadmap remain
   planned; graph embedding regions are removed.
4. **[partial]** Rework the graph: `stamp_embedding`, label-diff removes, remove embedding regions;
   the direct-ingest stamp is validation-only and does not update the existing DML mutation
   consumers.
5. **[partial]** Rework the Router: label sets, target-list resolution, global merge contract,
   immutable definition/shard target assignment, and the durable direct-ingestion suffix are
   implemented. The current cutover has no row-copy migration or compatibility endpoint; a future
   split requires a separate ADR and fresh installation.
6. **[planned]** Measure (canbench): scoring formulations, page scan cost at `d = 1536`, per-level
   `nlist` envelope; decide hierarchy deployment (`levels = 2`) and raw candidate region.

## Required design updates

- `design/index/vector-index.md` — rewritten as the active design contract (**done**).
- `design/adr/0031`, `0032`, `0033` — marked superseded by this ADR.
- `design/adr/README.md` — link this ADR.
- `design/storage/stable-memory-inventory.md`, `design/adr/0007-stable-memory-layout.md`, and
  `gleaph_graph_kernel::stable_layout` — updated for the Router direct-ingestion outbox at
  MemoryId 53; the remaining redesign layout work is still ahead of implementation.

## Supersedes

- [ADR 0031](0031-vertex-embedding-store-and-derived-vector-index.md)
- [ADR 0032](0032-vector-index-slab-page-store.md)
- [ADR 0033](0033-vector-rebuild-state-read-memoization.md)
