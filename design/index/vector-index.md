# Vector index

Last updated: 2026-08-07
Anchor timestamp: 2026-08-07 23:39:30 UTC +0000

## Status

**Deprecated → Planned (breaking redesign).** The ADR 0031/0032/0033 design and its Slices 1–10
implementation are **deprecated / completely discarded**; the new design below is **Planned** — it is
the active design contract and is intentionally ahead of implementation (none of it is implemented
yet). The stable-memory layout lineage restarts at version 1: the new slab/page headers use a 3-byte
magic (`VSL` / `VPG`) plus a **binary `u8` version byte `1`**. The discarded format's ASCII magic
(`VSL1` / `VPG1`, 4th byte `0x31`) no longer matches (4th byte `0x01`), so old-format data is
rejected fail-closed. This is a breaking change: dev stable data is wiped, and there is no
compatibility reader, migration path, or canonical backfill for the old layout. See _Superseded
implementation (Slices 1–10)_ below and the ADRs for the discarded design.

The redesign keeps the canonical/derived separation but changes four boundaries:

| Boundary               | Old (discarded)                                      | New                                                                                                        |
| ---------------------- | ---------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| Embedding bytes        | graph canister canonical store (`VERTEX_EMBEDDINGS`) | **vector canister owns the only durable copy**; graph holds zero embedding state                           |
| Ordering fence         | `(embedding_incarnation, embedding_version)`         | **graph per-shard `mutation_id`**                                                                          |
| Presence determination | per-vertex canonical store enumeration               | **label sets on index definitions** + injected label→index mapping                                         |
| Ingest path            | Router → Graph (bytes) → Vector (bytes)              | **stamp separation**: Router→Graph (metadata + stamp), Router→Vector (bytes) — bytes cross the subnet once |

The canister role is renamed from _vector index canister_ to **vector canister**: it owns the embedding
bytes (a derived encoding of graph data), the per-subject clock, and the search structures.

## Purpose

Define the boundary between:

- the graph-owned canonical data that embeddings are derived from;
- the vector canister's derived embedding bytes and search structures;
- the Router's definitions, routing, and orchestration.

The goal is to make vector search a graph-native candidate-generation path without turning Gleaph into
a standalone vector database, while keeping the vector canister's data volume and per-query instruction
cost bounded on the Internet Computer.

## Non-goals

- Committing PQ or HNSW stable-memory layouts (deferred; see _Algorithm roadmap_).
- Exposing physical index kinds or training knobs in GQL query syntax.
- Mixing multiple encodings or dimensions inside one index (one index = one model = one stride).
- Cross-encoding search in one index (merge across indexes at the Router if ever needed).
- Retaining the discarded Slice 1–10 implementation or its layout in any form.

## Design principles

1. **Growth-rate data placement.** Stores that grow fast hold the minimum possible bytes; per-row
   interpretation rules, constants, and progress state live in slow-growing stores (definitions,
   EncodingRecord, per-shard watermarks).
2. **Measurement-driven.** Every optimization (scoring formulation, pruning, page sizing, encoding)
   is adopted only after canbench isolates its instruction and storage effect against a baseline.
3. **Simplest structure that meets the budget.** No PMA/free-span/compaction tiers, no maintained
   reverse maps, no row moves: append-only positions and rebuild-as-compaction.
4. **Stored precision = search precision.** The encoding is the data; non-`F32` indexes are
   approximate by contract, documented per definition.

## Ownership model

| Layer           | Owns                                                                                                                                                                           | Must not own                                                       |
| --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------ |
| Router          | vector index definitions (incl. label sets), embedding-name catalog, activation/readiness, target resolution (list-capable), fan-out + deterministic merge, maintenance policy | embedding bytes, per-subject state, partition internals            |
| Graph           | canonical graph data (the semantic source of embeddings), vertex existence, label membership, per-shard `mutation_id` sequence                                                 | embedding bytes (never, not even transiently), vector-domain state |
| Vector canister | embedding bytes (only durable copy), per-(index, subject) clock, partitions, centroids, rebuild/search                                                                         | final graph rows, traversal, property filtering, vertex existence  |

## Embeddings are derived encodings

An embedding vector is a **derived encoding** of graph data through an external embedding model. The
graph cannot re-derive it (the model is external), so:

- the graph's canonical data is the _semantic_ source, never the _byte_ source;
- the vector canister's copy is the only durable byte copy;
- partition rebuild is self-serve (from the vector canister's own rows); full loss is recovered by
  client re-ingestion (documented recovery contract, same as any standalone vector store).

## Router catalog

Definitions (`ROUTER_VECTOR_INDEXES`) carry a **label set**, creation-fixed:

```text
VectorIndexDefRecord {
  index_id, embedding_name_id,
  labels: Vec<LabelId>,          // creation-fixed; change = drop + recreate
  kind, metric, encoding, dims,
  target: Option<VectorIndexTarget>,
  activation_state,
}
```

- One index per (embedding name × label set) per graph; label set of size 1 covers the single-label
  case.
- The injected `IndexedEmbeddingCatalog` carries each spec's label set plus a derived
  `label → [index_id]` projection (ephemeral), so the graph can determine locally whether a vertex's
  change/delete needs vector notification.
- **Presence is two-layered**: label membership is the graph-side _upper bound_ (could have a
  sidecar); the vector canister's subject map is the _exact_ authority. The graph may over-notify;
  remove-on-missing-row is a safe no-op.

## Mutation flow

### Ingest (stamp separation)

```text
Router ── VertexEmbeddingStampRequest {local_vertex_id, embedding_name_id, dims} ──▶ Graph
Graph  ── VertexEmbeddingStamp {mutation_id} ────────────────────────────────────────▶ Router
Router ── VectorEmbeddingSyncOp {index_id, subject, stamp, encoding, dims, metric, bytes} ─▶ Vector
```

- The graph validates vertex existence, label membership against the injected mapping, and
  dims/encoding, then **consumes one per-shard `mutation_id`** (the same sequence DML mutations use)
  and returns it. It never sees or persists the bytes.
- The Router holds the op durably in its request records and delivers bytes + stamp directly to the
  vector canister. Bytes cross the subnet exactly once; the graph's durable outbox never carries
  embedding bytes.
- Two calls is the minimum: the fence requires ingest stamps and DML stamps to be comparable, and the
  graph is the only authority that sees both.

### DML-driven removes

- **Vertex delete**: the graph captures the vertex's labels before deletion, intersects them with the
  injected mapping, and dispatches remove ops (metadata only) with the deletion's `mutation_id`.
- **Label loss**: the label-mutation path diffs old/new label sets against the mapping and dispatches
  removes for indexes the vertex fell out of. Embedding data does not outlive label membership on IC
  (deviation from Memgraph's move-back, documented).
- Failed remove delivery is re-derived from graph state (vertex gone / label gone); no byte replay.

### Wire op

```text
VectorEmbeddingSyncOp { index_id, subject, stamp: mutation_id, encoding, dims, metric, bytes, remove }
```

`embedding_name_id` is dropped (index_id is 1:1 per graph). Remove ops carry empty bytes.

## Ordering fence and watermark GC

### Mutation-ordering fence

Every op carries the graph's per-shard `mutation_id`; the vector canister keeps a per-`(index_id,
subject)` clock `current_stamp`:

- `op.stamp <= current_stamp` → no-op (replay/retry idempotence);
- `op.stamp > current_stamp` → apply: upsert appends a row and advances the clock; remove marks
  deleted (row retained as a deleted clock) and advances the clock;
- remove on a missing row → no-op (no row created). Safe: a later upsert sets the clock, and any
  stale replay compares against it.

The graph's per-shard mutation sequence plays the role of a monolithic DB's transaction log; the
fence exists only because delivery is async and at-least-once across the graph/vector boundary.

### Watermark GC (bounds the subject map)

The subject map is the only store that would otherwise grow monotonically with cumulative
ever-ingested subjects (deleted clocks are retained for the fence). Two per-shard watermarks bound it:

```text
shard → { graph_watermark, router_watermark }   // piggybacked on existing calls, O(shards)
```

- `graph_watermark`: highest graph→vector acked stamp (attached to an existing `vector_sync_batch`);
- `router_watermark`: highest Router→vector acked stamp (attached to the next ingest batch).

Watermarks are delivered-and-acked marks, maintained even when a delivery path is idle, so a quiet
graph or Router never blocks GC.

A deleted entry with `current_stamp <= min(both)` for its shard is unreachable (no stale replay can
arrive) and is GC'd. The subject map therefore stays at `≈ N + recently deleted`.

## Physical layout

### Slab and pages (two-table format)

The `VECTOR_ROW_SLAB` holds fixed-stride pages; `VECTOR_PAGE_META` is the directory. Each page:

```text
[PageHeader] [run_table × run_capacity] [row_meta × capacity] [vector_bytes × capacity]
```

- **Run table** shares the shard across contiguous rows: rows within a run store only the 30-bit
  vertex id. Runs are created at the page tail (no middle insertion); a run is bounded by the shard
  range the canister owns (`run_capacity = min(owned_shards, MAX_RUNS=64)`).
- **row_meta** unifies identity + aux constants into one table so the partition scan reads a single
  metadata span per page.
- **vector_bytes** is 16-byte aligned for SIMD scoring.
- Tail-only allocation, positions are write-once (never reused), so a row is validated **positionally**
  (subject map slot matches the scanned position) — no stamp/vector_id/generation per row.

### Headers (3-byte magic + u8 version; version 1, breaking)

```rust
pub struct SlabHeader {
    pub magic: [u8; 3],        // b"VSL"
    pub version: u8,           // 1 (fresh lineage; old "VSL1" ASCII magic is rejected)
    pub occupied_tail: u64,
    pub flags: u32,
    pub reserved: [u8; 16],
}

pub struct PageHeader {
    pub magic: [u8; 3],        // b"VPG"
    pub version: u8,           // 1
    pub capacity: u32,
    pub row_stride: u32,       // EncodingRecord.pad_stride_bytes (16B-aligned)
    pub meta_stride: u32,      // 4 + aux_bytes  (4 | 8 | 12)
    pub run_capacity: u32,
    pub run_count: u32,
}
```

Reopen validation is fail-closed: magic/version must match, `meta_stride ∈ {4,8,12}`, and the page
span must be overflow-safe.

### Row meta

```rust
#[repr(transparent)]
pub struct VertexPayload(u32);   // bits 0..30: vertex id; bit 30: reserved;
                                 // bit 31: tombstone (mirrors VertexRef::TOMBSTONE_BIT)

pub struct RowMeta {             // stored width = meta_stride (4 | 8 | 12)
    pub vertex: VertexPayload,
    pub aux: [u8; 8],            // low meta_stride - 4 bytes meaningful
}
```

- The 30-bit payload is a contract: the ingest boundary rejects `vertex_id >= 2^30` (fail-closed).
  The graph's own edge representation (`VertexRef::PAYLOAD_MASK = (1<<30)-1`) already treats
  shard-local vertex identity as 30-bit.
- The tombstone bit preserves the scan's cheap dead-row skip; correctness is backed by positional
  validation.
- Aux bytes are interpreted by `(encoding, metric, pruning_config)` via `RowAux`; the storage layer
  keeps them opaque.

### Memory manager

The vector canister uses `ic-stable-variable-memory-manager` with per-region bucket policies:

| Region                      | Bucket    |
| --------------------------- | --------- |
| `VECTOR_ROW_SLAB`           | 64 pages  |
| `VECTOR_SUBJECT_TO_ID`      | 32 pages  |
| `VECTOR_PAGE_META`          | 16 pages  |
| centroids / heads / defs    | 8 pages   |
| rebuild / maintenance state | 4–8 pages |

## Encodings

`EncodingRecord` is a first-class concept (the LARA `label → width` generalization, owned by the
vector canister):

```rust
pub struct EncodingRecord {
    pub encoding: VectorEncoding,          // F32 | F16 | Bf16 | I8 | U8 | Binary
    pub dims: u16,
    pub stride_bytes: u32,                 // stored stride, minimal per encoding
    pub pad_stride_bytes: u32,             // scoring scratch stride = ceil(dims/4)*16
    pub aux_bytes: u32,                    // 0 | 4 | 8
    pub binary_convention: Option<BinaryConvention>,  // Bits01 | Signs
    pub kernel: ScoringKernel,             // F32Dot | UpcastF32Dot | BinaryPopcnt
}
```

- **One index = one encoding = one dims = one stride** (different models → different indexes).
- Scoring **materializes once per page read** into the f32 scratch (except binary, which scores via
  `i64.popcnt` directly). `F16`/`Bf16`/`I8` upcast at page granularity, so upcast cost is amortized,
  never per row.
- **Binary cosine**: `Bits01` → `n11·rq·rv` (`n11 = Σ popcnt(q∧v)`, per-row `rv = 1/√popcnt(v)`);
  `Signs` → `1 − 2H/d` (`H = Σ popcnt(q⊕v)`, cosine ≡ Hamming order, no sqrt). The convention is a
  per-model property.
- **Stored precision = search precision** (accepted and documented): an `int8`/`binary` index's
  "exact rerank" is exact over the stored encoding, not full precision. Quantized indexes use
  **quantized centroids** with SPANN's representative-point replacement (k-means means of binary
  vectors are not binary).

### Aux interpretation (RowAux)

| encoding × metric        | aux_bytes | meaning                                         |
| ------------------------ | --------- | ----------------------------------------------- |
| F32 × L2 (default)       | 0         | sub-square SIMD + early exit; no norm, no bound |
| F32 × cosine (default)   | 0         | normalized dot only                             |
| I8                       | 4         | quantization scale (mandatory)                  |
| opt-in row-level pruning | +4        | L2 bound `dist(v, c_p)` (sub-square path)       |
| opt-in row-level pruning | 8         | cosine `(s_v, t_v)` or norm + bound             |

## Search algorithm

`ivf_flat` is the kind (category); SPANN is the blueprint (a high-performance instance of the same
family: inverted index + uncompressed vectors + exact rerank).

### Scoring formulation

- **L2 (default)**: `sub-square` SIMD (`Σ(q−v)²`) with HARMONY-style **dimension-blocked early exit**
  (prewarm the k-th best with the first rows, then stop each row's SIMD once its partial distance
  exceeds the running threshold; block order per-query by descending `‖q_block‖`). Monotone partial
  sums make early exit safe; no per-row norm is stored.
- **Cosine**: normalized vectors stored at ingest; score is `1 − dot` only.
- The `dot + norms` formulation (FMA inner loop, precomputed `‖v‖²`) remains an opt-in alternative.
- SIMD is `f32x4` with multi-accumulator batching; the scratch is 16-byte aligned and decoded by
  direct reinterpretation (no per-row `Vec<f32>` allocation).

### Partition selection

Fixed `nprobe` is replaced by the **query-aware pruning rule**:

```text
select partition p iff dist(q, c_p) <= (1 + eps_query) * dist(q, c_best)
```

`eps_query` is a per-definition setting. The selected partitions are scanned **in full** (no mid-scan
truncation; `VectorSearchResult` carries no partial marker).

### Hierarchical partition structure (level-generic, designed from the start)

The partition structure is **level-generic**: a definition carries `levels`, `nlist[level]`, and
`eps_query[level]`, and a leaf `partition_id` packs its path (`coarse_id × nlist_2 + fine_id`).
Centroids are tagged by level; each level's centroids are trained over its parent's members only
(per-subtree bounded training jobs). Search iterates levels: score the query against the selected
parents' children and recurse to the leaves.

- **Flat is the degenerate case (`levels = 1`)** — one level of `nlist` centroids, no fine training,
  single-stage selection, behaviorally identical to the non-hierarchical design. The same code path
  serves both, so designing the hierarchy from the start does **not** force a 2-level deployment.
- **Why from the start**: the 8 MiB training envelope is a _per-training-job_ bound, so the ≈677
  `nlist` ceiling at `d = 1536` applies **per level**, not globally. A hierarchy restores trainable
  centroid counts and avoids the flat candidate-pool degeneration (pool ≈ `nlist` → k-means
  degrades to sampled centroids) at the design target. Retrofitting level semantics later would
  touch partition keys, centroid metadata, the rebuild state machine, the search path, and the
  definition config at once.
- **Deployment stays measured**: `levels = 1` (flat behavior) is the default until the scan-cost
  measurement at the target scale justifies `levels = 2` (e.g., 64×64). The L1 centroid table is
  the Router's future routing index for partition-based multi-canister dispatch.

### Search path

1. Heap centroid cache (query path is read-only; a miss reads stable for that call only).
2. Query × nlist centroids (SIMD; `nlist <= MAX_NLIST`).
3. ε₂ partition selection.
4. Per selected partition: read extents as contiguous spans → tombstone skip (bit 31) → positional
   validation against the subject map → (opt-in) row-level bound pruning → SIMD.
5. Deterministic top-k heap ordered by `(score, subject)`.
6. Filtered search (ADR 0034): the candidate allowlist from the Property Index is unchanged.

## Rebuild and compaction

**Rebuild is the only compaction.** The shadow-version state machine
(`Idle → Sampling → Training → Building → ReadyToPublish → Cleaning`) with dual-write and atomic
publish is retained unchanged. There is no free-span allocator, no PMA rebalance, and no routine
extent compaction: tail-only allocation grows the slab, and the Slice 9/10 tombstone-ratio policy
bounds it (`slab ≤ live/(1 − r)` for trigger ratio `r`).

Rebuild reads the vector canister's own rows (self-rebuild); the graph is never consulted.

## Growth model

Symbols: `N` = live subjects per index, `G` = cumulative ever-ingested, `s` = stored stride,
`m` = row metadata (4B vertex + aux 0–8B + page amortization), `r` = rebuild tombstone trigger,
`η` = B-tree overhead.

| Store                         | Growth                            | Bound / lever                                              |
| ----------------------------- | --------------------------------- | ---------------------------------------------------------- |
| `VECTOR_ROW_SLAB`             | `N/(1−r) × (m + s)`               | encoding (F32→I8 1/4, Binary 1/32); tombstone-ratio policy |
| `VECTOR_SUBJECT_TO_ID`        | `(N + recent deletes) × ~60B × η` | **watermark GC** (monotonic growth eliminated)             |
| `VECTOR_PAGE_META` / heads    | O(N / slots per page), O(nlist)   | negligible                                                 |
| centroids / defs / watermarks | O(nlist × d), O(#defs), O(shards) | negligible                                                 |
| graph                         | 0 embedding bytes                 | outbox carries remove metadata only                        |

At `d = 1536` (the design target): `F32` at 10⁸ subjects (~616 GB) exceeds one canister, so large
high-dim indexes require `I8`/`Binary` or future multi-canister placement.

## Multi-canister readiness

Single canister today; two future split axes are distinguished:

- **A. Shard-group split (near-term future)**: each vector canister owns a contiguous graph shard
  range (mirrors ADR 0019; industry standard — Milvus/Qdrant/Vespa hash-stable routing). Per-canister
  IVF, Router fan-out + merge, rebuild stays canister-local. Run-table shard sharing becomes most
  effective here.
- **B. Partition-based split (future ADR)**: global centroids, partition ownership, query-aware
  dispatch (SPANN §7). Requires global training, cross-canister rebuild, and a routing story; research
  grounding (SPFresh/LIRE boundary-only reassignment) makes the subject→canister routing index mostly
  static. Deferred.

Prepared now:

- Router vector resolution returns a **target list** and the search orchestration is fan-out + merge
  with a **global deterministic top-k** contract (scores are comparable: same metric/dims/encoding).
- `VectorIndexOwnershipConfig` gains `index_group_size` / `group_index` (Option; `None` = whole
  graph).
- **Row-copy migration** primitive (`export_vector_rows` / `import_vector_rows`, bounded cursor) is
  the only split path — the graph holds no bytes, so graph backfill cannot migrate a vector canister.

## Algorithm roadmap

| Phase | Item                                                                               | Status                                                                                                          |
| ----- | ---------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| 1     | `ivf_flat` (degenerate `nlist=1` → production `nlist>1`)                           | current kind                                                                                                    |
| 2     | ε₂ query-aware pruning                                                             | Slice 11+ candidate                                                                                             |
| 3     | balance-aware training (SPANN HBC objective, λ penalty); k-means++ init (optional) | Slice 11+ candidate                                                                                             |
| 4     | dimension-blocked early exit (HARMONY-style)                                       | Slice 11+ candidate                                                                                             |
| 5     | two-level hierarchy (e.g., 64×64)                                                  | **designed level-generic from the start** (`levels = 1` = flat behavior); 2-level deployment measured           |
| 6     | closure replication (coarse-level, opt-in)                                         | Slice 11+ candidate                                                                                             |
| —     | `ivf_pq`, global HNSW                                                              | **deferred** (PQ recall; HNSW: random reads, unbounded instruction budget, delete repair, cross-canister edges) |

At `d = 1536` a single training job is bounded to ≈677 centroids by the 8 MiB envelope; with the
level-generic structure this becomes a **per-level** bound, restoring trainable counts. A raw
candidate region remains the documented fallback if per-job pools still bind.

## Research grounding

- **SPANN** (arXiv 2111.08566): ivf_flat-family blueprint — balanced clustering, closure, query-aware
  pruning. The hierarchy and ε₂ follow it directly.
- **SPFresh / LIRE** (SOSP 2023): boundary-only incremental reassignment makes cluster-partitioned
  indexes mutable cheaply; grounds the future partition-based split.
- **HARMONY** (ACM MOD 2025): dimension-split is unsuitable for IC (communication cost); its
  early-stop pruning transfers as the intra-row optimization.
- **Routing indices** (Crespo & Garcia-Molina, ICDCS 2002): query-side routing summaries; maps to the
  Router's centroid set + ownership map.
- **Industry survey**: Neo4j/Memgraph/Kuzu/TigerGraph/FalkorDB/Dgraph/pgvector — embeddings are node
  data, indexes are label-scoped, data outlives labels (we deviate on IC: bytes live only in the
  vector canister). Memgraph's single-store (bytes in the index, reference in the property store) is
  the closest precedent for this ownership split.

## Superseded implementation (Slices 1–10)

The discarded ADR 0031/0032/0033 implementation is documented in git history and the ADRs. In brief:
it stored canonical embedding bytes on graph shards (`VERTEX_EMBEDDINGS`, MemoryId 44; incarnations
MemoryId 45), fenced by `(incarnation, version)`, relayed bytes through the graph on ingest, and
sharded the vector canister by single-target-per-graph. None of its stable layout survives; the
canister's stable regions are rebuilt under the new layout.

## Open items

1. **Per-level `nlist` under the 8 MiB envelope**: the level-generic structure makes the ≈677 ceiling
   a per-training-job bound; a hierarchy restores trainable counts at `d = 1536`. A raw candidate
   region remains the documented fallback if per-job pools still bind.
2. **Hierarchy deployment timing**: ships as `levels = 1` (flat behavior); enabling `levels = 2`
   follows the scan-cost measurement at the target scale.
3. **Scoring formulation**: canbench `(a)` dot+norms+pruning, `(b)` sub-square+early-exit (default),
   `(c)` sub-square+bound, at `d=1536`.
4. **Page size / slots per page**: config tuned by the scan-cost measurement.

## Related documents

- [ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md) (superseded)
- [ADR 0032](../adr/0032-vector-index-slab-page-store.md) (superseded)
- [ADR 0033](../adr/0033-vector-rebuild-state-read-memoization.md) (superseded)
- [property-index.md](property-index.md)
- [derived-state-query-semantics.md](derived-state-query-semantics.md)
- [capacity-planning.md](capacity-planning.md)
- [../architecture/overview.md](../architecture/overview.md)
- [../storage/stable-memory-inventory.md](../storage/stable-memory-inventory.md)
